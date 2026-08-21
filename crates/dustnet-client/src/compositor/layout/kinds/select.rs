//! `NodeKind::Select` scene-native layout. Walks the child
//! `OptionLeaf` nodes to find the selected (or first) option label,
//! then renders `[▼ label]` below the optional `label` row.

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
    // Snapshot: select data + children list + per-child option labels.
    let (label_opt, selected) = {
        let Some(node) = scene.get(node_id) else {
            return Placement::empty_at(ctx.x, ctx.y);
        };
        let NodeKind::Select(data) = node.kind() else {
            return Placement::empty_at(ctx.x, ctx.y);
        };

        // Only the selected label is ever read, so pick it out of the child
        // walk directly. Materialising every option label into a vector
        // allocated one string per remote `[option]` to index one of them.
        let selected = node
            .children()
            .iter()
            .filter_map(|&child_id| scene.get(child_id))
            .filter_map(|child| match child.kind() {
                NodeKind::OptionLeaf(opt) => Some(&opt.label),
                _ => None,
            })
            .nth(data.selected_index)
            .cloned();
        (data.label.clone(), selected)
    };

    let start_x = ctx.x;
    let start_y = ctx.y;
    let ctx_width = ctx.width;

    let rows: u16 = if label_opt.is_some() { 2 } else { 1 };
    let display = selected.as_deref().unwrap_or("(none)");
    let field = format!("[▼ {display}]");
    let dim_style = CellStyle {
        dim: true,
        ..ctx.style.clone()
    };
    let plain_style = ctx.style.clone();

    scene.allocate_buffer(node_id, ctx_width.max(1), rows);
    if let Some(text_buf) = scene.layout_buffer_mut(node_id) {
        let mut local_y = 0u16;
        if let Some(ref label) = label_opt {
            engine::put_str_clipped(
                text_buf,
                0,
                local_y,
                label,
                &plain_style,
                ctx_width,
                ctx.wcfg,
            );
            local_y = local_y.saturating_add(1);
        }
        engine::put_str_clipped(
            text_buf, 0, local_y, &field, &dim_style, ctx_width, ctx.wcfg,
        );
    }

    ctx.y = start_y.saturating_add(rows);
    buf.ensure_height(ctx.y);

    let h = rows;
    let rect = Rect::new(start_x, start_y, ctx_width, h);
    let placement = Placement {
        rect,
        flow_advance: h,
        bbox: rect,
    };
    scene.update_placement(node_id, placement);
    scene.update_focusable_rect(node_id, rect);
    placement
}
