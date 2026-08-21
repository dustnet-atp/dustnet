//! The `Scene` type: a fallibly reserved generational node arena plus a root pointer.
//!
//! All mutation of scene state goes through three channels:
//! `build::from_document` for initial construction, the kind-gated
//! `*_buffer_mut` accessors for subsystem buffer writes, and
//! `PatchApplier` for structural/property changes. External code
//! gets read-only views via the `pub fn` accessors.

use std::sync::Arc;

use slotmap::{Key, KeyData};

use crate::compositor::layout::Rect;
use crate::compositor::layout::cell::CellBuffer;

use crate::compositor::layout::engine::Placement;
use crate::resource::{BudgetLease, ResourceCategory, ResourceGovernor};

use super::invalidation::Invalidation;
use super::node::{KindTag, Node, NodeBuilder, NodeId, NodeKind, OverlayData, OverlaySource};

#[cfg(test)]
thread_local! {
    static REJECT_NODE_ARENA_ALLOCATION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
pub(crate) fn reject_next_node_arena_allocation() {
    REJECT_NODE_ARENA_ALLOCATION.with(|reject| reject.set(true));
}

/// The scene allocations a test can force to behave as refused.
///
/// The arena already had a one-shot flag; a transaction that admits five
/// collections at once needs to name *which* of them refused, because the
/// property under test is that the other four leave nothing behind.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SceneAllocationSite {
    /// The relayout journal, admitted as one transaction before any patch is
    /// applied so that a rollback cannot itself allocate.
    RelayoutJournal,
    /// The layout-invalidation entry list sized by the node count.
    Invalidation,
    /// One node's own cell buffer, admitted against `SceneCells`.
    NodeBuffer,
    /// The parent/child relation topology admitted for the whole scene.
    RelationTopology,
}

#[cfg(test)]
thread_local! {
    static REJECT_SCENE_ALLOCATION: std::cell::Cell<Option<SceneAllocationSite>> =
        const { std::cell::Cell::new(None) };
}

/// Arms one scene allocation site to refuse, and disarms it on drop.
#[cfg(test)]
pub(crate) struct SceneRejectionGuard;

#[cfg(test)]
impl SceneRejectionGuard {
    pub(crate) fn at(site: SceneAllocationSite) -> Self {
        REJECT_SCENE_ALLOCATION.with(|rejected| rejected.set(Some(site)));
        Self
    }
}

#[cfg(test)]
impl Drop for SceneRejectionGuard {
    fn drop(&mut self) {
        REJECT_SCENE_ALLOCATION.with(|rejected| rejected.set(None));
    }
}

#[cfg(test)]
pub(super) fn reject_scene_allocation(site: SceneAllocationSite) -> bool {
    REJECT_SCENE_ALLOCATION.with(|rejected| rejected.get() == Some(site))
}

/// Compiled away in release builds.
#[cfg(not(test))]
pub(super) fn reject_scene_allocation(_site: SceneAllocationSite) -> bool {
    false
}

#[derive(Debug)]
struct SceneNodeSlot {
    version: u32,
    node: Option<Node>,
}

/// Fallibly reserved generational arena for scene nodes.
#[derive(Debug, Default)]
pub(super) struct SceneNodes {
    slots: Vec<SceneNodeSlot>,
    free: Vec<u32>,
    limit: usize,
    len: usize,
}

impl SceneNodes {
    pub(super) fn requested_bytes(capacity: usize) -> Option<usize> {
        capacity
            .checked_mul(std::mem::size_of::<SceneNodeSlot>())?
            .checked_add(capacity.checked_mul(std::mem::size_of::<u32>())?)
    }

    pub(super) fn try_with_capacity(
        capacity: usize,
    ) -> Result<Self, std::collections::TryReserveError> {
        #[cfg(test)]
        REJECT_NODE_ARENA_ALLOCATION.with(|reject| {
            if reject.replace(false) {
                let mut probe = Vec::<u8>::new();
                return probe.try_reserve(usize::MAX);
            }
            Ok(())
        })?;
        let mut slots = Vec::new();
        slots.try_reserve_exact(capacity)?;
        let mut free = Vec::new();
        free.try_reserve_exact(capacity)?;
        let limit = slots.capacity().min(free.capacity());
        Ok(Self {
            slots,
            free,
            limit,
            len: 0,
        })
    }

    fn key(index: u32, version: u32) -> NodeId {
        NodeId::from(KeyData::from_ffi(
            (u64::from(version) << 32) | u64::from(index),
        ))
    }

    fn parts(id: NodeId) -> (usize, u32) {
        let value = id.data().as_ffi();
        (value as u32 as usize, (value >> 32) as u32)
    }

    pub(super) fn insert_with_key(&mut self, build: impl FnOnce(NodeId) -> Node) -> Option<NodeId> {
        let index = if let Some(index) = self.free.pop() {
            index as usize
        } else {
            if self.slots.len() == self.limit {
                return None;
            }
            self.slots.push(SceneNodeSlot {
                version: 1,
                node: None,
            });
            self.slots.len() - 1
        };
        let slot = self.slots.get_mut(index)?;
        let id = Self::key(index as u32, slot.version);
        slot.node = Some(build(id));
        self.len += 1;
        Some(id)
    }

    pub(super) fn get(&self, id: NodeId) -> Option<&Node> {
        let (index, version) = Self::parts(id);
        let slot = self.slots.get(index)?;
        (slot.version == version)
            .then_some(slot.node.as_ref())
            .flatten()
    }

    pub(super) fn get_mut(&mut self, id: NodeId) -> Option<&mut Node> {
        let (index, version) = Self::parts(id);
        let slot = self.slots.get_mut(index)?;
        (slot.version == version)
            .then_some(slot.node.as_mut())
            .flatten()
    }

    pub(super) fn contains_key(&self, id: NodeId) -> bool {
        self.get(id).is_some()
    }
    pub(super) fn len(&self) -> usize {
        self.len
    }
    #[cfg(test)]
    pub(super) fn capacity(&self) -> usize {
        self.slots.capacity()
    }
    pub(super) fn has_spare_capacity(&self) -> bool {
        !self.free.is_empty() || self.slots.len() < self.limit
    }

    pub(super) fn try_reserve(
        &mut self,
        additional: usize,
    ) -> Result<(), std::collections::TryReserveError> {
        let needed_new = additional.saturating_sub(self.free.len());
        let Some(required_limit) = self.slots.len().checked_add(needed_new) else {
            let mut probe = Vec::<u8>::new();
            return probe.try_reserve(usize::MAX);
        };
        self.slots.try_reserve(needed_new)?;
        let free_additional = required_limit.saturating_sub(self.free.len());
        self.free.try_reserve(free_additional)?;
        self.limit = self.slots.capacity().min(self.free.capacity());
        Ok(())
    }

