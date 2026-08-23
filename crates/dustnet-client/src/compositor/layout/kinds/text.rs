//! `NodeKind::Text` scene-native layout.
//!
//! Covers every TextSource variant (Text, Pre, Heading, Art,
//! ElementDef, TextAnimate). Single-run / no-children content takes
//! the direct wrap path; multi-run / node-bearing-inline-children
//! content routes through `super::try_collect_inline_segments` and the
//! inline wrap/render primitives from `engine`.

use crate::compositor::layout::Rect;
use crate::compositor::layout::border::draw_hr;
use crate::compositor::layout::cell::{CellBuffer, CellStyle};
use crate::compositor::layout::engine::{
    self, LayoutCtx, Placement, resolve_or_inherit, try_temp_vec_from_slice,
};
use crate::compositor::scene::{NodeId, NodeKind, Scene, TextContent, TextSource};
use crate::parser::ast::{Alignment, HrStyle};

pub(crate) fn layout(
    buf: &mut CellBuffer,
    ctx: &mut LayoutCtx,
    scene: &mut Scene,
    node_id: NodeId,
) -> Placement {
    // Snapshot: TextContent + child ids.
    let (data, children) = {
        let Some(node) = scene.get(node_id) else {
            return Placement::empty_at(ctx.x, ctx.y);
        };
        let NodeKind::Text(d) = node.kind() else {
            return Placement::empty_at(ctx.x, ctx.y);
        };
        let Some(children) = try_temp_vec_from_slice(node.children(), ctx.governor.as_ref()) else {
            buf.record_allocation_failure();
            scene.record_resource_error();
            return Placement::empty_at(ctx.x, ctx.y);
        };
        (d.clone(), children)
    };

    let placement = match data.source {
        TextSource::Pre => layout_pre(buf, ctx, scene, node_id, &data),
        TextSource::Art => layout_art(buf, ctx, scene, node_id, &data),
        TextSource::ElementDef => layout_element_def(buf, ctx, scene, node_id, &data),
        TextSource::TextAnimate => layout_text_animate(buf, ctx, scene, node_id, &data),
        TextSource::Heading(level) => {
            layout_heading(buf, ctx, scene, node_id, &data, level, &children)
        }
        TextSource::Text => layout_text_plain(buf, ctx, scene, &data, node_id, &children),
    };
    scene.update_placement(node_id, placement);
    placement
}

fn layout_pre(
    buf: &mut CellBuffer,
    ctx: &mut LayoutCtx,
    scene: &mut Scene,
    node_id: NodeId,
    data: &TextContent,
) -> Placement {
    let start_x = ctx.x;
    let start_y = ctx.y;
    let width = ctx.width;

    // Alignment and height belong to the block, not to a run, so they are
    // measured over the concatenation of every run. Taking them from the first
    // run alone is what this used to do, and with more than one run it truncated
    // the block to whatever came before the first styled span.
    let mut whole = String::new();
    for run in &data.runs {
        whole.push_str(&run.text);
    }

    let block_offset = match data.align {
        Alignment::Left => 0u16,
        Alignment::Center | Alignment::Right => {
            let max_w = whole
                .split('\n')
                .map(|l| crate::compositor::layout::text::display_width(l, ctx.wcfg) as u16)
                .max()
                .unwrap_or(0);
            match data.align {
                Alignment::Center => ctx.width.saturating_sub(max_w) / 2,
                Alignment::Right => ctx.width.saturating_sub(max_w),
                _ => 0,
            }
        }
    };

    scene.allocate_buffer(node_id, width.max(1), 1);
    let line_count = whole.split('\n').count() as u16;
    let mut local_y: u16 = 0;
    if let Some(text_buf) = scene.layout_buffer_mut(node_id) {
        text_buf.ensure_height(line_count.max(1));

        // Runs are walked in order while a cursor is carried across them, because
        // a newline can fall anywhere -- inside a run or exactly on the boundary
        // between two. The cursor is what makes `IE `, `GB`, ` BE` land as one
        // line in three colours rather than three lines.
        let mut local_x: u16 = block_offset;
        for run in &data.runs {
            let style = CellStyle {
                fg: resolve_or_inherit(run.fg.as_ref(), ctx.style.fg, ctx.color_support),
                bg: resolve_or_inherit(run.bg.as_ref(), ctx.style.bg, ctx.color_support),
                // A run's own attribute wins; otherwise the block's is inherited,
                // so [pre dim] still dims a span that only set a colour.
                bold: run.bold || ctx.style.bold,
                italic: run.italic || ctx.style.italic,
                underline: run.underline || ctx.style.underline,
                strikethrough: run.strikethrough || ctx.style.strikethrough,
                dim: run.dim || ctx.style.dim,
                blink: run.blink || ctx.style.blink,
            };

            let mut first_segment = true;
            for segment in run.text.split('\n') {
                if !first_segment {
                    local_y = local_y.saturating_add(1);
                    local_x = block_offset;
                }
                first_segment = false;
                text_buf.ensure_height(local_y.saturating_add(1));
                if segment.is_empty() {
                    continue;
                }
                engine::put_str_clipped(
                    text_buf, local_x, local_y, segment, &style, width, ctx.wcfg,
                );
                local_x = local_x.saturating_add(crate::compositor::layout::text::display_width(
                    segment, ctx.wcfg,
                ) as u16);
            }
        }
        // The loop above counts newlines, not lines; the last line has none
        // after it.
        local_y = local_y.saturating_add(1);
    }

    let h = local_y;
    ctx.y = start_y.saturating_add(h);
    buf.ensure_height(ctx.y);
    let rect = Rect::new(start_x, start_y, width, h);
    Placement {
        rect,
        flow_advance: h,
        bbox: rect,
    }
}

