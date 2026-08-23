//! Panel transitions as `Animation` implementations.
//!
//! A trait-based adapter that keeps both old and new state buffers
//! live and writes the blended result into the scene node's buffer
//! per tick.
//!
//! ## Supported kinds
//!
//! - `Cut`    — scheduled as instantaneous; not modeled as a
//!   `TransitionAdapter` at all (the caller skips scheduling).
//! - `Fade`   — old cells vanish into black, hold, new cells appear.
//! - `Slide*` — boundary sweeps across the region; one layer on each
//!   side.
//! - `DrawDown` — traces the top edge, then reveals complete rows downward.
//! - `DrawRight` — traces the left edge, then reveals complete columns rightward.
//! - `DrawLeft` — the mirror: traces the right edge, then reveals columns leftward.
//! - `DrawOut` — grows a live rectangular frame from the panel centre.
//! - `Dissolve` — per-cell stochastic mask picks old or new.
//!
//! ## Shape-mismatched states
//!
//! When the old and new state buffers differ in position or size,
//! `TransitionAdapter` composes them over the **union** rect: cells
//! exclusive to A source from A, cells exclusive to B source from B,
//! overlapping cells follow the per-kind rule. "State A occupies
//! 10×5 at the top left; state B occupies 3×3 at the bottom right —
//! dissolve, wipe, slide all work."

use crate::compositor::layout::Rect;
use crate::compositor::layout::cell::{Cell, CellBuffer};
use crate::compositor::scene::{NodeId, Scene};
use crate::parser::ast::TransitionKind;

use super::runtime::AnimState;
use super::{AdvanceCtx, AdvanceResult, Animation};

/// Transition tick cadence (~30fps).
pub const TICK_MS: u32 = 33;

/// A transition adapter drives a per-cell blend between two buffers
/// over `duration_ms` into a target scene node's buffer.
pub struct TransitionAdapter {
    id: String,
    target_node: NodeId,
    /// Region the target panel occupies in scene-screen coordinates.
    /// The adapter's output paints into the target node's buffer at
    /// (0, 0); the region is used only for shape-mismatch math.
    target_region: Rect,
    /// Old state's buffer + its rect (in scene-screen coords).
    old_buf: CellBuffer,
    old_rect: Rect,
    /// New state's buffer + its rect (in scene-screen coords).
    new_buf: CellBuffer,
    new_rect: Rect,
    kind: TransitionKind,
    duration_ms: u32,
    elapsed_ms: u32,
    state: AnimState,
    /// Seeded splitmix64 for deterministic dissolve masks. Seeded from
    /// target_region + duration so the same transition looks identical
    /// every time (important for snapshot tests).
    seed: u64,
    /// Density-adaptive easing exponent for dissolve. Computed from the
    /// count of opaque cells in `new_buf`; higher counts get a steeper
    /// curve so the absolute number of cells revealed at the start of
    /// the transition stays small regardless of total content density.
    /// See `compute_dissolve_exponent`. Cached at construction so we
    /// don't redo the count + log per cell per tick.
    dissolve_exponent: f32,
}

/// Choose a power-curve exponent for the dissolve threshold so the
/// "first cell" appears at roughly the same fractional t regardless
/// of how many cells the panel covers.
///
/// Uses `width * height` (the box's full cell capacity) as the
/// density signal — cheap, no buffer scan, and the user-facing
/// character count is bounded by it. Solving `t_first ^ p ≈ 1 / N`
/// for `t_first = 0.1` yields `p = log10(N)`. Clamped to `[2.0, 6.0]`
/// so empty/sparse rects still get a perceptible ease-in and very
/// dense ones don't degenerate into a hard cut at midpoint.
fn compute_dissolve_exponent(rect: Rect) -> f32 {
    let cells = (rect.w as u32).saturating_mul(rect.h as u32);
    if cells == 0 {
        return 2.0;
    }
    (cells as f32).log10().clamp(2.0, 6.0)
}

