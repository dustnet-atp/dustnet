//! Animation subsystem: per-kind animation objects that emit patches or
//! write their own buffers, called by the render loop's "advance time"
//! stage.
//!
//! The `Animation` trait is the architectural abstraction; per-kind
//! adapters live in `frame`, `wasm`, `tween`, `effect`, `transition`,
//! and `page_transition`. `runtime.rs` owns a `Box<dyn Animation>`
//! collection and ticks it.
//!
//! ## The two channels
//!
//! Per `docs/internals/compositor.md`:
//!
//! - **Patches**: structural/property scene changes (tween a property,
//!   flip a panel state). Observable through `Invalidation` and
//!   replayable via the patch log.
//! - **Buffer writes**: the subsystem has mutable access to its owning
//!   node's `CellBuffer` via the scene's kind-gated buffer accessors
//!   and writes cells directly; it returns `wrote_buffer: Some(node_id)`
//!   so the render loop can mark that rect for composition.
//!
//! A single `advance` call may produce both (rare), one, or neither
//! (steady-state: animation is waiting/paused).

use crate::compositor::layout::cell::CellBuffer;

/// The animation-preparation allocations a test can force to behave as
/// refused.
///
/// Preparing one frame animation admits several collections in sequence — the
/// frame vector, then per-frame descendants and placement snapshots — and each
/// refusal has to leave the scene untouched. Exhausting a real governor cannot
/// say which one refused, so it cannot show that the snapshot taken before a
/// failed frame layout is still restored.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AnimateAllocationSite {
    /// The frame buffer collection backing `FrameAnimationAdapter.frames`.
    FrameCollection,
    /// The panel-transition adapter collection, sized by the panel count.
    TransitionCollection,
    /// A WASM adapter's swap buffer, allocated at construction and again on
    /// every resize.
    WasmSwapBuffer,
    /// The authored identifiers and `after` chains copied out of the scene for
    /// every animation adapter, admitted as one payload lease.
    Payload,
    /// Storage for the page's build notices, admitted with the text it will
    /// hold. Refusing this costs the notices and not the page, which is the one
    /// behaviour here worth a test of its own.
    BuildNotices,
}

#[cfg(test)]
thread_local! {
    static REJECT_ANIMATE_ALLOCATION: std::cell::Cell<Option<AnimateAllocationSite>> =
        const { std::cell::Cell::new(None) };
}

/// Arms one animation allocation site to refuse, and disarms it on drop.
#[cfg(test)]
pub(crate) struct AnimateRejectionGuard;

#[cfg(test)]
impl AnimateRejectionGuard {
    pub(crate) fn at(site: AnimateAllocationSite) -> Self {
        REJECT_ANIMATE_ALLOCATION.with(|rejected| rejected.set(Some(site)));
        Self
    }
}

#[cfg(test)]
impl Drop for AnimateRejectionGuard {
    fn drop(&mut self) {
        REJECT_ANIMATE_ALLOCATION.with(|rejected| rejected.set(None));
    }
}

#[cfg(test)]
pub(crate) fn reject_animate_allocation(site: AnimateAllocationSite) -> bool {
    REJECT_ANIMATE_ALLOCATION.with(|rejected| rejected.get() == Some(site))
}

/// Compiled away in release builds.
#[cfg(not(test))]
pub(crate) fn reject_animate_allocation(_site: AnimateAllocationSite) -> bool {
    false
}
use crate::compositor::scene::{NodeId, Patch};

pub mod effect;
pub mod frame;
pub mod page_transition;
pub mod runtime;
pub mod transition;
pub mod tween;
pub mod wasm;

pub use effect::TextEffectAdapter;
pub use frame::FrameAnimationAdapter;
pub use page_transition::{PAGE_TRANSITION_ID, PageTransitionAdapter};
pub use runtime::{AnimState, AnimationRuntime};
pub use transition::TransitionAdapter;
pub use tween::TweenAdapter;
pub use wasm::WasmAnimationAdapter;

/// Fully allocated host-side state for one size-dependent adapter resize.
/// Preparing this value may fail; consuming it after layout commit does not
/// perform another host allocation.
#[doc(hidden)]
pub struct AnimationResizeCandidate {
    pub(crate) width: u16,
    pub(crate) height: u16,
    pub(crate) swap_buffer: CellBuffer,
    pub(crate) output_buffer: CellBuffer,
}

#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnimationResizeRejected;

