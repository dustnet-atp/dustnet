//! Shared-governor ownership for process-lifetime ATP session state.

use dustnet_core::protocol::origin::Origin;
use dustnet_core::session::{
    MAX_SESSION_CANDIDATE_BYTES, SessionDirective, SessionStore, SessionToken,
};

use crate::resource::{BudgetLease, ResourceCategory, ResourceGovernor};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GovernedSessionError {
    BudgetRejected,
    StorageRejected,
}

/// Production session owner. The old store remains charged while a complete
/// fallible candidate is staged under a second temporary lease.
#[derive(Debug)]
pub(crate) struct GovernedSessionStore {
    inner: SessionStore,
    governor: ResourceGovernor,
    retained_lease: Option<BudgetLease>,
}

impl GovernedSessionStore {
    pub(crate) fn new(governor: ResourceGovernor) -> Self {
        Self {
            inner: SessionStore::new(),
            governor,
            retained_lease: None,
        }
    }

    pub(crate) fn find_token(&self, origin: &Origin, path: &str) -> Option<&SessionToken> {
        self.inner.find_token(origin, path)
    }

    pub(crate) fn as_store(&self) -> &SessionStore {
        &self.inner
    }

    pub(crate) fn apply_directive(
        &mut self,
        origin: &Origin,
        directive: &SessionDirective,
    ) -> Result<(), GovernedSessionError> {
        if matches!(directive, SessionDirective::Clear { .. }) {
            debug_assert!(self.inner.apply_directive(origin, directive));
            self.reconcile_after_shrink();
            return Ok(());
        }

        if crate::client::reject_client_storage(
            crate::client::ClientStorageAllocationSite::SessionCandidate,
        ) {
            return Err(GovernedSessionError::BudgetRejected);
        }
        let mut candidate_lease = self
            .governor
            .reserve(ResourceCategory::Sessions, MAX_SESSION_CANDIDATE_BYTES)
            .map_err(|_| GovernedSessionError::BudgetRejected)?;
        let mut candidate = self
            .inner
            .try_clone()
            .map_err(|_| GovernedSessionError::StorageRejected)?;
        if !candidate.apply_directive(origin, directive) {
            return Err(GovernedSessionError::StorageRejected);
        }
        let retained = candidate.retained_capacity_bytes();
        // The candidate admission is deliberately conservative, so resizing
        // to measured retained storage only ever releases units. A refusal
        // here would mean the admission was wrong, which is a budget error
        // rather than something to abort the client over.
        candidate_lease
            .try_resize_with_cost(retained, retained)
            .map_err(|_| GovernedSessionError::BudgetRejected)?;

        let old_store = std::mem::replace(&mut self.inner, candidate);
        let old_lease = self.retained_lease.replace(candidate_lease);
        drop(old_store);
        drop(old_lease);
        Ok(())
    }

    pub(crate) fn clear_storage_key(&mut self, storage_key: &str) {
        self.inner.clear_storage_key(storage_key);
        self.reconcile_after_shrink();
    }

    pub(crate) fn clear_all(&mut self) {
        self.inner.clear_all();
        self.reconcile_after_shrink();
    }

    fn reconcile_after_shrink(&mut self) {
        let retained = self.inner.retained_capacity_bytes();
        if retained == 0 {
            self.retained_lease = None;
            return;
        }
        // Shrinking exact ownership releases units and admits nothing, so a
        // refusal is not possible; a missing lease means the store and its
        // accounting already disagree, and leaving the accounting alone is the
        // safe direction.
        if let Some(lease) = self.retained_lease.as_mut() {
            let _ = lease.try_resize_with_cost(retained, retained);
        }
    }

    #[cfg(test)]
    pub(crate) fn retained_capacity_bytes(&self) -> usize {
        self.inner.retained_capacity_bytes()
    }

