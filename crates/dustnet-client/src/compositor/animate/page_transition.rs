//! Page-level transition between two full-page snapshots.
//!
//! `PageTransitionAdapter` implements the `Animation` trait and paints
//! its blended frame into a `NodeKind::Overlay` node's buffer each
//! tick. The composite walk (`compositor::composite::walk`) picks up
//! the overlay at Phase D — on top of every other scene layer — so a
//! running transition occludes foreground animations on the new page.
//! When the adapter finishes, the terminal runtime removes its compositor-owned
//! overlay before publishing completion; subsequent composites show the real
//! new page without any bypass machinery.
//!
//! Blending writes directly into the retained overlay buffer. No scratch frame
//! is allocated after an animation tick has advanced.
//!
//! Transition kinds:
//!
//! - `Cut`: instant swap.
//! - `Fade`: old dissolves to black (0.0–0.4), holds black (0.4–0.6),
//!   new appears from black (0.6–1.0). Per-cell jitter avoids a uniform
//!   wipe.
//! - `SlideLeft/Right/Up/Down`: old slides off, new slides in from the
//!   opposite edge.
//! - `DrawDown`: traces the top row left-to-right, then reveals rows downward.
//! - `Dissolve`: per-cell hashed threshold — cells flip from old to new
//!   when `t` crosses their individual threshold.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use crate::compositor::layout::cell::{Cell, CellBuffer, CellStyle};
use crate::compositor::scene::{NodeId, Scene};
use crate::parser::ast::TransitionKind;

use super::runtime::AnimState;
use super::{AdvanceCtx, AdvanceResult, Animation};

/// Internal `Animation::id()` for page transitions. The `__` prefix is
/// invalid AML attribute syntax, so this id cannot collide with any
/// author-declared animation.
pub const PAGE_TRANSITION_ID: &str = "__page_transition__";

/// Tick interval for page transitions (~30fps). Matches
/// `animate::wasm::TICK_MS`; duplicated here so the page-transition
/// module is self-contained.
const TICK_MS: u32 = 33;

