use crate::color::{Color, ColorSupport, ResolvedColor};
use crate::parser::ast::*;
use crate::resource::{BudgetLease, ResourceCategory, ResourceGovernor};

use super::border::draw_border_with_joints;
use super::cell::{CellBuffer, CellStyle};
use super::text::{WidthConfig, display_width, try_wrap_text};
use super::{LayoutAllocationSite, reject_layout_allocation};

/// Layout context passed down through the element tree.
///
/// Contains per-scope state (position, width, style) and immutable config
/// (color support, width config). Cheap to clone for child scopes since
/// it contains no heap-allocated collections.
#[derive(Debug, Clone)]
pub(crate) struct LayoutCtx {
    /// Left edge of available space.
    pub(crate) x: u16,
    /// Current vertical cursor position.
    pub(crate) y: u16,
    /// Available width in columns.
    pub(crate) width: u16,
    /// Page/viewport height — used by background animations to fill the screen.
    pub(crate) viewport_height: u16,
    /// Color support level of the terminal.
    pub(crate) color_support: ColorSupport,
    /// Width config for ambiguous characters.
    pub(crate) wcfg: WidthConfig,
    /// Inherited style stack.
    pub(crate) style: CellStyle,
    /// Shared admission authority for remotely influenced layout temporaries.
    pub(crate) governor: Option<ResourceGovernor>,
}

pub(super) struct GovernedTempVec<T> {
    values: Vec<T>,
    _lease: Option<BudgetLease>,
}

impl<T> std::ops::Deref for GovernedTempVec<T> {
    type Target = [T];

    fn deref(&self) -> &Self::Target {
        &self.values
    }
}

impl<T> std::ops::DerefMut for GovernedTempVec<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.values
    }
}

impl<T> GovernedTempVec<T> {
    /// Append a value within the capacity reserved at construction.
    ///
    /// Returns `None` rather than growing. The whole point of the exact
    /// reservation is that this vector never allocates again after it is
    /// admitted, so a push past capacity is a bound that was computed wrong,
    /// not something to paper over by reallocating.
    pub(super) fn try_push(&mut self, value: T) -> Option<()> {
        if self.values.len() == self.values.capacity() {
            return None;
        }
        self.values.push(value);
        Some(())
    }
}

pub(super) fn try_temp_vec<T>(
    capacity: usize,
    governor: Option<&ResourceGovernor>,
) -> Option<GovernedTempVec<T>> {
    if reject_layout_allocation(LayoutAllocationSite::TempVec) {
        return None;
    }
    let requested = capacity.checked_mul(std::mem::size_of::<T>())?;
    let mut lease = match (requested, governor) {
        (0, _) | (_, None) => None,
        (bytes, Some(governor)) => Some(
            governor
                .reserve(ResourceCategory::RemoteCollections, bytes)
                .ok()?,
        ),
    };
    let mut values = Vec::new();
    values.try_reserve_exact(capacity).ok()?;
    let retained = values.capacity().checked_mul(std::mem::size_of::<T>())?;
    if let Some(lease) = lease.as_mut() {
        lease.try_resize_with_cost(retained, retained).ok()?;
    }
    Some(GovernedTempVec {
        values,
        _lease: lease,
    })
}

/// Copy a slice into a governed temporary, admitting the copy before it is
/// made. The per-kind layouts use this to snapshot a node's child ids so the
/// immutable scene borrow can end before layout mutates the scene.
pub(super) fn try_temp_vec_from_slice<T: Copy>(
    source: &[T],
    governor: Option<&ResourceGovernor>,
) -> Option<GovernedTempVec<T>> {
    let mut values = try_temp_vec(source.len(), governor)?;
    values.values.extend_from_slice(source);
    Some(values)
}

/// A governed temporary pre-filled with `capacity` copies of `value`,
/// admitted before any of it is built. Replaces `vec![value; n]`, which
/// allocates a remotely sized collection with no way to refuse.
pub(super) fn try_temp_vec_filled<T: Clone>(
    capacity: usize,
    value: T,
    governor: Option<&ResourceGovernor>,
) -> Option<GovernedTempVec<T>> {
    let mut values = try_temp_vec(capacity, governor)?;
    for _ in 0..capacity {
        values.try_push(value.clone())?;
    }
    Some(values)
}

/// Position of a focusable element: (column, row, width).
///
/// Historically this was the element type in a `Vec<FocusablePos>`
/// threaded through layout via `LayoutAccum`. Post-Phase-B, focusable
/// rects live on `Node.focusable_screen_rect` and the list is derived
/// from `Scene::iter_focusable_rects()` at query time. The tuple alias
/// is retained because `LayoutResult.focusable_positions` still serves
/// a few downstream and test consumers that pre-date the scene walks.
pub type FocusablePos = (u16, u16, u16);

type GovernedLayoutMetadata = (
    Vec<FocusablePos>,
    Vec<PlacedElement>,
    Vec<StickyRegion>,
    Option<BudgetLease>,
);

pub use super::rect::Rect;

fn fallible_layout_buffer(width: u16, height: u16) -> CellBuffer {
    CellBuffer::try_new(width, height).unwrap_or_else(|_| {
        let mut fallback = CellBuffer::new(1, 1);
        fallback.record_allocation_failure();
        fallback
    })
}

fn fallible_layout_buffer_governed(
    width: u16,
    height: u16,
    governor: &ResourceGovernor,
) -> CellBuffer {
    CellBuffer::try_new_governed(width, height, governor, ResourceCategory::CompositorCells)
        .unwrap_or_else(|_| {
            let mut fallback = CellBuffer::new(1, 1);
            fallback.record_allocation_failure();
            fallback
        })
}

/// First-class output of a layout call.
///
/// `rect` is what the element itself occupies in buffer-absolute coordinates.
/// `bbox` is the union of `rect` and all descendants' bboxes — this is what
/// captures absolute-positioned descendants whose cells lie outside the
/// parent's flow rect.
/// `flow_advance` is how far the flow cursor advances for the next sibling.
/// For absolutely-positioned elements (e.g. `[box x=10 y=5]`) this is 0.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Placement {
    pub rect: Rect,
    pub flow_advance: u16,
    pub bbox: Rect,
}

impl Placement {
    pub fn empty_at(x: u16, y: u16) -> Self {
        let r = Rect { x, y, w: 0, h: 0 };
        Self {
            rect: r,
            flow_advance: 0,
            bbox: r,
        }
    }
}

/// A placed element from the layout pass: its identity, kind-specific data,
/// and the rectangle it occupies.
///
/// Kind-specific data that is *not* placement (a live region's endpoint, an
/// animation's background flag) lives inside the `PlacedKind` variant so the
/// outer struct stays uniform; this mirrors the `Node`/`NodeKind` split in
/// `compositor.md`.
#[derive(Debug, Clone)]
pub struct PlacedElement {
    pub id: String,
    pub kind: PlacedKind,
    pub rect: Rect,
}

impl PlacedElement {
    pub fn is_panel(&self) -> bool {
        matches!(self.kind, PlacedKind::Panel)
    }

    pub fn is_animation(&self) -> bool {
        matches!(self.kind, PlacedKind::Animation { .. })
    }

    pub fn is_live(&self) -> bool {
        matches!(self.kind, PlacedKind::Live { .. })
    }

    pub fn is_background_animation(&self) -> bool {
        matches!(self.kind, PlacedKind::Animation { background: true })
    }

    pub(crate) fn retained_string_capacity(&self) -> usize {
        self.id.capacity().saturating_add(match &self.kind {
            PlacedKind::Live { endpoint, .. } => endpoint.capacity(),
            PlacedKind::Panel | PlacedKind::Animation { .. } => 0,
        })
    }
}

#[derive(Debug, Clone)]
pub enum PlacedKind {
    Panel,
    Animation {
        /// When true, this animation renders behind base content (z=-1).
        background: bool,
    },
    Live {
        endpoint: String,
        scroll: LiveScroll,
        buffer: u32,
        delta: bool,
    },
}

/// A region pinned to the viewport edge during scrolling.
#[derive(Debug, Clone)]
pub struct StickyRegion {
    pub position: StickyPosition,
    pub y: u16,
    pub h: u16,
}

/// Result of a layout operation: an empty page-canvas `CellBuffer`
/// that carries the page's width and height, plus metadata derived
/// from the scene walk.
///
/// Post the per-node-buffer migration, `buffer` is always empty — every
/// visible cell is owned by a scene node and composited via
/// `composite::walk`. The field is retained because many call sites
/// read `buffer.width` / `buffer.height` to size the `Compositor` and
/// the viewport; those dimensions are still authoritative. `buffer`
/// grows via `ensure_height` in the kind helpers as flow cursors
/// advance past the initial term-height extent.
pub struct LayoutResult {
    /// Empty buffer carrying page dimensions. See the struct doc for
    /// the Phase 6 pivot; do not write cells into this.
    pub buffer: CellBuffer,
    /// (col, row) positions of focusable elements in document order.
    pub focusable_positions: Vec<FocusablePos>,
    /// Every placed element (panels, animations, live regions) in document
    /// order. Consumers filter by `PlacedKind`.
    pub placed: Vec<PlacedElement>,
    /// Regions pinned to viewport edges.
    pub sticky_regions: Vec<StickyRegion>,
    /// Owns the backing-capacity charge for fixed-size layout metadata.
    ///
    /// Focus positions and sticky regions are both derived from remote scene
    /// content. Governed layout pre-admits their combined vector storage before
    /// either vector allocates and retains that admission for the result's
    /// lifetime.
    _metadata_lease: Option<BudgetLease>,
}

impl LayoutResult {
    pub fn panels(&self) -> impl Iterator<Item = &PlacedElement> {
        self.placed.iter().filter(|p| p.is_panel())
    }

    pub fn animations(&self) -> impl Iterator<Item = &PlacedElement> {
        self.placed.iter().filter(|p| p.is_animation())
    }

    pub fn live_regions(&self) -> impl Iterator<Item = &PlacedElement> {
        self.placed.iter().filter(|p| p.is_live())
    }
}

