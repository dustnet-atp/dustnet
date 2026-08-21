//! Scene-walk composition.
//!
//! The composite pass produces a single output `CellBuffer` from the
//! scene's buffered nodes. It walks the scene once per tick, blitting
//! each visible buffered node into the output, honoring per-node
//! `z_index` and `Transform`.
//!
//! ## Z-ordering
//!
//! Post the per-node-buffer migration, every visible `NodeKind` owns
//! its own `CellBuffer`: `Flow { source: Box }`, `Absolute`, `Text`
//! (all sub-kinds), `Hr`, `Link`, `Button`, `Input`, `Select`,
//! `Table`, plus the pre-existing buffered kinds `Panel`, `Animation`,
//! `LiveRegion`. `z_index` on `Animation` is used to split
//! background (`-1`) from foreground (`10`); other buffered nodes
//! sit at the default `0` and composite in tree order.
//!
//! Composition order:
//! 1. All buffered scene nodes with `z_index < 0`, in (z_index, tree
//!    order) — background animations.
//! 2. All buffered scene nodes in tree order, **except** `Animation`,
//!    `LiveRegion`, `Panel`, and `Overlay`. Containers paint first,
//!    children on top. This replaces the old `page_buf` base blit;
//!    every cell a reader sees is now owned by some scene node.
//! 3. `LiveRegion` and `Panel` buffers, in tree order — these kinds
//!    are authoritative over their children (subscription payload,
//!    transition blend), so they re-blit after step 2. Transparent
//!    cells in their buffers let placeholder children show through.
//! 4. All buffered scene nodes with `z_index >= 0`, in (z_index, tree
//!    order) — foreground animations.
//! 5. `Overlay` nodes, sorted by `z_index` ascending. Overlays are
//!    system-synthesized (page transitions, future debug overlays)
//!    and must not be occluded by any scene content — a page
//!    transition blend has to cover even foreground animations on
//!    the new page while the transition is playing.
//!
//! ## Transparency
//!
//! Each blit is transparency-respecting: transparent cells (space with
//! no background) do not overwrite the output. This is what gives
//! "reveal through gaps" — a dialog's absent cells let the layer
//! beneath show through — for free. See
//! `docs/spec/04-rendering.md § Cell model` for the conformance contract.
//!
//! ## Waiting animations
//!
//! An animation in `AnimState::Waiting` has not started yet (e.g.
//! triggered on input). Its scene buffer is allocated but may be
//! empty; composition skips it. This preserves the previous
//! compositor's `render_layer` filter.
//!
//! ## Page transitions
//!
//! Page transitions flow through the composite walk like any other
//! animation. `PageTransitionAdapter` (in `animate/page_transition.rs`)
//! paints the blended frame into a `NodeKind::Overlay` node's buffer
//! each tick, and Phase D of the walk blits that overlay on top of
//! everything else. When the adapter finishes, it emits a
//! `Patch::RemoveNode` that drops the overlay for subsequent ticks.
//! There is no bypass path.

#[cfg(test)]
use std::cell::Cell as FlagCell;

use crate::compositor::animate::{AnimState, AnimationRuntime};
use crate::compositor::layout::Rect;
#[cfg(test)]
use crate::compositor::layout::cell::Cell;
use crate::compositor::layout::cell::CellBuffer;
use crate::compositor::scene::{DirtyRegions, NodeKind, Scene};
use crate::resource::{ResourceCategory, ResourceGovernor};

pub(crate) type SharedFrame = triomphe::Arc<CellBuffer>;

#[cfg(test)]
thread_local! {
    static REJECT_FRAME_OWNER: FlagCell<bool> = const { FlagCell::new(false) };
}

#[cfg(test)]
fn frame_owner_rejected() -> bool {
    REJECT_FRAME_OWNER.with(|site| site.replace(false))
}

#[cfg(not(test))]
fn frame_owner_rejected() -> bool {
    false
}

/// Composite-pass handle. Holds output dimensions and — post Phase 3
/// of the composite-unification migration — the previous frame's
/// composited buffer so idle ticks can short-circuit without walking
/// the scene.
///
/// The scene is the authority for layers; this handle caches the
/// *result* of the walk, not any layer state. Buffer caches are
/// fallibly allocated shared `CellBuffer` owners so repeated returns of the same frame are
/// refcount bumps, not memcpy — idle ticks pay a handful of bytes
/// instead of `width × height × sizeof(Cell)`.
pub struct Compositor {
    width: u16,
    height: u16,
    /// Cached result of the most recent `composite`. `None` before
    /// the first composite, after a resize, or when the cache is
    /// explicitly invalidated via `invalidate_cache`.
    last_output: Option<SharedFrame>,
    /// Document row where a viewport-sized background animation was
    /// anchored when `last_output` was built.
    last_output_background_offset: u16,
    /// Cached pointer to the frame most recently written to the
    /// terminal via `present_main`. Used as the `prev` in
    /// `render_diff` so subsequent ticks emit only changed cells.
    /// `None` when no frame has been presented yet, when the
    /// previous viewport offset differs from the current one (diff
    /// semantics require a fixed offset), or after `resize`.
    last_presented: Option<SharedFrame>,
    /// Viewport offset at which `last_presented` was emitted.
    /// `render_diff` is only safe when current and previous share
    /// the same offset; otherwise fall back to `render_full`.
    last_presented_offset: u16,
    /// Dirty rectangles captured from the scene at the most recent
    /// `composite` call. Drained by `present_main` to scope
    /// `render_diff` to only the changed regions; empty on cache
    /// hits (idle tick) so `render_diff` does zero work.
    pending_present_dirty: DirtyRegions,
    /// Span of cells where the focus-highlight overlay was most
    /// recently painted (via direct stdout writes in
    /// `draw_viewer_frame`). Tracked here because the highlight lives
    /// outside the `CellBuffer` — `render_diff` therefore cannot see
    /// it change, and without this record a stale reverse-video
    /// would remain on screen after focus moves. Cleared on resize
    /// and `invalidate_presented`.
    last_focus_span: Option<FocusSpan>,
    governor: ResourceGovernor,
}

/// Allocation-free snapshot of terminal-visible presentation state used while
/// a synchronized HUD frame is still unpublished.
pub(crate) struct PresentationCheckpoint {
    last_presented: Option<SharedFrame>,
    last_presented_offset: u16,
    pending_present_dirty: DirtyRegions,
    last_focus_span: Option<FocusSpan>,
}

/// Records the location of the focus-highlight overlay on the
/// terminal. Stored on `Compositor` so the next draw can repaint
/// those cells from the buffer and wipe the previous reverse-video
/// before drawing the new highlight.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FocusSpan {
    pub screen_row: u16,
    pub buf_row: u16,
    pub col_start: u16,
    pub col_end: u16,
    pub is_sticky: bool,
}

impl Compositor {
    pub fn new(width: u16, height: u16) -> Self {
        Self::with_governor(width, height, ResourceGovernor::new())
    }

    pub fn with_governor(width: u16, height: u16, governor: ResourceGovernor) -> Self {
        Self {
            width,
            height,
            last_output: None,
            last_output_background_offset: 0,
            last_presented: None,
            last_presented_offset: 0,
            pending_present_dirty: DirtyRegions::default(),
            last_focus_span: None,
            governor,
        }
    }

    /// Move cache accounting to a newly active page's governor. Existing
    /// frames belong to the old page and cannot cross this boundary.
    pub(crate) fn set_governor(&mut self, governor: ResourceGovernor) {
        if self.governor.shares_budget_with(&governor) {
            return;
        }
        self.last_output = None;
        self.last_presented = None;
        self.governor = governor;
        self.pending_present_dirty.clear();
        self.last_focus_span = None;
    }

    pub fn last_focus_span(&self) -> Option<FocusSpan> {
        self.last_focus_span
    }

    pub fn set_last_focus_span(&mut self, span: Option<FocusSpan>) {
        self.last_focus_span = span;
    }

    pub(crate) fn presentation_checkpoint(&self) -> PresentationCheckpoint {
        PresentationCheckpoint {
            last_presented: self.last_presented.clone(),
            last_presented_offset: self.last_presented_offset,
            pending_present_dirty: self.pending_present_dirty.clone(),
            last_focus_span: self.last_focus_span,
        }
    }