fn layout_art(
    buf: &mut CellBuffer,
    ctx: &mut LayoutCtx,
    scene: &mut Scene,
    node_id: NodeId,
    data: &TextContent,
) -> Placement {
    let start_x = ctx.x;
    let start_y = ctx.y;
    let ctx_width = ctx.width;

    let content = data.runs.first().map(|r| r.text.as_str()).unwrap_or("");

    scene.allocate_buffer(node_id, ctx_width.max(1), 1);
    let line_count = content.split('\n').count() as u16;
    let mut local_y: u16 = 0;
    let style = ctx.style.clone();
    if let Some(text_buf) = scene.layout_buffer_mut(node_id) {
        text_buf.ensure_height(line_count.max(1));
        for line in content.split('\n') {
            text_buf.ensure_height(local_y.saturating_add(1));
            engine::put_str_clipped(text_buf, 0, local_y, line, &style, ctx_width, ctx.wcfg);
            local_y = local_y.saturating_add(1);
        }
    }

    let h = local_y;
    ctx.y = start_y.saturating_add(h);
    buf.ensure_height(ctx.y);
    let rect = Rect::new(start_x, start_y, ctx_width, h);
    Placement {
        rect,
        flow_advance: h,
        bbox: rect,
    }
}

fn layout_element_def(
    buf: &mut CellBuffer,
    ctx: &mut LayoutCtx,
    scene: &mut Scene,
    node_id: NodeId,
    data: &TextContent,
) -> Placement {
    let start_x = ctx.x;
    let start_y = ctx.y;
    let ctx_width = ctx.width;

    let run = data.runs.first();
    let content = run.map(|r| r.text.as_str()).unwrap_or("").trim();
    let style = CellStyle {
        fg: resolve_or_inherit(
            run.and_then(|r| r.fg.as_ref()),
            ctx.style.fg,
            ctx.color_support,
        ),
        ..ctx.style.clone()
    };

    let content_w = (crate::compositor::layout::text::display_width(content, ctx.wcfg)
        .min(ctx_width as usize)) as u16;

    // ElementDef writes past ctx.width (it uses buf.width as the clip
    // boundary in the pre-pivot code). Mirror that by sizing the buffer
    // to the full content width.
    let buf_w = content_w.max(ctx_width).max(1);
    scene.allocate_buffer(node_id, buf_w, 1);
    if let Some(text_buf) = scene.layout_buffer_mut(node_id) {
        engine::put_str_clipped(text_buf, 0, 0, content, &style, buf_w, ctx.wcfg);
    }

    ctx.y = start_y.saturating_add(1);
    buf.ensure_height(ctx.y);

    let rect = Rect::new(start_x, start_y, ctx_width, 1);
    let bbox_rect = Rect::new(start_x, start_y, content_w.max(1), 1);
    Placement {
        rect,
        flow_advance: 1,
        bbox: rect.union(bbox_rect),
    }
}

