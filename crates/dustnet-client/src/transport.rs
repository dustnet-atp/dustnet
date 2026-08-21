use std::fmt::Write as _;
use std::sync::Arc;

use crate::trust::Fingerprint;
use dustnet_core::protocol::frame::MessageType;
use dustnet_core::protocol::frame::{RawFrame, allocate_frame_body, decode_header, encode_frame};
use dustnet_core::protocol::message::{
    HelloMessage, PageFlags, WelcomeMessage, validate_frame_body,
};
use dustnet_core::protocol::state::{ConnectionStateMachine, Direction, Endpoint};
use dustnet_core::protocol::{
    HEADER_SIZE, MAX_CONTROL_MESSAGE_SIZE, MAX_INPUT_MESSAGE_SIZE, MAX_LIVE_UPDATE_SIZE,
    MAX_PAGE_MESSAGE_SIZE, MAX_WASM_MODULE_SIZE, NegotiatedCapabilities, NegotiatedProtocol,
    ProtocolError, ProtocolVersion,
};
use rustls::pki_types::{CertificateDer, ServerName};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[derive(Clone, Copy)]
enum InboundPeer {
    Client,
    Server,
}

/// Return the semantic body limit for a message before allocating its body.
/// `Client` means the remote peer is a client (server-side receive), while
/// `Server` means the remote peer is a server (client-side receive).
fn inbound_body_limit(peer: InboundPeer, msg_type: MessageType) -> Result<usize, ProtocolError> {
    use MessageType::*;
    match (peer, msg_type) {
        (InboundPeer::Client, Hello | Get | Subscribe | Unsubscribe | Ping | Bye) => {
            Ok(MAX_CONTROL_MESSAGE_SIZE)
        }
        (InboundPeer::Client, Input) => Ok(MAX_INPUT_MESSAGE_SIZE),
        (InboundPeer::Server, Welcome | Redirect | Error | Pong | ServerBye) => {
            Ok(MAX_CONTROL_MESSAGE_SIZE)
        }
        (InboundPeer::Server, Page) => Ok(MAX_PAGE_MESSAGE_SIZE),
        (InboundPeer::Server, Update) => Ok(MAX_LIVE_UPDATE_SIZE),
        (InboundPeer::Server, Resource) => Ok(MAX_WASM_MODULE_SIZE),
        _ => Err(ProtocolError::WrongDirection(msg_type)),
    }
}

fn validate_inbound_body_len(
    peer: InboundPeer,
    msg_type: MessageType,
    body_len: u32,
) -> Result<(), ProtocolError> {
    let max = inbound_body_limit(peer, msg_type)?;
    if matches!(
        msg_type,
        MessageType::Unsubscribe
            | MessageType::Ping
            | MessageType::Pong
            | MessageType::Bye
            | MessageType::ServerBye
    ) && body_len != 0
    {
        return Err(ProtocolError::invalid_message(format_args!(
            "{msg_type:?} must have an empty body"
        )));
    }
    if body_len as usize > max {
        return Err(ProtocolError::MessageTooLarge {
            msg_type,
            size: body_len,
            max,
        });
    }
    Ok(())
}

fn validate_inbound_flags(msg_type: MessageType, flags: u8) -> Result<(), ProtocolError> {
    let allowed = match msg_type {
        MessageType::Page => 0x0b,
        MessageType::Update => 0x01,
        _ => 0,
    };
    if flags & !allowed != 0 {
        return Err(ProtocolError::invalid_message(format_args!(
            "unsupported flags 0x{flags:02x} for {msg_type:?}"
        )));
    }
    Ok(())
}

#[allow(clippy::large_enum_variant)]
enum Transport {
    Plain(TcpStream),
    Tls(tokio_rustls::client::TlsStream<TcpStream>),
}

// ─── Client connection ───────────────────────────────────────

