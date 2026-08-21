//! Path-scoped session token storage for ATP authentication.
//!
//! Sessions are keyed by site domain and scoped to path prefixes.
//! A token scoped to `/admin/` is never sent on requests to `/members/`.
//! There is no cross-site session mechanism.

use crate::protocol::{ProtocolError, origin::Origin};
use std::collections::TryReserveError;
use std::fmt::Write as _;

/// Maximum number of sessions stored per site.
pub const MAX_SESSIONS_PER_SITE: usize = 8;

/// Maximum total sessions across all sites.
pub const MAX_TOTAL_SESSIONS: usize = 256;

/// Maximum token string length.
pub const MAX_TOKEN_LEN: usize = 4096;

/// Maximum scope path length.
pub const MAX_SCOPE_LEN: usize = 1024;

/// Conservative admission for one complete fallible session-store candidate.
/// The active store remains separately charged while this scratch owner lives.
pub const MAX_SESSION_CANDIDATE_BYTES: usize = MAX_TOTAL_SESSIONS
    * (std::mem::size_of::<(Origin, SiteSessionStore)>()
        + crate::protocol::uri::MAX_URI_LEN
        + MAX_SESSIONS_PER_SITE * std::mem::size_of::<SessionToken>()
        + MAX_TOKEN_LEN
        + MAX_SCOPE_LEN)
    + MAX_TOKEN_LEN
    + MAX_SCOPE_LEN
    + crate::protocol::uri::MAX_URI_LEN;

/// A single session token, scoped to a path prefix on a specific site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionToken {
    /// Opaque token string (client stores but does not interpret).
    pub token: String,
    /// Path prefix this token applies to (e.g. `/admin/`).
    /// Defaults to `/` (entire site).
    pub scope: String,
    /// Optional expiry as Unix timestamp (seconds since epoch).
    pub expires: Option<u64>,
}

/// Session directive sent by the server in a PAGE response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionDirective {
    /// Set a session token with the given scope.
    Set {
        token: String,
        scope: String,
        expires: Option<u64>,
    },
    /// Clear the session token for the given scope.
    Clear { scope: String },
}

impl SessionDirective {
    /// Serialize to the wire format used in PAGE metadata.
    pub fn serialize(&self) -> Result<String, ProtocolError> {
        match self {
            SessionDirective::Set {
                token,
                scope,
                expires,
            } => {
                if token.is_empty() || token.len() > MAX_TOKEN_LEN || !valid_scope(scope) {
                    return Err(ProtocolError::InvalidMessage(
                        "invalid session directive".into(),
                    ));
                }
                let requested = "Set-Session:  \n".len()
                    + token.len()
                    + scope.len()
                    + expires.map_or(0, |value| 1 + decimal_len(value));
                let mut s = String::new();
                try_reserve_directive(&mut s, requested, DirectiveAllocationSite::Serialize)?;
                s.push_str("Set-Session: ");
                s.push_str(token);
                s.push(' ');
                s.push_str(scope);
                if let Some(exp) = expires {
                    s.push(' ');
                    let _ = write!(&mut s, "{exp}");
                }
                s.push('\n');
                Ok(s)
            }
            SessionDirective::Clear { scope } => {
                if !valid_scope(scope) {
                    return Err(ProtocolError::InvalidMessage(
                        "invalid session directive".into(),
                    ));
                }
                let requested = "Clear-Session: \n".len() + scope.len();
                let mut s = String::new();
                try_reserve_directive(&mut s, requested, DirectiveAllocationSite::Serialize)?;
                s.push_str("Clear-Session: ");
                s.push_str(scope);
                s.push('\n');
                Ok(s)
            }
        }
    }

