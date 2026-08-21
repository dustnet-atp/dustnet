#![forbid(unsafe_code)]
// Remote clients choose every byte this server frames and every path it is
// asked to serve, so an operation
// that can panic on out-of-range input is a denial of service against every
// other connection the process is holding. Denied in non-test builds only:
// tests assert on known-good values, where `unwrap` is the clearer spelling.
#![cfg_attr(
    not(test),
    deny(
        clippy::indexing_slicing,
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic
    )
)]

//! Bounded, plugin-free static ATP server.

mod certs;
pub mod include;
mod live_watch;
mod transport;

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dustnet_core::protocol::frame::{MessageType, RawFrame, allocate_frame_body};
use dustnet_core::protocol::message::{
    ErrorMessage, GetMessage, HelloMessage, PageFlags, PageMessage, SubscribeMessage,
    SubscribeMode, UpdateMessage, WelcomeMessage,
};
use dustnet_core::protocol::{
    MAX_PAGE_MESSAGE_SIZE, MAX_WASM_MODULE_SIZE, PROTOCOL_VERSION, ProtocolError,
    SUPPORTED_CAPABILITIES,
};
use tokio::io::AsyncReadExt;
use tokio::sync::{mpsc, watch};

use crate::live_watch::{
    ChangeSource, LiveGeneration, LiveWatcher, ManualChangeSource, NotifyChangeSource,
    WatchRegistry,
};
use crate::transport::{AtpListener, AtpServerStream};

/// Default server-wide connection ceiling.
///
/// An idle connection costs memory; a TLS handshake costs CPU. Holding
/// connections open is therefore the cheaper side of the trade, and the number
/// that used to sit here reflected the old per-connection subscription polling
/// rather than any protocol limit. Operators can raise or lower it with
/// `--max-connections`; note that the process also needs an `RLIMIT_NOFILE`
/// above this value, which the OS will not grant on our behalf (see
/// `docs/guides/production-support.md`).
pub const DEFAULT_MAX_CONNECTIONS: usize = 2048;

/// Default per-source-IP connection ceiling.
pub const DEFAULT_MAX_CONNECTIONS_PER_IP: usize = 4;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_SUBSCRIPTIONS_PER_CONNECTION: usize = 16;
const MAX_SERVER_SUBSCRIPTION_BYTES: usize = 128 * 1024 * 1024;

#[derive(Clone, Debug)]
pub(crate) struct SubscriptionBudget {
    state: Arc<Mutex<SubscriptionBudgetState>>,
}

#[derive(Debug)]
struct SubscriptionBudgetState {
    used: usize,
    limit: usize,
}

impl SubscriptionBudget {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            state: Arc::new(Mutex::new(SubscriptionBudgetState { used: 0, limit })),
        }
    }

    pub(crate) fn reserve(&self, requested: usize) -> Result<SubscriptionLease, ProtocolError> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let next = state
            .used
            .checked_add(requested)
            .ok_or(ProtocolError::ResourceExhausted { requested })?;
        if next > state.limit {
            let (used, limit) = (state.used, state.limit);
            drop(state);
            // Every refusal is logged, not the first or one per connection.
            // A refusal is the server silently declining to deliver a live
            // update someone asked for, and an operator who sees a stalled
            // region has no other way to find out that is what happened.
            tracing::warn!(
                requested,
                used,
                limit,
                "live update refused: subscription budget exhausted"
            );
            return Err(ProtocolError::ResourceExhausted { requested });
        }
        state.used = next;
        drop(state);
        Ok(SubscriptionLease {
            budget: self.clone(),
            amount: requested,
        })
    }

    /// Bytes currently reserved for retained subscription content.
    ///
    /// Exposed so an operator can see budget pressure before it becomes a
    /// stalled live region rather than after.
    pub(crate) fn used(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .used
    }

    /// The server-wide ceiling `used` is measured against.
    fn limit(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .limit
    }
}

#[derive(Debug)]
pub(crate) struct SubscriptionLease {
    budget: SubscriptionBudget,
    amount: usize,
}

impl SubscriptionLease {
    fn try_resize(&mut self, requested: usize) -> Result<(), ProtocolError> {
        let mut state = self
            .budget
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let without_self = state.used.saturating_sub(self.amount);
        let next = without_self
            .checked_add(requested)
            .ok_or(ProtocolError::ResourceExhausted { requested })?;
        if next > state.limit {
            return Err(ProtocolError::ResourceExhausted { requested });
        }
        state.used = next;
        self.amount = requested;
        Ok(())
    }

    fn ensure_at_least(&mut self, requested: usize) -> Result<(), ProtocolError> {
        if requested > self.amount {
            self.try_resize(requested)?;
        }
        Ok(())
    }

    pub(crate) fn shrink_to(&mut self, requested: usize) {
        debug_assert!(requested <= self.amount);
        let mut state = self
            .budget
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.used -= self.amount - requested;
        self.amount = requested;
    }
}

impl Drop for SubscriptionLease {
    fn drop(&mut self) {
        let mut state = self
            .budget
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.used = state.used.saturating_sub(self.amount);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ServerAllocationSite {
    Backing,
    Path,
    Region,
    Content,
    StaticBody,
    WatchEntry,
    WakeSlots,
}

#[cfg(test)]
struct AllocationRejectionGuard;

#[cfg(test)]
impl AllocationRejectionGuard {
    fn at(site: ServerAllocationSite, owner: u64) -> Self {
        REJECT_ALLOCATION_SITE.with(|rejected| rejected.set(Some((site, owner))));
        Self
    }
}

#[cfg(test)]
impl Drop for AllocationRejectionGuard {
    fn drop(&mut self) {
        REJECT_ALLOCATION_SITE.with(|rejected| rejected.set(None));
    }
}

#[cfg(test)]
thread_local! {
    static REJECT_ALLOCATION_SITE: std::cell::Cell<Option<(ServerAllocationSite, u64)>> = const {
        std::cell::Cell::new(None)
    };
}

#[cfg(test)]
pub(crate) fn reject_allocation_at(site: ServerAllocationSite, owner: u64) -> bool {
    REJECT_ALLOCATION_SITE.with(|rejected| rejected.get() == Some((site, owner)))
}

#[cfg(not(test))]
pub(crate) fn reject_allocation_at(_site: ServerAllocationSite, _owner: u64) -> bool {
    false
}

fn reject_subscription_backing_allocation() -> bool {
    reject_allocation_at(ServerAllocationSite::Backing, 0)
}

struct StaticSubscription {
    region: String,
    mode: SubscribeMode,
    /// This connection's last successfully sent generation.
    ///
    /// Not "the file's previous version": under `SubscribeMode::Delta` the
    /// suffix is computed against what *this* subscriber last received, and it
    /// advances only after a send succeeds. A subscriber that misses a
    /// generation therefore finds `strip_prefix` failing on the next one and
    /// falls back to a complete replace, resynchronising itself without any
    /// separate recovery path. Sharing the read must not disturb that, which is
    /// why the baseline stays per-subscription while only the text is shared.
    baseline: Arc<LiveGeneration>,
    updates: watch::Receiver<Arc<LiveGeneration>>,
    _metadata_lease: SubscriptionLease,
}

fn subscription_owner_key(path: &str, region: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in path
        .bytes()
        .chain(std::iter::once(0xff))
        .chain(region.bytes())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn try_subscription_storage() -> Result<Vec<StaticSubscription>, ProtocolError> {
    let requested = MAX_SUBSCRIPTIONS_PER_CONNECTION
        .checked_mul(std::mem::size_of::<StaticSubscription>())
        .ok_or(ProtocolError::ResourceExhausted {
            requested: usize::MAX,
        })?;
    if reject_subscription_backing_allocation() {
        return Err(ProtocolError::ResourceExhausted { requested });
    }
    let mut subscriptions = Vec::new();
    subscriptions
        .try_reserve_exact(MAX_SUBSCRIPTIONS_PER_CONNECTION)
        .map_err(|_| ProtocolError::ResourceExhausted { requested })?;
    Ok(subscriptions)
}

/// Configuration for a static server. Construction validates the site root.
pub struct StaticServerConfig {
    root: PathBuf,
    listener: AtpListener,
    max_connections: usize,
    max_connections_per_ip: usize,
    read_timeout: Duration,
    resolver: Option<Arc<dyn crate::include::IncludeResolver>>,
}

impl StaticServerConfig {
    fn new(root: PathBuf, listener: AtpListener) -> Result<Self, ProtocolError> {
        let metadata = std::fs::metadata(&root)?;
        if !metadata.is_dir() {
            return Err(ProtocolError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "static site root is not a directory",
            )));
        }
        // Canonicalise once, here, rather than on every request. Blocking at
        // bind time is free; blocking on a runtime worker per GET is not.
        let root = std::fs::canonicalize(&root)?;
        Ok(Self {
            root,
            listener,
            max_connections: DEFAULT_MAX_CONNECTIONS,
            max_connections_per_ip: DEFAULT_MAX_CONNECTIONS_PER_IP,
            read_timeout: READ_TIMEOUT,
            resolver: None,
        })
    }

    /// Install a resolver for `[include]` placeholders.
    ///
    /// Absent by default, and `dustnetd` never sets one: a server without a
    /// resolver serves authored AML unchanged, so an `[include]` travels to the
    /// client and renders as nothing. Setting one is what lets a page carry
    /// generated content, and it is the only way this server produces markup it
    /// did not read from a file.
    pub fn with_include_resolver(
        mut self,
        resolver: Arc<dyn crate::include::IncludeResolver>,
    ) -> Self {
        self.resolver = Some(resolver);
        self
    }

    /// Bind a plaintext development server. The listener enforces loopback-only use.
    pub async fn bind_plaintext_loopback(
        root: PathBuf,
        address: &str,
        port: u16,
    ) -> Result<Self, ProtocolError> {
        Self::new(root, AtpListener::bind_plain(address, port).await?)
    }

    /// Bind a TLS 1.3 static server using PEM certificate and private-key files.
    pub async fn bind_tls(
        root: PathBuf,
        address: &str,
        port: u16,
        certificate: &str,
        private_key: &str,
    ) -> Result<Self, ProtocolError> {
        Self::new(
            root,
            AtpListener::bind_tls(address, port, certificate, private_key).await?,
        )
    }

    /// Return the listener address, including an OS-selected port when port zero was requested.
    pub fn local_addr(&self) -> Result<std::net::SocketAddr, ProtocolError> {
        self.listener.local_addr()
    }

    /// Override the idle read deadline.
    ///
    /// Exposed for tests only. The keepalive behaviour this guards is defined
    /// by a 30-second deadline, and a test that waited it out honestly would
    /// dominate the suite's runtime; shortening the deadline lets the same
    /// behaviour be proven in milliseconds.
    #[doc(hidden)]
    pub fn with_read_timeout(mut self, read_timeout: Duration) -> Self {
        self.read_timeout = read_timeout;
        self
    }

    /// Override the connection ceilings.
    ///
    /// The right ceiling depends on the operator's machine — chiefly its
    /// `RLIMIT_NOFILE`, which this process cannot raise for itself under the
    /// workspace's `forbid(unsafe_code)`. Both values are clamped to at least
    /// one, and the per-IP ceiling is clamped to the global one: a per-IP limit
    /// above the global limit does not describe a reachable state.
    pub fn with_connection_limits(mut self, global: usize, per_ip: usize) -> Self {
        self.max_connections = global.max(1);
        self.max_connections_per_ip = per_ip.max(1).min(self.max_connections);
        self
    }

    #[doc(hidden)]
    pub async fn bind_self_signed_for_tests(
        root: PathBuf,
        address: &str,
        port: u16,
    ) -> Result<Self, ProtocolError> {
        Self::new(root, AtpListener::bind_self_signed(address, port).await?)
    }
}

/// Production ATP server with no custom-handler or plugin surface.
pub struct StaticServer {
    config: Option<StaticServerConfig>,
    shutdown_tx: watch::Sender<bool>,
    shutdown_rx: watch::Receiver<bool>,
    subscription_budget: SubscriptionBudget,
}

impl StaticServer {
    pub fn new(config: StaticServerConfig) -> Self {
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        Self {
            config: Some(config),
            shutdown_tx,
            shutdown_rx,
            subscription_budget: SubscriptionBudget::new(MAX_SERVER_SUBSCRIPTION_BYTES),
        }
    }