/// Blend directly into the retained overlay buffer. This is the production
/// transition path: after a tick begins it cannot encounter a scratch-buffer
/// allocation and therefore never advances without committing its frame.
fn render_transition_frame_into(
    old_buf: &CellBuffer,
    new_buf: &CellBuffer,
    kind: TransitionKind,
    t: f32,
    out: &mut CellBuffer,
) {
    let t = t.clamp(0.0, 1.0);
    let w = out.width;
    let h = out.height;
    let wu = usize::from(w);
    let hu = usize::from(h);
    let black = Cell {
        ch: ' ',
        grapheme: None,
        style: CellStyle {
            bg: Some(crate::color::ResolvedColor::Named(
                crate::color::NamedColor::Black,
            )),
            ..Default::default()
        },
    };
    for y in 0..h {
        for x in 0..w {
            out.set(x, y, black.clone());
        }
    }

    match kind {
        TransitionKind::Cut => {
            copy_region(new_buf, out, 0, 0);
        }
        TransitionKind::Fade => {
            // Fade to black, hold, fade in.
            if !(0.4..=0.6).contains(&t) {
                for y in 0..hu as u16 {
                    for x in 0..wu as u16 {
                        let mut hasher = DefaultHasher::new();
                        (x, y, 0xFADEu32).hash(&mut hasher);
                        let cell_t = (hasher.finish() % 1000) as f32 / 1000.0;

                        if t < 0.4 {
                            let progress = t / 0.4;
                            if progress <= cell_t
                                && let Some(cell) = old_buf.get(x, y)
                            {
                                out.set(x, y, cell.clone());
                            }
                        } else {
                            let progress = (t - 0.6) / 0.4;
                            if progress > cell_t
                                && let Some(cell) = new_buf.get(x, y)
                            {
                                out.set(x, y, cell.clone());
                            }
                        }
                    }
                }
            }
        }
        TransitionKind::SlideLeft => {
            let offset = (t * wu as f32) as u16;
            for y in 0..hu as u16 {
                for x in 0..wu as u16 {
                    let src_x = x + offset;
                    if src_x < wu as u16 {
                        if let Some(cell) = old_buf.get(src_x, y) {
                            out.set(x, y, cell.clone());
                        }
                    } else {
                        let new_x = src_x - wu as u16;
                        if let Some(cell) = new_buf.get(new_x, y) {
                            out.set(x, y, cell.clone());
                        }
                    }
                }
            }
        }
        TransitionKind::SlideRight => {
            let offset = (t * wu as f32) as u16;
            for y in 0..hu as u16 {
                for x in 0..wu as u16 {
                    if x < offset {
                        let new_x = (wu as u16).saturating_sub(offset) + x;
                        if let Some(cell) = new_buf.get(new_x, y) {
                            out.set(x, y, cell.clone());
                        }
                    } else {
                        let src_x = x - offset;
                        if let Some(cell) = old_buf.get(src_x, y) {
                            out.set(x, y, cell.clone());
                        }
                    }
                }
            }
        }
        TransitionKind::SlideUp => {
            let offset = (t * hu as f32) as u16;
            for y in 0..hu as u16 {
                for x in 0..wu as u16 {
                    let src_y = y + offset;
                    if src_y < hu as u16 {
                        if let Some(cell) = old_buf.get(x, src_y) {
                            out.set(x, y, cell.clone());
                        }
                    } else {
                        let new_y = src_y - hu as u16;
                        if let Some(cell) = new_buf.get(x, new_y) {
                            out.set(x, y, cell.clone());
                        }
                    }
                }
            }
        }
        TransitionKind::SlideDown => {
            let offset = (t * hu as f32) as u16;
            for y in 0..hu as u16 {
                for x in 0..wu as u16 {
                    if y < offset {
                        let new_y = (hu as u16).saturating_sub(offset) + y;
                        if let Some(cell) = new_buf.get(x, new_y) {
                            out.set(x, y, cell.clone());
                        }
                    } else {
                        let src_y = y - offset;
                        if let Some(cell) = old_buf.get(x, src_y) {
                            out.set(x, y, cell.clone());
                        }
                    }
                }
            }
        }
        TransitionKind::DrawDown => {
            const TOP_EDGE_PHASE: f32 = 0.50;
            let (revealed_columns, revealed_rows) = if t < TOP_EDGE_PHASE {
                (((t / TOP_EDGE_PHASE) * w as f32).ceil() as u16, 0)
            } else {
                let progress = (t - TOP_EDGE_PHASE) / (1.0 - TOP_EDGE_PHASE);
                let remaining_rows = h.saturating_sub(1);
                (w, 1 + (progress * remaining_rows as f32).ceil() as u16)
            };
            for y in 0..hu as u16 {
                for x in 0..wu as u16 {
                    let reveal_new = if t < TOP_EDGE_PHASE {
                        y == 0 && x < revealed_columns
                    } else {
                        y < revealed_rows
                    };
                    let cell = if t >= TOP_EDGE_PHASE
                        && revealed_rows > 1
                        && revealed_rows < h
                        && y + 1 == revealed_rows
                    {
                        new_buf.get(x, h.saturating_sub(1))
                    } else if reveal_new {
                        new_buf.get(x, y)
                    } else {
                        old_buf.get(x, y)
                    };
                    if let Some(cell) = cell {
                        out.set(x, y, cell.clone());
                    }
                }
            }
        }
        TransitionKind::DrawRight => {
            const LEFT_EDGE_PHASE: f32 = 0.50;
            let (revealed_rows, revealed_columns) = if t < LEFT_EDGE_PHASE {
                (((t / LEFT_EDGE_PHASE) * h as f32).ceil() as u16, 0)
            } else {
                let progress = (t - LEFT_EDGE_PHASE) / (1.0 - LEFT_EDGE_PHASE);
                let remaining_columns = w.saturating_sub(1);
                (h, 1 + (progress * remaining_columns as f32).ceil() as u16)
            };
            for y in 0..hu as u16 {
                for x in 0..wu as u16 {
                    let reveal_new = if t < LEFT_EDGE_PHASE {
                        x == 0 && y < revealed_rows
                    } else {
                        x < revealed_columns
                    };
                    let cell = if t >= LEFT_EDGE_PHASE
                        && revealed_columns > 1
                        && revealed_columns < w
                        && x + 1 == revealed_columns
                    {
                        new_buf.get(w.saturating_sub(1), y)
                    } else if reveal_new {
                        new_buf.get(x, y)
                    } else {
                        old_buf.get(x, y)
                    };
                    if let Some(cell) = cell {
                        out.set(x, y, cell.clone());
                    }
                }
            }
        }
        TransitionKind::DrawOut => {
            let revealed_w = ((t * w as f32).ceil() as u16).clamp(1, w.max(1));
            let revealed_h = ((t * h as f32).ceil() as u16).clamp(1, h.max(1));
            let start_x = w.saturating_sub(revealed_w) / 2;
            let start_y = h.saturating_sub(revealed_h) / 2;
            let end_x = start_x.saturating_add(revealed_w);
            let end_y = start_y.saturating_add(revealed_h);
            for y in 0..hu as u16 {
                for x in 0..wu as u16 {
                    let source = if x >= start_x && x < end_x && y >= start_y && y < end_y {
                        new_buf
                    } else {
                        old_buf
                    };
                    if let Some(cell) = source.get(x, y) {
                        out.set(x, y, cell.clone());
                    }
                }
            }
        }
        TransitionKind::Dissolve => {
            for y in 0..hu as u16 {
                for x in 0..wu as u16 {
                    let mut hasher = DefaultHasher::new();
                    (x, y).hash(&mut hasher);
                    let threshold = (hasher.finish() % 1000) as f32 / 1000.0;
                    if t > threshold {
                        if let Some(cell) = new_buf.get(x, y) {
                            out.set(x, y, cell.clone());
                        }
                    } else if let Some(cell) = old_buf.get(x, y) {
                        out.set(x, y, cell.clone());
                    }
                }
            }
        }
    }
}