    pub(crate) fn restore_presentation(&mut self, checkpoint: PresentationCheckpoint) {
        self.last_presented = checkpoint.last_presented;
        self.last_presented_offset = checkpoint.last_presented_offset;
        self.pending_present_dirty = checkpoint.pending_present_dirty;
        self.last_focus_span = checkpoint.last_focus_span;
    }

    pub fn width(&self) -> u16 {
        self.width
    }

    pub fn height(&self) -> u16 {
        self.height
    }

    /// Resize the output. Invalidates both the composite cache and
    /// the presented-frame cache — the next composite walks from
    /// scratch and the next present emits a full frame.
    pub fn resize(&mut self, width: u16, height: u16) {
        if width != self.width || height != self.height {
            self.width = width;
            self.height = height;
            self.last_output = None;
            self.last_presented = None;
            self.pending_present_dirty.clear();
            self.last_focus_span = None;
        }
    }

    /// Force the next `composite` call to do a full walk even if the
    /// scene's composite invalidation is empty. Used on first frame
    /// and anywhere the scene mutated without going through the
    /// invalidation channel.
    pub fn invalidate_cache(&mut self) {
        self.last_output = None;
    }

    /// Force the next `present_main` to emit a full frame rather
    /// than a diff. Used when the terminal state becomes unreliable
    /// (alt-screen switch, external `clear`, etc.).
    pub fn invalidate_presented(&mut self) {
        self.last_presented = None;
        self.last_focus_span = None;
    }

    /// Produce a composited frame.
    ///
    /// If `scene.invalidation.composite` is empty AND a cached frame
    /// of matching dimensions exists, returns the cached shared `CellBuffer`
    /// — refcount bump, no walk, no allocation. Otherwise walks the
    /// scene and updates the cache. Captures the scene's
    /// `invalidation.present` rects so `present_main` can scope
    /// `render_diff` to just the changed regions.
    ///
    /// Callers are responsible for clearing
    /// `scene.invalidation.composite` and `scene.invalidation.present`
    /// after both composite and present have run.
    pub fn composite(&mut self, scene: &Scene, anim_rt: &AnimationRuntime) -> Option<SharedFrame> {
        self.composite_at(scene, anim_rt, 0)
    }

    /// Produce a composited frame for a particular viewport position.
    /// Background animations are viewport layers, so their buffer is
    /// anchored at `viewport_offset` in the document-sized output.
    pub fn composite_at(
        &mut self,
        scene: &Scene,
        anim_rt: &AnimationRuntime,
        viewport_offset: u16,
    ) -> Option<SharedFrame> {
        let background_offset = if anim_rt
            .animations
            .iter()
            .any(|anim| !matches!(anim.state(), AnimState::Waiting) && anim.background())
        {
            viewport_offset
        } else {
            0
        };
        let cache_valid = self
            .last_output
            .as_ref()
            .map(|c| {
                c.width == self.width
                    && c.height == self.height
                    && self.last_output_background_offset == background_offset
            })
            .unwrap_or(false);
        if cache_valid
            && scene.invalidation.composite.is_empty()
            && let Some(cached) = self.last_output.as_ref()
        {
            self.pending_present_dirty.clear();
            return Some(cached.clone());
        }
        // Snapshot dirty rects *before* the walk; the loop clears
        // invalidation after present, so this is the authoritative
        // list of what needs diffing this tick.
        self.pending_present_dirty.clear();
        for rect in scene.invalidation.present.iter().copied() {
            self.pending_present_dirty.add(rect);
        }
        if background_offset > 0 {
            // The scene node remains placed at row zero, but a background
            // animation is composited as a viewport layer. Translate its
            // dirty area as well or diff presentation would refresh only
            // the small overlap with the node's document-space rect.
            self.pending_present_dirty.add(Rect::new(
                0,
                background_offset,
                self.width,
                self.height.saturating_sub(background_offset),
            ));
        }

        let out = walk_at_governed(
            scene,
            anim_rt,
            self.width,
            self.height,
            background_offset,
            &self.governor,
        );
        if out.allocation_failed() {
            return None;
        }
        if frame_owner_rejected() {
            return None;
        }
        let out = SharedFrame::try_new(out).ok()?;
        self.last_output_background_offset = background_offset;
        self.last_output = Some(out.clone());
        Some(out)
    }

    /// Emit the main viewport frame to `out`.
    ///
    /// Diffs against the previously-presented frame when safe
    /// (dimensions match, viewport offset unchanged), falling back
    /// to a full emit otherwise. Updates the presented cache so the
    /// next call has a baseline.
    ///
    /// When the compositor captured dirty rects in its most recent
    /// `composite()` call, `render_diff` scans only those rects;
    /// otherwise it falls back to a full-viewport scan. Either way
    /// `render_diff` emits only cells that actually changed — the
    /// dirty-rect scope is a cost optimization, not a correctness
    /// requirement.
    ///
    /// Post Phase 4 of the composite-unification migration: idle ticks
    /// that pass through `composite` via its cache end here as well —
    /// the diff against `last_presented` produces zero bytes when
    /// nothing changed, and with empty `pending_present_dirty` the
    /// diff touches no cells at all.
    pub fn present_main<W: std::io::Write>(
        &mut self,
        out: &mut W,
        frame: &SharedFrame,
        viewport_offset: u16,
        viewport_height: u16,
    ) -> std::io::Result<()> {
        let can_diff = self
            .last_presented
            .as_ref()
            .map(|p| {
                p.width == frame.width
                    && p.height == frame.height
                    && self.last_presented_offset == viewport_offset
            })
            .unwrap_or(false);

        if can_diff && let Some(prev) = self.last_presented.as_ref() {
            // Fast path: cache-hit composite cleared pending_dirty,
            // and if the frame is the same shared owner as last_presented we
            // know there's no change at all — skip the scan.
            let same_frame = SharedFrame::ptr_eq(prev, frame);
            if !same_frame {
                let dirty_scope: Option<&[Rect]> = if self.pending_present_dirty.is_empty() {
                    None
                } else {
                    Some(self.pending_present_dirty.as_slice())
                };
                crate::compositor::present::render_diff(
                    out,
                    prev,
                    frame,
                    viewport_offset,
                    viewport_height,
                    dirty_scope,
                )?;
            }
        } else {
            crate::compositor::present::render_full(out, frame, viewport_offset, viewport_height)?;
            // A full repaint overwrites the stdout-only focus overlay along
            // with every other visible cell. Forget its old screen position
            // so draw_viewer_frame does not try to wipe it afterward using
            // document-space glyphs at a now-stale viewport row.
            self.last_focus_span = None;
        }
        self.last_presented = Some(frame.clone());
        self.last_presented_offset = viewport_offset;
        self.pending_present_dirty.clear();
        Ok(())
    }
}

/// Walk the scene without animation phases, producing a buffer that
/// contains only the static scene-tree contributions (containers,
/// text and panels) — no background/foreground animations or page-transition
/// overlays.
///
/// Used by `relayout_panels_for` to extract `old_sub`/`new_sub` for
/// panel transitions: capturing animation output into the dissolve
/// source buffers freezes the animation under the panel area, so a
/// dissolve over a matrix-rain background ends up writing rain
/// characters into the panel buffer at low t — visually a "burst of
/// characters" rather than a clean fade-from-empty.
pub fn walk_static(scene: &Scene, width: u16, height: u16) -> CellBuffer {
    let empty = AnimationRuntime::empty();
    walk_inner(
        scene, &empty, width, height, 0, /*include_anims*/ false,
    )
}

pub(crate) fn walk_static_governed(
    scene: &Scene,
    width: u16,
    height: u16,
    governor: &ResourceGovernor,
) -> CellBuffer {
    let empty = AnimationRuntime::empty();
    walk_inner_with_governor(scene, &empty, width, height, 0, false, Some(governor))
}