/// The per-tick contract every animation kind implements.
///
/// Implementers own their subsystem state (tween `t`, frame index, WASM
/// VM, etc.) and never patch-encode it. `advance` returns scene-visible
/// effects only: `patches` for structural changes, `wrote_buffer` if
/// the call wrote into a node's buffer.
pub trait Animation {
    /// A stable identifier for this animation. Matches the AML `id=`
    /// attribute; also the key `after="..."` chains reference.
    fn id(&self) -> &str;

    /// Advance internal state by one tick. `ctx` carries scene-owned
    /// facilities the animation may need (time, viewport, finished
    /// dependency ids for chain resolution); all mutations to the scene
    /// go through the returned `AdvanceResult`, not through `ctx`.
    fn advance(&mut self, ctx: &mut AdvanceCtx) -> AdvanceResult;

    /// Scene-aware tick. Default delegates to `advance`. WASM animations
    /// override this to `mem::swap` the scene node's buffer into the
    /// host state before calling `tick` on the WASM module — realizing
    /// Invariant 2b with zero extra copies.
    fn advance_with_scene(
        &mut self,
        ctx: &mut AdvanceCtx,
        _scene: &mut crate::compositor::scene::Scene,
    ) -> AdvanceResult {
        self.advance(ctx)
    }

    /// `true` when the animation has reached its end and will produce no
    /// more effects. The render loop drops finished animations at the
    /// start of the next tick (retirement policy is author-declared;
    /// see compositor.md "Animation retirement").
    fn finished(&self) -> bool;

    /// Current lifecycle state. Required so the runtime can resolve
    /// chain dependencies (`after="other"`) and detect state changes
    /// (firing `animation-end` events).
    fn state(&self) -> runtime::AnimState;

    /// Whether this animation renders behind base content (z=-1). Used
    /// by the compositor to split foreground vs. background layers.
    fn background(&self) -> bool {
        false
    }

    /// Whether the global `f` action should seek this animation to its end.
    /// Background placement normally denotes ambient decoration, but finite
    /// background-layer adapters may override this (e.g. a cinematic drawn
    /// behind navigation chrome).
    fn fast_forwardable(&self) -> bool {
        !self.background()
    }

    /// Whether this animation needs the scene to paint its output after
    /// `advance` (via the adapter-specific `paint_into` or equivalent).
    /// Default: `true` — the runtime calls paint when `wrote_buffer` is
    /// `Some`. Override to `false` for patch-only animations (tween).
    fn paints_buffer(&self) -> bool {
        true
    }

    /// Paint the animation's current state into the scene. Called by the
    /// runtime after `advance` returns `wrote_buffer: Some(_)`. Default
    /// is a no-op for animations that produce only patches (tween).
    fn paint(&self, _scene: &mut crate::compositor::scene::Scene) {}

    /// Current linear-memory footprint of this animation's WASM instance in
    /// bytes; zero for non-WASM adapters. Summed for the `{mem}` status var.
    fn memory_bytes(&self) -> usize {
        0
    }

    /// Conservative byte bound for a notice that may be emitted by the next
    /// `advance` call. The runtime pre-admits this payload before mutating any
    /// animation state.
    fn next_notice_capacity_bound(&self) -> usize {
        0
    }

    /// Start (or restart) from the beginning. Called by
    /// `ActionKind::Animate` trigger.
    fn trigger_start(&mut self, _now: std::time::Instant) {}

    /// Force to Finished state. Called by `ActionKind::Stop` trigger.
    fn trigger_stop(&mut self) {}

    /// Seek to the animation's final rendered state and mark it
    /// finished. Returns any patch the runtime should apply (e.g.
    /// a tween's t=1.0 transform) and `wrote_buffer` for paint
    /// invalidation. Default: delegate to `trigger_stop` with no
    /// output.
    ///
    /// Distinct from `trigger_stop` because author-facing
    /// `ActionKind::Stop` historically means "halt where you are",
    /// while `skip` means "jump to the end". Called by the user's
    /// spacebar-to-skip shortcut via `AnimationRuntime::skip_all`;
    /// background animations (ambient decor like matrix rain) are
    /// filtered out there, so this only runs on foreground anims.
    fn skip(&mut self) -> AdvanceResult {
        self.trigger_stop();
        AdvanceResult::none()
    }

    /// Scene-aware fast-forward. Buffer-owning adapters such as WASM need
    /// direct scene access to render their terminal frame; other adapters
    /// retain the ordinary `skip` behavior.
    fn skip_with_scene(&mut self, _scene: &mut crate::compositor::scene::Scene) -> AdvanceResult {
        self.skip()
    }