    pub(super) fn remove(&mut self, id: NodeId) -> Option<Node> {
        let (index, version) = Self::parts(id);
        let slot = self.slots.get_mut(index)?;
        if slot.version != version {
            return None;
        }
        let node = slot.node.take()?;
        slot.version = slot.version.wrapping_add(2) | 1;
        self.free.push(index as u32);
        self.len -= 1;
        Some(node)
    }

    pub(super) fn values(&self) -> impl Iterator<Item = &Node> {
        self.slots.iter().filter_map(|slot| slot.node.as_ref())
    }

    pub(super) fn values_mut(&mut self) -> impl Iterator<Item = &mut Node> {
        self.slots.iter_mut().filter_map(|slot| slot.node.as_mut())
    }

    #[cfg(test)]
    pub(super) fn iter(&self) -> impl Iterator<Item = (NodeId, &Node)> {
        self.slots.iter().enumerate().filter_map(|(index, slot)| {
            slot.node
                .as_ref()
                .map(|node| (Self::key(index as u32, slot.version), node))
        })
    }

    pub(super) fn retained_bytes(&self) -> Option<usize> {
        self.slots
            .capacity()
            .checked_mul(std::mem::size_of::<SceneNodeSlot>())?
            .checked_add(
                self.free
                    .capacity()
                    .checked_mul(std::mem::size_of::<u32>())?,
            )
    }
}

#[cfg(test)]
mod node_arena_tests {
    use super::*;

    #[test]
    fn node_arena_reuses_slots_without_growth_and_rejects_stale_keys() {
        let mut nodes = SceneNodes::try_with_capacity(2).unwrap();
        let retained = nodes.retained_bytes().unwrap();
        let capacity = nodes.capacity();
        let mut ids = Vec::new();
        while nodes.has_spare_capacity() {
            ids.push(
                nodes
                    .insert_with_key(|id| NodeBuilder::new(NodeKind::Root).finish(id))
                    .unwrap(),
            );
        }
        let first = ids[0];
        assert!(nodes.contains_key(first));
        let free_capacity = nodes.free.capacity();
        nodes.remove(first).unwrap();
        assert!(!nodes.contains_key(first));
        assert_eq!(nodes.free.capacity(), free_capacity);

        let second = nodes
            .insert_with_key(|id| NodeBuilder::new(NodeKind::Root).finish(id))
            .unwrap();
        assert_ne!(second, first);
        assert!(nodes.contains_key(second));
        assert_eq!(nodes.capacity(), capacity);
        assert_eq!(nodes.free.capacity(), free_capacity);
        assert_eq!(nodes.retained_bytes(), Some(retained));
    }
}

/// Aggregate cells owned by one active remote scene, across every node buffer.
pub const MAX_SCENE_CELLS: usize = 1_048_576;

/// The persistent root of all display state for a page.
///
/// One scene per page. Navigation replaces the scene wholesale; in-page
/// state changes (panel toggles, focus, scroll) mutate it in place via
/// `PatchApplier`.
#[derive(Debug)]
pub struct Scene {
    pub(super) root: NodeId,
    pub(super) nodes: SceneNodes,
    /// Allocation-only index: authored strings remain owned solely by Nodes.
    pub(super) aml_id_index: AmlIdIndex,
    /// Per-tick invalidation set. Subsystems write to it; the render loop
    /// reads, processes, and clears it. See `invalidation::Invalidation`.
    pub invalidation: Invalidation,
    /// Currently-focused node, if any. Authoritative for hit-testing
    /// and event-trigger source resolution; mirrored by
    /// `ViewportState.focus_index` (which drives the render outline via
    /// `FocusableElement` layout data that has no scene home).
    pub(super) focus: Option<NodeId>,
    /// Viewport scroll offset in rows.
    pub(super) scroll: ScrollState,
    /// `[on]` event bindings collected at `build_scene` time. Read live
    /// by the runtime `EventDispatcher` on every `fire()`.
    pub(crate) event_bindings: super::events::EventBindings,
    /// Page mode — `Document` (scrollable) or `Screen { cols, rows }` (fixed).
    /// Captured at `build_scene` time from `doc.page.mode`; `layout_scene`
    /// reads it to size the root buffer.
    pub page_mode: crate::parser::ast::PageMode,
    /// Raw (pre-resolved) default foreground color. Resolved against the
    /// terminal's color-support level at layout time.
    pub default_fg: Option<crate::color::Color>,
    /// Raw (pre-resolved) default background color.
    pub default_bg: Option<crate::color::Color>,
    /// Transition kind requested for navigation to this page (if any).
    pub transition: Option<crate::parser::ast::TransitionKind>,
    pub transition_duration_ms: u32,
    /// Page title.
    pub title: Option<Arc<str>>,
    /// Sticky failure set before any allocation that would cross the page-wide
    /// cell budget. The client replaces such a page with its own error page.
    pub(super) resource_error: bool,
    /// Shared page budget used by every remotely influenced node buffer.
    /// Trusted/local scenes leave this unset.
    pub(super) governor: Option<ResourceGovernor>,
    /// Move-based rollback state for patch-driven relayout. Old node buffers
    /// retain their existing leases here while candidate buffers are built.
    pub(super) relayout_journal: Option<RelayoutJournal>,
    /// Stable compositor-owned topology for the at-most-one page transition.
    /// The node remains dormant between transitions so Start/teardown never
    /// grow the live arena or root child collection.
    pub(super) page_transition_overlay: Option<NodeId>,
    /// Exact page-lifetime admission for the complete node arena, including
    /// the dormant synthetic transition node. Its root edge is accounted by
    /// `_node_relation_topology_lease`.
    pub(super) _node_topology_lease: Option<BudgetLease>,
    /// Exact retained backing for the allocation-only AML-id index and every
    /// node child list. The disposable builder admits a conservative peak,
    /// then reconciles this lease to allocator-selected Vec capacities.
    pub(super) _node_relation_topology_lease: Option<BudgetLease>,
}

#[derive(Debug)]
struct NodeLayoutCheckpoint {
    id: NodeId,
    placement: Placement,
    focusable_screen_rect: Option<Rect>,
}

#[derive(Debug)]
struct NodeBufferCheckpoint {
    id: NodeId,
    buffer: Option<CellBuffer>,
}

#[derive(Debug)]
pub(super) struct RelayoutJournal {
    layouts: Vec<NodeLayoutCheckpoint>,
    buffers: Vec<NodeBufferCheckpoint>,
    invalidation_layout: Vec<NodeId>,
    invalidation_composite: Vec<Rect>,
    invalidation_present: Vec<Rect>,
    resource_error: bool,
    _lease: Option<BudgetLease>,
}

/// Scroll state carried on the scene.
#[derive(Debug, Default, Clone, Copy)]
pub struct ScrollState {
    pub offset: u16,
}

