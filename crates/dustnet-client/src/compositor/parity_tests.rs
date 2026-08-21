//! Parity harness for the composite-unification and per-node-buffer
//! migrations.
//!
//! Captures both the byte stream and the underlying `CellBuffer`
//! produced by the composite pipeline for a given AML input, and
//! asserts each matches a golden stored under
//! `tests/integration/fixtures/parity/`:
//!
//! - `<name>.ansi` — the ANSI byte stream from `render_full`.
//! - `<name>.grid` — a deterministic serialization of the composited
//!   `CellBuffer` (`(x, y, ch, style)` per non-default cell).
//!
//! **Grid parity is the primary acceptance gate** for the per-node-buffer
//! refactor: byte ordering can shift when previously-transparent gaps
//! become explicit bg cells, but the grid (what the user sees) must not.
//! The byte golden is retained for regression visibility on work that
//! does not change buffer ownership.
//!
//! The harness deliberately targets only the pixels-to-ANSI surface:
//! it calls `composite_pass` + `render_full` directly, skipping the
//! status bar, command line, focus indicator, and sticky compositing
//! that `draw_viewer_frame` wraps around the main buffer. Those are
//! user-facing presentation concerns, not composite concerns, and
//! including them would couple the harness to status-bar format
//! strings and focus state that have nothing to do with what the
//! migration is changing.
//!
//! Regenerate goldens with `UPDATE_GOLDENS=1 cargo test -p dustnet parity`.

use super::*;
use crate::color::ResolvedColor;
use crate::compositor::layout::cell::{Cell, CellBuffer, CellStyle};
use crate::compositor::present::render_full;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn fixtures_dir() -> PathBuf {
    let mut p = crate::repository_root();
    p.push("tests");
    p.push("integration");
    p.push("fixtures");
    p.push("parity");
    p
}

fn parse_aml(src: &str) -> Document {
    let mut scanner = crate::scanner::Scanner::new(src.as_bytes()).unwrap();
    let tokens = scanner.scan_all().unwrap();
    crate::parser::parse(tokens)
        .document
        .expect("AML parse failed in parity harness")
}

/// Load an AML fixture into a `LoadedPage` using default rendering
/// settings. No client, no base URI, no wasm dir — the fixture is
/// self-contained.
async fn build_page(aml: &str, term_w: u16, term_h: u16) -> LoadedPage {
    let doc = parse_aml(aml);
    layout_page(
        doc,
        term_w,
        term_h,
        ColorSupport::Truecolor,
        WidthConfig::default(),
        None,
        None,
        None,
    )
    .await
}

/// A single captured frame — both the composited grid and its ANSI
/// serialization. Grid is the primary parity check; bytes are kept for
/// regression visibility.
struct Frame {
    bytes: Vec<u8>,
    grid: CellBuffer,
}

/// Render a single composited frame, capturing both grid and bytes.
fn render_composite_frame(
    compositor: &mut Compositor,
    page: &mut LoadedPage,
    state: &ViewportState,
) -> Frame {
    let grid_rc = composite_pass(compositor, page, state).unwrap();
    let mut bytes: Vec<u8> = Vec::new();
    render_full(
        &mut bytes,
        &grid_rc,
        state.scroll_offset,
        state.scroll_height(),
    )
    .unwrap();
    let grid = grid_rc.try_clone().unwrap();
    Frame { bytes, grid }
}

/// Capture N animation ticks for an AML fixture, using a fake clock
/// rooted at `Instant::now()`. Returns one `Frame` per tick (including
/// tick 0, the pre-tick static frame). `n_ticks == 0` captures only
/// the static frame.
async fn capture_frames(aml: &str, term_w: u16, term_h: u16, n_ticks: usize) -> Vec<Frame> {
    let mut page = build_page(aml, term_w, term_h).await;
    let mut compositor = Compositor::new(term_w, page.buf.height);
    let state = ViewportState::with_sticky(term_w, term_h, page.buf.height, &page.sticky_buf);

    let mut frames = Vec::with_capacity(n_ticks + 1);
    frames.push(render_composite_frame(&mut compositor, &mut page, &state));

    if n_ticks == 0 {
        return frames;
    }

    // Fake clock: rooted in a real Instant (the runtime's `start_time`
    // field was set from Instant::now() during `from_scene`, so our
    // fake now must be strictly after that).
    let base = Instant::now();
    for i in 0..n_ticks {
        let now = base + Duration::from_millis(33 * (i as u64 + 1));
        let tick_result = page.anim_rt.tick(
            &mut page.scene,
            now,
            state.scroll_offset,
            state.viewport_height(),
        );
        if tick_result.changed {
            PatchApplier::apply_all(&mut page.scene, tick_result.patches);
            page.anim_rt.paint_into_scene(&mut page.scene);
        }
        frames.push(render_composite_frame(&mut compositor, &mut page, &state));
    }
    frames
}

