//! `NodeKind::Absolute` scene-native layout. Positioned `Box` —
//! reads all chrome fields from `AbsoluteData` and recurses into
//! scene children via the shared `engine::layout_box_node` primitive.

use crate::compositor::layout::cell::CellBuffer;
use crate::compositor::layout::engine::{self, LayoutCtx, Placement};
use crate::compositor::scene::{NodeId, NodeKind, Scene};

pub(crate) fn layout(
    buf: &mut CellBuffer,
    ctx: &mut LayoutCtx,
    scene: &mut Scene,
    node_id: NodeId,
) -> Placement {
    let data = match scene.get(node_id).and_then(|n| match n.kind() {
        NodeKind::Absolute(d) => Some(d.clone()),
        _ => None,
    }) {
        Some(d) => d,
        None => return Placement::empty_at(ctx.x, ctx.y),
    };

    let placement = engine::layout_box_node(
        buf,
        ctx,
        scene,
        node_id,
        data.x,
        data.y,
        data.w,
        data.h,
        data.border.to_ast(),
        data.title.as_deref(),
        data.join_top,
        data.join_bottom,
        data.join_left,
        data.join_right,
        data.padding,
        data.align,
        data.fg.as_ref(),
        data.bg.as_ref(),
    );
    scene.update_placement(node_id, placement);
    placement
}
