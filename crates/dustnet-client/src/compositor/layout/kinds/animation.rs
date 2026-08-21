//! `NodeKind::Animation` scene-native layout. Background animations
//! occupy the full viewport at z=-1 and consume no flow space. Frame-
//! based animations place the first frame as a visual placeholder.
//! WASM animations lay out children to measure the region, then blank
//! the cells so the WASM module can paint into the animation layer
//! without the base content showing through.

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
    // Snapshot: animation data, aml id, child ids, first child id.
    let (data, id, children) = {
        let Some(node) = scene.get(node_id) else {
            return Placement::empty_at(ctx.x, ctx.y);
        };
        let NodeKind::Animation(d) = node.kind() else {
            return Placement::empty_at(ctx.x, ctx.y);
        };
        let Some(children) = try_temp_vec_from_slice(node.children(), ctx.governor.as_ref()) else {
            buf.record_allocation_failure();
            scene.record_resource_error();
            return Placement::empty_at(ctx.x, ctx.y);
        };
        (d.clone(), node.aml_id().unwrap_or("").to_string(), children)
    };

    if data.background {
        // Background animations occupy the full viewport at z=-1 and
        // consume no flow space. `placement.rect` IS the visual region
        // rect — downstream consumers that need the animation's
        // on-screen rect read `node.placement().rect`.
        let rect = Rect::new(0, 0, ctx.width, ctx.viewport_height);
        let placement = Placement {
            rect,
            flow_advance: 0,
            bbox: rect,
        };
        scene.update_placement(node_id, placement);
        return placement;
    }

    let start_y = ctx.y;
    let start_x = ctx.x;
    let ctx_width = ctx.width;
    let mut children_bbox = Rect::new(start_x, start_y, 0, 0);

    if data.src.is_some() {
        for &child_id in children.iter() {
            if scene.get(child_id).is_none() {
                continue;
            }
            let p = super::layout_node(buf, ctx, scene, child_id);
            children_bbox = children_bbox.union(p.bbox);
        }
        // Post-pivot (per-node-buffer migration), children write into
        // their own buffers — none of them touch `buf`. The old fill_rect
        // that blanked `page_buf` under the animation region so the
        // WASM overlay didn't let base content bleed through is no
        // longer needed: the WASM animation's own buffer composites on
        // top in Phase C2 against an animation-region that page_buf no
        // longer paints into.
    } else if let Some(&first_frame) = children.first()
        && scene.get(first_frame).is_some()
    {
        let p = super::layout_node(buf, ctx, scene, first_frame);
        children_bbox = children_bbox.union(p.bbox);
    }

    let flow_h = ctx.y.saturating_sub(start_y);
    let region_h = if !children_bbox.is_empty() {
        children_bbox.h.max(flow_h)
    } else {
        flow_h
    };

    // Unified placement: `rect` is the visual region (what
    // `Scene::iter_placed()` reads), `flow_advance` is the flow-space
    // contribution, `bbox` unions the region with descendants.
    let has_region = region_h > 0 || !children.is_empty();
    let rect = Rect::new(
        data.x.unwrap_or(start_x),
        data.y.unwrap_or(start_y),
        ctx_width,
        if has_region { region_h.max(1) } else { 0 },
    );

    let _ = (has_region, id); // placed region derives from scene via `iter_placed`.
    let bbox = rect.union(children_bbox);
    let placement = Placement {
        rect,
        flow_advance: flow_h,
        bbox,
    };
    scene.update_placement(node_id, placement);
    placement
}
