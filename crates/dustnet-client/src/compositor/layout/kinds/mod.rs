//! Per-`NodeKind` layout helpers. Each reads its `Node` /
//! `NodeKind` fields directly; none reach back into the AST.
//!
//! Every helper has the signature:
//!
//! ```ignore
//! pub(crate) fn layout(
//!     buf: &mut CellBuffer,
//!     ctx: &mut LayoutCtx,
//!//!     scene: &mut Scene,
//!     node_id: NodeId,
//! ) -> Placement
//! ```
//!
//! Helpers take `&mut Scene` + `NodeId` (not `&Node`) so they can
//! write the node's `Placement` via `scene.update_placement` before
//! returning. The borrow-checker dance: snapshot kind data and child
//! ids from the node via a scoped immutable borrow, then release
//! before recursing or writing.
//!
//! Container helpers recurse via `layout_children_scene` (for
//! flowed children) or `layout_node` (for unusual dispatch cases).
//! Inline-flatten text layout routes through
//! `try_collect_inline_segments` + `engine::try_wrap_inline_segments` +
//! `engine::render_inline_lines` — the existing AST-free wrap/render
//! primitives, fed from scene `TextContent.runs`.

use crate::compositor::layout::Rect;
use crate::compositor::layout::cell::CellBuffer;
use crate::compositor::layout::engine::{LayoutCtx, Placement};
use crate::compositor::scene::{KindTag, Node, NodeId, NodeKind, Scene};
use crate::resource::{BudgetLease, ResourceCategory, ResourceGovernor};

/// Walk `children_ids` in tree order, dispatching each to `layout_node`.
/// Returns the `Placement` of the sequence: the union of all child bboxes
/// plus the sum of their flow advances.
///
/// Container kind helpers use this to recurse into scene children.
/// Each child's placement is written into the scene by its own
/// kind helper as part of the recursion.
pub(crate) fn layout_children_scene(
    buf: &mut CellBuffer,
    ctx: &mut LayoutCtx,
    scene: &mut Scene,
    children_ids: &[NodeId],
) -> Placement {
    let start_x = ctx.x;
    let start_y = ctx.y;
    let mut bbox = Rect::new(start_x, start_y, 0, 0);
    let mut flow_advance: u16 = 0;

    for &child_id in children_ids {
        if scene.get(child_id).is_none() {
            continue;
        }
        ctx.y = start_y.saturating_add(flow_advance);
        let p = layout_node(buf, ctx, scene, child_id);
        bbox = bbox.union(p.bbox);
        flow_advance = flow_advance.saturating_add(p.flow_advance);
    }

    ctx.y = start_y.saturating_add(flow_advance);
    Placement {
        rect: Rect::new(start_x, start_y, ctx.width, flow_advance),
        flow_advance,
        bbox,
    }
}

pub mod absolute;
pub mod animation;
pub mod button;
pub mod flow;
pub mod hr;
pub mod input;
pub mod link;
pub mod live;
pub mod panel;
pub mod row;
pub mod select;
pub mod spacer;
pub mod table;
pub mod text;

/// Dispatch on the node's kind tag to the appropriate kind-specific
/// helper. The kind tag is snapshotted via a scoped immutable borrow
/// so the helpers can take `&mut Scene`.
pub(crate) fn layout_node(
    buf: &mut CellBuffer,
    ctx: &mut LayoutCtx,
    scene: &mut Scene,
    node_id: NodeId,
) -> Placement {
    let tag = match scene.get(node_id) {
        Some(n) => n.kind_tag(),
        None => return Placement::empty_at(ctx.x, ctx.y),
    };
    match tag {
        KindTag::Root => Placement::empty_at(ctx.x, ctx.y),
        KindTag::Flow => flow::layout(buf, ctx, scene, node_id),
        KindTag::Row => row::layout(buf, ctx, scene, node_id),
        KindTag::Absolute => absolute::layout(buf, ctx, scene, node_id),
        KindTag::Text => text::layout(buf, ctx, scene, node_id),
        KindTag::Input => input::layout(buf, ctx, scene, node_id),
        KindTag::Select => select::layout(buf, ctx, scene, node_id),
        // OptionLeaf is handled by the Select helper.
        KindTag::OptionLeaf => Placement::empty_at(ctx.x, ctx.y),
        KindTag::Button => button::layout(buf, ctx, scene, node_id),
        KindTag::Hr => hr::layout(buf, ctx, scene, node_id),
        KindTag::Spacer => spacer::layout(buf, ctx, scene, node_id),
        KindTag::Panel => panel::layout(buf, ctx, scene, node_id),
        KindTag::Animation => animation::layout(buf, ctx, scene, node_id),
        KindTag::LiveRegion => live::layout(buf, ctx, scene, node_id),
        KindTag::Table => table::layout(buf, ctx, scene, node_id),
        // Table rows/cells are handled inside the Table helper.
        KindTag::Tr | KindTag::Th | KindTag::Td => Placement::empty_at(ctx.x, ctx.y),
        KindTag::Link => link::layout(buf, ctx, scene, node_id),
        // Overlay nodes are system-synthesized with their buffer
        // pre-allocated at creation time; the layout pass does not
        // walk into them. The composite walk picks them up directly
        // by `node.buffer()`.
        KindTag::Overlay => Placement::empty_at(ctx.x, ctx.y),
    }
}