/// Copy all cells from `src` into `dest` at `(dest_x, dest_y)`.
/// Includes spaces — transition frames must fully overwrite prior pixels
/// to avoid ghost artifacts.
fn copy_region(src: &CellBuffer, dest: &mut CellBuffer, dest_x: u16, dest_y: u16) {
    for y in 0..src.height {
        for x in 0..src.width {
            if let Some(cell) = src.get(x, y) {
                dest.set(dest_x + x, dest_y + y, cell.clone());
            }
        }
    }
}

// ─── PageTransitionAdapter ──────────────────────────────────────────

/// Scene-integrated page-level transition.
///
/// Owns old/new snapshots, reads blended frames from `render_transition_frame`,
/// and paints them into an `Overlay` node's buffer each tick. On the final
/// tick, the terminal runtime drops the overlay after painting and before
/// publishing completion.
///
/// Unlike the per-panel `TransitionAdapter`, this adapter covers the full
/// viewport (old and new page are always the same shape there) and does
/// not need shape-mismatched blending.
pub struct PageTransitionAdapter {
    /// `NodeId` of the `Overlay` node this adapter writes into. Created
    /// by `Scene::insert_overlay` before the adapter is pushed onto
    /// `AnimationRuntime.animations`.
    overlay: NodeId,
    old_snapshot: CellBuffer,
    new_snapshot: CellBuffer,
    kind: TransitionKind,
    /// Total transition duration in ms. `0` means instant (Cut).
    duration_ms: u32,
    /// Accumulated ticks. Counted in `TICK_MS` increments — the
    /// runtime calls `advance` at the ~33ms tick rate, so tick-count
    /// timing is a reasonable proxy for wall-clock without needing
    /// `Instant` arithmetic.
    elapsed_ms: u32,
    state: AnimState,
}

impl PageTransitionAdapter {
    pub fn new(
        overlay: NodeId,
        old_snapshot: CellBuffer,
        new_snapshot: CellBuffer,
        kind: TransitionKind,
        duration_ms: u32,
    ) -> Self {
        // Duration 0 (or Cut) finishes on the first advance — still
        // passes through Running first so `paint` fires once with t=1.0
        // and the overlay shows the new page before the remove patch.
        let state = AnimState::Running;
        Self {
            overlay,
            old_snapshot,
            new_snapshot,
            kind,
            duration_ms,
            elapsed_ms: 0,
            state,
        }
    }

    /// Normalized progress `[0.0, 1.0]`. Exposed for testing.
    pub fn progress(&self) -> f32 {
        if self.duration_ms == 0 {
            1.0
        } else {
            (self.elapsed_ms as f32 / self.duration_ms as f32).min(1.0)
        }
    }

    pub fn overlay(&self) -> NodeId {
        self.overlay
    }
}

impl Animation for PageTransitionAdapter {
    fn id(&self) -> &str {
        PAGE_TRANSITION_ID
    }

