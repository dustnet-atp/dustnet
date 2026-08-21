//! `NodeKind::Input` scene-native layout. Draws `[value_______]` or
//! `[placeholder_]` (dim) and registers a focusable position.

use crate::compositor::layout::Rect;
use crate::compositor::layout::cell::{CellBuffer, CellStyle};
use crate::compositor::layout::engine::{self, LayoutCtx, Placement};
use crate::compositor::layout::text::display_width;
use crate::compositor::scene::{NodeId, NodeKind, Scene};
use unicode_segmentation::UnicodeSegmentation;

pub(crate) fn layout(
    buf: &mut CellBuffer,
    ctx: &mut LayoutCtx,
    scene: &mut Scene,
    node_id: NodeId,
) -> Placement {
    let data = match scene.get(node_id).and_then(|n| match n.kind() {
        NodeKind::Input(d) => Some(d.clone()),
        _ => None,
    }) {
        Some(d) => d,
        None => return Placement::empty_at(ctx.x, ctx.y),
    };

    let start_x = ctx.x;
    let start_y = ctx.y;
    let ctx_width = ctx.width;

    let (display, style) = match data.value.as_deref() {
        Some(v) if !v.is_empty() => {
            let display = if data.password {
                "*".repeat(v.graphemes(true).count())
            } else {
                v.to_string()
            };
            (display, ctx.style.clone())
        }
        _ => {
            let placeholder = data
                .placeholder
                .clone()
                .unwrap_or_else(|| data.name.clone());
            (
                placeholder,
                CellStyle {
                    dim: true,
                    ..ctx.style.clone()
                },
            )
        }
    };

    let field_width = (ctx_width as usize).min(40);
    let content_width = field_width.saturating_sub(2);
    let mut visible = String::new();
    let mut visible_width = 0usize;
    for grapheme in display.graphemes(true) {
        let width = display_width(grapheme, ctx.wcfg);
        if visible_width + width > content_width {
            break;
        }
        visible.push_str(grapheme);
        visible_width += width;
    }
    let mut field = format!("[{visible}");
    let pad = content_width.saturating_sub(visible_width);
    field.extend(std::iter::repeat_n('_', pad));
    field.push(']');

    let width = field_width as u16;

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
