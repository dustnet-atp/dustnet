//! Scene-owned event bindings (from `[on]` elements).
//!
//! `EventBinding`s are collected at `build_scene` time from
//! `Element::On` ancillary elements and stored in
//! `Scene.event_bindings`. The runtime `EventDispatcher` reads this
//! list live whenever a dispatch batch is prepared, so scene mutations that add or
//! remove bindings take effect immediately.
//!
//! ## Future: per-node triggers
//!
//! A deeper redesign would attach `EventTrigger`s to individual
//! scene nodes and have `scene.fire_event(event, source)` walk the
//! source node's triggers + scene-root triggers. The flat fixed table on
//! `Scene` is simpler and covers every
//! binding shape currently in use.

pub const MAX_EVENT_BINDINGS: usize = crate::parser::MAX_ON_BINDINGS;
pub type EventBindings = arrayvec::ArrayVec<EventBinding, MAX_EVENT_BINDINGS>;

/// A parsed `[on]` event binding. Collected at `build_scene` time
/// from `Element::On` ancillary elements.
#[derive(Debug)]
pub struct EventBinding {
    pub event: crate::parser::ast::EventKind,
    pub source: Option<String>,
    pub action: crate::parser::ast::ActionKind,
    pub target: String,
    pub to: Option<String>,
    pub delay_ms: u32,
}