    fn advance(&mut self, _ctx: &mut AdvanceCtx) -> AdvanceResult {
        if matches!(self.state, AnimState::Finished) {
            return AdvanceResult::none();
        }

        // Tick-count timing: each advance counts as one TICK_MS.
        self.elapsed_ms = self.elapsed_ms.saturating_add(TICK_MS);

        let is_last_tick = self.elapsed_ms >= self.duration_ms;
        if is_last_tick {
            self.state = AnimState::Finished;
            // Teardown is compositor-owned and allocation-free. The terminal
            // runtime removes the overlay after this final paint and before it
            // publishes PAGE_TRANSITION_ID completion.
            return AdvanceResult::with_buffer(self.overlay);
        }

        AdvanceResult::with_buffer(self.overlay)
    }

    fn finished(&self) -> bool {
        matches!(self.state, AnimState::Finished)
    }

    fn state(&self) -> AnimState {
        self.state
    }

    fn background(&self) -> bool {
        false
    }

    fn paint(&self, scene: &mut Scene) {
        let t = self.progress();
        let Some(dst) = scene.overlay_buffer_mut(self.overlay) else {
            return;
        };
        render_transition_frame_into(&self.old_snapshot, &self.new_snapshot, self.kind, t, dst);
    }
}

#[cfg(test)]
mod adapter_tests {
    use super::*;
    use crate::compositor::layout::Rect;
    use crate::compositor::layout::cell::CellStyle;
    use crate::compositor::scene::{self, NodeKind, OverlaySource};

    fn empty_scene_with_overlay(w: u16, h: u16) -> (scene::Scene, NodeId) {
        let src = r#"[page mode=document][text]x[/text][/page]"#;
        let mut scanner = crate::scanner::Scanner::new(src.as_bytes()).unwrap();
        let tokens = scanner.scan_all().unwrap();
        let doc = crate::parser::parse(tokens).document.unwrap();
        let mut scene = scene::build::from_document(&doc);
        let id = scene.insert_overlay(
            i16::MAX,
            OverlaySource::PageTransition,
            Rect::new(0, 0, w, h),
        );
        (scene, id)
    }

    fn solid_buffer(w: u16, h: u16, ch: char) -> CellBuffer {
        let mut buf = CellBuffer::new_opaque(w, h);
        let style = CellStyle::default();
        for y in 0..h {
            for x in 0..w {
                buf.put_char(x, y, ch, &style);
            }
        }
        buf
    }

