//! The compositor — scene graph, layout, composition, presentation.
//!
//! Implements the display subsystem described in `docs/internals/compositor.md`.
//! `scene` owns the persistent tree and its mutation API; `layout`
//! produces `Placement`s from scene state; `animate` runs per-kind
//! animation adapters over scene node buffers; `composite` stacks
//! layers; `present` emits ANSI; `terminal` hosts the viewer loop.

pub mod animate;
pub mod composite;
pub mod layout;
pub mod panels;
pub mod present;
pub mod scene;
pub mod terminal;
mod terminal_lifecycle;
/// WASM runtime host for animations (`WasmInstance` + `WasmRuntime`).
pub mod wasm;