fn layout_text_animate(
    buf: &mut CellBuffer,
    ctx: &mut LayoutCtx,
    scene: &mut Scene,
    node_id: NodeId,
    data: &TextContent,
) -> Placement {
    let start_x = ctx.x;
    let start_y = ctx.y;
    let ctx_width = ctx.width;

    let content = data.runs.first().map(|r| r.text.as_str()).unwrap_or("");

    scene.allocate_buffer(node_id, ctx_width.max(1), 1);
    let style = ctx.style.clone();
    if let Some(text_buf) = scene.layout_buffer_mut(node_id) {
        engine::put_str_clipped(text_buf, 0, 0, content, &style, ctx_width, ctx.wcfg);
    }

    ctx.y = start_y.saturating_add(1);
    buf.ensure_height(ctx.y);

    let rect = Rect::new(start_x, start_y, ctx_width, 1);
    Placement {
        rect,
        flow_advance: 1,
        bbox: rect,
    }
}

fn layout_heading(
    buf: &mut CellBuffer,
    ctx: &mut LayoutCtx,
    scene: &mut Scene,
    node_id: NodeId,
    data: &TextContent,
    level: u8,
    children: &[NodeId],
) -> Placement {
    let start_x = ctx.x;
    let start_y = ctx.y;
    let width = ctx.width;

    let run = data.runs.first();
    let content = run.map(|r| r.text.as_str().trim()).unwrap_or("");

    let style = CellStyle {
        fg: resolve_or_inherit(
            run.and_then(|r| r.fg.as_ref()),
            ctx.style.fg,
            ctx.color_support,
        ),
        bold: true,
        ..ctx.style.clone()
    };

    let inline_lines = if children.is_empty() {
        None
    } else {
        let Some(segments) = super::try_collect_inline_segments(
            children,
            scene,
            &style,
            ctx.color_support,
            None,
            ctx.governor.as_ref(),
        ) else {
            buf.record_allocation_failure();
            scene.record_resource_error();
            return Placement::empty_at(start_x, start_y);
        };
        if segments.is_empty() {
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
        }
    };

    scene.allocate_buffer(node_id, width.max(1), 1);

    let mut local_ctx = LayoutCtx {
        x: 0,
        y: 0,
        width,
        viewport_height: ctx.viewport_height,
        color_support: ctx.color_support,
        wcfg: ctx.wcfg,
        style: style.clone(),
        governor: ctx.governor.clone(),
    };

    let wrapped = match scene.layout_buffer_mut(node_id) {
        Some(text_buf) if !content.is_empty() => {
            engine::render_wrapped_text(text_buf, &mut local_ctx, content, &style, Alignment::Left)
        }
        _ => true,
    };
    if !wrapped {
        buf.record_allocation_failure();
        scene.record_resource_error();
        return Placement::empty_at(start_x, start_y);
    }

    if let Some(lines) = inline_lines {
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
            Alignment::Left,
            (start_x, start_y),
        );
        if let Some(b) = scene.layout_buffer_mut(node_id) {
            *b = text_buf;
        }
    }

    if level == 1 && local_ctx.y > 0 {
        if let Some(text_buf) = scene.layout_buffer_mut(node_id) {
            text_buf.ensure_height(local_ctx.y.saturating_add(1));
            draw_hr(text_buf, local_ctx.y, 0, width, HrStyle::Heavy, &style);
        }
        local_ctx.y = local_ctx.y.saturating_add(1);
    }

    let h = local_ctx.y;
    ctx.y = start_y.saturating_add(h);
    buf.ensure_height(ctx.y);
    let rect = Rect::new(start_x, start_y, width, h);
    Placement {
        rect,
        flow_advance: h,
        bbox: rect,
    }
}

