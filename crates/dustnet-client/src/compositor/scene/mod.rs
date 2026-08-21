//! Scene graph: the persistent display state as a tree of `Node`s.
//!
//! The scene is authoritative for every piece of mutable display
//! state — panel active state, details open/closed, focus, scroll,
//! per-node buffers, event bindings. `build::from_document` is the
//! only AST read; after that, every layout, composite, and patch
//! operation consumes the scene directly.

pub mod build;
pub mod events;
pub mod input;
pub mod invalidation;
pub mod node;
// Test-time only, as its own module docs say: the parity assertion is never
// called by production. Gating it makes that structural rather than a
// convention — and takes its `panic!` out of the production surface, which is
// what surfaced it.
#[cfg(test)]
pub mod parity;
pub mod patch;
pub mod tree;

pub use events::EventBinding;

#[cfg(test)]
mod tests;

pub use invalidation::{DirtyRegions, Invalidation};
pub use node::{
    Action, FlowData, FlowSource, KindTag, Node, NodeId, NodeKind, OverlayData, OverlaySource,
    TextContent, TextRun, TextSource, Transform,
};
pub use patch::{NodeTemplate, Patch, PatchApplier};
pub use tree::{Scene, ScrollState};