// ─── Grid serialization ─────────────────────────────────────
//
// The grid golden is a deterministic text dump of a `CellBuffer`.
// Format:
//
//     grid WxH
//     <per-cell records, sorted by (y, x), only for non-default cells>
//
// A "non-default" cell is one whose `Cell::empty()` equivalence fails
// — i.e., any cell with a non-space char or any non-default style.
// Every absent `(x, y)` record implicitly asserts an empty cell.
//
// Record format (one per line):
//
//     x,y 'ch' [fg=...] [bg=...] [flag ...]
//
// - `ch` uses Rust's `char` debug form, so control chars and quotes
//   stay unambiguous.
// - `fg`/`bg` are `n:<name>`, `p:<idx>`, or `r:<rrggbb>`, matching the
//   three `ResolvedColor` variants. Absent when `None`.
// - Flags are the subset of `{bold, italic, underline, strike, dim,
//   blink}` that are true, space-separated.

fn serialize_color(c: ResolvedColor) -> String {
    match c {
        ResolvedColor::Named(n) => format!("n:{:?}", n),
        ResolvedColor::Palette(i) => format!("p:{}", i),
        ResolvedColor::Rgb(r, g, b) => format!("r:{:02x}{:02x}{:02x}", r, g, b),
    }
}

fn cell_is_default(cell: &Cell) -> bool {
    cell.ch == ' ' && cell.style == CellStyle::default()
}

fn serialize_cell_record(x: u16, y: u16, cell: &Cell) -> String {
    let mut s = format!("{},{} {:?}", x, y, cell.ch);
    if let Some(fg) = cell.style.fg {
        write!(&mut s, " fg={}", serialize_color(fg)).unwrap();
    }
    if let Some(bg) = cell.style.bg {
        write!(&mut s, " bg={}", serialize_color(bg)).unwrap();
    }
    for (flag, tag) in [
        (cell.style.bold, "bold"),
        (cell.style.italic, "italic"),
        (cell.style.underline, "underline"),
        (cell.style.strikethrough, "strike"),
        (cell.style.dim, "dim"),
        (cell.style.blink, "blink"),
    ] {
        if flag {
            s.push(' ');
            s.push_str(tag);
        }
    }
    s
}

fn serialize_grid(buf: &CellBuffer) -> String {
    let mut out = String::new();
    writeln!(&mut out, "grid {}x{}", buf.width, buf.height).unwrap();
    for y in 0..buf.height {
        for x in 0..buf.width {
            let cell = buf.get(x, y).expect("in-bounds cell");
            if cell_is_default(cell) {
                continue;
            }
            out.push_str(&serialize_cell_record(x, y, cell));
            out.push('\n');
        }
    }
    out
}

// ─── Golden comparison ──────────────────────────────────────

fn update_goldens() -> bool {
    std::env::var("UPDATE_GOLDENS")
        .map(|v| v == "1")
        .unwrap_or(false)
}

/// Compare `actual` bytes to a golden file at `<fixtures>/<name>.ansi`.
fn assert_byte_golden(actual: &[u8], name: &str) {
    let dir = fixtures_dir();
    std::fs::create_dir_all(&dir).expect("create parity fixtures dir");
    let path = dir.join(format!("{name}.ansi"));

    if update_goldens() {
        std::fs::write(&path, actual).expect("write byte golden");
        return;
    }

    let expected = std::fs::read(&path).unwrap_or_else(|_| {
        panic!(
            "byte golden missing: {}\nrun `UPDATE_GOLDENS=1 cargo test` to create",
            path.display()
        )
    });
    if actual != expected.as_slice() {
        panic!(
            "byte parity mismatch for {name}: {} vs {} bytes\n(rerun with UPDATE_GOLDENS=1 to regenerate, only after auditing the diff)",
            actual.len(),
            expected.len(),
        );
    }
}

