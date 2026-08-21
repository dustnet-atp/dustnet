//! `Invalidation`: the second channel (alongside `Patch`) through which
//! scene state changes become observable to the render loop.
//!
//! `PatchApplier` writes to `layout` and `composite`. The render loop
//! reads `layout` per tick to drive scoped relayout (via
//! `layout_pass_invalidated`), then reads `composite` to decide which
//! screen rects need repainting. `present` is derived from `composite`.

use crate::compositor::layout::Rect;
use crate::resource::{BudgetLease, ResourceCategory, ResourceGovernor};

use super::node::NodeId;

/// What changed this tick — subsystems write to this, the render
/// loop reads it to decide what work to run.
///
/// - `composite` — rects whose contents changed (WASM tick, live
///   update, patch applied).
/// - `layout` — NodeIds whose subtree needs re-layout. Populated by
///   `PatchApplier`, consumed by `layout_pass_invalidated` in the
///   render loop.
/// - `present` — screen rects that need ANSI emission. Currently
///   written in lockstep with `composite`; a smarter split could
///   decouple them if dirty tracking justifies it.
#[derive(Debug, Default)]
pub struct Invalidation {
    pub composite: DirtyRegions,
    pub(crate) layout: LayoutInvalidation,
    pub present: DirtyRegions,
}

#[cfg(test)]
impl Clone for Invalidation {
    fn clone(&self) -> Self {
        Self {
            composite: self.composite.clone(),
            layout: self.layout.clone(),
            present: self.present.clone(),
        }
    }
}

impl Invalidation {
    pub(super) fn try_for_nodes(nodes: usize, governor: Option<&ResourceGovernor>) -> Option<Self> {
        Some(Self {
            composite: DirtyRegions::default(),
            layout: LayoutInvalidation::try_for_nodes(nodes, governor)?,
            present: DirtyRegions::default(),
        })
    }

    pub fn clear(&mut self) {
        self.composite.clear();
        self.layout.clear();
        self.present.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.composite.is_empty() && self.layout.is_empty() && self.present.is_empty()
    }

    /// Mark a screen-space rect as needing re-composition. Subsystems
    /// call this after mutating a node's buffer. `present` is
    /// invalidated in lockstep; a smarter dirty-tracking pass could
    /// split them.
    pub fn mark_composite(&mut self, rect: Rect) {
        self.composite.add(rect);
        self.present.add(rect);
    }

    /// Conservatively replace each dirty region with its prior bounding box
    /// unioned with `rect`. The inline representation cannot allocate.
    pub(crate) fn mark_composite_bounded(&mut self, rect: Rect) {
        self.composite.add_bounded(rect);
        self.present.add_bounded(rect);
    }
}

/// Page-owned, pre-admitted set of nodes requiring layout. The backing Vec is
/// reserved to the candidate scene's authored node count before activation;
/// production property changes therefore only perform allocation-free
/// deduplicated pushes. Compatibility structural growth fails closed when it
/// exceeds this bound and remains outside the generic rollback claim.
#[derive(Debug, Default)]
pub(crate) struct LayoutInvalidation {
    entries: Vec<NodeId>,
    _lease: Option<BudgetLease>,
}

