//! Text effects: per-tick character animation applied to a fixed string.
//!
//! Each `TextEffect` variant maps to a per-tick renderer:
//!
//! - **Typewriter** — reveal one character per `speed_ms`, left to right.
//! - **Reveal** — instant-show after delay; serves as a fade-in for now.
//! - **Scramble** — cycle through random glyphs, settling into the
//!   target character over time.
//! - **FadeIn** — same as Reveal without elaboration (color fading is
//!   out of scope; terminals are binary-present).
//! - **Glitch** — periodic corruption of random cells; settles.
//!
//! `[text-animate]` is parsed into `TextAnimateElement` and builds
//! into a `NodeKind::Text { source: TextAnimate }` scene node. The
//! static-text rendering lives in `kinds::text::layout_text_animate`;
//! this adapter writes the animated cells into the node's buffer.

use std::time::Instant;

use unicode_segmentation::UnicodeSegmentation;

use crate::compositor::layout::cell::{Cell, CellStyle};
use crate::compositor::scene::{NodeId, Scene};
use crate::parser::ast::TextEffect as TextEffectKind;

use super::runtime::AnimState;
use super::{AdvanceCtx, AdvanceResult, Animation};

pub struct TextEffectAdapter {
    id: String,
    node: NodeId,
    kind: TextEffectKind,
    content: String,
    speed_ms: u32,
    started_at: Option<Instant>,
    state: AnimState,
    /// Simple splitmix64 PRNG state for scramble/glitch variants.
    rng: u64,
}

impl TextEffectAdapter {
    pub fn new(
        id: String,
        node: NodeId,
        kind: TextEffectKind,
        content: String,
        speed_ms: u32,
    ) -> Self {
        // Use the content length as a cheap deterministic seed so tests
        // are reproducible.
        let rng = (content.len() as u64).wrapping_mul(0x9E3779B97F4A7C15);
        Self {
            id,
            node,
            kind,
            content,
            speed_ms: speed_ms.max(1),
            started_at: None,
            state: AnimState::Running,
            rng,
        }
    }

    /// Test-only: step the internal PRNG and return a glyph.
    /// Used by `rand_char_deterministic_from_seed` to prove that two
    /// adapters seeded identically produce the same stream.
    #[cfg(test)]
    fn rand_char(&mut self) -> char {
        self.rng = self.rng.wrapping_add(0x9E3779B97F4A7C15);
        let mut x = self.rng;
        x = (x ^ (x >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94D049BB133111EB);
        x ^= x >> 31;
        let glyph_pool = "!#$%&()*+-=<>?@[]^_{|}~";
        glyph_pool
            .chars()
            .nth((x as usize) % glyph_pool.len())
            .unwrap_or('?')
    }
}

impl Animation for TextEffectAdapter {
    fn id(&self) -> &str {
        &self.id
    }

    fn advance(&mut self, ctx: &mut AdvanceCtx) -> AdvanceResult {
        if matches!(self.state, AnimState::Finished) {
            return AdvanceResult::none();
        }
        if self.started_at.is_none() {
            self.started_at = Some(ctx.now);
        }
        // Compute how long since the effect began; each variant decides
        // its finish condition.
        let Some(started) = self.started_at else {
            return AdvanceResult::default();
        };
        let elapsed_ms = ctx.now.duration_since(started).as_millis() as u32;
        let total_chars = self.content.chars().count() as u32;
        match self.kind {
            TextEffectKind::Typewriter => {
                let revealed = (elapsed_ms / self.speed_ms).min(total_chars);
                if revealed == total_chars {
                    self.state = AnimState::Finished;
                }
            }
            TextEffectKind::Reveal | TextEffectKind::FadeIn => {
                if elapsed_ms >= self.speed_ms {
                    self.state = AnimState::Finished;
                }
            }
            TextEffectKind::Scramble => {
                // Settle in the same duration as typewriter.
                let settle_end = self.speed_ms * total_chars.max(1);
                if elapsed_ms >= settle_end {
                    self.state = AnimState::Finished;
                }
            }
            TextEffectKind::Glitch => {
                // Glitch is an infinite effect — caller triggers Stop.
            }
        }
        AdvanceResult::with_buffer(self.node)
    }

    fn finished(&self) -> bool {
        matches!(self.state, AnimState::Finished)
    }

    fn state(&self) -> AnimState {
        self.state
    }