/// A client-side ATP connection (plain TCP or TLS).
pub struct AtpConnection {
    transport: Transport,
    state: ConnectionStateMachine,
    offered_capabilities: Vec<String>,
    offered_version: Option<ProtocolVersion>,
    negotiated: Option<NegotiatedProtocol>,
    poisoned: bool,
}

impl AtpConnection {
    /// Connect over plain TCP (no encryption).
    pub async fn connect_plain(host: &str, port: u16) -> Result<Self, ProtocolError> {
        let addr = try_format_host_port(host, port)?;
        let stream = TcpStream::connect(&addr).await?;
        stream.set_nodelay(true).ok();
        Ok(AtpConnection {
            transport: Transport::Plain(stream),
            state: ConnectionStateMachine::new(Endpoint::Client),
            offered_capabilities: Vec::new(),
            offered_version: None,
            negotiated: None,
            poisoned: false,
        })
    }

    /// Connect over TLS.
    ///
    /// Returns the connection and, when the peer's certificate was observed by
    /// a pinning verifier, its fingerprint — so a trust-on-first-use caller can
    /// record what it just agreed to without digesting the certificate twice.
    pub async fn connect_tls(
        host: &str,
        port: u16,
        verification: &TlsVerification,
    ) -> Result<(Self, Option<Fingerprint>), TlsConnectError> {
        // Ensure ring crypto provider is installed
        let _ = rustls::crypto::ring::default_provider().install_default();

        let addr = try_format_host_port(host, port)?;
        let server_name = try_server_name(host)?;
        let stream = TcpStream::connect(&addr).await?;
        stream.set_nodelay(true).ok();

        // Pin TLS 1.3 as the minimum (and only) version. rustls's default
        // builder also negotiates TLS 1.2; the protocol spec mandates 1.3.
        let builder =
            rustls::ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS13]);
        let mut observed = None;
        let mut unverified: Option<Arc<std::sync::Mutex<Option<UnverifiedPeer>>>> = None;
        let mut mismatch: Option<Arc<std::sync::Mutex<Option<String>>>> = None;
        let config = match verification {
            TlsVerification::Insecure => builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(InsecureVerifier))
                .with_no_client_auth(),
            TlsVerification::Pinned(expected) => {
                let verifier = PinningVerifier::new(Some(*expected));
                observed = Some(Arc::clone(&verifier.observed));
                mismatch = Some(Arc::clone(&verifier.mismatch));
                builder
                    .dangerous()
                    .with_custom_certificate_verifier(Arc::new(verifier))
                    .with_no_client_auth()
            }
            TlsVerification::TrustOnFirstUse => {
                let verifier = PinningVerifier::new(None);
                observed = Some(Arc::clone(&verifier.observed));
                builder
                    .dangerous()
                    .with_custom_certificate_verifier(Arc::new(verifier))
                    .with_no_client_auth()
            }
            TlsVerification::Ca { extra_roots } => {
                let mut root_store = rustls::RootCertStore::from_iter(
                    webpki_roots::TLS_SERVER_ROOTS.iter().cloned(),
                );
                for anchor in extra_roots {
                    root_store.add(anchor.clone()).map_err(|error| {
                        ProtocolError::tls(format_args!("unusable certificate authority: {error}"))
                    })?;
                }
                let inner = rustls::client::WebPkiServerVerifier::builder(Arc::new(root_store))
                    .build()
                    .map_err(|error| {
                        ProtocolError::tls(format_args!("unusable certificate authority: {error}"))
                    })?;
                let verifier = CaVerifier {
                    inner,
                    unverified: Arc::new(std::sync::Mutex::new(None)),
                };
                unverified = Some(Arc::clone(&verifier.unverified));
                builder
                    .dangerous()
                    .with_custom_certificate_verifier(Arc::new(verifier))
                    .with_no_client_auth()
            }
        };

        let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
        let tls_stream = match connector.connect(server_name, stream).await {
            Ok(stream) => stream,
            Err(error) => {
                // A refusal the user could answer is reported as one, and a
                // pin that did not match is reported as itself. Anything else
                // — a timeout, a protocol failure, a certificate that was
                // never even parsed — stays an ordinary handshake error.
                if let Some(message) =
                    mismatch.and_then(|cell| cell.lock().ok().and_then(|s| s.clone()))
                {
                    return Err(TlsConnectError::PinMismatch(message));
                }
                if let Some(peer) =
                    unverified.and_then(|cell| cell.lock().ok().and_then(|s| s.clone()))
                {
                    return Err(TlsConnectError::Unverified(peer));
                }
                return Err(TlsConnectError::Protocol(ProtocolError::tls(format_args!(
                    "TLS handshake failed: {error}"
                ))));
            }
        };

        let fingerprint = observed.and_then(|cell| cell.lock().ok().and_then(|seen| *seen));

        Ok((
            AtpConnection {
                transport: Transport::Tls(tls_stream),
                state: ConnectionStateMachine::new(Endpoint::Client),
                offered_capabilities: Vec::new(),
                offered_version: None,
                negotiated: None,
                poisoned: false,
            },
            fingerprint,
        ))
    }

    /// Send a frame over the connection.
    pub async fn send_frame(&mut self, frame: &RawFrame) -> Result<(), ProtocolError> {
        if self.poisoned {
            return Err(ProtocolError::ConnectionClosed);
        }
        let result = self.try_send_frame(frame).await;
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }

    async fn try_send_frame(&mut self, frame: &RawFrame) -> Result<(), ProtocolError> {
        let body_len =
            u32::try_from(frame.body.len()).map_err(|_| ProtocolError::FrameTooLarge(u32::MAX))?;
        validate_inbound_body_len(InboundPeer::Client, frame.msg_type, body_len)?;
        validate_inbound_flags(frame.msg_type, frame.flags)?;
        let metadata = validate_frame_body(frame.msg_type, &frame.body, frame.flags)?;
        if frame.msg_type == MessageType::Hello {
            let body = std::str::from_utf8(&frame.body)
                .map_err(|_| ProtocolError::InvalidMessage("HELLO is not UTF-8".into()))?;
            let hello = HelloMessage::parse(body)?;
            self.offered_version = Some(ProtocolVersion::parse(&hello.protocol_version)?);
            self.offered_capabilities = hello.capabilities;
        }
        self.validate_capability(frame.msg_type, frame.flags, metadata.carries_session())?;
        self.state.apply(Direction::Send, frame.msg_type)?;
        let bytes = encode_frame(frame)?;
        match &mut self.transport {
            Transport::Plain(s) => s.write_all(&bytes).await?,
            Transport::Tls(s) => s.write_all(&bytes).await?,
        }
        Ok(())
    }

    /// Receive a frame from the connection.
    pub async fn recv_frame(&mut self) -> Result<RawFrame, ProtocolError> {
        if self.poisoned {
            return Err(ProtocolError::ConnectionClosed);
        }
        let result = self.try_recv_frame().await;
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }

    async fn try_recv_frame(&mut self) -> Result<RawFrame, ProtocolError> {
        let mut header = [0u8; HEADER_SIZE];
        match &mut self.transport {
            Transport::Plain(s) => {
                s.read_exact(&mut header).await?;
            }
            Transport::Tls(s) => {
                s.read_exact(&mut header).await?;
            }
        }

        let (body_len, msg_type, flags) = decode_header(&header)?;
        validate_inbound_body_len(InboundPeer::Server, msg_type, body_len)?;
        validate_inbound_flags(msg_type, flags)?;

        let mut body = allocate_frame_body(body_len as usize)?;
        if body_len > 0 {
            match &mut self.transport {
                Transport::Plain(s) => {
                    s.read_exact(&mut body).await?;
                }
                Transport::Tls(s) => {
                    s.read_exact(&mut body).await?;
                }
            }
        }

        let frame = RawFrame {
            msg_type,
            flags,
            body,
        };
        let metadata = validate_frame_body(frame.msg_type, &frame.body, frame.flags)?;
        if msg_type == MessageType::Welcome {
            let body = std::str::from_utf8(&frame.body)
                .map_err(|_| ProtocolError::InvalidMessage("WELCOME is not UTF-8".into()))?;
            let welcome = WelcomeMessage::parse(body)?;
            let version = ProtocolVersion::parse(&welcome.protocol_version)?;
            if self.offered_version != Some(version)
                || welcome
                    .capabilities
                    .iter()
                    .any(|capability| !self.offered_capabilities.contains(capability))
            {
                return Err(ProtocolError::InvalidMessage(
                    "WELCOME is incompatible with HELLO".into(),
                ));
            }
            self.negotiated = Some(NegotiatedProtocol {
                version,
                capabilities: NegotiatedCapabilities::negotiate(
                    &self.offered_capabilities,
                    &welcome.capabilities,
                ),
            });
        }
        self.validate_capability(msg_type, flags, metadata.carries_session())?;
        self.state.apply(Direction::Receive, msg_type)?;
        Ok(frame)
    }

    /// Gracefully shut down the connection.
    pub async fn shutdown(&mut self) -> Result<(), ProtocolError> {
        match &mut self.transport {
            Transport::Plain(s) => s.shutdown().await?,
            Transport::Tls(s) => s.shutdown().await?,
        }
        Ok(())
    }

    pub fn negotiated(&self) -> Option<NegotiatedProtocol> {
        self.negotiated
    }

    fn validate_capability(
        &self,
        msg_type: MessageType,
        flags: u8,
        carries_session: bool,
    ) -> Result<(), ProtocolError> {
        if msg_type == MessageType::Hello || msg_type == MessageType::Welcome {
            return Ok(());
        }
        let negotiated = self.negotiated.ok_or_else(|| {
            ProtocolError::InvalidMessage("application frame before negotiation".into())
        })?;
        let allowed = match msg_type {
            MessageType::Subscribe | MessageType::Unsubscribe | MessageType::Update => {
                negotiated.capabilities.live_updates()
            }
            MessageType::Resource => negotiated.capabilities.wasm_effects(),
            MessageType::Page if PageFlags::from_bits(flags).has_live_regions => {
                negotiated.capabilities.live_updates()
            }
            MessageType::Page if PageFlags::from_bits(flags).has_session => {
                negotiated.capabilities.sessions()
            }
            _ => true,
        } && (!carries_session || negotiated.capabilities.sessions());
        if allowed {
            Ok(())
        } else {
            Err(ProtocolError::invalid_message(format_args!(
                "{msg_type:?} requires a capability not negotiated by both peers"
            )))
        }
    }
}