    /// Bytes of live-region content currently retained, and the server-wide
    /// ceiling they are measured against.
    ///
    /// Budget exhaustion shows up to a user as a live region that quietly stops
    /// updating. Every refusal is logged, and this is the counter that lets an
    /// operator watch the pressure build beforehand instead of diagnosing it
    /// afterwards.
    pub fn subscription_memory(&self) -> (usize, usize) {
        (
            self.subscription_budget.used(),
            self.subscription_budget.limit(),
        )
    }

    pub fn shutdown_handle(&self) -> watch::Sender<bool> {
        self.shutdown_tx.clone()
    }

    pub async fn run(&mut self) -> Result<(), ProtocolError> {
        let config = self.config.take().ok_or_else(|| {
            ProtocolError::InvalidMessage("static server may only be run once".into())
        })?;
        let root = Arc::new(config.root);
        let resolver = config.resolver;
        let semaphore = Arc::new(tokio::sync::Semaphore::new(config.max_connections));

        // One reader for the whole server. Every connection subscribing to a
        // path shares its reads; a file changes once and is read once, however
        // many viewers are watching it.
        let (events_tx, events_rx) = mpsc::channel(256);
        let source: Box<dyn ChangeSource> = match NotifyChangeSource::new(events_tx) {
            Some(source) => Box::new(source),
            // Losing filesystem notification costs latency, not correctness:
            // the registry's reconcile sweep serves every watched file on its
            // own. Refusing to start would be a worse trade.
            None => Box::new(ManualChangeSource),
        };
        let (registry, watcher) =
            WatchRegistry::new(self.subscription_budget.clone(), source, events_rx);
        let registry_shutdown = self.shutdown_rx.clone();
        let registry_task = tokio::spawn(registry.run(registry_shutdown));
        let max_connections_per_ip = config.max_connections_per_ip;
        let read_timeout = config.read_timeout;
        let per_ip = Arc::new(Mutex::new(HashMap::<IpAddr, usize>::new()));
        let subscription_budget = self.subscription_budget.clone();
        let mut tasks = tokio::task::JoinSet::new();
        tracing::info!(
            address = %config.listener.local_addr()?,
            max_connections = config.max_connections,
            max_connections_per_ip = config.max_connections_per_ip,
            "static ATP server listening"
        );

        loop {
            tokio::select! {
                changed = self.shutdown_rx.changed() => {
                    if changed.is_err() || *self.shutdown_rx.borrow() { break; }
                }
                accepted = config.listener.accept_pending() => {
                    let pending = accepted?;
                    let Ok(permit) = semaphore.clone().try_acquire_owned() else {
                        tracing::warn!(peer = %pending.peer_addr(), "global connection limit reached");
                        continue;
                    };
                    let peer_ip = pending.peer_addr().ip();
                    {
                        let mut counts = per_ip.lock().unwrap_or_else(|p| p.into_inner());
                        let count = counts.entry(peer_ip).or_default();
                        if *count >= max_connections_per_ip {
                            tracing::warn!(peer_ip = %peer_ip, "per-IP connection limit reached");
                            continue;
                        }
                        *count += 1;
                    }
                    let root = root.clone();
                    let resolver = resolver.clone();
                    let counts = per_ip.clone();
                    let subscription_budget = subscription_budget.clone();
                    let connection_shutdown = self.shutdown_rx.clone();
                    let watcher = watcher.clone();
                    let peer = pending.peer_addr();
                    tasks.spawn(async move {
                        let _permit = permit;
                        // Logged for every connection, not only failing ones.
                        // Without this the log answers "why can nobody
                        // connect?" but not "is anyone connected?", which is
                        // the question an operator actually has first.
                        tracing::info!(peer = %peer, "connection accepted");
                        match tokio::time::timeout(HANDSHAKE_TIMEOUT, pending.handshake()).await {
                            Ok(Ok(stream)) => {
                                match serve_connection(
                                    stream,
                                    &root,
                                    resolver.as_deref(),
                                    subscription_budget,
                                    watcher,
                                    read_timeout,
                                    connection_shutdown,
                                )
                                .await
                                {
                                    Ok(()) => tracing::info!(peer = %peer, "connection closed"),
                                    Err(error) => tracing::warn!(
                                        peer = %peer,
                                        error = %error,
                                        "connection closed with an error"
                                    ),
                                }
                            }
                            Ok(Err(error)) => tracing::warn!(
                                peer = %peer,
                                error = %error,
                                "handshake failed"
                            ),
                            // Previously silent: a client that connected and
                            // then said nothing left no trace at all.
                            Err(_) => tracing::warn!(
                                peer = %peer,
                                timeout = ?HANDSHAKE_TIMEOUT,
                                "handshake deadline reached"
                            ),
                        }
                        let mut counts = counts.lock().unwrap_or_else(|p| p.into_inner());
                        if let Some(count) = counts.get_mut(&peer_ip) {
                            *count = count.saturating_sub(1);
                            if *count == 0 { counts.remove(&peer_ip); }
                        }
                    });
                }
            }
        }

        let drain = async { while tasks.join_next().await.is_some() {} };
        if tokio::time::timeout(DRAIN_TIMEOUT, drain).await.is_err() {
            tasks.abort_all();
            tracing::warn!("graceful shutdown deadline reached");
        }
        // The registry observes the same shutdown signal and holds no client
        // sockets, so joining it is bounded by its own loop, not by any peer.
        registry_task.abort();
        Ok(())
    }
}

async fn serve_connection(
    mut stream: AtpServerStream,
    root: &Path,
    resolver: Option<&dyn crate::include::IncludeResolver>,
    subscription_budget: SubscriptionBudget,
    watcher: LiveWatcher,
    read_timeout: Duration,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), ProtocolError> {
    // Retained subscription-table growth must be settled before the first response. Once an
    // initial UPDATE is on the wire, installing its matching entry is allocation-free.
    let mut subscriptions = try_subscription_storage()?;
    let hello_frame = recv(&mut stream, HANDSHAKE_TIMEOUT).await?;
    if hello_frame.msg_type != MessageType::Hello {
        return Err(ProtocolError::InvalidMessage("HELLO required".into()));
    }
    let hello_text = std::str::from_utf8(&hello_frame.body)
        .map_err(|_| ProtocolError::InvalidMessage("HELLO is not UTF-8".into()))?;
    let hello = HelloMessage::parse(hello_text)?;
    if hello.protocol_version != PROTOCOL_VERSION {
        send_error(&mut stream, 505, "unsupported ATP version").await?;
        return Ok(());
    }
    let capabilities = hello
        .capabilities
        .into_iter()
        .filter(|capability| SUPPORTED_CAPABILITIES.contains(&capability.as_str()))
        .collect();
    let welcome = WelcomeMessage {
        protocol_version: PROTOCOL_VERSION.into(),
        server: Some(format!("dustnetd/{}", env!("CARGO_PKG_VERSION"))),
        site_name: None,
        capabilities,
    };
    tracing::debug!(
        version = %hello.protocol_version,
        capabilities = ?welcome.capabilities,
        "negotiated"
    );
    send(
        &mut stream,
        RawFrame {
            msg_type: MessageType::Welcome,
            flags: 0,
            body: welcome.serialize()?.into_bytes(),
        },
    )
    .await?;

    // No per-connection timer. 2048 connections each holding a 250ms interval
    // is roughly eight thousand timer wakeups a second before any work happens;
    // the registry wakes a connection only when a file it watches has actually
    // changed. Capacity one, because a pending wake already says everything a
    // second one would.
    let (wake_tx, mut wake_rx) = mpsc::channel::<()>(1);
    let idle = tokio::time::sleep(read_timeout);
    tokio::pin!(idle);
    loop {
        tokio::select! {
            received = stream.recv_frame() => {
                let frame = received?;
                idle.as_mut().reset(tokio::time::Instant::now() + read_timeout);
                // Per-request detail sits at debug: one line per request is the
                // right amount of information when diagnosing a site and far
                // too much at two thousand connections, so the level is what
                // separates them rather than a sampling rule. Messages that
                // carry no detail worth naming are logged here; the rest log
                // themselves where their fields are already parsed.
                if matches!(
                    frame.msg_type,
                    MessageType::Ping | MessageType::Bye | MessageType::Input
                ) {
                    tracing::debug!(message = ?frame.msg_type, "request");
                }
                match frame.msg_type {
                    MessageType::Get => serve_get(&mut stream, root, resolver, frame).await?,
                    MessageType::Input => {
                        send_error(&mut stream, 405, "static server rejects INPUT").await?
                    }
                    MessageType::Subscribe => {
                        if subscriptions.len() >= MAX_SUBSCRIPTIONS_PER_CONNECTION {
                            return Err(ProtocolError::InvalidMessage(
                                "subscription limit reached".into(),
                            ));
                        } else {
                            subscriptions.push(serve_subscription(
                                &mut stream,
                                root,
                                frame,
                                &watcher,
                                &wake_tx,
                                &subscription_budget,
                            ).await?);
                        }
                    }
                    MessageType::Unsubscribe => {
                        tracing::debug!(
                            released = subscriptions.len(),
                            "UNSUBSCRIBE"
                        );
                        subscriptions.clear();
                    }
                    // The idle deadline above was already reset by this frame
                    // arriving; that reset is the whole point of PING. PONG
                    // exists so the client can distinguish a live server from
                    // a black-holed socket that is silently absorbing writes.
                    MessageType::Ping => {
                        send(
                            &mut stream,
                            RawFrame {
                                msg_type: MessageType::Pong,
                                flags: 0,
                                body: Vec::new(),
                            },
                        )
                        .await?
                    }
                    MessageType::Bye => {
                        send(
                            &mut stream,
                            RawFrame {
                                msg_type: MessageType::ServerBye,
                                flags: 0,
                                body: Vec::new(),
                            },
                        )
                        .await?;
                        return Ok(());
                    }
                    _ => {
                        return Err(ProtocolError::InvalidMessage(
                            "unexpected client message".into(),
                        ));
                    }
                }
            }
            Some(()) = wake_rx.recv(), if !subscriptions.is_empty() => {
                flush_subscriptions(&mut stream, &mut subscriptions).await?;
            }
            _ = &mut idle => return Err(ProtocolError::Timeout),
            // Without this arm no connection observes shutdown, so a single
            // idle client guarantees the drain deadline expires and every task
            // is aborted. Leaving voluntarily turns DRAIN_TIMEOUT back into the
            // backstop it is meant to be — which matters far more at 2048
            // connections than at 64.
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    let _ = send(
                        &mut stream,
                        RawFrame {
                            msg_type: MessageType::ServerBye,
                            flags: 0,
                            body: Vec::new(),
                        },
                    )
                    .await;
                    return Ok(());
                }
            }
        }
    }
}