impl Scene {
    /// Whether `id` belongs to the active branch of every ancestor panel.
    /// Inactive state nodes retain their last placement for transactional
    /// rollback, so placement alone cannot decide animation visibility.
    pub(crate) fn is_in_active_panel_state(&self, id: NodeId) -> bool {
        let mut child = id;
        while let Some(parent) = self.nodes.get(child).and_then(Node::parent) {
            if let Some(parent_node) = self.nodes.get(parent)
                && let NodeKind::Panel { active, .. } = parent_node.kind()
                && *active != child
            {
                return false;
            }
            child = parent;
        }
        true
    }

    pub(super) fn has_governed_nodes(&self) -> bool {
        self._node_topology_lease.is_some()
    }
    pub(super) fn has_governed_node_relations(&self) -> bool {
        self._node_relation_topology_lease.is_some()
    }

    pub(super) fn node_relation_capacity_bytes(&self) -> Option<usize> {
        self.aml_id_index
            .capacity()
            .checked_add(self.nodes.values().try_fold(0usize, |total, node| {
                total.checked_add(node.children.capacity())
            })?)?
            .checked_mul(std::mem::size_of::<NodeId>())
    }

    pub(super) fn reconcile_node_relation_lease(&mut self) -> bool {
        let Some(actual) = self.node_relation_capacity_bytes() else {
            return false;
        };
        self._node_relation_topology_lease
            .as_mut()
            .is_none_or(|lease| lease.try_resize_with_cost(actual, actual).is_ok())
    }

    #[cfg(test)]
    pub(super) fn node_relation_topology_stats(&self) -> (usize, usize, usize, usize) {
        let child_len = self.nodes.values().map(|node| node.children.len()).sum();
        let child_capacity = self
            .nodes
            .values()
            .map(|node| node.children.capacity())
            .sum();
        (
            self.aml_id_index.len(),
            self.aml_id_index.capacity(),
            child_len,
            child_capacity,
        )
    }

    #[cfg(test)]
    pub(crate) fn node_relation_admission_bytes(&self) -> usize {
        self._node_relation_topology_lease
            .as_ref()
            .map_or(0, BudgetLease::amount)
    }

    #[cfg(test)]
    pub(crate) fn node_topology_capacity_bytes(&self) -> Option<usize> {
        self.nodes.retained_bytes()
    }

    #[cfg(test)]
    pub(crate) fn node_topology_admission_bytes(&self) -> usize {
        self._node_topology_lease
            .as_ref()
            .map_or(0, BudgetLease::amount)
    }

    #[cfg(test)]
    pub(crate) fn layout_invalidation_admission_bytes(&self) -> usize {
        self.invalidation.layout.retained_bytes()
    }

    #[cfg(test)]
    pub(super) fn relayout_journal_retained_bytes(&self) -> Option<usize> {
        self.relayout_journal
            .as_ref()
            .map(|journal| journal._lease.as_ref().map_or(0, BudgetLease::byte_cost))
    }

    #[cfg(test)]
    pub(super) fn relayout_journal_capacity_bytes(&self) -> Option<usize> {
        let journal = self.relayout_journal.as_ref()?;
        journal
            .layouts
            .capacity()
            .checked_mul(std::mem::size_of::<NodeLayoutCheckpoint>())
            .and_then(|bytes| {
                journal
                    .buffers
                    .capacity()
                    .checked_mul(std::mem::size_of::<NodeBufferCheckpoint>())
                    .and_then(|more| bytes.checked_add(more))
            })
            .and_then(|bytes| {
                journal
                    .invalidation_layout
                    .capacity()
                    .checked_mul(std::mem::size_of::<NodeId>())
                    .and_then(|more| bytes.checked_add(more))
            })
            .and_then(|bytes| {
                journal
                    .invalidation_composite
                    .capacity()
                    .checked_mul(std::mem::size_of::<Rect>())
                    .and_then(|more| bytes.checked_add(more))
            })
            .and_then(|bytes| {
                journal
                    .invalidation_present
                    .capacity()
                    .checked_mul(std::mem::size_of::<Rect>())
                    .and_then(|more| bytes.checked_add(more))
            })
    }

    #[cfg(test)]
    pub(super) fn relayout_journal_buffer_state(&self) -> Option<(usize, usize)> {
        self.relayout_journal
            .as_ref()
            .map(|journal| (journal.buffers.len(), journal.buffers.capacity()))
    }

    /// Begin a property-relayout transaction. The journal is fully admitted
    /// before callers apply the authored patch, so a later rollback cannot
    /// itself allocate.
    pub(crate) fn begin_relayout_transaction(&mut self) -> bool {
        if self.relayout_journal.is_some() {
            return false;
        }
        if reject_scene_allocation(SceneAllocationSite::RelayoutJournal) {
            return false;
        }
        let nodes = self.nodes.len();
        let layout_invalidations = self.invalidation.layout.len();
        let composite_invalidations = self.invalidation.composite.as_slice().len();
        let present_invalidations = self.invalidation.present.as_slice().len();
        let requested = nodes
            .checked_mul(std::mem::size_of::<NodeLayoutCheckpoint>())
            .and_then(|bytes| {
                nodes
                    .checked_mul(std::mem::size_of::<NodeBufferCheckpoint>())
                    .and_then(|more| bytes.checked_add(more))
            })
            .and_then(|bytes| {
                layout_invalidations
                    .checked_mul(std::mem::size_of::<NodeId>())
                    .and_then(|more| bytes.checked_add(more))
            })
            .and_then(|bytes| {
                composite_invalidations
                    .checked_mul(std::mem::size_of::<Rect>())
                    .and_then(|more| bytes.checked_add(more))
            })
            .and_then(|bytes| {
                present_invalidations
                    .checked_mul(std::mem::size_of::<Rect>())
                    .and_then(|more| bytes.checked_add(more))
            });
        let Some(requested) = requested else {
            return false;
        };
        let mut lease = match (&self.governor, requested) {
            (_, 0) | (None, _) => None,
            (Some(governor), bytes) => {
                let Ok(lease) = governor.reserve(ResourceCategory::RemoteCollections, bytes) else {
                    return false;
                };
                Some(lease)
            }
        };
        let mut layouts = Vec::new();
        let mut buffers = Vec::new();
        let mut invalidation_layout = Vec::new();
        let mut invalidation_composite = Vec::new();
        let mut invalidation_present = Vec::new();
        if layouts.try_reserve_exact(nodes).is_err()
            || buffers.try_reserve_exact(nodes).is_err()
            || invalidation_layout
                .try_reserve_exact(layout_invalidations)
                .is_err()
            || invalidation_composite
                .try_reserve_exact(composite_invalidations)
                .is_err()
            || invalidation_present
                .try_reserve_exact(present_invalidations)
                .is_err()
        {
            return false;
        }
        invalidation_layout.extend(self.invalidation.layout.iter().copied());
        invalidation_composite.extend_from_slice(self.invalidation.composite.as_slice());
        invalidation_present.extend_from_slice(self.invalidation.present.as_slice());
        let retained = layouts
            .capacity()
            .checked_mul(std::mem::size_of::<NodeLayoutCheckpoint>())
            .and_then(|bytes| {
                buffers
                    .capacity()
                    .checked_mul(std::mem::size_of::<NodeBufferCheckpoint>())
                    .and_then(|more| bytes.checked_add(more))
            })
            .and_then(|bytes| {
                invalidation_layout
                    .capacity()
                    .checked_mul(std::mem::size_of::<NodeId>())
                    .and_then(|more| bytes.checked_add(more))
            })
            .and_then(|bytes| {
                invalidation_composite
                    .capacity()
                    .checked_mul(std::mem::size_of::<Rect>())
                    .and_then(|more| bytes.checked_add(more))
            })
            .and_then(|bytes| {
                invalidation_present
                    .capacity()
                    .checked_mul(std::mem::size_of::<Rect>())
                    .and_then(|more| bytes.checked_add(more))
            });
        let Some(retained) = retained else {
            return false;
        };
        if let Some(lease) = lease.as_mut()
            && lease.try_resize_with_cost(retained, retained).is_err()
        {
            return false;
        }
        self.relayout_journal = Some(RelayoutJournal {
            layouts,
            buffers,
            invalidation_layout,
            invalidation_composite,
            invalidation_present,
            resource_error: self.resource_error,
            _lease: lease,
        });
        true
    }

