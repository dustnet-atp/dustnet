#![forbid(unsafe_code)]
// Remote content reaches every parser, layout and rendering path in this
// crate, so an operation that can
// panic on out-of-range input is a denial of service even though
// `forbid(unsafe_code)` bounds it to an abort. Denied in non-test builds only:
// tests assert on known-good values, where `unwrap` is the clearer spelling. A
// new panicking operation on a production path fails `make ci` until it is
// rewritten as a checked one, or exempted here with a reason.
#![cfg_attr(
    not(test),
    deny(
        clippy::indexing_slicing,
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic
    )
)]

//! Production terminal client and rendering runtime.

use dustnet_core::{color, parser, protocol, scanner, session};

mod client;
pub mod compositor;
pub mod config;
pub mod resource;
pub mod session_file;
mod session_store;
mod transport;
pub mod trust;
pub mod viewer;

pub use client::{TlsMode, TlsPolicy};
pub use resource::{BudgetError, BudgetLease, ResourceCategory, ResourceGovernor};
pub use viewer::{
    ControlToken, OperationOwner, PageScope, SubscriptionRegionKey, ViewerEffect, ViewerEvent,
    ViewerModel,
};

#[cfg(test)]
pub(crate) fn repository_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("client crate is nested under workspace/crates")
        .to_path_buf()
}