async fn serve_get(
    stream: &mut AtpServerStream,
    root: &Path,
    resolver: Option<&dyn crate::include::IncludeResolver>,
    frame: RawFrame,
) -> Result<(), ProtocolError> {
    let text = std::str::from_utf8(&frame.body)
        .map_err(|_| ProtocolError::InvalidMessage("GET is not UTF-8".into()))?;
    let get = GetMessage::parse(text)?;
    tracing::debug!(path = %get.path, "GET");
    let path = resolve_path(root, &get.path).await?;
    let limit = if path
        .extension()
        .is_some_and(|extension| extension == "wasm")
    {
        MAX_WASM_MODULE_SIZE
    } else {
        MAX_PAGE_MESSAGE_SIZE
    };
    let file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(_) => {
            tracing::debug!(path = %get.path, "404 not found");
            send_error(stream, 404, "not found").await?;
            return Ok(());
        }
    };
    let metadata = match file.metadata().await {
        Ok(metadata) if metadata.is_file() && metadata.len() <= limit as u64 => metadata,
        _ => {
            send_error(stream, 404, "not found").await?;
            return Ok(());
        }
    };
    let owner_key = subscription_owner_key(&get.path, "");
    let body = read_static_body(file, metadata.len() as usize, owner_key).await?;
    if path
        .extension()
        .is_some_and(|extension| extension == "wasm")
    {
        send(
            stream,
            RawFrame {
                msg_type: MessageType::Resource,
                flags: 0,
                body,
            },
        )
        .await
    } else {
        let content = String::from_utf8(body)
            .map_err(|_| ProtocolError::InvalidMessage("AML file is not UTF-8".into()))?;

        // Substitute [include] placeholders before anything reads the content.
        // `has_live_regions` in particular must see the resolved page: a
        // resolver may emit a [live] region, and a flag computed from the
        // authored source would leave the client never subscribing to it.
        let content = match resolver {
            Some(resolver) => {
                let request = crate::include::IncludeRequest {
                    path: &get.path,
                    query: get.query.as_deref(),
                };
                let resolved = crate::include::resolve_page(&content, resolver, &request)?;
                // A resolver can produce more than a page may carry. Refuse
                // rather than truncate: half a page of stories is a worse
                // answer than an error naming the cause.
                if resolved.len() > MAX_PAGE_MESSAGE_SIZE {
                    tracing::warn!(
                        path = %get.path,
                        bytes = resolved.len(),
                        limit = MAX_PAGE_MESSAGE_SIZE,
                        "resolved page exceeds the page limit"
                    );
                    send_error(stream, 500, "resolved page too large").await?;
                    return Ok(());
                }
                resolved
            }
            None => content,
        };

        let has_live_regions = content.contains("[live");
        let page = PageMessage {
            content,
            flags: PageFlags {
                cacheable: true,
                has_live_regions,
                ..PageFlags::default()
            },
            session_directives: Vec::new(),
        };
        let (body, flags) = page.encode_body()?;
        send(
            stream,
            RawFrame {
                msg_type: MessageType::Page,
                flags,
                body,
            },
        )
        .await
    }
}