    /// Parse from a key-value line (key already matched).
    pub fn parse_set(value: &str) -> Result<Option<SessionDirective>, ProtocolError> {
        let mut parts = value.splitn(3, ' ');
        let Some(token) = parts.next() else {
            return Ok(None);
        };
        let scope = parts.next().unwrap_or("/");
        let expires = parts.next().and_then(|s| s.parse::<u64>().ok());

        if token.is_empty() || token.len() > MAX_TOKEN_LEN || !valid_scope(scope) {
            return Ok(None);
        }

        Ok(Some(SessionDirective::Set {
            token: try_copy_string(token, DirectiveAllocationSite::ParseToken)?,
            scope: try_copy_string(scope, DirectiveAllocationSite::ParseScope)?,
            expires,
        }))
    }

    pub fn parse_clear(value: &str) -> Result<Option<SessionDirective>, ProtocolError> {
        let scope = value.trim();
        if !valid_scope(scope) {
            return Ok(None);
        }
        Ok(Some(SessionDirective::Clear {
            scope: try_copy_string(scope, DirectiveAllocationSite::ParseClear)?,
        }))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DirectiveAllocationSite {
    Serialize,
    ParseToken,
    ParseScope,
    ParseClear,
}

fn try_copy_string(value: &str, site: DirectiveAllocationSite) -> Result<String, ProtocolError> {
    let mut copy = String::new();
    try_reserve_directive(&mut copy, value.len(), site)?;
    copy.push_str(value);
    Ok(copy)
}

fn try_reserve_directive(
    value: &mut String,
    requested: usize,
    site: DirectiveAllocationSite,
) -> Result<(), ProtocolError> {
    #[cfg(not(test))]
    let _ = site;
    #[cfg(test)]
    if reject_directive_allocation(site) {
        return Err(ProtocolError::ResourceExhausted { requested });
    }
    value
        .try_reserve_exact(requested)
        .map_err(|_| ProtocolError::ResourceExhausted { requested })
}

#[cfg(test)]
thread_local! {
    static REJECT_DIRECTIVE_ALLOCATION: std::cell::Cell<Option<DirectiveAllocationSite>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn reject_directive_allocation(site: DirectiveAllocationSite) -> bool {
    REJECT_DIRECTIVE_ALLOCATION.with(|reject| {
        if reject.get() == Some(site) {
            reject.set(None);
            true
        } else {
            false
        }
    })
}

fn decimal_len(mut value: u64) -> usize {
    let mut len = 1;
    while value >= 10 {
        value /= 10;
        len += 1;
    }
    len
}

/// Per-site session store.
#[derive(Debug, Default)]
pub struct SiteSessionStore {
    tokens: Vec<SessionToken>,
}

impl SiteSessionStore {
    pub fn new() -> Self {
        SiteSessionStore { tokens: Vec::new() }
    }

    /// Find the most specific matching token for a request path.
    ///
    /// Matches tokens whose scope is a prefix of the path, then returns
    /// the one with the longest (most specific) scope.
    pub fn find_token(&self, path: &str) -> Option<&SessionToken> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        self.tokens
            .iter()
            .filter(|t| scope_matches(&t.scope, path))
            .filter(|t| t.expires.is_none_or(|expires| expires > now))
            .max_by_key(|t| t.scope.len())
    }

    /// Store a session token, replacing any existing token with the same scope.
    pub fn set_token(&mut self, token: SessionToken) -> bool {
        self.try_set_token(token).is_ok()
    }

    fn try_set_token(
        &mut self,
        token: SessionToken,
    ) -> Result<(), std::collections::TryReserveError> {
        if let Some(existing) = self
            .tokens
            .iter_mut()
            .find(|existing| existing.scope == token.scope)
        {
            *existing = token;
            return Ok(());
        }
        if self.tokens.len() >= MAX_SESSIONS_PER_SITE {
            self.tokens.remove(0);
        } else {
            self.tokens.try_reserve(1)?;
        }
        self.tokens.push(token);
        Ok(())
    }

    /// Clear the token for a specific scope.
    pub fn clear_scope(&mut self, scope: &str) {
        self.tokens.retain(|t| t.scope != scope);
    }

    /// Clear all tokens.
    pub fn clear_all(&mut self) {
        self.tokens.clear();
    }

    /// Return all stored tokens (for session management UI).
    pub fn list_tokens(&self) -> &[SessionToken] {
        &self.tokens
    }

    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }

    fn try_clone(&self) -> Result<Self, TryReserveError> {
        let mut tokens = Vec::new();
        tokens.try_reserve_exact(self.tokens.len())?;
        for token in &self.tokens {
            tokens.push(try_clone_token(&token.token, &token.scope, token.expires)?);
        }
        Ok(Self { tokens })
    }

    fn retained_capacity_bytes(&self) -> usize {
        self.tokens
            .capacity()
            .saturating_mul(std::mem::size_of::<SessionToken>())
            .saturating_add(
                self.tokens
                    .iter()
                    .map(|token| {
                        token
                            .token
                            .capacity()
                            .saturating_add(token.scope.capacity())
                    })
                    .sum::<usize>(),
            )
    }
}

fn valid_scope(scope: &str) -> bool {
    !scope.is_empty()
        && scope.len() <= MAX_SCOPE_LEN
        && scope.starts_with('/')
        && !scope.chars().any(|c| c.is_control())
        && !scope.contains(['?', '#'])
        && !scope
            .split('/')
            .any(|segment| segment == "." || segment == "..")
}

fn scope_matches(scope: &str, path: &str) -> bool {
    if scope == "/" || path == scope {
        return true;
    }
    let Some(rest) = path.strip_prefix(scope) else {
        return false;
    };
    scope.ends_with('/') || rest.starts_with('/')
}

/// Global session store across all sites.
#[derive(Debug, Default)]
pub struct SessionStore {
    sites: Vec<(Origin, SiteSessionStore)>,
}

impl SessionStore {
    pub fn new() -> Self {
        SessionStore { sites: Vec::new() }
    }