// Test-only counter for how many times `layout_scene` has been
// called. Thread-local so parallel tests don't collide. Used by
// the scoped-panel-relayout tests to assert the scoped path issues
// fewer layout calls than the full-page path for the same action.
#[cfg(test)]
thread_local! {
    pub static LAYOUT_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Lay out a scene subtree at `(0, 0)` into a fresh buffer of
/// `(width, height)` cells. Used by animation frame rendering to
/// produce per-frame `CellBuffer`s without reaching back into the
/// AST via `render_frame_element`.
///
/// The subtree's children are walked via `kinds::layout_children_scene`
/// — the same path `layout_scene` uses for the root. Post-per-node
/// buffer pivot, each kind's layout helper writes into its own
/// `scene.layout_buffer_mut(node_id)` rather than into the passed-in
/// `buf`, so after the walk we run a tree-order mini-composite over
/// the subtree's descendants to fold their per-node buffers into the
/// returned `CellBuffer` at the placements the walk just wrote.
pub fn layout_subtree(
    scene: &mut crate::compositor::scene::Scene,
    node_id: crate::compositor::scene::NodeId,
    width: u16,
    height: u16,
    color_support: ColorSupport,
    wcfg: WidthConfig,
) -> CellBuffer {
    layout_subtree_inner(scene, node_id, width, height, color_support, wcfg, None)
}

/// Governed counterpart used for remotely authored animation snapshots.
pub fn layout_subtree_governed(
    scene: &mut crate::compositor::scene::Scene,
    node_id: crate::compositor::scene::NodeId,
    width: u16,
    height: u16,
    color_support: ColorSupport,
    wcfg: WidthConfig,
    governor: &ResourceGovernor,
) -> CellBuffer {
    layout_subtree_inner(
        scene,
        node_id,
        width,
        height,
        color_support,
        wcfg,
        Some(governor),
    )
}

fn layout_subtree_inner(
    scene: &mut crate::compositor::scene::Scene,
    node_id: crate::compositor::scene::NodeId,
    width: u16,
    height: u16,
    color_support: ColorSupport,
    wcfg: WidthConfig,
    governor: Option<&ResourceGovernor>,
) -> CellBuffer {
    #[cfg(test)]
    {
        LAYOUT_CALLS.with(|c| c.set(c.get() + 1));
    }
    let mut buf = governor.map_or_else(
        || fallible_layout_buffer(width, height),
        |governor| fallible_layout_buffer_governed(width, height, governor),
    );
    let child_ids = match scene.get(node_id) {
        Some(node) => node.children(),
        None => return buf,
    };
    let Some(children) = try_temp_vec_from_slice(child_ids, governor) else {
        buf.record_allocation_failure();
        scene.record_resource_error();
        return buf;
    };

    let mut ctx = LayoutCtx {
        x: 0,
        y: 0,
        width,
        viewport_height: height,
        color_support,
        wcfg,
        style: CellStyle::default(),
        governor: governor.cloned(),
    };
    super::kinds::layout_children_scene(&mut buf, &mut ctx, scene, &children);

    // Mini-composite: walk the subtree in tree order and blit each
    // node's per-node buffer at its just-written placement. Mirrors
    // Phase B of `composite::walk` but scoped to this subtree and
    // into `buf` instead of the screen. Transparent cells pass
    // through (matches blit semantics in the main composite).
    let Some(mut stack) =
        try_temp_vec::<crate::compositor::scene::NodeId>(scene.node_count(), governor)
    else {
        buf.record_allocation_failure();
        scene.record_resource_error();
        return buf;
    };
    stack.values.extend(children.iter().rev().copied());
    while let Some(id) = stack.values.pop() {
        let Some(n) = scene.get(id) else { continue };
        if n.visible() {
            if let Some(src) = n.buffer() {
                let rect = n.placement().rect;
                if !rect.is_empty() {
                    let h = src.height.min(buf.height.saturating_sub(rect.y));
                    let w = src.width.min(buf.width.saturating_sub(rect.x));
                    for y in 0..h {
                        if let Some(row) = src.row(y) {
                            for x in 0..w {
                                if let Some(cell) = row.get(x as usize)
                                    && !cell.is_transparent()
                                {
                                    buf.set(
                                        rect.x.saturating_add(x),
                                        rect.y.saturating_add(y),
                                        cell.clone(),
                                    );
                                }
                            }
                        }
                    }
                }
            }
            for &c in n.children().iter().rev() {
                stack.values.push(c);
            }
        }
    }

    buf
}

/// In-place relayout of a subtree rooted at `node_id`. Reads the
/// node's current `placement.rect` as the origin/size, re-runs
/// `layout_node` with `ctx.x`/`ctx.y` at the node's screen position.
/// Post the per-node-buffer migration, layout writes land in per-node
/// `CellBuffer`s on the scene; `page_buf` is the empty dimensions
/// holder — kept as a parameter so existing call sites don't churn,
/// and so `ensure_height` still propagates page extents.
///
/// Unlike `layout_subtree` (which lays at `(0, 0)` into a fresh buffer
/// for animation frame rendering), this primitive writes
/// **screen-absolute** placements to `Node.placement` — preserving the
/// invariant that placement rects are on-screen coordinates.
///
/// Used by `layout_pass_invalidated` (Stage 5 drain) to scope-relayout
/// any `NodeKind` without corrupting the scene's placement state.
pub(crate) fn relayout_in_place(
    scene: &mut crate::compositor::scene::Scene,
    page_buf: &mut CellBuffer,
    node_id: crate::compositor::scene::NodeId,
    color_support: ColorSupport,
    wcfg: WidthConfig,
) -> bool {
    #[cfg(test)]
    {
        LAYOUT_CALLS.with(|c| c.set(c.get() + 1));
    }

    let rect = match scene.get(node_id).map(|n| n.placement().rect) {
        Some(r) if !r.is_empty() => r,
        _ => return true,
    };

    // Keep page_buf tall enough for downstream consumers (scroll
    // metrics, Compositor sizing). No cell-clear is needed because
    // layout writes into per-node buffers on the scene, not into
    // page_buf — re-allocating a node's buffer (in its layout helper)
    // discards any prior content.
    page_buf.ensure_height(rect.y.saturating_add(rect.h));

    let default_style = CellStyle {
        fg: scene
            .default_fg
            .as_ref()
            .and_then(|c| c.resolve(color_support)),
        bg: scene
            .default_bg
            .as_ref()
            .and_then(|c| c.resolve(color_support)),
        ..Default::default()
    };

    let mut ctx = LayoutCtx {
        x: rect.x,
        y: rect.y,
        width: rect.w,
        viewport_height: rect.h,
        color_support,
        wcfg,
        style: default_style,
        governor: scene.resource_governor(),
    };

    // Dispatch to the node's own kind helper. The helper writes
    // `scene.update_placement(node_id, placement)` at the correct
    // screen-absolute coordinates because ctx starts at (rect.x, rect.y).
    super::kinds::layout_node(page_buf, &mut ctx, scene, node_id);
    !page_buf.allocation_failed() && !scene.resource_limit_exceeded()
}

/// Scene-native layout entry point — the only public layout entry.
/// All page metadata (mode, default style, transition) is read from
/// `scene`; the scene is self-sufficient after `build_scene`.
pub fn layout_scene(
    scene: &mut crate::compositor::scene::Scene,
    term_width: u16,
    term_height: u16,
    color_support: ColorSupport,
    wcfg: WidthConfig,
) -> LayoutResult {
    layout_scene_inner(scene, term_width, term_height, color_support, wcfg, None)
}

/// Layout a remotely-authored scene into a page canvas whose exact allocation
/// owns its compositor lease from construction through drop and resize.
pub(crate) fn layout_scene_governed(
    scene: &mut crate::compositor::scene::Scene,
    term_width: u16,
    term_height: u16,
    color_support: ColorSupport,
    wcfg: WidthConfig,
    governor: &ResourceGovernor,
) -> LayoutResult {
    layout_scene_inner(
        scene,
        term_width,
        term_height,
        color_support,
        wcfg,
        Some(governor),
    )
}

fn layout_scene_inner(
    scene: &mut crate::compositor::scene::Scene,
    term_width: u16,
    term_height: u16,
    color_support: ColorSupport,
    wcfg: WidthConfig,
    governor: Option<&ResourceGovernor>,
) -> LayoutResult {
    #[cfg(test)]
    {
        LAYOUT_CALLS.with(|c| c.set(c.get() + 1));
    }

    let (buf_width, buf_height) = match scene.page_mode {
        PageMode::Screen { cols, rows } => {
            (cols.unwrap_or(term_width), rows.unwrap_or(term_height))
        }
        PageMode::Document => (term_width, term_height),
    };

    let mut buf = governor.map_or_else(
        || fallible_layout_buffer(buf_width, buf_height),
        |governor| fallible_layout_buffer_governed(buf_width, buf_height, governor),
    );
    let buf_width = buf.width;
    let buf_height = buf.height;

    let default_style = CellStyle {
        fg: scene
            .default_fg
            .as_ref()
            .and_then(|c| c.resolve(color_support)),
        bg: scene
            .default_bg
            .as_ref()
            .and_then(|c| c.resolve(color_support)),
        ..Default::default()
    };

    let mut ctx = LayoutCtx {
        x: 0,
        y: 0,
        width: buf_width,
        viewport_height: buf_height,
        color_support,
        wcfg,
        style: default_style,
        governor: governor.cloned(),
    };

    // Walk the scene's root children. Each child goes through
    // `kinds::layout_node`, which dispatches on `NodeKind`. Sibling
    // cursor pinning, bbox union, and flow-advance summing preserve
    // the invariant that absolute-positioned descendants contribute
    // zero to flow advance but still accumulate into the bbox.
    let root_id = scene.root();
    let root_child_count = match scene.get(root_id) {
        Some(n) => n.children().len(),
        None => return build_layout_result(buf, scene, governor),
    };

    let start_x = ctx.x;
    let start_y = ctx.y;
    let mut flow_advance: u16 = 0;

    for child_index in 0..root_child_count {
        let Some(child_id) = scene
            .get(root_id)
            .and_then(|root| root.children().get(child_index))
            .copied()
        else {
            continue;
        };
        ctx.y = start_y.saturating_add(flow_advance);
        let p = super::kinds::layout_node(&mut buf, &mut ctx, scene, child_id);
        flow_advance = flow_advance.saturating_add(p.flow_advance);
    }

    let _ = (start_x, start_y, flow_advance);

    build_layout_result(buf, scene, governor)
}

/// Populate `LayoutResult` from scene-authoritative state. Reads
/// focusable rects, placed regions, and sticky regions directly off
/// the scene — no parallel accumulator involved.
fn build_layout_result(
    mut buffer: CellBuffer,
    scene: &crate::compositor::scene::Scene,
    governor: Option<&ResourceGovernor>,
) -> LayoutResult {
    // Both paths go through the same counted, fallible construction. The
    // ungoverned one used to `.collect()` three scene-sized vectors with no
    // bound and no way to refuse, purely because no governor was installed;
    // the absence of a governor is a reason to take no lease, not a reason to
    // stop reserving.
    let (focusable_positions, placed, sticky_regions, metadata_lease) =
        match governed_fixed_metadata(scene, governor) {
            Some(metadata) => metadata,
            None => {
                buffer.record_allocation_failure();
                empty_layout_metadata()
            }
        };
    LayoutResult {
        buffer,
        focusable_positions,
        placed,
        sticky_regions,
        _metadata_lease: metadata_lease,
    }
}

/// Empty layout metadata, holding no lease. Kept as one function so the
/// `Vec::new()`s standing for "nothing placed" are not repeated; they are
/// never grown, so they never allocate.
fn empty_layout_metadata() -> GovernedLayoutMetadata {
    (Vec::new(), Vec::new(), Vec::new(), None)
}

/// Pre-admit and allocate the fixed-size metadata vectors as one transaction.
/// A failed budget or allocator reservation drops the temporary lease and
/// returns before any collection is exposed.
///
/// `governor` is optional: without one the vectors are still counted and
/// reserved exactly, they simply carry no lease.
fn governed_fixed_metadata(
    scene: &crate::compositor::scene::Scene,
    governor: Option<&ResourceGovernor>,
) -> Option<GovernedLayoutMetadata> {
    if reject_layout_allocation(LayoutAllocationSite::FixedMetadata) {
        return None;
    }
    let focusable_count = scene.iter_focusable_rects().count();
    let (placed_count, placed_string_bound) = scene.placed_storage_requirements(false)?;
    let sticky_count = scene.iter_sticky().count();
    let focusable_bytes = focusable_count.checked_mul(std::mem::size_of::<FocusablePos>())?;
    let placed_bytes = placed_count.checked_mul(std::mem::size_of::<PlacedElement>())?;
    let sticky_bytes = sticky_count.checked_mul(std::mem::size_of::<StickyRegion>())?;
    let admitted_bytes = focusable_bytes
        .checked_add(placed_bytes)?
        .checked_add(sticky_bytes)?
        .checked_add(placed_string_bound)?;

    if admitted_bytes == 0 {
        return Some(empty_layout_metadata());
    }

    let mut lease = match governor {
        Some(governor) => Some(
            governor
                .reserve(ResourceCategory::RemoteCollections, admitted_bytes)
                .ok()?,
        ),
        None => None,
    };
    let mut focusable_positions = Vec::new();
    focusable_positions
        .try_reserve_exact(focusable_count)
        .ok()?;
    let mut placed = Vec::new();
    placed.try_reserve_exact(placed_count).ok()?;
    let mut sticky_regions = Vec::new();
    sticky_regions.try_reserve_exact(sticky_count).ok()?;

    focusable_positions.extend(scene.iter_focusable_rects().map(|(_, r)| (r.x, r.y, r.w)));
    placed.extend(scene.iter_placed());
    sticky_regions.extend(scene.iter_sticky());

    let retained_bytes = focusable_positions
        .capacity()
        .checked_mul(std::mem::size_of::<FocusablePos>())?
        .checked_add(
            placed
                .capacity()
                .checked_mul(std::mem::size_of::<PlacedElement>())?,
        )?
        .checked_add(
            sticky_regions
                .capacity()
                .checked_mul(std::mem::size_of::<StickyRegion>())?,
        )?
        .checked_add(placed.iter().try_fold(0usize, |total, placed| {
            total.checked_add(placed.retained_string_capacity())
        })?)?;
    if let Some(lease) = lease.as_mut() {
        lease
            .try_resize_with_cost(retained_bytes, retained_bytes)
            .ok()?;
    }

    Some((focusable_positions, placed, sticky_regions, lease))
}

/// Scene-native box layout primitive. Shared by `kinds::absolute` and
/// `kinds::flow` (Box source). Draws the box chrome (bg, border, title,
/// padding) and recurses into the node's children via
/// `kinds::layout_children_scene`.
///
/// `x`/`y` `Some` means absolute positioning; `None` means flow-placed
/// at the cursor. Alignment applies only to flow-placed boxes.
#[allow(clippy::too_many_arguments)]
pub(crate) fn layout_box_node(
    buf: &mut CellBuffer,
    ctx: &mut LayoutCtx,
    scene: &mut crate::compositor::scene::Scene,
    node_id: crate::compositor::scene::NodeId,
    x: Option<u16>,
    y: Option<u16>,
    w: Dimension,
    h: Dimension,
    border: crate::parser::ast::BorderStyle,
    title: Option<&str>,
    join_top: Option<u16>,
    join_bottom: Option<u16>,
    join_left: Option<u16>,
    join_right: Option<u16>,
    padding: u16,
    align: Alignment,
    fg: Option<&Color>,
    bg: Option<&Color>,
) -> Placement {
    let absolute = x.is_some() || y.is_some();

    let box_style = CellStyle {
        fg: resolve_or_inherit(fg, ctx.style.fg, ctx.color_support),
        bg: resolve_or_inherit(bg, ctx.style.bg, ctx.color_support),
        ..ctx.style.clone()
    };

    let children = match scene.get(node_id) {
        Some(n) => match try_temp_vec_from_slice(n.children(), ctx.governor.as_ref()) {
            Some(children) => children,
            None => {
                buf.record_allocation_failure();
                scene.record_resource_error();
                return Placement::empty_at(ctx.x, ctx.y);
            }
        },
        None => return Placement::empty_at(ctx.x, ctx.y),
    };

    let box_y = y.unwrap_or(ctx.y);
    let tentative_x = x.unwrap_or(ctx.x);
    let box_w = match w {
        Dimension::Fixed(w) => w.min(ctx.width),
        Dimension::Fill => ctx.width.saturating_sub(tentative_x.saturating_sub(ctx.x)),
        Dimension::Fit => ctx.width,
    };

    let box_x = if x.is_some() {
        tentative_x
    } else {
        match align {
            Alignment::Center => ctx.x + (ctx.width.saturating_sub(box_w)) / 2,
            Alignment::Right => ctx.x + ctx.width.saturating_sub(box_w),
            Alignment::Left => tentative_x,
        }
    };

    let border_overhead = if border != BorderStyle::None { 2 } else { 0 };
    let padding_overhead = padding * 2;
    let inner_w = box_w.saturating_sub(border_overhead + padding_overhead);

    let content_height = if h == Dimension::Fit {
        measure_children_height_scene(
            inner_w,
            ctx.color_support,
            ctx.wcfg,
            &box_style,
            scene,
            &children,
            ctx.governor.clone(),
        )
    } else {
        0
    };

    let box_h = match h {
        Dimension::Fixed(hh) => hh,
        Dimension::Fit => content_height
            .saturating_add(border_overhead)
            .saturating_add(padding_overhead),
        Dimension::Fill => (buf.height.saturating_sub(box_y)).max(3),
    };

    buf.ensure_height(box_y.saturating_add(box_h));

    // Phase 2 pivot (per-node-buffer migration, Strategy D): the box
    // owns its chrome buffer. Allocate a buffer of (box_w, box_h), paint
    // bg + border + title into it at LOCAL (0, 0, box_w, box_h), and let
    // composite blit it at (box_x, box_y) in tree order. Children still
    // lay out at GLOBAL coords (their own pivot translates internally),
    // so the dispatcher translates local inner coords back to global for
    // `inner_ctx`.
    scene.allocate_buffer(node_id, box_w.max(1), box_h.max(1));

    let (local_inner_x, local_inner_y, inner_w_actual, _inner_h) = {
        if let Some(box_buf) = scene.layout_buffer_mut(node_id) {
            if bg.is_some() {
                box_buf.fill_rect(0, 0, box_w, box_h, ' ', &box_style);
            }
            draw_border_with_joints(
                box_buf,
                0,
                0,
                box_w,
                box_h,
                border,
                title,
                &box_style,
                join_top,
                join_bottom,
                join_left,
                join_right,
            )
        } else {
            (0, 0, box_w, box_h)
        }
    };

    let content_x = box_x
        .saturating_add(local_inner_x)
        .saturating_add(padding.min(inner_w_actual));
    let content_y = box_y.saturating_add(local_inner_y).saturating_add(padding);
    let content_w = inner_w_actual.saturating_sub(padding * 2);

    let mut inner_ctx = LayoutCtx {
        x: content_x,
        y: content_y,
        width: content_w,
        viewport_height: ctx.viewport_height,
        color_support: ctx.color_support,
        wcfg: ctx.wcfg,
        style: box_style,
        governor: ctx.governor.clone(),
    };

    let children_placement =
        super::kinds::layout_children_scene(buf, &mut inner_ctx, scene, &children);

    if !absolute {
        ctx.y = box_y.saturating_add(box_h);
    }

    let rect = Rect::new(box_x, box_y, box_w, box_h);
    let bbox = rect.union(children_placement.bbox);
    let flow_advance = if absolute { 0 } else { box_h };
    Placement {
        rect,
        flow_advance,
        bbox,
    }
}

/// Scene-tree variant of `measure_children_height` used by
/// `layout_box_node`. Takes `&mut Scene` for consistency with the
/// layout pass; the throwaway buffer means placements written during
/// measurement get overwritten by the real layout pass anyway.
fn measure_children_height_scene(
    width: u16,
    color_support: ColorSupport,
    wcfg: WidthConfig,
    style: &CellStyle,
    scene: &mut crate::compositor::scene::Scene,
    children: &[crate::compositor::scene::NodeId],
    governor: Option<ResourceGovernor>,
) -> u16 {
    let mut measure_buf = fallible_layout_buffer(width, 1);
    let mut measure_ctx = LayoutCtx {
        x: 0,
        y: 0,
        width,
        viewport_height: 0,
        color_support,
        wcfg,
        style: style.clone(),
        governor,
    };
    let p =
        super::kinds::layout_children_scene(&mut measure_buf, &mut measure_ctx, scene, children);
    let bbox_h = if p.bbox.is_empty() { 0 } else { p.bbox.h };
    p.flow_advance.max(bbox_h)
}

// All layout flows through `layout_scene` → `kinds::layout_node` →
// per-kind helpers in `super::kinds`. Inline layout is fed from
// scene `TextContent.runs` via `kinds::try_collect_inline_segments`.

/// Returns `false` when the wrap was refused.
///
/// The caller has to act on that: `buf` here is the node's own layout buffer,
/// and nothing reads its failure flag, so a refusal recorded only there is
/// silently swallowed and the page is presented as if it had laid out.
#[must_use]
pub(crate) fn render_wrapped_text(
    buf: &mut CellBuffer,
    ctx: &mut LayoutCtx,
    text: &str,
    style: &CellStyle,
    align: Alignment,
) -> bool {
    let Some(lines) = try_wrap_text(text, ctx.width as usize, ctx.wcfg, ctx.governor.as_ref())
    else {
        buf.record_allocation_failure();
        return false;
    };

    for line in lines.iter() {
        buf.ensure_height(ctx.y.saturating_add(1));

        let x_offset = match align {
            Alignment::Left => 0,
            Alignment::Center => ((ctx.width as usize).saturating_sub(line.width) / 2) as u16,
            Alignment::Right => (ctx.width as usize).saturating_sub(line.width) as u16,
        };

        put_str_clipped(
            buf,
            ctx.x.saturating_add(x_offset),
            ctx.y,
            &line.text,
            style,
            ctx.x.saturating_add(ctx.width),
            ctx.wcfg,
        );
        ctx.y = ctx.y.saturating_add(1);
    }
    true
}

/// Write a string to the buffer, accounting for wide characters.
/// Clips at `max_col` (exclusive) — content beyond this column is not drawn.
pub(crate) fn put_str_clipped(
    buf: &mut CellBuffer,
    x: u16,
    y: u16,
    s: &str,
    style: &CellStyle,
    max_col: u16,
    wcfg: WidthConfig,
) {
    use unicode_segmentation::UnicodeSegmentation;

    let clip = max_col.min(buf.width);
    let mut col = x;
    for grapheme in s.graphemes(true) {
        if col >= clip {
            break;
        }
        let w = display_width(grapheme, wcfg);
        if w == 0 {
            continue;
        }
        if col.saturating_add(w as u16) > clip {
            break;
        }
        buf.put_grapheme(col, y, grapheme, style);
        for continuation in 1..w {
            buf.put_char(col.saturating_add(continuation as u16), y, '\0', style);
        }
        col += w as u16;
    }
}

// ─── Inline Layout ────────────────────────────────────────────

/// A segment of inline content with a uniform style.
#[derive(Debug, Clone)]
pub(crate) struct InlineSegment {
    pub(crate) text: String,
    pub(crate) style: CellStyle,
    /// If this segment belongs to a focusable element (link).
    pub(crate) focusable: Option<InlineFocusable>,
}

/// Metadata for a focusable inline element. Carries the scene
/// `NodeId` of the owning Link or Button so `render_inline_lines` can
/// write `focusable_screen_rect` directly on the scene node.
#[derive(Debug, Clone, Copy)]
pub(crate) struct InlineFocusable {
    pub(crate) node_id: crate::compositor::scene::NodeId,
}

/// A styled span within a single wrapped line.
#[derive(Debug, Clone)]
pub(crate) struct InlineSpan {
    text: String,
    width: usize,
    style: CellStyle,
    focusable: Option<InlineFocusable>,
}

/// A single wrapped line composed of styled spans.
#[derive(Debug)]
pub(crate) struct InlineLine {
    spans: Vec<InlineSpan>,
    width: usize,
}

pub(crate) struct InlineLines {
    values: Vec<InlineLine>,
    _lease: Option<BudgetLease>,
}

impl std::ops::Deref for InlineLines {
    type Target = [InlineLine];

    fn deref(&self) -> &Self::Target {
        &self.values
    }
}

impl InlineLines {
    #[cfg(test)]
    fn retained_capacity(&self) -> usize {
        self.values
            .capacity()
            .saturating_mul(std::mem::size_of::<InlineLine>())
            .saturating_add(
                self.values
                    .iter()
                    .map(|line| {
                        line.spans
                            .capacity()
                            .saturating_mul(std::mem::size_of::<InlineSpan>())
                            .saturating_add(
                                line.spans
                                    .iter()
                                    .map(|span| span.text.capacity())
                                    .sum::<usize>(),
                            )
                    })
                    .sum::<usize>(),
            )
    }
}

/// A word with style, extracted from inline segments for wrapping.
#[derive(Debug, Clone)]
struct StyledWord {
    text: String,
    width: usize,
    style: CellStyle,
    focusable: Option<InlineFocusable>,
    /// True if this word should be joined to the previous word without a space
    /// (e.g., a mid-word style change).
    join_prev: bool,
}

// Inline flattening lives in
// `super::kinds::try_collect_inline_segments` and reads
// `TextContent.runs` + node-bearing inline children.

struct InlineWrapRequirements {
    word_count: usize,
    line_bound: usize,
    span_bound: usize,
    payload_bound: usize,
}

fn inline_wrap_requirements(
    segments: &[InlineSegment],
    max_width: usize,
    wcfg: WidthConfig,
) -> Option<InlineWrapRequirements> {
    use unicode_segmentation::UnicodeSegmentation;

    let mut word_count = 0usize;
    let mut word_payload = 0usize;
    let mut forced_chunks = 0usize;
    for segment in segments {
        for word in segment.text.split_whitespace() {
            let width = display_width(word, wcfg);
            if width == 0 {
                continue;
            }
            word_count = word_count.checked_add(1)?;
            word_payload = word_payload.checked_add(word.len())?;
            if max_width > 0 && width > max_width {
                let mut chunk_width = 0usize;
                let mut chunk_has_content = false;
                for grapheme in word.graphemes(true) {
                    let grapheme_width = display_width(grapheme, wcfg);
                    if grapheme_width == 0 {
                        chunk_has_content = true;
                        continue;
                    }
                    if chunk_width.checked_add(grapheme_width)? > max_width && chunk_has_content {
                        forced_chunks = forced_chunks.checked_add(1)?;
                        chunk_width = 0;
                    }
                    chunk_width = chunk_width.checked_add(grapheme_width)?;
                    chunk_has_content = true;
                }
                if chunk_has_content {
                    forced_chunks = forced_chunks.checked_add(1)?;
                }
            }
        }
    }
    let line_bound = if max_width == 0 || word_count == 0 {
        1
    } else {
        word_count.checked_add(forced_chunks)?.max(1)
    };
    let span_bound = word_count
        .checked_mul(2)?
        .checked_add(forced_chunks)?
        .checked_add(segments.len())?
        .checked_add(1)?;
    let payload_bound = word_payload.checked_mul(2)?.checked_add(word_count)?;
    Some(InlineWrapRequirements {
        word_count,
        line_bound,
        span_bound,
        payload_bound,
    })
}

fn try_inline_string(value: &str) -> Option<String> {
    let mut copy = String::new();
    copy.try_reserve_exact(value.len()).ok()?;
    copy.push_str(value);
    Some(copy)
}

/// Split inline segments into styled words for wrapping.
fn segments_to_words(
    segments: &[InlineSegment],
    wcfg: WidthConfig,
    word_count: usize,
) -> Option<Vec<StyledWord>> {
    let mut words = Vec::new();
    words.try_reserve_exact(word_count).ok()?;

    for (seg_idx, seg) in segments.iter().enumerate() {
        let text = &seg.text;

        // Track whether the segment starts/ends with whitespace, which affects
        // how words join across segment boundaries.
        let starts_with_ws = text.starts_with(|c: char| c.is_whitespace());
        let ends_with_ws = text.ends_with(|c: char| c.is_whitespace());

        for (word_idx, word) in text.split_whitespace().enumerate() {
            let w = display_width(word, wcfg);
            if w == 0 {
                continue;
            }

            // A word joins to the previous without a space when:
            // - It's the first word in a non-first segment
            // - AND the segment doesn't start with whitespace
            // - AND there was a previous word (segments could be empty)
            let join_prev = word_idx == 0
                && seg_idx > 0
                && !starts_with_ws
                && !words.is_empty()
                && prev_segment_ends_without_ws(segments, seg_idx);

            words.push(StyledWord {
                text: try_inline_string(word)?,
                width: w,
                style: seg.style.clone(),
                focusable: seg.focusable,
                join_prev,
            });
        }

        // If the segment was entirely whitespace or empty, and it's between
        // other segments, it acts as a word separator (which is the default).
        // If a segment ends with whitespace, the next segment's first word
        // won't join. This is handled by `starts_with_ws` check above, but
        // we also need to record that *this* segment ended with whitespace.
        // We use a helper function below for that.
        let _ = ends_with_ws; // used implicitly by prev_segment_ends_without_ws
    }

    Some(words)
}

/// Check if the segment before `seg_idx` ends without whitespace.
fn prev_segment_ends_without_ws(segments: &[InlineSegment], seg_idx: usize) -> bool {
    let Some(prev) = seg_idx.checked_sub(1).and_then(|index| segments.get(index)) else {
        return false;
    };
    !prev.text.is_empty() && !prev.text.ends_with(|c: char| c.is_whitespace())
}

/// Word-wrap styled words into lines, preserving style per-span. The scratch
/// word collection and the returned outer/nested line storage share one
/// conservative pre-admission; after words drop the lease is reconciled to the
/// exact line, span-vector, and string capacities retained for rendering.
pub(crate) fn try_wrap_inline_segments(
    segments: &[InlineSegment],
    max_width: usize,
    wcfg: WidthConfig,
    governor: Option<&ResourceGovernor>,
) -> Option<InlineLines> {
    let requirements = inline_wrap_requirements(segments, max_width, wcfg)?;
    let requested = requirements
        .word_count
        .checked_mul(std::mem::size_of::<StyledWord>())?
        .checked_add(
            requirements
                .line_bound
                .checked_mul(std::mem::size_of::<InlineLine>())?,
        )?
        .checked_add(
            requirements
                .span_bound
                .checked_mul(std::mem::size_of::<InlineSpan>())?,
        )?
        .checked_add(requirements.payload_bound)?;
    let mut lease = match (requested, governor) {
        (0, _) | (_, None) => None,
        (bytes, Some(governor)) => Some(
            governor
                .reserve(ResourceCategory::RemoteCollections, bytes)
                .ok()?,
        ),
    };
    let words = segments_to_words(segments, wcfg, requirements.word_count)?;
    let mut lines: Vec<InlineLine> = Vec::new();
    lines.try_reserve_exact(requirements.line_bound).ok()?;
    let mut current_spans: Vec<InlineSpan> = Vec::new();
    let mut current_width: usize = 0;

    if max_width == 0 || words.is_empty() {
        try_push_line(&mut lines, empty_inline_line())?;
    }

    for word in words.iter().filter(|_| max_width > 0) {
        if word.join_prev {
            // Join to previous word without a space
            let needed = current_width + word.width;
            if needed <= max_width {
                // Append to current line
                push_or_merge_span(&mut current_spans, word)?;
                current_width = needed;
            } else {
                // Wrap: finish current line, start new one with this word
                if !current_spans.is_empty() {
                    try_push_line(
                        &mut lines,
                        InlineLine {
                            spans: std::mem::take(&mut current_spans),
                            width: current_width,
                        },
                    )?;
                    current_width = 0;
                }
                // Force-break if single word wider than max
                if word.width > max_width {
                    force_break_styled_word(
                        word,
                        max_width,
                        &mut lines,
                        &mut current_spans,
                        &mut current_width,
                        wcfg,
                    )?;
                } else {
                    try_push_span(
                        &mut current_spans,
                        InlineSpan {
                            text: try_inline_string(&word.text)?,
                            width: word.width,
                            style: word.style.clone(),
                            focusable: word.focusable,
                        },
                    )?;
                    current_width = word.width;
                }
            }
        } else {
            // Normal word: add space separator if line isn't empty
            if current_spans.is_empty() {
                // First word on line
                if word.width > max_width {
                    force_break_styled_word(
                        word,
                        max_width,
                        &mut lines,
                        &mut current_spans,
                        &mut current_width,
                        wcfg,
                    )?;
                } else {
                    try_push_span(
                        &mut current_spans,
                        InlineSpan {
                            text: try_inline_string(&word.text)?,
                            width: word.width,
                            style: word.style.clone(),
                            focusable: word.focusable,
                        },
                    )?;
                    current_width = word.width;
                }
            } else {
                let needed = current_width + 1 + word.width;
                if needed <= max_width {
                    // Add space separator then word.
                    // If the previous span and next word share the same focusable
                    // context, append space to the previous span. Otherwise, add
                    // a neutral space span to avoid inflating the focusable region.
                    let last_foc_id = current_spans
                        .last()
                        .and_then(|s| s.focusable.as_ref().map(|f| f.node_id));
                    let word_foc_id = word.focusable.as_ref().map(|f| f.node_id);

                    if last_foc_id == word_foc_id {
                        // Same context — merge space into last span
                        if let Some(last) = current_spans.last_mut() {
                            last.text.try_reserve(1).ok()?;
                            last.text.push(' ');
                            last.width += 1;
                        }
                    } else {
                        // Different context — the separator space belongs to
                        // neither link, so strip underline/strikethrough to
                        // prevent decoration bleed in either direction.
                        let mut space_style = word.style.clone();
                        space_style.underline = false;
                        space_style.strikethrough = false;
                        try_push_span(
                            &mut current_spans,
                            InlineSpan {
                                text: try_inline_string(" ")?,
                                width: 1,
                                style: space_style,
                                focusable: None,
                            },
                        )?;
                    }
                    current_width += 1;
                    push_or_merge_span(&mut current_spans, word)?;
                    current_width += word.width;
                } else {
                    // Wrap
                    try_push_line(
                        &mut lines,
                        InlineLine {
                            spans: std::mem::take(&mut current_spans),
                            width: current_width,
                        },
                    )?;
                    current_width = 0;

                    if word.width > max_width {
                        force_break_styled_word(
                            word,
                            max_width,
                            &mut lines,
                            &mut current_spans,
                            &mut current_width,
                            wcfg,
                        )?;
                    } else {
                        try_push_span(
                            &mut current_spans,
                            InlineSpan {
                                text: try_inline_string(&word.text)?,
                                width: word.width,
                                style: word.style.clone(),
                                focusable: word.focusable,
                            },
                        )?;
                        current_width = word.width;
                    }
                }
            }
        }
    }

    if !current_spans.is_empty() {
        try_push_line(
            &mut lines,
            InlineLine {
                spans: current_spans,
                width: current_width,
            },
        )?;
    }

    if lines.is_empty() {
        try_push_line(&mut lines, empty_inline_line())?;
    }

    drop(words);
    let retained = lines
        .capacity()
        .checked_mul(std::mem::size_of::<InlineLine>())?
        .checked_add(lines.iter().try_fold(0usize, |total, line| {
            total
                .checked_add(
                    line.spans
                        .capacity()
                        .checked_mul(std::mem::size_of::<InlineSpan>())?,
                )?
                .checked_add(line.spans.iter().try_fold(0usize, |payload, span| {
                    payload.checked_add(span.text.capacity())
                })?)
        })?)?;
    if let Some(lease) = lease.as_mut() {
        lease.try_resize_with_cost(retained, retained).ok()?;
    }
    Some(InlineLines {
        values: lines,
        _lease: lease,
    })
}

/// An empty inline line. Kept as one function so the single `Vec::new()`
/// standing for "no spans" is not repeated; it is never grown, so it never
/// allocates.
fn empty_inline_line() -> InlineLine {
    InlineLine {
        spans: Vec::new(),
        width: 0,
    }
}

fn try_push_line(lines: &mut Vec<InlineLine>, line: InlineLine) -> Option<()> {
    lines.try_reserve(1).ok()?;
    lines.push(line);
    Some(())
}

fn try_push_span(spans: &mut Vec<InlineSpan>, span: InlineSpan) -> Option<()> {
    spans.try_reserve(1).ok()?;
    spans.push(span);
    Some(())
}

/// Push a word as a new span, or merge it into the last span if style matches.
fn push_or_merge_span(spans: &mut Vec<InlineSpan>, word: &StyledWord) -> Option<()> {
    if let Some(last) = spans.last_mut()
        && last.style == word.style
        && last.focusable.as_ref().map(|f| f.node_id) == word.focusable.as_ref().map(|f| f.node_id)
    {
        last.text.try_reserve(word.text.len()).ok()?;
        last.text.push_str(&word.text);
        last.width += word.width;
        return Some(());
    }
    try_push_span(
        spans,
        InlineSpan {
            text: try_inline_string(&word.text)?,
            width: word.width,
            style: word.style.clone(),
            focusable: word.focusable,
        },
    )
}

/// Force-break a word that's wider than max_width into multiple lines.
fn force_break_styled_word(
    word: &StyledWord,
    max_width: usize,
    lines: &mut Vec<InlineLine>,
    current_spans: &mut Vec<InlineSpan>,
    current_width: &mut usize,
    wcfg: WidthConfig,
) -> Option<()> {
    use unicode_segmentation::UnicodeSegmentation;

    let mut chunk = String::new();
    let mut chunk_width: usize = 0;

    for grapheme in word.text.graphemes(true) {
        let gw = display_width(grapheme, wcfg);
        if gw == 0 {
            chunk.try_reserve(grapheme.len()).ok()?;
            chunk.push_str(grapheme);
            continue;
        }

        if chunk_width + gw > max_width {
            if !chunk.is_empty() {
                let span = InlineSpan {
                    text: std::mem::take(&mut chunk),
                    width: chunk_width,
                    style: word.style.clone(),
                    focusable: word.focusable,
                };
                let mut spans = Vec::new();
                spans.try_reserve_exact(1).ok()?;
                spans.push(span);
                try_push_line(
                    lines,
                    InlineLine {
                        spans,
                        width: chunk_width,
                    },
                )?;
            }
            chunk.clear();
            chunk_width = 0;
        }

        chunk.try_reserve(grapheme.len()).ok()?;
        chunk.push_str(grapheme);
        chunk_width += gw;
    }

    // Remainder goes into current_spans for the next line
    if !chunk.is_empty() {
        try_push_span(
            current_spans,
            InlineSpan {
                text: chunk,
                width: chunk_width,
                style: word.style.clone(),
                focusable: word.focusable,
            },
        )?;
        *current_width = chunk_width;
    }
    Some(())
}

/// Render wrapped inline lines to the cell buffer. Writes each
/// focusable span's screen rect to the scene via
/// `Scene::update_focusable_rect`, first-occurrence-wins per NodeId
/// (wrapped inline links pick up the first line's rect, matching
/// where Tab focus highlights).
/// Render inline-wrapped lines into `buf` at `ctx.x / ctx.y`. Callers
/// running in local coordinates pass `focusable_origin = (global_x,
/// global_y)` so the focusable rects written for inline `Link`/`Button`
/// spans still land at screen coordinates — Scene focus and hit-testing
/// both consume global coords. Callers whose `ctx` is already global
/// (e.g. structural containers that haven't pivoted) pass `(0, 0)`.
pub(crate) fn render_inline_lines_with_origin(
    buf: &mut CellBuffer,
    ctx: &mut LayoutCtx,
    scene: &mut crate::compositor::scene::Scene,
    lines: &[InlineLine],
    align: Alignment,
    focusable_origin: (u16, u16),
) {
    let focusable_bound = match lines.iter().try_fold(0usize, |total, line| {
        total.checked_add(
            line.spans
                .iter()
                .filter(|span| span.focusable.is_some())
                .count(),
        )
    }) {
        Some(bound) => bound,
        None => {
            buf.record_allocation_failure();
            scene.record_resource_error();
            return;
        }
    };
    let Some(mut seen_nodes) =
        try_temp_vec::<crate::compositor::scene::NodeId>(focusable_bound, ctx.governor.as_ref())
    else {
        buf.record_allocation_failure();
        scene.record_resource_error();
        return;
    };

    for line in lines {
        buf.ensure_height(ctx.y.saturating_add(1));

        let x_offset = match align {
            Alignment::Left => 0,
            Alignment::Center => ((ctx.width as usize).saturating_sub(line.width) / 2) as u16,
            Alignment::Right => (ctx.width as usize).saturating_sub(line.width) as u16,
        };

        let mut col = ctx.x.saturating_add(x_offset);
        let max_col = ctx.x.saturating_add(ctx.width);

        let mut current_focusable: Option<crate::compositor::scene::NodeId> = None;
        let mut focusable_start_col: u16 = 0;

        for span in &line.spans {
            let span_start_col = col;
            put_str_clipped(buf, col, ctx.y, &span.text, &span.style, max_col, ctx.wcfg);
            col += span.width as u16;

            let span_foc = span.focusable.as_ref().map(|f| f.node_id);
            if span_foc != current_focusable {
                if let Some(node_id) = current_focusable {
                    let width = span_start_col.saturating_sub(focusable_start_col);
                    if width > 0 && !seen_nodes.values.contains(&node_id) {
                        seen_nodes.values.push(node_id);
                        scene.update_focusable_rect(
                            node_id,
                            Rect::new(
                                focusable_start_col.saturating_add(focusable_origin.0),
                                ctx.y.saturating_add(focusable_origin.1),
                                width,
                                1,
                            ),
                        );
                    }
                }
                current_focusable = span_foc;
                focusable_start_col = span_start_col;
            }
        }

        if let Some(node_id) = current_focusable {
            let width = col.saturating_sub(focusable_start_col);
            if width > 0 && !seen_nodes.values.contains(&node_id) {
                seen_nodes.values.push(node_id);
                scene.update_focusable_rect(
                    node_id,
                    Rect::new(
                        focusable_start_col.saturating_add(focusable_origin.0),
                        ctx.y.saturating_add(focusable_origin.1),
                        width,
                        1,
                    ),
                );
            }
        }

        ctx.y = ctx.y.saturating_add(1);
    }
}

// ─── Helpers ─────────────────────────────────────────────────

/// Resolve a color, falling back to the inherited value.
pub(crate) fn resolve_or_inherit(
    color: Option<&Color>,
    inherited: Option<ResolvedColor>,
    support: ColorSupport,
) -> Option<ResolvedColor> {
    if let Some(c) = color {
        c.resolve(support)
    } else {
        inherited
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser;
    use crate::scanner::Scanner;

    fn parse_and_layout(input: &str, width: u16, height: u16) -> CellBuffer {
        let mut scanner = Scanner::new(input.as_bytes()).unwrap();
        let tokens = scanner.scan_all().unwrap();
        let result = parser::parse(tokens);
        let doc = result.document.expect("parse failed");
        let mut scene = crate::compositor::scene::build::from_document(&doc);
        let page_buf = layout_scene(
            &mut scene,
            width,
            height,
            ColorSupport::Truecolor,
            WidthConfig::default(),
        )
        .buffer;
        // Compose the final frame via the scene walk — post-Phase-1 the
        // composite pass owns the pixels, and post-Phase-2 (sub-phase
        // pivot of Pre etc.) some kinds write into per-node buffers
        // rather than `page.buf`. Tests assert on the composited output,
        // which is what users see. The output dimensions must match
        // `page_buf` (authoritative for `mode=screen` cols/rows).
        let anim_rt = crate::compositor::animate::AnimationRuntime::new(Vec::new());
        crate::compositor::composite::walk(&scene, &anim_rt, page_buf.width, page_buf.height)
    }

    fn row_text(buf: &CellBuffer, y: u16) -> String {
        if let Some(row) = buf.row(y) {
            row.iter()
                .map(|c| if c.ch == '\0' { ' ' } else { c.ch })
                .collect::<String>()
                .trim_end()
                .to_string()
        } else {
            String::new()
        }
    }

    #[test]
    fn governed_page_canvas_owns_exact_lease_until_drop() {
        let input = "[page mode=screen cols=20 rows=5][text]owned[/text][/page]";
        let mut scanner = Scanner::new(input.as_bytes()).unwrap();
        let tokens = scanner.scan_all().unwrap();
        let doc = parser::parse(tokens).document.expect("parse failed");
        let mut scene = crate::compositor::scene::build::from_document(&doc);
        let governor = ResourceGovernor::new();

        let result = layout_scene_governed(
            &mut scene,
            80,
            24,
            ColorSupport::Truecolor,
            WidthConfig::default(),
            &governor,
        );
        let expected = result
            .buffer
            .cell_count()
            .saturating_mul(std::mem::size_of::<crate::compositor::layout::cell::Cell>());
        assert_eq!(governor.used(ResourceCategory::CompositorCells), expected);

        drop(result);
        assert_eq!(governor.used(ResourceCategory::CompositorCells), 0);
    }

    #[test]
    fn governed_layout_metadata_owns_exact_capacity_until_drop() {
        let input = r#"[page mode=document]
            [input name="first" /]
            [button action=submit]Send[/button]
            [panel id="panel-with-capacity" state="a"]
                [state name="a"][text]A[/text][/state]
            [/panel]
            [live id="live-with-capacity" endpoint="/endpoint-with-capacity"][/live]
            [nav sticky=bottom][text]Footer[/text][/nav]
        [/page]"#;
        let mut scanner = Scanner::new(input.as_bytes()).unwrap();
        let tokens = scanner.scan_all().unwrap();
        let doc = parser::parse(tokens).document.expect("parse failed");
        let mut scene = crate::compositor::scene::build::from_document(&doc);
        let governor = ResourceGovernor::new();

        let result = layout_scene_governed(
            &mut scene,
            40,
            10,
            ColorSupport::Truecolor,
            WidthConfig::default(),
            &governor,
        );
        assert_eq!(result.focusable_positions.len(), 2);
        assert_eq!(result.placed.len(), 2);
        assert_eq!(result.sticky_regions.len(), 1);
        let expected = result
            .focusable_positions
            .capacity()
            .saturating_mul(std::mem::size_of::<FocusablePos>())
            .saturating_add(
                result
                    .placed
                    .capacity()
                    .saturating_mul(std::mem::size_of::<PlacedElement>()),
            )
            .saturating_add(
                result
                    .sticky_regions
                    .capacity()
                    .saturating_mul(std::mem::size_of::<StickyRegion>()),
            )
            .saturating_add(
                result
                    .placed
                    .iter()
                    .map(PlacedElement::retained_string_capacity)
                    .sum::<usize>(),
            );
        assert_eq!(governor.used(ResourceCategory::RemoteCollections), expected);

        drop(result);
        assert_eq!(governor.used(ResourceCategory::RemoteCollections), 0);
    }

    #[test]
    fn layout_metadata_rejection_marks_the_buffer_and_empties_the_projection() {
        use crate::compositor::layout::{LayoutAllocationSite, LayoutRejectionGuard};
        let input = r#"[page mode=document]
            [input name="first" /]
            [button action=submit]Send[/button]
        [/page]"#;
        let mut scanner = Scanner::new(input.as_bytes()).unwrap();
        let tokens = scanner.scan_all().unwrap();
        let doc = parser::parse(tokens).document.expect("parse failed");
        let governor = ResourceGovernor::new();
        let mut scene = crate::compositor::scene::build::from_document_governed(&doc, &governor);

        let rejection = LayoutRejectionGuard::at(LayoutAllocationSite::FixedMetadata);
        let refused = layout_scene_governed(
            &mut scene,
            40,
            10,
            ColorSupport::Truecolor,
            WidthConfig::default(),
            &governor,
        );
        assert!(
            refused.buffer.allocation_failed(),
            "a refused metadata admission must mark the buffer"
        );
        assert!(refused.placed.is_empty());
        assert!(refused.focusable_positions.is_empty());
        assert!(refused.sticky_regions.is_empty());
        drop(refused);
        drop(rejection);

        let accepted = layout_scene_governed(
            &mut scene,
            40,
            10,
            ColorSupport::Truecolor,
            WidthConfig::default(),
            &governor,
        );
        assert!(!accepted.buffer.allocation_failed());
        assert!(!accepted.focusable_positions.is_empty());
    }

    #[test]
    fn governed_layout_metadata_budget_failure_rolls_back() {
        let input = r#"[page mode=document]
            [input name="first" /]
            [button action=submit]Send[/button]
            [nav sticky=bottom][text]Footer[/text][/nav]
        [/page]"#;
        let mut scanner = Scanner::new(input.as_bytes()).unwrap();
        let tokens = scanner.scan_all().unwrap();
        let doc = parser::parse(tokens).document.expect("parse failed");
        let mut scene = crate::compositor::scene::build::from_document(&doc);
        let _ = layout_scene(
            &mut scene,
            40,
            10,
            ColorSupport::Truecolor,
            WidthConfig::default(),
        );
        let governor = ResourceGovernor::new();
        let blocker = governor
            .reserve(
                ResourceCategory::RemoteCollections,
                crate::resource::MAX_REMOTE_MEMORY - 1,
            )
            .unwrap();
        let before = governor.used(ResourceCategory::RemoteCollections);

        assert!(governed_fixed_metadata(&scene, Some(&governor)).is_none());
        assert_eq!(governor.used(ResourceCategory::RemoteCollections), before);

        drop(blocker);
        assert_eq!(governor.used(ResourceCategory::RemoteCollections), 0);
    }

    #[test]
    fn governed_layout_node_ids_hold_exact_temporary_capacity() {
        let governor = ResourceGovernor::new();
        let ids = try_temp_vec::<crate::compositor::scene::NodeId>(4, Some(&governor)).unwrap();
        let expected = ids
            .values
            .capacity()
            .saturating_mul(std::mem::size_of::<crate::compositor::scene::NodeId>());
        assert_eq!(governor.used(ResourceCategory::RemoteCollections), expected);
        drop(ids);
        assert_eq!(governor.used(ResourceCategory::RemoteCollections), 0);
    }

    #[test]
    fn governed_layout_node_id_rejection_preserves_existing_usage() {
        let governor = ResourceGovernor::new();
        let blocker = governor
            .reserve(
                ResourceCategory::RemoteCollections,
                crate::resource::MAX_REMOTE_MEMORY,
            )
            .unwrap();

        assert!(try_temp_vec::<crate::compositor::scene::NodeId>(1, Some(&governor)).is_none());
        assert_eq!(
            governor.used(ResourceCategory::RemoteCollections),
            blocker.amount()
        );
        drop(blocker);
        assert_eq!(governor.used(ResourceCategory::RemoteCollections), 0);
    }

    #[test]
    fn governed_inline_segments_hold_exact_nested_string_capacity() {
        let input = r#"[page mode=document]
            [text][text bold]alpha beta[/text][button action=submit]Go[/button][/text]
        [/page]"#;
        let mut scanner = Scanner::new(input.as_bytes()).unwrap();
        let tokens = scanner.scan_all().unwrap();
        let doc = parser::parse(tokens).document.expect("parse failed");
        let scene = crate::compositor::scene::build::from_document(&doc);
        let text_node = scene.get(scene.root()).unwrap().children()[0];
        let governor = ResourceGovernor::new();
        let segments = crate::compositor::layout::kinds::try_collect_inline_segments(
            &[text_node],
            &scene,
            &CellStyle::default(),
            ColorSupport::Truecolor,
            None,
            Some(&governor),
        )
        .unwrap();

        assert_eq!(segments.len(), 2);
        assert_eq!(
            governor.used(ResourceCategory::RemoteCollections),
            segments.retained_capacity()
        );
        drop(segments);
        assert_eq!(governor.used(ResourceCategory::RemoteCollections), 0);
    }

    #[test]
    fn governed_inline_segment_payload_rejection_rolls_back() {
        let input = r#"[page mode=document]
            [text][text bold]a remotely supplied inline payload[/text][/text]
        [/page]"#;
        let mut scanner = Scanner::new(input.as_bytes()).unwrap();
        let tokens = scanner.scan_all().unwrap();
        let doc = parser::parse(tokens).document.expect("parse failed");
        let scene = crate::compositor::scene::build::from_document(&doc);
        let text_node = scene.get(scene.root()).unwrap().children()[0];
        let governor = ResourceGovernor::new();
        let blocker = governor
            .reserve(
                ResourceCategory::RemoteCollections,
                crate::resource::MAX_REMOTE_MEMORY - 1,
            )
            .unwrap();
        let before = governor.used(ResourceCategory::RemoteCollections);

        assert!(
            crate::compositor::layout::kinds::try_collect_inline_segments(
                &[text_node],
                &scene,
                &CellStyle::default(),
                ColorSupport::Truecolor,
                None,
                Some(&governor),
            )
            .is_none()
        );
        assert_eq!(governor.used(ResourceCategory::RemoteCollections), before);
        drop(blocker);
        assert_eq!(governor.used(ResourceCategory::RemoteCollections), 0);
    }

    #[test]
    fn governed_force_break_lines_hold_exact_nested_capacity() {
        let segments = vec![InlineSegment {
            text: "abcdefghijk".to_string(),
            style: CellStyle::default(),
            focusable: None,
        }];
        let governor = ResourceGovernor::new();
        let lines = try_wrap_inline_segments(&segments, 3, WidthConfig::default(), Some(&governor))
            .unwrap();

        assert_eq!(lines.len(), 4);
        assert!(lines.iter().all(|line| line.spans.len() == 1));
        assert_eq!(
            governor.used(ResourceCategory::RemoteCollections),
            lines.retained_capacity()
        );
        drop(lines);
        assert_eq!(governor.used(ResourceCategory::RemoteCollections), 0);
    }

    #[test]
    fn governed_inline_focus_tracking_rejects_without_partial_focus() {
        let input = r#"[page mode=document][link href="/next"][text]next[/text][/link][/page]"#;
        let mut scanner = Scanner::new(input.as_bytes()).unwrap();
        let tokens = scanner.scan_all().unwrap();
        let doc = parser::parse(tokens).document.expect("parse failed");
        let mut scene = crate::compositor::scene::build::from_document(&doc);
        let link = scene
            .iter_tree_order()
            .find(|node| matches!(node.kind(), crate::compositor::scene::NodeKind::Link(_)))
            .unwrap()
            .id();
        let segments = vec![InlineSegment {
            text: "next".to_string(),
            style: CellStyle::default(),
            focusable: Some(InlineFocusable { node_id: link }),
        }];
        let lines = try_wrap_inline_segments(&segments, 10, WidthConfig::default(), None).unwrap();
        let governor = ResourceGovernor::new();
        let blocker = governor
            .reserve(
                ResourceCategory::RemoteCollections,
                crate::resource::MAX_REMOTE_MEMORY,
            )
            .unwrap();
        let mut buffer = CellBuffer::new(10, 1);
        let mut ctx = LayoutCtx {
            x: 0,
            y: 0,
            width: 10,
            viewport_height: 1,
            color_support: ColorSupport::Truecolor,
            wcfg: WidthConfig::default(),
            style: CellStyle::default(),
            governor: Some(governor.clone()),
        };

        render_inline_lines_with_origin(
            &mut buffer,
            &mut ctx,
            &mut scene,
            &lines,
            Alignment::Left,
            (0, 0),
        );

        assert!(buffer.allocation_failed());
        assert!(scene.resource_limit_exceeded());
        assert!(scene.get(link).unwrap().focusable_screen_rect().is_none());
        assert_eq!(
            governor.used(ResourceCategory::RemoteCollections),
            blocker.amount()
        );
        drop(blocker);
        assert_eq!(governor.used(ResourceCategory::RemoteCollections), 0);
    }

    #[test]
    fn governed_inline_temporaries_release_before_layout_result_returns() {
        let input = r#"[page mode=document]
            [text][text bold]alpha beta[/text] [link href="/x"][text italic]gamma delta epsilon[/text][/link][/text]
        [/page]"#;
        let mut scanner = Scanner::new(input.as_bytes()).unwrap();
        let tokens = scanner.scan_all().unwrap();
        let doc = parser::parse(tokens).document.expect("parse failed");
        let governor = ResourceGovernor::new();
        let mut scene = crate::compositor::scene::build::from_document_governed(&doc, &governor);
        let scene_metadata = governor.used(ResourceCategory::RemoteCollections);
        let result = layout_scene_governed(
            &mut scene,
            9,
            8,
            ColorSupport::Truecolor,
            WidthConfig::default(),
            &governor,
        );
        assert!(!result.buffer.allocation_failed());
        let retained_metadata = result
            .focusable_positions
            .capacity()
            .saturating_mul(std::mem::size_of::<FocusablePos>());
        assert_eq!(
            governor.used(ResourceCategory::RemoteCollections),
            scene_metadata + retained_metadata
        );
        drop(result);
        assert_eq!(
            governor.used(ResourceCategory::RemoteCollections),
            scene_metadata
        );
        drop(scene);
        assert_eq!(governor.used(ResourceCategory::RemoteCollections), 0);
    }

    #[test]
    fn governed_layout_metadata_preadmits_placed_string_payloads() {
        let input = r#"[page mode=document]
            [panel id="panel-with-capacity" state="a"]
                [state name="a"][text]A[/text][/state]
            [/panel]
            [live id="live-with-capacity" endpoint="/endpoint-with-capacity"][/live]
        [/page]"#;
        let mut scanner = Scanner::new(input.as_bytes()).unwrap();
        let tokens = scanner.scan_all().unwrap();
        let doc = parser::parse(tokens).document.expect("parse failed");
        let mut scene = crate::compositor::scene::build::from_document(&doc);
        let _ = layout_scene(
            &mut scene,
            40,
            10,
            ColorSupport::Truecolor,
            WidthConfig::default(),
        );
        let (placed_count, string_bound) = scene.placed_storage_requirements(false).unwrap();
        assert!(string_bound > 0);
        let structural_bytes = placed_count * std::mem::size_of::<PlacedElement>();
        let governor = ResourceGovernor::new();
        let blocker = governor
            .reserve(
                ResourceCategory::RemoteCollections,
                crate::resource::MAX_REMOTE_MEMORY - structural_bytes,
            )
            .unwrap();
        let before = governor.used(ResourceCategory::RemoteCollections);

        assert!(governed_fixed_metadata(&scene, Some(&governor)).is_none());
        assert_eq!(governor.used(ResourceCategory::RemoteCollections), before);

        drop(blocker);
        assert_eq!(governor.used(ResourceCategory::RemoteCollections), 0);
    }

    // ─── Text Layout ─────────────────────────────────────────

    #[test]
    fn simple_text() {
        let buf = parse_and_layout(
            "[page mode=document][text]Hello world[/text][/page]",
            40,
            10,
        );
        assert_eq!(row_text(&buf, 0), "Hello world");
    }

    #[test]
    fn text_wrapping() {
        let buf = parse_and_layout(
            "[page mode=document][text]The quick brown fox jumps over the lazy dog[/text][/page]",
            20,
            10,
        );
        let line0 = row_text(&buf, 0);
        let line1 = row_text(&buf, 1);
        assert!(!line0.is_empty());
        assert!(!line1.is_empty());
        assert!(display_width(&line0, WidthConfig::default()) <= 20);
        assert!(display_width(&line1, WidthConfig::default()) <= 20);
    }

    #[test]
    fn text_centered() {
        let buf = parse_and_layout(
            "[page mode=document][text align=center]Hi[/text][/page]",
            20,
            5,
        );
        let line = row_text(&buf, 0);
        // "Hi" should be centered in 20 cols = 9 spaces + "Hi"
        assert!(line.starts_with("         Hi") || line.starts_with("        Hi"));
    }

    #[test]
    fn text_right_aligned() {
        let buf = parse_and_layout(
            "[page mode=document][text align=right]Hi[/text][/page]",
            20,
            5,
        );
        let line = row_text(&buf, 0);
        // "Hi" at right edge: 18 spaces + "Hi"
        assert!(line.ends_with("Hi"));
        assert!(line.len() >= 18);
    }

    #[test]
    fn text_with_color() {
        let buf = parse_and_layout(
            "[page mode=document][text fg=red bold]Red[/text][/page]",
            40,
            10,
        );
        let cell = buf.get(0, 0).unwrap();
        assert_eq!(cell.ch, 'R');
        assert!(cell.style.bold);
        assert!(cell.style.fg.is_some());
    }

    // ─── Pre Layout ──────────────────────────────────────────

    #[test]
    fn pre_preserves_whitespace() {
        let buf = parse_and_layout(
            "[page mode=document][pre]  hello  \n  world  [/pre][/page]",
            40,
            10,
        );
        let line0 = row_text(&buf, 0);
        assert!(line0.starts_with("  hello"));
    }

    // ─── Heading Layout ──────────────────────────────────────

    #[test]
    fn heading_is_bold() {
        let buf = parse_and_layout(
            "[page mode=document][heading level=2]Title[/heading][/page]",
            40,
            10,
        );
        let cell = buf.get(0, 0).unwrap();
        assert_eq!(cell.ch, 'T');
        assert!(cell.style.bold);
    }

    #[test]
    fn heading_level_1_has_underline() {
        let buf = parse_and_layout(
            "[page mode=document][heading level=1]Title[/heading][/page]",
            40,
            10,
        );
        // Row 0: "Title", Row 1: underline
        assert_eq!(row_text(&buf, 0), "Title");
        let underline_char = buf.get(0, 1).unwrap().ch;
        assert_eq!(underline_char, '━'); // Heavy HR style
    }

    // ─── Hr Layout ───────────────────────────────────────────

    #[test]
    fn hr_draws_across() {
        let buf = parse_and_layout(
            "[page mode=document][hr style=dash fg=yellow /][/page]",
            10,
            5,
        );
        for x in 0..10 {
            assert_eq!(buf.get(x, 0).unwrap().ch, '╌');
        }
    }

    // ─── Spacer Layout ───────────────────────────────────────

    #[test]
    fn spacer_advances_cursor() {
        let buf = parse_and_layout(
            "[page mode=document][text]before[/text][spacer lines=3 /][text]after[/text][/page]",
            40,
            10,
        );
        assert_eq!(row_text(&buf, 0), "before");
        assert_eq!(row_text(&buf, 1), ""); // spacer
        assert_eq!(row_text(&buf, 2), ""); // spacer
        assert_eq!(row_text(&buf, 3), ""); // spacer
        assert_eq!(row_text(&buf, 4), "after");
    }

    // ─── Box Layout ──────────────────────────────────────────

    #[test]
    fn box_with_border() {
        let buf = parse_and_layout(
            "[page mode=document][box w=10 h=5 border=single][text]Hi[/text][/box][/page]",
            40,
            10,
        );
        // Top-left corner
        assert_eq!(buf.get(0, 0).unwrap().ch, '┌');
        // Top-right corner
        assert_eq!(buf.get(9, 0).unwrap().ch, '┐');
        // Bottom-left
        assert_eq!(buf.get(0, 4).unwrap().ch, '└');
        // Content inside (border row 0, padding row 1, content row 2)
        let content_row = row_text(&buf, 2);
        assert!(content_row.contains("Hi"));
    }

    #[test]
    fn box_with_title() {
        let buf = parse_and_layout(
            "[page mode=document][box w=20 h=3 border=double title=\"Info\"][/box][/page]",
            40,
            10,
        );
        let top_row = row_text(&buf, 0);
        assert!(top_row.contains("Info"));
        assert_eq!(buf.get(0, 0).unwrap().ch, '╔');
    }

    #[test]
    fn box_fit_height() {
        let buf = parse_and_layout(
            "[page mode=document][box w=20 h=fit border=single][text]Content here[/text][/box][text]after[/text][/page]",
            40,
            20,
        );
        // The box should fit around "Content here" + border + padding
        // After the box, "after" should appear
        let mut found_after = false;
        for y in 0..buf.height {
            if row_text(&buf, y) == "after" {
                found_after = true;
                break;
            }
        }
        assert!(found_after, "text after box should be visible");
    }

    #[test]
    fn nested_boxes() {
        let buf = parse_and_layout(
            "[page mode=document][box w=30 h=8 border=single][box w=20 h=4 border=double][text]inner[/text][/box][/box][/page]",
            40,
            15,
        );
        // Outer box top-left
        assert_eq!(buf.get(0, 0).unwrap().ch, '┌');
        // Inner box should be inside
        let mut found_double = false;
        for y in 0..buf.height {
            for x in 0..buf.width {
                if buf.get(x, y).unwrap().ch == '╔' {
                    found_double = true;
                }
            }
        }
        assert!(found_double, "inner double-border box should be visible");
    }

    // ─── Row/Col Layout ──────────────────────────────────────

    #[test]
    fn two_columns() {
        let buf = parse_and_layout(
            "[page mode=document][row gap=1][col w=10][text]LEFT[/text][/col][col w=10][text]RIGHT[/text][/col][/row][/page]",
            40,
            10,
        );
        let line = row_text(&buf, 0);
        assert!(line.contains("LEFT"));
        assert!(line.contains("RIGHT"));
        // LEFT should be at col 0, RIGHT at col 11
        assert_eq!(buf.get(0, 0).unwrap().ch, 'L');
        assert_eq!(buf.get(11, 0).unwrap().ch, 'R');
    }

    #[test]
    fn fill_columns() {
        let buf = parse_and_layout(
            "[page mode=document][row gap=2][col]A[/col][col]B[/col][/row][/page]",
            20,
            10,
        );
        // Two fill columns in 20 width with gap 2 = 9 each
        assert_eq!(buf.get(0, 0).unwrap().ch, 'A');
        // B should start at 9 + 2 = 11
        assert_eq!(buf.get(11, 0).unwrap().ch, 'B');
    }

    // ─── List Layout ─────────────────────────────────────────

    #[test]
    fn bullet_list() {
        let buf = parse_and_layout(
            "[page mode=document][list style=bullet][item][text]First[/text][/item][item][text]Second[/text][/item][/list][/page]",
            40,
            10,
        );
        let line0 = row_text(&buf, 0);
        let line1 = row_text(&buf, 1);
        assert!(line0.contains('•'));
        assert!(line0.contains("First"));
        assert!(line1.contains('•'));
        assert!(line1.contains("Second"));
    }

    #[test]
    fn item_with_link_and_text_is_one_row() {
        // Regression: AML formatting whitespace between an item's child
        // elements used to be laid out as blank flow rows, blowing up the
        // item's height. The item's children (link + text) should flatten
        // to a single inline-wrapped row.
        let buf = parse_and_layout(
            r#"[page mode=document]
              [list style=bullet]
                [item]
                  [link href="/board"]
                    [text bold]Message Board[/text]
                  [/link]
                  [text dim] — read the latest posts[/text]
                [/item]
              [/list]
            [/page]"#,
            80,
            20,
        );
        let line0 = row_text(&buf, 0);
        assert!(
            line0.contains('•'),
            "row 0 should have bullet, got {line0:?}"
        );
        assert!(
            line0.contains("Message Board"),
            "row 0 should have link text, got {line0:?}"
        );
        assert!(
            line0.contains("read the latest posts"),
            "row 0 should have trailing text, got {line0:?}"
        );
        // Row 1 must be empty — the item used to spill over multiple rows.
        let line1 = row_text(&buf, 1);
        assert!(
            line1.trim().is_empty(),
            "row 1 should be empty, got {line1:?}"
        );
    }

    #[test]
    fn numbered_list() {
        let buf = parse_and_layout(
            "[page mode=document][list style=number][item][text]A[/text][/item][item][text]B[/text][/item][/list][/page]",
            40,
            10,
        );
        let line0 = row_text(&buf, 0);
        let line1 = row_text(&buf, 1);
        assert!(
            line0.starts_with("1. A"),
            "marker needs a space before content: {line0:?}"
        );
        assert!(
            line1.starts_with("2. B"),
            "marker needs a space before content: {line1:?}"
        );
    }

    #[test]
    fn numbered_list_aligns_single_and_double_digit_markers() {
        let items = (b'A'..=b'J')
            .map(|ch| format!("[item][text]{}[/text][/item]", ch as char))
            .collect::<String>();
        let aml = format!("[page mode=document][list style=number]{items}[/list][/page]");
        let buf = parse_and_layout(&aml, 40, 20);
        let first = row_text(&buf, 0);
        let tenth = row_text(&buf, 9);
        assert!(
            first.starts_with(" 1. A"),
            "single digit marker should align: {first:?}"
        );
        assert!(
            tenth.starts_with("10. J"),
            "double digit marker should align: {tenth:?}"
        );
    }

    // ─── Table Layout ────────────────────────────────────────

    #[test]
    fn simple_table() {
        let buf = parse_and_layout(
            "[page mode=document][table][thead][tr][th]Name[/th][th]Score[/th][/tr][/thead][tbody][tr][td]Alice[/td][td]98[/td][/tr][/tbody][/table][/page]",
            40,
            10,
        );
        let header = row_text(&buf, 0);
        assert!(header.contains("Name"));
        assert!(header.contains("Score"));

        // Separator line
        let sep = row_text(&buf, 1);
        assert!(sep.contains('─'));

        // Data row
        let data = row_text(&buf, 2);
        assert!(data.contains("Alice"));
        assert!(data.contains("98"));
    }

    // ─── Interactive Elements ────────────────────────────────

    #[test]
    fn link_renders_children() {
        let buf = parse_and_layout(
            "[page mode=document][link href=\"atp://x\"][text]Click me[/text][/link][/page]",
            40,
            10,
        );
        let line = row_text(&buf, 0);
        assert!(line.contains("Click me"));
    }

    #[test]
    fn input_renders() {
        let buf = parse_and_layout(
            "[page mode=document][input name=\"msg\" placeholder=\"Type here\" /][/page]",
            40,
            10,
        );
        let line = row_text(&buf, 0);
        assert!(line.contains("Type here"));
    }

    #[test]
    fn button_renders() {
        let buf = parse_and_layout(
            "[page mode=document][button action=submit]Send[/button][/page]",
            40,
            10,
        );
        let line = row_text(&buf, 0);
        assert!(line.contains("Send"));
    }

    // ─── Screen Mode ─────────────────────────────────────────

    #[test]
    fn screen_mode_dimensions() {
        let buf = parse_and_layout(
            "[page mode=screen cols=80 rows=24][text]Hello[/text][/page]",
            120,
            40,
        );
        // Buffer should match declared dimensions, not terminal
        assert_eq!(buf.width, 80);
        assert_eq!(buf.height, 24);
    }

    #[test]
    fn screen_mode_absolute_position() {
        let buf = parse_and_layout(
            "[page mode=screen cols=40 rows=20][box x=5 y=3 w=10 h=5 border=single][/box][/page]",
            40,
            20,
        );
        assert_eq!(buf.get(5, 3).unwrap().ch, '┌');
        assert_eq!(buf.get(14, 3).unwrap().ch, '┐');
    }

    // ─── Buffer Growth ───────────────────────────────────────

    #[test]
    fn document_mode_grows_buffer() {
        let buf = parse_and_layout(
            "[page mode=document][text]Line 1[/text][spacer lines=20 /][text]Line 2[/text][/page]",
            40,
            5, // terminal is only 5 rows tall
        );
        // Buffer should have grown beyond 5 rows
        assert!(buf.height > 5);
        // Line 2 should be at row 22 (line1=row0, spacer=rows1-20, line2=row21)
        assert_eq!(row_text(&buf, 21), "Line 2");
    }

    // ─── Animation Placeholder ───────────────────────────────

    #[test]
    fn animate_shows_first_frame() {
        let buf = parse_and_layout(
            "[page mode=document][animate id=\"x\" fps=10][frame][text]Frame 1[/text][/frame][frame][text]Frame 2[/text][/frame][/animate][/page]",
            40,
            10,
        );
        let line = row_text(&buf, 0);
        assert!(line.contains("Frame 1"));
    }

    // ─── Realistic Document ──────────────────────────────────

    #[test]
    fn full_page_layout() {
        let input = r#"[page mode=document title="Test"]
  [heading level=1 fg=cyan]Welcome[/heading]
  [box w=30 h=fit border=double title="Info"]
    [text]This is a test page.[/text]
  [/box]
  [spacer lines=1 /]
  [hr style=dash /]
  [list style=bullet]
    [item][text]Item one[/text][/item]
    [item][text]Item two[/text][/item]
  [/list]
  [link href="atp://test"][text]Go somewhere[/text][/link]
[/page]"#;

        let buf = parse_and_layout(input, 40, 20);

        // Verify key content is present
        let mut found_welcome = false;
        let mut found_item_one = false;
        let mut found_go = false;

        for y in 0..buf.height {
            let line = row_text(&buf, y);
            if line.contains("Welcome") {
                found_welcome = true;
            }
            if line.contains("Item one") {
                found_item_one = true;
            }
            if line.contains("Go somewhere") {
                found_go = true;
            }
        }

        assert!(found_welcome, "should contain Welcome heading");
        assert!(found_item_one, "should contain list item");
        assert!(found_go, "should contain link text");
    }

    // ─── Panel Region Tracking ──────────────────────────────

    fn parse_and_layout_full(input: &str, width: u16, height: u16) -> LayoutResult {
        let mut scanner = Scanner::new(input.as_bytes()).unwrap();
        let tokens = scanner.scan_all().unwrap();
        let result = parser::parse(tokens);
        let doc = result.document.expect("parse failed");
        let mut scene = crate::compositor::scene::build::from_document(&doc);
        let mut lr = layout_scene(
            &mut scene,
            width,
            height,
            ColorSupport::Truecolor,
            WidthConfig::default(),
        );
        // Phase 2: some kinds write into per-node buffers; the composited
        // view is what downstream assertions should see. Replace the raw
        // page_buf with the composited frame.
        let anim_rt = crate::compositor::animate::AnimationRuntime::new(Vec::new());
        lr.buffer =
            crate::compositor::composite::walk(&scene, &anim_rt, lr.buffer.width, lr.buffer.height);
        lr
    }

    #[test]
    fn panel_regions_tracked() {
        let result = parse_and_layout_full(
            r#"[page mode=document]
                [panel id="p1" state="a"]
                    [state name="a"][text]State A[/text][/state]
                    [state name="b"][text]State B[/text][/state]
                [/panel]
            [/page]"#,
            40,
            10,
        );
        let panels: Vec<_> = result.panels().collect();
        assert_eq!(panels.len(), 1);
        assert_eq!(panels[0].id, "p1");
        assert!(panels[0].rect.h >= 1);
    }

    #[test]
    fn live_regions_tracked() {
        let result = parse_and_layout_full(
            r#"[page mode=document]
                [live id="clock" endpoint="/clock"][text]00:00:00[/text][/live]
            [/page]"#,
            40,
            10,
        );
        let lives: Vec<_> = result.live_regions().collect();
        assert_eq!(lives.len(), 1);
        assert_eq!(lives[0].id, "clock");
        match &lives[0].kind {
            PlacedKind::Live { endpoint, .. } => assert_eq!(endpoint, "/clock"),
            _ => panic!("expected live kind"),
        }
        assert!(lives[0].rect.h >= 1);
    }

    #[test]
    fn live_regions_inside_box() {
        let result = parse_and_layout_full(
            r#"[page mode=document]
                [box w=50 h=5 border=double]
                    [live id="clock" endpoint="/clock"]
                        [text]Connecting...[/text]
                    [/live]
                [/box]
            [/page]"#,
            60,
            20,
        );
        let lives: Vec<_> = result.live_regions().collect();
        assert_eq!(lives.len(), 1, "live region should be collected inside box");
        assert_eq!(lives[0].id, "clock");
        match &lives[0].kind {
            PlacedKind::Live { endpoint, .. } => assert_eq!(endpoint, "/clock"),
            _ => panic!("expected live kind"),
        }
        // Position should be inside the box (not at 0,0)
        assert!(lives[0].rect.x > 0, "x should be inside box border");
        assert!(lives[0].rect.y > 0, "y should be inside box border");
    }

    // ─── Placement / screen-mode panel region ───────────────

    /// Screen-mode panel with an absolutely-positioned child. With
    /// `Placement.bbox` tracking the full descendant footprint, the
    /// panel's region reflects the child's actual rect rather than
    /// collapsing to `(0, 0, w, 1)` (which is what a pure cursor-
    /// delta heuristic would give for an absolute child that
    /// never advances `ctx.y`).
    #[test]
    fn panel_region_in_screen_mode_with_absolute_child() {
        let result = parse_and_layout_full(
            r#"[page mode=screen cols=40 rows=20]
                [panel id="p1" state="a"]
                    [state name="a"]
                        [box x=10 y=5 w=8 h=3 border=single][/box]
                    [/state]
                [/panel]
            [/page]"#,
            40,
            20,
        );
        let panels: Vec<_> = result.panels().collect();
        assert_eq!(panels.len(), 1);
        let pr = &panels[0];
        assert_eq!(pr.id, "p1");
        // The panel's region must match the absolute child, not degenerate to 1 row.
        assert_eq!(
            (pr.rect.x, pr.rect.y, pr.rect.w, pr.rect.h),
            (10, 5, 8, 3),
            "panel region should reflect absolute child's rect, got ({}, {}, {}, {})",
            pr.rect.x,
            pr.rect.y,
            pr.rect.w,
            pr.rect.h,
        );
    }

    /// When a panel state contains both a flow text element and an absolute
    /// box, the panel region must union both.
    #[test]
    fn panel_region_unions_flow_and_absolute_children() {
        let result = parse_and_layout_full(
            r#"[page mode=screen cols=40 rows=20]
                [panel id="p1" state="a"]
                    [state name="a"]
                        [text]Hi[/text]
                        [box x=20 y=10 w=5 h=2 border=single][/box]
                    [/state]
                [/panel]
            [/page]"#,
            40,
            20,
        );
        let pr = result.panels().next().unwrap();
        // Union: text at (0,0) with h=1 + box at (20,10) h=2 → (0,0, 25, 12)
        assert_eq!(pr.rect.x, 0);
        assert_eq!(pr.rect.y, 0);
        assert!(
            pr.rect.w >= 25,
            "w should span to absolute box right edge, got {}",
            pr.rect.w
        );
        assert!(
            pr.rect.h >= 12,
            "h should reach absolute box bottom, got {}",
            pr.rect.h
        );
    }

    /// A live region inside a screen-mode absolute box should report its
    /// position inside the box. This exercises that Placement.bbox propagates
    /// correctly through nested absolute positioning.
    #[test]
    fn live_region_inside_screen_mode_absolute_box() {
        let result = parse_and_layout_full(
            r#"[page mode=screen cols=60 rows=20]
                [box x=5 y=3 w=30 h=6 border=double]
                    [live id="c" endpoint="/c" height=3][/live]
                [/box]
            [/page]"#,
            60,
            20,
        );
        let lives: Vec<_> = result.live_regions().collect();
        assert_eq!(lives.len(), 1);
        let lr = lives[0];
        // live is inside a box at (5,3) with a double border, so its origin
        // is offset by 1 from the box corner.
        assert!(
            lr.rect.x >= 6 && lr.rect.x <= 7,
            "live x should be inside box, got {}",
            lr.rect.x
        );
        assert!(
            lr.rect.y >= 4 && lr.rect.y <= 5,
            "live y should be inside box, got {}",
            lr.rect.y
        );
        assert_eq!(lr.rect.h, 3);
    }

    /// Document-mode regression check: a mixed flow document
    /// produces the same panel region that a pure cursor-delta
    /// heuristic would give when all children are flow (no absolute
    /// positioning). The bbox-tracking pass doesn't widen it.
    #[test]
    fn document_mode_panel_region_regression() {
        let result = parse_and_layout_full(
            r#"[page mode=document]
                [panel id="p1" state="a"]
                    [state name="a"]
                        [text]Line one[/text]
                        [text]Line two[/text]
                        [text]Line three[/text]
                    [/state]
                [/panel]
            [/page]"#,
            40,
            10,
        );
        let pr = result.panels().next().unwrap();
        // Three lines of flow text in a column at (0, 0).
        assert_eq!(pr.rect.x, 0);
        assert_eq!(pr.rect.y, 0);
        assert!(
            pr.rect.h >= 3,
            "expected at least 3 rows of content, got h={}",
            pr.rect.h
        );
    }

    /// Absolute Box returns a Placement with flow_advance=0, so siblings
    /// stacked after it in a flow container start at the top.
    #[test]
    fn absolute_box_does_not_advance_flow_cursor() {
        let result = parse_and_layout_full(
            r#"[page mode=screen cols=40 rows=20]
                [box x=10 y=8 w=8 h=3 border=single][/box]
                [text]FlowStart[/text]
            [/page]"#,
            40,
            20,
        );
        // "FlowStart" is a flow child following an absolute box. It should
        // render at row 0, not at row 11 (which would be the case if the
        // absolute box had advanced the flow cursor).
        let line0 = row_text(&result.buffer, 0);
        assert!(
            line0.starts_with("FlowStart"),
            "flow text should start at row 0 after an absolute box, got line0 = {:?}",
            line0
        );
    }

    // ─── PlacedElement unification ──────────────────────────

    /// A document with one of each kind yields one `PlacedElement` per kind,
    /// all in the single `placed` vec and distinguished by `PlacedKind`.
    #[test]
    fn placed_vec_contains_one_of_each_kind() {
        let result = parse_and_layout_full(
            r#"[page mode=document]
                [panel id="p" state="a"]
                    [state name="a"][text]Panel[/text][/state]
                [/panel]
                [animate id="anim" fps=10]
                    [frame][text]frame1[/text][/frame]
                [/animate]
                [live id="clock" endpoint="/clock"][text]--[/text][/live]
            [/page]"#,
            40,
            20,
        );

        let panels: Vec<_> = result.panels().collect();
        let animations: Vec<_> = result.animations().collect();
        let lives: Vec<_> = result.live_regions().collect();

        assert_eq!(panels.len(), 1, "one panel expected");
        assert_eq!(animations.len(), 1, "one animation expected");
        assert_eq!(lives.len(), 1, "one live region expected");

        assert_eq!(panels[0].id, "p");
        assert_eq!(animations[0].id, "anim");
        assert_eq!(lives[0].id, "clock");

        // Every entry in `placed` is one of the three kinds.
        assert_eq!(
            result.placed.len(),
            3,
            "placed vec should hold exactly 3 elements, got {}: {:?}",
            result.placed.len(),
            result.placed,
        );
    }

    /// `PlacedKind::Animation { background: true }` survives through to
    /// `is_background_animation` so downstream code can distinguish z=-1
    /// background animations without reaching into variant internals.
    #[test]
    fn background_animation_kind_preserved() {
        let result = parse_and_layout_full(
            r#"[page mode=screen cols=40 rows=10]
                [animate id="bg" background=true src="/x.wasm"/]
                [animate id="fg" fps=10]
                    [frame][text]ok[/text][/frame]
                [/animate]
            [/page]"#,
            40,
            10,
        );
        let anims: Vec<_> = result.animations().collect();
        assert_eq!(anims.len(), 2);
        let bg = anims.iter().find(|a| a.id == "bg").unwrap();
        let fg = anims.iter().find(|a| a.id == "fg").unwrap();
        assert!(bg.is_background_animation(), "bg should flag background");
        assert!(!fg.is_background_animation(), "fg should not");
    }

    /// Rect::union — the core helper for aggregating bboxes — treats empty
    /// rects as "no contribution" so that an empty initial accumulator
    /// doesn't pollute the first real rect.
    #[test]
    fn rect_union_is_identity_with_empty() {
        let a = Rect::new(5, 10, 0, 0);
        let b = Rect::new(20, 30, 4, 2);
        assert_eq!(a.union(b), b);
        assert_eq!(b.union(a), b);
    }

    #[test]
    fn rect_union_merges_disjoint_rects() {
        let a = Rect::new(0, 0, 5, 2);
        let b = Rect::new(10, 10, 3, 3);
        assert_eq!(a.union(b), Rect::new(0, 0, 13, 13));
    }

    #[test]
    fn input_is_focusable() {
        let result = parse_and_layout_full(
            r#"[page mode=document]
                [input name="msg" placeholder="Type here" /]
            [/page]"#,
            40,
            10,
        );
        assert_eq!(result.focusable_positions.len(), 1);
    }

    #[test]
    fn link_focusable_has_nonzero_width() {
        let result = parse_and_layout_full(
            r#"[page mode=document]
                [link href="/foo"][text]Click me[/text][/link]
            [/page]"#,
            60,
            10,
        );
        assert_eq!(result.focusable_positions.len(), 1);
        let (col, _row, width) = result.focusable_positions[0];
        assert!(
            width > 0,
            "link should have non-zero width, got col={col} width={width}"
        );
        // "Click me" is 8 chars
        assert!(width >= 8, "link width should cover the text, got {width}");
    }

    #[test]
    fn multiple_links_all_focusable() {
        let result = parse_and_layout_full(
            r#"[page mode=document]
                [link href="/a"][text]First[/text][/link]
                [link href="/b"][text]Second[/text][/link]
                [link href="/c"][text]Third[/text][/link]
            [/page]"#,
            60,
            10,
        );
        assert_eq!(result.focusable_positions.len(), 3);
        for (i, &(_col, _row, width)) in result.focusable_positions.iter().enumerate() {
            assert!(
                width > 0,
                "link {i} should have non-zero width, got {width}"
            );
        }
    }

    // ─── Event Binding Layout ───────────────────────────────
    //
    // Event bindings live on the scene (`Scene.event_bindings`),
    // collected by `build_scene`. The tests below parse the
    // document, build a scene, and verify bindings land there.

    fn parse_and_build_scene(input: &str) -> crate::compositor::scene::Scene {
        let mut scanner = Scanner::new(input.as_bytes()).unwrap();
        let tokens = scanner.scan_all().unwrap();
        let result = parser::parse(tokens);
        let doc = result.document.expect("parse failed");
        crate::compositor::scene::build::from_document(&doc)
    }

    #[test]
    fn on_bindings_collected() {
        let scene = parse_and_build_scene(
            r#"[page mode=document]
                [animate id="intro" fps=10][frame][text]Hi[/text][/frame][/animate]
                [panel id="p" state="a"]
                    [state name="a"][text]A[/text][/state]
                    [state name="b"][text]B[/text][/state]
                [/panel]
                [on event="page-load" do="animate" target="intro" /]
                [on event="animation-end" source="intro" do="set" target="p" to="b" delay="500ms" /]
            [/page]"#,
        );
        assert_eq!(scene.event_bindings.len(), 2);

        let b0 = &scene.event_bindings[0];
        assert_eq!(b0.event, crate::parser::ast::EventKind::PageLoad);
        assert_eq!(b0.action, crate::parser::ast::ActionKind::Animate);
        assert_eq!(b0.target, "intro");
        assert!(b0.source.is_none());
        assert_eq!(b0.delay_ms, 0);

        let b1 = &scene.event_bindings[1];
        assert_eq!(b1.event, crate::parser::ast::EventKind::AnimationEnd);
        assert_eq!(b1.action, crate::parser::ast::ActionKind::Set);
        assert_eq!(b1.target, "p");
        assert_eq!(b1.source.as_deref(), Some("intro"));
        assert_eq!(b1.to.as_deref(), Some("b"));
        assert_eq!(b1.delay_ms, 500);
    }

    #[test]
    fn on_bindings_no_visual_output() {
        let input = r#"[page mode=document]
                [text]Before[/text]
                [on event="page-load" do="animate" target="x" /]
                [text]After[/text]
            [/page]"#;
        let result = parse_and_layout_full(input, 40, 10);
        // [on] should not consume any vertical space
        assert_eq!(row_text(&result.buffer, 0), "Before");
        assert_eq!(row_text(&result.buffer, 1), "After");
        // Bindings land on the scene, not on layout output.
        let scene = parse_and_build_scene(input);
        assert_eq!(scene.event_bindings.len(), 1);
    }

    // ─── Inline Layout ──────────────────────────────────────

    #[test]
    fn inline_nested_text() {
        let buf = parse_and_layout(
            "[page mode=document][text]Hello [text bold]world[/text][/text][/page]",
            40,
            10,
        );
        assert_eq!(row_text(&buf, 0), "Hello world");
        // "world" should be bold (starts at col 6)
        let cell = buf.get(6, 0).unwrap();
        assert_eq!(cell.ch, 'w');
        assert!(cell.style.bold);
        // "Hello" should not be bold
        let cell = buf.get(0, 0).unwrap();
        assert_eq!(cell.ch, 'H');
        assert!(!cell.style.bold);
    }

    #[test]
    fn inline_multiple_styled_spans() {
        let buf = parse_and_layout(
            "[page mode=document][text][text bold]bold[/text] normal [text italic]italic[/text][/text][/page]",
            40,
            10,
        );
        assert_eq!(row_text(&buf, 0), "bold normal italic");
        assert!(buf.get(0, 0).unwrap().style.bold);
        assert!(!buf.get(5, 0).unwrap().style.bold);
        assert!(buf.get(12, 0).unwrap().style.italic);
    }

    #[test]
    fn inline_link() {
        let result = parse_and_layout_full(
            r#"[page mode=document][text]Click [link href="/x"][text]here[/text][/link] to go[/text][/page]"#,
            40,
            10,
        );
        assert_eq!(row_text(&result.buffer, 0), "Click here to go");
        // "here" should be underlined
        let cell = result.buffer.get(6, 0).unwrap();
        assert_eq!(cell.ch, 'h');
        assert!(cell.style.underline);
        // "Click" should not be underlined
        assert!(!result.buffer.get(0, 0).unwrap().style.underline);
        // Should have a focusable position for the link
        assert_eq!(result.focusable_positions.len(), 1);
        let (col, row, width) = result.focusable_positions[0];
        assert_eq!(row, 0);
        assert_eq!(col, 6);
        assert_eq!(width, 4); // "here"
    }

    #[test]
    fn inline_link_space_underline_boundaries() {
        // Spaces within a link should be underlined; spaces adjacent to
        // a link (before or after) should NOT be underlined.
        let result = parse_and_layout_full(
            r#"[page mode=document][text]Click [link href="/x"][text]here now[/text][/link] end[/text][/page]"#,
            40,
            10,
        );
        assert_eq!(row_text(&result.buffer, 0), "Click here now end");
        // Space at col 5 (between "Click" and "here") — before link
        let before = result.buffer.get(5, 0).unwrap();
        assert_eq!(before.ch, ' ');
        assert!(
            !before.style.underline,
            "space before link should not be underlined"
        );
        // 'h' at col 6 — link start
        assert!(result.buffer.get(6, 0).unwrap().style.underline);
        // Space at col 10 (between "here" and "now") — inside link
        let inside = result.buffer.get(10, 0).unwrap();
        assert_eq!(inside.ch, ' ');
        assert!(
            inside.style.underline,
            "space within link should be underlined"
        );
        // 'w' at col 13 — link end
        assert!(result.buffer.get(13, 0).unwrap().style.underline);
        // Space at col 14 (between "now" and "end") — after link
        let after = result.buffer.get(14, 0).unwrap();
        assert_eq!(after.ch, ' ');
        assert!(
            !after.style.underline,
            "space after link should not be underlined"
        );
    }

    #[test]
    fn block_link_space_underlined() {
        let result = parse_and_layout_full(
            r#"[page mode=document][link href="/x"][text]click here[/text][/link][/page]"#,
            40,
            10,
        );
        assert_eq!(row_text(&result.buffer, 0), "click here");
        // 'c' at col 0 should be underlined
        let c_cell = result.buffer.get(0, 0).unwrap();
        assert_eq!(c_cell.ch, 'c');
        assert!(
            c_cell.style.underline,
            "'c' in block link should be underlined"
        );
        // space at col 5 should be underlined
        let space_cell = result.buffer.get(5, 0).unwrap();
        assert_eq!(space_cell.ch, ' ', "col 5 should be a space");
        assert!(
            space_cell.style.underline,
            "space in block link should be underlined"
        );
        // 'h' at col 6 should be underlined
        let h_cell = result.buffer.get(6, 0).unwrap();
        assert_eq!(h_cell.ch, 'h');
        assert!(
            h_cell.style.underline,
            "'h' in block link should be underlined"
        );
    }

    #[test]
    fn inline_wrapping() {
        let buf = parse_and_layout(
            "[page mode=document][text]Hello [text bold]world[/text] from here[/text][/page]",
            12,
            10,
        );
        // Should wrap: "Hello world" won't fit in 12, so it wraps
        assert_eq!(row_text(&buf, 0), "Hello world");
        assert_eq!(row_text(&buf, 1), "from here");
    }

    #[test]
    fn inline_text_no_children_unchanged() {
        // Plain text without children should still work as before
        let buf = parse_and_layout(
            "[page mode=document][text]Hello world[/text][/page]",
            40,
            10,
        );
        assert_eq!(row_text(&buf, 0), "Hello world");
    }

    #[test]
    fn inline_link_wrapping() {
        let result = parse_and_layout_full(
            r#"[page mode=document][text]Visit [link href="/x"][text]this long link text[/text][/link] now[/text][/page]"#,
            20,
            10,
        );
        // Should wrap properly and track focusable positions
        assert!(!result.focusable_positions.is_empty());
    }

    #[test]
    fn inline_multiple_links() {
        let result = parse_and_layout_full(
            r#"[page mode=document][text][link href="/a"][text]A[/text][/link] and [link href="/b"][text]B[/text][/link][/text][/page]"#,
            40,
            10,
        );
        assert_eq!(row_text(&result.buffer, 0), "A and B");
        assert_eq!(result.focusable_positions.len(), 2);
    }

    #[test]
    fn inline_centered() {
        let buf = parse_and_layout(
            "[page mode=document][text align=center]Hello [text bold]world[/text][/text][/page]",
            20,
            5,
        );
        let line = row_text(&buf, 0);
        // "Hello world" = 11 chars, centered in 20 = ~4-5 leading spaces
        assert!(line.contains("Hello world"));
        assert!(line.starts_with("    ") || line.starts_with("     "));
    }

    #[test]
    fn inline_button() {
        let result = parse_and_layout_full(
            r#"[page mode=document][text]Press [button action="submit"]OK[/button] to continue[/text][/page]"#,
            40,
            10,
        );
        assert_eq!(row_text(&result.buffer, 0), "Press [ OK ] to continue");
        assert_eq!(result.focusable_positions.len(), 1);
    }

    #[test]
    fn inline_space_between_styled_spans() {
        // Space between [/text] and [text] should be preserved as word separator
        let buf = parse_and_layout(
            r#"[page mode=document][text][text bold]clients:[/text] [text fg=cyan]1[/text][/text][/page]"#,
            40,
            10,
        );
        assert_eq!(row_text(&buf, 0), "clients: 1");
    }

    #[test]
    fn stats_in_box_no_leading_blank() {
        // Simulate stats plugin inside a box with padding=0
        let input = concat!(
            "[page mode=screen]",
            "[box y=0 w=30 h=5 border=rounded fg=white bg=black align=center title=\"Server\" padding=0]",
            "[text][text fg=white bold]clients:[/text] [text fg=cyan]1[/text][/text]",
            "[text][text fg=white bold]uptime:[/text] [text fg=cyan]0m[/text][/text]",
            "[text][text fg=white bold]clock:[/text] [text fg=cyan]08:33:51 UTC[/text][/text]",
            "[/box]",
            "[/page]",
        );
        let buf = parse_and_layout(input, 40, 10);
        // Row 0: box border with title
        // Row 1: first stats line (no padding)
        let r1 = row_text(&buf, 1);
        assert!(
            r1.contains("clients:"),
            "Row 1 should contain clients, got: '{}'",
            r1
        );
    }
}
