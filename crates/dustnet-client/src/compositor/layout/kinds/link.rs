//! `NodeKind::Link` scene-native layout.
//!
//! Simple case: exactly one `Text` child with a single run — rendered
//! underlined with a focusable at the rendered width. Complex case:
//! descendants walked through `super::try_collect_inline_segments` under
//! the link's underline+focusable styling, then
//! `engine::try_wrap_inline_segments` + `render_inline_lines` handle
//! multi-segment inline wrapping.

use crate::compositor::layout::Rect;
use crate::compositor::layout::cell::{CellBuffer, CellStyle};
use crate::compositor::layout::engine::{self, LayoutCtx, Placement};
use crate::compositor::layout::text::display_width;
use crate::compositor::scene::{NodeId, NodeKind, Scene};

pub(crate) fn layout(
    buf: &mut CellBuffer,
    ctx: &mut LayoutCtx,
    scene: &mut Scene,
    node_id: NodeId,
) -> Placement {
    // Guard.
    if !matches!(
        scene.get(node_id).map(|n| n.kind()),
        Some(NodeKind::Link(_))
    ) {
        return Placement::empty_at(ctx.x, ctx.y);
    }

    if let Some(simple) = try_simple_link(buf, ctx, scene, node_id) {
        scene.update_placement(node_id, simple);
        return simple;
    }

    // Complex link: collect descendants as inline segments under the
    // link's underline + focusable styling, render into the link's own
    // buffer at local coords. focusable_origin = (start_x, start_y) so
    // the link's own focusable_rect (written via render_inline_lines
    // when its InlineFocusable is seen) still lands at screen coords.
    let start_x = ctx.x;
    let start_y = ctx.y;
    let width = ctx.width;

    let link_style = CellStyle {
        underline: true,
        ..ctx.style.clone()
    };
    let foc = crate::compositor::layout::engine::InlineFocusable { node_id };
    let segments = {
        let scene_ref: &Scene = &*scene;
        let roots = scene_ref
            .get(node_id)
            .map(|node| node.children())
            .unwrap_or_default();
        super::try_collect_inline_segments(
            roots,
            scene_ref,
            &link_style,
            ctx.color_support,
            Some(&foc),
            ctx.governor.as_ref(),
        )
    };
    let Some(segments) = segments else {
        buf.record_allocation_failure();
        scene.record_resource_error();
        return Placement::empty_at(start_x, start_y);
    };
    let lines = if segments.is_empty() {
        None
    } else {
        let Some(lines) = engine::try_wrap_inline_segments(
            &segments,
            width as usize,
            ctx.wcfg,
            ctx.governor.as_ref(),
        ) else {
            buf.record_allocation_failure();
            scene.record_resource_error();
            return Placement::empty_at(start_x, start_y);
        };
        Some(lines)
    };
    drop(segments);

    scene.allocate_buffer(node_id, width.max(1), 1);

    let mut local_ctx = LayoutCtx {
        x: 0,
        y: 0,
        width,
        viewport_height: ctx.viewport_height,
        color_support: ctx.color_support,
        wcfg: ctx.wcfg,
        style: link_style,
        governor: ctx.governor.clone(),
    };

    if let Some(lines) = lines {
        let placeholder = CellBuffer::new(1, 1);
        let mut text_buf = match scene.layout_buffer_mut(node_id) {
            Some(b) => std::mem::replace(b, placeholder),
            None => placeholder,
        };
        engine::render_inline_lines_with_origin(
            &mut text_buf,
            &mut local_ctx,
            scene,
            &lines,
            crate::parser::ast::Alignment::Left,
            (start_x, start_y),
        );
        if let Some(b) = scene.layout_buffer_mut(node_id) {
            *b = text_buf;
        }
    }

    let h = local_ctx.y;
    ctx.y = start_y.saturating_add(h);
    buf.ensure_height(ctx.y);
    let rect = Rect::new(start_x, start_y, width, h);
    let placement = Placement {
        rect,
        flow_advance: h,
        bbox: rect,
    };
    scene.update_placement(node_id, placement);
    placement
}

fn try_simple_link(
    buf: &mut CellBuffer,
    ctx: &mut LayoutCtx,
    scene: &mut Scene,
    node_id: NodeId,
) -> Option<Placement> {
    // Snapshot the simple-case decision from the read phase.
    let (label, style) = {
        let node = scene.get(node_id)?;
        let children = node.children();
        let [only_child] = children else {
            return None;
        };
        let text_node = scene.get(*only_child)?;
        let NodeKind::Text(tc) = text_node.kind() else {
            return None;
        };
        let ([run], true) = (tc.runs.as_slice(), text_node.children().is_empty()) else {
            return None;
        };
        let label = run.text.trim().to_string();
        if label.is_empty() {
            return None;
        }
        let style = CellStyle {
            underline: true,
            bold: run.bold || ctx.style.bold,
            italic: run.italic || ctx.style.italic,
            dim: run.dim || ctx.style.dim,
            fg: engine::resolve_or_inherit(run.fg.as_ref(), ctx.style.fg, ctx.color_support),
            bg: engine::resolve_or_inherit(run.bg.as_ref(), ctx.style.bg, ctx.color_support),
            ..ctx.style.clone()
        };
        (label, style)
    };

    let start_x = ctx.x;
    let start_y = ctx.y;
    let width = ctx.width;

    let w = (display_width(&label, ctx.wcfg) as u16).min(width);
    scene.allocate_buffer(node_id, width.max(1), 1);
    if let Some(text_buf) = scene.layout_buffer_mut(node_id) {
        engine::put_str_clipped(text_buf, 0, 0, &label, &style, width, ctx.wcfg);
    }
    scene.update_focusable_rect(node_id, Rect::new(start_x, start_y, w, 1));

    ctx.y = start_y.saturating_add(1);
    buf.ensure_height(ctx.y);

    let rect = Rect::new(start_x, start_y, width, 1);
    Some(Placement {
        rect,
        flow_advance: 1,
        bbox: rect,
    })
}