    /// Find matching token for a site and path.
    pub fn find_token(&self, origin: &Origin, path: &str) -> Option<&SessionToken> {
        self.sites
            .iter()
            .find(|(candidate, _)| candidate == origin)?
            .1
            .find_token(path)
    }

    /// Apply a session directive from a server response.
    /// Apply a server directive atomically.
    ///
    /// Returns `false` when the global bound or a fallible collection/string
    /// allocation rejects the directive. A rejection never creates an empty
    /// origin bucket or changes an existing token.
    pub fn apply_directive(&mut self, origin: &Origin, directive: &SessionDirective) -> bool {
        let total: usize = self.sites.iter().map(|(_, site)| site.tokens.len()).sum();

        match directive {
            SessionDirective::Set {
                token,
                scope,
                expires,
            } => {
                let existing = self
                    .sites
                    .iter()
                    .find(|(candidate, _)| candidate == origin)
                    .map(|(_, site)| site);
                let is_replacement = existing
                    .is_some_and(|site| site.tokens.iter().any(|entry| entry.scope == *scope));
                let evicts_at_site_bound =
                    existing.is_some_and(|site| site.tokens.len() >= MAX_SESSIONS_PER_SITE);
                if !is_replacement && !evicts_at_site_bound && total >= MAX_TOTAL_SESSIONS {
                    return false;
                }
                let Ok(candidate) = try_clone_token(token, scope, *expires) else {
                    return false;
                };
                if let Some((_, site_store)) = self
                    .sites
                    .iter_mut()
                    .find(|(candidate, _)| candidate == origin)
                {
                    return site_store.try_set_token(candidate).is_ok();
                }
                let Ok(origin) = origin.try_clone() else {
                    return false;
                };
                let mut site_store = SiteSessionStore::new();
                if site_store.try_set_token(candidate).is_err() {
                    return false;
                }
                if self.sites.try_reserve(1).is_err() {
                    return false;
                }
                self.sites.push((origin, site_store));
                true
            }
            SessionDirective::Clear { scope } => {
                if let Some((_, site_store)) = self
                    .sites
                    .iter_mut()
                    .find(|(candidate, _)| candidate == origin)
                {
                    site_store.clear_scope(scope);
                }
                self.sites
                    .retain(|(candidate, site)| candidate != origin || !site.is_empty());
                true
            }
        }
    }