pub(crate) struct InlineSegments {
    values: Vec<crate::compositor::layout::engine::InlineSegment>,
    _lease: Option<BudgetLease>,
}

impl std::ops::Deref for InlineSegments {
    type Target = [crate::compositor::layout::engine::InlineSegment];

    fn deref(&self) -> &Self::Target {
        &self.values
    }
}

impl InlineSegments {
    #[cfg(test)]
    pub(crate) fn retained_capacity(&self) -> usize {
        self.values
            .capacity()
            .saturating_mul(std::mem::size_of::<
                crate::compositor::layout::engine::InlineSegment,
            >())
            .saturating_add(
                self.values
                    .iter()
                    .map(|segment| segment.text.capacity())
                    .sum::<usize>(),
            )
    }
}

fn inline_segment_requirements(node: &Node, scene: &Scene) -> Option<(usize, usize)> {
    let mut count = 0usize;
    let mut payload = 0usize;
    match node.kind() {
        NodeKind::Text(data) => {
            for run in &data.runs {
                if !run.text.is_empty() {
                    count = count.checked_add(1)?;
                    payload = payload.checked_add(run.text.len())?;
                }
            }
            for &child_id in node.children() {
                if let Some(child) = scene.get(child_id) {
                    let (child_count, child_payload) = inline_segment_requirements(child, scene)?;
                    count = count.checked_add(child_count)?;
                    payload = payload.checked_add(child_payload)?;
                }
            }
        }
        NodeKind::Link(_) => {
            for &child_id in node.children() {
                if let Some(child) = scene.get(child_id) {
                    let (child_count, child_payload) = inline_segment_requirements(child, scene)?;
                    count = count.checked_add(child_count)?;
                    payload = payload.checked_add(child_payload)?;
                }
            }
        }
        NodeKind::Button(data) => {
            count = 1;
            payload = data.label.len().checked_add(4)?;
        }
        _ => {}
    }
    Some((count, payload))
}

fn try_copy_string(value: &str) -> Option<String> {
    let mut copy = String::new();
    copy.try_reserve_exact(value.len()).ok()?;
    copy.push_str(value);
    Some(copy)
}

