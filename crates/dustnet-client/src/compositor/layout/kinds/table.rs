//! `NodeKind::Table` scene-native layout. Walks the table's scene
//! children (`Tr` / `Flow(Thead|Tbody)` → `Tr`) to collect cell data,
//! allocates column widths, renders each row with `│` separators,
//! and draws a `─┼─` separator after the header row.

use crate::color::ResolvedColor;
use crate::compositor::layout::Rect;
use crate::compositor::layout::cell::{CellBuffer, CellStyle};
use crate::compositor::layout::engine::{self, LayoutCtx, Placement, try_temp_vec_filled};
use crate::compositor::layout::text::{char_width, display_width};
use crate::compositor::scene::{FlowSource, NodeId, NodeKind, Scene};

struct TableCellData {
    text: String,
    fg: Option<ResolvedColor>,
    is_header: bool,
}

pub(crate) fn layout(
    buf: &mut CellBuffer,
    ctx: &mut LayoutCtx,
    scene: &mut Scene,
    node_id: NodeId,
) -> Placement {
    let start_x = ctx.x;
    let start_y = ctx.y;
    let ctx_width = ctx.width;

    // Snapshot: children + full row data. Read-only walk; `scene` as
    // `&Scene` via reborrow is sufficient.
    let rows: Vec<Vec<TableCellData>> = {
        let scene_ref: &Scene = &*scene;
        let node = match scene_ref.get(node_id) {
            Some(n) => n,
            None => return Placement::empty_at(start_x, start_y),
        };
        match collect_table_rows(scene_ref, node.children()) {
            Some(rows) => rows,
            None => {
                buf.record_allocation_failure();
                scene.record_resource_error();
                return Placement::empty_at(start_x, start_y);
            }
        }
    };
    if rows.is_empty() {
        let placement = Placement::empty_at(start_x, start_y);
        scene.update_placement(node_id, placement);
        return placement;
    }

    let col_count = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    if col_count == 0 {
        let placement = Placement::empty_at(start_x, start_y);
        scene.update_placement(node_id, placement);
        return placement;
    }

    let Some(mut col_widths) = try_temp_vec_filled(col_count, 0usize, ctx.governor.as_ref()) else {
        buf.record_allocation_failure();
        scene.record_resource_error();
        return Placement::empty_at(start_x, start_y);
    };
    for row in &rows {
        for (j, cell) in row.iter().enumerate() {
            let w = display_width(&cell.text, ctx.wcfg);
            if let Some(slot) = col_widths.get_mut(j) {
                *slot = (*slot).max(w);
            }
        }
    }

    let total_width: usize = col_widths.iter().sum::<usize>() + (col_count - 1) * 3;
    if total_width > ctx.width as usize {
        let available = ctx.width as usize;
        let separator_space = (col_count - 1) * 3;
        let content_space = available.saturating_sub(separator_space);
        let total_content: usize = col_widths.iter().sum();
        for w in col_widths.iter_mut() {
            *w = (*w * content_space).checked_div(total_content).unwrap_or(0);
            *w = (*w).max(1);
        }
    }

    scene.allocate_buffer(node_id, ctx_width.max(1), 1);
    let base_style = ctx.style.clone();
    let mut local_y: u16 = 0;

    if let Some(table_buf) = scene.layout_buffer_mut(node_id) {
        for (i, row) in rows.iter().enumerate() {
            table_buf.ensure_height(local_y.saturating_add(1));

            let mut col_x: u16 = 0;
            for (j, cell) in row.iter().enumerate() {
                let cw = col_widths.get(j).copied().unwrap_or(5);
                let style = CellStyle {
                    fg: cell.fg,
                    bold: cell.is_header,
                    ..base_style.clone()
                };

                let display = if display_width(&cell.text, ctx.wcfg) > cw {
                    let mut s = String::new();
                    let mut w = 0;
                    for ch in cell.text.chars() {
                        let ch_w = char_width(ch, ctx.wcfg);
                        if w + ch_w > cw.saturating_sub(1) {
                            break;
                        }
                        s.push(ch);
                        w += ch_w;
                    }
                    s.push('…');
                    s
                } else {
                    let padding = cw.saturating_sub(display_width(&cell.text, ctx.wcfg));
                    let mut s = cell.text.clone();
                    s.extend(std::iter::repeat_n(' ', padding));
                    s
                };

                engine::put_str_clipped(
                    table_buf, col_x, local_y, &display, &style, ctx_width, ctx.wcfg,
                );
                col_x += cw as u16;

                if j + 1 < row.len() {
                    engine::put_str_clipped(
                        table_buf,
                        col_x,
                        local_y,
                        " │ ",
                        &base_style,
                        ctx_width,
                        ctx.wcfg,
                    );
                    col_x += 3;
                }
            }

            local_y = local_y.saturating_add(1);

            if i == 0 && row.iter().any(|c| c.is_header) {
                table_buf.ensure_height(local_y.saturating_add(1));
                let sep_style = base_style.clone();
                let mut sx: u16 = 0;
                for (j, &cw) in col_widths.iter().enumerate() {
                    for _ in 0..cw {
                        table_buf.put_char(sx, local_y, '─', &sep_style);
                        sx += 1;
                    }
                    if j + 1 < col_count {
                        table_buf.put_char(sx, local_y, '─', &sep_style);
                        table_buf.put_char(sx + 1, local_y, '┼', &sep_style);
                        table_buf.put_char(sx + 2, local_y, '─', &sep_style);
                        sx += 3;
                    }
                }
                local_y = local_y.saturating_add(1);
            }
        }
    }

    let h = local_y;
    ctx.y = start_y.saturating_add(h);
    buf.ensure_height(ctx.y);

    let rect = Rect::new(start_x, start_y, ctx_width, h);
    let placement = Placement {
        rect,
        flow_advance: h,
        bbox: rect,
    };
    scene.update_placement(node_id, placement);
    placement
}

