//! `NodeKind::Panel` scene-native layout. The Panel node carries a
//! list of state `NodeId`s plus an `active` pointer; this helper
//! walks the active state's children through the scene tree and
//! records a `PlacedElement` covering the state's bbox.

use crate::compositor::layout::Rect;
use crate::compositor::layout::cell::CellBuffer;
use crate::compositor::layout::engine::{LayoutCtx, Placement, try_temp_vec_from_slice};
use crate::compositor::scene::{NodeId, NodeKind, Scene};

pub(crate) fn layout(
    buf: &mut CellBuffer,
    ctx: &mut LayoutCtx,
    scene: &mut Scene,
    node_id: NodeId,
) -> Placement {
    // Snapshot: active state id, aml id, and the active state's
    // child list. Release borrow before recursing.
    let (active, id, active_children) = {
        let Some(node) = scene.get(node_id) else {
            return Placement::empty_at(ctx.x, ctx.y);
        };
        let NodeKind::Panel { active, .. } = node.kind() else {
            return Placement::empty_at(ctx.x, ctx.y);
        };
        let id = node.aml_id().unwrap_or("").to_string();
        let active = *active;
        // An absent active state is a legitimate empty child list; only an
        // allocator or budget refusal is a failure.
        let source = scene.get(active).map_or(&[][..], |n| n.children());
        let Some(children) = try_temp_vec_from_slice(source, ctx.governor.as_ref()) else {
            buf.record_allocation_failure();
            scene.record_resource_error();
            return Placement::empty_at(ctx.x, ctx.y);
        };
        (active, id, children)
    };

    let start_x = ctx.x;
    let start_y = ctx.y;
    let width = ctx.width;

    let state_placement = if active_children.is_empty() && scene.get(active).is_none() {
        Placement::empty_at(start_x, start_y)
    } else {
        super::layout_children_scene(buf, ctx, scene, &active_children)
    };

    let region_bbox = if state_placement.bbox.is_empty() {
        Rect::new(start_x, start_y, width, 1)
    } else {
        state_placement.bbox
    };
    let region_rect = Rect::new(
        region_bbox.x,
        region_bbox.y,
        region_bbox.w.max(1),
        region_bbox.h.max(1),
    );
    let _ = id; // placed region derives from scene via `iter_placed`.

    // Unified placement: `rect` is the panel's visual region (same as
    // the `PlacedElement` rect), `flow_advance` is the state's flow
    // contribution, `bbox` unions the region with the state's bbox.
    let bbox = region_rect.union(state_placement.bbox);
    let placement = Placement {
        rect: region_rect,
        flow_advance: state_placement.flow_advance,
        bbox,
    };
    scene.update_placement(node_id, placement);
    placement
}
