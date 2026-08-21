//! Terminal viewer façade and lifecycle dispatcher.
//!
//! Runtime transport and presentation mechanics live in the private runner;
//! this module retains the stable public entry points and is the sole
//! production boundary allowed to drive the viewer reducer.

mod lifecycle;
mod presentation;
mod rendering;
mod runner;

pub use crate::compositor::terminal_lifecycle::{Terminal, detect_color_support};
pub use lifecycle::ViewerError;
pub use runner::{run_connected_viewer, run_viewer};

use std::collections::VecDeque;
use std::ops::Deref;

use crate::viewer::{ViewerEffect, ViewerEvent, ViewerModel};

/// Owns the reducer for production terminal execution. Consumers may inspect
/// snapshots through `Deref`, but mutation is available only by dispatching a
/// domain event back through this module.
pub(crate) struct ReducerPort {
    model: ViewerModel,
}

impl ReducerPort {
    pub(crate) fn new(model: ViewerModel) -> Self {
        Self { model }
    }
}

impl Deref for ReducerPort {
    type Target = ViewerModel;

    fn deref(&self) -> &Self::Target {
        &self.model
    }
}

mod reducer_ingress_sealed {
    pub trait Sealed {}

    impl Sealed for super::ViewerModel {}
    impl Sealed for super::ReducerPort {}
}

pub(crate) trait ReducerIngress: reducer_ingress_sealed::Sealed {
    fn model(&self) -> &ViewerModel;
    fn reduce_event(&mut self, event: ViewerEvent) -> Vec<ViewerEffect>;
}

impl ReducerIngress for ViewerModel {
    fn model(&self) -> &ViewerModel {
        self
    }

    fn reduce_event(&mut self, event: ViewerEvent) -> Vec<ViewerEffect> {
        self.reduce(event)
    }
}

impl ReducerIngress for ReducerPort {
    fn model(&self) -> &ViewerModel {
        &self.model
    }

    fn reduce_event(&mut self, event: ViewerEvent) -> Vec<ViewerEffect> {
        self.model.reduce(event)
    }
}

/// Reduce events in FIFO order and append every reducer-issued effect in the
/// exact order in which it was produced.
#[cfg(test)]
pub(crate) fn dispatch_reducer_events(
    model: &mut impl ReducerIngress,
    events: impl IntoIterator<Item = ViewerEvent>,
) -> VecDeque<ViewerEffect> {
    let mut pending = events.into_iter().collect::<VecDeque<_>>();
    let mut effects = VecDeque::new();
    while let Some(event) = pending.pop_front() {
        effects.extend(model.reduce_event(event));
    }
    effects
}

#[cfg(test)]
pub(crate) fn dispatch_event(
    model: &mut impl ReducerIngress,
    event: ViewerEvent,
) -> Vec<ViewerEffect> {
    dispatch_reducer_events(model, [event]).into()
}

