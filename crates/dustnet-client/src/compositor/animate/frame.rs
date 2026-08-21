//! Frame-based animation: a sequence of pre-rendered `CellBuffer`s
//! advanced by elapsed time. Under the `Animation` trait, the adapter
//! writes the current frame into its owning scene node's buffer via
//! `Scene::wasm_buffer_mut` and reports `wrote_buffer: Some(node_id)`
//! for composite invalidation.

use std::time::Instant;

use crate::compositor::layout::cell::CellBuffer;
use crate::compositor::scene::{NodeId, Scene};
use crate::parser::ast::LoopBehavior;
use crate::resource::BudgetLease;

use super::runtime::AnimState;
use super::{AdvanceCtx, AdvanceResult, Animation};

/// Frame-animation adapter: delay, chain dependencies (`after`),
/// autoplay, loop modes, viewport awareness.
pub struct FrameAnimationAdapter {
    id: String,
    node: NodeId,
    frames: Vec<CellBuffer>,
    fps: u8,
    loop_behavior: LoopBehavior,
    state: AnimState,
    current_frame: usize,
    last_advance: Instant,
    delay_ms: u32,
    after: Option<String>,
    started_at: Option<Instant>,
    /// For `Bounce` loop mode: current direction.
    forward: bool,
    loops_done: u32,
    background: bool,
    autoplay: bool,
    _collection_budget_lease: Option<BudgetLease>,
}

impl FrameAnimationAdapter {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: String,
        node: NodeId,
        frames: Vec<CellBuffer>,
        fps: u8,
        loop_behavior: LoopBehavior,
        delay_ms: u32,
        after: Option<String>,
        autoplay: bool,
        background: bool,
    ) -> Self {
        let initial_state = if delay_ms > 0 || after.is_some() || !autoplay {
            AnimState::Waiting
        } else {
            AnimState::Running
        };
        Self {
            id,
            node,
            frames,
            fps: fps.max(1),
            loop_behavior,
            state: initial_state,
            current_frame: 0,
            last_advance: Instant::now(),
            delay_ms,
            after,
            started_at: None,
            forward: true,
            loops_done: 0,
            background,
            autoplay,
            _collection_budget_lease: None,
        }
    }

    pub(crate) fn with_collection_budget(mut self, budget_lease: BudgetLease) -> Self {
        self._collection_budget_lease = Some(budget_lease);
        self
    }

    pub fn current_frame(&self) -> usize {
        self.current_frame
    }

    pub fn node(&self) -> NodeId {
        self.node
    }

    /// Is this animation visible at the given viewport window?
    /// Currently always true — most animations today are screen-
    /// mode full-canvas or hero regions. Region-based viewport
    /// culling can thread the node's `screen_rect` through
    /// `AdvanceCtx` when a concrete need arises.
    fn visible(&self, _ctx: &AdvanceCtx) -> bool {
        true
    }

    fn advance_frame(&mut self) {
        let num = self.frames.len();
        if num == 0 {
            return;
        }
        match self.loop_behavior {
            LoopBehavior::None => {
                if self.current_frame + 1 < num {
                    self.current_frame += 1;
                } else {
                    self.state = AnimState::Finished;
                }
            }
            LoopBehavior::Infinite => {
                self.current_frame = (self.current_frame + 1) % num;
            }
            LoopBehavior::Count(max) => {
                if self.current_frame + 1 < num {
                    self.current_frame += 1;
                } else if self.loops_done + 1 < max {
                    self.current_frame = 0;
                    self.loops_done += 1;
                } else {
                    self.state = AnimState::Finished;
                }
            }
            LoopBehavior::Bounce => {
                if self.forward {
                    if self.current_frame + 1 < num {
                        self.current_frame += 1;
                    } else {
                        self.forward = false;
                        self.current_frame = self.current_frame.saturating_sub(1);
                    }
                } else if self.current_frame > 0 {
                    self.current_frame -= 1;
                } else {
                    self.forward = true;
                    self.current_frame = (self.current_frame + 1).min(num - 1);
                }
            }
        }
    }
}

impl Animation for FrameAnimationAdapter {
    fn id(&self) -> &str {
        &self.id
    }