impl TransitionAdapter {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: String,
        target_node: NodeId,
        target_region: Rect,
        old_buf: CellBuffer,
        old_rect: Rect,
        new_buf: CellBuffer,
        new_rect: Rect,
        kind: TransitionKind,
        duration_ms: u32,
    ) -> Self {
        // Derive a seed from the target region and duration. Deterministic
        // per transition instance; two identical transitions produce the
        // same mask. Not cryptographically strong — just reproducible.
        let seed = ((target_region.x as u64) << 48)
            ^ ((target_region.y as u64) << 32)
            ^ ((target_region.w as u64) << 16)
            ^ (duration_ms as u64);
        let dissolve_exponent = compute_dissolve_exponent(new_rect);
        Self {
            id,
            target_node,
            target_region,
            old_buf,
            old_rect,
            new_buf,
            new_rect,
            kind,
            duration_ms,
            elapsed_ms: 0,
            state: if matches!(kind, TransitionKind::Cut) || duration_ms == 0 {
                AnimState::Finished
            } else {
                AnimState::Running
            },
            seed,
            dissolve_exponent,
        }
    }

    /// The scene node this adapter paints into. Exposed so the
    /// composite walk can suppress the panel's children's chrome
    /// during transitions (the panel buffer is then the sole source
    /// of visible cells in the panel's region).
    pub fn target_node(&self) -> NodeId {
        self.target_node
    }

    /// Progress ∈ [0, 1].
    pub fn t(&self) -> f32 {
        if self.duration_ms == 0 {
            1.0
        } else {
            (self.elapsed_ms as f32 / self.duration_ms as f32).clamp(0.0, 1.0)
        }
    }

    fn cell_from_old(&self, screen_x: u16, screen_y: u16) -> Option<Cell> {
        let local_x = screen_x.checked_sub(self.old_rect.x)?;
        let local_y = screen_y.checked_sub(self.old_rect.y)?;
        if local_x >= self.old_rect.w || local_y >= self.old_rect.h {
            return None;
        }
        self.old_buf.get(local_x, local_y).cloned()
    }

    fn cell_from_new(&self, screen_x: u16, screen_y: u16) -> Option<Cell> {
        let local_x = screen_x.checked_sub(self.new_rect.x)?;
        let local_y = screen_y.checked_sub(self.new_rect.y)?;
        if local_x >= self.new_rect.w || local_y >= self.new_rect.h {
            return None;
        }
        self.new_buf.get(local_x, local_y).cloned()
    }

    /// Grow a temporary version of the new frame from its centre. Border
    /// cells are sampled from the corresponding edge of the completed frame,
    /// so the viewer sees a real rectangle expanding in both axes instead of
    /// an interior crop whose border appears only on the final tick.
    fn draw_out_cell(&self, screen_x: u16, screen_y: u16, old: Option<Cell>) -> Option<Cell> {
        let local_x = screen_x.checked_sub(self.new_rect.x)?;
        let local_y = screen_y.checked_sub(self.new_rect.y)?;
        let w = self.new_rect.w;
        let h = self.new_rect.h;
        if w == 0 || h == 0 || local_x >= w || local_y >= h {
            return old;
        }
        if self.t() >= 1.0 {
            return self.cell_from_new(screen_x, screen_y);
        }

        let revealed_w = ((self.t() * w as f32).ceil() as u16).clamp(1, w);
        let revealed_h = ((self.t() * h as f32).ceil() as u16).clamp(1, h);
        let start_x = w.saturating_sub(revealed_w) / 2;
        let start_y = h.saturating_sub(revealed_h) / 2;
        let end_x = start_x.saturating_add(revealed_w);
        let end_y = start_y.saturating_add(revealed_h);

        if local_x < start_x || local_x >= end_x || local_y < start_y || local_y >= end_y {
            return old;
        }

        let source_x = if local_x == start_x {
            0
        } else if local_x + 1 == end_x {
            w - 1
        } else {
            local_x
        };
        let source_y = if local_y == start_y {
            0
        } else if local_y + 1 == end_y {
            h - 1
        } else {
            local_y
        };
        self.new_buf.get(source_x, source_y).cloned()
    }

    /// Deterministic splitmix64 cell-threshold for dissolve/fade masks.
    /// The threshold is stable per-cell within a transition instance
    /// (derived from cell coords + seed), so the mask is reproducible.
    fn cell_threshold(&self, screen_x: u16, screen_y: u16, salt: u64) -> f32 {
        let mut x = self.seed ^ ((screen_x as u64) << 16) ^ (screen_y as u64) ^ salt;
        x = (x ^ (x >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94D049BB133111EB);
        x ^= x >> 31;
        ((x % 1000) as f32) / 1000.0
    }

    /// Compose one blended cell at `(screen_x, screen_y)` given the
    /// current progress `t`. Returns `None` when neither old nor new
    /// contributes at this position — the caller leaves the destination
    /// cell untouched (reveals the base layer).
    ///
    /// **Shape-mismatch handling**: cells inside one rect but outside
    /// the other are sourced from the rect that contains them — they
    /// don't get gated on the transition threshold, since the only
    /// "alternative" is "empty," which would just create flicker. Cells
    /// inside *both* rects get the threshold treatment.
    pub fn blend_cell(&self, screen_x: u16, screen_y: u16) -> Option<Cell> {
        let t = self.t();
        let in_old = self.old_rect.contains_point(screen_x, screen_y);
        let in_new = self.new_rect.contains_point(screen_x, screen_y);
        let old = self.cell_from_old(screen_x, screen_y);
        let new = self
            .cell_from_new(screen_x, screen_y)
            .map(|cell| self.mask_unfurl_title(screen_x, screen_y, cell));

        // Shape-mismatch fast paths: cell is only in one rect, so the
        // other state has nothing to say about it.
        if in_old && !in_new {
            return old;
        }
        if in_new && !in_old {
            return new;
        }
        if !in_old && !in_new {
            return None;
        }

        match self.kind {
            TransitionKind::Cut => new.or(old),
            TransitionKind::Fade => {
                let threshold = self.cell_threshold(screen_x, screen_y, 0xFADE);
                if (0.4..=0.6).contains(&t) {
                    // Hold black / base — neither layer wins.
                    None
                } else if t < 0.4 {
                    let progress = t / 0.4;
                    if progress <= threshold { old } else { None }
                } else {
                    let progress = (t - 0.6) / 0.4;
                    if progress > threshold { new } else { None }
                }
            }
            TransitionKind::Dissolve => {
                // Density-adaptive ease-in-out. The exponent `p` was
                // computed from `new_rect`'s w*h (`compute_dissolve_exponent`)
                // so a denser reveal gets a steeper curve — the absolute
                // count of cells visible at any fraction of t stays in a
                // perceptual band regardless of total content density.
                //
                // Standard ease-in-out generalized to power `p`:
                //   t<0.5: f = 2^(p-1) * t^p
                //   t>=0.5: f = 1 - 2^(p-1) * (1-t)^p
                // For p=2 this collapses to the classic 2t² / 1-2(1-t)²;
                // higher p compresses the slow-start phase further.
                //
                // Returns `new` or `old` directly (no `.or()` fallback)
                // so a transparent cell on either side dissolves cleanly
                // through transparency rather than holding the opposite
                // side opaquely — critical for empty→content transitions
                // where the old state is intentionally invisible.
                let p = self.dissolve_exponent;
                let scale = 2.0_f32.powf(p - 1.0);
                let eased_t = if t < 0.5 {
                    scale * t.powf(p)
                } else {
                    1.0 - scale * (1.0 - t).powf(p)
                };
                let threshold = self.cell_threshold(screen_x, screen_y, 0xD155);
                if eased_t > threshold { new } else { old }
            }
            TransitionKind::DrawDown => {
                // Grow the top edge outward from its centre, then unfurl
                // rows from top to bottom. The last revealed row samples the
                // authored bottom edge, so the growing box remains visibly
                // closed throughout the animation.
                const TOP_EDGE_PHASE: f32 = 0.50;
                let local_x = screen_x.saturating_sub(self.target_region.x);
                let local_y = screen_y.saturating_sub(self.target_region.y);
                if t < TOP_EDGE_PHASE {
                    let progress = t / TOP_EDGE_PHASE;
                    let revealed_columns = (progress * self.target_region.w as f32).ceil() as u16;
                    let start = self.target_region.w.saturating_sub(revealed_columns) / 2;
                    let end = start.saturating_add(revealed_columns);
                    if local_y == 0 && local_x >= start && local_x < end {
                        new
                    } else {
                        old
                    }
                } else {
                    let progress = (t - TOP_EDGE_PHASE) / (1.0 - TOP_EDGE_PHASE);
                    let remaining_rows = self.target_region.h.saturating_sub(1);
                    let revealed_rows = 1 + (progress * remaining_rows as f32).ceil() as u16;
                    if local_y >= revealed_rows {
                        old
                    } else if revealed_rows > 1
                        && revealed_rows < self.target_region.h
                        && local_y + 1 == revealed_rows
                    {
                        let new_x = screen_x.saturating_sub(self.new_rect.x);
                        self.new_buf
                            .get(new_x, self.new_buf.height.saturating_sub(1))
                            .cloned()
                    } else {
                        new
                    }
                }
            }
            TransitionKind::DrawRight => {
                // Rotated counterpart to DrawDown: construct the left edge
                // top-to-bottom, then unfurl columns left-to-right. The last
                // revealed column samples the authored right edge so the box
                // remains visibly closed while it grows.
                const LEFT_EDGE_PHASE: f32 = 0.50;
                let local_x = screen_x.saturating_sub(self.target_region.x);
                let local_y = screen_y.saturating_sub(self.target_region.y);
                if t < LEFT_EDGE_PHASE {
                    let progress = t / LEFT_EDGE_PHASE;
                    let revealed_rows = (progress * self.target_region.h as f32).ceil() as u16;
                    if local_x == 0 && local_y < revealed_rows {
                        new
                    } else {
                        old
                    }
                } else {
                    let progress = (t - LEFT_EDGE_PHASE) / (1.0 - LEFT_EDGE_PHASE);
                    let remaining_columns = self.target_region.w.saturating_sub(1);
                    let revealed_columns = 1 + (progress * remaining_columns as f32).ceil() as u16;
                    if local_x >= revealed_columns {
                        old
                    } else if revealed_columns > 1
                        && revealed_columns < self.target_region.w
                        && local_x + 1 == revealed_columns
                    {
                        let new_y = screen_y.saturating_sub(self.new_rect.y);
                        self.new_buf
                            .get(self.new_buf.width.saturating_sub(1), new_y)
                            .cloned()
                    } else {
                        new
                    }
                }
            }
            TransitionKind::DrawLeft => {
                // Mirror of DrawRight: construct the right edge top-to-bottom,
                // then unfurl columns right-to-left. The frontier column samples
                // the authored left edge so the box stays visibly closed while it
                // grows -- the same trick DrawRight plays with the right edge,
                // reflected.
                const RIGHT_EDGE_PHASE: f32 = 0.50;
                let local_x = screen_x.saturating_sub(self.target_region.x);
                let local_y = screen_y.saturating_sub(self.target_region.y);
                let last_x = self.target_region.w.saturating_sub(1);
                if t < RIGHT_EDGE_PHASE {
                    let progress = t / RIGHT_EDGE_PHASE;
                    let revealed_rows = (progress * self.target_region.h as f32).ceil() as u16;
                    if local_x == last_x && local_y < revealed_rows {
                        new
                    } else {
                        old
                    }
                } else {
                    let progress = (t - RIGHT_EDGE_PHASE) / (1.0 - RIGHT_EDGE_PHASE);
                    let remaining_columns = self.target_region.w.saturating_sub(1);
                    let revealed_columns = 1 + (progress * remaining_columns as f32).ceil() as u16;
                    // Counted inward from the right edge, so the frontier is a
                    // column index measured backwards.
                    let frontier = last_x.saturating_sub(revealed_columns.saturating_sub(1));
                    if local_x < frontier {
                        old
                    } else if revealed_columns > 1
                        && revealed_columns < self.target_region.w
                        && local_x == frontier
                    {
                        let new_y = screen_y.saturating_sub(self.new_rect.y);
                        self.new_buf.get(0, new_y).cloned()
                    } else {
                        new
                    }
                }
            }
            TransitionKind::DrawOut => self.draw_out_cell(screen_x, screen_y, old),
            TransitionKind::SlideLeft
            | TransitionKind::SlideRight
            | TransitionKind::SlideUp
            | TransitionKind::SlideDown => {
                // Compute a moving boundary across the union region and
                // decide per cell which side it falls on.
                let target = self.target_region;
                let offset_x = (t * target.w as f32) as u16;
                let offset_y = (t * target.h as f32) as u16;
                let rel_x = screen_x.saturating_sub(target.x);
                let rel_y = screen_y.saturating_sub(target.y);
                let boundary_crossed = match self.kind {
                    TransitionKind::SlideLeft => rel_x + offset_x >= target.w,
                    TransitionKind::SlideRight => rel_x < offset_x,
                    TransitionKind::SlideUp => rel_y + offset_y >= target.h,
                    TransitionKind::SlideDown => rel_y < offset_y,
                    _ => unreachable!(),
                };
                if boundary_crossed {
                    new.or(old)
                } else {
                    old.or(new)
                }
            }
        }
    }

    /// During draw-style unfurls, render a box heading as uninterrupted top
    /// border until the terminal frame. This lets the chrome construct first;
    /// the authored title then appears atomically at t=1.
    fn mask_unfurl_title(&self, screen_x: u16, screen_y: u16, mut cell: Cell) -> Cell {
        if self.t() >= 1.0
            || !matches!(
                self.kind,
                TransitionKind::DrawDown
                    | TransitionKind::DrawRight
                    | TransitionKind::DrawLeft
                    | TransitionKind::DrawOut
            )
            || screen_y != self.new_rect.y
        {
            return cell;
        }

        let Some(local_x) = screen_x.checked_sub(self.new_rect.x) else {
            return cell;
        };
        if local_x == 0 || local_x + 1 >= self.new_buf.width {
            return cell;
        }

        let Some(left) = self.new_buf.get(0, 0) else {
            return cell;
        };
        let Some(right) = self.new_buf.get(self.new_buf.width.saturating_sub(1), 0) else {
            return cell;
        };
        let stroke = match (left.ch, right.ch) {
            ('╭' | '┌', '╮' | '┐') => '─',
            ('╔', '╗') => '═',
            ('┏', '┓') => '━',
            ('+', '+') => '-',
            _ => return cell,
        };

        if cell.ch != stroke {
            cell.ch = stroke;
            cell.grapheme = None;
            cell.style = left.style.clone();
        }
        cell
    }

    /// Paint the current transition frame into the scene node's buffer.
    /// Called by the runtime via `Animation::paint` after each `advance`
    /// that returns `wrote_buffer`.
    pub fn paint_into(&self, scene: &mut Scene) -> Option<NodeId> {
        let target = self.target_region;
        // Try each kind-gated accessor in turn. We can't chain `or_else`
        // because each call borrows `scene` mutably; take turns via early
        // returns.
        let dst = if scene.panel_buffer_mut(self.target_node).is_some() {
            scene.panel_buffer_mut(self.target_node)?
        } else if scene.wasm_buffer_mut(self.target_node).is_some() {
            scene.wasm_buffer_mut(self.target_node)?
        } else if scene.live_buffer_mut(self.target_node).is_some() {
            scene.live_buffer_mut(self.target_node)?
        } else {
            return None;
        };

        // Iterate over the target region in screen coords but write to
        // the destination buffer in its local coords.
        for dy in 0..dst.height.min(target.h) {
            for dx in 0..dst.width.min(target.w) {
                let sx = target.x.saturating_add(dx);
                let sy = target.y.saturating_add(dy);
                if let Some(cell) = self.blend_cell(sx, sy) {
                    dst.set(dx, dy, cell);
                } else {
                    // Neither layer contributes — write a transparent
                    // empty cell so the base layer shows through when
                    // this buffer is composited.
                    dst.set(dx, dy, Cell::empty());
                }
            }
        }
        Some(self.target_node)
    }
}