/// Compare the composited `CellBuffer` to a grid golden at
/// `<fixtures>/<name>.grid`. This is the primary acceptance check for
/// the per-node-buffer refactor.
fn assert_grid_golden(actual: &CellBuffer, name: &str) {
    let dir = fixtures_dir();
    std::fs::create_dir_all(&dir).expect("create parity fixtures dir");
    let path = dir.join(format!("{name}.grid"));

    let serialized = serialize_grid(actual);

    if update_goldens() {
        std::fs::write(&path, &serialized).expect("write grid golden");
        return;
    }

    let expected = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!(
            "grid golden missing: {}\nrun `UPDATE_GOLDENS=1 cargo test` to create",
            path.display()
        )
    });
    if serialized != expected {
        // Produce a minimal first-difference hint so the diff isn't a
        // 400-line wall. The full diff is available via a normal
        // file-vs-stdout comparison once goldens are regenerated.
        let diff_line = serialized
            .lines()
            .zip(expected.lines())
            .enumerate()
            .find(|(_, (a, b))| a != b)
            .map(|(i, (a, b))| format!("line {i}: actual=`{a}` expected=`{b}`"))
            .unwrap_or_else(|| {
                format!(
                    "length differs: actual={} expected={}",
                    serialized.lines().count(),
                    expected.lines().count()
                )
            });
        panic!(
            "grid parity mismatch for {name}: {}\n(rerun with UPDATE_GOLDENS=1 to regenerate, only after auditing the diff)",
            diff_line
        );
    }
}

/// Assert both grid and byte goldens for a single frame. Grid comes
/// first because it's the primary gate; a grid mismatch means a real
/// visual regression and the byte comparison is redundant information.
fn assert_frame_golden(frame: &Frame, name: &str) {
    assert_grid_golden(&frame.grid, name);
    assert_byte_golden(&frame.bytes, name);
}

// ─── Fixtures ────────────────────────────────────────────────

const AML_STATIC_TEXT: &str = r#"[page mode=document]
[text]Hello, world[/text]
[box w=20 border=single]
  [text]Inside a box[/text]
[/box]
[/page]"#;

const AML_PANEL: &str = r#"[page mode=screen cols=40 rows=10]
[panel id="p" state="a"]
  [state name="a" x=0 y=0 w=40 h=5][text]State A[/text][/state]
  [state name="b" x=0 y=0 w=40 h=5][text]State B[/text][/state]
[/panel]
[/page]"#;

const AML_LAYERS: &str = r#"[page mode=screen cols=40 rows=10]
[box x=0 y=0 w=40 h=10 bg=blue][text]background[/text][/box]
[box x=5 y=2 w=20 h=4 border=single bg=black][text]foreground[/text][/box]
[/page]"#;

// Absolute box overlapping flow content, both sides stylized. This is
// the exact shape that the per-node-buffer migration flags as the
// paint-order inversion trigger (`parity_layers` is the predecessor
// fixture that first surfaced the bug).
const AML_ABSOLUTE_OVER_FLOW: &str = r#"[page mode=screen cols=40 rows=10]
[text fg=yellow]aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa[/text]
[text fg=yellow]bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb[/text]
[text fg=yellow]cccccccccccccccccccccccccccccccccccccccc[/text]
[box x=10 y=1 w=15 h=3 border=single bg=red fg=white][text]OVERLAY[/text][/box]
[/page]"#;

// Inline link inside a wrapped text block — exercises the focusable-
// rect translation risk called out in the per-node-buffer migration
// ("Focusable rect translation bugs" under Risks).
const AML_INLINE_LINK_IN_TEXT: &str = r#"[page mode=screen cols=40 rows=6]
[text]Click [link href="atp://example.com"]here[/link] to navigate, then read on.[/text]
[/page]"#;

// Table with per-cell content and header/body distinction. Tables are
// the trickiest kind per the plan (Phase 5) because rows and cells
// compose via nested flow.
const AML_TABLE_CELLS: &str = r#"[page mode=screen cols=40 rows=8]
[table]
  [thead]
    [tr][th]Name[/th][th]Qty[/th][/tr]
  [/thead]
  [tbody]
    [tr][td]apple[/td][td]3[/td][/tr]
    [tr][td]pear[/td][td]12[/td][/tr]
  [/tbody]
[/table]
[/page]"#;

// Sticky region above non-sticky flow content, in document mode so
// that scroll offset is meaningful. The parity harness does not scroll,
// but the sticky region still composes into the separate `sticky_buf`
// that `composite_pass` does not consult — so this fixture only
// exercises the main-buffer scroll-then-paint flow. Adequate as a
// smoke test; full sticky coverage would need `draw_viewer_frame`,
// which is intentionally excluded (see module doc).
const AML_STICKY_WITH_SCROLL: &str = r#"[page mode=document]
[box sticky=top w=40 bg=blue fg=white][text]STICKY HEADER[/text][/box]
[text]row 1[/text]
[text]row 2[/text]
[text]row 3[/text]
[text]row 4[/text]
[/page]"#;

// Panel containing another panel with distinct background and states.
// Tests that nested state resolution doesn't confuse the flow placement.
const AML_NESTED_PANELS: &str = r#"[page mode=screen cols=40 rows=10]
[panel id="outer" state="x"]
  [state name="x" x=0 y=0 w=40 h=10]
    [box w=40 bg=blue fg=white][text]outer X[/text][/box]
    [panel id="inner" state="p"]
      [state name="p" x=0 y=3 w=20 h=4][text]inner P[/text][/state]
      [state name="q" x=0 y=3 w=20 h=4][text]inner Q[/text][/state]
    [/panel]
  [/state]
  [state name="y" x=0 y=0 w=40 h=10][text]outer Y[/text][/state]