async fn read_static_body(
    mut file: tokio::fs::File,
    expected_len: usize,
    owner_key: u64,
) -> Result<Vec<u8>, ProtocolError> {
    let requested = expected_len
        .checked_add(1)
        .ok_or(ProtocolError::ResourceExhausted {
            requested: usize::MAX,
        })?;
    if reject_allocation_at(ServerAllocationSite::StaticBody, owner_key) {
        return Err(ProtocolError::ResourceExhausted { requested });
    }
    let mut body = allocate_frame_body(requested)?;
    let mut filled = 0;
    loop {
        let Some(unfilled) = body.get_mut(filled..) else {
            break;
        };
        if unfilled.is_empty() {
            break;
        }
        let read = file.read(unfilled).await?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    if filled > expected_len {
        return Err(ProtocolError::InvalidMessage(
            "static file changed while it was being read".into(),
        ));
    }
    body.truncate(filled);
    Ok(body)
}

async fn serve_subscription(
    stream: &mut AtpServerStream,
    root: &Path,
    frame: RawFrame,
    watcher: &LiveWatcher,
    wake: &mpsc::Sender<()>,
    budget: &SubscriptionBudget,
) -> Result<StaticSubscription, ProtocolError> {
    let text = std::str::from_utf8(&frame.body)
        .map_err(|_| ProtocolError::InvalidMessage("SUBSCRIBE is not UTF-8".into()))?;
    let subscription = SubscribeMessage::parse(text)?;
    tracing::debug!(
        path = %subscription.path,
        region = %subscription.region,
        mode = ?subscription.mode,
        "SUBSCRIBE"
    );
    let owner_key = subscription_owner_key(&subscription.path, &subscription.region);
    let resolved_path = resolve_path(root, &subscription.path).await?;
    let metadata_bytes = resolved_path
        .as_os_str()
        .len()
        .checked_add(subscription.region.len())
        .ok_or(ProtocolError::ResourceExhausted {
            requested: usize::MAX,
        })?;
    let mut metadata_lease = budget.reserve(metadata_bytes)?;
    let path = try_owned_subscription_path(&resolved_path, owner_key)?;
    let region = try_owned_subscription_string(
        &subscription.region,
        ServerAllocationSite::Region,
        owner_key,
    )?;
    let retained_metadata_bytes =
        path.capacity()
            .checked_add(region.capacity())
            .ok_or(ProtocolError::ResourceExhausted {
                requested: usize::MAX,
            })?;
    metadata_lease.try_resize(retained_metadata_bytes)?;
    let metadata = tokio::fs::metadata(&path).await?;
    if !metadata.is_file() {
        return Err(ProtocolError::InvalidMessage(
            "invalid live update file".into(),
        ));
    }

    // Attaching reads the file only if it has moved since anyone last read it,
    // so a reconnecting subscriber is never handed a staler generation than the
    // moment it asked, and a second subscriber to a live path costs a stat.
    let attached = watcher
        .attach(
            try_owned_subscription_path(&path, owner_key)?,
            wake.clone(),
            owner_key,
        )
        .await?;

    // The initial value is always a complete replace: the subscriber has no
    // baseline to append to yet.
    UpdateMessage::validate_update_parts(&region, attached.current.text.len())?;
    let candidate = StaticSubscription {
        region,
        mode: subscription.mode,
        baseline: attached.current,
        updates: attached.updates,
        _metadata_lease: metadata_lease,
    };
    send_update(stream, &candidate.region, &candidate.baseline.text, false).await?;
    Ok(candidate)
}

/// Send every subscription whatever it has not yet seen.
///
/// Called when the registry wakes this connection. The wake carries no payload,
/// so this reconciles all of the connection's subscriptions by pointer identity
/// — which is why a dropped wake is harmless: a full wake channel already means
/// "you have unhandled work".
///
/// Every failure here is either terminal for the connection or permanent for
/// this generation. That is deliberate. The old 250ms tick let a failure be a
/// `continue` that would be retried a quarter-second later; without a tick, a
/// retryable failure skipped here would never be retried at all. Retryable
/// failures now live in the watcher's read path, where the reconcile timer is
/// the retry.
async fn flush_subscriptions(
    stream: &mut AtpServerStream,
    subscriptions: &mut [StaticSubscription],
) -> Result<(), ProtocolError> {
    for subscription in subscriptions {
        // Clone out of the guard in its own scope: holding a `watch::Ref`
        // across the await below would block the registry's `send_replace` and
        // deadlock the whole fan-out.
        let next = {
            let guard = subscription.updates.borrow();
            Arc::clone(&guard)
        };
        if Arc::ptr_eq(&next, &subscription.baseline) {
            continue;
        }
        let (wire_content, delta) = if subscription.mode == SubscribeMode::Delta {
            match next.text.strip_prefix(&subscription.baseline.text) {
                Some(suffix) => (suffix, true),
                None => (next.text.as_str(), false),
            }
        } else {
            (next.text.as_str(), false)
        };
        // Permanent for this generation, not transient: the region name is
        // fixed and the content will not shrink on a retry.
        if UpdateMessage::validate_update_parts(&subscription.region, wire_content.len()).is_err() {
            tracing::warn!(
                region = %subscription.region,
                bytes = wire_content.len(),
                "live update not sent: body exceeds the maximum update size"
            );
            subscription.baseline = next;
            continue;
        }
        send_update(stream, &subscription.region, wire_content, delta).await?;
        // Advances only after a successful send, so a subscriber that missed a
        // generation resynchronises on the next one.
        subscription.baseline = next;
    }
    Ok(())
}

fn try_owned_subscription_path(path: &Path, owner_key: u64) -> Result<PathBuf, ProtocolError> {
    let requested = path.as_os_str().len();
    if reject_allocation_at(ServerAllocationSite::Path, owner_key) {
        return Err(ProtocolError::ResourceExhausted { requested });
    }
    let mut owned = PathBuf::new();
    owned
        .try_reserve_exact(requested)
        .map_err(|_| ProtocolError::ResourceExhausted { requested })?;
    owned.push(path);
    Ok(owned)
}

fn try_owned_subscription_string(
    value: &str,
    site: ServerAllocationSite,
    owner_key: u64,
) -> Result<String, ProtocolError> {
    let requested = value.len();
    if reject_allocation_at(site, owner_key) {
        return Err(ProtocolError::ResourceExhausted { requested });
    }
    let mut owned = String::new();
    owned
        .try_reserve_exact(requested)
        .map_err(|_| ProtocolError::ResourceExhausted { requested })?;
    owned.push_str(value);
    Ok(owned)
}

pub(crate) async fn read_subscription_content(
    path: &Path,
    expected_len: usize,
    owner_key: u64,
    lease: &mut SubscriptionLease,
) -> Result<String, ProtocolError> {
    let requested = expected_len
        .checked_add(1)
        .ok_or(ProtocolError::ResourceExhausted {
            requested: usize::MAX,
        })?;
    if reject_allocation_at(ServerAllocationSite::Content, owner_key) {
        return Err(ProtocolError::ResourceExhausted { requested });
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(requested)
        .map_err(|_| ProtocolError::ResourceExhausted { requested })?;
    bytes.resize(requested, 0);
    let mut file = tokio::fs::File::open(path).await?;
    let mut filled = 0;
    loop {
        let Some(unfilled) = bytes.get_mut(filled..) else {
            break;
        };
        if unfilled.is_empty() {
            break;
        }
        let read = file.read(unfilled).await?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    if filled > expected_len {
        return Err(ProtocolError::InvalidMessage(
            "live update changed while it was being read".into(),
        ));
    }
    let Some(read_bytes) = bytes.get(..filled) else {
        return Err(ProtocolError::InvalidMessage(
            "live update read past its own buffer".into(),
        ));
    };
    let text = std::str::from_utf8(read_bytes)
        .map_err(|_| ProtocolError::InvalidMessage("live update is not UTF-8".into()))?;
    let mut content = String::new();
    content
        .try_reserve_exact(filled)
        .map_err(|_| ProtocolError::ResourceExhausted { requested: filled })?;
    content.push_str(text);
    let read_peak = bytes.capacity().checked_add(content.capacity()).ok_or(
        ProtocolError::ResourceExhausted {
            requested: usize::MAX,
        },
    )?;
    lease.ensure_at_least(read_peak)?;
    Ok(content)
}

/// Map a request path onto a candidate file path without touching the disk.
///
/// Purely lexical: no syscall, so it is safe to run on a runtime worker. It
/// establishes nothing about whether the result is inside the site root —
/// symlinks are resolved by `resolve_path`, which is the only function that may
/// conclude a path is safe to serve.
fn resolve_path_lexical(root: &Path, request: &str) -> Result<PathBuf, ProtocolError> {
    let relative = request.trim_start_matches('/');
    let mut result = root.to_path_buf();
    for component in Path::new(relative).components() {
        match component {
            Component::Normal(part) => result.push(part),
            Component::CurDir => {}
            _ => return Err(ProtocolError::InvalidMessage("invalid static path".into())),
        }
    }
    if request.ends_with('/') || relative.is_empty() {
        result.push("index.aml");
    }
    if result.extension().is_none() {
        result.set_extension("aml");
    }
    Ok(result)
}

/// Resolve a request path to a file path that is proven to live inside `root`.
///
/// `root` must already be canonical; `StaticServerConfig::new` canonicalises it
/// once at bind time, where blocking costs nothing. This used to canonicalise
/// the root again on every request and probe `Path::exists` first — three
/// synchronous filesystem calls on a runtime worker thread for every GET and
/// every SUBSCRIBE, which is a hard ceiling on concurrent connections. One
/// async call replaces all three: `canonicalize` already fails for a path that
/// does not exist, so the probe was only ever telling us what it would tell us.
///
/// A path that does not resolve is returned unchanged rather than rejected. It
/// is not an escape attempt, it is a miss, and the caller's `open` turns it into
/// the 404 it always was.
async fn resolve_path(root: &Path, request: &str) -> Result<PathBuf, ProtocolError> {
    let candidate = resolve_path_lexical(root, request)?;
    let Ok(canonical) = tokio::fs::canonicalize(&candidate).await else {
        return Ok(candidate);
    };
    if !canonical.starts_with(root) {
        return Err(ProtocolError::InvalidMessage(
            "static path escapes the configured site root".into(),
        ));
    }
    Ok(canonical)
}

async fn recv(stream: &mut AtpServerStream, timeout: Duration) -> Result<RawFrame, ProtocolError> {
    tokio::time::timeout(timeout, stream.recv_frame())
        .await
        .map_err(|_| ProtocolError::Timeout)?
}

async fn send(stream: &mut AtpServerStream, frame: RawFrame) -> Result<(), ProtocolError> {
    send_with_timeout(stream, &frame, WRITE_TIMEOUT).await
}

/// Send an UPDATE from its parts under the same write deadline as any other
/// frame, so `stalled_frame_write_is_bounded` still covers the fan-out path.
async fn send_update_with_timeout(
    stream: &mut AtpServerStream,
    region: &str,
    content: &str,
    delta: bool,
    timeout: Duration,
) -> Result<(), ProtocolError> {
    tokio::time::timeout(timeout, stream.send_update_parts(region, content, delta))
        .await
        .map_err(|_| ProtocolError::Timeout)?
}

async fn send_update(
    stream: &mut AtpServerStream,
    region: &str,
    content: &str,
    delta: bool,
) -> Result<(), ProtocolError> {
    send_update_with_timeout(stream, region, content, delta, WRITE_TIMEOUT).await
}

async fn send_with_timeout(
    stream: &mut AtpServerStream,
    frame: &RawFrame,
    timeout: Duration,
) -> Result<(), ProtocolError> {
    tokio::time::timeout(timeout, stream.send_frame(frame))
        .await
        .map_err(|_| ProtocolError::Timeout)?
}

async fn send_error(
    stream: &mut AtpServerStream,
    code: u16,
    message: &str,
) -> Result<(), ProtocolError> {
    let error = ErrorMessage {
        code,
        message: Some(message.into()),
    };
    send(
        stream,
        RawFrame {
            msg_type: MessageType::Error,
            flags: 0,
            body: error.serialize()?.into_bytes(),
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use dustnet_core::protocol::frame::{allocate_frame_body, decode_header, encode_frame};
    use dustnet_core::protocol::message::UpdateFlags;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    /// A subscription with no real watcher behind it, for tests that only care
    /// about the table that holds them.
    fn placeholder_subscription(region: String, budget: &SubscriptionBudget) -> StaticSubscription {
        let (updates, receiver) =
            watch::channel(live_watch::LiveGeneration::empty_for_tests(budget).unwrap());
        // The sender must outlive the receiver for `borrow` to stay valid.
        std::mem::forget(updates);
        let baseline = {
            let guard = receiver.borrow();
            Arc::clone(&guard)
        };
        StaticSubscription {
            region,
            mode: SubscribeMode::Replace,
            baseline,
            updates: receiver,
            _metadata_lease: budget.reserve(0).unwrap(),
        }
    }

    #[test]
    fn static_subscription_backing_rejection_precedes_connection_state() {
        let rejection = AllocationRejectionGuard::at(ServerAllocationSite::Backing, 0);
        assert!(matches!(
            try_subscription_storage(),
            Err(ProtocolError::ResourceExhausted { .. })
        ));
        drop(rejection);
        let mut subscriptions = try_subscription_storage().unwrap();
        assert!(subscriptions.is_empty());
        let capacity = subscriptions.capacity();
        let allocation = subscriptions.as_ptr();
        let budget = SubscriptionBudget::new(1024);
        for index in 0..MAX_SUBSCRIPTIONS_PER_CONNECTION {
            subscriptions.push(placeholder_subscription(format!("region-{index}"), &budget));
        }
        assert_eq!(subscriptions.capacity(), capacity);
        assert_eq!(subscriptions.as_ptr(), allocation);
    }

    /// Canonicalise a tempdir for use as a site root.
    ///
    /// Production canonicalises the root once in `StaticServerConfig::new`, and
    /// `resolve_path` relies on that. Tests that drive `serve_subscription` and
    /// `poll_subscriptions` directly bypass the config, so they must establish
    /// the same invariant — on macOS a tempdir is `/var/...` whose canonical
    /// form is `/private/var/...`, and comparing the two rejects every path.
    fn site_root(directory: &tempfile::TempDir) -> PathBuf {
        std::fs::canonicalize(directory.path()).unwrap()
    }

    struct RawClient(TcpStream);

    impl RawClient {
        async fn connect(address: std::net::SocketAddr) -> Self {
            Self(TcpStream::connect(address).await.unwrap())
        }

        async fn send(&mut self, frame: RawFrame) {
            self.0
                .write_all(&encode_frame(&frame).unwrap())
                .await
                .unwrap();
        }

        async fn send_fragmented(&mut self, frame: RawFrame) {
            for byte in encode_frame(&frame).unwrap() {
                self.0.write_all(&[byte]).await.unwrap();
                tokio::task::yield_now().await;
            }
        }

        async fn receive(&mut self) -> RawFrame {
            let mut header = [0_u8; dustnet_core::protocol::HEADER_SIZE];
            self.0.read_exact(&mut header).await.unwrap();
            let (length, msg_type, flags) = decode_header(&header).unwrap();
            let mut body = allocate_frame_body(length as usize).unwrap();
            self.0.read_exact(&mut body).await.unwrap();
            RawFrame {
                msg_type,
                flags,
                body,
            }
        }

        async fn handshake(&mut self) {
            self.send(RawFrame {
                msg_type: MessageType::Hello,
                flags: 0,
                body: b"HELLO/0.2\nCapabilities: live-updates\n".to_vec(),
            })
            .await;
            assert_eq!(self.receive().await.msg_type, MessageType::Welcome);
        }
    }

    fn subscription_frame(mode: SubscribeMode) -> RawFrame {
        let mode = if mode == SubscribeMode::Delta {
            "Mode: delta\n"
        } else {
            ""
        };
        RawFrame {
            msg_type: MessageType::Subscribe,
            flags: 0,
            body: format!("SUBSCRIBE /ticker.aml\nRegion: ticker\n{mode}").into_bytes(),
        }
    }

    async fn negotiated_server_pair() -> (AtpServerStream, RawClient) {
        let listener = AtpListener::bind_plain("127.0.0.1", 0).await.unwrap();
        let address = listener.local_addr().unwrap();
        let mut client = RawClient::connect(address).await;
        let pending = listener.accept_pending().await.unwrap();
        let mut server = pending.handshake().await.unwrap();
        client
            .send(RawFrame {
                msg_type: MessageType::Hello,
                flags: 0,
                body: b"HELLO/0.2\nCapabilities: live-updates\n".to_vec(),
            })
            .await;
        assert_eq!(
            server.recv_frame().await.unwrap().msg_type,
            MessageType::Hello
        );
        send(
            &mut server,
            RawFrame {
                msg_type: MessageType::Welcome,
                flags: 0,
                body: b"WELCOME/0.2\nCapabilities: live-updates\n".to_vec(),
            },
        )
        .await
        .unwrap();
        assert_eq!(client.receive().await.msg_type, MessageType::Welcome);
        (server, client)
    }

    /// A registry running in the background with intervals short enough that a
    /// file change is observed promptly.
    ///
    /// The change source observes nothing on purpose: reconciliation alone must
    /// serve every watched file, so driving these tests through it also proves
    /// the fallback path the real watcher degrades to.
    fn test_watcher(budget: &SubscriptionBudget) -> (LiveWatcher, watch::Sender<bool>) {
        let (events_tx, events_rx) = mpsc::channel(64);
        // Hold the sender open for the registry's lifetime.
        std::mem::forget(events_tx);
        let (registry, watcher) =
            WatchRegistry::new(budget.clone(), Box::new(ManualChangeSource), events_rx);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        tokio::spawn(
            registry
                .with_intervals(Duration::from_millis(1), Duration::from_millis(5))
                .run(shutdown_rx),
        );
        (watcher, shutdown_tx)
    }

    async fn subscribe_for_test(
        server: &mut AtpServerStream,
        root: &Path,
        watcher: &LiveWatcher,
        budget: &SubscriptionBudget,
        mode: SubscribeMode,
    ) -> Result<(StaticSubscription, mpsc::Receiver<()>), ProtocolError> {
        let (wake_tx, wake_rx) = mpsc::channel(1);
        let subscription = serve_subscription(
            server,
            root,
            subscription_frame(mode),
            watcher,
            &wake_tx,
            budget,
        )
        .await?;
        Ok((subscription, wake_rx))
    }

    /// Wait for a wake, then flush. Mirrors the connection loop's own arm.
    async fn wake_and_flush(
        server: &mut AtpServerStream,
        subscriptions: &mut [StaticSubscription],
        wake: &mut mpsc::Receiver<()>,
    ) -> Result<(), ProtocolError> {
        let _ = tokio::time::timeout(Duration::from_secs(2), wake.recv()).await;
        flush_subscriptions(server, subscriptions).await
    }

    #[tokio::test]
    async fn static_subscription_complete_body_limit_and_release_are_exact() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ticker.aml");
        let prefix = "UPDATE \n\n".len() + "ticker".len();
        let exact = "x".repeat(dustnet_core::protocol::MAX_LIVE_UPDATE_SIZE - prefix);
        std::fs::write(&path, &exact).unwrap();
        let budget = SubscriptionBudget::new(8 * dustnet_core::protocol::MAX_LIVE_UPDATE_SIZE);
        let (watcher, _shutdown) = test_watcher(&budget);
        let (mut server, mut client) = negotiated_server_pair().await;

        let (subscription, _wake) = subscribe_for_test(
            &mut server,
            &site_root(&directory),
            &watcher,
            &budget,
            SubscribeMode::Replace,
        )
        .await
        .unwrap();
        let update = client.receive().await;
        assert_eq!(
            update.body.len(),
            dustnet_core::protocol::MAX_LIVE_UPDATE_SIZE,
            "a body at exactly the limit must still be sent"
        );
        assert_eq!(subscription.baseline.text, exact);
        // The subscription itself now retains only its region; the content is
        // charged once by the generation it shares.
        assert!(
            budget.used() >= subscription.region.capacity(),
            "the subscription retains its region; the content is charged once \
             by the generation it shares"
        );
        drop(subscription);

        // A file one byte too large cannot be framed for any subscriber, so it
        // must never be published at all. Asserting that it never appears is
        // the property; asserting *when* the registry notices would be a race.
        let oversize = "x".repeat(exact.len() + 1);
        std::fs::write(&path, &oversize).unwrap();
        for _ in 0..5 {
            if let Ok((subscription, _wake)) = subscribe_for_test(
                &mut server,
                &site_root(&directory),
                &watcher,
                &budget,
                SubscribeMode::Replace,
            )
            .await
            {
                assert_ne!(
                    subscription.baseline.text, oversize,
                    "a file too large to frame must never reach a subscriber"
                );
                let _ = tokio::time::timeout(Duration::from_millis(50), client.receive()).await;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn subscription_owner_keyed_allocation_rejection_rolls_back_every_candidate() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("ticker.aml"), "first").unwrap();
        let owner = subscription_owner_key("/ticker.aml", "ticker");
        for site in [ServerAllocationSite::Path, ServerAllocationSite::Region] {
            let budget = SubscriptionBudget::new(1024 * 1024);
            let (watcher, _shutdown) = test_watcher(&budget);
            let (mut server, mut client) = negotiated_server_pair().await;
            let rejection = AllocationRejectionGuard::at(site, owner);
            assert!(
                matches!(
                    subscribe_for_test(
                        &mut server,
                        &site_root(&directory),
                        &watcher,
                        &budget,
                        SubscribeMode::Delta,
                    )
                    .await,
                    Err(ProtocolError::ResourceExhausted { .. })
                ),
                "{site:?} must refuse the subscription"
            );
            drop(rejection);
            assert_eq!(budget.used(), 0, "candidate leaked at {site:?}");
            assert!(
                tokio::time::timeout(Duration::from_millis(20), client.receive())
                    .await
                    .is_err(),
                "a refused subscription must not have sent anything"
            );
        }
    }

    #[tokio::test]
    async fn initial_subscription_send_failure_drops_complete_candidate() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("ticker.aml"), "first").unwrap();
        let budget = SubscriptionBudget::new(1024 * 1024);
        let (watcher, _shutdown) = test_watcher(&budget);
        let (mut server, _client) = negotiated_server_pair().await;
        assert!(
            server
                .send_frame(&RawFrame {
                    msg_type: MessageType::Update,
                    flags: 0,
                    body: b"invalid".to_vec(),
                })
                .await
                .is_err()
        );

        let attempt = subscribe_for_test(
            &mut server,
            &site_root(&directory),
            &watcher,
            &budget,
            SubscribeMode::Delta,
        )
        .await;
        assert!(
            attempt.is_err(),
            "a poisoned stream must fail the subscribe"
        );
        drop(attempt);
        // The per-connection lease is returned; the shared generation is
        // released once the registry reaps an entry nobody is listening to.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while budget.used() != 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "a failed subscribe leaked {} bytes",
                budget.used()
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// The property the old polling test protected, restated for a shared read:
    /// a subscriber's delta baseline advances only on a successful send, so a
    /// missed generation resynchronises itself instead of corrupting the
    /// subscriber's view.
    #[tokio::test]
    async fn shared_generation_pressure_preserves_the_delta_baseline() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ticker.aml");
        std::fs::write(&path, "first").unwrap();
        let budget = SubscriptionBudget::new(1024 * 1024);
        let (watcher, _shutdown) = test_watcher(&budget);
        let (mut server, mut client) = negotiated_server_pair().await;
        let (subscription, mut wake) = subscribe_for_test(
            &mut server,
            &site_root(&directory),
            &watcher,
            &budget,
            SubscribeMode::Delta,
        )
        .await
        .unwrap();
        let _ = client.receive().await;
        let mut subscriptions = vec![subscription];

        // An unsendable file must not advance anyone's baseline.
        let prefix = "UPDATE \n\n".len() + subscriptions[0].region.len();
        std::fs::write(
            &path,
            "x".repeat(dustnet_core::protocol::MAX_LIVE_UPDATE_SIZE - prefix + 1),
        )
        .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        flush_subscriptions(&mut server, &mut subscriptions)
            .await
            .unwrap();
        assert_eq!(subscriptions[0].baseline.text, "first");
        assert!(
            tokio::time::timeout(Duration::from_millis(50), client.receive())
                .await
                .is_err(),
            "an unsendable generation must produce no wire traffic"
        );

        // A sendable successor resumes deltas from the preserved baseline.
        std::fs::write(&path, "first-second").unwrap();
        wake_and_flush(&mut server, &mut subscriptions, &mut wake)
            .await
            .unwrap();
        let update = client.receive().await;
        assert_eq!(update.flags, UpdateFlags { delta: true }.to_bits());
        assert_eq!(
            UpdateMessage::parse(std::str::from_utf8(&update.body).unwrap())
                .unwrap()
                .content,
            "-second",
            "the delta must be computed against what this subscriber last saw"
        );
        assert_eq!(subscriptions[0].baseline.text, "first-second");

        // A flush with nothing new sends nothing.
        flush_subscriptions(&mut server, &mut subscriptions)
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), client.receive())
                .await
                .is_err()
        );

        // Divergent content falls back to a complete replace.
        std::fs::write(&path, "replacement").unwrap();
        wake_and_flush(&mut server, &mut subscriptions, &mut wake)
            .await
            .unwrap();
        let update = client.receive().await;
        assert_eq!(update.flags, 0, "a divergent value is a full replace");
        assert_eq!(
            UpdateMessage::parse(std::str::from_utf8(&update.body).unwrap())
                .unwrap()
                .content,
            "replacement"
        );
        assert_eq!(subscriptions[0].baseline.text, "replacement");
    }

    /// The claim the whole change exists to make, at the connection level: two
    /// connections watching one path are served by one read, and each still
    /// gets the encoding its own subscription asked for. Sharing the read must
    /// not collapse into sharing the message.
    #[tokio::test]
    async fn two_connections_share_one_read_with_independent_modes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ticker.aml");
        std::fs::write(&path, "first").unwrap();
        let budget = SubscriptionBudget::new(1024 * 1024);
        let (watcher, _shutdown) = test_watcher(&budget);

        let (mut delta_server, mut delta_client) = negotiated_server_pair().await;
        let (delta_subscription, mut delta_wake) = subscribe_for_test(
            &mut delta_server,
            &site_root(&directory),
            &watcher,
            &budget,
            SubscribeMode::Delta,
        )
        .await
        .unwrap();
        let _ = delta_client.receive().await;

        let (mut replace_server, mut replace_client) = negotiated_server_pair().await;
        let (replace_subscription, mut replace_wake) = subscribe_for_test(
            &mut replace_server,
            &site_root(&directory),
            &watcher,
            &budget,
            SubscribeMode::Replace,
        )
        .await
        .unwrap();
        let _ = replace_client.receive().await;

        assert!(
            Arc::ptr_eq(&delta_subscription.baseline, &replace_subscription.baseline),
            "both connections must be looking at the very same generation"
        );
        let shared = budget.used();

        let mut delta_subscriptions = vec![delta_subscription];
        let mut replace_subscriptions = vec![replace_subscription];
        std::fs::write(&path, "first-second").unwrap();

        wake_and_flush(&mut delta_server, &mut delta_subscriptions, &mut delta_wake)
            .await
            .unwrap();
        wake_and_flush(
            &mut replace_server,
            &mut replace_subscriptions,
            &mut replace_wake,
        )
        .await
        .unwrap();

        let delta_update = delta_client.receive().await;
        assert_eq!(delta_update.flags, UpdateFlags { delta: true }.to_bits());
        assert_eq!(
            UpdateMessage::parse(std::str::from_utf8(&delta_update.body).unwrap())
                .unwrap()
                .content,
            "-second",
            "the delta subscriber gets a suffix"
        );

        let replace_update = replace_client.receive().await;
        assert_eq!(replace_update.flags, 0);
        assert_eq!(
            UpdateMessage::parse(std::str::from_utf8(&replace_update.body).unwrap())
                .unwrap()
                .content,
            "first-second",
            "the replace subscriber gets the whole value, from the same read"
        );

        assert!(
            Arc::ptr_eq(
                &delta_subscriptions[0].baseline,
                &replace_subscriptions[0].baseline
            ),
            "one read served both subscribers"
        );
        // Two subscribers of one path cost one copy of its content, so the
        // second subscriber adds only its own region.
        assert!(
            budget.used() < shared + "first-second".len() * 2,
            "content appears to have been charged per subscriber"
        );
    }

    #[tokio::test]
    async fn read_and_send_failures_preserve_the_delta_baseline() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ticker.aml");
        std::fs::write(&path, "first").unwrap();
        let budget = SubscriptionBudget::new(1024 * 1024);
        let (watcher, _shutdown) = test_watcher(&budget);
        let (mut server, mut client) = negotiated_server_pair().await;
        let (subscription, _wake) = subscribe_for_test(
            &mut server,
            &site_root(&directory),
            &watcher,
            &budget,
            SubscribeMode::Delta,
        )
        .await
        .unwrap();
        let _ = client.receive().await;
        let mut subscriptions = vec![subscription];

        // A file that is not UTF-8 cannot be published at all.
        std::fs::write(&path, [0xff, 0xfe]).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        flush_subscriptions(&mut server, &mut subscriptions)
            .await
            .unwrap();
        assert_eq!(subscriptions[0].baseline.text, "first");

        // A send failure must leave the baseline where it was, so the missed
        // generation is re-sent rather than skipped.
        std::fs::write(&path, "first-after-failure").unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            server
                .send_frame(&RawFrame {
                    msg_type: MessageType::Update,
                    flags: 0,
                    body: b"invalid".to_vec(),
                })
                .await
                .is_err()
        );
        assert!(
            flush_subscriptions(&mut server, &mut subscriptions)
                .await
                .is_err()
        );
        assert_eq!(subscriptions[0].baseline.text, "first");
        drop(client);
    }

    #[tokio::test]
    async fn static_subscription_polls_delta_and_unsubscribe_stops_updates() {
        let directory = tempfile::tempdir().unwrap();
        let live_path = directory.path().join("ticker.aml");
        std::fs::write(&live_path, "first").unwrap();
        let config = StaticServerConfig::bind_plaintext_loopback(
            directory.path().to_path_buf(),
            "127.0.0.1",
            0,
        )
        .await
        .unwrap();
        let address = config.local_addr().unwrap();
        let mut server = StaticServer::new(config);
        let shutdown = server.shutdown_handle();
        let task = tokio::spawn(async move { server.run().await });

        let mut client = RawClient::connect(address).await;
        client.handshake().await;
        client
            .send(RawFrame {
                msg_type: MessageType::Subscribe,
                flags: 0,
                body: b"SUBSCRIBE /ticker.aml\nRegion: ticker\nMode: delta\n".to_vec(),
            })
            .await;
        let initial = client.receive().await;
        assert_eq!(initial.msg_type, MessageType::Update);
        assert_eq!(initial.flags, 0);
        assert!(String::from_utf8(initial.body).unwrap().ends_with("first"));

        std::fs::write(&live_path, "first-second").unwrap();
        let changed = tokio::time::timeout(Duration::from_secs(2), client.receive())
            .await
            .unwrap();
        assert_eq!(changed.flags, 1);
        assert!(
            String::from_utf8(changed.body)
                .unwrap()
                .ends_with("-second")
        );

        client
            .send(RawFrame {
                msg_type: MessageType::Unsubscribe,
                flags: 0,
                body: Vec::new(),
            })
            .await;
        std::fs::write(&live_path, "first-second-third").unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(700), client.receive())
                .await
                .is_err()
        );

        shutdown.send(true).unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn reconnect_and_resubscribe_replays_current_content_then_resumes_deltas() {
        let directory = tempfile::tempdir().unwrap();
        let live_path = directory.path().join("ticker.aml");
        std::fs::write(&live_path, "first").unwrap();
        let config = StaticServerConfig::bind_plaintext_loopback(
            directory.path().to_path_buf(),
            "127.0.0.1",
            0,
        )
        .await
        .unwrap();
        let address = config.local_addr().unwrap();
        let mut server = StaticServer::new(config);
        let shutdown = server.shutdown_handle();
        let task = tokio::spawn(async move { server.run().await });

        let subscribe = || RawFrame {
            msg_type: MessageType::Subscribe,
            flags: 0,
            body: b"SUBSCRIBE /ticker.aml\nRegion: ticker\nMode: delta\n".to_vec(),
        };

        let mut first = RawClient::connect(address).await;
        first.handshake().await;
        first.send(subscribe()).await;
        let initial = first.receive().await;
        assert_eq!(initial.msg_type, MessageType::Update);
        assert_eq!(initial.flags, 0);
        assert_eq!(
            UpdateMessage::parse(std::str::from_utf8(&initial.body).unwrap())
                .unwrap()
                .content,
            "first"
        );
        drop(first);

        std::fs::write(&live_path, "first-while-disconnected").unwrap();
        let mut reconnected = RawClient::connect(address).await;
        reconnected.handshake().await;
        reconnected.send(subscribe()).await;
        let replay = reconnected.receive().await;
        assert_eq!(replay.msg_type, MessageType::Update);
        assert_eq!(replay.flags, 0, "a replay is a complete current value");
        assert_eq!(
            UpdateMessage::parse(std::str::from_utf8(&replay.body).unwrap())
                .unwrap()
                .content,
            "first-while-disconnected"
        );

        std::fs::write(&live_path, "first-while-disconnected-after").unwrap();
        let changed = tokio::time::timeout(Duration::from_secs(2), reconnected.receive())
            .await
            .expect("reconnected subscription did not resume polling");
        assert_eq!(changed.msg_type, MessageType::Update);
        assert_eq!(changed.flags, UpdateFlags { delta: true }.to_bits());
        assert_eq!(
            UpdateMessage::parse(std::str::from_utf8(&changed.body).unwrap())
                .unwrap()
                .content,
            "-after"
        );

        drop(reconnected);
        shutdown.send(true).unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn stalled_frame_body_read_is_bounded() {
        let listener = AtpListener::bind_plain("127.0.0.1", 0).await.unwrap();
        let address = listener.local_addr().unwrap();
        let mut client = TcpStream::connect(address).await.unwrap();
        let pending = listener.accept_pending().await.unwrap();
        let mut server_stream = pending.handshake().await.unwrap();
        let encoded = encode_frame(&RawFrame {
            msg_type: MessageType::Hello,
            flags: 0,
            body: b"HELLO/0.2\n".to_vec(),
        })
        .unwrap();

        client
            .write_all(&encoded[..dustnet_core::protocol::HEADER_SIZE + 1])
            .await
            .unwrap();
        let result = recv(&mut server_stream, Duration::from_millis(50)).await;
        assert!(matches!(result, Err(ProtocolError::Timeout)));
    }

    #[tokio::test]
    async fn stalled_frame_write_is_bounded() {
        let listener = AtpListener::bind_plain("127.0.0.1", 0).await.unwrap();
        let address = listener.local_addr().unwrap();
        let mut client = TcpStream::connect(address).await.unwrap();
        let pending = listener.accept_pending().await.unwrap();
        let mut server_stream = pending.handshake().await.unwrap();

        client
            .write_all(
                &encode_frame(&RawFrame {
                    msg_type: MessageType::Hello,
                    flags: 0,
                    body: b"HELLO/0.2\nCapabilities: live-updates\n".to_vec(),
                })
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            recv(&mut server_stream, Duration::from_secs(1))
                .await
                .unwrap()
                .msg_type,
            MessageType::Hello
        );
        send_with_timeout(
            &mut server_stream,
            &RawFrame {
                msg_type: MessageType::Welcome,
                flags: 0,
                body: b"WELCOME/0.2\nCapabilities: live-updates\n".to_vec(),
            },
            Duration::from_secs(1),
        )
        .await
        .unwrap();

        // Never read from the peer. Repeated maximum-sized live updates fill
        // the kernel send window, after which the same timeout wrapper used by
        // production must bound the pending write.
        let update = UpdateMessage {
            region: "ticker".into(),
            content: "x".repeat(dustnet_core::protocol::MAX_LIVE_UPDATE_SIZE - 64),
            flags: UpdateFlags::default(),
        };
        let frame = RawFrame {
            msg_type: MessageType::Update,
            flags: 0,
            body: update.serialize().unwrap().into_bytes(),
        };
        let mut timed_out = false;
        for _ in 0..128 {
            match send_with_timeout(&mut server_stream, &frame, Duration::from_millis(50)).await {
                Err(ProtocolError::Timeout) => {
                    timed_out = true;
                    break;
                }
                Ok(()) => {}
                Err(error) => panic!("unexpected write failure: {error}"),
            }
        }
        assert!(timed_out, "peer receive window accepted more than 128 MiB");
    }

    #[tokio::test]
    async fn plaintext_configuration_rejects_non_loopback_binding() {
        let directory = tempfile::tempdir().unwrap();
        let result = StaticServerConfig::bind_plaintext_loopback(
            directory.path().to_path_buf(),
            "0.0.0.0",
            0,
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn invalid_tls_handshake_is_closed_without_stopping_the_listener() {
        let directory = tempfile::tempdir().unwrap();
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
        let task = tokio::spawn(async move { server.run().await });

        let mut invalid = TcpStream::connect(address).await.unwrap();
        invalid.write_all(b"not a TLS handshake").await.unwrap();
        invalid.shutdown().await.unwrap();
        let mut alert = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), invalid.read_to_end(&mut alert))
            .await
            .expect("invalid handshakes must be bounded")
            .unwrap();
        assert!(!alert.is_empty(), "rustls should return a TLS alert");

        // A rejected handshake must not poison the accept loop.
        let second = TcpStream::connect(address).await.unwrap();
        assert_eq!(second.peer_addr().unwrap(), address);
        drop(second);

        shutdown.send(true).unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn silent_tls_handshake_is_closed_at_the_production_deadline() {
        let directory = tempfile::tempdir().unwrap();
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
        let task = tokio::spawn(async move { server.run().await });

        let started = std::time::Instant::now();
        let mut silent = TcpStream::connect(address).await.unwrap();
        let mut received = Vec::new();
        tokio::time::timeout(
            HANDSHAKE_TIMEOUT + Duration::from_secs(2),
            silent.read_to_end(&mut received),
        )
        .await
        .expect("silent TLS handshake must be closed at the configured deadline")
        .unwrap();
        assert!(
            started.elapsed() >= HANDSHAKE_TIMEOUT / 2,
            "the listener closed materially before its configured handshake timeout: {:?}",
            started.elapsed()
        );
        assert!(received.is_empty());

        shutdown.send(true).unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn fragmented_frames_are_read_completely_and_missing_files_are_bounded_errors() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("index.aml"), "[page][/page]").unwrap();
        let config = StaticServerConfig::bind_plaintext_loopback(
            directory.path().to_path_buf(),
            "127.0.0.1",
            0,
        )
        .await
        .unwrap();
        let address = config.local_addr().unwrap();
        let mut server = StaticServer::new(config);
        let shutdown = server.shutdown_handle();
        let task = tokio::spawn(async move { server.run().await });

        let mut client = RawClient::connect(address).await;
        client
            .send_fragmented(RawFrame {
                msg_type: MessageType::Hello,
                flags: 0,
                body: b"HELLO/0.2\n".to_vec(),
            })
            .await;
        assert_eq!(client.receive().await.msg_type, MessageType::Welcome);

        client
            .send_fragmented(RawFrame {
                msg_type: MessageType::Get,
                flags: 0,
                body: b"GET /missing.aml\n".to_vec(),
            })
            .await;
        let response = client.receive().await;
        assert_eq!(response.msg_type, MessageType::Error);
        assert!(response.body.len() <= dustnet_core::protocol::MAX_CONTROL_MESSAGE_SIZE);

        drop(client);
        shutdown.send(true).unwrap();
        task.await.unwrap().unwrap();
    }

    fn ping_frame() -> RawFrame {
        RawFrame {
            msg_type: MessageType::Ping,
            flags: 0,
            body: Vec::new(),
        }
    }

    /// Spawn a server whose idle deadline is `read_timeout`, and return its
    /// address plus the handles needed to stop it.
    async fn server_with_read_timeout(
        directory: &std::path::Path,
        read_timeout: Duration,
    ) -> (
        std::net::SocketAddr,
        watch::Sender<bool>,
        tokio::task::JoinHandle<Result<(), ProtocolError>>,
    ) {
        let config =
            StaticServerConfig::bind_plaintext_loopback(directory.to_path_buf(), "127.0.0.1", 0)
                .await
                .unwrap()
                .with_read_timeout(read_timeout);
        let address = config.local_addr().unwrap();
        let mut server = StaticServer::new(config);
        let shutdown = server.shutdown_handle();
        let task = tokio::spawn(async move { server.run().await });
        (address, shutdown, task)
    }

    /// Subscribing must not wait on the platform watcher.
    ///
    /// Registering an FSEvents watch was measured at 1.7 seconds. The registry
    /// is one task shared by every connection, so a blocking registration there
    /// stalls every other subscriber behind it — which is why the platform
    /// watcher lives on its own thread. Watch registration is only ever a
    /// latency optimisation, since reconciliation serves the file regardless,
    /// so a subscriber must never be waiting on it.
    #[tokio::test]
    async fn subscribing_is_not_delayed_by_watcher_registration() {
        let directory = tempfile::tempdir().unwrap();
        let (address, shutdown, task) =
            server_with_read_timeout(directory.path(), READ_TIMEOUT).await;
        // Created after the server bound, so this is the first watch of it.
        std::fs::write(directory.path().join("ticker.aml"), "initial content").unwrap();

        let mut client = RawClient::connect(address).await;
        client.handshake().await;
        client
            .send(RawFrame {
                msg_type: MessageType::Subscribe,
                flags: 0,
                body: b"SUBSCRIBE /ticker\nRegion: my-region\n".to_vec(),
            })
            .await;

        let started = std::time::Instant::now();
        let frame = tokio::time::timeout(Duration::from_secs(3), client.receive())
            .await
            .expect("an initial UPDATE must arrive");
        let elapsed = started.elapsed();
        assert_eq!(frame.msg_type, MessageType::Update);
        assert!(
            elapsed < Duration::from_millis(500),
            "the initial UPDATE took {elapsed:?}; subscribing appears to be \
             waiting on platform watch registration again"
        );

        drop(client);
        shutdown.send(true).unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn ping_is_answered_with_an_empty_pong() {
        let directory = tempfile::tempdir().unwrap();
        let (address, shutdown, task) =
            server_with_read_timeout(directory.path(), READ_TIMEOUT).await;

        let mut client = RawClient::connect(address).await;
        client.handshake().await;
        client.send(ping_frame()).await;
        let pong = tokio::time::timeout(Duration::from_secs(2), client.receive())
            .await
            .expect("PONG must arrive");
        assert_eq!(pong.msg_type, MessageType::Pong);
        assert_eq!(pong.flags, 0);
        assert!(pong.body.is_empty(), "PONG carries no body");

        drop(client);
        shutdown.send(true).unwrap();
        task.await.unwrap().unwrap();
    }

    /// The control for the test below: without PING the deadline really does
    /// close the connection. Without this, a keepalive test could pass against
    /// a server that had simply stopped enforcing any deadline at all.
    #[tokio::test]
    async fn a_silent_connection_is_closed_at_the_idle_deadline() {
        let directory = tempfile::tempdir().unwrap();
        let deadline = Duration::from_millis(300);
        let (address, shutdown, task) = server_with_read_timeout(directory.path(), deadline).await;

        let mut client = RawClient::connect(address).await;
        client.handshake().await;

        let mut byte = [0_u8; 1];
        let started = std::time::Instant::now();
        let result = tokio::time::timeout(deadline * 10, client.0.read(&mut byte))
            .await
            .expect("a silent connection must be closed at the deadline");
        assert!(matches!(result, Ok(0) | Err(_)));
        assert!(
            started.elapsed() >= deadline / 2,
            "the connection closed before the deadline could have elapsed"
        );

        shutdown.send(true).unwrap();
        task.await.unwrap().unwrap();
    }

    /// PING exists to reset that deadline. Pinging at a third of it keeps a
    /// connection alive well past the point where the control above dies.
    #[tokio::test]
    async fn pinging_holds_a_connection_open_past_the_idle_deadline() {
        let directory = tempfile::tempdir().unwrap();
        let deadline = Duration::from_millis(300);
        let (address, shutdown, task) = server_with_read_timeout(directory.path(), deadline).await;

        let mut client = RawClient::connect(address).await;
        client.handshake().await;

        // Four deadlines' worth of wall clock, pinged at a third of each.
        for _ in 0..12 {
            tokio::time::sleep(deadline / 3).await;
            client.send(ping_frame()).await;
            let pong = tokio::time::timeout(Duration::from_secs(2), client.receive())
                .await
                .expect("the connection must still be open");
            assert_eq!(pong.msg_type, MessageType::Pong);
        }

        drop(client);
        shutdown.send(true).unwrap();
        task.await.unwrap().unwrap();
    }

    /// A PING sent while a request is outstanding must not be mistaken for the
    /// response, and must not poison the connection state machine.
    #[tokio::test]
    async fn ping_does_not_disturb_an_in_flight_request() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("index.aml"), "hello").unwrap();
        let (address, shutdown, task) =
            server_with_read_timeout(directory.path(), READ_TIMEOUT).await;

        let mut client = RawClient::connect(address).await;
        client.handshake().await;
        client
            .send(RawFrame {
                msg_type: MessageType::Get,
                flags: 0,
                body: b"GET /\n".to_vec(),
            })
            .await;
        client.send(ping_frame()).await;

        // Both replies must arrive, and the PAGE must not have been displaced.
        let mut seen_page = false;
        let mut seen_pong = false;
        for _ in 0..2 {
            let frame = tokio::time::timeout(Duration::from_secs(2), client.receive())
                .await
                .expect("both replies must arrive");
            match frame.msg_type {
                MessageType::Page => seen_page = true,
                MessageType::Pong => seen_pong = true,
                other => panic!("unexpected {other:?}"),
            }
        }
        assert!(seen_page && seen_pong);

        drop(client);
        shutdown.send(true).unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn fifth_simultaneous_connection_from_one_ip_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let config = StaticServerConfig::bind_plaintext_loopback(
            directory.path().to_path_buf(),
            "127.0.0.1",
            0,
        )
        .await
        .unwrap();
        let address = config.local_addr().unwrap();
        let mut server = StaticServer::new(config);
        let shutdown = server.shutdown_handle();
        let task = tokio::spawn(async move { server.run().await });

        let mut admitted = Vec::new();
        for _ in 0..DEFAULT_MAX_CONNECTIONS_PER_IP {
            let mut client = RawClient::connect(address).await;
            client.handshake().await;
            admitted.push(client);
        }

        let mut rejected = TcpStream::connect(address).await.unwrap();
        rejected
            .write_all(
                &encode_frame(&RawFrame {
                    msg_type: MessageType::Hello,
                    flags: 0,
                    body: b"HELLO/0.2\n".to_vec(),
                })
                .unwrap(),
            )
            .await
            .ok();
        let mut byte = [0_u8; 1];
        let result = tokio::time::timeout(Duration::from_secs(1), rejected.read(&mut byte))
            .await
            .expect("over-limit connection must be closed promptly");
        assert!(matches!(result, Ok(0) | Err(_)));

        drop(rejected);
        drop(admitted);
        shutdown.send(true).unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn global_connection_limit_rejects_a_concurrent_flood() {
        let directory = tempfile::tempdir().unwrap();
        let config = StaticServerConfig::bind_plaintext_loopback(
            directory.path().to_path_buf(),
            "127.0.0.1",
            0,
        )
        .await
        .unwrap()
        .with_connection_limits(3, 4);
        let address = config.local_addr().unwrap();
        let mut server = StaticServer::new(config);
        let shutdown = server.shutdown_handle();
        let task = tokio::spawn(async move { server.run().await });

        let mut admitted = Vec::new();
        for _ in 0..3 {
            let mut client = RawClient::connect(address).await;
            client.handshake().await;
            admitted.push(client);
        }

        let mut rejected = RawClient::connect(address).await;
        rejected
            .send(RawFrame {
                msg_type: MessageType::Hello,
                flags: 0,
                body: b"HELLO/0.2\n".to_vec(),
            })
            .await;
        let mut byte = [0_u8; 1];
        let result = tokio::time::timeout(Duration::from_secs(1), rejected.0.read(&mut byte))
            .await
            .expect("globally over-limit connection must close promptly");
        assert!(matches!(result, Ok(0) | Err(_)));

        drop(rejected);
        drop(admitted);
        shutdown.send(true).unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn malformed_tls_material_is_rejected_before_serving() {
        let directory = tempfile::tempdir().unwrap();
        let certificate = directory.path().join("certificate.pem");
        let private_key = directory.path().join("private-key.pem");
        std::fs::write(&certificate, "not a certificate").unwrap();
        std::fs::write(&private_key, "not a private key").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&certificate, std::fs::Permissions::from_mode(0o600)).unwrap();
            std::fs::set_permissions(&private_key, std::fs::Permissions::from_mode(0o600)).unwrap();
        }

        let result = StaticServerConfig::bind_tls(
            directory.path().to_path_buf(),
            "127.0.0.1",
            0,
            certificate.to_str().unwrap(),
            private_key.to_str().unwrap(),
        )
        .await;
        assert!(matches!(result, Err(ProtocolError::Tls(_))));
    }

    /// The test above only proves shutdown finishes inside the drain deadline —
    /// which it did even when every task had to be aborted. This one proves the
    /// connections leave voluntarily, so the deadline is a backstop rather than
    /// the normal path. That distinction is what makes 2048 connections
    /// survivable: aborting 2048 tasks is very different from joining them.
    #[tokio::test]
    async fn idle_connections_leave_voluntarily_well_inside_the_drain_deadline() {
        let directory = tempfile::tempdir().unwrap();
        let (address, shutdown, task) =
            server_with_read_timeout(directory.path(), READ_TIMEOUT).await;

        let mut clients = Vec::new();
        for _ in 0..DEFAULT_MAX_CONNECTIONS_PER_IP {
            let mut client = RawClient::connect(address).await;
            client.handshake().await;
            clients.push(client);
        }

        let started = std::time::Instant::now();
        shutdown.send(true).unwrap();
        task.await.unwrap().unwrap();
        let elapsed = started.elapsed();

        assert!(
            elapsed < DRAIN_TIMEOUT / 2,
            "shutdown took {elapsed:?}, which suggests connections were aborted \
             at the drain deadline rather than leaving on their own"
        );
        drop(clients);
    }

    /// A per-IP ceiling above the global ceiling does not describe a reachable
    /// state, and zero would refuse every connection including the operator's
    /// own health check.
    #[tokio::test]
    async fn connection_limits_are_clamped_to_a_reachable_range() {
        let directory = tempfile::tempdir().unwrap();
        let config = || async {
            StaticServerConfig::bind_plaintext_loopback(
                directory.path().to_path_buf(),
                "127.0.0.1",
                0,
            )
            .await
            .unwrap()
        };

        let zeroed = config().await.with_connection_limits(0, 0);
        assert_eq!(zeroed.max_connections, 1, "zero would refuse every client");
        assert_eq!(zeroed.max_connections_per_ip, 1);

        let inverted = config().await.with_connection_limits(4, 100);
        assert_eq!(inverted.max_connections, 4);
        assert_eq!(
            inverted.max_connections_per_ip, 4,
            "a per-IP ceiling above the global ceiling is not a reachable state"
        );

        let ordinary = config().await.with_connection_limits(2048, 4);
        assert_eq!(ordinary.max_connections, 2048);
        assert_eq!(ordinary.max_connections_per_ip, 4);
    }

    #[tokio::test]
    async fn shutdown_is_bounded_with_an_idle_client() {
        let directory = tempfile::tempdir().unwrap();
        let config = StaticServerConfig::bind_plaintext_loopback(
            directory.path().to_path_buf(),
            "127.0.0.1",
            0,
        )
        .await
        .unwrap();
        let address = config.local_addr().unwrap();
        let mut server = StaticServer::new(config);
        let shutdown = server.shutdown_handle();
        let task = tokio::spawn(async move { server.run().await });

        let mut client = RawClient::connect(address).await;
        client.handshake().await;
        shutdown.send(true).unwrap();

        tokio::time::timeout(DRAIN_TIMEOUT + Duration::from_secs(2), task)
            .await
            .expect("shutdown exceeded its drain deadline")
            .unwrap()
            .unwrap();
        drop(client);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlinks_cannot_escape_static_root() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        symlink(outside.path(), root.path().join("escape.aml")).unwrap();
        assert!(resolve_path(&canonical_root, "/escape.aml").await.is_err());
    }

    /// The lexical half must not touch the disk, so it can run on a runtime
    /// worker. A traversal attempt is rejected before any syscall.
    #[test]
    fn lexical_resolution_rejects_traversal_without_touching_the_disk() {
        let root = Path::new("/nonexistent-root");
        assert!(resolve_path_lexical(root, "/../escape.aml").is_err());
        assert_eq!(
            resolve_path_lexical(root, "/page").unwrap(),
            root.join("page.aml"),
            "a bare name gains the .aml extension"
        );
        assert_eq!(
            resolve_path_lexical(root, "/").unwrap(),
            root.join("index.aml"),
            "a directory request resolves to its index"
        );
    }

    /// A miss is a 404, not an escape. Regression guard for the removed
    /// `Path::exists` probe.
    #[tokio::test]
    async fn a_missing_file_resolves_without_error() {
        let root = tempfile::tempdir().unwrap();
        let canonical_root = std::fs::canonicalize(root.path()).unwrap();
        let resolved = resolve_path(&canonical_root, "/absent.aml").await.unwrap();
        assert_eq!(resolved, canonical_root.join("absent.aml"));
    }

    #[tokio::test]
    async fn static_resource_read_is_fallible_and_returns_only_filled_bytes() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), b"wasm").unwrap();
        let opened = tokio::fs::File::open(file.path()).await.unwrap();
        let body = read_static_body(opened, 8, 41).await.unwrap();
        assert_eq!(body, b"wasm");

        let opened = tokio::fs::File::open(file.path()).await.unwrap();
        let _rejection = AllocationRejectionGuard::at(ServerAllocationSite::StaticBody, 41);
        assert!(matches!(
            read_static_body(opened, 4, 41).await,
            Err(ProtocolError::ResourceExhausted { requested: 5 })
        ));
    }

    #[tokio::test]
    async fn static_resource_growth_after_metadata_is_rejected() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), b"old").unwrap();
        let opened = tokio::fs::File::open(file.path()).await.unwrap();
        std::fs::write(file.path(), b"new content").unwrap();
        assert!(matches!(
            read_static_body(opened, 3, 42).await,
            Err(ProtocolError::InvalidMessage(_))
        ));
    }

    /// A resolver's content reaches the wire, and the placeholder does not.
    ///
    /// The unit tests in `include` cover substitution; this covers the wiring,
    /// which is the part that can silently not happen — a resolver configured
    /// but never consulted would leave every page looking exactly like the
    /// no-resolver case.
    #[tokio::test]
    async fn a_configured_resolver_substitutes_includes_on_the_wire() {
        struct Stories;
        impl crate::include::IncludeResolver for Stories {
            fn resolve(
                &self,
                name: &str,
                request: &crate::include::IncludeRequest<'_>,
            ) -> Option<Vec<dustnet_core::scanner::Token>> {
                assert_eq!(request.path, "/index.aml");
                (name == "links").then(|| {
                    vec![
                        dustnet_core::scanner::Token::OpenTag {
                            name: "text".into(),
                            attributes: Vec::new(),
                            self_closing: false,
                        },
                        dustnet_core::scanner::Token::Text("a generated story".into()),
                        dustnet_core::scanner::Token::CloseTag {
                            name: "text".into(),
                        },
                    ]
                })
            }
        }

        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("index.aml"),
            r#"[page mode=document][include name="links" /][/page]"#,
        )
        .unwrap();
        let config = StaticServerConfig::bind_plaintext_loopback(
            directory.path().to_path_buf(),
            "127.0.0.1",
            0,
        )
        .await
        .unwrap()
        .with_include_resolver(Arc::new(Stories));
        let address = config.local_addr().unwrap();
        let mut server = StaticServer::new(config);
        let shutdown = server.shutdown_handle();
        let task = tokio::spawn(async move { server.run().await });

        let mut client = RawClient::connect(address).await;
        client
            .send(RawFrame {
                msg_type: MessageType::Hello,
                flags: 0,
                body: b"HELLO/0.2\n".to_vec(),
            })
            .await;
        assert_eq!(client.receive().await.msg_type, MessageType::Welcome);
        client
            .send(RawFrame {
                msg_type: MessageType::Get,
                flags: 0,
                body: b"GET /index.aml\n".to_vec(),
            })
            .await;

        let response = client.receive().await;
        assert_eq!(response.msg_type, MessageType::Page);
        let body = String::from_utf8(response.body).unwrap();
        assert!(body.contains("a generated story"), "{body}");
        assert!(
            !body.contains("include"),
            "the placeholder survived: {body}"
        );

        drop(client);
        shutdown.send(true).unwrap();
        task.await.unwrap().unwrap();
    }

    /// Without a resolver the server is byte-for-byte what it was: the include
    /// travels to the client, which renders it as nothing. This is the
    /// assertion that `dustnetd` has not quietly become dynamic.
    #[tokio::test]
    async fn without_a_resolver_includes_are_served_verbatim() {
        let source = r#"[page mode=document][include name="links" /][/page]"#;
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("index.aml"), source).unwrap();
        let config = StaticServerConfig::bind_plaintext_loopback(
            directory.path().to_path_buf(),
            "127.0.0.1",
            0,
        )
        .await
        .unwrap();
        let address = config.local_addr().unwrap();
        let mut server = StaticServer::new(config);
        let shutdown = server.shutdown_handle();
        let task = tokio::spawn(async move { server.run().await });

        let mut client = RawClient::connect(address).await;
        client
            .send(RawFrame {
                msg_type: MessageType::Hello,
                flags: 0,
                body: b"HELLO/0.2\n".to_vec(),
            })
            .await;
        assert_eq!(client.receive().await.msg_type, MessageType::Welcome);
        client
            .send(RawFrame {
                msg_type: MessageType::Get,
                flags: 0,
                body: b"GET /index.aml\n".to_vec(),
            })
            .await;

        let response = client.receive().await;
        assert_eq!(response.msg_type, MessageType::Page);
        assert_eq!(String::from_utf8(response.body).unwrap(), source);

        drop(client);
        shutdown.send(true).unwrap();
        task.await.unwrap().unwrap();
    }
}
