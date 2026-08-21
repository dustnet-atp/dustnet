//! `NodeKind::Button` scene-native layout. Renders `[ label ]` bold
//! and registers a focusable position.

use crate::compositor::layout::Rect;
use crate::compositor::layout::cell::{CellBuffer, CellStyle};
use crate::compositor::layout::engine::{self, LayoutCtx, Placement};
use crate::compositor::scene::{NodeId, NodeKind, Scene};

pub(crate) fn layout(
    buf: &mut CellBuffer,
    ctx: &mut LayoutCtx,
    scene: &mut Scene,
    node_id: NodeId,
) -> Placement {
    let label = match scene.get(node_id).and_then(|n| match n.kind() {
        NodeKind::Button(d) => Some(d.label.clone()),
        _ => None,
    }) {
        Some(l) => l,
        None => return Placement::empty_at(ctx.x, ctx.y),
    };

    let start_x = ctx.x;
    let start_y = ctx.y;
    let ctx_width = ctx.width;

    let style = CellStyle {
        bold: true,
        ..ctx.style.clone()
    };

    let field = format!("[ {} ]", label);
    let width = (field.len() as u16).min(ctx_width);

    scene.allocate_buffer(node_id, ctx_width.max(1), 1);
    if let Some(text_buf) = scene.layout_buffer_mut(node_id) {
        engine::put_str_clipped(text_buf, 0, 0, &field, &style, ctx_width, ctx.wcfg);
    }
    scene.update_focusable_rect(node_id, Rect::new(start_x, start_y, width, 1));

    ctx.y = start_y.saturating_add(1);
    buf.ensure_height(ctx.y);

    let rect = Rect::new(start_x, start_y, ctx_width, 1);
    let placement = Placement {
        rect,
        flow_advance: 1,
        bbox: rect,
    };
    scene.update_placement(node_id, placement);
    placement
}
