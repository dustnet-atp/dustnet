//! `NodeKind::Hr` scene-native layout. Draws a full-width divider at
//! the cursor using `HrData.style` and inherits fg from parent style
//! unless `HrData.fg` overrides.

use crate::compositor::layout::Rect;
use crate::compositor::layout::border::draw_hr;
use crate::compositor::layout::cell::{CellBuffer, CellStyle};
use crate::compositor::layout::engine::{LayoutCtx, Placement, resolve_or_inherit};
use crate::compositor::scene::{NodeId, NodeKind, Scene};

pub(crate) fn layout(
    buf: &mut CellBuffer,
    ctx: &mut LayoutCtx,
    scene: &mut Scene,
    node_id: NodeId,
) -> Placement {
    let data = match scene.get(node_id).and_then(|n| match n.kind() {
        NodeKind::Hr(d) => Some(d.clone()),
        _ => None,
    }) {
        Some(d) => d,
        None => return Placement::empty_at(ctx.x, ctx.y),
    };

    let start_x = ctx.x;
    let start_y = ctx.y;
    let width = ctx.width;

    let style = CellStyle {
        fg: resolve_or_inherit(data.fg.as_ref(), ctx.style.fg, ctx.color_support),
        ..ctx.style.clone()
    };

    scene.allocate_buffer(node_id, width.max(1), 1);
    if let Some(text_buf) = scene.layout_buffer_mut(node_id) {
        draw_hr(text_buf, 0, 0, width, data.style, &style);
    }

    ctx.y = start_y.saturating_add(1);
    buf.ensure_height(ctx.y);

    let rect = Rect::new(start_x, start_y, width, 1);
    let placement = Placement {
        rect,
        flow_advance: 1,
        bbox: rect,
    };
    scene.update_placement(node_id, placement);
    placement
}
