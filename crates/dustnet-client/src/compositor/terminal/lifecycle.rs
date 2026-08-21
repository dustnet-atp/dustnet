//! Terminal restoration and shutdown execution belongs to this ownership
//! slice. The concrete guard remains in `terminal_lifecycle` so the CLI and
//! integration tests continue to share one implementation.

use std::io;

use crate::client::ClientError;

/// Errors that can occur in the connected viewer.
#[derive(Debug)]
pub enum ViewerError {
    /// Terminal I/O error.
    Io(io::Error),
    /// Network or protocol error.
    Client(ClientError),
    /// Failed to parse AML content.
    ParseFailed,
    /// An effect reached the wrong runtime ownership slice.
    UnexpectedEffect(&'static str),
    /// The user was asked whether to trust a site's certificate and declined.
    /// Distinct from a failure: nothing went wrong, the answer was no.
    TrustDeclined,
    /// The first navigation never reached a page, and this is why.
    ///
    /// Carries the reason rather than flattening it, because the viewer exits
    /// before a status bar exists to show it. A pinned-certificate mismatch
    /// surfaces here and nowhere else, and "failed to parse AML content" is
    /// not what someone being intercepted should be told.
    InitialNavigationFailed(String),
}

impl std::fmt::Display for ViewerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ViewerError::Io(e) => write!(f, "{e}"),
            ViewerError::Client(e) => write!(f, "{e}"),
            ViewerError::ParseFailed => write!(f, "failed to parse AML content"),
            ViewerError::TrustDeclined => {
                write!(f, "certificate not trusted; nothing was connected to")
            }
            ViewerError::InitialNavigationFailed(reason) => write!(f, "{reason}"),
            ViewerError::UnexpectedEffect(effect) => {
                write!(f, "unexpected viewer effect in terminal runtime: {effect}")
            }
        }
    }
}

impl std::error::Error for ViewerError {}

impl From<io::Error> for ViewerError {
    fn from(e: io::Error) -> Self {
        ViewerError::Io(e)
    }
}

impl From<ClientError> for ViewerError {
    fn from(e: ClientError) -> Self {
        ViewerError::Client(e)
    }
}