fn append_inline_segments(
    node: &Node,
    scene: &Scene,
    parent_style: &crate::compositor::layout::cell::CellStyle,
    color_support: crate::color::ColorSupport,
    focusable: Option<&crate::compositor::layout::engine::InlineFocusable>,
    segments: &mut Vec<crate::compositor::layout::engine::InlineSegment>,
) -> Option<()> {
    use crate::compositor::layout::cell::CellStyle;
    use crate::compositor::layout::engine::{InlineFocusable, InlineSegment, resolve_or_inherit};

    match node.kind() {
        NodeKind::Text(tc) => {
            for run in &tc.runs {
                let style = CellStyle {
                    fg: resolve_or_inherit(run.fg.as_ref(), parent_style.fg, color_support),
                    bg: resolve_or_inherit(run.bg.as_ref(), parent_style.bg, color_support),
                    bold: run.bold || parent_style.bold,
                    italic: run.italic || parent_style.italic,
                    underline: run.underline || parent_style.underline,
                    strikethrough: run.strikethrough || parent_style.strikethrough,
                    dim: run.dim || parent_style.dim,
                    blink: run.blink || parent_style.blink,
                };
                if !run.text.is_empty() {
                    segments.push(InlineSegment {
                        text: try_copy_string(&run.text)?,
                        style,
                        focusable: focusable.cloned(),
                    });
                }
            }
            for &child_id in node.children() {
                if let Some(child) = scene.get(child_id) {
                    append_inline_segments(
                        child,
                        scene,
                        parent_style,
                        color_support,
                        focusable,
                        segments,
                    )?;
                }
            }
        }
        NodeKind::Link(_) => {
            let style = CellStyle {
                underline: true,
                ..*parent_style
            };
            let foc = InlineFocusable { node_id: node.id() };
            for &child_id in node.children() {
                if let Some(child) = scene.get(child_id) {
                    append_inline_segments(
                        child,
                        scene,
                        &style,
                        color_support,
                        Some(&foc),
                        segments,
                    )?;
                }
            }
        }
        NodeKind::Button(data) => {
            let style = CellStyle {
                bold: true,
                ..*parent_style
            };
            let foc = InlineFocusable { node_id: node.id() };
            let text_len = data.label.len().checked_add(4)?;
            let mut text = String::new();
            text.try_reserve_exact(text_len).ok()?;
            text.push_str("[ ");
            text.push_str(&data.label);
            text.push_str(" ]");
            segments.push(InlineSegment {
                text,
                style,
                focusable: Some(foc),
            });
        }
        _ => {}
    }
    Some(())
}

/// Fallibly flatten inline roots into capacity-accounted segments. The
/// structural vector and every cloned/formatted string are admitted before
/// remote values are inserted and remain leased until the wrapper drops.
pub(crate) fn try_collect_inline_segments(
    roots: &[NodeId],
    scene: &Scene,
    parent_style: &crate::compositor::layout::cell::CellStyle,
    color_support: crate::color::ColorSupport,
    focusable: Option<&crate::compositor::layout::engine::InlineFocusable>,
    governor: Option<&ResourceGovernor>,
) -> Option<InlineSegments> {
    let (count, payload_bound) =
        roots
            .iter()
            .try_fold((0usize, 0usize), |(count, payload), &node_id| {
                let Some(node) = scene.get(node_id) else {
                    return Some((count, payload));
                };
                let (node_count, node_payload) = inline_segment_requirements(node, scene)?;
                Some((
                    count.checked_add(node_count)?,
                    payload.checked_add(node_payload)?,
                ))
            })?;
    let structural_bound = count.checked_mul(std::mem::size_of::<
        crate::compositor::layout::engine::InlineSegment,
    >())?;
    let requested = structural_bound.checked_add(payload_bound)?;
    let mut lease = match (requested, governor) {
        (0, _) | (_, None) => None,
        (bytes, Some(governor)) => Some(
            governor
                .reserve(ResourceCategory::RemoteCollections, bytes)
                .ok()?,
        ),
    };
    let mut values = Vec::new();
    values.try_reserve_exact(count).ok()?;
    let admitted = values
        .capacity()
        .checked_mul(std::mem::size_of::<
            crate::compositor::layout::engine::InlineSegment,
        >())?
        .checked_add(payload_bound)?;
    if let Some(lease) = lease.as_mut() {
        lease.try_resize_with_cost(admitted, admitted).ok()?;
    }
    for &node_id in roots {
        if let Some(node) = scene.get(node_id) {
            append_inline_segments(
                node,
                scene,
                parent_style,
                color_support,
                focusable,
                &mut values,
            )?;
        }
    }
    let retained = values
        .capacity()
        .checked_mul(std::mem::size_of::<
            crate::compositor::layout::engine::InlineSegment,
        >())?
        .checked_add(values.iter().try_fold(0usize, |total, segment| {
            total.checked_add(segment.text.capacity())
        })?)?;
    if let Some(lease) = lease.as_mut() {
        lease.try_resize_with_cost(retained, retained).ok()?;
    }
    Some(InlineSegments {
        values,
        _lease: lease,
    })
}
