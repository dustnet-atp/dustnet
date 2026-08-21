//! The present pass: emit ANSI for dirty rectangles.
//!
//! The ANSI renderer, terminal-capability probe, and SGR formatting
//! live together as the final stage of the pipeline — translating
//! composited buffer state into bytes over the wire.

pub mod ansi;
pub mod probe;

use std::io::{self, Write};

use crate::compositor::layout::Rect;
use crate::compositor::layout::cell::{CellBuffer, CellStyle};
use ansi::{RESET, TERMINAL_DEFAULT_SGR, write_cursor, write_style_sgr};

/// Render a cell buffer to an output stream.
///
/// `viewport_offset` is the scroll position (first visible row).
/// `viewport_height` is how many rows fit on the terminal.
///
/// This performs a **full render** — every visible cell is written.
pub fn render_full<W: Write>(
    out: &mut W,
    buf: &CellBuffer,
    viewport_offset: u16,
    viewport_height: u16,
) -> io::Result<()> {
    // A full repaint is used when the scene identity changes (including
    // back/forward navigation). Reset and clear the physical screen first so
    // cells the new frame intentionally leaves transparent — and continuation
    // columns skipped for wide glyphs — cannot retain pixels from the old
    // page. The new frame is written into the same buffered output and flushed
    // only at the end, so this does not introduce an intermediate blank frame.
    // Set the fallback before erasing: terminals with background-colour erase
    // paint the cleared surface black immediately, avoiding a light flash.
    write!(out, "{RESET}{TERMINAL_DEFAULT_SGR}\x1b[2J")?;

    let start_row = viewport_offset;

    let mut last_style = CellStyle::default();
    let mut needs_reset = true;

    for screen_y in 0..viewport_height {
        let buf_y = start_row + screen_y;

        // Move to start of this screen row
        write_cursor(out, screen_y, 0)?;

        if let Some(row) = buf.row(buf_y) {
            for cell in row {
                // Skip null continuation cells (second half of wide chars)
                if cell.ch == '\0' {
                    continue;
                }

                // Emit style changes
                if cell.style != last_style {
                    if needs_reset {
                        write!(out, "{RESET}")?;
                    }
                    write_style_sgr(out, &cell.style)?;
                    needs_reset = true;
                    last_style = cell.style.clone();
                }

                cell.write_glyph(out)?;
            }
        } else {
            // Content shorter than viewport — clear this row
            if needs_reset {
                write!(out, "{RESET}")?;
                write!(out, "{TERMINAL_DEFAULT_SGR}")?;
                needs_reset = true;
                last_style = CellStyle::default();
            }
            for _ in 0..buf.width {
                write!(out, " ")?;
            }
        }
    }

    // Final reset
    if needs_reset {
        write!(out, "{RESET}")?;
    }

    out.flush()
}

/// Render a buffer at a fixed screen row offset.
///
/// Used for sticky content: renders all rows of `buf` starting at `screen_start`.
pub fn render_at_offset<W: Write>(
    out: &mut W,
    buf: &CellBuffer,
    screen_start: u16,
) -> io::Result<()> {
    let mut last_style = CellStyle::default();
    let mut needs_reset = true;

    write!(out, "{TERMINAL_DEFAULT_SGR}")?;

    for buf_y in 0..buf.height {
        let screen_y = screen_start + buf_y;

        write_cursor(out, screen_y, 0)?;

        if let Some(row) = buf.row(buf_y) {
            for cell in row {
                if cell.ch == '\0' {
                    continue;
                }
                if cell.style != last_style {
                    if needs_reset {
                        write!(out, "{RESET}")?;
                    }
                    write_style_sgr(out, &cell.style)?;
                    needs_reset = true;
                    last_style = cell.style.clone();
                }
                cell.write_glyph(out)?;
            }
        }
    }

    if needs_reset {
        write!(out, "{RESET}")?;
    }

    out.flush()
}