    fn advance(&mut self, ctx: &mut AdvanceCtx) -> AdvanceResult {
        if matches!(self.state, AnimState::Finished) || self.frames.is_empty() {
            return AdvanceResult::none();
        }

        // Waiting → Running: check delay and after-dependency.
        if matches!(self.state, AnimState::Waiting) {
            if !self.autoplay {
                return AdvanceResult::none();
            }
            if let Some(dep) = &self.after
                && !ctx.finished_ids.iter().any(|id| id == dep)
            {
                return AdvanceResult::none();
            }
            match self.started_at {
                None => {
                    self.started_at = Some(ctx.now);
                    if self.delay_ms > 0 {
                        return AdvanceResult::none();
                    }
                }
                Some(t) => {
                    if ctx.now.duration_since(t).as_millis() < self.delay_ms as u128 {
                        return AdvanceResult::none();
                    }
                }
            }
            self.state = AnimState::Running;
            // Fall through to first frame's write below.
        }

        // Viewport pause.
        let visible = self.visible(ctx);
        match (self.state, visible) {
            (AnimState::Running, false) => self.state = AnimState::Paused,
            (AnimState::Paused, true) => self.state = AnimState::Running,
            _ => {}
        }

        if !matches!(self.state, AnimState::Running) {
            return AdvanceResult::none();
        }

        // Frame advance gated by fps.
        let interval_ms = 1000u128 / self.fps as u128;
        let elapsed = ctx.now.duration_since(self.last_advance).as_millis();
        if elapsed < interval_ms {
            return AdvanceResult::none();
        }
        self.last_advance = ctx.now;
        self.advance_frame();
        AdvanceResult::with_buffer(self.node)
    }

    fn finished(&self) -> bool {
        matches!(self.state, AnimState::Finished)
    }

    fn state(&self) -> AnimState {
        self.state
    }

    fn background(&self) -> bool {
        self.background
    }

    fn paint(&self, scene: &mut Scene) {
        if self.frames.is_empty() {
            return;
        }
        let Some(last) = self.frames.len().checked_sub(1) else {
            return;
        };
        let Some(frame) = self.frames.get(self.current_frame.min(last)) else {
            return;
        };
        let Some(dst) = scene.wasm_buffer_mut(self.node) else {
            return;
        };
        let h = frame.height.min(dst.height);
        let w = frame.width.min(dst.width);
        for y in 0..h {
            if let Some(row) = frame.row(y) {
                for x in 0..w {
                    if let Some(cell) = row.get(x as usize) {
                        dst.set(x, y, cell.clone());
                    }
                }
            }
        }
    }

    fn trigger_start(&mut self, now: Instant) {
        self.state = AnimState::Running;
        self.current_frame = 0;
        self.last_advance = now;
        self.started_at = Some(now);
        self.loops_done = 0;
        self.forward = true;
    }

    fn trigger_stop(&mut self) {
        // Jump to the last frame before finishing. This matches the
        // state left by natural completion (`advance_frame` leaves
        // `current_frame` at the final index when a non-looping
        // animation runs out), which matters because `paint` copies
        // `frames[current_frame]` into the node buffer — stopping at
        // frame 0 would overwrite already-rendered content with the
        // blank first frame. Consumer that relies on this: `relayout_
        // panels_for` re-marking previously-finished animations after
        // scene rebuild.
        if !self.frames.is_empty() {
            self.current_frame = self.frames.len() - 1;
        }
        self.state = AnimState::Finished;
    }