    pub(crate) fn commit_relayout_transaction(&mut self) {
        self.relayout_journal = None;
    }

    pub(crate) fn rollback_relayout_transaction(&mut self) {
        let Some(journal) = self.relayout_journal.take() else {
            return;
        };
        for checkpoint in journal.buffers {
            if let Some(node) = self.nodes.get_mut(checkpoint.id) {
                node.buffer = checkpoint.buffer;
            }
        }
        for checkpoint in journal.layouts {
            if let Some(node) = self.nodes.get_mut(checkpoint.id) {
                node.placement = checkpoint.placement;
                node.focusable_screen_rect = checkpoint.focusable_screen_rect;
            }
        }
        self.invalidation.clear();
        let restored_layout = self.invalidation.layout.extend(journal.invalidation_layout);
        for rect in journal.invalidation_composite {
            self.invalidation.composite.add(rect);
        }
        for rect in journal.invalidation_present {
            self.invalidation.present.add(rect);
        }
        self.resource_error = journal.resource_error || !restored_layout;
    }

    fn checkpoint_layout(&mut self, id: NodeId) {
        let Some(journal) = self.relayout_journal.as_mut() else {
            return;
        };
        if journal.layouts.iter().any(|entry| entry.id == id) {
            return;
        }
        if let Some(node) = self.nodes.get(id) {
            journal.layouts.push(NodeLayoutCheckpoint {
                id,
                placement: node.placement,
                focusable_screen_rect: node.focusable_screen_rect,
            });
        }
    }

    fn checkpoint_buffer(&mut self, id: NodeId) {
        if !self.nodes.contains_key(id) {
            return;
        }
        let Some(journal) = self.relayout_journal.as_mut() else {
            return;
        };
        if journal.buffers.iter().any(|entry| entry.id == id) {
            return;
        }
        let buffer = self.nodes.get_mut(id).and_then(|node| node.buffer.take());
        journal.buffers.push(NodeBufferCheckpoint { id, buffer });
    }

    /// Give an in-place subsystem write a candidate buffer while retaining the
    /// live buffer in the active relayout journal. Layout allocation normally
    /// stages buffers through `allocate_buffer`; animation painting needs this
    /// explicit path when dimensions did not change.
    pub(crate) fn stage_buffer_for_relayout(&mut self, id: NodeId) -> bool {
        if self.relayout_journal.is_none()
            || self
                .relayout_journal
                .as_ref()
                .is_some_and(|journal| journal.buffers.iter().any(|entry| entry.id == id))
        {
            return true;
        }
        let candidate = match self.nodes.get(id).and_then(|node| node.buffer.as_ref()) {
            Some(buffer) => match self.governor.as_ref() {
                Some(governor) => buffer
                    .try_clone_governed(governor, ResourceCategory::SceneCells)
                    .ok(),
                None => buffer.try_clone().ok(),
            },
            None => None,
        };
        if self.nodes.get(id).is_some_and(|node| node.buffer.is_some()) && candidate.is_none() {
            self.resource_error = true;
            return false;
        }
        self.checkpoint_buffer(id);
        if let Some(candidate) = candidate
            && let Some(node) = self.nodes.get_mut(id)
        {
            node.buffer = Some(candidate);
        }
        true
    }
    pub fn root(&self) -> NodeId {
        self.root
    }

    pub fn get(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(id)
    }

    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn buffer_cell_count(&self) -> usize {
        self.nodes
            .values()
            .filter_map(|node| node.buffer.as_ref())
            .map(CellBuffer::cell_count)
            .sum()
    }

    /// Exact allocated capacity of the remotely influenced payload the live
    /// scene retains: every string, the vectors that hold them, and event
    /// bindings. This is what the `AstStrings` admission charges.
    pub fn retained_string_capacity(&self) -> usize {
        let nodes = self.nodes.values().fold(0usize, |total, node| {
            total.saturating_add(node.retained_string_capacity())
        });
        let bindings = self.event_bindings.iter().fold(0usize, |total, binding| {
            total
                .saturating_add(binding.source.as_ref().map_or(0, String::capacity))
                .saturating_add(binding.target.capacity())
                .saturating_add(binding.to.as_ref().map_or(0, String::capacity))
        });
        nodes
            .saturating_add(bindings)
            .saturating_add(self.title.as_ref().map_or(0, |title| title.len()))
    }

    pub fn resource_limit_exceeded(&self) -> bool {
        self.resource_error
    }

    pub(crate) fn record_resource_error(&mut self) {
        self.resource_error = true;
    }

    /// Shared governor for fallible, remotely influenced scene work.
    pub(crate) fn resource_governor(&self) -> Option<ResourceGovernor> {
        self.governor.clone()
    }

    #[cfg(test)]
    pub(crate) fn shares_budget_with(&self, governor: &ResourceGovernor) -> bool {
        self.governor
            .as_ref()
            .is_some_and(|owned| owned.shares_budget_with(governor))
    }

    fn can_replace_buffer(&self, id: NodeId, new_cells: usize) -> bool {
        let old_cells = self
            .nodes
            .get(id)
            .and_then(|node| node.buffer.as_ref())
            .map_or(0, CellBuffer::cell_count);
        self.buffer_cell_count()
            .saturating_sub(old_cells)
            .saturating_add(new_cells)
            <= MAX_SCENE_CELLS
    }