    fn paint(&self, scene: &mut Scene) {
        let Some(dst) = scene.wasm_buffer_mut(self.node) else {
            return;
        };
        if dst.height == 0 {
            return;
        }
        let w = dst.width;
        let h = dst.height;
        // Clear destination first.
        for y in 0..h {
            for x in 0..w {
                dst.set(x, y, Cell::empty());
            }
        }
        let style = CellStyle::default();
        let elapsed_ms = self
            .started_at
            .map(|s| std::time::Instant::now().duration_since(s).as_millis() as u32)
            .unwrap_or(0);
        match self.kind {
            TextEffectKind::Typewriter => {
                let revealed = (elapsed_ms / self.speed_ms) as usize;
                let end = self
                    .content
                    .char_indices()
                    .nth(revealed)
                    .map_or(self.content.len(), |(offset, _)| offset);
                dst.put_str(0, 0, &self.content[..end], &style);
            }
            TextEffectKind::Reveal | TextEffectKind::FadeIn => {
                // Show full string once past speed_ms; empty before.
                if elapsed_ms >= self.speed_ms {
                    dst.put_str(0, 0, &self.content, &style);
                }
            }
            TextEffectKind::Scramble => {
                // Stable RNG seeded by content length; not ideal for
                // production but deterministic for tests. Real code uses
                // a rng stored on the adapter; we clone a local copy
                // here because paint is &self.
                let mut rng = self.rng;
                let per_char_settle_ms = self.speed_ms;
                let mut column = 0u16;
                for (idx, target) in self.content.graphemes(true).enumerate() {
                    let settle_at = per_char_settle_ms * (idx as u32 + 1);
                    let mut encoded = [0u8; 4];
                    let rendered = if elapsed_ms >= settle_at {
                        target
                    } else {
                        rng = rng.wrapping_add(0x9E3779B97F4A7C15);
                        let mut x = rng;
                        x = (x ^ (x >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
                        x = (x ^ (x >> 27)).wrapping_mul(0x94D049BB133111EB);
                        x ^= x >> 31;
                        let pool = "!#$%&()*+-=<>?@[]^_{|}~";
                        pool.chars()
                            .nth((x as usize) % pool.len())
                            .unwrap_or('?')
                            .encode_utf8(&mut encoded)
                    };
                    column = column.saturating_add(dst.put_str(column, 0, rendered, &style));
                    if column >= w {
                        break;
                    }
                }
            }
            TextEffectKind::Glitch => {
                // Draw the full content, then corrupt a handful of cells
                // based on elapsed time.
                dst.put_str(0, 0, &self.content, &style);
                let mut rng = self.rng.wrapping_mul(elapsed_ms as u64 | 1);
                let glitch_count = 2;
                for _ in 0..glitch_count {
                    rng = rng.wrapping_add(0x9E3779B97F4A7C15);
                    let mut x = rng;
                    x = (x ^ (x >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
                    x ^= x >> 27;
                    let idx = (x as usize) % self.content.chars().count().max(1);
                    let pool = "▓▒░█▌▐■□▬▪";
                    let ch = pool.chars().nth((x as usize) % pool.len()).unwrap_or('?');
                    dst.put_char(idx as u16, 0, ch, &style);
                }
            }
        }
    }

    fn trigger_start(&mut self, now: Instant) {
        self.state = AnimState::Running;
        self.started_at = Some(now);
    }

    fn trigger_stop(&mut self) {
        self.state = AnimState::Finished;
    }

    fn skip(&mut self) -> AdvanceResult {
        // `paint()` drives its render off `elapsed_ms = now -
        // started_at`; each variant has a settle threshold beyond
        // which it produces the complete final string. Backdating
        // `started_at` far into the past guarantees we're past every
        // variant's threshold without needing per-variant math.
        self.started_at = Some(
            Instant::now()
                .checked_sub(std::time::Duration::from_secs(3600))
                .unwrap_or_else(Instant::now),
        );
        self.state = AnimState::Finished;
        AdvanceResult::with_buffer(self.node)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compositor::scene;

    fn make_effect_scene(node_w: u16, node_h: u16) -> (Scene, NodeId) {
        let doc = {
            let src = r#"[page mode=document][animate id="fx" fps=30][/animate][/page]"#;
            let mut scanner = crate::scanner::Scanner::new(src.as_bytes()).unwrap();
            let tokens = scanner.scan_all().unwrap();
            crate::parser::parse(tokens).document.unwrap()
        };
        let mut scene = scene::build::from_document(&doc);
        let id = scene.find_by_aml_id("fx").unwrap();
        scene.allocate_buffer(id, node_w, node_h);
        (scene, id)
    }

    #[test]
    fn typewriter_reveals_characters_over_time() {
        let (mut scene, node) = make_effect_scene(20, 1);
        let mut eff = TextEffectAdapter::new(
            "t".into(),
            node,
            TextEffectKind::Typewriter,
            "HELLO".into(),
            100,
        );
        // Seed started_at via an initial advance.
        let start = Instant::now();
        let empty: Vec<String> = Vec::new();
        let mut c = AdvanceCtx::new(start, 0, 24, &empty);
        eff.advance(&mut c);
        eff.started_at = Some(start); // pin to start for deterministic paint
        eff.paint(&mut scene);
        // At t=0ms, no characters revealed yet.
        let row: String = (0..5)
            .map(|x| scene.buffer_of(node).unwrap().get(x, 0).unwrap().ch)
            .collect();
        assert_eq!(
            row, "     ",
            "typewriter at t=0 should be empty, got {row:?}"
        );

        // At t=250ms with speed_ms=100, 2 characters should be revealed.
        eff.started_at = Some(Instant::now() - std::time::Duration::from_millis(250));
        eff.paint(&mut scene);
        let row: String = (0..5)
            .map(|x| scene.buffer_of(node).unwrap().get(x, 0).unwrap().ch)
            .collect();
        // First two chars should be 'H' and 'E'.
        assert!(
            row.starts_with("HE"),
            "expected 'HE' prefix at t=250ms, got {row:?}"
        );
    }

    #[test]
    fn typewriter_finishes_when_all_revealed() {
        let (_scene, node) = make_effect_scene(10, 1);
        let mut eff = TextEffectAdapter::new(
            "t".into(),
            node,
            TextEffectKind::Typewriter,
            "AB".into(),
            50,
        );
        let start = Instant::now() - std::time::Duration::from_millis(200);
        eff.started_at = Some(start);
        let empty: Vec<String> = Vec::new();
        let mut c = AdvanceCtx::new(Instant::now(), 0, 24, &empty);
        eff.advance(&mut c);
        assert!(eff.finished(), "should finish once elapsed >= chars*speed");
    }

    #[test]
    fn reveal_finishes_after_speed_ms() {
        let (_scene, node) = make_effect_scene(10, 1);
        let mut eff =
            TextEffectAdapter::new("r".into(), node, TextEffectKind::Reveal, "hi".into(), 100);
        eff.started_at = Some(Instant::now() - std::time::Duration::from_millis(150));
        let empty: Vec<String> = Vec::new();
        let mut c = AdvanceCtx::new(Instant::now(), 0, 24, &empty);
        eff.advance(&mut c);
        assert!(eff.finished());
    }

    #[test]
    fn glitch_never_finishes_on_its_own() {
        let (_scene, node) = make_effect_scene(10, 1);
        let mut eff =
            TextEffectAdapter::new("g".into(), node, TextEffectKind::Glitch, "hello".into(), 50);
        eff.started_at = Some(Instant::now() - std::time::Duration::from_secs(60));
        let empty: Vec<String> = Vec::new();
        let mut c = AdvanceCtx::new(Instant::now(), 0, 24, &empty);
        eff.advance(&mut c);
        assert!(!eff.finished(), "glitch is perpetual until trigger_stop");
        eff.trigger_stop();
        assert!(eff.finished());
    }

    #[test]
    fn scramble_settles_into_target() {
        let (mut scene, node) = make_effect_scene(10, 1);
        let mut eff =
            TextEffectAdapter::new("s".into(), node, TextEffectKind::Scramble, "ABC".into(), 50);
        // Well past settlement time: all characters should land on target.
        eff.started_at = Some(Instant::now() - std::time::Duration::from_secs(10));
        eff.paint(&mut scene);
        let row: String = (0..3)
            .map(|x| scene.buffer_of(node).unwrap().get(x, 0).unwrap().ch)
            .collect();
        assert_eq!(row, "ABC", "scramble settled, got {row:?}");
    }

    #[test]
    fn text_effect_paint_uses_no_string_scratch() {
        let (mut scene, node) = make_effect_scene(10, 1);
        let mut effect = TextEffectAdapter::new(
            "scratch-free".into(),
            node,
            TextEffectKind::Scramble,
            "ÁBC".into(),
            50,
        );
        effect.started_at = Some(Instant::now() - std::time::Duration::from_secs(10));
        effect.paint(&mut scene);
        assert_eq!(
            scene
                .buffer_of(node)
                .unwrap()
                .get(0, 0)
                .unwrap()
                .grapheme
                .as_ref()
                .unwrap()
                .as_str(),
            "Á"
        );
    }

    #[test]
    fn rand_char_deterministic_from_seed() {
        let (_scene, node) = make_effect_scene(1, 1);
        let mut a = TextEffectAdapter::new(
            "a".into(),
            node,
            TextEffectKind::Scramble,
            "HELLO".into(),
            50,
        );
        let mut b = TextEffectAdapter::new(
            "b".into(),
            node,
            TextEffectKind::Scramble,
            "HELLO".into(),
            50,
        );
        assert_eq!(a.rand_char(), b.rand_char());
        assert_eq!(a.rand_char(), b.rand_char());
    }
}
