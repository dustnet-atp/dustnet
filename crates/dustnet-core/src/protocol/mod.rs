pub mod frame;
pub mod message;
pub mod origin;
pub mod state;
pub mod uri;

use std::borrow::Cow;
use std::fmt::Write as _;

/// Default ATP port (1985 — the year FidoNet echomail launched).
pub const DEFAULT_PORT: u16 = 1985;

/// Maximum frame size: 16 MiB.
pub const MAX_FRAME_SIZE: u32 = 16 * 1024 * 1024;

/// Maximum bodies accepted for small protocol-control messages.
pub const MAX_CONTROL_MESSAGE_SIZE: usize = 16 * 1024;

/// Maximum serialized form submission accepted by the reference server.
/// Individual fields have much smaller application-level limits; this bound
/// protects the protocol layer before it allocates the frame body.
pub const MAX_INPUT_MESSAGE_SIZE: usize = 64 * 1024;

/// PAGE may contain up to 1 MiB of AML plus bounded session metadata.
pub const MAX_PAGE_MESSAGE_SIZE: usize = 1024 * 1024 + 64 * 1024;

/// Maximum accepted size of a remotely fetched WASM module.
/// Enforced by both clients and servers.
pub const MAX_WASM_MODULE_SIZE: usize = 512 * 1024;

/// Maximum accepted serialized UPDATE body (1 MiB).
///
/// UPDATEs are AML fragments and should never need the protocol-wide 16 MiB
/// frame allowance. Keeping a separate bound prevents a live region from
/// becoming an unbounded memory channel.
pub const MAX_LIVE_UPDATE_SIZE: usize = 1024 * 1024;

/// Header size in bytes (4 length + 1 type + 1 flags).
pub const HEADER_SIZE: usize = 6;

/// Maximum retained diagnostic text attached to a protocol error.
pub const MAX_PROTOCOL_DIAGNOSTIC_BYTES: usize = 2048;

/// Preview protocol contract. ATP/AML 1.0 remains intentionally unfrozen.
pub const PROTOCOL_VERSION: &str = "0.2";

/// Capabilities implemented by this reference client and server.
pub const SUPPORTED_CAPABILITIES: &[&str] = &["live-updates", "sessions", "wasm-effects"];

/// A validated ATP protocol version negotiated during HELLO/WELCOME.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProtocolVersion {
    pub major: u16,
    pub minor: u16,
}

impl ProtocolVersion {
    pub fn parse(value: &str) -> Result<Self, ProtocolError> {
        let (major, minor) = value.split_once('.').ok_or_else(|| {
            ProtocolError::invalid_message(format_args!("invalid protocol version: {value}"))
        })?;
        let version = Self {
            major: major.parse().map_err(|_| {
                ProtocolError::invalid_message(format_args!("invalid protocol version: {value}"))
            })?,
            minor: minor.parse().map_err(|_| {
                ProtocolError::invalid_message(format_args!("invalid protocol version: {value}"))
            })?,
        };
        if version.to_string() != value {
            return Err(ProtocolError::invalid_message(format_args!(
                "non-canonical protocol version: {value}"
            )));
        }
        Ok(version)
    }
}

impl std::fmt::Display for ProtocolVersion {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}.{}", self.major, self.minor)
    }
}

/// Capabilities accepted by both peers. Unknown capabilities are never enabled.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NegotiatedCapabilities(u8);

impl NegotiatedCapabilities {
    const LIVE_UPDATES: u8 = 1 << 0;
    const SESSIONS: u8 = 1 << 1;
    const WASM_EFFECTS: u8 = 1 << 2;

    pub fn negotiate(offered: &[String], accepted: &[String]) -> Self {
        let mut bits = 0;
        for capability in offered {
            if !accepted.contains(capability) {
                continue;
            }
            bits |= match capability.as_str() {
                "live-updates" => Self::LIVE_UPDATES,
                "sessions" => Self::SESSIONS,
                "wasm-effects" => Self::WASM_EFFECTS,
                _ => 0,
            };
        }
        Self(bits)
    }

    pub fn live_updates(self) -> bool {
        self.0 & Self::LIVE_UPDATES != 0
    }
    pub fn sessions(self) -> bool {
        self.0 & Self::SESSIONS != 0
    }
    pub fn wasm_effects(self) -> bool {
        self.0 & Self::WASM_EFFECTS != 0
    }
}

/// Typed result of a successful HELLO/WELCOME negotiation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NegotiatedProtocol {
    pub version: ProtocolVersion,
    pub capabilities: NegotiatedCapabilities,
}