    /// Find the `[details]` node at document-order `index` (0-based).
    /// Matches the indexing scheme used by `FocusAction::ToggleDetails`,
    /// which comes from the focusables collector's tree walk.
    pub fn find_details_by_index(&self, index: usize) -> Option<NodeId> {
        use super::node::{FlowSource, NodeKind};
        let mut seen = 0usize;
        for n in self.iter_tree_order() {
            if let NodeKind::Flow(d) = n.kind()
                && matches!(d.source, FlowSource::Details)
            {
                if seen == index {
                    return Some(n.id());
                }
                seen += 1;
            }
        }
        None
    }

    pub fn find_by_aml_id(&self, aml_id: &str) -> Option<NodeId> {
        self.aml_id_index.iter().rev().copied().find(|id| {
            self.nodes
                .get(*id)
                .is_some_and(|node| node.aml_id() == Some(aml_id))
        })
    }

    pub fn event_bindings(&self) -> &[super::events::EventBinding] {
        &self.event_bindings
    }

    pub fn focus(&self) -> Option<NodeId> {
        self.focus
    }

    pub fn scroll(&self) -> ScrollState {
        self.scroll
    }

    /// Read-only borrow of a node's buffer, regardless of kind. Used by
    /// composition, which doesn't care which subsystem owns the buffer —
    /// it just needs to blit it onto the output.
    pub fn buffer_of(&self, id: NodeId) -> Option<&CellBuffer> {
        self.nodes.get(id).and_then(|n| n.buffer.as_ref())
    }

    // ─── Kind-gated buffer-write accessors ──────────────────────
    //
    // These are the concrete realization of Invariant 2b (one subsystem
    // per buffer). Each returns `Some` only when the node's `kind` matches
    // the calling subsystem's expected kind — a subsystem calling the
    // wrong accessor gets `None` rather than a write into the wrong
    // buffer. `node.buffer` itself is `pub(super)`, so outside code cannot
    // bypass the gate via direct field access (enforced by Rust visibility).
    // Every other mutable Node field is also `pub(super)`; the only write
    // paths from outside the module are `PatchApplier` and these accessors.

    /// Mutable buffer access for the layout subsystem: every kind the
    /// layout pass writes into directly. Returns `Some` only when the
    /// node's kind is layout-owned AND a buffer has been allocated;
    /// `None` otherwise (including "layout-owned kind but no buffer
    /// yet," which is the starting state for most kinds today).
    ///
    /// The set below matches the target-kind list in
    /// the per-node-buffer migration — every "visible" kind except
    /// those whose writes belong to other subsystems (`Animation` →
    /// `wasm_buffer_mut`, `LiveRegion` → `live_buffer_mut`, `Panel` →
    /// `panel_buffer_mut`). `Root` is structural only and never owns
    /// pixels. `Border` is not a `NodeKind` — borders are the parent's
    /// chrome per the plan's Open-Questions recommendation (a).
    pub fn layout_buffer_mut(&mut self, id: NodeId) -> Option<&mut CellBuffer> {
        let node = self.nodes.get_mut(id)?;
        match &node.kind {
            NodeKind::Flow(_)
            | NodeKind::Row(_)
            | NodeKind::Absolute(_)
            | NodeKind::Text(_)
            | NodeKind::Hr(_)
            | NodeKind::Spacer { .. }
            | NodeKind::Link(_)
            | NodeKind::Button(_)
            | NodeKind::Input(_)
            | NodeKind::Select(_)
            | NodeKind::OptionLeaf(_)
            | NodeKind::Table
            | NodeKind::Tr
            | NodeKind::Th(_)
            | NodeKind::Td(_) => node.buffer.as_mut(),
            NodeKind::Root
            | NodeKind::Panel { .. }
            | NodeKind::Animation(_)
            | NodeKind::LiveRegion(_)
            | NodeKind::Overlay(_) => None,
        }
    }

    /// Mutable buffer access for the WASM runtime. Returns `Some` only
    /// for `NodeKind::Animation` (frame or WASM — both are animation
    /// subsystem territory).
    pub fn wasm_buffer_mut(&mut self, id: NodeId) -> Option<&mut CellBuffer> {
        let node = self.nodes.get_mut(id)?;
        match &node.kind {
            NodeKind::Animation(_) => node.buffer.as_mut(),
            _ => None,
        }
    }

    /// Mutable buffer access for the live-region subscription subsystem.
    pub fn live_buffer_mut(&mut self, id: NodeId) -> Option<&mut CellBuffer> {
        let node = self.nodes.get_mut(id)?;
        match &node.kind {
            NodeKind::LiveRegion(_) => node.buffer.as_mut(),
            _ => None,
        }
    }

    /// Mutable buffer access for the panel compositor — a panel's buffer
    /// holds the composed active-state content.
    pub fn panel_buffer_mut(&mut self, id: NodeId) -> Option<&mut CellBuffer> {
        let node = self.nodes.get_mut(id)?;
        match &node.kind {
            NodeKind::Panel { .. } => node.buffer.as_mut(),
            _ => None,
        }
    }

    /// Mutable buffer access for the overlay owner (e.g.,
    /// `PageTransitionAdapter`). Overlays are system-synthesized nodes
    /// whose buffers are written by their creator each tick; the
    /// compositor walk reads them via `node.buffer()`.
    pub fn overlay_buffer_mut(&mut self, id: NodeId) -> Option<&mut CellBuffer> {
        let node = self.nodes.get_mut(id)?;
        match &node.kind {
            NodeKind::Overlay(_) => node.buffer.as_mut(),
            _ => None,
        }
    }

    /// Allocate (or reallocate) a node's buffer to the given dimensions.
    /// Used during scene hydration after layout runs — the layout pass
    /// produces rects, the hydrate pass installs buffers of matching size.
    ///
    /// `pub(crate)` because only intra-crate code (the layout→scene
    /// bridge) should be installing buffers; external callers go through
    /// the kind-gated `*_buffer_mut` accessors for writes. Any future
    /// `PatchApplier` path that needs to resize a buffer routes through
    /// here.
    pub(crate) fn allocate_buffer(&mut self, id: NodeId, width: u16, height: u16) {
        let width = width.max(1);
        let height = height.max(1);
        if reject_scene_allocation(SceneAllocationSite::NodeBuffer) {
            self.checkpoint_buffer(id);
            self.resource_error = true;
            return;
        }
        // Move the live buffer into the rollback journal before attempting
        // candidate allocation. If admission fails, layout must not continue
        // painting into the previous page's buffer.
        self.checkpoint_buffer(id);
        let buffer = match self.governor.as_ref() {
            Some(governor) => {
                CellBuffer::try_new_governed(width, height, governor, ResourceCategory::SceneCells)
                    .ok()
            }
            None => CellBuffer::try_new(width, height).ok(),
        };
        let Some(buffer) = buffer else {
            self.resource_error = true;
            return;
        };
        if !self.can_replace_buffer(id, buffer.cell_count()) {
            self.resource_error = true;
            return;
        }
        if let Some(node) = self.nodes.get_mut(id) {
            // Opaque buffers: cells start as solid-space with no bg, which
            // is what compositor.rs calls "transparent" when composited —
            // empty cells pass through to lower layers. A populated cell
            // (from set_cell / blit) becomes opaque.
            node.buffer = Some(buffer);
        }
    }

