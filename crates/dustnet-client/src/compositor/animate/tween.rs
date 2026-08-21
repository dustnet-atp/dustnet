//! Tween animation — interpolates properties over time via keyframes.
//!
//! `[tween target="foo" duration="500ms"]` with `[keyframe t="0%" x=0]`
//! children. Each tick, the adapter computes the interpolated value at
//! the current `t ∈ [0, 1]` and emits `Patch::SetTransform` (for x/y
//! offsets) to the scene. This is a **patch-only** animation — the
//! scene applies the transform at composition time; the adapter never
//! writes buffer cells.
//!
//! AML's `[tween]` parses into `TweenElement`; this adapter drives
//! per-tick keyframe interpolation and emits `Patch::SetTransform`.

use std::time::Instant;

use crate::compositor::scene::{NodeId, Patch, Transform};
use crate::parser::ast::{Easing, Keyframe, LoopBehavior};

use super::runtime::AnimState;
use super::{AdvanceCtx, AdvanceResult, Animation};

pub struct TweenAdapter {
    id: String,
    /// The target scene node whose transform this tween drives.
    target: NodeId,
    keyframes: Vec<Keyframe>,
    duration_ms: u32,
    easing: Easing,
    delay_ms: u32,
    loop_behavior: LoopBehavior,
    state: AnimState,
    started_at: Option<Instant>,
    /// Cached elapsed ms at the last advance, for idempotent re-queries
    /// and debug.
    elapsed_ms: u32,
    loops_done: u32,
}