    /// Clear all sessions for a site.
    pub fn clear_origin(&mut self, origin: &Origin) {
        self.sites.retain(|(candidate, _)| candidate != origin);
    }

    /// Remove an origin selected by its display/storage key in the local UI.
    pub fn clear_storage_key(&mut self, storage_key: &str) {
        self.sites
            .retain(|(origin, _)| !origin.matches_storage_key(storage_key));
    }

    /// Clear all sessions.
    pub fn clear_all(&mut self) {
        self.sites = Vec::new();
    }

    /// Total number of stored sessions across all sites.
    pub fn total_count(&self) -> usize {
        self.sites.iter().map(|(_, site)| site.tokens.len()).sum()
    }

    /// Iterate over all sites and their session stores.
    pub fn iter_sites(&self) -> impl Iterator<Item = (&Origin, &SiteSessionStore)> {
        self.sites.iter().map(|(origin, site)| (origin, site))
    }

    /// Fallibly clone all retained session state for an atomic governed update.
    pub fn try_clone(&self) -> Result<Self, TryReserveError> {
        let mut sites = Vec::new();
        sites.try_reserve_exact(self.sites.len())?;
        for (origin, site) in &self.sites {
            sites.push((origin.try_clone()?, site.try_clone()?));
        }
        Ok(Self { sites })
    }