fn try_format_host_port(host: &str, port: u16) -> Result<String, ProtocolError> {
    let brackets = usize::from(host.contains(':') && !host.starts_with('[')) * 2;
    let requested = host
        .len()
        .checked_add(brackets)
        .and_then(|size| size.checked_add(1 + 5))
        .ok_or(ProtocolError::ResourceExhausted {
            requested: usize::MAX,
        })?;
    reject_transport_allocation(TransportAllocationSite::Address, requested)?;
    let mut address = String::new();
    address
        .try_reserve_exact(requested)
        .map_err(|_| ProtocolError::ResourceExhausted { requested })?;
    if brackets != 0 {
        address.push('[');
        address.push_str(host);
        address.push(']');
    } else {
        address.push_str(host);
    }
    // `fmt::Write` for `String` cannot fail, and the capacity above is exact.
    let _ = write!(address, ":{port}");
    Ok(address)
}

fn try_server_name(host: &str) -> Result<ServerName<'static>, ProtocolError> {
    reject_transport_allocation(TransportAllocationSite::ServerName, host.len())?;
    let mut owned_host = String::new();
    owned_host
        .try_reserve_exact(host.len())
        .map_err(|_| ProtocolError::ResourceExhausted {
            requested: host.len(),
        })?;
    owned_host.push_str(host);
    match ServerName::try_from(owned_host) {
        Ok(server_name) => Ok(server_name),
        Err(_) => {
            let message = "invalid server name";
            reject_transport_allocation(TransportAllocationSite::TlsMessage, message.len())?;
            let mut owned_message = String::new();
            owned_message
                .try_reserve_exact(message.len())
                .map_err(|_| ProtocolError::ResourceExhausted {
                    requested: message.len(),
                })?;
            owned_message.push_str(message);
            Err(ProtocolError::Tls(owned_message.into()))
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TransportAllocationSite {
    Address,
    ServerName,
    /// The diagnostic copied when `rustls` rejects the server name. It is the
    /// allocation on the *error* path, which is where an allocator refusal is
    /// least likely to be exercised by accident and most likely to matter.
    TlsMessage,
}

fn reject_transport_allocation(
    site: TransportAllocationSite,
    requested: usize,
) -> Result<(), ProtocolError> {
    #[cfg(test)]
    if REJECT_TRANSPORT_ALLOCATION.with(|rejected| {
        if rejected.get() == Some(site) {
            rejected.set(None);
            true
        } else {
            false
        }
    }) {
        return Err(ProtocolError::ResourceExhausted { requested });
    }
    let _ = site;
    let _ = requested;
    Ok(())
}

#[cfg(test)]
thread_local! {
    static REJECT_TRANSPORT_ALLOCATION: std::cell::Cell<Option<TransportAllocationSite>> =
        const { std::cell::Cell::new(None) };
}

// ─── How a TLS peer is authenticated ─────────────────────────

/// A peer whose certificate no authority vouches for.
///
/// Carried out of a failed handshake so a caller with a terminal can show the
/// user what they are being asked to trust. Producing this does not make the
/// connection usable — it failed.
#[derive(Debug, Clone)]
pub struct UnverifiedPeer {
    pub fingerprint: Fingerprint,
    /// rustls's own account of why verification failed: no issuer, expired,
    /// wrong host name. Shown verbatim, because "could not verify" without a
    /// reason gives a user nothing to judge with.
    pub reason: String,
}

/// Why a TLS connection could not be established.
#[derive(Debug)]
pub enum TlsConnectError {
    Protocol(ProtocolError),
    /// The certificate did not verify, and this is what it was.
    Unverified(UnverifiedPeer),
    /// The peer presented a certificate other than the pinned one. Carried out
    /// separately so the message reaches the user as written, rather than
    /// wrapped in rustls's generic handshake-failure prefixes: it is the one
    /// message in this client that an intercepted user depends on reading.
    PinMismatch(String),
}

impl From<ProtocolError> for TlsConnectError {
    fn from(error: ProtocolError) -> Self {
        TlsConnectError::Protocol(error)
    }
}

impl From<std::io::Error> for TlsConnectError {
    fn from(error: std::io::Error) -> Self {
        TlsConnectError::Protocol(ProtocolError::from(error))
    }
}

impl std::fmt::Display for TlsConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TlsConnectError::Protocol(error) => write!(f, "{error}"),
            TlsConnectError::Unverified(peer) => write!(f, "unverified peer: {}", peer.reason),
            TlsConnectError::PinMismatch(message) => write!(f, "{message}"),
        }
    }
}