/// Protocol errors.
#[derive(Debug)]
pub enum ProtocolError {
    /// Frame exceeds maximum size.
    FrameTooLarge(u32),
    /// A known message type exceeds its direction-specific body limit.
    MessageTooLarge {
        msg_type: frame::MessageType,
        size: u32,
        max: usize,
    },
    /// A peer sent a message reserved for the opposite direction.
    WrongDirection(frame::MessageType),
    /// Unknown message type code.
    UnknownMessageType(u8),
    /// Invalid message body.
    InvalidMessage(Cow<'static, str>),
    /// I/O error.
    Io(std::io::Error),
    /// TLS error.
    Tls(Cow<'static, str>),
    /// URI parse error.
    InvalidUri(Cow<'static, str>),
    /// Connection closed.
    ConnectionClosed,
    /// The process could not reserve bounded protocol storage.
    ResourceExhausted { requested: usize },
    /// Timeout.
    Timeout,
}

impl ProtocolError {
    pub fn invalid_message(args: std::fmt::Arguments<'_>) -> Self {
        match try_protocol_diagnostic(args) {
            Ok(message) => Self::InvalidMessage(Cow::Owned(message)),
            Err(requested) => Self::ResourceExhausted { requested },
        }
    }

    pub fn invalid_uri(args: std::fmt::Arguments<'_>) -> Self {
        match try_protocol_diagnostic(args) {
            Ok(message) => Self::InvalidUri(Cow::Owned(message)),
            Err(requested) => Self::ResourceExhausted { requested },
        }
    }

    pub fn tls(args: std::fmt::Arguments<'_>) -> Self {
        match try_protocol_diagnostic(args) {
            Ok(message) => Self::Tls(Cow::Owned(message)),
            Err(requested) => Self::ResourceExhausted { requested },
        }
    }
}

struct ProtocolDiagnosticWriter {
    value: String,
    requested: usize,
    rejected: bool,
    truncated: bool,
}

impl std::fmt::Write for ProtocolDiagnosticWriter {
    fn write_str(&mut self, text: &str) -> std::fmt::Result {
        self.requested = self.requested.saturating_add(text.len());
        if self.truncated {
            return Ok(());
        }
        let remaining = MAX_PROTOCOL_DIAGNOSTIC_BYTES.saturating_sub(self.value.len());
        if remaining == 0 {
            self.truncated = true;
            return Ok(());
        }
        let mut take = remaining.min(text.len());
        while take > 0 && !text.is_char_boundary(take) {
            take -= 1;
        }
        if take == 0 {
            self.truncated = true;
            return Ok(());
        }
        #[cfg(test)]
        if REJECT_PROTOCOL_DIAGNOSTIC_ALLOCATION.with(|reject| reject.replace(false)) {
            self.rejected = true;
            return Err(std::fmt::Error);
        }
        self.value.try_reserve_exact(take).map_err(|_| {
            self.rejected = true;
            std::fmt::Error
        })?;
        self.value.push_str(&text[..take]);
        if take < text.len() {
            self.truncated = true;
        }
        Ok(())
    }
}

fn try_protocol_diagnostic(args: std::fmt::Arguments<'_>) -> Result<String, usize> {
    let mut writer = ProtocolDiagnosticWriter {
        value: String::new(),
        requested: 0,
        rejected: false,
        truncated: false,
    };
    if writer.write_fmt(args).is_err() || writer.rejected {
        return Err(writer.requested.max(1));
    }
    Ok(writer.value)
}

#[cfg(test)]
thread_local! {
    static REJECT_PROTOCOL_DIAGNOSTIC_ALLOCATION: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProtocolError::FrameTooLarge(size) => write!(f, "frame too large: {size} bytes"),
            ProtocolError::MessageTooLarge {
                msg_type,
                size,
                max,
            } => {
                write!(f, "{msg_type:?} body too large: {size} bytes (max {max})")
            }
            ProtocolError::WrongDirection(msg_type) => {
                write!(
                    f,
                    "message {msg_type:?} is invalid in this protocol direction"
                )
            }
            ProtocolError::UnknownMessageType(code) => {
                write!(f, "unknown message type: 0x{code:02x}")
            }
            ProtocolError::InvalidMessage(msg) => write!(f, "invalid message: {msg}"),
            ProtocolError::Io(e) => write!(f, "I/O error: {e}"),
            ProtocolError::Tls(msg) => write!(f, "TLS error: {msg}"),
            ProtocolError::InvalidUri(msg) => write!(f, "invalid URI: {msg}"),
            ProtocolError::ConnectionClosed => write!(f, "connection closed"),
            ProtocolError::ResourceExhausted { requested } => {
                write!(
                    f,
                    "unable to reserve {requested} bytes for bounded protocol storage"
                )
            }
            ProtocolError::Timeout => write!(f, "timeout"),
        }
    }
}

impl std::error::Error for ProtocolError {}

impl From<std::io::Error> for ProtocolError {
    fn from(e: std::io::Error) -> Self {
        match e.kind() {
            std::io::ErrorKind::UnexpectedEof => ProtocolError::ConnectionClosed,
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => ProtocolError::Timeout,
            _ => ProtocolError::Io(e),
        }
    }
}

#[cfg(test)]
mod diagnostic_tests {
    use super::*;

    #[test]
    fn protocol_diagnostic_formatting_is_bounded_and_fallible() {
        let ProtocolError::InvalidMessage(Cow::Borrowed(message)) =
            ProtocolError::InvalidMessage("static diagnostic".into())
        else {
            panic!("static diagnostics must remain allocation-free");
        };
        assert_eq!(message, "static diagnostic");

        let oversized = "é".repeat(MAX_PROTOCOL_DIAGNOSTIC_BYTES);
        let ProtocolError::InvalidMessage(message) =
            ProtocolError::invalid_message(format_args!("prefix:{oversized}"))
        else {
            panic!("bounded formatting must produce an invalid-message diagnostic");
        };
        assert!(message.len() <= MAX_PROTOCOL_DIAGNOSTIC_BYTES);
        assert!(message.is_char_boundary(message.len()));
        assert!(message.starts_with("prefix:"));

        let prefix = "a".repeat(MAX_PROTOCOL_DIAGNOSTIC_BYTES - 1);
        let ProtocolError::InvalidMessage(message) =
            ProtocolError::invalid_message(format_args!("{prefix}{}{}", "é", "Z"))
        else {
            panic!("segmented formatting must remain a diagnostic");
        };
        assert_eq!(message.as_ref(), prefix);

        REJECT_PROTOCOL_DIAGNOSTIC_ALLOCATION.with(|reject| reject.set(true));
        assert!(matches!(
            ProtocolError::invalid_message(format_args!("remote {oversized}")),
            ProtocolError::ResourceExhausted { .. }
        ));
    }
}