    /// Exact heap capacity retained by the store and all nested owners.
    pub fn retained_capacity_bytes(&self) -> usize {
        self.sites
            .capacity()
            .saturating_mul(std::mem::size_of::<(Origin, SiteSessionStore)>())
            .saturating_add(
                self.sites
                    .iter()
                    .map(|(origin, site)| {
                        origin
                            .host_capacity()
                            .saturating_add(site.retained_capacity_bytes())
                    })
                    .sum::<usize>(),
            )
    }
}

fn try_clone_token(
    token: &str,
    scope: &str,
    expires: Option<u64>,
) -> Result<SessionToken, std::collections::TryReserveError> {
    let mut owned_token = String::new();
    owned_token.try_reserve_exact(token.len())?;
    owned_token.push_str(token);
    let mut owned_scope = String::new();
    owned_scope.try_reserve_exact(scope.len())?;
    owned_scope.push_str(scope);
    Ok(SessionToken {
        token: owned_token,
        scope: owned_scope,
        expires,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::origin::TransportSecurity;
    use crate::protocol::uri::AtpUri;

    fn origin(host: &str) -> Origin {
        Origin::from_uri(
            &AtpUri::parse(&format!("atp://{host}/")).unwrap(),
            TransportSecurity::VerifiedTls,
        )
        .unwrap()
    }

    #[test]
    fn find_most_specific_scope() {
        let mut store = SiteSessionStore::new();
        store.set_token(SessionToken {
            token: "root-token".into(),
            scope: "/".into(),
            expires: None,
        });
        store.set_token(SessionToken {
            token: "admin-token".into(),
            scope: "/admin/".into(),
            expires: None,
        });

        // /admin/users → admin-token (more specific)
        let t = store.find_token("/admin/users").unwrap();
        assert_eq!(t.token, "admin-token");

        // /about → root-token
        let t = store.find_token("/about").unwrap();
        assert_eq!(t.token, "root-token");

        // /admin/ → admin-token
        let t = store.find_token("/admin/").unwrap();
        assert_eq!(t.token, "admin-token");
    }

    #[test]
    fn no_match_returns_none() {
        let store = SiteSessionStore::new();
        assert!(store.find_token("/anything").is_none());
    }

    #[test]
    fn scope_isolation() {
        let mut store = SiteSessionStore::new();
        store.set_token(SessionToken {
            token: "admin-only".into(),
            scope: "/admin/".into(),
            expires: None,
        });

        // /members/ should NOT get the admin token
        assert!(store.find_token("/members/page").is_none());
        // /admin/dashboard should get it
        assert!(store.find_token("/admin/dashboard").is_some());
    }

    #[test]
    fn scope_matching_respects_path_segments() {
        let mut store = SiteSessionStore::new();
        store.set_token(SessionToken {
            token: "admin-only".into(),
            scope: "/admin".into(),
            expires: None,
        });
        assert!(store.find_token("/admin/users").is_some());
        assert!(store.find_token("/administrator").is_none());
    }

    #[test]
    fn directives_reject_non_absolute_or_traversing_scopes() {
        assert!(
            SessionDirective::parse_set("token admin")
                .unwrap()
                .is_none()
        );
        assert!(
            SessionDirective::parse_set("token /a/../b")
                .unwrap()
                .is_none()
        );
        assert!(SessionDirective::parse_clear("/a?query").unwrap().is_none());
    }

    #[test]
    fn replace_existing_scope() {
        let mut store = SiteSessionStore::new();
        store.set_token(SessionToken {
            token: "old".into(),
            scope: "/".into(),
            expires: None,
        });
        store.set_token(SessionToken {
            token: "new".into(),
            scope: "/".into(),
            expires: None,
        });

        assert_eq!(store.tokens.len(), 1);
        assert_eq!(store.find_token("/").unwrap().token, "new");
    }

    #[test]
    fn clear_scope() {
        let mut store = SiteSessionStore::new();
        store.set_token(SessionToken {
            token: "tok".into(),
            scope: "/admin/".into(),
            expires: None,
        });
        store.clear_scope("/admin/");
        assert!(store.find_token("/admin/page").is_none());
    }

    #[test]
    fn expired_token_not_returned() {
        let mut store = SiteSessionStore::new();
        store.set_token(SessionToken {
            token: "expired".into(),
            scope: "/".into(),
            expires: Some(1), // Unix timestamp 1 — long expired
        });
        assert!(store.find_token("/").is_none());
    }

    #[test]
    fn per_site_limit() {
        let mut store = SiteSessionStore::new();
        for i in 0..MAX_SESSIONS_PER_SITE + 2 {
            store.set_token(SessionToken {
                token: format!("tok-{i}"),
                scope: format!("/scope-{i}/"),
                expires: None,
            });
        }
        assert!(store.tokens.len() <= MAX_SESSIONS_PER_SITE);
    }

    #[test]
    fn global_store_site_isolation() {
        let mut store = SessionStore::new();
        let site_a = origin("site-a.com");
        let site_b = origin("site-b.com");
        let site_c = origin("site-c.com");
        store.apply_directive(
            &site_a,
            &SessionDirective::Set {
                token: "a-token".into(),
                scope: "/".into(),
                expires: None,
            },
        );
        store.apply_directive(
            &site_b,
            &SessionDirective::Set {
                token: "b-token".into(),
                scope: "/".into(),
                expires: None,
            },
        );

        // site-a gets its own token
        assert_eq!(store.find_token(&site_a, "/").unwrap().token, "a-token");
        // site-b gets its own token
        assert_eq!(store.find_token(&site_b, "/").unwrap().token, "b-token");
        // unknown site gets nothing
        assert!(store.find_token(&site_c, "/").is_none());
    }

    #[test]
    fn retained_capacity_counts_every_nested_owner_and_fallible_clone() {
        let mut store = SessionStore::new();
        let site = origin("capacity.example");
        assert!(store.apply_directive(
            &site,
            &SessionDirective::Set {
                token: "retained-token".into(),
                scope: "/private".into(),
                expires: None,
            },
        ));
        let expected = store
            .sites
            .capacity()
            .saturating_mul(std::mem::size_of::<(Origin, SiteSessionStore)>())
            .saturating_add(store.sites[0].0.host_capacity())
            .saturating_add(
                store.sites[0]
                    .1
                    .tokens
                    .capacity()
                    .saturating_mul(std::mem::size_of::<SessionToken>()),
            )
            .saturating_add(store.sites[0].1.tokens[0].token.capacity())
            .saturating_add(store.sites[0].1.tokens[0].scope.capacity());
        assert_eq!(store.retained_capacity_bytes(), expected);

        let cloned = store.try_clone().unwrap();
        assert_eq!(cloned.total_count(), 1);
        assert_eq!(
            cloned.find_token(&site, "/private/page").unwrap(),
            store.find_token(&site, "/private/page").unwrap()
        );
        assert_eq!(
            cloned.retained_capacity_bytes(),
            cloned
                .sites
                .capacity()
                .saturating_mul(std::mem::size_of::<(Origin, SiteSessionStore)>())
                .saturating_add(cloned.sites[0].0.host_capacity())
                .saturating_add(
                    cloned.sites[0]
                        .1
                        .tokens
                        .capacity()
                        .saturating_mul(std::mem::size_of::<SessionToken>()),
                )
                .saturating_add(cloned.sites[0].1.tokens[0].token.capacity())
                .saturating_add(cloned.sites[0].1.tokens[0].scope.capacity())
        );
    }

    #[test]
    fn rejected_directives_never_create_empty_origin_buckets() {
        let mut store = SessionStore::new();
        for index in 0..MAX_TOTAL_SESSIONS {
            assert!(store.apply_directive(
                &origin(&format!("accepted-{index}.example")),
                &SessionDirective::Set {
                    token: format!("token-{index}"),
                    scope: "/".into(),
                    expires: None,
                },
            ));
        }
        assert_eq!(store.total_count(), MAX_TOTAL_SESSIONS);
        assert_eq!(store.iter_sites().count(), MAX_TOTAL_SESSIONS);

        for index in 0..512 {
            assert!(!store.apply_directive(
                &origin(&format!("rejected-{index}.example")),
                &SessionDirective::Set {
                    token: "rejected".into(),
                    scope: "/".into(),
                    expires: None,
                },
            ));
        }
        assert_eq!(store.total_count(), MAX_TOTAL_SESSIONS);
        assert_eq!(store.iter_sites().count(), MAX_TOTAL_SESSIONS);
    }

    #[test]
    fn clearing_the_last_token_removes_its_origin_bucket() {
        let mut store = SessionStore::new();
        let site = origin("clear.example");
        assert!(store.apply_directive(
            &site,
            &SessionDirective::Set {
                token: "token".into(),
                scope: "/".into(),
                expires: None,
            },
        ));
        assert_eq!(store.iter_sites().count(), 1);
        assert!(store.apply_directive(&site, &SessionDirective::Clear { scope: "/".into() },));
        assert_eq!(store.iter_sites().count(), 0);
    }

    #[test]
    fn generated_origin_matrix_partitions_session_state() {
        let ports = [1_u16, crate::protocol::DEFAULT_PORT, u16::MAX];
        let securities = TransportSecurity::ALL;
        let mut store = SessionStore::new();
        let mut expected = Vec::new();

        for port in ports {
            for security in securities {
                let uri = AtpUri::parse(&format!("atp://localhost:{port}/page?ignored=1")).unwrap();
                let origin = Origin::from_uri(&uri, security).unwrap();
                let token = format!("{security:?}-{port}");
                store.apply_directive(
                    &origin,
                    &SessionDirective::Set {
                        token: token.clone(),
                        scope: "/".into(),
                        expires: None,
                    },
                );
                expected.push((port, security, token));
            }
        }

        assert_eq!(store.iter_sites().count(), ports.len() * securities.len());
        for (port, security, token) in &expected {
            // A differently spelled authority must converge within the same
            // partition, while the matrix asserts distinct state across port
            // and transport-security dimensions.
            let alias = AtpUri::for_test("LOCALHOST.", *port, "/different", None);
            let alias_origin = Origin::from_uri(&alias, *security).unwrap();
            assert_eq!(
                store.find_token(&alias_origin, "/anything").unwrap().token,
                *token
            );
        }

        let removed = Origin::from_uri(
            &AtpUri::parse("atp://localhost:1/").unwrap(),
            TransportSecurity::InsecureTls,
        )
        .unwrap();
        store.clear_storage_key(&removed.storage_key().unwrap());
        assert!(store.find_token(&removed, "/").is_none());
        assert_eq!(
            store.iter_sites().count(),
            ports.len() * securities.len() - 1
        );
    }

    #[test]
    fn directive_serialize_roundtrip() {
        let dir = SessionDirective::Set {
            token: "abc123".into(),
            scope: "/admin/".into(),
            expires: Some(1700000000),
        };
        let s = dir.serialize().unwrap();
        assert_eq!(s, "Set-Session: abc123 /admin/ 1700000000\n");

        // Parse back
        let value = s.strip_prefix("Set-Session: ").unwrap().trim_end();
        let parsed = SessionDirective::parse_set(value).unwrap().unwrap();
        assert_eq!(parsed, dir);
    }

    #[test]
    fn directive_clear_roundtrip() {
        let dir = SessionDirective::Clear {
            scope: "/admin/".into(),
        };
        let s = dir.serialize().unwrap();
        assert_eq!(s, "Clear-Session: /admin/\n");

        let value = s.strip_prefix("Clear-Session: ").unwrap().trim_end();
        let parsed = SessionDirective::parse_clear(value).unwrap().unwrap();
        assert_eq!(parsed, dir);
    }

    #[test]
    fn directive_allocation_rejection_never_publishes_a_partial_value() {
        let set = SessionDirective::Set {
            token: "token".into(),
            scope: "/scope".into(),
            expires: Some(42),
        };
        let clear = SessionDirective::Clear {
            scope: "/scope".into(),
        };

        REJECT_DIRECTIVE_ALLOCATION
            .with(|reject| reject.set(Some(DirectiveAllocationSite::Serialize)));
        assert!(matches!(
            set.serialize(),
            Err(ProtocolError::ResourceExhausted { .. })
        ));

        REJECT_DIRECTIVE_ALLOCATION
            .with(|reject| reject.set(Some(DirectiveAllocationSite::ParseScope)));
        assert!(matches!(
            SessionDirective::parse_set("token /scope"),
            Err(ProtocolError::ResourceExhausted { .. })
        ));

        REJECT_DIRECTIVE_ALLOCATION
            .with(|reject| reject.set(Some(DirectiveAllocationSite::ParseClear)));
        assert!(matches!(
            SessionDirective::parse_clear("/scope"),
            Err(ProtocolError::ResourceExhausted { .. })
        ));

        assert_eq!(set.serialize().unwrap(), "Set-Session: token /scope 42\n");
        assert_eq!(clear.serialize().unwrap(), "Clear-Session: /scope\n");
    }

    #[test]
    fn directive_serialization_rejects_unbounded_public_values() {
        let oversized = SessionDirective::Set {
            token: "x".repeat(MAX_TOKEN_LEN + 1),
            scope: "/".into(),
            expires: None,
        };
        assert!(matches!(
            oversized.serialize(),
            Err(ProtocolError::InvalidMessage(_))
        ));
    }

    #[test]
    fn set_without_expiry() {
        let dir = SessionDirective::parse_set("mytoken /").unwrap().unwrap();
        assert_eq!(
            dir,
            SessionDirective::Set {
                token: "mytoken".into(),
                scope: "/".into(),
                expires: None,
            }
        );
    }

    #[test]
    fn rejects_oversized_token() {
        let big_token = "x".repeat(MAX_TOKEN_LEN + 1);
        assert!(
            SessionDirective::parse_set(&format!("{big_token} /"))
                .unwrap()
                .is_none()
        );
    }
}