/// Governed static composition for isolated, remotely-authored fragments.
pub(crate) fn walk_governed(
    scene: &Scene,
    anim_rt: &AnimationRuntime,
    width: u16,
    height: u16,
    governor: &ResourceGovernor,
) -> CellBuffer {
    walk_inner_with_governor(scene, anim_rt, width, height, 0, true, Some(governor))
}

/// Walk the scene and composite every visible buffered node,
/// producing a fresh output buffer of size `width × height`.
///
/// See module docs for Phases A–D ordering and transparency rules.
/// The scene tree is walked **once**, bucketing nodes into the three
/// tree-order phases (B containers, B-post parent-authoritative,
/// D overlays). Animation phases (A, C) are driven from
/// `anim_rt.animations` and require no scene walk.
pub fn walk(scene: &Scene, anim_rt: &AnimationRuntime, width: u16, height: u16) -> CellBuffer {
    walk_at(scene, anim_rt, width, height, 0)
}

/// Composite with viewport-sized background animations anchored at the
/// supplied document row.
pub fn walk_at(
    scene: &Scene,
    anim_rt: &AnimationRuntime,
    width: u16,
    height: u16,
    background_offset: u16,
) -> CellBuffer {
    walk_inner(
        scene,
        anim_rt,
        width,
        height,
        background_offset,
        /*include_anims*/ true,
    )
}

fn walk_inner(
    scene: &Scene,
    anim_rt: &AnimationRuntime,
    width: u16,
    height: u16,
    background_offset: u16,
    include_anims: bool,
) -> CellBuffer {
    walk_inner_with_governor(
        scene,
        anim_rt,
        width,
        height,
        background_offset,
        include_anims,
        None,
    )
}

fn walk_at_governed(
    scene: &Scene,
    anim_rt: &AnimationRuntime,
    width: u16,
    height: u16,
    background_offset: u16,
    governor: &ResourceGovernor,
) -> CellBuffer {
    walk_inner_with_governor(
        scene,
        anim_rt,
        width,
        height,
        background_offset,
        true,
        Some(governor),
    )
}

fn walk_inner_with_governor(
    scene: &Scene,
    anim_rt: &AnimationRuntime,
    width: u16,
    height: u16,
    background_offset: u16,
    include_anims: bool,
    governor: Option<&ResourceGovernor>,
) -> CellBuffer {
    // Panels in active transition take sole responsibility for painting
    // their region — their buffer holds the dissolve/fade blend, and the
    // children's chrome buffers must NOT be blit underneath in Phase B,
    // or transparent cells in the dissolve mask would let the children
    // show through (visually defeating the transition). We collect the
    // transitioning panels' NodeIds and, in the phase_b filter below,
    // skip any node that lives under one of them. Empty for the common
    // case (no transition active) — zero-cost.
    let transitioning_panels: std::collections::HashSet<crate::compositor::scene::NodeId> = anim_rt
        .transition_animations
        .iter()
        .map(|t| t.target_node())
        .collect();
    let is_under_transitioning_panel = |node: &crate::compositor::scene::Node| -> bool {
        if transitioning_panels.is_empty() {
            return false;
        }
        let mut current = node.parent();
        while let Some(parent_id) = current {
            if transitioning_panels.contains(&parent_id) {
                return true;
            }
            current = scene.get(parent_id).and_then(|p| p.parent());
        }
        false
    };
    // Frame and WASM animation children are source material. Their composed
    // pixels are painted by the animation node in Phase C and must not also
    // leak into the ordinary foreground pass while an effect is waiting or
    // revealing transparent cells.
    let is_under_animation = |node: &crate::compositor::scene::Node| -> bool {
        let mut current = node.parent();
        while let Some(parent_id) = current {
            let Some(parent) = scene.get(parent_id) else {
                break;
            };
            if matches!(parent.kind(), NodeKind::Animation(_)) {
                // A static walk is used to capture panel-transition source
                // buffers. Animation children are only source material for
                // their owning effect and must never be baked into those
                // snapshots, even though the static walk has no runtime
                // adapters with which to identify them.
                if !include_anims {
                    return true;
                }
                return parent.aml_id().is_some_and(|id| {
                    anim_rt
                        .animations
                        .iter()
                        .any(|animation| animation.id() == id)
                });
            }
            current = parent.parent();
        }
        false
    };
    let allocation = match governor {
        Some(governor) => {
            CellBuffer::try_new_governed(width, height, governor, ResourceCategory::CompositorCells)
                .map_err(|_| ())
        }
        None => CellBuffer::try_new(width, height).map_err(|_| ()),
    };
    let mut out = allocation.unwrap_or_else(|()| {
        let mut fallback = CellBuffer::new(1, 1);
        fallback.record_allocation_failure();
        fallback
    });
    if out.allocation_failed() {
        return out;
    }

    // Single tree walk — bucket visible buffered nodes into Phase B
    // (containers/text/etc, paint early so children overlay chrome),
    // Phase B-post (LiveRegion/Panel, re-blit after children), and
    // Phase D (Overlay, paints above everything). Animations are
    // iterated from `anim_rt` so their ordering is independent of
    // tree position.
    let mut phase_b: Vec<&crate::compositor::scene::Node> = Vec::new();
    let mut phase_bp: Vec<&crate::compositor::scene::Node> = Vec::new();
    let mut phase_d: Vec<&crate::compositor::scene::Node> = Vec::new();
    for node in scene.iter_tree_order() {
        if !node.visible() || node.buffer().is_none() {
            continue;
        }
        match node.kind() {
            NodeKind::Animation(_) => {}
            NodeKind::LiveRegion(_) | NodeKind::Panel { .. } => phase_bp.push(node),
            NodeKind::Overlay(data)
                if !include_anims
                    && matches!(
                        data.source,
                        crate::compositor::scene::OverlaySource::PageTransition
                    ) => {}
            NodeKind::Overlay(_) => phase_d.push(node),
            _ => {
                // Suppress chrome of children of transitioning panels so
                // the dissolve/fade blend in the panel buffer is the sole
                // source of visible cells in that region.
                if !is_under_transitioning_panel(node) && !is_under_animation(node) {
                    phase_b.push(node);
                }
            }
        }
    }

    // Phase A — background animations (z < 0). Viewport-sized; paint
    // at the current viewport's document row regardless of the
    // animation node's placement.
    if include_anims {
        for anim in &anim_rt.animations {
            if matches!(anim.state(), AnimState::Waiting) || !anim.background() {
                continue;
            }
            let Some(node_id) = scene.find_by_aml_id(anim.id()) else {
                continue;
            };
            let Some(src) = scene.buffer_of(node_id) else {
                continue;
            };
            blit(src, &mut out, 0, background_offset);
        }
    }

    // Phase B — containers and content in tree order. Parents blit
    // first so children naturally overlay parent chrome.
    for node in &phase_b {
        blit_placed(node, &mut out);
    }

    // Phase B-post — `LiveRegion` and `Panel` overlay their children.
    // Transparent cells pass through (empty LiveRegion shows the
    // placeholder underneath; idle Panel is a no-op). Opaque cells —
    // subscription payload or transition-adapter blend — override
    // the child content, matching pre-pivot semantics where these
    // buffers blit after the shared page_buf base.
    for node in &phase_bp {
        blit_placed(node, &mut out);
    }

    // Phase C — foreground animations (z >= 0).
    if include_anims {
        for anim in &anim_rt.animations {
            if matches!(anim.state(), AnimState::Waiting) || anim.background() {
                continue;
            }
            let Some(node_id) = scene.find_by_aml_id(anim.id()) else {
                continue;
            };
            let Some(node) = scene.get(node_id) else {
                continue;
            };
            if !node.visible() {
                continue;
            }
            let Some(src) = node.buffer() else { continue };
            let rect = node.placement().rect;
            let t = node.transform();
            let tx = (rect.x as i32 + t.dx as i32).clamp(0, width as i32) as u16;
            let ty = (rect.y as i32 + t.dy as i32).clamp(0, height as i32) as u16;
            blit(src, &mut out, tx, ty);
        }
    }

    // Phase D — system-synthesized overlays (page transitions,
    // future debug overlays). Paint last so nothing can cover them.
    // Sorted by z_index among themselves (lower-z debug overlays
    // paint first; a higher-z transition overlay sits on top). Sort
    // skipped when 0 or 1 overlay is present — the common case.
    if phase_d.len() > 1 {
        phase_d.sort_by_key(|n| n.z_index());
    }
    for node in &phase_d {
        blit_placed(node, &mut out);
    }

    out
}