    /// Ensure a subsystem-owned buffer has the requested dimensions while
    /// retaining pixels in the overlapping region.
    ///
    /// Animation and live-region buffers carry runtime state across unrelated
    /// panel relayouts, so hydration must not clear them unconditionally.
    pub(crate) fn ensure_buffer(&mut self, id: NodeId, width: u16, height: u16) {
        let width = width.max(1);
        let height = height.max(1);
        let needs_resize = self
            .nodes
            .get(id)
            .and_then(|node| node.buffer.as_ref())
            .map(|buffer| buffer.width != width || buffer.height != height)
            .unwrap_or(true);
        if needs_resize {
            let Ok(new_cells) = CellBuffer::checked_cell_count(width, height) else {
                self.resource_error = true;
                return;
            };
            if !self.can_replace_buffer(id, new_cells) {
                self.resource_error = true;
                return;
            }
            if !self.stage_buffer_for_relayout(id) {
                return;
            }
            if let Some(buffer) = self.nodes.get_mut(id).and_then(|node| node.buffer.as_mut()) {
                if buffer.try_resize_preserving(width, height).is_err() {
                    self.resource_error = true;
                }
            } else {
                self.allocate_buffer(id, width, height);
            }
        }
    }

    /// Insert a system-synthesized `Overlay` node at the scene root and
    /// return its `NodeId`. The buffer is allocated eagerly at the
    /// requested viewport size and marked opaque — overlays need to
    /// fully paint every cell they cover (a page-transition blend
    /// can't leak the old underlying scene through gaps).
    ///
    /// `z` controls the stacking order of multiple simultaneous overlays
    /// (lower z paints first). Page transitions use `i16::MAX` to guarantee
    /// they're the topmost layer.
    ///
    /// `rect` is the screen-absolute rectangle the overlay occupies; the
    /// composite walk reads `placement.rect` to position the blit. Typical
    /// callers pass `Rect::new(0, 0, viewport_w, viewport_h)`.
    ///
    /// Distinct from `PatchApplier::apply` — overlays are not author-
    /// declared structural changes, so they bypass the patch log. The
    /// caller is responsible for marking `invalidation.composite` for
    /// the overlay's rect on insertion and on every subsequent buffer
    /// write; this method seeds that with a mark for `rect`.
    pub fn insert_overlay(&mut self, z: i16, source: OverlaySource, rect: Rect) -> NodeId {
        if !self.nodes.has_spare_capacity()
            && !self.has_governed_nodes()
            && self.nodes.try_reserve(1).is_err()
        {
            self.resource_error = true;
            return NodeId::default();
        }
        if !self.nodes.has_spare_capacity()
            || self.has_governed_node_relations()
                && self
                    .nodes
                    .get(self.root)
                    .is_none_or(|root| root.children.len() == root.children.capacity())
        {
            self.resource_error = true;
            return NodeId::default();
        }
        self.insert_overlay_inner(z, source, rect, None)
    }

    /// Prepare one dormant page-transition node while this Scene is still a
    /// disposable page candidate. Failure is reported through the ordinary
    /// candidate-preparation boundary; no active Scene topology is changed.
    pub(crate) fn prepare_page_transition_overlay(&mut self) -> bool {
        if self.page_transition_overlay.is_some() {
            return true;
        }

        if self
            .nodes
            .get(self.root)
            .is_none_or(|root| root.children.len() == root.children.capacity())
            || !self.nodes.has_spare_capacity()
        {
            return false;
        }

        let kind = NodeKind::Overlay(OverlayData {
            source: OverlaySource::PageTransition,
        });
        let Some(overlay_id) = self
            .nodes
            .insert_with_key(|id| NodeBuilder::new(kind).finish(id))
        else {
            return false;
        };
        if let Some(overlay) = self.nodes.get_mut(overlay_id) {
            overlay.parent = Some(self.root);
            overlay.visible = false;
        }
        let Some(root) = self.nodes.get_mut(self.root) else {
            return false;
        };
        root.children.push(overlay_id);
        self.page_transition_overlay = Some(overlay_id);
        true
    }

    pub(crate) fn can_activate_page_transition_overlay(&self, cells: usize) -> bool {
        self.page_transition_overlay
            .and_then(|id| self.nodes.get(id).map(|node| (id, node)))
            .is_some_and(|(id, node)| node.buffer.is_none() && self.can_replace_buffer(id, cells))
    }

    pub(crate) fn page_transition_overlay_slot(&self) -> Option<NodeId> {
        self.page_transition_overlay
    }

    #[cfg(test)]
    pub(crate) fn page_transition_topology_admission_bytes(&self) -> usize {
        self._node_topology_lease
            .as_ref()
            .map_or(0, BudgetLease::amount)
    }

    /// Activate the prebuilt slot. All fallible allocation must have produced
    /// `buffer` before this allocation-free commit step.
    pub(crate) fn activate_page_transition_overlay(
        &mut self,
        rect: Rect,
        buffer: CellBuffer,
    ) -> Option<NodeId> {
        let id = self.page_transition_overlay?;
        if !self.can_activate_page_transition_overlay(buffer.cell_count()) {
            return None;
        }
        let node = self.nodes.get_mut(id)?;
        node.placement = Placement {
            rect,
            flow_advance: 0,
            bbox: rect,
        };
        node.z_index = i16::MAX;
        node.buffer = Some(buffer);
        node.visible = true;
        self.invalidation.mark_composite_bounded(rect);
        Some(id)
    }

    fn insert_overlay_inner(
        &mut self,
        z: i16,
        source: OverlaySource,
        rect: Rect,
        lease: Option<BudgetLease>,
    ) -> NodeId {
        let kind = NodeKind::Overlay(OverlayData { source });
        let Some(overlay_id) = self
            .nodes
            .insert_with_key(|id| NodeBuilder::new(kind).finish(id))
        else {
            self.resource_error = true;
            return NodeId::default();
        };

        // Populate placement + buffer + z_index. Field access is legal
        // here because `tree.rs` is inside the `scene` module where
        // `Node` fields are `pub(super)`.
        let overlay_buffer = match (lease, self.governor.as_ref()) {
            (Some(lease), _) if lease.category() == ResourceCategory::SceneCells => {
                CellBuffer::try_new_opaque_with_lease(rect.w.max(1), rect.h.max(1), lease).ok()
            }
            (Some(_), _) => None,
            (None, Some(governor)) => CellBuffer::try_new_opaque_governed(
                rect.w.max(1),
                rect.h.max(1),
                governor,
                ResourceCategory::SceneCells,
            )
            .ok(),
            (None, None) => CellBuffer::try_new_opaque(rect.w.max(1), rect.h.max(1)).ok(),
        };
        if overlay_buffer.is_none()
            || !self.can_replace_buffer(
                overlay_id,
                overlay_buffer.as_ref().map_or(0, CellBuffer::cell_count),
            )
        {
            self.resource_error = true;
        }
        if let Some(node) = self.nodes.get_mut(overlay_id) {
            node.placement = Placement {
                rect,
                flow_advance: 0,
                bbox: rect,
            };
            node.z_index = z;
            node.buffer = overlay_buffer;
        }

        // Attach as a child of the root so tree-order iteration reaches
        // the overlay. We take the root parent pointer + child-list edit
        // separately to avoid a double borrow.
        let root_id = self.root;
        if let Some(overlay) = self.nodes.get_mut(overlay_id) {
            overlay.parent = Some(root_id);
        }
        if let Some(root) = self.nodes.get_mut(root_id) {
            root.children.push(overlay_id);
        }

        self.invalidation.mark_composite(rect);
        overlay_id
    }