fn layout_text_plain(
    buf: &mut CellBuffer,
    ctx: &mut LayoutCtx,
    scene: &mut Scene,
    data: &TextContent,
    node_id: NodeId,
    children: &[NodeId],
) -> Placement {
    let has_children = !children.is_empty();
    let multi_run = data.runs.len() > 1;
    if has_children || multi_run {
        // Phase 1 pivot: multi-run / inline-children text allocates its
        // own buffer and writes at local (0, 0). Focusable rects inside
        // (Links, Buttons) are written via render_inline_lines_with_origin
        // with the node's global origin so hit-testing reads screen coords.
        let start_x = ctx.x;
        let start_y = ctx.y;
        let width = ctx.width;
        let Some(segments) = super::try_collect_inline_segments(
            &[node_id],
            scene,
            &ctx.style,
            ctx.color_support,
            None,
            ctx.governor.as_ref(),
        ) else {
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
            style: ctx.style.clone(),
            governor: ctx.governor.clone(),
        };

        if let Some(lines) = lines {
            // We need a mutable borrow of text_buf AND scene in the
            // render call. Take the buffer by raw pointer dance? No — we
            // can pass scene through render_inline_lines_with_origin which
            // takes both; the node's buffer is accessed via
            // scene.layout_buffer_mut inside each call. Simpler: pull the
            // buffer out, render, don't touch scene inside the call, then
            // write focusable rects separately. But render_inline_lines
            // already interleaves cell writes and focusable_rect writes.
            // Use a scope to reborrow: take the text buffer via a
            // "swap-out / swap-in" — replace with an empty placeholder
            // buffer during the render so scene is free to be mutated.
            let placeholder = crate::compositor::layout::cell::CellBuffer::new(1, 1);
            let mut text_buf = match scene.layout_buffer_mut(node_id) {
                Some(b) => std::mem::replace(b, placeholder),
                None => placeholder,
            };
            engine::render_inline_lines_with_origin(
                &mut text_buf,
                &mut local_ctx,
                scene,
                &lines,
                data.align,
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
        return Placement {
            rect,
            flow_advance: h,
            bbox: rect,
        };
    }

    // Phase 1 pivot (per-node-buffer migration, Strategy D): single-run /
    // no-children text allocates its own `CellBuffer` and writes at LOCAL
    // coordinates (0, 0) inside it. The dispatcher translates the local
    // extent to a global `placement.rect` so the composite walk blits the
    // text buffer at the right screen position — no writes to `buf`
    // (page_buf) happen on this path. The companion pivot of
    // `layout_box_node` (Phase 2) keeps paint order correct when text
    // sits beside or inside a box, by making the box chrome a separate
    // per-node buffer that blits in tree order.

    let start_x = ctx.x;
    let start_y = ctx.y;
    let width = ctx.width;

    let run = data.runs.first();
    let content = run.map(|r| r.text.as_str()).unwrap_or("");

    let style = run
        .map(|r| CellStyle {
            fg: resolve_or_inherit(r.fg.as_ref(), ctx.style.fg, ctx.color_support),
            bg: resolve_or_inherit(r.bg.as_ref(), ctx.style.bg, ctx.color_support),
            bold: r.bold || ctx.style.bold,
            italic: r.italic || ctx.style.italic,
            underline: r.underline || ctx.style.underline,
            strikethrough: r.strikethrough || ctx.style.strikethrough,
            dim: r.dim || ctx.style.dim,
            blink: r.blink || ctx.style.blink,
        })
        .unwrap_or_else(|| ctx.style.clone());

    scene.allocate_buffer(node_id, width.max(1), 1);

    let mut local_ctx = LayoutCtx {
        x: 0,
        y: 0,
        width,
        viewport_height: ctx.viewport_height,
        color_support: ctx.color_support,
        wcfg: ctx.wcfg,
        style: ctx.style.clone(),
        governor: ctx.governor.clone(),
    };

    let wrapped = if content.is_empty() {
        true
    } else {
        let text = content.trim_matches(|c: char| c == '\t' || c == ' ');
        match scene.layout_buffer_mut(node_id) {
            Some(text_buf) if !text.is_empty() => {
                engine::render_wrapped_text(text_buf, &mut local_ctx, text, &style, data.align)
            }
            _ => true,
        }
    };
    if !wrapped {
        buf.record_allocation_failure();
        scene.record_resource_error();
        return Placement::empty_at(start_x, start_y);
    }

    let h = local_ctx.y;
    ctx.y = start_y.saturating_add(h);

    // Keep page_buf tall enough for downstream consumers (document-mode
    // scroll height, `Compositor` dimensions). Pre-pivot, writes to `buf`
    // grew it via `ensure_height`; post-pivot, content-height bookkeeping
    // still has to happen here until `page.buf` retires in Phase 6.
    buf.ensure_height(ctx.y);

    let rect = Rect::new(start_x, start_y, width, h);
    Placement {
        rect,
        flow_advance: h,
        bbox: rect,
    }
}
