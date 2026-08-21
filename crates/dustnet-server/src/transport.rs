use std::net::SocketAddr;
use std::sync::Arc;

use dustnet_core::protocol::frame::MessageType;
use dustnet_core::protocol::frame::{RawFrame, allocate_frame_body, decode_header, encode_frame};
use dustnet_core::protocol::message::{
    HelloMessage, PageFlags, UpdateFlags, UpdateMessage, WelcomeMessage, validate_frame_body,
};
use dustnet_core::protocol::state::{ConnectionStateMachine, Direction, Endpoint};
use dustnet_core::protocol::{
    HEADER_SIZE, MAX_CONTROL_MESSAGE_SIZE, MAX_FRAME_SIZE, MAX_INPUT_MESSAGE_SIZE,
    MAX_LIVE_UPDATE_SIZE, MAX_PAGE_MESSAGE_SIZE, MAX_WASM_MODULE_SIZE, NegotiatedCapabilities,
    NegotiatedProtocol, ProtocolError, ProtocolVersion,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls_pki_types::pem::PemObject;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;

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
enum ServerTransport {
    Plain(TcpStream),
    Tls(tokio_rustls::server::TlsStream<TcpStream>),
}

fn format_host_port(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

// ─── Server-side accepted connection ─────────────────────────

/// A server-side accepted connection.
pub struct AtpServerStream {
    transport: ServerTransport,
    state: ConnectionStateMachine,
    offered_capabilities: Vec<String>,
    offered_version: Option<ProtocolVersion>,
    negotiated: Option<NegotiatedProtocol>,
    poisoned: bool,
}

/// A TCP connection accepted by the listener but not yet TLS-negotiated.
///
/// Keeping this phase separate lets the server accept and bound multiple TLS
/// handshakes concurrently instead of allowing one slow client to block the
/// listener's accept loop.
#[doc(hidden)]
pub struct PendingAtpServerStream {
    stream: TcpStream,
    peer_addr: SocketAddr,
    tls_acceptor: Option<TlsAcceptor>,
}

impl PendingAtpServerStream {
    pub fn peer_addr(&self) -> SocketAddr {
        self.peer_addr
    }

    pub async fn handshake(self) -> Result<AtpServerStream, ProtocolError> {
        let transport = if let Some(acceptor) = self.tls_acceptor {
            let tls_stream = acceptor
                .accept(self.stream)
                .await
                .map_err(|e| ProtocolError::tls(format_args!("TLS accept error: {e}")))?;
            ServerTransport::Tls(tls_stream)
        } else {
            ServerTransport::Plain(self.stream)
        };

        Ok(AtpServerStream {
            transport,
            state: ConnectionStateMachine::new(Endpoint::Server),
            offered_capabilities: Vec::new(),
            offered_version: None,
            negotiated: None,
            poisoned: false,
        })
    }
}

impl AtpServerStream {
    /// Send a frame to the client.
    /// Send an UPDATE assembled directly onto the wire from its parts.
    ///
    /// `send_frame` allocates twice for a live update: once to build the body
    /// and once more inside `encode_frame`. Per subscriber that is two copies
    /// of the region's content, so fanning one change out to N subscribers
    /// costs O(N) copies of it. Writing the header and the body's pieces
    /// straight to the stream costs none, which is what lets a single shared
    /// read serve every subscriber of a path.
    ///
    /// Validation is deliberately the same set `try_send_frame` performs, in
    /// the same order, so this path cannot admit a frame the ordinary one would
    /// refuse. `UpdateMessage::validate_update_parts` stands in for
    /// `validate_frame_body`, and `parts_framing_matches_assembled_body` in
    /// dustnet-core asserts the two agree.
    pub async fn send_update_parts(
        &mut self,
        region: &str,
        content: &str,
        delta: bool,
    ) -> Result<(), ProtocolError> {
        if self.poisoned {
            return Err(ProtocolError::ConnectionClosed);
        }
        let result = self.try_send_update_parts(region, content, delta).await;
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }

    async fn try_send_update_parts(
        &mut self,
        region: &str,
        content: &str,
        delta: bool,
    ) -> Result<(), ProtocolError> {
        let flags = UpdateFlags { delta }.to_bits();
        let body_len = UpdateMessage::parts_body_len(region, content.len())?;
        let body_len_u32 =
            u32::try_from(body_len).map_err(|_| ProtocolError::FrameTooLarge(u32::MAX))?;
        validate_inbound_body_len(InboundPeer::Server, MessageType::Update, body_len_u32)?;
        validate_inbound_flags(MessageType::Update, flags)?;
        UpdateMessage::validate_update_parts(region, content.len())?;
        self.validate_capability(MessageType::Update, flags, false)?;
        self.state.apply(Direction::Send, MessageType::Update)?;

        // The header plus the fixed `"UPDATE "` prefix, on the stack.
        let total_len = (HEADER_SIZE as u32)
            .checked_add(body_len_u32)
            .ok_or(ProtocolError::FrameTooLarge(u32::MAX))?;
        if total_len > MAX_FRAME_SIZE {
            return Err(ProtocolError::FrameTooLarge(total_len));
        }
        let mut prefix = [0_u8; HEADER_SIZE + 7];
        prefix[..4].copy_from_slice(&total_len.to_be_bytes());
        prefix[4] = MessageType::Update as u8;
        prefix[5] = flags;
        prefix[HEADER_SIZE..].copy_from_slice(b"UPDATE ");

        // Four writes rather than one: rustls frames into records of its own
        // choosing regardless, and the connection owns its stream exclusively,
        // so no other writer can interleave between them.
        match &mut self.transport {
            ServerTransport::Plain(stream) => {
                stream.write_all(&prefix).await?;
                stream.write_all(region.as_bytes()).await?;
                stream.write_all(b"\n\n").await?;
                stream.write_all(content.as_bytes()).await?;
            }
            ServerTransport::Tls(stream) => {
                stream.write_all(&prefix).await?;
                stream.write_all(region.as_bytes()).await?;
                stream.write_all(b"\n\n").await?;
                stream.write_all(content.as_bytes()).await?;
            }
        }
        Ok(())
    }

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
        validate_inbound_body_len(InboundPeer::Server, frame.msg_type, body_len)?;
        validate_inbound_flags(frame.msg_type, frame.flags)?;
        let metadata = validate_frame_body(frame.msg_type, &frame.body, frame.flags)?;
        if frame.msg_type == MessageType::Welcome {
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
        self.validate_capability(frame.msg_type, frame.flags, metadata.carries_session())?;
        self.state.apply(Direction::Send, frame.msg_type)?;
        let bytes = encode_frame(frame)?;
        match &mut self.transport {
            ServerTransport::Plain(s) => s.write_all(&bytes).await?,
            ServerTransport::Tls(s) => s.write_all(&bytes).await?,
        }
        Ok(())
    }

    /// Receive a frame from the client.
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
            ServerTransport::Plain(s) => {
                s.read_exact(&mut header).await?;
            }
            ServerTransport::Tls(s) => {
                s.read_exact(&mut header).await?;
            }
        }

        let (body_len, msg_type, flags) = decode_header(&header)?;
        validate_inbound_body_len(InboundPeer::Client, msg_type, body_len)?;
        validate_inbound_flags(msg_type, flags)?;

        let mut body = allocate_frame_body(body_len as usize)?;
        if body_len > 0 {
            match &mut self.transport {
                ServerTransport::Plain(s) => {
                    s.read_exact(&mut body).await?;
                }
                ServerTransport::Tls(s) => {
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
        if msg_type == MessageType::Hello {
            let body = std::str::from_utf8(&frame.body)
                .map_err(|_| ProtocolError::InvalidMessage("HELLO is not UTF-8".into()))?;
            let hello = HelloMessage::parse(body)?;
            self.offered_version = Some(ProtocolVersion::parse(&hello.protocol_version)?);
            self.offered_capabilities = hello.capabilities;
        }
        self.validate_capability(msg_type, flags, metadata.carries_session())?;
        self.state.apply(Direction::Receive, msg_type)?;
        Ok(frame)
    }

    fn validate_capability(
        &self,
        msg_type: MessageType,
        flags: u8,
        carries_session: bool,
    ) -> Result<(), ProtocolError> {
        if matches!(msg_type, MessageType::Hello | MessageType::Welcome) {
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

// ─── Server listener ─────────────────────────────────────────

/// ATP server listener — accepts incoming connections.
pub struct AtpListener {
    listener: TcpListener,
    tls_acceptor: Option<TlsAcceptor>,
}

impl AtpListener {
    /// Bind a plain TCP listener (no encryption).
    pub async fn bind_plain(addr: &str, port: u16) -> Result<Self, ProtocolError> {
        let address = addr.parse::<std::net::IpAddr>().map_err(|_| {
            ProtocolError::InvalidUri("plaintext listener address must be a loopback IP".into())
        })?;
        if !address.is_loopback() {
            return Err(ProtocolError::InvalidUri(
                "plaintext ATP listeners are restricted to loopback".into(),
            ));
        }
        let bind_addr = format_host_port(addr, port);
        let listener = TcpListener::bind(&bind_addr).await?;
        Ok(AtpListener {
            listener,
            tls_acceptor: None,
        })
    }

    /// Bind a TLS listener with cert/key from PEM files.
    pub async fn bind_tls(
        addr: &str,
        port: u16,
        cert_path: &str,
        key_path: &str,
    ) -> Result<Self, ProtocolError> {
        let _ = rustls::crypto::ring::default_provider().install_default();

        validate_key_permissions(std::path::Path::new(key_path))?;
        validate_cert_permissions(std::path::Path::new(cert_path))?;

        let cert_data = std::fs::read(cert_path)
            .map_err(|e| ProtocolError::tls(format_args!("reading cert file: {e}")))?;
        let key_data = std::fs::read(key_path)
            .map_err(|e| ProtocolError::tls(format_args!("reading key file: {e}")))?;

        let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(&cert_data)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| ProtocolError::tls(format_args!("parsing cert PEM: {e}")))?;

        let key = PrivateKeyDer::from_pem_slice(&key_data)
            .map_err(|e| ProtocolError::tls(format_args!("parsing key PEM: {e}")))?;

        let config =
            rustls::ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
                .with_no_client_auth()
                .with_single_cert(certs, key)
                .map_err(|e| ProtocolError::tls(format_args!("TLS config error: {e}")))?;

        let bind_addr = format!("{addr}:{port}");
        let listener = TcpListener::bind(&bind_addr).await?;

        Ok(AtpListener {
            listener,
            tls_acceptor: Some(TlsAcceptor::from(Arc::new(config))),
        })
    }

    /// Bind a TLS listener with a self-signed certificate.
    pub async fn bind_self_signed(addr: &str, port: u16) -> Result<Self, ProtocolError> {
        let _ = rustls::crypto::ring::default_provider().install_default();

        let hostname = if addr == "0.0.0.0" || addr == "127.0.0.1" {
            "localhost"
        } else {
            addr
        };

        let (cert_der, key_der) = crate::certs::generate_self_signed(hostname)?;

        let cert = CertificateDer::from(cert_der);
        let key = PrivateKeyDer::try_from(key_der)
            .map_err(|e| ProtocolError::tls(format_args!("key conversion error: {e}")))?;

        let config =
            rustls::ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
                .with_no_client_auth()
                .with_single_cert(vec![cert], key)
                .map_err(|e| ProtocolError::tls(format_args!("TLS config error: {e}")))?;

        let bind_addr = format!("{addr}:{port}");
        let listener = TcpListener::bind(&bind_addr).await?;

        Ok(AtpListener {
            listener,
            tls_acceptor: Some(TlsAcceptor::from(Arc::new(config))),
        })
    }

    /// Accept the TCP connection without waiting for its TLS handshake.
    #[doc(hidden)]
    pub async fn accept_pending(&self) -> Result<PendingAtpServerStream, ProtocolError> {
        let (stream, peer_addr) = self.listener.accept().await?;
        stream.set_nodelay(true).ok();

        Ok(PendingAtpServerStream {
            stream,
            peer_addr,
            tls_acceptor: self.tls_acceptor.clone(),
        })
    }

    /// Get the local address this listener is bound to.
    pub fn local_addr(&self) -> Result<SocketAddr, ProtocolError> {
        Ok(self.listener.local_addr()?)
    }
}

#[cfg(unix)]
fn validate_cert_permissions(path: &std::path::Path) -> Result<(), ProtocolError> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = std::fs::metadata(path).map_err(|error| {
        ProtocolError::tls(format_args!("reading certificate metadata: {error}"))
    })?;
    if !metadata.is_file() {
        return Err(ProtocolError::Tls(
            "certificate path is not a regular file".into(),
        ));
    }
    if metadata.permissions().mode() & 0o022 != 0 {
        return Err(ProtocolError::Tls(
            "certificate file must not be group/world writable".into(),
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_cert_permissions(path: &std::path::Path) -> Result<(), ProtocolError> {
    if std::fs::metadata(path)
        .map_err(|error| ProtocolError::tls(format_args!("reading certificate metadata: {error}")))?
        .is_file()
    {
        Ok(())
    } else {
        Err(ProtocolError::Tls(
            "certificate path is not a regular file".into(),
        ))
    }
}

#[cfg(unix)]
fn validate_key_permissions(path: &std::path::Path) -> Result<(), ProtocolError> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = std::fs::metadata(path)
        .map_err(|error| ProtocolError::tls(format_args!("reading key metadata: {error}")))?;
    if !metadata.is_file() {
        return Err(ProtocolError::Tls(
            "private key path is not a regular file".into(),
        ));
    }
    let mode = metadata.permissions().mode();
    if mode & 0o077 != 0 {
        return Err(ProtocolError::tls(format_args!(
            "private key permissions are too broad: {:03o} (expected 600 or stricter)",
            mode & 0o777
        )));
    }
    Ok(())
}

#[cfg(test)]
mod permission_tests {
    use super::*;

    #[test]
    fn certificate_and_key_paths_must_be_regular_files() {
        let directory = tempfile::tempdir().unwrap();
        assert!(matches!(
            validate_cert_permissions(directory.path()),
            Err(ProtocolError::Tls(message)) if message.contains("not a regular file")
        ));
        assert!(matches!(
            validate_key_permissions(directory.path()),
            Err(ProtocolError::Tls(message)) if message.contains("not a regular file")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn unix_rejects_writable_certificates_and_broad_private_keys() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let certificate = directory.path().join("cert.pem");
        let private_key = directory.path().join("key.pem");
        std::fs::write(&certificate, b"certificate").unwrap();
        std::fs::write(&private_key, b"private key").unwrap();

        std::fs::set_permissions(&certificate, std::fs::Permissions::from_mode(0o622)).unwrap();
        assert!(matches!(
            validate_cert_permissions(&certificate),
            Err(ProtocolError::Tls(message)) if message.contains("group/world writable")
        ));

        std::fs::set_permissions(&private_key, std::fs::Permissions::from_mode(0o640)).unwrap();
        assert!(matches!(
            validate_key_permissions(&private_key),
            Err(ProtocolError::Tls(message)) if message.contains("too broad")
        ));

        std::fs::set_permissions(&certificate, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::set_permissions(&private_key, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(validate_cert_permissions(&certificate).is_ok());
        assert!(validate_key_permissions(&private_key).is_ok());
    }

    #[test]
    fn client_frame_limits_are_checked_before_body_allocation() {
        assert!(
            validate_inbound_body_len(
                InboundPeer::Client,
                MessageType::Input,
                MAX_INPUT_MESSAGE_SIZE as u32,
            )
            .is_ok()
        );
        assert!(matches!(
            validate_inbound_body_len(
                InboundPeer::Client,
                MessageType::Input,
                MAX_INPUT_MESSAGE_SIZE as u32 + 1,
            ),
            Err(ProtocolError::MessageTooLarge {
                msg_type: MessageType::Input,
                ..
            })
        ));
        assert!(matches!(
            validate_inbound_body_len(InboundPeer::Client, MessageType::Page, 0),
            Err(ProtocolError::WrongDirection(MessageType::Page))
        ));
    }
}

#[cfg(not(unix))]
fn validate_key_permissions(path: &std::path::Path) -> Result<(), ProtocolError> {
    let metadata = std::fs::metadata(path)
        .map_err(|error| ProtocolError::tls(format_args!("reading key metadata: {error}")))?;
    if metadata.is_file() {
        Ok(())
    } else {
        Err(ProtocolError::Tls(
            "private key path is not a regular file".into(),
        ))
    }
}