    /// Remove a node from the scene. Intended for compositor-owned
    /// synthesized nodes (overlays) that the creator manages directly;
    /// AML-backed nodes go through `PatchApplier::apply(Patch::Remove)`
    /// which validates more invariants.
    ///
    /// Detaches from parent's child list, drops the node, clears any
    /// `aml_id_index` entry, marks composite invalidation for the
    /// node's last-known rect.
    pub fn remove_overlay(&mut self, id: NodeId) {
        // Only overlays are removable via this path — guards against
        // accidental use on AML-backed nodes.
        let Some(node) = self.nodes.get(id) else {
            return;
        };
        if !matches!(node.kind(), NodeKind::Overlay(_)) {
            return;
        }
        let rect = node.placement().rect;
        let parent = node.parent();

        if let Some(pid) = parent
            && let Some(parent_node) = self.nodes.get_mut(pid)
        {
            parent_node.children.retain(|&c| c != id);
        }
        self.nodes.remove(id);
        self.invalidation.mark_composite(rect);
    }

    /// Locate the single compositor-owned page-transition overlay without a
    /// traversal stack allocation.
    pub(crate) fn page_transition_overlay(&self) -> Option<NodeId> {
        self.page_transition_overlay.filter(|id| {
            self.nodes
                .get(*id)
                .is_some_and(|node| node.visible && node.buffer.is_some())
        })
    }

    /// Detach the compositor-owned transition overlay without allocating or
    /// touching generic layout invalidation. Callers pair this with a full
    /// compositor/presentation invalidation when cancelling or completing the
    /// transition.
    pub(crate) fn remove_page_transition_overlay(&mut self, id: NodeId) {
        if self.page_transition_overlay != Some(id) {
            return;
        }
        let Some(node) = self.nodes.get_mut(id) else {
            return;
        };
        let rect = node.placement.rect;
        node.buffer = None;
        node.visible = false;
        node.z_index = 0;
        node.placement = Placement::default();
        self.invalidation.mark_composite_bounded(rect);
    }

    /// Remove a node's buffer. Used when a region changes size
    /// (reallocation) or disappears. Called by `PatchApplier` on
    /// structural removes.
    #[allow(dead_code)]
    pub(crate) fn clear_buffer(&mut self, id: NodeId) {
        if let Some(node) = self.nodes.get_mut(id) {
            node.buffer = None;
        }
    }

    /// Update a node's placement. Called by the layout pass
    /// (`layout_scene` and its kind helpers) as each node is placed,
    /// so that downstream consumers (hit-testing, animation rect
    /// queries, invalidation bboxes, the placed/sticky/focusable
    /// derive-walks) see real on-screen coordinates instead of the
    /// default `(0, 0, 0, 0)`.
    pub(crate) fn update_placement(
        &mut self,
        id: NodeId,
        placement: crate::compositor::layout::engine::Placement,
    ) {
        let Some(old) = self.nodes.get(id).map(|node| node.placement) else {
            return;
        };
        if old == placement {
            return;
        }
        self.checkpoint_layout(id);
        if let Some(node) = self.nodes.get_mut(id) {
            node.placement = placement;
        }
        self.invalidation.mark_composite(old.bbox);
        self.invalidation.mark_composite(placement.bbox);
    }

    /// Update a node's focusable surface rect. Called by the layout
    /// pass for focusable nodes whose focusable surface differs from
    /// their placement (Details summaries, inline Links inside wrapped
    /// Text). Block focusables (Buttons, Inputs, Selects) may still
    /// call this with their placement.rect for uniformity — downstream
    /// derive-walks read this field rather than reconstructing from
    /// placement + kind.
    pub(crate) fn update_focusable_rect(&mut self, id: NodeId, rect: Rect) {
        self.checkpoint_layout(id);
        if let Some(node) = self.nodes.get_mut(id) {
            node.focusable_screen_rect = Some(rect);
        }
    }