/// Render only the cells that differ between `prev` and `curr`.
///
/// `viewport_offset` is the scroll position.
/// `viewport_height` is the terminal height.
///
/// `dirty` scopes the scan: when `Some`, only cells inside those
/// rects (intersected with the viewport) are compared; when `None`,
/// the full viewport is scanned. Either way the emitted output is
/// identical — `dirty` is a cost optimization, not a correctness
/// constraint. The compositor supplies dirty rects captured from
/// `scene.invalidation.present`; external callers without that
/// information pass `None`.
pub fn render_diff<W: Write>(
    out: &mut W,
    prev: &CellBuffer,
    curr: &CellBuffer,
    viewport_offset: u16,
    viewport_height: u16,
    dirty: Option<&[Rect]>,
) -> io::Result<()> {
    let start_row = viewport_offset;
    let end_row = (viewport_offset + viewport_height).min(curr.height);
    let vx_end = curr.width;

    match dirty {
        None => {
            for buf_y in start_row..end_row {
                emit_row_diff(out, prev, curr, buf_y, 0, vx_end, start_row)?;
            }
        }
        Some(rects) => {
            for r in rects {
                let y0 = r.y.max(start_row);
                let y1 = (r.y.saturating_add(r.h)).min(end_row);
                if y1 <= y0 {
                    continue;
                }
                let x0 = r.x.min(vx_end);
                let x1 = r.x.saturating_add(r.w).min(vx_end);
                if x1 <= x0 {
                    continue;
                }
                for buf_y in y0..y1 {
                    emit_row_diff(out, prev, curr, buf_y, x0, x1, start_row)?;
                }
            }
        }
    }

    out.flush()
}

/// Helper for `render_diff`: emit one row's worth of changed cells
/// within `[x_start, x_end)` of buffer-row `buf_y`. Shared between
/// the full-viewport and dirty-rect code paths so they can't drift.
fn emit_row_diff<W: Write>(
    out: &mut W,
    prev: &CellBuffer,
    curr: &CellBuffer,
    buf_y: u16,
    x_start: u16,
    x_end: u16,
    viewport_offset: u16,
) -> io::Result<()> {
    let screen_y = buf_y - viewport_offset;
    for x in x_start..x_end {
        let curr_cell = match curr.get(x, buf_y) {
            Some(c) => c,
            None => continue,
        };
        if curr_cell.ch == '\0' {
            continue;
        }
        let changed = match prev.get(x, buf_y) {
            Some(p) => p != curr_cell,
            None => true,
        };
        if changed {
            write_cursor(out, screen_y, x)?;
            write!(out, "{RESET}")?;
            write_style_sgr(out, &curr_cell.style)?;
            curr_cell.write_glyph(out)?;
            write!(out, "{RESET}")?;
        }
    }
    Ok(())
}

/// Render a cell buffer to a string (for testing).
///
/// Returns the raw text content without escape sequences.
/// Null continuation cells are replaced with spaces.
pub fn render_to_string(buf: &CellBuffer) -> String {
    let mut output = String::new();

    for y in 0..buf.height {
        if let Some(row) = buf.row(y) {
            for cell in row {
                if cell.ch == '\0' {
                    output.push(' ');
                } else {
                    output.push_str(&cell.glyph());
                }
            }
            // Trim trailing spaces from each row
            while output.ends_with(' ') {
                output.pop();
            }
            output.push('\n');
        }
    }

    // Remove trailing empty lines
    while output.ends_with("\n\n") {
        output.pop();
    }

    output
}

