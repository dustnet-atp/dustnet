#![forbid(unsafe_code)]
// Remote content reaches every parser and protocol path in this crate, so an
// operation that can panic on
// out-of-range input is a denial of service even though `forbid(unsafe_code)`
// bounds it to an abort. Denied in
// non-test builds only: tests assert on known-good values, where `unwrap` is
// the clearer spelling. A new panicking operation on a production path fails
// `make ci` until it is rewritten as a checked one, or exempted here with a
// reason.
#![cfg_attr(
    not(test),
    deny(
        clippy::indexing_slicing,
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic
    )
)]

//! ATP/AML parsing and protocol primitives with no client or server dependency.

pub mod color;
pub mod parser;
pub mod protocol;
pub mod scanner;
pub mod serialize;
pub mod session;

pub use protocol::origin::{Origin, TransportSecurity};
pub use protocol::{NegotiatedCapabilities, ProtocolVersion};