/// Count the `Tr` rows a table's children produce, mirroring
/// `collect_table_rows` so the row vector can be reserved exactly.
fn count_table_rows(scene: &Scene, children: &[NodeId]) -> usize {
    let mut rows = 0usize;
    for &child_id in children {
        let Some(child) = scene.get(child_id) else {
            continue;
        };
        match child.kind() {
            NodeKind::Tr => rows = rows.saturating_add(1),
            NodeKind::Flow(data)
                if matches!(data.source, FlowSource::Thead | FlowSource::Tbody) =>
            {
                for &inner_id in child.children() {
                    if scene
                        .get(inner_id)
                        .is_some_and(|inner| matches!(inner.kind(), NodeKind::Tr))
                    {
                        rows = rows.saturating_add(1);
                    }
                }
            }
            _ => {}
        }
    }
    rows
}

fn collect_table_rows(scene: &Scene, children: &[NodeId]) -> Option<Vec<Vec<TableCellData>>> {
    let mut rows = Vec::new();
    rows.try_reserve_exact(count_table_rows(scene, children))
        .ok()?;

    for &child_id in children {
        let Some(child) = scene.get(child_id) else {
            continue;
        };
        match child.kind() {
            NodeKind::Tr => {
                rows.push(collect_row_cells(scene, child.children())?);
            }
            NodeKind::Flow(data)
                if matches!(data.source, FlowSource::Thead | FlowSource::Tbody) =>
            {
                for &inner_id in child.children() {
                    let Some(inner) = scene.get(inner_id) else {
                        continue;
                    };
                    if matches!(inner.kind(), NodeKind::Tr) {
                        rows.push(collect_row_cells(scene, inner.children())?);
                    }
                }
            }
            _ => {}
        }
    }

    Some(rows)
}

/// Count the cells a row's children produce, mirroring `collect_row_cells`.
fn count_row_cells(scene: &Scene, children: &[NodeId]) -> usize {
    children
        .iter()
        .filter(|&&child_id| {
            scene
                .get(child_id)
                .is_some_and(|child| matches!(child.kind(), NodeKind::Th(_) | NodeKind::Td(_)))
        })
        .count()
}

fn collect_row_cells(scene: &Scene, children: &[NodeId]) -> Option<Vec<TableCellData>> {
    let mut cells = Vec::new();
    cells
        .try_reserve_exact(count_row_cells(scene, children))
        .ok()?;
    for &child_id in children {
        let Some(child) = scene.get(child_id) else {
            continue;
        };
        match child.kind() {
            NodeKind::Th(_) => {
                cells.push(TableCellData {
                    text: extract_text_content(scene, child.children())?,
                    fg: None,
                    is_header: true,
                });
            }
            NodeKind::Td(data) => {
                cells.push(TableCellData {
                    text: extract_text_content(scene, child.children())?,
                    fg: data
                        .fg
                        .as_ref()
                        .and_then(|c| c.resolve(crate::color::ColorSupport::Truecolor)),
                    is_header: false,
                });
            }
            _ => {}
        }
    }
    Some(cells)
}

/// Count the bytes `extract_text_content` will produce, mirroring it exactly
/// so the string can be reserved rather than grown from remote cell text.
fn count_text_bytes(scene: &Scene, children: &[NodeId]) -> Option<usize> {
    let mut total = 0usize;
    for &child_id in children {
        let Some(child) = scene.get(child_id) else {
            continue;
        };
        if let NodeKind::Text(tc) = child.kind() {
            for run in &tc.runs {
                total = total.checked_add(run.text.trim().len())?;
            }
            total = total.checked_add(count_text_bytes(scene, child.children())?)?;
        }
    }
    Some(total)
}

fn extract_text_content(scene: &Scene, children: &[NodeId]) -> Option<String> {
    let mut text = String::new();
    text.try_reserve_exact(count_text_bytes(scene, children)?)
        .ok()?;
    push_text_content(scene, children, &mut text);
    Some(text)
}

/// Fill an already-reserved buffer. Split from `extract_text_content` so the
/// recursion reserves once at the top rather than once per level.
fn push_text_content(scene: &Scene, children: &[NodeId], text: &mut String) {
    for &child_id in children {
        let Some(child) = scene.get(child_id) else {
            continue;
        };
        if let NodeKind::Text(tc) = child.kind() {
            for run in &tc.runs {
                text.push_str(run.text.trim());
            }
            push_text_content(scene, child.children(), text);
        }
    }
}