/// Render a cell buffer to a string with ANSI escape sequences (for testing).
pub fn render_to_ansi_string(
    buf: &CellBuffer,
    viewport_offset: u16,
    viewport_height: u16,
) -> String {
    let mut output = Vec::new();
    // `io::Write` for `Vec<u8>` cannot fail, and lossy decoding is identical
    // for the valid case — which cell contents always are, since they are
    // built from `char`s.
    let _ = render_full(&mut output, buf, viewport_offset, viewport_height);
    String::from_utf8_lossy(&output).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::{ColorSupport, NamedColor, ResolvedColor};

    fn make_styled_buf() -> CellBuffer {
        let mut buf = CellBuffer::new(10, 3);
        let red_bold = CellStyle {
            fg: Some(ResolvedColor::Named(NamedColor::Red)),
            bold: true,
            ..Default::default()
        };
        buf.put_str(0, 0, "Hello", &red_bold);
        buf.put_str(0, 1, "World", &CellStyle::default());
        buf
    }

    #[test]
    fn render_to_string_basic() {
        let mut buf = CellBuffer::new(10, 2);
        buf.put_str(0, 0, "Hello", &CellStyle::default());
        buf.put_str(0, 1, "World", &CellStyle::default());

        let s = render_to_string(&buf);
        assert!(s.contains("Hello"));
        assert!(s.contains("World"));
    }

    #[test]
    fn renderers_preserve_complete_grapheme_clusters() {
        let mut buf = CellBuffer::new(12, 1);
        buf.put_str(0, 0, "e\u{301} 👩‍💻", &CellStyle::default());

        assert!(render_to_string(&buf).contains("e\u{301} 👩‍💻"));
        assert!(render_to_ansi_string(&buf, 0, 1).contains("e\u{301} 👩‍💻"));
    }

    #[test]
    fn render_to_string_trims_trailing_spaces() {
        let mut buf = CellBuffer::new(20, 1);
        buf.put_str(0, 0, "Hi", &CellStyle::default());

        let s = render_to_string(&buf);
        let first_line = s.lines().next().unwrap();
        assert_eq!(first_line, "Hi");
    }

    #[test]
    fn render_full_produces_escape_sequences() {
        let buf = make_styled_buf();
        let s = render_to_ansi_string(&buf, 0, 3);

        // Should contain cursor movement
        assert!(s.contains("\x1b["));
        // Should contain red color (SGR 31)
        assert!(s.contains("31"));
        // Should contain bold (SGR 1)
        assert!(s.contains("\x1b[1;31m") || s.contains("1;") || s.contains(";1"));
        // Should contain the text
        assert!(s.contains("Hello"));
        assert!(s.contains("World"));
        // Should contain reset
        assert!(s.contains("\x1b[0m"));
    }

    #[test]
    fn render_full_viewport_offset() {
        let mut buf = CellBuffer::new(10, 5);
        buf.put_str(0, 0, "Line0", &CellStyle::default());
        buf.put_str(0, 1, "Line1", &CellStyle::default());
        buf.put_str(0, 2, "Line2", &CellStyle::default());
        buf.put_str(0, 3, "Line3", &CellStyle::default());
        buf.put_str(0, 4, "Line4", &CellStyle::default());

        // Render with offset=2, height=2 → should show Line2 and Line3
        let s = render_to_ansi_string(&buf, 2, 2);
        assert!(s.contains("Line2"));
        assert!(s.contains("Line3"));
        assert!(!s.contains("Line0"));
        assert!(!s.contains("Line4"));
    }

    #[test]
    fn render_diff_only_changed_cells() {
        let mut prev = CellBuffer::new(10, 2);
        prev.put_str(0, 0, "Hello", &CellStyle::default());
        prev.put_str(0, 1, "World", &CellStyle::default());

        let mut curr = CellBuffer::new(10, 2);
        curr.put_str(0, 0, "Hello", &CellStyle::default()); // same
        curr.put_str(0, 1, "WORLD", &CellStyle::default()); // changed

        let mut output = Vec::new();
        render_diff(&mut output, &prev, &curr, 0, 2, None).unwrap();
        let s = String::from_utf8(output).unwrap();

        // Should contain the changed characters but not "Hello"
        // (Hello is unchanged so should not appear)
        // Actually, "W", "O", "R", "L", "D" should appear since they differ
        // from "W", "o", "r", "l", "d"
        assert!(s.contains('O'));
        assert!(s.contains('R'));
        assert!(s.contains('L'));
        assert!(s.contains('D'));
        // "Hello" is identical — check that H doesn't appear in cursor-positioned output
        // (it might appear as part of cursor sequences, so just check overall length is small)
        assert!(s.len() < 200, "diff output should be small");
    }

    #[test]
    fn render_diff_no_changes() {
        let buf = CellBuffer::new(10, 2);

        let mut output = Vec::new();
        render_diff(&mut output, &buf, &buf, 0, 2, None).unwrap();
        let s = String::from_utf8(output).unwrap();

        // No changes — output should be empty (just flush)
        assert!(s.is_empty());
    }

    #[test]
    fn render_diff_dirty_scope_ignores_changes_outside_rect() {
        // prev and curr differ in two places: col 2 and col 8. If
        // dirty only names cols 0-4, col 8's change must NOT be
        // emitted (the scope narrows the scan, not the correctness —
        // a caller that lies about dirty bounds gets a stale display).
        let mut prev = CellBuffer::new(10, 1);
        prev.put_str(0, 0, "aaaaaaaaaa", &CellStyle::default());
        let mut curr = CellBuffer::new(10, 1);
        curr.put_str(0, 0, "aaXaaaaaYa", &CellStyle::default());

        let dirty = [Rect::new(0, 0, 5, 1)];
        let mut output = Vec::new();
        render_diff(&mut output, &prev, &curr, 0, 1, Some(&dirty)).unwrap();
        let s = String::from_utf8(output).unwrap();

        assert!(s.contains('X'), "in-scope change emitted");
        assert!(!s.contains('Y'), "out-of-scope change skipped");
    }

    #[test]
    fn render_diff_empty_dirty_scope_emits_nothing() {
        let mut prev = CellBuffer::new(10, 1);
        prev.put_str(0, 0, "aaaaa", &CellStyle::default());
        let mut curr = CellBuffer::new(10, 1);
        curr.put_str(0, 0, "bbbbb", &CellStyle::default());

        let mut output = Vec::new();
        render_diff(&mut output, &prev, &curr, 0, 1, Some(&[])).unwrap();
        let s = String::from_utf8(output).unwrap();

        // No rects → nothing scanned → nothing emitted, even though
        // prev and curr differ. Matches the compositor idle-tick case.
        assert!(s.is_empty());
    }

    #[test]
    fn render_full_skips_null_cells() {
        let mut buf = CellBuffer::new(5, 1);
        // Simulate a wide character: 'あ' at col 0, '\0' at col 1
        let style = CellStyle::default();
        buf.put_char(0, 0, 'あ', &style);
        buf.put_char(1, 0, '\0', &style);
        buf.put_char(2, 0, 'B', &style);

        let s = render_to_ansi_string(&buf, 0, 1);
        // Should contain 'あ' and 'B' but not output a visible char for '\0'
        assert!(s.contains('あ'));
        assert!(s.contains('B'));
    }

    #[test]
    fn render_to_string_replaces_null_with_space() {
        let mut buf = CellBuffer::new(5, 1);
        let style = CellStyle::default();
        buf.put_char(0, 0, 'A', &style);
        buf.put_char(1, 0, '\0', &style);
        buf.put_char(2, 0, 'B', &style);

        let s = render_to_string(&buf);
        // Null should be replaced with space
        assert!(s.contains("A B"));
    }

    #[test]
    fn render_full_to_vec() {
        let mut buf = CellBuffer::new(5, 1);
        buf.put_str(0, 0, "Test", &CellStyle::default());

        let mut output = Vec::new();
        render_full(&mut output, &buf, 0, 1).unwrap();

        let s = String::from_utf8(output).unwrap();
        assert!(s.starts_with("\x1b[0m\x1b[38;2;255;255;255;48;2;0;0;0m\x1b[2J"));
        assert!(s.contains("Test"));
    }

    // ─── Integration with layout ─────────────────────────────

    #[test]
    fn end_to_end_render() {
        use crate::compositor::layout::engine::layout_scene;
        use crate::compositor::layout::text::WidthConfig;
        use crate::compositor::scene::build;
        use crate::parser;
        use crate::scanner::Scanner;

        let input = r#"[page mode=document title="Test"]
            [text bold fg=cyan]Hello Dustnet[/text]
            [hr style=dash /]
            [text dim]Goodbye[/text]
        [/page]"#;

        let mut scanner = Scanner::new(input.as_bytes()).unwrap();
        let tokens = scanner.scan_all().unwrap();
        let result = parser::parse(tokens);
        let doc = result.document.unwrap();
        let mut scene = build::from_document(&doc);
        let page_buf = layout_scene(
            &mut scene,
            40,
            10,
            ColorSupport::Truecolor,
            WidthConfig::default(),
        )
        .buffer;
        // Phase 2: text lives in per-node buffers; compose for the
        // user-visible view.
        let anim_rt = crate::compositor::animate::AnimationRuntime::new(Vec::new());
        let buf =
            crate::compositor::composite::walk(&scene, &anim_rt, page_buf.width, page_buf.height);

        // Render to plain text
        let plain = render_to_string(&buf);
        assert!(plain.contains("Hello Dustnet"));
        assert!(plain.contains("Goodbye"));

        // Render to ANSI string. The styled text words appear but may
        // not be contiguous: a ch=' ' cell with no bg is treated as
        // transparent by the cell model (see 04-rendering.md § Cell
        // model), so an unstyled default space fills the gap between
        // "Hello" and "Dustnet". We assert on each word plus an SGR
        // sequence, not the whole contiguous string.
        let ansi = render_to_ansi_string(&buf, 0, 10);
        assert!(ansi.contains("Hello"));
        assert!(ansi.contains("Dustnet"));
        assert!(ansi.contains("Goodbye"));
        assert!(ansi.contains("\x1b["));
    }
}
