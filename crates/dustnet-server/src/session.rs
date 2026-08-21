//! Resolving a session token to an identity, and issuing session directives.
//!
//! # The token stops here
//!
//! This is the whole reason the module exists. A request may carry a session
//! token; a [`SessionResolver`] turns that into an identity; and the include
//! resolver and input handler are told **the identity, never the token**.
//!
//! That boundary matters because a token is a bearer credential and an identity
//! is not. A handler holding a raw token could replay it, log it, or embed it in
//! a generated page — and a token in a page is a session-stealing bug that looks
//! like a rendering bug. A handler holding a username can do none of those
//! things, and the code that *can* is one small implementation of one trait.
//!
//! It is the same arrangement the quarantined prototype got right — the one
//! part of it worth keeping — with the difference that the trust boundary is a
//! trait here rather than a convention.
//!
//! # What a resolver is not asked to do
//!
//! Expiry, revocation and storage are the resolver's business, not this
//! module's. It is asked one question — who does this token belong to *now* —
//! and answering `None` for an expired or revoked token is how expiry is
//! enforced. There is deliberately no "is this valid" call separate from "who
//! is this", because two calls invite a gap between them.

use dustnet_core::session::SessionDirective;

/// Turns a session token into the identity it belongs to.
///
/// `None` means the token is unknown, expired or revoked. Those are one answer
/// on purpose: a caller that distinguished them would leak whether a token had
/// ever existed, and nothing downstream would do anything different.
pub trait SessionResolver: Send + Sync {
    fn identity(&self, token: &str) -> Option<String>;

    /// Destroy the session `token` names.
    ///
    /// Called by the server, not by a handler, when a handler's outcome clears
    /// the session — because a handler holds the *identity* and not the token,
    /// and so cannot name its own session to end it. Routing revocation
    /// through here keeps that boundary intact and still ends exactly the one
    /// session that was presented, rather than every session the person has.
    ///
    /// A no-op by default, for a resolver whose sessions cannot be revoked.
    fn revoke(&self, token: &str) {
        let _ = token;
    }
}

/// Who a request is from, as far as anything downstream is told.
///
/// `None` is anonymous. There is no variant for "presented a token we did not
/// recognise", because a handler should treat that identically to presenting
/// none — anything else is an oracle.
pub type Identity = Option<String>;

/// Session changes a handler wants applied to the response.
///
/// Returned rather than performed, so that issuing a session is visible in a
/// handler's return type instead of happening as a side effect somewhere in the
/// middle of it.
#[derive(Debug, Clone, Default)]
pub struct SessionChange {
    directives: Vec<SessionDirective>,
}

impl SessionChange {
    pub fn none() -> Self {
        Self::default()
    }

    /// Issue `token` for `scope`, expiring at the given Unix timestamp.
    ///
    /// The expiry travels to the client so it can drop the token when it lapses,
    /// but it is not what enforces expiry: a client that ignores it still gets
    /// `None` from the resolver. Client-side expiry is a courtesy, server-side
    /// expiry is the control.
    pub fn set(token: impl Into<String>, scope: impl Into<String>, expires: u64) -> Self {
        Self {
            directives: vec![SessionDirective::Set {
                token: token.into(),
                scope: scope.into(),
                expires: Some(expires),
            }],
        }
    }

    /// Clear whatever token the client holds for `scope`.
    ///
    /// Returning this also revokes the presented token server-side, through
    /// [`SessionResolver::revoke`]. The destruction is the part that matters: a
    /// client that ignores `Clear-Session` keeps a token, and that token has to
    /// stop working regardless.
    pub fn clear(scope: impl Into<String>) -> Self {
        Self {
            directives: vec![SessionDirective::Clear {
                scope: scope.into(),
            }],
        }
    }

    pub fn is_empty(&self) -> bool {
        self.directives.is_empty()
    }

    /// Whether this change clears the session, and so should revoke the token
    /// that was presented.
    pub(crate) fn clears(&self) -> bool {
        self.directives
            .iter()
            .any(|directive| matches!(directive, SessionDirective::Clear { .. }))
    }

    pub(crate) fn into_directives(self) -> Vec<SessionDirective> {
        self.directives
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_set_carries_its_scope_and_expiry() {
        let change = SessionChange::set("abc", "/", 1_800_000_000);
        let directives = change.into_directives();
        assert_eq!(directives.len(), 1);
        match &directives[0] {
            SessionDirective::Set {
                token,
                scope,
                expires,
            } => {
                assert_eq!(token, "abc");
                assert_eq!(scope, "/");
                assert_eq!(*expires, Some(1_800_000_000));
            }
            other => panic!("expected Set, got {other:?}"),
        }
    }

    #[test]
    fn a_clear_names_only_its_scope() {
        match &SessionChange::clear("/").into_directives()[0] {
            SessionDirective::Clear { scope } => assert_eq!(scope, "/"),
            other => panic!("expected Clear, got {other:?}"),
        }
    }

    #[test]
    fn only_a_clear_asks_for_revocation() {
        assert!(SessionChange::clear("/").clears());
        assert!(!SessionChange::set("t", "/", 1).clears());
        assert!(!SessionChange::none().clears());
    }

    #[test]
    fn no_change_is_empty() {
        assert!(SessionChange::none().is_empty());
        assert!(SessionChange::none().into_directives().is_empty());
    }

    /// A directive must survive serialization, since an invalid one is refused
    /// at encode time and would turn a successful login into a failed response.
    #[test]
    fn issued_directives_serialize() {
        for change in [
            SessionChange::set("a".repeat(64), "/", 1_800_000_000),
            SessionChange::clear("/"),
        ] {
            for directive in change.into_directives() {
                directive.serialize().expect("directive serializes");
            }
        }
    }
}
