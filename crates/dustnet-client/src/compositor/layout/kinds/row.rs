//! `NodeKind::Row` scene-native layout. Walks scene children
//! (expected to be `NodeKind::Flow { source: Col }`), allocates
//! horizontal widths from each child `FlowData.width`, and
//! dispatches each column through `layout_node`.

use crate::compositor::layout::Rect;
use crate::compositor::layout::cell::CellBuffer;
use crate::compositor::layout::engine::{
    LayoutCtx, Placement, try_temp_vec, try_temp_vec_from_slice,
};
use crate::compositor::scene::{NodeId, NodeKind, Scene};
use crate::parser::ast::Dimension;

pub(crate) fn layout(
    buf: &mut CellBuffer,
    ctx: &mut LayoutCtx,
    scene: &mut Scene,
    node_id: NodeId,
) -> Placement {
    // Snapshot: gap + child ids + each child's declared width.
    let (gap, children, declared_widths) = {
        let Some(node) = scene.get(node_id) else {
            return Placement::empty_at(ctx.x, ctx.y);
        };
        let NodeKind::Row(data) = node.kind() else {
            return Placement::empty_at(ctx.x, ctx.y);
        };
        let Some(children) = try_temp_vec_from_slice(node.children(), ctx.governor.as_ref()) else {
            buf.record_allocation_failure();
            scene.record_resource_error();
            return Placement::empty_at(ctx.x, ctx.y);
        };
        // One declared width per child: the same count, so the same bound.
        let Some(mut widths) =
            try_temp_vec::<Option<Dimension>>(children.len(), ctx.governor.as_ref())
        else {
            buf.record_allocation_failure();
            scene.record_resource_error();
            return Placement::empty_at(ctx.x, ctx.y);
        };
        for &cid in children.iter() {
            let declared = scene.get(cid).and_then(|n| match n.kind() {
                NodeKind::Flow(f) => f.width,
                _ => None,
            });
            if widths.try_push(declared).is_none() {
                buf.record_allocation_failure();
                scene.record_resource_error();
                return Placement::empty_at(ctx.x, ctx.y);
            }
        }
        (data.gap, children, widths)
    };

    let start_x = ctx.x;
    let start_y = ctx.y;
    let width = ctx.width;

    if children.is_empty() {
        let rect = Rect::new(start_x, start_y, width, 0);
        let placement = Placement {
            rect,
            flow_advance: 0,
            bbox: rect,
        };
        scene.update_placement(node_id, placement);
        return placement;
    }

    let col_count = children.len();
    let total_gap = gap * (col_count as u16).saturating_sub(1);
    let available = ctx.width.saturating_sub(total_gap);

    let Some(mut col_widths) = try_temp_vec::<u16>(col_count, ctx.governor.as_ref()) else {
        buf.record_allocation_failure();
        scene.record_resource_error();
        return Placement::empty_at(ctx.x, ctx.y);
    };
    let mut fixed_total = 0u16;
    let mut fill_count = 0u16;

    for declared in declared_widths.iter() {
        let width = match declared {
            Some(Dimension::Fixed(w)) => {
                let clipped = (*w).min(available);
                fixed_total += clipped;
                clipped
            }
            _ => {
                fill_count += 1;
                0
            }
        };
        if col_widths.try_push(width).is_none() {
            buf.record_allocation_failure();
            scene.record_resource_error();
            return Placement::empty_at(ctx.x, ctx.y);
        }
    }

    let remaining = available.saturating_sub(fixed_total);
    let fill_width = remaining.checked_div(fill_count).unwrap_or(0);
    for w in col_widths.iter_mut() {
        if *w == 0 {
            *w = fill_width;
        }
    }

    let row_start_y = ctx.y;
    let mut max_height = 0u16;
    let mut col_x = ctx.x;
    let mut bbox = Rect::new(start_x, start_y, 0, 0);

    for (i, &child_id) in children.iter().enumerate() {
        if scene.get(child_id).is_none() {
            continue;
        }
        let Some(&cw) = col_widths.get(i) else {
            continue;
        };

        let mut col_ctx = LayoutCtx {
            x: col_x,
            y: row_start_y,
            width: cw,
            viewport_height: ctx.viewport_height,
            color_support: ctx.color_support,
            wcfg: ctx.wcfg,
            style: ctx.style.clone(),
            governor: ctx.governor.clone(),
        };

        let col_placement = super::layout_node(buf, &mut col_ctx, scene, child_id);
        bbox = bbox.union(col_placement.bbox);
        max_height = max_height.max(col_placement.flow_advance);

        col_x = col_x.saturating_add(cw).saturating_add(gap);
    }

    ctx.y = row_start_y.saturating_add(max_height);

    let rect = Rect::new(start_x, start_y, width, max_height);
    let bbox = rect.union(bbox);
    let placement = Placement {
        rect,
        flow_advance: max_height,
        bbox,
    };
    scene.update_placement(node_id, placement);
    placement
}
