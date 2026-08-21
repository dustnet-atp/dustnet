//! `NodeKind::LiveRegion` scene-native layout. Reads `LiveData`
//! directly, walks scene children for initial content sizing, and
//! records a `PlacedElement` with the subscription details.

use crate::compositor::layout::Rect;
use crate::compositor::layout::cell::CellBuffer;
use crate::compositor::layout::engine::{LayoutCtx, Placement, try_temp_vec_from_slice};
use crate::compositor::scene::{NodeId, NodeKind, Scene};
use crate::parser::ast::Dimension;

pub(crate) fn layout(
    buf: &mut CellBuffer,
    ctx: &mut LayoutCtx,
    scene: &mut Scene,
    node_id: NodeId,
) -> Placement {
    // Snapshot: live data + child ids + aml id.
    let (data, id, children) = {
        let Some(node) = scene.get(node_id) else {
            return Placement::empty_at(ctx.x, ctx.y);
        };
        let NodeKind::LiveRegion(d) = node.kind() else {
            return Placement::empty_at(ctx.x, ctx.y);
        };
        let Some(children) = try_temp_vec_from_slice(node.children(), ctx.governor.as_ref()) else {
            buf.record_allocation_failure();
            scene.record_resource_error();
            return Placement::empty_at(ctx.x, ctx.y);
        };
        (d.clone(), node.aml_id().unwrap_or("").to_string(), children)
    };

    let start_x = ctx.x;
    let start_y = ctx.y;
    let ctx_width = ctx.width;

    let children_placement = super::layout_children_scene(buf, ctx, scene, &children);

    let content_h = if children_placement.bbox.is_empty() {
        children_placement.flow_advance
    } else {
        children_placement
            .bbox
            .y
            .saturating_add(children_placement.bbox.h)
            .saturating_sub(start_y)
    };
    let h = match data.height {
        Dimension::Fixed(fixed) => fixed,
        _ => content_h.max(1),
    };
    ctx.y = start_y.saturating_add(h);
    buf.ensure_height(ctx.y);

    let _ = id; // placed region derives from scene via `iter_placed`.

    let rect = Rect::new(start_x, start_y, ctx_width, h);
    let bbox = rect.union(children_placement.bbox);
    let placement = Placement {
        rect,
        flow_advance: h,
        bbox,
    };
    scene.update_placement(node_id, placement);
    placement
}