    /// Allocate every host resource needed to adapt to post-layout geometry,
    /// without mutating adapter or guest state. Most adapters are size-free.
    fn prepare_resize(
        &self,
        _scene: &crate::compositor::scene::Scene,
    ) -> Result<Option<AnimationResizeCandidate>, AnimationResizeRejected> {
        Ok(None)
    }

    /// Consume a successfully prepared resize after the page projection has
    /// committed. Guest traps are committed adapter-stop outcomes; host
    /// pressure has already been excluded by `prepare_resize`.
    fn commit_resize(
        &mut self,
        _scene: &mut crate::compositor::scene::Scene,
        _candidate: AnimationResizeCandidate,
    ) {
    }
}

/// The context passed to `Animation::advance`: time + viewport
/// metrics + finished-dependency ids. Scene-buffer accessors and a
/// random-number source could land here if an adapter needs them.
pub struct AdvanceCtx<'a> {
    /// Current tick's instant. Animations compare against their own
    /// `last_advance` to decide whether to emit anything this tick.
    pub now: std::time::Instant,
    /// Viewport scroll offset in rows. Animations that pause when
    /// scrolled off-screen consult this.
    pub viewport_offset: u16,
    /// Viewport height in rows.
    pub viewport_height: u16,
    /// Ids of animations that have already reached `Finished` this
    /// tick. Used to resolve `after="other"` chain dependencies.
    pub finished_ids: &'a [String],
}

impl<'a> AdvanceCtx<'a> {
    pub fn new(
        now: std::time::Instant,
        viewport_offset: u16,
        viewport_height: u16,
        finished_ids: &'a [String],
    ) -> Self {
        Self {
            now,
            viewport_offset,
            viewport_height,
            finished_ids,
        }
    }
}

/// What an animation's `advance` call wants the render loop to do.
///
/// `patch` is fed to `PatchApplier`. `wrote_buffer` marks
/// the named node's rect for composition via `Invalidation.mark_composite`;
/// the buffer itself was already written by the animation during
/// `advance` using the kind-gated `Scene::*_buffer_mut` accessor.
#[derive(Debug, Default)]
pub struct AdvanceResult {
    pub patch: Option<Patch>,
    pub wrote_buffer: Option<NodeId>,
    /// A user-facing message to surface transiently (e.g. an effect stopped
    /// by a resource limit). Bubbled up through `TickResult.notices`.
    pub notice: Option<String>,
}

impl AdvanceResult {
    pub fn none() -> Self {
        Self::default()
    }

    pub fn with_buffer(node: NodeId) -> Self {
        Self {
            patch: None,
            wrote_buffer: Some(node),
            notice: None,
        }
    }

    pub fn with_patch(patch: Patch) -> Self {
        Self {
            patch: Some(patch),
            wrote_buffer: None,
            notice: None,
        }
    }

    pub fn is_noop(&self) -> bool {
        self.patch.is_none() && self.wrote_buffer.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trivial Animation implementation proving the trait compiles and
    /// dispatches as expected. Real kinds (frame, wasm, tween) live in
    /// sibling files.
    struct DummyAnim {
        id: String,
        done: bool,
    }

    impl Animation for DummyAnim {
        fn id(&self) -> &str {
            &self.id
        }

        fn advance(&mut self, _ctx: &mut AdvanceCtx) -> AdvanceResult {
            self.done = true;
            AdvanceResult::none()
        }

        fn finished(&self) -> bool {
            self.done
        }

        fn state(&self) -> runtime::AnimState {
            if self.done {
                runtime::AnimState::Finished
            } else {
                runtime::AnimState::Running
            }
        }
    }

    #[test]
    fn trait_dispatches_via_trait_object() {
        let mut a: Box<dyn Animation> = Box::new(DummyAnim {
            id: "d".into(),
            done: false,
        });
        assert!(!a.finished());
        let finished: Vec<String> = Vec::new();
        let mut ctx = AdvanceCtx::new(std::time::Instant::now(), 0, 24, &finished);
        let r = a.advance(&mut ctx);
        assert!(r.is_noop());
        assert!(a.finished());
    }

    #[test]
    fn advance_result_helpers() {
        assert!(AdvanceResult::none().is_noop());
        let changed = AdvanceResult::with_patch(Patch::SetScroll { offset: 1 });
        assert!(!changed.is_noop());
    }
}