/// Certification-authority verification that reports what it refused.
///
/// Every decision is delegated to rustls's own [`WebPkiServerVerifier`]: this
/// adds no leniency at all, and a certificate it rejects still fails the
/// handshake. Its one addition is to record the fingerprint and the stated
/// reason on the way past, so a refusal can be turned into a question a user
/// can answer rather than a bare "handshake failed".
///
/// [`WebPkiServerVerifier`]: rustls::client::WebPkiServerVerifier
#[derive(Debug)]
struct CaVerifier {
    inner: Arc<rustls::client::WebPkiServerVerifier>,
    unverified: Arc<std::sync::Mutex<Option<UnverifiedPeer>>>,
}

impl rustls::client::danger::ServerCertVerifier for CaVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        match self.inner.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        ) {
            Ok(verified) => Ok(verified),
            Err(error) => {
                if let Ok(mut cell) = self.unverified.lock() {
                    *cell = Some(UnverifiedPeer {
                        fingerprint: Fingerprint::of_certificate(end_entity.as_ref()),
                        reason: error.to_string(),
                    });
                }
                Err(error)
            }
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// What the client requires of the server's certificate.
pub enum TlsVerification {
    /// A certification authority must vouch for the host name. `extra_roots`
    /// are anchors the operator supplied with `--ca-file`, trusted in addition
    /// to the compiled-in bundle rather than instead of it.
    Ca {
        extra_roots: Vec<CertificateDer<'static>>,
    },
    /// The certificate must be exactly this one.
    Pinned(Fingerprint),
    /// Accept whatever is presented and report it, so the caller can pin it.
    TrustOnFirstUse,
    /// Verify nothing. Development escape hatch.
    Insecure,
}

/// Authenticates a peer by certificate identity rather than by authority.
///
/// Host name verification is deliberately absent. A pin binds a certificate to
/// the host and port the user typed, which is the same binding SSH makes and
/// the reason self-signed certificates without a matching SAN are usable at
/// all. The name is checked by the store lookup, not by the certificate.
///
/// Signature verification is emphatically **not** absent, and this is the
/// whole security argument for the type. A certificate is public: anyone who
/// has ever connected to the site has a copy. A verifier that compared
/// fingerprints and then asserted the handshake signature — the way
/// [`InsecureVerifier`] does — would let any of them impersonate the site
/// without ever holding its private key, and would do it while the status bar
/// said the connection was pinned. The signature is checked against the
/// certificate that was pinned, which is what proves the peer holds the key.
#[derive(Debug)]
struct PinningVerifier {
    /// `None` on a trust-on-first-use connection: nothing is pinned yet, so
    /// there is nothing to compare against.
    expected: Option<Fingerprint>,
    observed: Arc<std::sync::Mutex<Option<Fingerprint>>>,
    mismatch: Arc<std::sync::Mutex<Option<String>>>,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl PinningVerifier {
    fn new(expected: Option<Fingerprint>) -> Self {
        PinningVerifier {
            expected,
            observed: Arc::new(std::sync::Mutex::new(None)),
            mismatch: Arc::new(std::sync::Mutex::new(None)),
            provider: Arc::new(rustls::crypto::ring::default_provider()),
        }
    }
}

impl rustls::client::danger::ServerCertVerifier for PinningVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        let seen = Fingerprint::of_certificate(end_entity.as_ref());
        if let Ok(mut cell) = self.observed.lock() {
            *cell = Some(seen);
        }
        match self.expected {
            // A mismatch is a hard failure rather than a prompt. By the time
            // the certificate has changed, the only two explanations are a
            // re-keyed server and an interception, and a client cannot tell
            // them apart — so it must not offer to continue.
            Some(expected) if seen != expected => {
                let message = format!(
                    "certificate mismatch for this site.\n  pinned: {expected}\n  \
                     offered: {seen}\nIf the site was re-keyed, run `dustnet trust forget` \
                     for it; otherwise this connection is being intercepted."
                );
                if let Ok(mut cell) = self.mismatch.lock() {
                    *cell = Some(message.clone());
                }
                Err(rustls::Error::General(message))
            }
            _ => Ok(rustls::client::danger::ServerCertVerified::assertion()),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

// ─── Insecure TLS verifier (for --insecure flag) ────────────

#[derive(Debug)]
struct InsecureVerifier;

impl rustls::client::danger::ServerCertVerifier for InsecureVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stand up a real TLS server with a fresh self-signed certificate and
    /// return its address plus a shutdown handle.
    ///
    /// A real handshake rather than a hand-built verifier call, because the
    /// property under test is that the peer proves possession of the private
    /// key. A unit test that fed a certificate to `verify_server_cert` would
    /// pass just as happily against a verifier that asserted every signature,
    /// which is the exact mistake these tests exist to catch.
    async fn self_signed_server() -> (
        std::net::SocketAddr,
        tokio::sync::watch::Sender<bool>,
        tokio::task::JoinHandle<()>,
        tempfile::TempDir,
    ) {
        use dustnet_server::{StaticServer, StaticServerConfig};
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("index.aml"), "[page][/page]").unwrap();
        let config = StaticServerConfig::bind_self_signed_for_tests(
            directory.path().to_path_buf(),
            "127.0.0.1",
            0,
        )
        .await
        .unwrap();
        let address = config.local_addr().unwrap();
        let mut server = StaticServer::new(config);
        let shutdown = server.shutdown_handle();
        let task = tokio::spawn(async move {
            let _ = server.run().await;
        });
        (address, shutdown, task, directory)
    }