    /// Iterator over every node that is currently a placed region
    /// (Panel, Animation, LiveRegion) with a non-empty rect. Returns
    /// the synthesized `PlacedElement` — downstream consumers filter
    /// further by kind via `PlacedElement::is_panel()` etc.
    ///
    /// This is the scene-authoritative replacement for
    /// `LayoutResult.placed`: after layout runs, placement rects live
    /// on their nodes and derive directly from a tree walk.
    pub fn iter_placed(
        &self,
    ) -> impl Iterator<Item = crate::compositor::layout::engine::PlacedElement> + '_ {
        use crate::compositor::layout::engine::{PlacedElement, PlacedKind};
        self.iter_tree_order().filter_map(|n| {
            let rect = n.placement().rect;
            if rect.is_empty() {
                return None;
            }
            let id = n.aml_id().unwrap_or("").to_string();
            match n.kind() {
                NodeKind::Panel { .. } => Some(PlacedElement {
                    id,
                    kind: PlacedKind::Panel,
                    rect,
                }),
                NodeKind::Animation(data) => Some(PlacedElement {
                    id,
                    kind: PlacedKind::Animation {
                        background: data.background,
                    },
                    rect,
                }),
                NodeKind::LiveRegion(data) => Some(PlacedElement {
                    id,
                    kind: PlacedKind::Live {
                        endpoint: data.endpoint.clone(),
                        scroll: data.scroll,
                        buffer: data.buffer,
                        delta: data.delta,
                    },
                    rect,
                }),
                _ => None,
            }
        })
    }

    /// Count placed-region records and conservatively bound the string
    /// capacity their synthesized `PlacedElement`s will clone. This walks the
    /// scene without constructing those records, so callers can admit all
    /// remotely influenced storage before allocation.
    ///
    /// When `include_unplaced` is true, all panel/animation/live nodes are
    /// included. Runtime relayout uses that mode before scene mutation because
    /// a previously hidden node may become placed during the layout drain.
    pub(crate) fn placed_storage_requirements(
        &self,
        include_unplaced: bool,
    ) -> Option<(usize, usize)> {
        self.iter_tree_order()
            .try_fold((0usize, 0usize), |(count, string_bytes), node| {
                if !include_unplaced && node.placement().rect.is_empty() {
                    return Some((count, string_bytes));
                }
                let endpoint_capacity = match node.kind() {
                    NodeKind::LiveRegion(data) => data.endpoint.capacity(),
                    NodeKind::Panel { .. } | NodeKind::Animation(_) => 0,
                    _ => return Some((count, string_bytes)),
                };
                let id_capacity = node.aml_id.as_ref().map_or(0, String::capacity);
                Some((
                    count.checked_add(1)?,
                    string_bytes
                        .checked_add(id_capacity)?
                        .checked_add(endpoint_capacity)?,
                ))
            })
    }

    /// Iterator over sticky regions derived from scene state. A Flow
    /// node with `FlowData.sticky == Some(position)` and a non-zero
    /// placement height contributes one region.
    pub fn iter_sticky(
        &self,
    ) -> impl Iterator<Item = crate::compositor::layout::engine::StickyRegion> + '_ {
        use crate::compositor::layout::engine::StickyRegion;
        self.iter_tree_order().filter_map(|n| {
            if let NodeKind::Flow(data) = n.kind()
                && let Some(pos) = data.sticky
            {
                let r = n.placement().rect;
                if r.h > 0 {
                    return Some(StickyRegion {
                        position: pos,
                        y: r.y,
                        h: r.h,
                    });
                }
            }
            None
        })
    }

    /// Iterator over every focusable node and its on-screen rect, in
    /// tree order. Nodes whose layout pass hasn't written a focusable
    /// rect yet are skipped.
    pub fn iter_focusable_rects(&self) -> impl Iterator<Item = (NodeId, Rect)> + '_ {
        self.iter_tree_order().filter_map(|n| {
            if n.focusable() {
                n.focusable_screen_rect().map(|r| (n.id(), r))
            } else {
                None
            }
        })
    }

    /// Walk the tree in depth-first, child-order from the root.
    ///
    /// The parity assertion relies on this producing the same order as
    /// `enumerate_node_bearing` against the source document — i.e. the
    /// order in which `build_scene` attached children.
    pub fn iter_tree_order(&self) -> TreeOrderIter<'_> {
        TreeOrderIter::new(self, self.root)
    }

    /// Walk only the node-bearing scene content under `root`. Equivalent to
    /// `iter_tree_order` starting at a subtree, excluding the root itself
    /// only when requested.
    pub fn iter_subtree(&self, root: NodeId) -> TreeOrderIter<'_> {
        TreeOrderIter::new(self, root)
    }

    /// Produce a human-readable tree dump. Used by the `--dump-scene`
    /// CLI and by snapshot tests. Deterministic for a given scene.
    pub fn debug_dump(&self) -> String {
        let mut out = String::new();
        self.dump_node(self.root, 0, &mut out);
        out
    }

    fn dump_node(&self, id: NodeId, depth: usize, out: &mut String) {
        let Some(node) = self.nodes.get(id) else {
            return;
        };
        for _ in 0..depth {
            out.push_str("  ");
        }
        out.push_str(&format_node_line(node));
        out.push('\n');
        for &child in &node.children {
            self.dump_node(child, depth + 1, out);
        }
    }
}

pub type AmlIdIndex = Vec<NodeId>;

fn format_node_line(node: &Node) -> String {
    let mut s = format!("{:?}", node.kind_tag());
    if let Some(aml) = node.aml_id() {
        s.push_str(&format!(" #{aml}"));
    }
    match node.kind() {
        NodeKind::Panel { active, .. } => {
            s.push_str(&format!(" active={:?}", active));
        }
        NodeKind::Text(tc) => {
            let flat: String = tc.runs.iter().map(|r| r.text.as_str()).collect();
            let trimmed: String = flat.chars().take(40).collect();
            s.push_str(&format!(" text={:?}", trimmed));
        }
        NodeKind::Animation(ad) => {
            if let Some(src) = &ad.src {
                s.push_str(&format!(" src={:?}", src));
            } else {
                s.push_str(&format!(" frames={}", ad.frames.len()));
            }
            if ad.background {
                s.push_str(" background");
            }
        }
        NodeKind::LiveRegion(ld) => {
            s.push_str(&format!(" endpoint={:?}", ld.endpoint));
        }
        NodeKind::Overlay(od) => {
            s.push_str(&format!(" source={:?}", od.source));
        }
        _ => {}
    }
    if node.focusable() {
        s.push_str(" [focusable]");
    }
    if node.hit_target().is_some() {
        s.push_str(" [hit]");
    }
    s
}

/// Iterator that walks a scene subtree in depth-first child order. Yields
/// each node in the order `build_scene` attached it, which matches source-
/// document order under the isomorphism rules.
pub struct TreeOrderIter<'a> {
    scene: &'a Scene,
    stack: Vec<NodeId>,
}

impl<'a> TreeOrderIter<'a> {
    fn new(scene: &'a Scene, root: NodeId) -> Self {
        Self {
            scene,
            stack: vec![root],
        }
    }
}

impl<'a> Iterator for TreeOrderIter<'a> {
    type Item = &'a Node;

    fn next(&mut self) -> Option<&'a Node> {
        let id = self.stack.pop()?;
        let node = self.scene.nodes.get(id)?;
        // Push children in reverse so they pop in declaration order.
        for &child in node.children.iter().rev() {
            self.stack.push(child);
        }
        Some(node)
    }
}

/// Minimum contract the scene must preserve across any mutation.
/// Checked in tests; `PatchApplier` is responsible for keeping these
/// true at runtime.
#[cfg(test)]
pub(crate) fn validate_scene_structure(scene: &Scene) -> Result<(), String> {
    // 1. Every non-root node's parent is set and refers to an existing node
    //    that lists this node among its children.
    for (id, node) in scene.nodes.iter() {
        if id == scene.root {
            if node.parent.is_some() {
                return Err(format!("root {id:?} has a parent"));
            }
            continue;
        }
        let parent_id = node
            .parent
            .ok_or_else(|| format!("non-root {id:?} has no parent"))?;
        let parent = scene
            .nodes
            .get(parent_id)
            .ok_or_else(|| format!("{id:?} parent {parent_id:?} does not exist"))?;
        if !parent.children.contains(&id) {
            return Err(format!(
                "parent {parent_id:?} does not list {id:?} as child"
            ));
        }
    }
    // 2. Panel invariant: `active` is one of `states`.
    for node in scene.nodes.values() {
        if let NodeKind::Panel { states, active, .. } = node.kind()
            && !states.contains(active)
        {
            return Err(format!(
                "panel {:?} active {:?} not in states {:?}",
                node.id, active, states
            ));
        }
    }
    Ok(())
}

#[allow(dead_code)]
const _TAGS: [KindTag; 0] = []; // suppress unused-import warning