/// Blit a scene node's buffer onto `out` at its transformed
/// placement. Centralizes the transform + clamp + empty-rect guard
/// so the `walk` phase loops stay readable.
fn blit_placed(node: &crate::compositor::scene::Node, out: &mut CellBuffer) {
    let Some(src) = node.buffer() else { return };
    let rect = node.placement().rect;
    if rect.is_empty() {
        return;
    }
    let t = node.transform();
    let tx = (rect.x as i32 + t.dx as i32).clamp(0, out.width as i32) as u16;
    let ty = (rect.y as i32 + t.dy as i32).clamp(0, out.height as i32) as u16;
    blit(src, out, tx, ty);
}

/// Transparency-respecting blit: writes `src`'s non-transparent cells
/// onto `dst` at `(dst_x, dst_y)`. Clips at `dst`'s bounds.
fn blit(src: &CellBuffer, dst: &mut CellBuffer, dst_x: u16, dst_y: u16) {
    let h = src.height.min(dst.height.saturating_sub(dst_y));
    let w = src.width.min(dst.width.saturating_sub(dst_x));
    for y in 0..h {
        let Some(row) = src.row(y) else { continue };
        for x in 0..w {
            let Some(cell) = row.get(x as usize) else {
                continue;
            };
            if cell.is_transparent() {
                continue;
            }
            dst.set(dst_x + x, dst_y + y, cell.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::{NamedColor, ResolvedColor};
    use crate::compositor::layout::cell::CellStyle;

    // These unit tests exercise the `blit` primitive and the thin
    // `Compositor` handle independent of the scene. Scene-walk
    // integration is covered by `compositor::parity` (golden-file
    // byte-parity) and `scene::tests` (layer reveal semantics).

    #[test]
    fn pending_present_dirty_union_is_allocation_free_and_reusable() {
        let mut compositor = Compositor::new(20, 10);
        compositor.pending_present_dirty.add(Rect::new(1, 2, 3, 2));
        compositor.pending_present_dirty.add(Rect::new(10, 6, 2, 3));

        assert_eq!(
            compositor.pending_present_dirty.as_slice(),
            &[Rect::new(1, 2, 11, 7)]
        );

        compositor.pending_present_dirty.clear();
        assert!(compositor.pending_present_dirty.is_empty());
        compositor.pending_present_dirty.add(Rect::new(4, 5, 1, 1));
        assert_eq!(
            compositor.pending_present_dirty.as_slice(),
            &[Rect::new(4, 5, 1, 1)]
        );
    }

    #[test]
    fn blit_respects_transparency() {
        let mut dst = CellBuffer::new(5, 1);
        for x in 0..5 {
            dst.put_char(x, 0, 'X', &CellStyle::default());
        }
        let mut src = CellBuffer::new(5, 1);
        src.put_char(2, 0, 'O', &CellStyle::default());
        // positions 0, 1, 3, 4 in src are default-empty (transparent)

        blit(&src, &mut dst, 0, 0);

        assert_eq!(
            dst.get(0, 0).unwrap().ch,
            'X',
            "transparent should not overwrite"
        );
        assert_eq!(dst.get(1, 0).unwrap().ch, 'X');
        assert_eq!(
            dst.get(2, 0).unwrap().ch,
            'O',
            "non-transparent should overwrite"
        );
        assert_eq!(dst.get(3, 0).unwrap().ch, 'X');
        assert_eq!(dst.get(4, 0).unwrap().ch, 'X');
    }

    #[test]
    fn blit_clips_at_dst_bounds() {
        let mut dst = CellBuffer::new(3, 1);
        let mut src = CellBuffer::new(5, 1);
        src.put_str(0, 0, "ABCDE", &CellStyle::default());

        blit(&src, &mut dst, 1, 0);

        assert_eq!(dst.get(0, 0).unwrap().ch, ' '); // untouched
        assert_eq!(dst.get(1, 0).unwrap().ch, 'A');
        assert_eq!(dst.get(2, 0).unwrap().ch, 'B');
        // No panic from out-of-bounds 'C'/'D'/'E'.
    }

    #[test]
    fn blit_opaque_space_writes() {
        let mut dst = CellBuffer::new(3, 1);
        for x in 0..3 {
            dst.put_char(x, 0, 'X', &CellStyle::default());
        }
        let mut src = CellBuffer::new(3, 1);
        let bg_style = CellStyle {
            bg: Some(ResolvedColor::Named(NamedColor::Blue)),
            ..Default::default()
        };
        src.put_char(1, 0, ' ', &bg_style);

        blit(&src, &mut dst, 0, 0);

        assert_eq!(dst.get(1, 0).unwrap().ch, ' ');
        assert_eq!(
            dst.get(1, 0).unwrap().style.bg,
            Some(ResolvedColor::Named(NamedColor::Blue)),
            "opaque space (space + bg) is not transparent and should overwrite",
        );
    }

    #[test]
    fn static_walk_excludes_animation_source_content() {
        let src = r#"[page mode=screen cols=30 rows=6]
            [box y=1 w=30 h=4 border=single]
                [animate id="delayed" fps=12 autoplay=false]
                    [pre]SHOULD-NOT-LEAK[/pre]
                [/animate]
            [/box]
        [/page]"#;
        let mut scanner = crate::scanner::Scanner::new(src.as_bytes()).unwrap();
        let tokens = scanner.scan_all().unwrap();
        let doc = crate::parser::parse(tokens).document.unwrap();
        let mut scene = crate::compositor::scene::build::from_document(&doc);
        let _ = crate::compositor::layout::engine::layout_scene(
            &mut scene,
            30,
            6,
            crate::color::ColorSupport::Truecolor,
            crate::compositor::layout::text::WidthConfig::default(),
        );

        let out = walk_static(&scene, 30, 6);
        let mut rendered = String::new();
        for y in 0..out.height {
            for x in 0..out.width {
                rendered.push(out.get(x, y).unwrap().ch);
            }
        }

        assert!(
            rendered.contains('┌'),
            "static box chrome should remain visible"
        );
        assert!(
            !rendered.contains("SHOULD-NOT-LEAK"),
            "animation source content leaked into a static transition snapshot",
        );
    }

    #[test]
    fn compositor_resize_updates_dims() {
        let mut c = Compositor::new(10, 5);
        assert_eq!(c.width(), 10);
        assert_eq!(c.height(), 5);
        c.resize(20, 8);
        assert_eq!(c.width(), 20);
        assert_eq!(c.height(), 8);
    }

    // ─── Phase 3 cache behavior ──────────────────────────────

    fn simple_scene() -> (crate::compositor::scene::Scene, AnimationRuntime) {
        let src = r#"[page mode=document][text]hello[/text][/page]"#;
        let mut scanner = crate::scanner::Scanner::new(src.as_bytes()).unwrap();
        let tokens = scanner.scan_all().unwrap();
        let doc = crate::parser::parse(tokens).document.unwrap();
        let mut scene = crate::compositor::scene::build::from_document(&doc);
        let _lr = crate::compositor::layout::engine::layout_scene(
            &mut scene,
            40,
            10,
            crate::color::ColorSupport::Truecolor,
            crate::compositor::layout::text::WidthConfig::default(),
        );
        scene.invalidation.clear();
        let anim_rt = AnimationRuntime::new(Vec::new());
        (scene, anim_rt)
    }

    #[test]
    fn governed_cache_tracks_unique_retained_frames_exactly() {
        let (mut scene, anim_rt) = simple_scene();
        let governor = ResourceGovernor::new();
        let mut compositor = Compositor::with_governor(40, 10, governor.clone());
        let frame_bytes = 40 * 10 * std::mem::size_of::<Cell>();

        let first = compositor.composite(&scene, &anim_rt).unwrap();
        assert_eq!(
            governor.used(ResourceCategory::CompositorCells),
            frame_bytes
        );

        compositor
            .present_main(&mut Vec::new(), &first, 0, 10)
            .unwrap();
        assert_eq!(
            governor.used(ResourceCategory::CompositorCells),
            frame_bytes,
            "output and presented aliases must be charged once",
        );

        scene
            .invalidation
            .mark_composite(crate::compositor::layout::Rect::new(0, 0, 1, 1));
        let second = compositor.composite(&scene, &anim_rt).unwrap();
        assert_eq!(
            governor.used(ResourceCategory::CompositorCells),
            frame_bytes * 2,
            "old presented and new output are distinct retained buffers",
        );

        compositor
            .present_main(&mut Vec::new(), &second, 0, 10)
            .unwrap();
        assert_eq!(
            governor.used(ResourceCategory::CompositorCells),
            frame_bytes * 2,
            "an externally retained old frame must keep its own lease",
        );
        drop(first);
        assert_eq!(
            governor.used(ResourceCategory::CompositorCells),
            frame_bytes,
            "the old frame releases its lease at its actual last owner",
        );

        compositor.invalidate_cache();
        assert_eq!(
            governor.used(ResourceCategory::CompositorCells),
            frame_bytes,
            "the presented baseline still owns the frame",
        );
        compositor.invalidate_presented();
        assert_eq!(
            governor.used(ResourceCategory::CompositorCells),
            frame_bytes,
            "the caller still retains the second frame",
        );
        drop(second);
        assert_eq!(governor.used(ResourceCategory::CompositorCells), 0);
    }

    #[test]
    fn shared_frame_owner_rejection_preserves_the_exact_cached_frame() {
        let (mut scene, anim_rt) = simple_scene();
        let governor = ResourceGovernor::new();
        let mut compositor = Compositor::with_governor(40, 10, governor.clone());
        let frame_bytes = 40 * 10 * std::mem::size_of::<Cell>();
        let previous = compositor.composite(&scene, &anim_rt).unwrap();
        compositor
            .present_main(&mut Vec::new(), &previous, 0, 10)
            .unwrap();
        scene.invalidation.mark_composite(Rect::new(0, 0, 1, 1));

        REJECT_FRAME_OWNER.with(|site| site.set(true));
        assert!(compositor.composite(&scene, &anim_rt).is_none());
        assert!(SharedFrame::ptr_eq(
            compositor.last_output.as_ref().unwrap(),
            &previous,
        ));
        assert!(SharedFrame::ptr_eq(
            compositor.last_presented.as_ref().unwrap(),
            &previous,
        ));
        assert_eq!(
            governor.used(ResourceCategory::CompositorCells),
            frame_bytes,
            "the rejected candidate must release its cell lease",
        );

        let replacement = compositor.composite(&scene, &anim_rt).unwrap();
        assert!(!SharedFrame::ptr_eq(&previous, &replacement));
        drop(previous);
        compositor.invalidate_cache();
        compositor.invalidate_presented();
        drop(replacement);
        assert_eq!(governor.used(ResourceCategory::CompositorCells), 0);
    }

    #[test]
    fn governed_cache_rejects_before_remote_sized_allocation() {
        let (scene, anim_rt) = simple_scene();
        let governor = ResourceGovernor::new();
        let frame_bytes = 40 * 10 * std::mem::size_of::<Cell>();
        let _pressure = governor
            .reserve(
                ResourceCategory::CompositorCells,
                crate::resource::MAX_REMOTE_MEMORY - frame_bytes + 1,
            )
            .unwrap();
        let mut compositor = Compositor::with_governor(40, 10, governor.clone());

        assert!(compositor.composite(&scene, &anim_rt).is_none());
        assert_eq!(
            governor.used(ResourceCategory::CompositorCells),
            crate::resource::MAX_REMOTE_MEMORY - frame_bytes + 1,
            "failed admission must not leave a cache reservation",
        );
    }

    #[test]
    fn first_composite_walks_and_caches() {
        let (scene, anim_rt) = simple_scene();
        let mut c = Compositor::new(40, 10);
        assert!(c.last_output.is_none(), "cache starts empty");

        let out = c.composite(&scene, &anim_rt).unwrap();
        assert_eq!(out.width, 40);
        assert!(c.last_output.is_some(), "first composite populates cache");
    }

    #[test]
    fn clean_invalidation_returns_cached_frame() {
        let (scene, anim_rt) = simple_scene();
        let mut c = Compositor::new(40, 10);
        let first = c.composite(&scene, &anim_rt).unwrap();
        // Invalidation is already empty (cleared in simple_scene).
        // Mutating the scene's buffer directly without marking
        // composite is exactly the case the cache is meant to skip.
        let second = c.composite(&scene, &anim_rt).unwrap();
        assert_eq!(first.width, second.width);
        assert_eq!(first.height, second.height);
        for y in 0..first.height {
            for x in 0..first.width {
                assert_eq!(
                    first.get(x, y),
                    second.get(x, y),
                    "clean invalidation must produce identical cells at ({x},{y})",
                );
            }
        }
    }

    #[test]
    fn dirty_invalidation_walks_again() {
        let (mut scene, anim_rt) = simple_scene();
        let mut c = Compositor::new(40, 10);
        let _first = c.composite(&scene, &anim_rt).unwrap();

        // Mark composite; next call must re-walk.
        scene
            .invalidation
            .mark_composite(crate::compositor::layout::Rect::new(0, 0, 5, 1));
        // Can't observe the walk directly, but we can observe that
        // the cache was refreshed: resize should still work after.
        let _second = c.composite(&scene, &anim_rt).unwrap();
        assert!(c.last_output.is_some());
    }

    #[test]
    fn resize_invalidates_cache() {
        let (scene, anim_rt) = simple_scene();
        let mut c = Compositor::new(40, 10);
        c.composite(&scene, &anim_rt);
        assert!(c.last_output.is_some());

        c.resize(60, 10);
        assert!(
            c.last_output.is_none(),
            "resize to different dims must drop cache"
        );

        // Resize to same dims should NOT drop cache.
        c.composite(&scene, &anim_rt);
        c.resize(60, 10);
        assert!(
            c.last_output.is_some(),
            "resize to same dims must preserve cache"
        );
    }

    #[test]
    fn invalidate_cache_forces_walk() {
        let (scene, anim_rt) = simple_scene();
        let mut c = Compositor::new(40, 10);
        c.composite(&scene, &anim_rt);
        assert!(c.last_output.is_some());

        c.invalidate_cache();
        assert!(c.last_output.is_none());
    }

    // ─── Phase 4 present-main behavior ──────────────────────

    #[test]
    fn present_main_first_frame_emits_full() {
        let (scene, anim_rt) = simple_scene();
        let mut c = Compositor::new(40, 10);
        let frame = c.composite(&scene, &anim_rt).unwrap();

        let mut out = Vec::new();
        c.present_main(&mut out, &frame, 0, 10).unwrap();
        assert!(!out.is_empty(), "first present must emit bytes");
        // Contains cursor-move escape (indicates render_full, not empty).
        assert!(out.windows(2).any(|w| w == b"\x1b["));
    }

    #[test]
    fn present_main_idle_tick_emits_nothing() {
        let (scene, anim_rt) = simple_scene();
        let mut c = Compositor::new(40, 10);
        let frame = c.composite(&scene, &anim_rt).unwrap();

        // First present — full emit.
        let mut out1 = Vec::new();
        c.present_main(&mut out1, &frame, 0, 10).unwrap();
        assert!(!out1.is_empty());

        // Second present of identical frame at identical offset.
        // render_diff should find nothing changed → empty output.
        let mut out2 = Vec::new();
        c.present_main(&mut out2, &frame, 0, 10).unwrap();
        assert!(
            out2.is_empty(),
            "idle re-present must emit zero bytes; got {} bytes",
            out2.len()
        );
    }

    #[test]
    fn present_main_viewport_change_emits_full() {
        let (scene, anim_rt) = simple_scene();
        let mut c = Compositor::new(40, 10);
        let frame = c.composite(&scene, &anim_rt).unwrap();

        let mut _out1 = Vec::new();
        c.present_main(&mut _out1, &frame, 0, 10).unwrap();
        c.set_last_focus_span(Some(FocusSpan {
            screen_row: 4,
            buf_row: 4,
            col_start: 2,
            col_end: 8,
            is_sticky: false,
        }));

        // Same frame, different viewport offset — diff semantics
        // don't hold; fall back to full. The full repaint also wipes
        // the terminal-only focus overlay, so its stale screen span
        // must not survive the scroll.
        let mut out2 = Vec::new();
        c.present_main(&mut out2, &frame, 2, 8).unwrap();
        assert!(
            !out2.is_empty(),
            "viewport offset change must trigger full emit",
        );
        assert_eq!(
            c.last_focus_span(),
            None,
            "full repaint must discard the focus overlay's old screen row",
        );
    }

    #[test]
    fn present_main_invalidate_presented_forces_full() {
        let (scene, anim_rt) = simple_scene();
        let mut c = Compositor::new(40, 10);
        let frame = c.composite(&scene, &anim_rt).unwrap();

        let mut out1 = Vec::new();
        c.present_main(&mut out1, &frame, 0, 10).unwrap();

        c.invalidate_presented();

        // Next present should be full even though frame is identical.
        let mut out2 = Vec::new();
        c.present_main(&mut out2, &frame, 0, 10).unwrap();
        assert!(
            !out2.is_empty(),
            "invalidate_presented must force next emit to be full",
        );
        assert!(
            out2.windows(b"\x1b[2J".len())
                .any(|window| window == b"\x1b[2J"),
            "a scene-changing full repaint must clear retained terminal cells",
        );
    }

    // ─── Page-transition render_diff continuity ─────────────────

    /// The payoff test for folding page transitions into the scene walk:
    /// at the end of a transition, the overlay's final frame shows the
    /// new page (that's how `TransitionKind::Cut` ends, and how every
    /// other kind resolves at t=1.0). When `Patch::RemoveNode` drops
    /// the overlay, the next composite shows the same content — because
    /// the new page IS what the overlay was showing.
    ///
    /// Therefore: `render_diff` against the last transition frame
    /// should emit **zero bytes**. The old bypass path could not
    /// achieve this — it forced `invalidate_presented` after every
    /// transition, triggering a full repaint.
    #[test]
    fn render_diff_across_transition_end_emits_zero_bytes() {
        use crate::compositor::layout::Rect;
        use crate::compositor::layout::cell::CellStyle;
        use crate::compositor::scene::OverlaySource;

        // A "new page" with recognizable content. Both the overlay's
        // final-frame contents AND the scene-under-overlay resolve to
        // the same cells, so toggling the overlay's presence should
        // leave every visible cell unchanged.
        let src = r#"[page mode=screen cols=10 rows=2][text]HELLO WRLD[/text][/page]"#;
        let mut scanner = crate::scanner::Scanner::new(src.as_bytes()).unwrap();
        let tokens = scanner.scan_all().unwrap();
        let doc = crate::parser::parse(tokens).document.unwrap();
        let mut scene = crate::compositor::scene::build::from_document(&doc);
        let _ = crate::compositor::layout::engine::layout_scene(
            &mut scene,
            10,
            2,
            crate::color::ColorSupport::Truecolor,
            crate::compositor::layout::text::WidthConfig::default(),
        );

        let anim_rt = AnimationRuntime::new(Vec::new());
        let mut c = Compositor::new(10, 2);

        // Render baseline without overlay — this is the "real new page".
        let base_frame = c.composite(&scene, &anim_rt).unwrap();

        // Insert the overlay and populate it with a copy of the new
        // page's base frame (= what a TransitionAdapter at t=1.0 does
        // via Cut/Dissolve/etc.).
        let overlay_id = scene.insert_overlay(
            i16::MAX,
            OverlaySource::PageTransition,
            Rect::new(0, 0, 10, 2),
        );
        {
            let buf = scene.overlay_buffer_mut(overlay_id).unwrap();
            for y in 0..2 {
                for x in 0..10 {
                    if let Some(cell) = base_frame.get(x, y) {
                        buf.set(x, y, cell.clone());
                    } else {
                        buf.put_char(x, y, ' ', &CellStyle::default());
                    }
                }
            }
        }

        // Tick 1: present the final transition frame (content matches
        // base_frame because the overlay was seeded with it).
        let frame1 = c.composite(&scene, &anim_rt).unwrap();
        let mut out1 = Vec::new();
        c.present_main(&mut out1, &frame1, 0, 2).unwrap();
        assert!(!out1.is_empty(), "first present is always non-empty");

        // Transition ends: remove the overlay. The composite walk now
        // reveals the underlying scene — same content.
        scene.remove_overlay(overlay_id);

        // Tick 2: content visible to the user is identical to tick 1,
        // but we went through composite::walk again. render_diff
        // against last_presented should produce an empty emit.
        let frame2 = c.composite(&scene, &anim_rt).unwrap();
        let mut out2 = Vec::new();
        c.present_main(&mut out2, &frame2, 0, 2).unwrap();

        assert!(
            out2.is_empty(),
            "post-transition-end frame should diff to zero bytes when overlay content \
             matched the underlying scene; got {} bytes (bypass path used to force \
             full repaint here)",
            out2.len(),
        );
    }

    // ─── Parent-authoritative overlay (LiveRegion / Panel) ──────

    /// `LiveRegion` subscription content must paint **on top of** its
    /// placeholder children, matching the pre-per-node-buffer
    /// semantics where the live buffer blit ran after `page_buf` (which
    /// held the children). With every kind now owning its own buffer,
    /// tree-order alone would paint children *over* the subscription —
    /// `walk` re-blits `LiveRegion` in Phase B-post to preserve authority.
    #[test]
    fn live_region_overlays_its_children() {
        let src = r#"[page mode=screen cols=20 rows=2]
[live src="atp://example.com" h=2][text]PLACEHOLDER[/text][/live]
[/page]"#;
        let mut scanner = crate::scanner::Scanner::new(src.as_bytes()).unwrap();
        let tokens = scanner.scan_all().unwrap();
        let doc = crate::parser::parse(tokens).document.unwrap();
        let mut scene = crate::compositor::scene::build::from_document(&doc);
        let _lr = crate::compositor::layout::engine::layout_scene(
            &mut scene,
            20,
            2,
            crate::color::ColorSupport::Truecolor,
            crate::compositor::layout::text::WidthConfig::default(),
        );

        // Manually allocate + populate the LiveRegion buffer (normally
        // done by `hydrate_scene_buffers` and the subscription).
        let live_id = scene.find_by_aml_id("").unwrap_or_else(|| {
            // No aml_id on this fixture — find the LiveRegion by kind.
            scene
                .iter_tree_order()
                .find(|n| matches!(n.kind(), NodeKind::LiveRegion(_)))
                .map(|n| n.id())
                .expect("scene has a LiveRegion node")
        });
        scene.allocate_buffer(live_id, 20, 2);
        {
            let buf = scene.live_buffer_mut(live_id).expect("live buffer");
            buf.put_str(0, 0, "LIVE", &CellStyle::default());
        }

        let anim_rt = AnimationRuntime::new(Vec::new());
        let out = walk(&scene, &anim_rt, 20, 2);

        // Cells 0-3 of row 0: live content "LIVE" (overlay wins),
        // cells 4+: placeholder still visible (transparent in live buf).
        assert_eq!(
            out.get(0, 0).unwrap().ch,
            'L',
            "subscription overlays placeholder"
        );
        assert_eq!(out.get(1, 0).unwrap().ch, 'I');
        assert_eq!(out.get(2, 0).unwrap().ch, 'V');
        assert_eq!(out.get(3, 0).unwrap().ch, 'E');
        // The placeholder "PLACEHOLDER" text starts at col 0 of row 0
        // in the child's buffer; col 4 is 'E' (the 5th letter). With
        // LIVE overlaying 0-3, col 4+ is the child's remainder.
        assert_eq!(
            out.get(4, 0).unwrap().ch,
            'E',
            "transparent live cells let placeholder show through"
        );
    }

    // ─── Phase 1: Overlay composition (Phase D of walk) ───────────

    /// An `Overlay` node at `z = i16::MAX` must paint *last* — on top of
    /// every other scene node including foreground animations. Phase D
    /// of the walk handles this; if Overlay were swept up in Phase B
    /// (tree-order), foreground animations in Phase C would cover it.
    #[test]
    fn overlay_paints_on_top_of_tree_order_nodes() {
        use crate::compositor::layout::Rect;
        use crate::compositor::scene::OverlaySource;

        let src = r#"[page mode=screen cols=10 rows=2][text]BASE[/text][/page]"#;
        let mut scanner = crate::scanner::Scanner::new(src.as_bytes()).unwrap();
        let tokens = scanner.scan_all().unwrap();
        let doc = crate::parser::parse(tokens).document.unwrap();
        let mut scene = crate::compositor::scene::build::from_document(&doc);
        let _lr = crate::compositor::layout::engine::layout_scene(
            &mut scene,
            10,
            2,
            crate::color::ColorSupport::Truecolor,
            crate::compositor::layout::text::WidthConfig::default(),
        );

        // Insert an overlay covering the whole viewport and paint "OVER"
        // into it. Composite walk must show "OVER" in cols 0-3, not "BASE".
        let overlay_id = scene.insert_overlay(
            i16::MAX,
            OverlaySource::PageTransition,
            Rect::new(0, 0, 10, 2),
        );
        {
            let buf = scene.overlay_buffer_mut(overlay_id).unwrap();
            buf.put_str(0, 0, "OVER", &CellStyle::default());
        }

        let anim_rt = AnimationRuntime::new(Vec::new());
        let out = walk(&scene, &anim_rt, 10, 2);

        assert_eq!(out.get(0, 0).unwrap().ch, 'O', "overlay wins col 0");
        assert_eq!(out.get(1, 0).unwrap().ch, 'V');
        assert_eq!(out.get(2, 0).unwrap().ch, 'E');
        assert_eq!(out.get(3, 0).unwrap().ch, 'R');
    }

    /// Panel entrance transitions take their old/new source images from
    /// `walk_static`. If a page transition is still running when a page-load
    /// action starts the panel, its overlay must not be baked into that panel
    /// source or departing-page glyphs will appear inside the unfurl.
    #[test]
    fn static_walk_excludes_page_transition_overlay() {
        use crate::compositor::layout::Rect;
        use crate::compositor::scene::OverlaySource;

        let src = r#"[page mode=screen cols=10 rows=1][text]DESTINATION[/text][/page]"#;
        let mut scanner = crate::scanner::Scanner::new(src.as_bytes()).unwrap();
        let tokens = scanner.scan_all().unwrap();
        let doc = crate::parser::parse(tokens).document.unwrap();
        let mut scene = crate::compositor::scene::build::from_document(&doc);
        let _ = crate::compositor::layout::engine::layout_scene(
            &mut scene,
            10,
            1,
            crate::color::ColorSupport::Truecolor,
            crate::compositor::layout::text::WidthConfig::default(),
        );

        let overlay_id = scene.insert_overlay(
            i16::MAX,
            OverlaySource::PageTransition,
            Rect::new(0, 0, 10, 1),
        );
        scene.overlay_buffer_mut(overlay_id).unwrap().put_str(
            0,
            0,
            "OLD PAGE!!",
            &CellStyle::default(),
        );

        let live = walk(&scene, &AnimationRuntime::empty(), 10, 1);
        assert_eq!(live.get(0, 0).unwrap().ch, 'O', "normal walk shows overlay");

        let panel_source = walk_static(&scene, 10, 1);
        let text: String = (0..10)
            .map(|x| panel_source.get(x, 0).unwrap().ch)
            .collect();
        assert_eq!(text, "DESTINATIO");
        assert!(!text.contains("OLD"));
    }

    /// Overlay transparent cells must let tree-order content show through —
    /// the same "reveal through gaps" property the rest of the compositor
    /// has. A partially-transparent debug overlay should not blank out
    /// everything beneath it.
    #[test]
    fn overlay_transparent_cells_pass_through() {
        use crate::compositor::layout::Rect;
        use crate::compositor::scene::OverlaySource;

        let src = r#"[page mode=screen cols=10 rows=1][text]ABCDEFGHIJ[/text][/page]"#;
        let mut scanner = crate::scanner::Scanner::new(src.as_bytes()).unwrap();
        let tokens = scanner.scan_all().unwrap();
        let doc = crate::parser::parse(tokens).document.unwrap();
        let mut scene = crate::compositor::scene::build::from_document(&doc);
        let _lr = crate::compositor::layout::engine::layout_scene(
            &mut scene,
            10,
            1,
            crate::color::ColorSupport::Truecolor,
            crate::compositor::layout::text::WidthConfig::default(),
        );

        // Overlay buffer is opaque-constructed (insert_overlay uses
        // `CellBuffer::new_opaque`), so by default cells are
        // non-transparent space — the overlay would blank the base.
        // Switch the overlay's buffer to one that has the `new` (transparent)
        // default in cells 0..5 to observe passthrough.
        let overlay_id = scene.insert_overlay(
            i16::MAX,
            OverlaySource::PageTransition,
            Rect::new(0, 0, 10, 1),
        );
        {
            // Replace with a transparent-base buffer and paint "XY" at cols 6-7.
            let buf = scene.overlay_buffer_mut(overlay_id).unwrap();
            *buf = CellBuffer::new(10, 1); // transparent base
            buf.put_str(6, 0, "XY", &CellStyle::default());
        }

        let anim_rt = AnimationRuntime::new(Vec::new());
        let out = walk(&scene, &anim_rt, 10, 1);

        // Cols 0-5: base text shows through (transparent overlay cells).
        // Cols 6-7: overlay wins. Cols 8-9: base again.
        assert_eq!(
            out.get(0, 0).unwrap().ch,
            'A',
            "base shows through transparent overlay"
        );
        assert_eq!(out.get(5, 0).unwrap().ch, 'F');
        assert_eq!(
            out.get(6, 0).unwrap().ch,
            'X',
            "overlay paints its opaque cells"
        );
        assert_eq!(out.get(7, 0).unwrap().ch, 'Y');
        assert_eq!(out.get(8, 0).unwrap().ch, 'I');
        assert_eq!(out.get(9, 0).unwrap().ch, 'J');
    }

    /// `Panel` during a transition paints the adapter's blended frame
    /// into `panel_buffer_mut`, and that must overlay the (freshly
    /// laid-out) state's child buffers. Same post-order requirement
    /// as `LiveRegion`.
    #[test]
    fn panel_overlays_its_children() {
        let src = r#"[page mode=screen cols=10 rows=2]
[panel id="p" state="a"]
  [state name="a" x=0 y=0 w=10 h=2][text]AAAA[/text][/state]
[/panel]
[/page]"#;
        let mut scanner = crate::scanner::Scanner::new(src.as_bytes()).unwrap();
        let tokens = scanner.scan_all().unwrap();
        let doc = crate::parser::parse(tokens).document.unwrap();
        let mut scene = crate::compositor::scene::build::from_document(&doc);
        let _lr = crate::compositor::layout::engine::layout_scene(
            &mut scene,
            10,
            2,
            crate::color::ColorSupport::Truecolor,
            crate::compositor::layout::text::WidthConfig::default(),
        );

        // Stand in for `hydrate_scene_buffers` + `TransitionAdapter`.
        let panel_id = scene.find_by_aml_id("p").expect("panel");
        scene.allocate_buffer(panel_id, 10, 2);
        {
            let buf = scene.panel_buffer_mut(panel_id).expect("panel buffer");
            buf.put_str(0, 0, "TRANS", &CellStyle::default());
        }

        let anim_rt = AnimationRuntime::new(Vec::new());
        let out = walk(&scene, &anim_rt, 10, 2);

        // Panel overlay wins for cols 0-4.
        assert_eq!(out.get(0, 0).unwrap().ch, 'T');
        assert_eq!(out.get(1, 0).unwrap().ch, 'R');
        assert_eq!(out.get(2, 0).unwrap().ch, 'A');
        assert_eq!(out.get(3, 0).unwrap().ch, 'N');
        assert_eq!(out.get(4, 0).unwrap().ch, 'S');
    }

    /// Regression: `AnimationRuntime::from_scene` builds each frame's
    /// bitmap by calling `layout_subtree` on the frame node. The shared
    /// kind dispatch writes `Node.placement` on every descendant it
    /// lays out, so without a snapshot+restore guard the `[pre]` inside
    /// each `[frame]` would end up with origin `(0, 0)` and a fresh
    /// per-node buffer — then the composite walk would blit that
    /// "ROWZERO-B" text at row 0, on top of the surrounding UI.
    /// `from_scene` must preserve the placements the main layout pass
    /// wrote so the composite walk never sees frame-descendant buffers
    /// at bogus positions.
    #[test]
    #[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
    fn from_scene_preserves_frame_descendant_placements() {
        use crate::compositor::animate::AnimationRuntime;
        let src = r#"[page mode=screen cols=20 rows=3]
[box y=1 w=20 h=2 border=none]
  [animate id="tw" fps=4 autoplay=false]
    [frame][pre]ROWZERO-A[/pre][/frame]
    [frame][pre]ROWZERO-B[/pre][/frame]
  [/animate]
[/box]
[/page]"#;
        let mut scanner = crate::scanner::Scanner::new(src.as_bytes()).unwrap();
        let tokens = scanner.scan_all().unwrap();
        let doc = crate::parser::parse(tokens).document.unwrap();
        let mut scene = crate::compositor::scene::build::from_document(&doc);
        let _ = crate::compositor::layout::engine::layout_scene(
            &mut scene,
            20,
            3,
            crate::color::ColorSupport::Truecolor,
            crate::compositor::layout::text::WidthConfig::default(),
        );

        // Snapshot every frame-descendant placement before `from_scene`
        // runs its `layout_subtree` passes.
        let frame_desc: Vec<(
            crate::compositor::scene::NodeId,
            crate::compositor::layout::Rect,
        )> = {
            let mut out = Vec::new();
            let frame_ids: Vec<crate::compositor::scene::NodeId> = scene
                .iter_tree_order()
                .filter(|n| {
                    matches!(n.kind(),
                    NodeKind::Flow(f) if f.source == crate::compositor::scene::FlowSource::Frame)
                })
                .map(|n| n.id())
                .collect();
            for fid in frame_ids {
                let mut stack = vec![fid];
                while let Some(id) = stack.pop() {
                    if let Some(n) = scene.get(id) {
                        out.push((id, n.placement().rect));
                        stack.extend(n.children().iter().copied());
                    }
                }
            }
            out
        };

        // Build the runtime — this is the call that used to clobber
        // placements via `layout_subtree`.
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let _anim_rt = rt.block_on(AnimationRuntime::from_scene(
            &mut scene,
            crate::color::ColorSupport::Truecolor,
            crate::compositor::layout::text::WidthConfig::default(),
            None,
        ));

        for (id, before) in frame_desc {
            let after = scene.get(id).map(|n| n.placement().rect).unwrap_or(before);
            assert_eq!(
                after, before,
                "node {id:?} placement changed from {:?} to {:?} — from_scene clobbered a frame descendant",
                before, after,
            );
        }
    }

    /// End-to-end: after panel state flip + anim_rt construction +
    /// trigger + frame advance + paint, the composite walk must show
    /// the advanced frame's content at the animation's rect — not at
    /// row 0, and not blank.
    #[test]
    #[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
    fn typewriter_paints_at_animation_rect_not_row_zero() {
        use crate::compositor::animate::{AnimState, AnimationRuntime};
        use crate::compositor::scene::{Patch, PatchApplier};

        let src = r#"[page mode=screen cols=30 rows=6]
[panel id="p" state="hidden"]
  [state name="hidden"][/state]
  [state name="visible"]
    [box y=1 w=30 h=4 border=none]
      [animate id="tw" fps=12 autoplay=false]
        [frame][pre]AAAA[/pre][/frame]
        [frame][pre]BBBB[/pre][/frame]
        [frame][pre]CCCC[/pre][/frame]
      [/animate]
    [/box]
  [/state]
[/panel]
[/page]"#;
        let mut scanner = crate::scanner::Scanner::new(src.as_bytes()).unwrap();
        let tokens = scanner.scan_all().unwrap();
        let doc = crate::parser::parse(tokens).document.unwrap();
        let mut scene = crate::compositor::scene::build::from_document(&doc);
        let color = crate::color::ColorSupport::Truecolor;
        let wcfg = crate::compositor::layout::text::WidthConfig::default();
        let _ = crate::compositor::layout::engine::layout_scene(&mut scene, 30, 6, color, wcfg);

        // Flip panel hidden → visible.
        let panel_id = scene.find_by_aml_id("p").unwrap();
        let visible_state = {
            let panel = scene.get(panel_id).unwrap();
            let NodeKind::Panel { states, .. } = panel.kind() else {
                unreachable!()
            };
            *states
                .iter()
                .find(|&&s| scene.get(s).and_then(|n| n.aml_id()) == Some("visible"))
                .unwrap()
        };
        PatchApplier::apply(
            &mut scene,
            Patch::SetPanelActive {
                panel: panel_id,
                active: visible_state,
            },
        );

        // Re-lay the panel subtree in place (what relayout_panels_for does).
        let mut page_buf = CellBuffer::new(30, 6);
        crate::compositor::layout::engine::relayout_in_place(
            &mut scene,
            &mut page_buf,
            panel_id,
            color,
            wcfg,
        );

        // Hydrate the Animation node's buffer from the refreshed placed list.
        let placed: Vec<_> = scene.iter_placed().collect();
        for p in &placed {
            if let Some(nid) = scene.find_by_aml_id(&p.id)
                && !p.rect.is_empty()
            {
                scene.allocate_buffer(nid, p.rect.w, p.rect.h);
            }
        }

        // Build anim_rt. This runs the snapshot/restore branch under test.
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let mut anim_rt = rt.block_on(AnimationRuntime::from_scene(&mut scene, color, wcfg, None));

        // Sanity: typewriter must sit at a non-zero y (inside the box).
        // Actual row depends on box border/padding; we just assert it
        // isn't row 0.
        let anim_id = scene.find_by_aml_id("tw").unwrap();
        let anim_rect = scene.get(anim_id).unwrap().placement().rect;
        assert!(
            anim_rect.y >= 1,
            "typewriter should sit inside the box, got {anim_rect:?}",
        );
        let anim_row = anim_rect.y;

        // Trigger + tick past the fps interval so frame advances 0 → 1.
        anim_rt.trigger_start("tw");
        let base = std::time::Instant::now();
        for i in 0..3 {
            let now = base + std::time::Duration::from_millis(200 * (i + 1));
            anim_rt.tick(&mut scene, now, 0, 6);
        }
        anim_rt.paint_into_scene(&mut scene);

        // Animation must have advanced past Waiting.
        let tw_state = anim_rt
            .animations
            .iter()
            .find(|a| a.id() == "tw")
            .map(|a| a.state())
            .unwrap();
        assert!(
            !matches!(tw_state, AnimState::Waiting),
            "typewriter should have left Waiting after trigger + ticks, got {tw_state:?}",
        );

        let out = walk(&scene, &anim_rt, 30, 6);

        // Row 0 must be empty — typewriter should never render at row 0.
        for x in 0..30 {
            let ch = out.get(x, 0).map(|c| c.ch).unwrap_or(' ');
            assert!(
                ch == ' ' || ch == '\0',
                "col {x} of row 0 should be empty, got {:?}",
                ch,
            );
        }

        // The animation's row should carry a frame's letter — either
        // 'B' (frame 1) or 'C' (frame 2) depending on timing.
        let row: String = (0..30)
            .map(|x| out.get(x, anim_row).map(|c| c.ch).unwrap_or(' '))
            .collect();
        assert!(
            row.contains('B') || row.contains('C'),
            "row {anim_row} should show an advanced frame letter, got {:?}",
            row,
        );
    }
}