    fn dummy_ctx() -> AdvanceCtx<'static> {
        // Empty finished_ids slice tied to a 'static reference; AdvanceCtx
        // only borrows it for the lifetime of the call. Use a leak-free
        // static empty slice.
        static EMPTY: [String; 0] = [];
        AdvanceCtx::new(std::time::Instant::now(), 0, 24, &EMPTY)
    }

    #[test]
    fn id_is_internal_constant() {
        let (_scene, id) = empty_scene_with_overlay(4, 2);
        let adapter = PageTransitionAdapter::new(
            id,
            solid_buffer(4, 2, 'A'),
            solid_buffer(4, 2, 'B'),
            TransitionKind::Dissolve,
            300,
        );
        assert_eq!(adapter.id(), PAGE_TRANSITION_ID);
    }

    #[test]
    fn governed_adapter_releases_transition_storage_on_drop() {
        let governor = crate::resource::ResourceGovernor::new();
        let mut old = CellBuffer::try_new_governed(
            4,
            2,
            &governor,
            crate::resource::ResourceCategory::CompositorCells,
        )
        .unwrap();
        let mut new = CellBuffer::try_new_governed(
            4,
            2,
            &governor,
            crate::resource::ResourceCategory::CompositorCells,
        )
        .unwrap();
        for y in 0..2 {
            for x in 0..4 {
                old.put_char(x, y, 'A', &Default::default());
                new.put_char(x, y, 'B', &Default::default());
            }
        }
        let (_scene, id) = empty_scene_with_overlay(4, 2);
        let adapter = PageTransitionAdapter::new(id, old, new, TransitionKind::Fade, 300);
        let expected = 16 * std::mem::size_of::<crate::compositor::layout::cell::Cell>();
        assert_eq!(governor.total_used(), expected);
        drop(adapter);
        assert_eq!(governor.total_used(), 0);
    }

    #[test]
    fn advance_accumulates_elapsed_and_reports_buffer_write() {
        let (_scene, id) = empty_scene_with_overlay(4, 2);
        let mut adapter = PageTransitionAdapter::new(
            id,
            solid_buffer(4, 2, 'A'),
            solid_buffer(4, 2, 'B'),
            TransitionKind::Dissolve,
            // Three ticks of TICK_MS (33ms) → 99ms total before finish
            TICK_MS * 3,
        );

        let mut ctx = dummy_ctx();
        // Tick 1
        let r1 = adapter.advance(&mut ctx);
        assert_eq!(r1.wrote_buffer, Some(id));
        assert!(r1.patch.is_none());
        assert!(!adapter.finished());
        assert!((adapter.progress() - 1.0 / 3.0).abs() < 0.01);

        // Tick 2
        let r2 = adapter.advance(&mut ctx);
        assert_eq!(r2.wrote_buffer, Some(id));
        assert!(r2.patch.is_none());
        assert!(!adapter.finished());
    }

    #[test]
    fn final_tick_defers_compositor_owned_teardown_and_transitions_to_finished() {
        let (_scene, id) = empty_scene_with_overlay(4, 2);
        let mut adapter = PageTransitionAdapter::new(
            id,
            solid_buffer(4, 2, 'A'),
            solid_buffer(4, 2, 'B'),
            TransitionKind::Cut,
            TICK_MS, // one tick → finishes on first advance
        );

        let mut ctx = dummy_ctx();
        let r = adapter.advance(&mut ctx);

        assert_eq!(r.wrote_buffer, Some(id), "final tick still paints t=1.0");
        assert!(r.patch.is_none(), "generic structural patches are not used");
        assert!(adapter.finished());
        assert!(matches!(adapter.state(), AnimState::Finished));
    }

    #[test]
    fn subsequent_advance_after_finished_is_noop() {
        let (_scene, id) = empty_scene_with_overlay(4, 2);
        let mut adapter = PageTransitionAdapter::new(
            id,
            solid_buffer(4, 2, 'A'),
            solid_buffer(4, 2, 'B'),
            TransitionKind::Cut,
            TICK_MS,
        );
        let mut ctx = dummy_ctx();
        let _first = adapter.advance(&mut ctx); // transitions to Finished
        let second = adapter.advance(&mut ctx);
        assert!(second.is_noop(), "post-finish advance must be a noop");
    }

    #[test]
    fn paint_writes_blended_frame_into_overlay_buffer() {
        let (mut scene, id) = empty_scene_with_overlay(4, 2);
        // For Cut kind, t=1.0 produces exactly the new_snapshot — easy to
        // assert on. Use solid 'A' → 'B' buffers so any cell in the
        // overlay after paint reveals which side of the blend ran.
        let mut adapter = PageTransitionAdapter::new(
            id,
            solid_buffer(4, 2, 'A'),
            solid_buffer(4, 2, 'B'),
            TransitionKind::Cut,
            TICK_MS,
        );
        let mut ctx = dummy_ctx();
        adapter.advance(&mut ctx); // elapsed = TICK_MS → t = 1.0

        adapter.paint(&mut scene);

        // Cut at t=1.0 fills with new_snapshot ('B').
        let overlay = scene.get(id).expect("overlay still present pre-remove");
        let buf = overlay.buffer().expect("overlay has a buffer");
        for y in 0..2 {
            for x in 0..4 {
                assert_eq!(
                    buf.get(x, y).unwrap().ch,
                    'B',
                    "paint at t=1.0 for Cut must show new_snapshot at ({x},{y})",
                );
            }
        }
    }

    #[test]
    fn paint_on_wrong_node_kind_is_noop() {
        // overlay_buffer_mut is kind-gated: if we hand the adapter a
        // non-Overlay NodeId, paint silently skips instead of writing
        // into the wrong subsystem's buffer.
        let (mut scene, _overlay_id) = empty_scene_with_overlay(4, 2);
        let text_id = scene
            .iter_tree_order()
            .find(|n| matches!(n.kind(), NodeKind::Text(_)))
            .map(|n| n.id())
            .expect("fixture has a Text node");

        let mut adapter = PageTransitionAdapter::new(
            text_id,
            solid_buffer(4, 2, 'A'),
            solid_buffer(4, 2, 'B'),
            TransitionKind::Cut,
            TICK_MS,
        );
        let mut ctx = dummy_ctx();
        adapter.advance(&mut ctx);
        // Would panic if paint tried to write into the wrong kind.
        adapter.paint(&mut scene);
    }

    #[test]
    fn progress_clamps_at_one() {
        let (_scene, id) = empty_scene_with_overlay(4, 2);
        let mut adapter = PageTransitionAdapter::new(
            id,
            solid_buffer(4, 2, 'A'),
            solid_buffer(4, 2, 'B'),
            TransitionKind::Dissolve,
            TICK_MS * 2,
        );
        let mut ctx = dummy_ctx();
        adapter.advance(&mut ctx);
        adapter.advance(&mut ctx);
        // Over-advance — elapsed_ms already saturated, progress stays 1.0.
        adapter.advance(&mut ctx);
        assert!((adapter.progress() - 1.0).abs() < 0.001);
    }

    // ─── All-kinds smoke test ────────────────────────────────────
    //
    // Drive the adapter through tick→paint for every `TransitionKind`
    // and assert the overlay buffer ends up populated with something
    // at t=1.0. The per-kind blending math is exercised directly by
    // `render_transition_frame`'s own tests; this test pins the
    // integration between adapter lifecycle and paint.

    fn drive_to_end(adapter: &mut PageTransitionAdapter, scene: &mut Scene) {
        let mut ctx = dummy_ctx();
        // Three ticks at TICK_MS — duration was TICK_MS * 3 so the
        // third tick marks the adapter Finished.
        for _ in 0..3 {
            adapter.advance(&mut ctx);
            adapter.paint(scene);
        }
    }

    fn smoke_for_kind(kind: TransitionKind) {
        let (mut scene, overlay_id) = empty_scene_with_overlay(8, 2);
        let mut adapter = PageTransitionAdapter::new(
            overlay_id,
            solid_buffer(8, 2, 'A'),
            solid_buffer(8, 2, 'B'),
            kind,
            TICK_MS * 3,
        );
        drive_to_end(&mut adapter, &mut scene);
        assert!(adapter.finished(), "{kind:?}: adapter should be finished");
        // Overlay still in scene (RemoveNode patch hasn't been applied
        // by a runtime here — only emitted), buffer populated.
        let buf = scene.get(overlay_id).unwrap().buffer().unwrap();
        // At t=1.0 every kind should produce the new_snapshot as the
        // dominant output — at minimum, some cell carries 'B'.
        let has_b = (0..buf.height)
            .any(|y| (0..buf.width).any(|x| buf.get(x, y).is_some_and(|c| c.ch == 'B')));
        assert!(
            has_b,
            "{kind:?} at t=1.0 should have painted at least one 'B' cell"
        );
    }

    #[test]
    fn smoke_cut() {
        smoke_for_kind(TransitionKind::Cut);
    }
    #[test]
    fn smoke_fade() {
        smoke_for_kind(TransitionKind::Fade);
    }
    #[test]
    fn smoke_slide_left() {
        smoke_for_kind(TransitionKind::SlideLeft);
    }
    #[test]
    fn smoke_slide_right() {
        smoke_for_kind(TransitionKind::SlideRight);
    }
    #[test]
    fn smoke_slide_up() {
        smoke_for_kind(TransitionKind::SlideUp);
    }
    #[test]
    fn smoke_slide_down() {
        smoke_for_kind(TransitionKind::SlideDown);
    }
    #[test]
    fn smoke_draw_down() {
        smoke_for_kind(TransitionKind::DrawDown);
    }
    #[test]
    fn smoke_draw_right() {
        smoke_for_kind(TransitionKind::DrawRight);
    }
    #[test]
    fn smoke_draw_out() {
        smoke_for_kind(TransitionKind::DrawOut);
    }
    #[test]
    fn smoke_dissolve() {
        smoke_for_kind(TransitionKind::Dissolve);
    }

    #[test]
    fn duration_zero_finishes_on_first_tick() {
        let (_scene, id) = empty_scene_with_overlay(4, 2);
        let mut adapter = PageTransitionAdapter::new(
            id,
            solid_buffer(4, 2, 'A'),
            solid_buffer(4, 2, 'B'),
            TransitionKind::Cut,
            0,
        );
        let mut ctx = dummy_ctx();
        let r = adapter.advance(&mut ctx);
        assert!(adapter.finished(), "duration=0 must finish on first tick");
        assert!(r.patch.is_none());
    }
}
