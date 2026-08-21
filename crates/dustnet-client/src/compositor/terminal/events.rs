//! Runtime scheduling for authored events and deferred navigation.

use crate::compositor::scene::EventBinding;
use crate::parser::ast;
use crate::resource::{BudgetLease, ResourceCategory, ResourceGovernor};

/// Bound synchronous `state-change` cascades driven by remote AML.
pub(super) const MAX_CASCADE_DEPTH: u8 = 16;
/// Retained authored work for one page. New work beyond this hard semantic
/// bound is rejected before the triggering action mutates presentation state.
pub(super) const MAX_PENDING_EVENT_ACTIONS: usize = 256;
const MAX_ON_BINDINGS: usize = crate::compositor::scene::events::MAX_EVENT_BINDINGS;

/// Runtime event dispatcher for `[on]` bindings.
pub(super) struct EventDispatcher {
    pending: Vec<ScheduledAction>,
    _collection_lease: Option<BudgetLease>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ScheduledAction {
    pub(super) binding_index: usize,
    fire_at: std::time::Instant,
    pub(super) cascade_depth: u8,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct PreparedDispatch {
    actions: [ScheduledAction; MAX_ON_BINDINGS],
    len: usize,
}

#[cfg(test)]
impl PreparedDispatch {
    pub(super) fn len(&self) -> usize {
        self.len
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct EventDispatchRejected;

impl std::fmt::Display for EventDispatchRejected {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("authored action queue exceeded the client resource limit")
    }
}

/// Navigation held until its authored exit animation presents a final frame.
pub(super) struct DeferredNavigation {
    pub(super) scope: crate::viewer::PageScope,
    pub(super) request_id: u64,
    pub(super) wait_for: String,
    pub(super) action: crate::compositor::panels::FocusAction,
    pub(super) ready: bool,
    pub(super) final_frame_presented: bool,
}

impl DeferredNavigation {
    pub(super) fn mark_animation_finished(&mut self, id: &str) {
        if self.wait_for == id {
            self.ready = true;
        }
    }

    pub(super) fn mark_final_frame_presented(&mut self) {
        if self.ready {
            self.final_frame_presented = true;
        }
    }

    pub(super) fn can_resume(&self, current_scope: Option<&crate::viewer::PageScope>) -> bool {
        current_scope == Some(&self.scope) && self.ready && self.final_frame_presented
    }
}

impl EventDispatcher {
    pub(super) fn try_governed(governor: &ResourceGovernor) -> Result<Self, EventDispatchRejected> {
        let requested = MAX_PENDING_EVENT_ACTIONS
            .checked_mul(std::mem::size_of::<ScheduledAction>())
            .ok_or(EventDispatchRejected)?;
        let mut lease = governor
            .reserve(ResourceCategory::RemoteCollections, requested)
            .map_err(|_| EventDispatchRejected)?;
        if crate::compositor::terminal::runner::reject_runner_allocation(
            crate::compositor::terminal::runner::RunnerAllocationSite::EventQueue,
        ) {
            return Err(EventDispatchRejected);
        }
        let mut pending = Vec::new();
        pending
            .try_reserve_exact(MAX_PENDING_EVENT_ACTIONS)
            .map_err(|_| EventDispatchRejected)?;
        let retained = pending
            .capacity()
            .checked_mul(std::mem::size_of::<ScheduledAction>())
            .ok_or(EventDispatchRejected)?;
        lease
            .try_resize_with_cost(retained, retained)
            .map_err(|_| EventDispatchRejected)?;
        Ok(Self {
            pending,
            _collection_lease: Some(lease),
        })
    }

    /// A dispatcher that holds nothing and owns no lease.
    ///
    /// Used where a prepared page arrived without its admitted queue: the page
    /// still renders and still responds to input, it just cannot schedule
    /// delayed actions. It allocates nothing, so it needs no admission.
    pub(super) fn unadmitted() -> Self {
        Self {
            pending: Vec::new(),
            _collection_lease: None,
        }
    }

    #[cfg(test)]
    pub(super) fn new() -> Self {
        Self::try_governed(&ResourceGovernor::new())
            .expect("the fixed test dispatcher must fit an empty governor")
    }

    /// Stage every matching binding without mutating the queue. The fixed
    /// batch and pre-admitted queue make commit allocation-free.
    pub(super) fn prepare_fire(
        &self,
        bindings: &[EventBinding],
        event: ast::EventKind,
        source: Option<&str>,
        cascade_depth: u8,
    ) -> Result<PreparedDispatch, EventDispatchRejected> {
        let now = std::time::Instant::now();
        let sentinel = ScheduledAction {
            binding_index: 0,
            fire_at: now,
            cascade_depth,
        };
        let mut prepared = PreparedDispatch {
            actions: [sentinel; MAX_ON_BINDINGS],
            len: 0,
        };

        for (binding_index, binding) in bindings.iter().enumerate() {
            if binding.event != event {
                continue;
            }
            if let Some(ref expected) = binding.source {
                match source {
                    Some(actual) if actual == expected => {}
                    _ => continue,
                }
            }
            if prepared.len >= MAX_ON_BINDINGS
                || self.pending.len().saturating_add(prepared.len + 1) > MAX_PENDING_EVENT_ACTIONS
                || self.pending.len().saturating_add(prepared.len + 1) > self.pending.capacity()
            {
                return Err(EventDispatchRejected);
            }
            let Some(slot) = prepared.actions.get_mut(prepared.len) else {
                return Err(EventDispatchRejected);
            };
            *slot = ScheduledAction {
                binding_index,
                fire_at: now + std::time::Duration::from_millis(binding.delay_ms as u64),
                cascade_depth,
            };
            prepared.len += 1;
        }
        Ok(prepared)
    }

    pub(super) fn commit(&mut self, prepared: PreparedDispatch) {
        debug_assert!(self.pending.len() + prepared.len <= self.pending.capacity());
        // Clamp to the admitted capacity. `prepare_fire` already refuses
        // beyond it, so this only matters for a dispatcher that was never
        // admitted — which must hold nothing rather than allocate outside the
        // governor.
        let room = self.pending.capacity().saturating_sub(self.pending.len());
        if let Some(actions) = prepared.actions.get(..prepared.len.min(room)) {
            self.pending.extend_from_slice(actions);
        }
    }

    /// Borrow the first ready action without retiring it. Peeking is what lets
    /// resource pressure resume the exact item: a mutation refused mid-flight
    /// leaves its action queued, so recovery retries that one action instead
    /// of replaying the event that scheduled it.
    ///
    /// The caller therefore retires an action once it is *settled*, which is
    /// not the same as "applied": a `set` whose panel already holds the
    /// requested state changed nothing and is still finished with. Leaving a
    /// settled action queued is not a delay, it is a spin — a ready action is
    /// handed straight back on the next call, forever.
    pub(super) fn next_ready(&self) -> Option<(usize, ScheduledAction)> {
        let now = std::time::Instant::now();
        self.pending
            .iter()
            .copied()
            .enumerate()
            .find(|(_, action)| now >= action.fire_at)
    }

    pub(super) fn complete(&mut self, index: usize, expected: ScheduledAction) {
        debug_assert_eq!(
            self.pending.get(index).map(|action| action.binding_index),
            Some(expected.binding_index)
        );
        self.pending.remove(index);
    }

    pub(super) fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    #[cfg(test)]
    pub(super) fn retained_collection_capacity_bytes(&self) -> usize {
        self.pending
            .capacity()
            .saturating_mul(std::mem::size_of::<ScheduledAction>())
    }

    /// Make the next tick drain every delayed action when animation is skipped.
    pub(super) fn flush_pending_now(&mut self) {
        let now = std::time::Instant::now();
        for action in &mut self.pending {
            action.fire_at = now;
        }
    }

    #[cfg(test)]
    pub(super) fn pending_len(&self) -> usize {
        self.pending.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding(delay_ms: u32) -> EventBinding {
        EventBinding {
            event: ast::EventKind::PageLoad,
            source: None,
            action: ast::ActionKind::Animate,
            target: "probe".into(),
            to: None,
            delay_ms,
        }
    }

    #[test]
    fn queue_capacity_is_governed_for_the_page_lifetime() {
        let governor = ResourceGovernor::new();
        let dispatcher = EventDispatcher::try_governed(&governor).unwrap();
        let expected = dispatcher.retained_collection_capacity_bytes();

        assert_eq!(dispatcher.pending.capacity(), MAX_PENDING_EVENT_ACTIONS);
        assert_eq!(governor.used(ResourceCategory::RemoteCollections), expected);
        assert_eq!(governor.count(ResourceCategory::RemoteCollections), 1);
        drop(dispatcher);
        assert_eq!(governor.used(ResourceCategory::RemoteCollections), 0);
        assert_eq!(governor.count(ResourceCategory::RemoteCollections), 0);

        let blocker = governor
            .reserve(
                ResourceCategory::RemoteCollections,
                crate::resource::MAX_REMOTE_MEMORY,
            )
            .unwrap();
        let used = governor.used(ResourceCategory::RemoteCollections);
        let count = governor.count(ResourceCategory::RemoteCollections);
        assert!(EventDispatcher::try_governed(&governor).is_err());
        assert_eq!(governor.used(ResourceCategory::RemoteCollections), used);
        assert_eq!(governor.count(ResourceCategory::RemoteCollections), count);
        drop(blocker);
        assert_eq!(governor.used(ResourceCategory::RemoteCollections), 0);
    }

    #[test]
    fn hard_queue_bound_rejects_the_whole_batch_without_mutation() {
        let mut dispatcher = EventDispatcher::new();
        let bindings = [binding(u32::MAX)];
        for _ in 0..MAX_PENDING_EVENT_ACTIONS {
            let prepared = dispatcher
                .prepare_fire(&bindings, ast::EventKind::PageLoad, None, 0)
                .unwrap();
            dispatcher.commit(prepared);
        }
        assert_eq!(dispatcher.pending_len(), MAX_PENDING_EVENT_ACTIONS);
        let before = dispatcher
            .pending
            .iter()
            .map(|action| (action.binding_index, action.fire_at, action.cascade_depth))
            .collect::<Vec<_>>();

        assert_eq!(
            dispatcher.prepare_fire(&bindings, ast::EventKind::PageLoad, None, 0),
            Err(EventDispatchRejected)
        );
        assert_eq!(dispatcher.pending_len(), MAX_PENDING_EVENT_ACTIONS);
        assert_eq!(
            dispatcher
                .pending
                .iter()
                .map(|action| (action.binding_index, action.fire_at, action.cascade_depth))
                .collect::<Vec<_>>(),
            before
        );
    }

    #[test]
    fn ready_action_is_retained_until_exact_completion() {
        let mut dispatcher = EventDispatcher::new();
        let bindings = [binding(0)];
        let prepared = dispatcher
            .prepare_fire(&bindings, ast::EventKind::PageLoad, None, 7)
            .unwrap();
        dispatcher.commit(prepared);

        let first = dispatcher.next_ready().unwrap();
        let second = dispatcher.next_ready().unwrap();
        assert_eq!(first.0, second.0);
        assert_eq!(first.1.binding_index, second.1.binding_index);
        assert_eq!(first.1.cascade_depth, 7);
        assert_eq!(dispatcher.pending_len(), 1);

        dispatcher.complete(first.0, first.1);
        assert!(dispatcher.next_ready().is_none());
        assert_eq!(dispatcher.pending_len(), 0);
    }
}