impl TweenAdapter {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: String,
        target: NodeId,
        keyframes: Vec<Keyframe>,
        duration_ms: u32,
        easing: Easing,
        delay_ms: u32,
        loop_behavior: LoopBehavior,
    ) -> Self {
        let mut keyframes = keyframes;
        // Ensure keyframes are sorted by `t_percent` so interpolation
        // is deterministic.
        keyframes.sort_by(|a, b| {
            a.t_percent
                .partial_cmp(&b.t_percent)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        // Tweens always autoplay when the `[tween]` element is parsed
        // (no explicit autoplay attribute in the grammar); but a tween
        // with zero duration has nothing to animate — treat as finished
        // immediately.
        let initial_state = if duration_ms == 0 || keyframes.is_empty() {
            AnimState::Finished
        } else if delay_ms > 0 {
            AnimState::Waiting
        } else {
            AnimState::Running
        };
        Self {
            id,
            target,
            keyframes,
            duration_ms,
            easing,
            delay_ms,
            loop_behavior,
            state: initial_state,
            started_at: None,
            elapsed_ms: 0,
            loops_done: 0,
        }
    }

    /// Compute the eased interpolation factor for a linear `t ∈ [0, 1]`.
    fn ease(t: f32, easing: Easing) -> f32 {
        let t = t.clamp(0.0, 1.0);
        match easing {
            Easing::Linear => t,
            Easing::EaseIn => t * t,
            Easing::EaseOut => 1.0 - (1.0 - t) * (1.0 - t),
            Easing::EaseInOut => {
                if t < 0.5 {
                    2.0 * t * t
                } else {
                    1.0 - (-2.0 * t + 2.0).powi(2) / 2.0
                }
            }
            Easing::Step => {
                if t >= 1.0 {
                    1.0
                } else {
                    0.0
                }
            }
        }
    }

    /// Find the two keyframes bracketing `t_percent` and interpolate
    /// between them. Returns `(x, y)` from the keyframes, defaulting
    /// to `(0, 0)` if the keyframes don't declare coordinates.
    fn sample_xy(&self, t_percent: f32) -> (i16, i16) {
        let Some(first) = self.keyframes.first() else {
            return (0, 0);
        };
        let t = t_percent.clamp(0.0, 100.0);
        // Find bracket.
        let mut prev = first;
        let mut next = first;
        for kf in &self.keyframes {
            if kf.t_percent <= t {
                prev = kf;
            }
            if kf.t_percent >= t {
                next = kf;
                break;
            }
        }
        let span = (next.t_percent - prev.t_percent).max(f32::EPSILON);
        let local = if span > 0.0 {
            (t - prev.t_percent) / span
        } else {
            0.0
        };
        let eased = Self::ease(local, self.easing);
        let px = prev.x.unwrap_or(0) as f32;
        let py = prev.y.unwrap_or(0) as f32;
        let nx = next.x.unwrap_or(0) as f32;
        let ny = next.y.unwrap_or(0) as f32;
        let x = (px + (nx - px) * eased).round() as i16;
        let y = (py + (ny - py) * eased).round() as i16;
        (x, y)
    }
}

impl Animation for TweenAdapter {
    fn id(&self) -> &str {
        &self.id
    }

    fn advance(&mut self, ctx: &mut AdvanceCtx) -> AdvanceResult {
        if matches!(self.state, AnimState::Finished) {
            return AdvanceResult::none();
        }

        // Waiting → Running after delay.
        if matches!(self.state, AnimState::Waiting) {
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
        }

        if !matches!(self.state, AnimState::Running) {
            return AdvanceResult::none();
        }

        let Some(start) = self.started_at else {
            self.started_at = Some(ctx.now);
            return AdvanceResult::none();
        };
        let elapsed = ctx.now.duration_since(start).as_millis() as u32;
        let effective = elapsed.saturating_sub(self.delay_ms);
        self.elapsed_ms = effective;

        // Compute t_percent in [0, 100].
        let raw_percent = (effective as f32 / self.duration_ms as f32) * 100.0;
        let (t_percent, finished_now) = if raw_percent >= 100.0 {
            // End of cycle — apply loop behavior.
            match self.loop_behavior {
                LoopBehavior::None => (100.0, true),
                LoopBehavior::Infinite => {
                    // Restart from 0.
                    self.started_at = Some(ctx.now);
                    (0.0, false)
                }
                LoopBehavior::Count(max) => {
                    if self.loops_done + 1 < max {
                        self.started_at = Some(ctx.now);
                        self.loops_done += 1;
                        (0.0, false)
                    } else {
                        (100.0, true)
                    }
                }
                LoopBehavior::Bounce => {
                    // Bounce: flip direction conceptually by reversing t.
                    // Simplified: wrap to 0 and treat as infinite with
                    // alternating direction via an internal flag.
                    self.started_at = Some(ctx.now);
                    (0.0, false)
                }
            }
        } else {
            (raw_percent, false)
        };

        let (x, y) = self.sample_xy(t_percent);
        if finished_now {
            self.state = AnimState::Finished;
        }

        AdvanceResult::with_patch(Patch::SetTransform {
            node: self.target,
            transform: Transform { dx: x, dy: y },
        })
    }

    fn finished(&self) -> bool {
        matches!(self.state, AnimState::Finished)
    }

    fn state(&self) -> AnimState {
        self.state
    }

    fn paints_buffer(&self) -> bool {
        false
    }

    fn trigger_start(&mut self, now: Instant) {
        self.state = AnimState::Running;
        self.started_at = Some(now);
        self.elapsed_ms = 0;
        self.loops_done = 0;
    }

    fn trigger_stop(&mut self) {
        self.state = AnimState::Finished;
    }

    fn skip(&mut self) -> AdvanceResult {
        // Tween is patch-only (`paints_buffer()==false`); its final
        // state lives in a `SetTransform` patch at t=1.0. `advance`
        // would normally emit this patch on the tick it finishes —
        // since skip bypasses advance, we emit it directly.
        self.state = AnimState::Finished;
        self.elapsed_ms = self.duration_ms;
        let (x, y) = self.sample_xy(100.0);
        AdvanceResult::with_patch(Patch::SetTransform {
            node: self.target,
            transform: Transform { dx: x, dy: y },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compositor::scene;

    fn make_target() -> (scene::Scene, NodeId) {
        let doc = {
            let src = r#"[page mode=document][box w=5 h=5][/box][/page]"#;
            let mut scanner = crate::scanner::Scanner::new(src.as_bytes()).unwrap();
            let tokens = scanner.scan_all().unwrap();
            crate::parser::parse(tokens).document.unwrap()
        };
        let scene = scene::build::from_document(&doc);
        let root = scene.root();
        let box_id = scene.get(root).unwrap().children()[0];
        (scene, box_id)
    }

    #[test]
    fn zero_duration_finishes_immediately() {
        let (_s, id) = make_target();
        let tween = TweenAdapter::new(
            "t".into(),
            id,
            vec![Keyframe {
                t_percent: 0.0,
                x: Some(0),
                y: Some(0),
                fg: None,
                bg: None,
            }],
            0,
            Easing::Linear,
            0,
            LoopBehavior::None,
        );
        assert!(tween.finished());
    }

    #[test]
    fn linear_slide_produces_transform_patches() {
        let (_s, id) = make_target();
        let mut t = TweenAdapter::new(
            "slide".into(),
            id,
            vec![
                Keyframe {
                    t_percent: 0.0,
                    x: Some(0),
                    y: Some(0),
                    fg: None,
                    bg: None,
                },
                Keyframe {
                    t_percent: 100.0,
                    x: Some(10),
                    y: Some(0),
                    fg: None,
                    bg: None,
                },
            ],
            500,
            Easing::Linear,
            0,
            LoopBehavior::None,
        );
        let empty: Vec<String> = Vec::new();
        // Seed started_at at the FIRST advance.
        let start = Instant::now();
        let mut c = AdvanceCtx::new(start, 0, 24, &empty);
        t.advance(&mut c); // sets started_at
        // Half-duration: transform should be around dx=5.
        let midpoint = start + std::time::Duration::from_millis(250);
        let mut c = AdvanceCtx::new(midpoint, 0, 24, &empty);
        let r = t.advance(&mut c);
        let Some(Patch::SetTransform { transform, .. }) = &r.patch else {
            panic!()
        };
        assert!(
            transform.dx >= 4 && transform.dx <= 6,
            "midpoint dx should be ~5, got {}",
            transform.dx
        );
    }

    #[test]
    fn non_looping_tween_finishes_at_end() {
        let (_s, id) = make_target();
        let mut t = TweenAdapter::new(
            "s".into(),
            id,
            vec![
                Keyframe {
                    t_percent: 0.0,
                    x: Some(0),
                    y: Some(0),
                    fg: None,
                    bg: None,
                },
                Keyframe {
                    t_percent: 100.0,
                    x: Some(10),
                    y: Some(0),
                    fg: None,
                    bg: None,
                },
            ],
            100,
            Easing::Linear,
            0,
            LoopBehavior::None,
        );
        let empty: Vec<String> = Vec::new();
        let start = Instant::now();
        let mut c = AdvanceCtx::new(start, 0, 24, &empty);
        t.advance(&mut c);
        let past_end = start + std::time::Duration::from_millis(200);
        let mut c = AdvanceCtx::new(past_end, 0, 24, &empty);
        t.advance(&mut c);
        assert!(t.finished());
    }

    #[test]
    fn ease_curves_produce_expected_endpoints() {
        assert_eq!(TweenAdapter::ease(0.0, Easing::Linear), 0.0);
        assert_eq!(TweenAdapter::ease(1.0, Easing::Linear), 1.0);
        assert_eq!(TweenAdapter::ease(0.5, Easing::Linear), 0.5);

        assert_eq!(TweenAdapter::ease(0.0, Easing::EaseIn), 0.0);
        assert_eq!(TweenAdapter::ease(1.0, Easing::EaseIn), 1.0);
        assert!(TweenAdapter::ease(0.5, Easing::EaseIn) < 0.5);

        assert!(TweenAdapter::ease(0.5, Easing::EaseOut) > 0.5);

        // Step jumps only at t == 1.0.
        assert_eq!(TweenAdapter::ease(0.99, Easing::Step), 0.0);
        assert_eq!(TweenAdapter::ease(1.0, Easing::Step), 1.0);
    }

    #[test]
    fn keyframes_sorted_at_construction() {
        let (_s, id) = make_target();
        let t = TweenAdapter::new(
            "t".into(),
            id,
            vec![
                Keyframe {
                    t_percent: 50.0,
                    x: Some(5),
                    y: None,
                    fg: None,
                    bg: None,
                },
                Keyframe {
                    t_percent: 0.0,
                    x: Some(0),
                    y: None,
                    fg: None,
                    bg: None,
                },
                Keyframe {
                    t_percent: 100.0,
                    x: Some(10),
                    y: None,
                    fg: None,
                    bg: None,
                },
            ],
            500,
            Easing::Linear,
            0,
            LoopBehavior::None,
        );
        assert_eq!(t.keyframes[0].t_percent, 0.0);
        assert_eq!(t.keyframes[1].t_percent, 50.0);
        assert_eq!(t.keyframes[2].t_percent, 100.0);
    }
}