    fn skip(&mut self) -> AdvanceResult {
        // Seek to the final frame (via trigger_stop) and flag the node
        // buffer as dirty so the runtime's paint pass writes the last
        // frame into the scene.
        self.trigger_stop();
        AdvanceResult::with_buffer(self.node)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compositor::layout::cell::CellStyle;
    use crate::compositor::scene;

    fn make_frame(w: u16, h: u16, ch: char) -> CellBuffer {
        let mut buf = CellBuffer::new(w, h);
        for y in 0..h {
            for x in 0..w {
                buf.put_char(x, y, ch, &CellStyle::default());
            }
        }
        buf
    }

    fn minimal_scene_with_animation(node_w: u16, node_h: u16) -> (Scene, NodeId) {
        let doc = {
            let src = r#"[page mode=document]
                [animate id="fa" fps=30][frame][text]x[/text][/frame][/animate]
            [/page]"#;
            let mut scanner = crate::scanner::Scanner::new(src.as_bytes()).unwrap();
            let tokens = scanner.scan_all().unwrap();
            crate::parser::parse(tokens).document.unwrap()
        };
        let mut scene = scene::build::from_document(&doc);
        let node = scene.find_by_aml_id("fa").unwrap();
        scene.allocate_buffer(node, node_w, node_h);
        (scene, node)
    }

    fn ctx<'a>(now: Instant, finished: &'a [String]) -> AdvanceCtx<'a> {
        AdvanceCtx::new(now, 0, 24, finished)
    }

    #[test]
    fn autoplay_starts_immediately_without_delay() {
        let (_scene, node) = minimal_scene_with_animation(1, 1);
        let a = FrameAnimationAdapter::new(
            "x".into(),
            node,
            vec![make_frame(1, 1, 'A'), make_frame(1, 1, 'B')],
            2,
            LoopBehavior::None,
            0,
            None,
            true,
            false,
        );
        assert_eq!(a.state(), AnimState::Running);
    }

    #[test]
    fn non_autoplay_waits_for_trigger() {
        let (_scene, node) = minimal_scene_with_animation(1, 1);
        let mut a = FrameAnimationAdapter::new(
            "x".into(),
            node,
            vec![make_frame(1, 1, 'A')],
            30,
            LoopBehavior::None,
            0,
            None,
            false,
            false,
        );
        assert_eq!(a.state(), AnimState::Waiting);
        let now = Instant::now();
        a.trigger_start(now);
        assert_eq!(a.state(), AnimState::Running);
    }

    #[test]
    fn after_dependency_gates_start() {
        let (_scene, node) = minimal_scene_with_animation(1, 1);
        let mut a = FrameAnimationAdapter::new(
            "b".into(),
            node,
            vec![make_frame(1, 1, 'A')],
            30,
            LoopBehavior::None,
            0,
            Some("a".into()),
            true,
            false,
        );
        assert_eq!(a.state(), AnimState::Waiting);
        let now = Instant::now();
        // Dependency not finished yet.
        let empty: Vec<String> = Vec::new();
        let mut c = ctx(now, &empty);
        a.advance(&mut c);
        assert_eq!(a.state(), AnimState::Waiting);
        // Dependency satisfied.
        let finished = vec!["a".into()];
        let mut c = ctx(now, &finished);
        a.advance(&mut c);
        assert_eq!(a.state(), AnimState::Running);
    }

    #[test]
    fn looping_wraps() {
        let (_scene, node) = minimal_scene_with_animation(1, 1);
        let mut a = FrameAnimationAdapter::new(
            "x".into(),
            node,
            vec![make_frame(1, 1, 'A'), make_frame(1, 1, 'B')],
            60,
            LoopBehavior::Infinite,
            0,
            None,
            true,
            false,
        );
        a.last_advance = Instant::now() - std::time::Duration::from_secs(1);
        let empty: Vec<String> = Vec::new();
        let mut c = ctx(Instant::now(), &empty);
        a.advance(&mut c);
        assert_eq!(a.current_frame(), 1);
        a.last_advance = Instant::now() - std::time::Duration::from_secs(1);
        a.advance(&mut c);
        assert_eq!(a.current_frame(), 0);
    }

    #[test]
    fn non_looping_finishes_after_last_frame() {
        let (_scene, node) = minimal_scene_with_animation(1, 1);
        let mut a = FrameAnimationAdapter::new(
            "x".into(),
            node,
            vec![make_frame(1, 1, 'A'), make_frame(1, 1, 'B')],
            60,
            LoopBehavior::None,
            0,
            None,
            true,
            false,
        );
        let empty: Vec<String> = Vec::new();
        a.last_advance = Instant::now() - std::time::Duration::from_secs(1);
        let mut c = ctx(Instant::now(), &empty);
        a.advance(&mut c); // frame 0 -> 1
        assert!(!a.finished());
        a.last_advance = Instant::now() - std::time::Duration::from_secs(1);
        a.advance(&mut c); // 1 -> finished
        assert!(a.finished());
    }

    #[test]
    fn paint_writes_current_frame_into_scene() {
        let (mut scene, node) = minimal_scene_with_animation(3, 1);
        let a = FrameAnimationAdapter::new(
            "x".into(),
            node,
            vec![make_frame(3, 1, 'A')],
            60,
            LoopBehavior::None,
            0,
            None,
            true,
            false,
        );
        a.paint(&mut scene);
        for x in 0..3 {
            assert_eq!(scene.buffer_of(node).unwrap().get(x, 0).unwrap().ch, 'A');
        }
    }
}
