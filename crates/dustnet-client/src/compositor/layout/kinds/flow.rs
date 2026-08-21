//! `NodeKind::Flow` layout. Vertical container — every AML element
//! that maps to `NodeKind::Flow` flows through here. Scene-native:
//! each branch reads from `FlowData` and walks scene children via
//! `super::layout_children_scene` or kind-specific helpers.

use crate::compositor::layout::Rect;
use crate::compositor::layout::cell::{CellBuffer, CellStyle};
use crate::compositor::layout::engine::{self, LayoutCtx, Placement, try_temp_vec_from_slice};
use crate::compositor::layout::text::display_width;
use crate::compositor::scene::{FlowData, FlowSource, NodeId, NodeKind, Scene};
use crate::parser::ast::ListStyle;

fn layout_details(
    buf: &mut CellBuffer,
    ctx: &mut LayoutCtx,
    scene: &mut Scene,
    node_id: NodeId,
    data: &FlowData,
    children: &[NodeId],
) -> Placement {
    let start_x = ctx.x;
    let start_y = ctx.y;
    let ctx_width = ctx.width;
    let mut bbox = Rect::new(start_x, start_y, 0, 0);

    let indicator = if data.details_open {
        "\u{25BC}"
    } else {
        "\u{25B6}"
    };
    let bold_style = CellStyle {
        bold: true,
        ..ctx.style.clone()
    };

    let summary_count = data.details_summary_count as usize;
    let summary_end = summary_count.min(children.len());
    let summary_children = children.get(..summary_end).unwrap_or_default();
    let body_children = children.get(summary_count..).unwrap_or_default();

    // The Details node owns a buffer for its summary row (indicator +
    // inline summary). Body children remain their own buffered nodes
    // laid out in global coords below.
    scene.allocate_buffer(node_id, ctx_width.max(1), 1);

    let summary_h: u16;
    if !summary_children.is_empty() {
        let indicator_text = format!("{} ", indicator);
        let indicator_w = display_width(&indicator_text, ctx.wcfg) as u16;
        scene.update_focusable_rect(node_id, Rect::new(start_x, start_y, indicator_w, 1));

        let inline_width = ctx_width.saturating_sub(indicator_w);
        let Some(segments) = super::try_collect_inline_segments(
            summary_children,
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
                inline_width as usize,
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
        let mut local_ctx = LayoutCtx {
            x: indicator_w,
            y: 0,
            width: inline_width,
            viewport_height: ctx.viewport_height,
            color_support: ctx.color_support,
            wcfg: ctx.wcfg,
            style: ctx.style.clone(),
            governor: ctx.governor.clone(),
        };

        let placeholder = CellBuffer::new(1, 1);
        let mut details_buf = match scene.layout_buffer_mut(node_id) {
            Some(b) => std::mem::replace(b, placeholder),
            None => placeholder,
        };
        // Indicator at local (0, 0) in bold.
        engine::put_str_clipped(
            &mut details_buf,
            0,
            0,
            &indicator_text,
            &bold_style,
            ctx_width,
            ctx.wcfg,
        );
        if let Some(lines) = lines {
            engine::render_inline_lines_with_origin(
                &mut details_buf,
                &mut local_ctx,
                scene,
                &lines,
                crate::parser::ast::Alignment::Left,
                (start_x, start_y),
            );
        }
        if let Some(b) = scene.layout_buffer_mut(node_id) {
            *b = details_buf;
        }

        summary_h = local_ctx.y.max(1);
        let summary_rect = Rect::new(start_x + indicator_w, start_y, inline_width, summary_h);
        bbox = bbox.union(summary_rect);
    } else {
        let summary_text = format!(
            "{} {}",
            indicator,
            data.details_summary.as_deref().unwrap_or("")
        );
        let width = display_width(&summary_text, ctx.wcfg).min(ctx_width as usize) as u16;
        if let Some(details_buf) = scene.layout_buffer_mut(node_id) {
            engine::put_str_clipped(
                details_buf,
                0,
                0,
                &summary_text,
                &bold_style,
                ctx_width,
                ctx.wcfg,
            );
        }
        scene.update_focusable_rect(node_id, Rect::new(start_x, start_y, width, 1));
        summary_h = 1;
    }

    ctx.y = start_y.saturating_add(summary_h);

    if data.details_open && !body_children.is_empty() {
        let indent = 2u16;
        let saved_x = ctx.x;
        let saved_w = ctx.width;
        ctx.x = ctx.x.saturating_add(indent);
        ctx.width = ctx.width.saturating_sub(indent);
        let children_p = super::layout_children_scene(buf, ctx, scene, body_children);
        bbox = bbox.union(children_p.bbox);
        ctx.x = saved_x;
        ctx.width = saved_w;
    }

    let h = ctx.y.saturating_sub(start_y);
    buf.ensure_height(ctx.y);
    let rect = Rect::new(start_x, start_y, ctx_width, h);
    let bbox = rect.union(bbox);
    Placement {
        rect,
        flow_advance: h,
        bbox,
    }
}

fn layout_list(
    buf: &mut CellBuffer,
    ctx: &mut LayoutCtx,
    scene: &mut Scene,
    node_id: NodeId,
    data: &FlowData,
    children: &[NodeId],
) -> Placement {
    let start_x = ctx.x;
    let start_y = ctx.y;
    let width = ctx.width;
    let mut bbox = Rect::new(start_x, start_y, 0, 0);

    let style = data.list_style.unwrap_or(ListStyle::Bullet);
    let bullet_char = data.list_bullet_char;
    // Reserve one blank cell between the widest marker and item content.
    // Numbered lists right-align shorter markers so `1. Foo` and `10. Bar`
    // share the same content column without either becoming `1.Foo`.
    let indent = match style {
        ListStyle::Number => children.len().max(1).to_string().len() as u16 + 2,
        ListStyle::None => 0,
        _ => 2,
    };

    // The List node owns a buffer of its own chrome: markers at column
    // 0 of each item row. Items themselves lay out at global (x+indent)
    // into their own buffers. Marker rows expand as each item advances
    // the cursor.
    scene.allocate_buffer(node_id, width.max(1), 1);
    let base_style = ctx.style.clone();

    for (i, &child_id) in children.iter().enumerate() {
        if scene.get(child_id).is_none() {
            continue;
        }

        let marker = match style {
            ListStyle::Bullet => bullet_char.unwrap_or('•').to_string(),
            ListStyle::Number => format!("{}.", i + 1),
            ListStyle::Dash => "–".to_string(),
            ListStyle::Arrow => "→".to_string(),
            ListStyle::None => String::new(),
        };
        let marker_y = ctx.y.saturating_sub(start_y);
        if !marker.is_empty() {
            let marker_x = indent.saturating_sub(marker.chars().count() as u16 + 1);
            if let Some(list_buf) = scene.layout_buffer_mut(node_id) {
                list_buf.ensure_height(marker_y + 1);
                engine::put_str_clipped(
                    list_buf,
                    marker_x,
                    marker_y,
                    &marker,
                    &base_style,
                    width,
                    ctx.wcfg,
                );
            }
        }

        let saved_x = ctx.x;
        let saved_w = ctx.width;
        ctx.x = ctx.x.saturating_add(indent);
        ctx.width = ctx.width.saturating_sub(indent);

        let child_placement = super::layout_node(buf, ctx, scene, child_id);
        bbox = bbox.union(child_placement.bbox);

        ctx.x = saved_x;
        ctx.width = saved_w;
    }

    let h = ctx.y.saturating_sub(start_y);
    buf.ensure_height(ctx.y);
    let rect = Rect::new(start_x, start_y, width, h);
    let bbox = rect.union(bbox);
    Placement {
        rect,
        flow_advance: h,
        bbox,
    }
}

pub(crate) fn layout(
    buf: &mut CellBuffer,
    ctx: &mut LayoutCtx,
    scene: &mut Scene,
    node_id: NodeId,
) -> Placement {
    // Snapshot: FlowData + children.
    let (data, children) = {
        let Some(node) = scene.get(node_id) else {
            return Placement::empty_at(ctx.x, ctx.y);
        };
        let NodeKind::Flow(d) = node.kind() else {
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
        // Structural containers: no own chrome, just walk children.
        FlowSource::Header
        | FlowSource::Body
        | FlowSource::Footer
        | FlowSource::Thead
        | FlowSource::Tbody
        | FlowSource::Pagination
        | FlowSource::Col
        | FlowSource::Form
        | FlowSource::Frame
        | FlowSource::State => super::layout_children_scene(buf, ctx, scene, &children),

        FlowSource::Nav => {
            let start_y = ctx.y;
            let inner = super::layout_children_scene(buf, ctx, scene, &children);
            // Sticky region is derived from scene at query time via
            // `Scene::iter_sticky()` reading FlowData.sticky + placement.rect.
            let _ = (data.sticky, start_y);
            inner
        }

        FlowSource::Box => engine::layout_box_node(
            buf,
            ctx,
            scene,
            node_id,
            None,
            None,
            data.width.unwrap_or(crate::parser::ast::Dimension::Fill),
            data.height.unwrap_or(crate::parser::ast::Dimension::Fit),
            data.border.to_ast(),
            data.title.as_deref(),
            data.join_top,
            data.join_bottom,
            data.join_left,
            data.join_right,
            data.padding,
            data.align,
            data.fg.as_ref(),
            data.bg.as_ref(),
        ),

        FlowSource::List => layout_list(buf, ctx, scene, node_id, &data, &children),

        FlowSource::Item => {
            let start_x = ctx.x;
            let start_y = ctx.y;
            let width = ctx.width;
            let Some(segments) = super::try_collect_inline_segments(
                &children,
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
                let placeholder = CellBuffer::new(1, 1);
                let mut item_buf = match scene.layout_buffer_mut(node_id) {
                    Some(b) => std::mem::replace(b, placeholder),
                    None => placeholder,
                };
                engine::render_inline_lines_with_origin(
                    &mut item_buf,
                    &mut local_ctx,
                    scene,
                    &lines,
                    crate::parser::ast::Alignment::Left,
                    (start_x, start_y),
                );
                if let Some(b) = scene.layout_buffer_mut(node_id) {
                    *b = item_buf;
                }
            }
            let h = local_ctx.y;
            ctx.y = start_y.saturating_add(h);
            buf.ensure_height(ctx.y);
            let rect = crate::compositor::layout::Rect::new(start_x, start_y, width, h);
            Placement {
                rect,
                flow_advance: h,
                bbox: rect,
            }
        }

        FlowSource::Details => layout_details(buf, ctx, scene, node_id, &data, &children),
    };

    scene.update_placement(node_id, placement);
    placement
}