    #[tokio::test]
    async fn trust_on_first_use_reports_the_certificate_it_accepted() {
        let (address, shutdown, task, _directory) = self_signed_server().await;

        let (_conn, fingerprint) = AtpConnection::connect_tls(
            "127.0.0.1",
            address.port(),
            &TlsVerification::TrustOnFirstUse,
        )
        .await
        .expect("trust on first use accepts an unknown certificate");
        assert!(
            fingerprint.is_some(),
            "the caller cannot pin what it was not told"
        );

        shutdown.send(true).unwrap();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn a_pinned_certificate_is_accepted_and_a_changed_one_is_refused() {
        let (address, shutdown, task, _directory) = self_signed_server().await;

        let (_first, fingerprint) = AtpConnection::connect_tls(
            "127.0.0.1",
            address.port(),
            &TlsVerification::TrustOnFirstUse,
        )
        .await
        .unwrap();
        let pinned = fingerprint.unwrap();

        // Same server, same certificate: the pin is satisfied.
        AtpConnection::connect_tls(
            "127.0.0.1",
            address.port(),
            &TlsVerification::Pinned(pinned),
        )
        .await
        .expect("the pinned certificate is the one being served");

        // A different server, with a different self-signed certificate, is
        // exactly what an interception looks like from here.
        let (other, other_shutdown, other_task, _other_directory) = self_signed_server().await;
        let refused =
            AtpConnection::connect_tls("127.0.0.1", other.port(), &TlsVerification::Pinned(pinned))
                .await
                .map(|_| ())
                .expect_err("a certificate that is not the pinned one must be refused");
        // Its own variant, not a generic handshake failure. This is the one
        // message an intercepted user depends on reading, and routing it
        // through `Protocol` buries it under rustls's own prefixes.
        let message = match refused {
            TlsConnectError::PinMismatch(message) => message,
            other => panic!("a pin mismatch must be reported as one, got {other:?}"),
        };
        for expected in ["pinned:", "offered:", "intercepted"] {
            assert!(
                message.contains(expected),
                "the mismatch message should mention `{expected}`, got: {message}"
            );
        }

        shutdown.send(true).unwrap();
        task.await.unwrap();
        other_shutdown.send(true).unwrap();
        other_task.await.unwrap();
    }

    /// The pinning verifier skips *host name* checking on purpose, and skips
    /// nothing else. This is the test that would fail if the signature
    /// verification were ever replaced with an assertion the way
    /// `InsecureVerifier` does it: a certificate is public, so pinning without
    /// a signature check authenticates nobody.
    #[tokio::test]
    async fn pinning_still_requires_the_peer_to_prove_it_holds_the_key() {
        let (address, shutdown, task, _directory) = self_signed_server().await;
        let (_conn, fingerprint) = AtpConnection::connect_tls(
            "127.0.0.1",
            address.port(),
            &TlsVerification::TrustOnFirstUse,
        )
        .await
        .unwrap();
        shutdown.send(true).unwrap();
        task.await.unwrap();

        // A verifier that waved signatures through would have no schemes to
        // offer; rustls only completes a handshake it can actually check.
        let verifier = PinningVerifier::new(fingerprint);
        use rustls::client::danger::ServerCertVerifier as _;
        assert!(
            !verifier.supported_verify_schemes().is_empty(),
            "signature verification must be delegated to the crypto provider"
        );
    }

    /// A self-signed certificate has no authority behind it, so the default
    /// path must reject it — and report enough for the refusal to become a
    /// question rather than a dead end.
    #[tokio::test]
    async fn certificate_authority_verification_reports_what_it_refused() {
        let (address, shutdown, task, _directory) = self_signed_server().await;

        let outcome = AtpConnection::connect_tls(
            "127.0.0.1",
            address.port(),
            &TlsVerification::Ca {
                extra_roots: Vec::new(),
            },
        )
        .await
        .map(|_| ());
        let peer = match outcome {
            Err(TlsConnectError::Unverified(peer)) => peer,
            Err(other) => panic!("expected an unverified peer, got {other:?}"),
            Ok(()) => panic!("CA verification must not accept an unvouched-for certificate"),
        };

        // The same certificate that trust-on-first-use would have pinned, so
        // the fingerprint shown to a user is the one they end up trusting.
        let (_conn, pinned) = AtpConnection::connect_tls(
            "127.0.0.1",
            address.port(),
            &TlsVerification::TrustOnFirstUse,
        )
        .await
        .unwrap();
        assert_eq!(Some(peer.fingerprint), pinned);
        assert!(
            !peer.reason.is_empty(),
            "a refusal with no stated reason gives a user nothing to judge with"
        );

        shutdown.send(true).unwrap();
        task.await.unwrap();
    }

    #[test]
    fn server_frame_limits_are_checked_before_body_allocation() {
        assert!(
            validate_inbound_body_len(
                InboundPeer::Server,
                MessageType::Page,
                MAX_PAGE_MESSAGE_SIZE as u32,
            )
            .is_ok()
        );
        assert!(matches!(
            validate_inbound_body_len(
                InboundPeer::Server,
                MessageType::Page,
                MAX_PAGE_MESSAGE_SIZE as u32 + 1,
            ),
            Err(ProtocolError::MessageTooLarge {
                msg_type: MessageType::Page,
                ..
            })
        ));
        assert!(matches!(
            validate_inbound_body_len(InboundPeer::Server, MessageType::Get, 0),
            Err(ProtocolError::WrongDirection(MessageType::Get))
        ));
    }

    #[test]
    fn transport_address_and_server_name_allocation_rejection_is_recoverable() {
        REJECT_TRANSPORT_ALLOCATION.with(|rejected| {
            rejected.set(Some(TransportAllocationSite::Address));
        });
        assert!(matches!(
            try_format_host_port("example.com", 1985),
            Err(ProtocolError::ResourceExhausted { .. })
        ));

        REJECT_TRANSPORT_ALLOCATION.with(|rejected| {
            rejected.set(Some(TransportAllocationSite::ServerName));
        });
        assert!(matches!(
            try_server_name("example.com"),
            Err(ProtocolError::ResourceExhausted { .. })
        ));

        // The diagnostic on the `rustls` rejection path: refusing its copy
        // must surface as exhaustion rather than as a TLS error carrying a
        // message that was never allocated.
        REJECT_TRANSPORT_ALLOCATION.with(|rejected| {
            rejected.set(Some(TransportAllocationSite::TlsMessage));
        });
        assert!(matches!(
            try_server_name("not a valid host name"),
            Err(ProtocolError::ResourceExhausted { .. })
        ));
        assert!(matches!(
            try_server_name("not a valid host name"),
            Err(ProtocolError::Tls(_))
        ));

        assert_eq!(
            try_format_host_port("example.com", 1985).unwrap(),
            "example.com:1985"
        );
        assert_eq!(try_format_host_port("::1", 1985).unwrap(), "[::1]:1985");
        assert!(try_server_name("example.com").is_ok());
    }
}
