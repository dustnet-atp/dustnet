//! `NodeKind::Spacer` scene-native layout. Reads `lines` from the
//! `Spacer { lines }` variant; leaves the buffer cells untouched and
//! advances the flow cursor.

use crate::compositor::layout::Rect;
use crate::compositor::layout::cell::CellBuffer;
use crate::compositor::layout::engine::{LayoutCtx, Placement};
use crate::compositor::scene::{NodeId, NodeKind, Scene};

pub(crate) fn layout(
    buf: &mut CellBuffer,
    ctx: &mut LayoutCtx,
    scene: &mut Scene,
    node_id: NodeId,
) -> Placement {
    let lines = match scene.get(node_id).and_then(|n| match n.kind() {
        NodeKind::Spacer { lines } => Some(*lines),
        _ => None,
    }) {
        Some(l) => l,
        None => return Placement::empty_at(ctx.x, ctx.y),
    };

    let start_x = ctx.x;
    let start_y = ctx.y;
    let width = ctx.width;

    ctx.y = ctx.y.saturating_add(lines);
    buf.ensure_height(ctx.y);

    let rect = Rect::new(start_x, start_y, width, lines);
    let placement = Placement {
        rect,
        flow_advance: lines,
        bbox: rect,
    };
    scene.update_placement(node_id, placement);
    placement
}