[/panel]
[/page]"#;

// Live region without a client — the region gets laid out and
// allocated its buffer but never subscribes (no network), so the
// captured frame shows its initial empty state. That's the case we
// need grid parity on across the refactor; the per-node-buffer
// migration does not change live-region subscription behavior.
const AML_LIVE_REGION: &str = r#"[page mode=screen cols=40 rows=6]
[text]before[/text]
[live src="atp://example.com/events" h=3][/live]
[text]after[/text]
[/page]"#;

// ─── Tests ───────────────────────────────────────────────────

#[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
#[tokio::test]
async fn parity_static_text() {
    let frames = capture_frames(AML_STATIC_TEXT, 40, 10, 0).await;
    assert_eq!(frames.len(), 1);
    assert_frame_golden(&frames[0], "static_text");
}

#[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
#[tokio::test]
async fn parity_panel_static() {
    let frames = capture_frames(AML_PANEL, 40, 10, 0).await;
    assert_eq!(frames.len(), 1);
    assert_frame_golden(&frames[0], "panel_static");
}

#[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
#[tokio::test]
async fn parity_layers() {
    let frames = capture_frames(AML_LAYERS, 40, 10, 0).await;
    assert_eq!(frames.len(), 1);
    assert_frame_golden(&frames[0], "layers");
}

#[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
#[tokio::test]
async fn parity_absolute_over_flow() {
    let frames = capture_frames(AML_ABSOLUTE_OVER_FLOW, 40, 10, 0).await;
    assert_eq!(frames.len(), 1);
    assert_frame_golden(&frames[0], "absolute_over_flow");
}

#[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
#[tokio::test]
async fn parity_inline_link_in_text() {
    let frames = capture_frames(AML_INLINE_LINK_IN_TEXT, 40, 6, 0).await;
    assert_eq!(frames.len(), 1);
    assert_frame_golden(&frames[0], "inline_link_in_text");
}

#[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
#[tokio::test]
async fn parity_table_cells() {
    let frames = capture_frames(AML_TABLE_CELLS, 40, 8, 0).await;
    assert_eq!(frames.len(), 1);
    assert_frame_golden(&frames[0], "table_cells");
}

#[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
#[tokio::test]
async fn parity_sticky_with_scroll() {
    let frames = capture_frames(AML_STICKY_WITH_SCROLL, 40, 8, 0).await;
    assert_eq!(frames.len(), 1);
    assert_frame_golden(&frames[0], "sticky_with_scroll");
}

#[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
#[tokio::test]
async fn parity_nested_panels() {
    let frames = capture_frames(AML_NESTED_PANELS, 40, 10, 0).await;
    assert_eq!(frames.len(), 1);
    assert_frame_golden(&frames[0], "nested_panels");
}

#[cfg_attr(miri, ignore = "tokio runtime needs kqueue, which Miri cannot emulate")]
#[tokio::test]
async fn parity_live_region() {
    let frames = capture_frames(AML_LIVE_REGION, 40, 6, 0).await;
    assert_eq!(frames.len(), 1);
    assert_frame_golden(&frames[0], "live_region");
}

// ─── Grid-serializer unit tests ──────────────────────────────

#[test]
fn grid_serializer_skips_default_cells() {
    use crate::color::NamedColor;

    let mut buf = CellBuffer::new(4, 2);
    buf.put_char(
        1,
        0,
        'X',
        &CellStyle {
            fg: Some(ResolvedColor::Named(NamedColor::Red)),
            bold: true,
            ..Default::default()
        },
    );
    let out = serialize_grid(&buf);
    // Header + single record + trailing newline.
    assert_eq!(out, "grid 4x2\n1,0 'X' fg=n:Red bold\n");
}

#[test]
fn grid_serializer_is_deterministic() {
    let mut buf = CellBuffer::new(3, 3);
    buf.put_char(2, 1, 'b', &CellStyle::default());
    buf.put_char(0, 2, 'a', &CellStyle::default());
    let s1 = serialize_grid(&buf);
    let s2 = serialize_grid(&buf);
    assert_eq!(s1, s2);
    // Sorted by (y, x): (2,1) comes before (0,2).
    let idx_b = s1.find("2,1 'b'").unwrap();
    let idx_a = s1.find("0,2 'a'").unwrap();
    assert!(idx_b < idx_a, "records must be sorted by (y,x) row-major");
}