impl Animation for TransitionAdapter {
    fn id(&self) -> &str {
        &self.id
    }

    fn advance(&mut self, _ctx: &mut AdvanceCtx) -> AdvanceResult {
        if matches!(self.state, AnimState::Finished) {
            return AdvanceResult::none();
        }
        self.elapsed_ms += TICK_MS;
        if self.elapsed_ms >= self.duration_ms {
            self.state = AnimState::Finished;
        }
        AdvanceResult::with_buffer(self.target_node)
    }

    fn finished(&self) -> bool {
        matches!(self.state, AnimState::Finished)
    }

    fn state(&self) -> AnimState {
        self.state
    }

    fn paint(&self, scene: &mut Scene) {
        self.paint_into(scene);
    }

    fn trigger_stop(&mut self) {
        self.state = AnimState::Finished;
        self.elapsed_ms = self.duration_ms;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compositor::layout::cell::CellStyle;
    use crate::compositor::scene;
    use std::time::Instant;

    fn fill(w: u16, h: u16, ch: char) -> CellBuffer {
        let mut buf = CellBuffer::new(w, h);
        for y in 0..h {
            for x in 0..w {
                buf.put_char(x, y, ch, &CellStyle::default());
            }
        }
        buf
    }

    fn panel_scene(w: u16, h: u16) -> (Scene, NodeId) {
        let doc = {
            let src = r#"[page mode=document]
                [panel id="p" state="a"]
                    [state name="a"][text]A[/text][/state]
                    [state name="b"][text]B[/text][/state]
                [/panel]
            [/page]"#;
            let mut scanner = crate::scanner::Scanner::new(src.as_bytes()).unwrap();
            let tokens = scanner.scan_all().unwrap();
            crate::parser::parse(tokens).document.unwrap()
        };
        let mut scene = scene::build::from_document(&doc);
        let panel = scene.find_by_aml_id("p").unwrap();
        scene.allocate_buffer(panel, w, h);
        (scene, panel)
    }

    #[test]
    fn cut_is_scheduled_as_finished() {
        let (_scene, node) = panel_scene(5, 2);
        let rect = Rect::new(0, 0, 5, 2);
        let t = TransitionAdapter::new(
            "cut".into(),
            node,
            rect,
            fill(5, 2, 'A'),
            rect,
            fill(5, 2, 'B'),
            rect,
            TransitionKind::Cut,
            0,
        );
        assert!(t.finished(), "Cut transitions should be born finished");
    }

    #[test]
    fn zero_duration_finishes_immediately() {
        let (_scene, node) = panel_scene(5, 2);
        let rect = Rect::new(0, 0, 5, 2);
        let t = TransitionAdapter::new(
            "d".into(),
            node,
            rect,
            fill(5, 2, 'A'),
            rect,
            fill(5, 2, 'B'),
            rect,
            TransitionKind::Dissolve,
            0,
        );
        assert!(t.finished());
    }

    #[test]
    fn dissolve_is_deterministic() {
        let (_scene, node) = panel_scene(5, 2);
        let rect = Rect::new(0, 0, 5, 2);
        let a = TransitionAdapter::new(
            "d".into(),
            node,
            rect,
            fill(5, 2, 'A'),
            rect,
            fill(5, 2, 'B'),
            rect,
            TransitionKind::Dissolve,
            100,
        );
        let b = TransitionAdapter::new(
            "d".into(),
            node,
            rect,
            fill(5, 2, 'A'),
            rect,
            fill(5, 2, 'B'),
            rect,
            TransitionKind::Dissolve,
            100,
        );
        // Same seed → same threshold at same cell.
        assert_eq!(
            a.cell_threshold(2, 1, 0xD155),
            b.cell_threshold(2, 1, 0xD155),
        );
    }

    #[test]
    fn slide_left_at_t0_shows_old() {
        let (_scene, node) = panel_scene(5, 2);
        let rect = Rect::new(0, 0, 5, 2);
        let t = TransitionAdapter::new(
            "sl".into(),
            node,
            rect,
            fill(5, 2, 'A'),
            rect,
            fill(5, 2, 'B'),
            rect,
            TransitionKind::SlideLeft,
            100,
        );
        // t=0: old layer wins everywhere.
        assert_eq!(t.blend_cell(2, 1).map(|c| c.ch), Some('A'));
    }

    #[test]
    fn slide_left_at_t1_shows_new() {
        let (mut scene, node) = panel_scene(5, 2);
        let rect = Rect::new(0, 0, 5, 2);
        let mut t = TransitionAdapter::new(
            "sl".into(),
            node,
            rect,
            fill(5, 2, 'A'),
            rect,
            fill(5, 2, 'B'),
            rect,
            TransitionKind::SlideLeft,
            100,
        );
        // Advance past duration.
        t.elapsed_ms = 200;
        t.paint_into(&mut scene);
        // All cells should now show 'B'.
        for y in 0..2 {
            for x in 0..5 {
                assert_eq!(
                    scene.buffer_of(node).unwrap().get(x, y).unwrap().ch,
                    'B',
                    "slide-left at t=1, ({x},{y})",
                );
            }
        }
    }

    #[test]
    fn draw_down_traces_top_edge_before_revealing_rows() {
        let (_scene, node) = panel_scene(10, 4);
        let rect = Rect::new(0, 0, 10, 4);
        let mut next = fill(10, 4, 'B');
        for x in 0..10 {
            next.put_char(x, 0, '─', &CellStyle::default());
            next.put_char(x, 3, '─', &CellStyle::default());
        }
        next.put_char(0, 0, '╭', &CellStyle::default());
        next.put_char(9, 0, '╮', &CellStyle::default());
        next.put_char(0, 3, '╰', &CellStyle::default());
        next.put_char(9, 3, '╯', &CellStyle::default());
        let mut transition = TransitionAdapter::new(
            "draw".into(),
            node,
            rect,
            fill(10, 4, 'A'),
            rect,
            next,
            rect,
            TransitionKind::DrawDown,
            1000,
        );

        transition.elapsed_ms = 200;
        assert_eq!(transition.blend_cell(0, 0).map(|c| c.ch), Some('A'));
        assert_eq!(transition.blend_cell(3, 0).map(|c| c.ch), Some('─'));
        assert_eq!(transition.blend_cell(6, 0).map(|c| c.ch), Some('─'));
        assert_eq!(transition.blend_cell(7, 0).map(|c| c.ch), Some('A'));
        assert_eq!(transition.blend_cell(0, 1).map(|c| c.ch), Some('A'));

        transition.elapsed_ms = 500;
        assert_eq!(transition.blend_cell(0, 0).map(|c| c.ch), Some('╭'));
        assert_eq!(transition.blend_cell(9, 0).map(|c| c.ch), Some('╮'));
        assert_eq!(transition.blend_cell(0, 1).map(|c| c.ch), Some('A'));

        transition.elapsed_ms = 750;
        assert_eq!(transition.blend_cell(0, 2).map(|c| c.ch), Some('╰'));
        assert_eq!(transition.blend_cell(5, 2).map(|c| c.ch), Some('─'));
        assert_eq!(transition.blend_cell(9, 2).map(|c| c.ch), Some('╯'));
        assert_eq!(transition.blend_cell(0, 3).map(|c| c.ch), Some('A'));

        transition.elapsed_ms = 1000;
        assert_eq!(transition.blend_cell(9, 3).map(|c| c.ch), Some('╯'));
    }

    #[test]
    fn draw_right_traces_left_edge_before_revealing_columns() {
        let (_scene, node) = panel_scene(10, 4);
        let rect = Rect::new(0, 0, 10, 4);
        let mut next = fill(10, 4, 'B');
        for y in 0..4 {
            next.put_char(0, y, '│', &CellStyle::default());
            next.put_char(9, y, '│', &CellStyle::default());
        }
        next.put_char(0, 0, '╭', &CellStyle::default());
        next.put_char(0, 3, '╰', &CellStyle::default());
        next.put_char(9, 0, '╮', &CellStyle::default());
        next.put_char(9, 3, '╯', &CellStyle::default());
        let mut transition = TransitionAdapter::new(
            "draw".into(),
            node,
            rect,
            fill(10, 4, 'A'),
            rect,
            next,
            rect,
            TransitionKind::DrawRight,
            1000,
        );

        transition.elapsed_ms = 200;
        assert_eq!(transition.blend_cell(0, 0).map(|c| c.ch), Some('╭'));
        assert_eq!(transition.blend_cell(0, 1).map(|c| c.ch), Some('│'));
        assert_eq!(transition.blend_cell(0, 2).map(|c| c.ch), Some('A'));
        assert_eq!(transition.blend_cell(1, 0).map(|c| c.ch), Some('A'));

        transition.elapsed_ms = 500;
        assert_eq!(transition.blend_cell(0, 0).map(|c| c.ch), Some('╭'));
        assert_eq!(transition.blend_cell(0, 3).map(|c| c.ch), Some('╰'));
        assert_eq!(transition.blend_cell(1, 0).map(|c| c.ch), Some('A'));

        transition.elapsed_ms = 750;
        assert_eq!(transition.blend_cell(5, 0).map(|c| c.ch), Some('╮'));
        assert_eq!(transition.blend_cell(5, 2).map(|c| c.ch), Some('│'));
        assert_eq!(transition.blend_cell(5, 3).map(|c| c.ch), Some('╯'));
        assert_eq!(transition.blend_cell(9, 0).map(|c| c.ch), Some('A'));

        transition.elapsed_ms = 1000;
        assert_eq!(transition.blend_cell(9, 3).map(|c| c.ch), Some('╯'));
    }

    /// The mirror of `draw_right_traces_left_edge_before_revealing_columns`.
    ///
    /// Asserted as the reflection rather than as fresh expectations: the point of
    /// DrawLeft is that it does exactly what DrawRight does, from the other side.
    /// So the right edge is traced first, the frontier moves leftward, and the
    /// column at the frontier shows the authored *left* edge while it travels.
    #[test]
    fn draw_left_traces_right_edge_before_revealing_columns() {
        let (_scene, node) = panel_scene(10, 4);
        let rect = Rect::new(0, 0, 10, 4);
        let mut next = fill(10, 4, 'B');
        for y in 0..4u16 {
            next.put_char(0, y, '│', &CellStyle::default());
            next.put_char(9, y, '│', &CellStyle::default());
        }
        next.put_char(0, 0, '╭', &CellStyle::default());
        next.put_char(0, 3, '╰', &CellStyle::default());
        next.put_char(9, 0, '╮', &CellStyle::default());
        next.put_char(9, 3, '╯', &CellStyle::default());
        let mut transition = TransitionAdapter::new(
            "draw".into(),
            node,
            rect,
            fill(10, 4, 'A'),
            rect,
            next,
            rect,
            TransitionKind::DrawLeft,
            1000,
        );

        // First half traces the right edge downward; the left edge is untouched.
        transition.elapsed_ms = 200;
        assert_eq!(transition.blend_cell(9, 0).map(|c| c.ch), Some('╮'));
        assert_eq!(transition.blend_cell(9, 1).map(|c| c.ch), Some('│'));
        assert_eq!(transition.blend_cell(9, 2).map(|c| c.ch), Some('A'));
        assert_eq!(transition.blend_cell(8, 0).map(|c| c.ch), Some('A'));

        transition.elapsed_ms = 500;
        assert_eq!(transition.blend_cell(9, 0).map(|c| c.ch), Some('╮'));
        assert_eq!(transition.blend_cell(9, 3).map(|c| c.ch), Some('╯'));
        assert_eq!(transition.blend_cell(8, 0).map(|c| c.ch), Some('A'));

        // Second half unfurls leftward, the frontier carrying the left edge.
        transition.elapsed_ms = 750;
        assert_eq!(transition.blend_cell(4, 0).map(|c| c.ch), Some('╭'));
        assert_eq!(transition.blend_cell(4, 2).map(|c| c.ch), Some('│'));
        assert_eq!(transition.blend_cell(4, 3).map(|c| c.ch), Some('╰'));
        assert_eq!(transition.blend_cell(0, 0).map(|c| c.ch), Some('A'));

        transition.elapsed_ms = 1000;
        assert_eq!(transition.blend_cell(0, 0).map(|c| c.ch), Some('╭'));
        assert_eq!(transition.blend_cell(0, 3).map(|c| c.ch), Some('╰'));
    }

    #[test]
    fn draw_out_grows_a_live_frame_from_the_centre() {
        let (_scene, node) = panel_scene(10, 6);
        let rect = Rect::new(0, 0, 10, 6);
        let mut next = fill(10, 6, 'B');
        for x in 1..9 {
            next.put_char(x, 0, '═', &CellStyle::default());
            next.put_char(x, 5, '═', &CellStyle::default());
        }
        for y in 1..5 {
            next.put_char(0, y, '║', &CellStyle::default());
            next.put_char(9, y, '║', &CellStyle::default());
        }
        next.put_char(0, 0, '╔', &CellStyle::default());
        next.put_char(9, 0, '╗', &CellStyle::default());
        next.put_char(0, 5, '╚', &CellStyle::default());
        next.put_char(9, 5, '╝', &CellStyle::default());

        let mut transition = TransitionAdapter::new(
            "draw-out".into(),
            node,
            rect,
            fill(10, 6, 'A'),
            rect,
            next,
            rect,
            TransitionKind::DrawOut,
            1000,
        );
        transition.elapsed_ms = 500;

        // At halfway the completed frame's corners form a smaller 5×3
        // rectangle centred inside the final 10×6 bounds.
        assert_eq!(transition.blend_cell(2, 1).map(|c| c.ch), Some('╔'));
        assert_eq!(transition.blend_cell(6, 1).map(|c| c.ch), Some('╗'));
        assert_eq!(transition.blend_cell(2, 3).map(|c| c.ch), Some('╚'));
        assert_eq!(transition.blend_cell(6, 3).map(|c| c.ch), Some('╝'));
        assert_eq!(transition.blend_cell(4, 2).map(|c| c.ch), Some('B'));
        assert_eq!(transition.blend_cell(1, 1).map(|c| c.ch), Some('A'));

        transition.elapsed_ms = 1000;
        assert_eq!(transition.blend_cell(0, 0).map(|c| c.ch), Some('╔'));
        assert_eq!(transition.blend_cell(9, 5).map(|c| c.ch), Some('╝'));
    }

    #[test]
    fn unfurl_titles_appear_only_after_completion() {
        let rect = Rect::new(0, 0, 9, 3);
        let (_scene, node) = panel_scene(9, 3);
        let old = CellBuffer::new(9, 3);
        let mut new = CellBuffer::new(9, 3);
        new.put_str(0, 0, "╭─ Box ─╮", &CellStyle::default());

        for kind in [
            TransitionKind::DrawDown,
            TransitionKind::DrawRight,
            TransitionKind::DrawLeft,
        ] {
            let mut transition = TransitionAdapter::new(
                "unfurl".into(),
                node,
                rect,
                old.try_clone().unwrap(),
                rect,
                new.try_clone().unwrap(),
                rect,
                kind,
                1000,
            );
            transition.elapsed_ms = 900;
            assert_eq!(
                transition.blend_cell(3, 0).map(|cell| cell.ch),
                Some('─'),
                "{kind:?} must keep the title masked before completion",
            );

            transition.trigger_stop();
            assert_eq!(
                transition.blend_cell(3, 0).map(|cell| cell.ch),
                Some('B'),
                "{kind:?} must reveal the title on its terminal frame",
            );
        }
    }

    /// Shape-mismatched states: State A is 3x2 at (0, 0); State B is 2x2
    /// at (5, 3). Their union is (0, 0) to (7, 5). During dissolve, cells
    /// exclusive to A (x<3, y<2 when B-rect excluded) source from A; cells
    /// exclusive to B source from B; cells in neither leave the base
    /// layer to show through.
    #[test]
    fn shape_mismatched_dissolve_routes_cells_correctly() {
        let (_scene, node) = panel_scene(10, 8);
        let target = Rect::new(0, 0, 10, 8);
        let old_rect = Rect::new(0, 0, 3, 2);
        let new_rect = Rect::new(5, 3, 2, 2);
        let t = TransitionAdapter::new(
            "mix".into(),
            node,
            target,
            fill(3, 2, 'A'),
            old_rect,
            fill(2, 2, 'B'),
            new_rect,
            TransitionKind::Dissolve,
            100,
        );

        // Cell inside A only, outside B: should be 'A'.
        assert_eq!(
            t.blend_cell(1, 1).map(|c| c.ch),
            Some('A'),
            "A-exclusive cell"
        );
        // Cell inside B only, outside A: should be 'B'.
        assert_eq!(
            t.blend_cell(5, 3).map(|c| c.ch),
            Some('B'),
            "B-exclusive cell"
        );
        // Cell outside both rects: None (base layer shows through).
        assert_eq!(t.blend_cell(8, 7), None, "neither-rect cell reveals base");
    }

    /// Advancing a transition through time and calling paint() each tick
    /// writes into the scene node's buffer.
    #[test]
    fn advance_and_paint_updates_scene_buffer() {
        let (mut scene, node) = panel_scene(5, 2);
        let rect = Rect::new(0, 0, 5, 2);
        let mut t = TransitionAdapter::new(
            "d".into(),
            node,
            rect,
            fill(5, 2, 'A'),
            rect,
            fill(5, 2, 'B'),
            rect,
            TransitionKind::Dissolve,
            100,
        );
        let empty: Vec<String> = Vec::new();
        // Advance near the end so B wins most cells.
        for _ in 0..10 {
            let mut ctx = AdvanceCtx::new(Instant::now(), 0, 24, &empty);
            t.advance(&mut ctx);
        }
        t.paint_into(&mut scene);
        let mut a_count = 0;
        let mut b_count = 0;
        for y in 0..2 {
            for x in 0..5 {
                match scene.buffer_of(node).unwrap().get(x, y).unwrap().ch {
                    'A' => a_count += 1,
                    'B' => b_count += 1,
                    _ => {}
                }
            }
        }
        assert_eq!(
            a_count + b_count,
            10,
            "every cell should be A or B after dissolve"
        );
        // Very permissive: with duration=100, 10 ticks * 33ms = 330ms >
        // duration, so we're fully past end — mostly B. But the
        // dissolve mask can leave some A if thresholds are high — at
        // t >= 1.0, `t > threshold` is true for every threshold < 1.0,
        // so all cells go to B.
        assert_eq!(b_count, 10, "at t=1 all cells should be B");
    }

    #[test]
    fn panel_transition_buffers_retain_exact_governed_storage() {
        let governor = crate::resource::ResourceGovernor::new();
        let old = CellBuffer::try_new_governed(
            4,
            2,
            &governor,
            crate::resource::ResourceCategory::CompositorCells,
        )
        .unwrap();
        let new = CellBuffer::try_new_governed(
            4,
            2,
            &governor,
            crate::resource::ResourceCategory::CompositorCells,
        )
        .unwrap();
        let expected = 16 * std::mem::size_of::<Cell>();
        let adapter = TransitionAdapter::new(
            "panel".into(),
            NodeId::default(),
            Rect::new(0, 0, 4, 2),
            old,
            Rect::new(0, 0, 4, 2),
            new,
            Rect::new(0, 0, 4, 2),
            TransitionKind::Fade,
            300,
        );
        assert_eq!(
            governor.used(crate::resource::ResourceCategory::CompositorCells),
            expected
        );
        drop(adapter);
        assert_eq!(
            governor.used(crate::resource::ResourceCategory::CompositorCells),
            0
        );
    }
}