    #[cfg(test)]
    pub(crate) fn retained_lease_amount(&self) -> usize {
        self.retained_lease.as_ref().map_or(0, BudgetLease::amount)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dustnet_core::protocol::origin::TransportSecurity;
    use dustnet_core::protocol::uri::AtpUri;

    fn origin(host: &str) -> Origin {
        Origin::from_uri(
            &AtpUri::parse(&format!("atp://{host}/")).unwrap(),
            TransportSecurity::VerifiedTls,
        )
        .unwrap()
    }

    fn set(token: &str, scope: &str) -> SessionDirective {
        SessionDirective::Set {
            token: token.into(),
            scope: scope.into(),
            expires: None,
        }
    }

    #[test]
    fn governed_sessions_account_exact_retained_capacity_until_clear_and_drop() {
        let governor = ResourceGovernor::new();
        let mut sessions = GovernedSessionStore::new(governor.clone());
        let site = origin("sessions.example");
        assert_eq!(governor.used(ResourceCategory::Sessions), 0);
        assert_eq!(governor.count(ResourceCategory::Sessions), 0);

        sessions.apply_directive(&site, &set("token", "/")).unwrap();
        assert_eq!(
            sessions.retained_lease_amount(),
            sessions.retained_capacity_bytes()
        );
        assert_eq!(
            governor.used(ResourceCategory::Sessions),
            sessions.retained_capacity_bytes()
        );
        assert_eq!(governor.count(ResourceCategory::Sessions), 1);

        sessions
            .apply_directive(&site, &SessionDirective::Clear { scope: "/".into() })
            .unwrap();
        assert_eq!(
            governor.used(ResourceCategory::Sessions),
            sessions.retained_capacity_bytes()
        );
        assert!(sessions.retained_capacity_bytes() > 0);
        let other = origin("clear.example");
        sessions
            .apply_directive(&other, &set("other", "/private"))
            .unwrap();
        sessions.clear_storage_key(&other.storage_key().unwrap());
        assert_eq!(sessions.as_store().total_count(), 0);
        assert_eq!(
            governor.used(ResourceCategory::Sessions),
            sessions.retained_capacity_bytes()
        );
        sessions.clear_all();
        assert_eq!(sessions.retained_capacity_bytes(), 0);
        assert_eq!(governor.used(ResourceCategory::Sessions), 0);
        assert_eq!(governor.count(ResourceCategory::Sessions), 0);
        drop(sessions);
        assert_eq!(governor.total_used(), 0);
    }

    /// Naming the candidate site refuses it while the budget is untouched, so
    /// the assertions below are about the store surviving *this* refusal
    /// rather than about the governor being full. The budget-pressure leg
    /// follows.
    #[test]
    fn governed_sessions_candidate_rejection_preserves_the_exact_store() {
        use crate::client::{ClientStorageAllocationSite, ClientStorageRejectionGuard};

        let governor = ResourceGovernor::new();
        let mut sessions = GovernedSessionStore::new(governor.clone());
        let site = origin("named.example");
        sessions.apply_directive(&site, &set("old", "/")).unwrap();
        let retained = sessions.retained_capacity_bytes();
        let used = governor.used(ResourceCategory::Sessions);

        let rejection =
            ClientStorageRejectionGuard::at(ClientStorageAllocationSite::SessionCandidate);
        assert_eq!(
            sessions.apply_directive(&site, &set("new-and-longer", "/")),
            Err(GovernedSessionError::BudgetRejected)
        );
        assert_eq!(sessions.find_token(&site, "/").unwrap().token, "old");
        assert_eq!(sessions.retained_capacity_bytes(), retained);
        assert_eq!(sessions.retained_lease_amount(), retained);
        assert_eq!(
            governor.used(ResourceCategory::Sessions),
            used,
            "a refused candidate must not move the budget"
        );
        drop(rejection);

        sessions
            .apply_directive(&site, &set("new-and-longer", "/"))
            .unwrap();
        assert_eq!(
            sessions.find_token(&site, "/").unwrap().token,
            "new-and-longer"
        );
        drop(sessions);
        assert_eq!(governor.total_used(), 0);
    }

    #[test]
    fn candidate_budget_rejection_preserves_exact_store_and_lease() {
        let governor = ResourceGovernor::new();
        let mut sessions = GovernedSessionStore::new(governor.clone());
        let site = origin("pressure.example");
        sessions.apply_directive(&site, &set("old", "/")).unwrap();
        let retained = sessions.retained_capacity_bytes();
        let count = governor.count(ResourceCategory::Sessions);
        let blocker = governor
            .reserve(ResourceCategory::Sessions, MAX_SESSION_CANDIDATE_BYTES)
            .unwrap();

        assert_eq!(
            sessions.apply_directive(&site, &set("new-and-longer", "/")),
            Err(GovernedSessionError::BudgetRejected)
        );
        assert_eq!(sessions.find_token(&site, "/").unwrap().token, "old");
        assert_eq!(sessions.retained_capacity_bytes(), retained);
        assert_eq!(sessions.retained_lease_amount(), retained);
        assert_eq!(governor.count(ResourceCategory::Sessions), count + 1);

        drop(blocker);
        sessions
            .apply_directive(&site, &set("new-and-longer", "/"))
            .unwrap();
        assert_eq!(
            sessions.find_token(&site, "/").unwrap().token,
            "new-and-longer"
        );
        assert_eq!(
            governor.used(ResourceCategory::Sessions),
            sessions.retained_capacity_bytes()
        );
        assert_eq!(governor.count(ResourceCategory::Sessions), 1);
    }

    #[test]
    fn aggregate_pressure_rejects_before_candidate_allocation() {
        let governor = ResourceGovernor::new();
        let mut sessions = GovernedSessionStore::new(governor.clone());
        let site = origin("aggregate.example");
        sessions.apply_directive(&site, &set("old", "/")).unwrap();
        let before = governor.total_used();
        let blocker_bytes = crate::resource::MAX_REMOTE_MEMORY
            .saturating_sub(before)
            .saturating_sub(MAX_SESSION_CANDIDATE_BYTES - 1);
        let blocker = governor
            .reserve(ResourceCategory::AstStrings, blocker_bytes)
            .unwrap();
        let blocked = governor.total_used();

        assert_eq!(
            sessions.apply_directive(&site, &set("new", "/private")),
            Err(GovernedSessionError::BudgetRejected)
        );
        assert_eq!(sessions.as_store().total_count(), 1);
        assert_eq!(sessions.find_token(&site, "/private").unwrap().token, "old");
        assert_eq!(governor.total_used(), blocked);
        drop(blocker);
        assert_eq!(governor.total_used(), before);
        drop(sessions);
        assert_eq!(governor.total_used(), 0);
        assert_eq!(governor.count(ResourceCategory::Sessions), 0);
    }

    #[test]
    fn site_bound_eviction_occurs_only_after_candidate_admission() {
        let governor = ResourceGovernor::new();
        let mut sessions = GovernedSessionStore::new(governor.clone());
        let site = origin("bounded.example");
        for index in 0..dustnet_core::session::MAX_SESSIONS_PER_SITE {
            sessions
                .apply_directive(
                    &site,
                    &set(&format!("token-{index}"), &format!("/scope-{index}")),
                )
                .unwrap();
        }
        let retained = sessions.retained_capacity_bytes();
        let blocker = governor
            .reserve(ResourceCategory::Sessions, MAX_SESSION_CANDIDATE_BYTES)
            .unwrap();
        assert_eq!(
            sessions.apply_directive(&site, &set("ninth", "/ninth")),
            Err(GovernedSessionError::BudgetRejected)
        );
        assert_eq!(
            sessions.find_token(&site, "/scope-0").unwrap().token,
            "token-0"
        );
        assert_eq!(sessions.retained_capacity_bytes(), retained);

        drop(blocker);
        sessions
            .apply_directive(&site, &set("ninth", "/ninth"))
            .unwrap();
        assert!(sessions.find_token(&site, "/scope-0").is_none());
        assert_eq!(sessions.find_token(&site, "/ninth").unwrap().token, "ninth");
        assert_eq!(
            sessions.as_store().total_count(),
            dustnet_core::session::MAX_SESSIONS_PER_SITE
        );
    }
}