impl LayoutInvalidation {
    fn try_for_nodes(nodes: usize, governor: Option<&ResourceGovernor>) -> Option<Self> {
        if super::tree::reject_scene_allocation(super::tree::SceneAllocationSite::Invalidation) {
            return None;
        }
        let requested = nodes.checked_mul(std::mem::size_of::<NodeId>())?;
        let mut lease = match (governor, requested) {
            (_, 0) | (None, _) => None,
            (Some(governor), bytes) => Some(
                governor
                    .reserve(ResourceCategory::RemoteCollections, bytes)
                    .ok()?,
            ),
        };
        let mut entries = Vec::new();
        entries.try_reserve_exact(nodes).ok()?;
        let retained = entries
            .capacity()
            .checked_mul(std::mem::size_of::<NodeId>())?;
        if let Some(lease) = lease.as_mut() {
            lease.try_resize_with_cost(retained, retained).ok()?;
        }
        Some(Self {
            entries,
            _lease: lease,
        })
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    pub(crate) fn contains(&self, id: &NodeId) -> bool {
        self.entries.contains(id)
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &NodeId> {
        self.entries.iter()
    }

    pub(crate) fn insert(&mut self, id: NodeId) -> bool {
        if self.entries.contains(&id) {
            return true;
        }
        if self.entries.len() == self.entries.capacity() {
            return false;
        }
        self.entries.push(id);
        true
    }

    pub(super) fn extend(&mut self, ids: impl IntoIterator<Item = NodeId>) -> bool {
        for id in ids {
            if !self.entries.contains(&id) {
                if self.entries.len() == self.entries.capacity() {
                    return false;
                }
                self.entries.push(id);
            }
        }
        true
    }

    #[cfg(test)]
    pub(super) fn capacity(&self) -> usize {
        self.entries.capacity()
    }

    #[cfg(test)]
    pub(super) fn retained_bytes(&self) -> usize {
        self._lease.as_ref().map_or(0, BudgetLease::byte_cost)
    }
}

impl PartialEq for LayoutInvalidation {
    fn eq(&self, other: &Self) -> bool {
        self.entries == other.entries
    }
}

impl Eq for LayoutInvalidation {}

#[cfg(test)]
impl Clone for LayoutInvalidation {
    fn clone(&self) -> Self {
        Self {
            entries: self.entries.clone(),
            _lease: None,
        }
    }
}

/// Allocation-free bounding rectangle for dirty screen-space regions.
#[derive(Debug, Default, Clone)]
pub struct DirtyRegions {
    rect: Option<Rect>,
}

impl DirtyRegions {
    pub fn clear(&mut self) {
        self.rect = None;
    }

    pub fn is_empty(&self) -> bool {
        self.rect.is_none()
    }

    pub fn add(&mut self, rect: Rect) {
        if rect.is_empty() {
            return;
        }
        self.rect = Some(self.rect.map_or(rect, |current| current.union(rect)));
    }

    fn add_bounded(&mut self, rect: Rect) {
        let union = match (self.bounding_box(), rect.is_empty()) {
            (Some(current), false) => Some(current.union(rect)),
            (Some(current), true) => Some(current),
            (None, false) => Some(rect),
            (None, true) => None,
        };
        self.rect = union;
    }

    pub fn iter(&self) -> impl Iterator<Item = &Rect> {
        self.rect.iter()
    }

    pub fn as_slice(&self) -> &[Rect] {
        self.rect.as_slice()
    }

    /// Union of all rects — useful when the caller wants a single bounding
    /// box rather than the per-rect list.
    pub fn bounding_box(&self) -> Option<Rect> {
        self.rect
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mark_composite_populates_both_sets() {
        let mut inv = Invalidation::default();
        inv.mark_composite(Rect::new(1, 2, 3, 4));
        assert_eq!(inv.composite.as_slice(), &[Rect::new(1, 2, 3, 4)]);
        assert_eq!(inv.present.as_slice(), &[Rect::new(1, 2, 3, 4)]);
    }

    #[test]
    fn empty_rect_is_no_op() {
        let mut r = DirtyRegions::default();
        r.add(Rect::new(0, 0, 0, 0));
        assert!(r.is_empty());
    }

    #[test]
    fn bounding_box_unions_all() {
        let mut r = DirtyRegions::default();
        r.add(Rect::new(0, 0, 3, 3));
        r.add(Rect::new(10, 10, 2, 2));
        assert_eq!(r.bounding_box(), Some(Rect::new(0, 0, 12, 12)));
        assert_eq!(r.as_slice(), &[Rect::new(0, 0, 12, 12)]);
    }

    #[test]
    fn clear_resets_everything() {
        let mut inv = Invalidation::default();
        inv.mark_composite(Rect::new(0, 0, 5, 5));
        inv.clear();
        assert!(inv.is_empty());
    }
}