/// Drive one or more domain events through the reducer and terminal runtime.
/// Effects and their completion events are processed in FIFO order. This is
/// the only production event loop that couples reduction to effect execution.
async fn dispatch_runtime_events(
    runtime: &mut runner::TerminalRuntime,
    model: &mut ReducerPort,
    events: impl IntoIterator<Item = ViewerEvent>,
) -> Result<(), ViewerError> {
    let mut pending_events = events.into_iter().collect::<VecDeque<_>>();
    let mut pending_effects = VecDeque::new();

    while !pending_events.is_empty() || !pending_effects.is_empty() {
        while let Some(event) = pending_events.pop_front() {
            pending_effects.extend(model.reduce_event(event));
        }
        if let Some(effect) = pending_effects.pop_front() {
            let completions = runtime.execute(effect, model.model()).await?;
            let mut completion_effects = Vec::new();
            for event in completions {
                completion_effects.extend(model.reduce_event(event));
            }
            // Completion effects belong to the operation that just finished,
            // so preserve their FIFO order ahead of unrelated queued work.
            for effect in completion_effects.into_iter().rev() {
                pending_effects.push_front(effect);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod architecture_tests {
    #[test]
    fn reducer_is_only_called_by_the_terminal_dispatcher() {
        let forbidden = [
            ("runner", include_str!("terminal/runner.rs")),
            ("navigation", include_str!("terminal/navigation.rs")),
            ("events", include_str!("terminal/events.rs")),
            ("presentation", include_str!("terminal/presentation.rs")),
            ("rendering", include_str!("terminal/rendering.rs")),
            ("lifecycle", include_str!("terminal/lifecycle.rs")),
            ("animation runtime", include_str!("animate/runtime.rs")),
        ];
        for (name, source) in forbidden {
            assert!(
                !source.contains(".reduce("),
                "{name} bypasses the ordered terminal dispatcher"
            );
            assert!(
                !source.contains("&mut ViewerModel"),
                "{name} accepts mutable reducer ownership"
            );
            assert!(
                !source.contains("&mut LifecycleModel"),
                "{name} accepts mutable reducer ownership through the old alias"
            );
        }
    }

    // The animation runtime's isolation from the network and the reducer is
    // no longer checked here. This test searched `animate/runtime.rs` for the
    // absence of the strings `AtpClient`, `fetch_resource` and
    // `load_wasm_network`, which breaks on a rename and passes on any
    // equivalent bypass — and covered only that one file, so reaching the
    // network through something the module imports went unseen.
    //
    // `MODULE_ISOLATION` in `tools/allocation-audit` replaces it with a check
    // over the crate-internal `use` graph: every file under
    // `compositor/animate`, everything it imports, and everything those
    // modules declare as submodules, must not reach `crate::client`,
    // `crate::transport`, `crate::viewer` or `crate::session_store`. Gaining
    // access requires an import, so a rename cannot defeat it and a mention in
    // a comment cannot trip it. `make ci` runs it via `ci-tools`.

    #[test]
    fn viewer_is_the_only_lifecycle_identity_issuer() {
        let lib = include_str!("../lib.rs");
        let client = include_str!("../client.rs");
        let viewer = include_str!("../viewer.rs");

        assert!(
            !lib.contains("pub mod client"),
            "raw transport module is public"
        );
        assert!(
            !lib.contains("AtpClient as Client"),
            "raw transport client is re-exported"
        );
        for forbidden in [
            "next_generation",
            "active_request_id",
            "fetch_owned",
            "submit_owned",
            "subscribe_owned",
            "fetch_resource_owned",
            "activate_cached_page",
        ] {
            assert!(
                !client.contains(forbidden),
                "transport retains compatibility authority through {forbidden}"
            );
        }
        for constructor in [
            "pub(crate) const fn new(scope: PageScope, request_id: RequestId)",
            "pub(crate) fn from_placed_index(index: usize)",
        ] {
            assert!(
                viewer.contains(constructor),
                "viewer identity constructor is not crate-private: {constructor}"
            );
        }
    }

    #[test]
    fn terminal_loop_cannot_mutate_the_active_scene() {
        let source = include_str!("terminal/runner.rs");
        let loop_start = source
            .find("async fn viewer_main_loop(")
            .expect("viewer loop must exist");
        let loop_end = source[loop_start..]
            .find("// ─── Panel relayout and transitions")
            .map(|offset| loop_start + offset)
            .expect("viewer loop boundary must exist");
        let viewer_loop = &source[loop_start..loop_end];
        for forbidden in [
            "PatchApplier::apply",
            "runtime.page =",
            "runtime.page.anim_rt.skip_all",
            "runtime.event_dispatcher.fire",
            "install_page_transition(",
            "relayout_panels_for(",
        ] {
            assert!(
                !viewer_loop.contains(forbidden),
                "viewer loop bypasses the reducer effect runner via {forbidden}"
            );
        }
    }
}
