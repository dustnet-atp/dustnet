//! Shared-governor ownership for process-lifetime ATP session state.

use dustnet_core::protocol::origin::Origin;
use dustnet_core::session::{
    MAX_SESSION_CANDIDATE_BYTES, SessionDirective, SessionStore, SessionToken,
};

use crate::resource::{BudgetLease, ResourceCategory, ResourceGovernor};
use crate::session_file::{SessionFile, SessionFileError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GovernedSessionError {
    BudgetRejected,
    StorageRejected,
}

/// Production session owner. The old store remains charged while a complete
/// fallible candidate is staged under a second temporary lease.
///
/// Every mutation the client can make to session state passes through here,
/// which is why at-rest persistence hangs off this type rather than off the
/// call sites: there is no way to set or clear a session that skips the file.
#[derive(Debug)]
pub(crate) struct GovernedSessionStore {
    inner: SessionStore,
    governor: ResourceGovernor,
    retained_lease: Option<BudgetLease>,
    /// Detached until the viewer attaches one, so a store that nobody asked
    /// to persist needs no second code path to stay in memory.
    file: SessionFile,
    last_persistence_error: Option<SessionFileError>,
}

impl GovernedSessionStore {
    pub(crate) fn new(governor: ResourceGovernor) -> Self {
        Self {
            inner: SessionStore::new(),
            governor,
            retained_lease: None,
            file: SessionFile::detached(),
            last_persistence_error: None,
        }
    }

    /// Adopt a session file, restoring whatever it holds.
    ///
    /// Restoration replays each stored session through [`Self::apply_directive`]
    /// rather than reaching into the store, so the per-site and total bounds
    /// and the memory accounting treat a file exactly as they treat a hostile
    /// server. A file listing more sessions than the client admits therefore
    /// loses the excess rather than becoming a way around the limits.
    ///
    /// Returns how many sessions were restored. A read failure is reported but
    /// is not fatal: the client starts with no sessions, which costs a login
    /// and nothing else.
    pub(crate) fn with_persistence(
        governor: ResourceGovernor,
        file: SessionFile,
    ) -> (Self, Result<usize, SessionFileError>) {
        let mut store = Self {
            inner: SessionStore::new(),
            governor,
            retained_lease: None,
            file,
            last_persistence_error: None,
        };
        let outcome = store.restore();
        (store, outcome)
    }

    fn restore(&mut self) -> Result<usize, SessionFileError> {
        let stored = self.file.load()?;
        let mut restored = 0usize;
        for (origin, directive) in &stored {
            if self.apply_directive(origin, directive).is_ok() {
                restored += 1;
            }
        }
        // The file is rewritten from what was actually admitted, so lines the
        // bounds refused, or that lapsed while the client was not running, do
        // not sit on disk being re-refused at every launch.
        if restored != stored.len() {
            self.persist_after_set();
        }
        Ok(restored)
    }

    /// Whether sessions outlive the process.
    pub(crate) fn is_persistent(&self) -> bool {
        !self.file.is_detached()
    }

    /// The most recent write failure, cleared by reading it.
    ///
    /// Persistence failures are reported this way rather than folded into
    /// [`Self::apply_directive`]'s result because they are a different kind of
    /// event: the session applied and works, and only its survival past exit
    /// is in doubt. Compare [`crate::client::ClientError::Trust`], which is
    /// fatal — a pin that does not persist would make the status bar claim a
    /// connection was authenticated when the next one will not be.
    pub(crate) fn take_persistence_error(&mut self) -> Option<SessionFileError> {
        self.last_persistence_error.take()
    }

    /// Persist after storing a session. A failure leaves the working in-memory
    /// session alone: the cost is re-logging in next launch.
    fn persist_after_set(&mut self) {
        if let Err(error) = self.file.save(&self.inner) {
            self.last_persistence_error = Some(error);
        }
    }

    /// Persist after clearing a session, failing safe.
    ///
    /// A clear that does not reach the disk is the one direction that matters:
    /// it would leave a token the user just revoked sitting at rest. So a
    /// failed rewrite deletes the store outright. That forgets the sessions
    /// that were still valid — a login each — rather than remembering one that
    /// was meant to be gone.
    fn persist_after_clear(&mut self) {
        if let Err(error) = self.file.save(&self.inner) {
            self.last_persistence_error = Some(error);
            if let Err(error) = self.file.remove() {
                self.last_persistence_error = Some(error);
            }
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
            self.persist_after_clear();
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
        self.persist_after_set();
        Ok(())
    }

    pub(crate) fn clear_storage_key(&mut self, storage_key: &str) {
        self.inner.clear_storage_key(storage_key);
        self.reconcile_after_shrink();
        self.persist_after_clear();
    }

    pub(crate) fn clear_all(&mut self) {
        self.inner.clear_all();
        self.reconcile_after_shrink();
        self.persist_after_clear();
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

    fn temporary_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("dustnet-governed-sessions-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn far_future() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 86_400
    }

    /// The reason restoration replays directives instead of assigning a store:
    /// a file is admitted under exactly the bounds a hostile server is, so a
    /// long one loses its excess rather than becoming a way around the limits.
    #[test]
    fn a_session_file_is_admitted_under_the_same_bounds_as_a_server() {
        let dir = temporary_dir("bounds");
        let path = dir.join("sessions");
        let expires = far_future();
        let mut body = String::new();
        // Twice the per-site bound, all on one site and all distinct scopes,
        // so nothing is a replacement and the store has to shed the excess.
        let overflow = dustnet_core::session::MAX_SESSIONS_PER_SITE * 2;
        for index in 0..overflow {
            body.push_str(&format!(
                "example.com 1985 token{index} /s{index}/ {expires}\n"
            ));
        }
        crate::trust::write_private(&path, &body).unwrap();

        let governor = ResourceGovernor::new();
        let (sessions, restored) =
            GovernedSessionStore::with_persistence(governor.clone(), SessionFile::at(&path));
        // Every line was offered and accepted by `apply_directive`; the store
        // itself evicts at its own bound rather than refusing the directive.
        assert_eq!(restored.unwrap(), overflow);
        assert_eq!(
            sessions.as_store().total_count(),
            dustnet_core::session::MAX_SESSIONS_PER_SITE,
            "the per-site bound has to hold for a file as it does for a server"
        );
        assert_eq!(
            governor.used(ResourceCategory::Sessions),
            sessions.retained_capacity_bytes(),
            "a restored session is charged like any other"
        );

        // The file is rewritten from what was admitted, so the excess is not
        // re-offered and re-shed at every launch.
        let remaining = SessionFile::at(&path).load().unwrap();
        assert_eq!(
            remaining.len(),
            dustnet_core::session::MAX_SESSIONS_PER_SITE
        );
    }

    /// A session with no expiry is remembered in memory and never written, so
    /// a persistent client is not a way to acquire an endless credential.
    #[test]
    fn a_persisted_session_needs_an_expiry_to_reach_the_disk() {
        let dir = temporary_dir("expiry");
        let path = dir.join("sessions");
        let (mut sessions, restored) =
            GovernedSessionStore::with_persistence(ResourceGovernor::new(), SessionFile::at(&path));
        assert_eq!(restored.unwrap(), 0);
        let site = origin("sessions.example");

        sessions
            .apply_directive(&site, &set("endless", "/"))
            .unwrap();
        assert!(sessions.find_token(&site, "/").is_some());
        assert!(!path.exists(), "a token with no expiry was written out");

        sessions
            .apply_directive(
                &site,
                &SessionDirective::Set {
                    token: "bounded".into(),
                    scope: "/admin/".into(),
                    expires: Some(far_future()),
                },
            )
            .unwrap();
        let stored = SessionFile::at(&path).load().unwrap();
        assert_eq!(stored.len(), 1, "only the expiring session belongs on disk");
    }

    /// Clearing has to reach the disk. A logout that left the token at rest
    /// would be the one way persistence could be worse than no persistence.
    #[test]
    fn clearing_a_session_removes_it_from_the_disk() {
        let dir = temporary_dir("clear");
        let path = dir.join("sessions");
        let (mut sessions, _) =
            GovernedSessionStore::with_persistence(ResourceGovernor::new(), SessionFile::at(&path));
        let site = origin("sessions.example");
        let expiring = SessionDirective::Set {
            token: "tok".into(),
            scope: "/".into(),
            expires: Some(far_future()),
        };

        sessions.apply_directive(&site, &expiring).unwrap();
        assert_eq!(SessionFile::at(&path).load().unwrap().len(), 1);

        sessions
            .apply_directive(&site, &SessionDirective::Clear { scope: "/".into() })
            .unwrap();
        assert!(
            !path.exists(),
            "the last session was cleared, so nothing should be left at rest"
        );

        // The same has to hold for the two local clears, which do not go
        // through a directive at all.
        sessions.apply_directive(&site, &expiring).unwrap();
        sessions.clear_storage_key(&site.storage_key().unwrap());
        assert!(
            !path.exists(),
            "`:sessions clear <site>` left the token at rest"
        );

        sessions.apply_directive(&site, &expiring).unwrap();
        sessions.clear_all();
        assert!(!path.exists(), "`:sessions clear` left the token at rest");
    }

    #[test]
    fn a_detached_store_keeps_sessions_in_memory_only() {
        let mut sessions = GovernedSessionStore::new(ResourceGovernor::new());
        assert!(!sessions.is_persistent());
        let site = origin("sessions.example");
        sessions.apply_directive(&site, &set("token", "/")).unwrap();
        assert!(sessions.find_token(&site, "/").is_some());
        assert!(sessions.take_persistence_error().is_none());
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
