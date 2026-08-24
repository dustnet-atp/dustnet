use std::time::Duration;

use arrayvec::ArrayVec;

use crate::protocol::frame::{MessageType, RawFrame};
use crate::protocol::message::{
    ErrorMessage, GetMessage, HelloMessage, InputMessage, PageMessage, RedirectMessage,
    SubscribeMessage, SubscribeMode, UpdateMessage,
};
use crate::protocol::origin::{Origin, TransportSecurity};
use crate::protocol::uri::AtpUri;
use crate::protocol::{
    MAX_CONTROL_MESSAGE_SIZE, MAX_LIVE_UPDATE_SIZE, MAX_WASM_MODULE_SIZE, NegotiatedCapabilities,
    PROTOCOL_VERSION, ProtocolError, ProtocolVersion, SUPPORTED_CAPABILITIES,
};
use crate::resource::{BudgetLease, ResourceCategory, ResourceGovernor};
use crate::session::SessionStore;
use crate::session_store::GovernedSessionStore;
use crate::transport::AtpConnection;
use crate::viewer::{OperationOwner, PageScope, SubscriptionRegionKey};

fn decode_control_body(body: &[u8]) -> Result<&str, ClientError> {
    std::str::from_utf8(body).map_err(|_| {
        ClientError::Protocol(ProtocolError::InvalidMessage(
            "control message body is not valid UTF-8".into(),
        ))
    })
}

/// Maximum number of cached resources before LRU eviction.
const MAX_RESOURCE_CACHE: usize = 32;

/// Maximum aggregate body bytes retained in the resource cache.
const MAX_RESOURCE_CACHE_BYTES: usize = 8 * 1024 * 1024;

/// Maximum time for a connection, handshake, request write, or response.
const NETWORK_TIMEOUT: Duration = Duration::from_secs(10);

/// UPDATEs that arrive while waiting for a request response are retained for
/// the render loop. Bounding the queue prevents a pushing server from keeping
/// a request pending while growing client memory without limit.
const MAX_PENDING_UPDATES: usize = 64;

/// Maximum aggregate serialized bytes retained for pending UPDATEs.
const MAX_PENDING_UPDATE_BYTES: usize = 8 * 1024 * 1024;

/// A page currently has few live regions; this leaves headroom while bounding
/// reconnect replay and server-side subscription state.
pub(crate) const MAX_ACTIVE_SUBSCRIPTIONS: usize = 16;

#[derive(Debug)]
struct ActiveSubscription {
    endpoint: String,
    region: String,
    mode: SubscribeMode,
    owner: OperationOwner,
    projection: Option<SubscriptionRegionKey>,
    _lease: BudgetLease,
}

#[derive(Debug)]
struct ActiveSubscriptions {
    entries: [Option<ActiveSubscription>; MAX_ACTIVE_SUBSCRIPTIONS],
}

#[derive(Debug)]
struct PendingUpdate {
    update: UpdateMessage,
    owner: OperationOwner,
    projection: Option<SubscriptionRegionKey>,
    wire_bytes: usize,
    _lease: BudgetLease,
}

impl PendingUpdate {
    fn try_new(
        governor: &ResourceGovernor,
        update: UpdateMessage,
        owner: &OperationOwner,
        projection: Option<SubscriptionRegionKey>,
    ) -> Result<Self, ClientError> {
        let wire_bytes = update
            .region
            .len()
            .checked_add(update.content.len())
            .ok_or(ClientError::TooManyPendingUpdates)?;
        let update_bytes = AtpClient::update_retained_bytes(&update);
        let initial_bytes = update_bytes
            .checked_add(owner.scope.origin.host_capacity())
            .ok_or(ClientError::TooManyPendingUpdates)?;
        let mut lease = governor
            .reserve(ResourceCategory::PendingUpdates, initial_bytes)
            .map_err(|_| ClientError::TooManyPendingUpdates)?;
        if !pending_update_allocation_allowed(PendingUpdateAllocationSite::Owner) {
            return Err(ClientError::Protocol(ProtocolError::ResourceExhausted {
                requested: owner.scope.origin.host_capacity(),
            }));
        }
        let retained_origin = owner.scope.origin.try_clone().map_err(|_| {
            ClientError::Protocol(ProtocolError::ResourceExhausted {
                requested: owner.scope.origin.host_capacity(),
            })
        })?;
        let retained_bytes = update_bytes
            .checked_add(retained_origin.host_capacity())
            .ok_or(ClientError::TooManyPendingUpdates)?;
        if retained_bytes > lease.amount() {
            lease
                .try_grow(retained_bytes)
                .map_err(|_| ClientError::TooManyPendingUpdates)?;
        } else {
            lease.shrink_to(retained_bytes);
        }
        Ok(Self {
            update,
            owner: OperationOwner::new(
                PageScope {
                    origin: retained_origin,
                    generation: owner.scope.generation,
                },
                owner.request_id,
            ),
            projection,
            wire_bytes,
            _lease: lease,
        })
    }

    fn into_scoped(self) -> ScopedUpdate {
        ScopedUpdate {
            scope: self.owner.scope,
            request_id: self.owner.request_id,
            update: self.update,
            projection: self.projection,
            _lease: self._lease,
        }
    }
}

#[derive(Debug)]
struct PendingUpdates {
    entries: [Option<PendingUpdate>; MAX_PENDING_UPDATES],
    head: usize,
    len: usize,
    bytes: usize,
}

struct PreparedConnectionOrigin {
    target: Origin,
    retained_target: Option<Origin>,
}

impl PendingUpdates {
    fn new() -> Self {
        Self {
            entries: std::array::from_fn(|_| None),
            head: 0,
            len: 0,
            bytes: 0,
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.len
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[cfg(test)]
    fn bytes(&self) -> usize {
        self.bytes
    }

    fn can_push(&self, wire_bytes: usize) -> bool {
        self.len < MAX_PENDING_UPDATES
            && self
                .bytes
                .checked_add(wire_bytes)
                .is_some_and(|bytes| bytes <= MAX_PENDING_UPDATE_BYTES)
    }

    fn push_back(&mut self, pending: PendingUpdate) -> Result<(), ()> {
        if !self.can_push(pending.wire_bytes) {
            return Err(());
        }
        let index = (self.head + self.len) % MAX_PENDING_UPDATES;
        // Obtain the slot before touching the counters, so a slot that is
        // somehow absent leaves the queue's accounting untouched rather than
        // charging bytes for an entry that was never stored.
        let Some(slot) = self.entries.get_mut(index) else {
            return Err(());
        };
        debug_assert!(slot.is_none());
        let wire_bytes = pending.wire_bytes;
        *slot = Some(pending);
        self.bytes += wire_bytes;
        self.len += 1;
        Ok(())
    }

    fn pop_front(&mut self) -> Option<PendingUpdate> {
        if self.len == 0 {
            return None;
        }
        // A live slot holds its owner; an empty one means the length and the
        // ring disagree, and reporting an empty queue is the safe direction.
        let pending = self.entries.get_mut(self.head)?.take()?;
        self.head = (self.head + 1) % MAX_PENDING_UPDATES;
        self.len -= 1;
        self.bytes = self.bytes.saturating_sub(pending.wire_bytes);
        if self.len == 0 {
            self.head = 0;
        }
        Some(pending)
    }

    fn retire_scope(&mut self, scope: &PageScope) {
        let original_len = self.len;
        for _ in 0..original_len {
            let Some(pending) = self.pop_front() else {
                break;
            };
            if pending.owner.scope != *scope {
                // Re-pushing what was just popped cannot exceed a bound the
                // entry already satisfied.
                let pushed = self.push_back(pending);
                debug_assert!(pushed.is_ok(), "retention cannot grow the queue");
            }
        }
    }

    fn clear(&mut self) {
        self.entries.iter_mut().for_each(|entry| *entry = None);
        self.head = 0;
        self.len = 0;
        self.bytes = 0;
    }

    #[cfg(test)]
    fn get(&self, logical_index: usize) -> Option<&PendingUpdate> {
        if logical_index >= self.len {
            return None;
        }
        self.entries[(self.head + logical_index) % MAX_PENDING_UPDATES].as_ref()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingUpdateAllocationSite {
    Owner,
}

#[cfg(test)]
thread_local! {
    static REJECT_PENDING_UPDATE_ALLOCATION: std::cell::Cell<Option<PendingUpdateAllocationSite>> = const { std::cell::Cell::new(None) };
}

fn pending_update_allocation_allowed(site: PendingUpdateAllocationSite) -> bool {
    #[cfg(test)]
    {
        REJECT_PENDING_UPDATE_ALLOCATION.with(|rejected| rejected.get() != Some(site))
    }
    #[cfg(not(test))]
    {
        let _ = site;
        true
    }
}

impl ActiveSubscriptions {
    fn new() -> Self {
        Self {
            entries: std::array::from_fn(|_| None),
        }
    }

    fn iter(&self) -> impl Iterator<Item = &ActiveSubscription> {
        self.entries.iter().filter_map(Option::as_ref)
    }

    fn len(&self) -> usize {
        self.iter().count()
    }

    fn clear_and_release(&mut self) {
        self.entries.iter_mut().for_each(|entry| *entry = None);
    }

    fn insert(&mut self, entry: ActiveSubscription) {
        if let Some(slot) = self.entries.iter_mut().find(|slot| slot.is_none()) {
            *slot = Some(entry);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SubscriptionAllocationSite {
    Endpoint,
    Region,
    Body,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CurrentOwnerAllocationSite {
    CanonicalOrigin,
    Origin,
    Scope,
}

/// Client-owned retained storage a test can force to behave as refused.
///
/// These two are admitted before the structure that will hold them exists, so
/// the property under test is that the *previous* contents survive the
/// refusal. A governor exhausted enough to refuse them also refuses whatever
/// else the same category holds, which is why the site is named instead.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ClientStorageAllocationSite {
    /// A resource cache key's origin and path capacity.
    CacheKey,
    /// The session-store candidate cloned before a directive is applied.
    SessionCandidate,
}

#[cfg(test)]
thread_local! {
    static REJECT_CLIENT_STORAGE: std::cell::Cell<Option<ClientStorageAllocationSite>> =
        const { std::cell::Cell::new(None) };
}

/// Arms one client-storage allocation site to refuse, and disarms it on drop.
#[cfg(test)]
pub(crate) struct ClientStorageRejectionGuard;

#[cfg(test)]
impl ClientStorageRejectionGuard {
    pub(crate) fn at(site: ClientStorageAllocationSite) -> Self {
        REJECT_CLIENT_STORAGE.with(|rejected| rejected.set(Some(site)));
        Self
    }
}

#[cfg(test)]
impl Drop for ClientStorageRejectionGuard {
    fn drop(&mut self) {
        REJECT_CLIENT_STORAGE.with(|rejected| rejected.set(None));
    }
}

#[cfg(test)]
pub(crate) fn reject_client_storage(site: ClientStorageAllocationSite) -> bool {
    REJECT_CLIENT_STORAGE.with(|rejected| rejected.get() == Some(site))
}

/// Compiled away in release builds.
#[cfg(not(test))]
pub(crate) fn reject_client_storage(_site: ClientStorageAllocationSite) -> bool {
    false
}

#[cfg(test)]
thread_local! {
    static REJECT_SUBSCRIPTION_ALLOCATION: std::cell::Cell<Option<SubscriptionAllocationSite>> = const { std::cell::Cell::new(None) };
    static REJECT_CURRENT_OWNER_ALLOCATION: std::cell::Cell<Option<CurrentOwnerAllocationSite>> = const { std::cell::Cell::new(None) };
    static CANONICAL_ORIGIN_ATTEMPTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn subscription_allocation_allowed(site: SubscriptionAllocationSite) -> bool {
    #[cfg(test)]
    {
        REJECT_SUBSCRIPTION_ALLOCATION.with(|rejected| rejected.get() != Some(site))
    }
    #[cfg(not(test))]
    {
        let _ = site;
        true
    }
}

fn current_owner_allocation_allowed(site: CurrentOwnerAllocationSite) -> bool {
    #[cfg(test)]
    {
        REJECT_CURRENT_OWNER_ALLOCATION.with(|rejected| {
            if rejected.get() == Some(site) {
                rejected.set(None);
                false
            } else {
                true
            }
        })
    }
    #[cfg(not(test))]
    {
        let _ = site;
        true
    }
}

fn current_owner_error(requested: usize) -> ClientError {
    ClientError::Protocol(ProtocolError::ResourceExhausted { requested })
}

fn try_clone_current_origin(origin: &Origin) -> Result<Origin, ClientError> {
    if !current_owner_allocation_allowed(CurrentOwnerAllocationSite::Origin) {
        return Err(current_owner_error(origin.host_capacity()));
    }
    origin
        .try_clone()
        .map_err(|_| current_owner_error(origin.host_capacity()))
}

fn try_clone_current_scope(scope: &PageScope) -> Result<PageScope, ClientError> {
    if !current_owner_allocation_allowed(CurrentOwnerAllocationSite::Scope) {
        return Err(current_owner_error(scope.origin.host_capacity()));
    }
    scope
        .try_clone()
        .map_err(|_| current_owner_error(scope.origin.host_capacity()))
}

fn try_owned_subscription_string(
    value: &str,
    site: SubscriptionAllocationSite,
) -> Result<String, ClientError> {
    if !subscription_allocation_allowed(site) {
        return Err(ClientError::Protocol(ProtocolError::ResourceExhausted {
            requested: value.len(),
        }));
    }
    let mut owned = String::new();
    owned.try_reserve_exact(value.len()).map_err(|_| {
        ClientError::Protocol(ProtocolError::ResourceExhausted {
            requested: value.len(),
        })
    })?;
    owned.push_str(value);
    Ok(owned)
}

fn try_owned_client_string(value: &str) -> Result<String, ClientError> {
    let mut owned = String::new();
    owned.try_reserve_exact(value.len()).map_err(|_| {
        ClientError::Protocol(ProtocolError::ResourceExhausted {
            requested: value.len(),
        })
    })?;
    owned.push_str(value);
    Ok(owned)
}

fn subscription_retained_upper_bound(owner: &OperationOwner) -> usize {
    MAX_CONTROL_MESSAGE_SIZE.saturating_add(owner.scope.origin.host_capacity())
}

/// Result of fetching a page.
#[derive(Debug)]
pub(crate) struct FetchResult {
    /// Page generation that owns this completion.
    pub scope: PageScope,
    /// Identity of the request within that page generation.
    pub request_id: u64,
    /// The AML content of the page.
    pub aml_content: String,
    /// The final URI after following redirects.
    pub final_uri: AtpUri,
}

/// Where a PAGE says it is, or the URI it was fetched from.
///
/// A PAGE may name its own path, because the answer to a submission is often a
/// different page from the one submitted to: a login is answered with the front
/// page, and without this the client would keep `/login` as its location and
/// put the person back on the form the moment they reloaded.
///
/// Resolved against the URI that produced it, so it can only ever relabel the
/// location within the same origin — moving between sites is what `REDIRECT` is
/// for, and that path performs a fresh HELLO and counts against the redirect
/// limit. A `Path` that does not resolve is *ignored*, not fatal: the page
/// itself is intact, and refusing to display it because its label was malformed
/// would be a worse answer than showing it where it was fetched from.
fn page_location(base: &AtpUri, page: &PageMessage) -> Option<AtpUri> {
    base.resolve(page.path.as_deref()?).ok()
}

/// A redirect completion tagged with the operation that received it.
///
/// Redirects are deliberately returned to the viewer instead of being
/// followed by the owned APIs. The reducer can then issue the next page scope
/// and request ID, keeping navigation ownership in one place.
#[derive(Debug)]
pub(crate) struct ScopedRedirect {
    pub scope: PageScope,
    pub request_id: u64,
    pub target: AtpUri,
}

/// One response to a viewer-owned page request.
#[derive(Debug)]
pub(crate) enum NavigationResponse {
    Page(FetchResult),
    Redirect(ScopedRedirect),
}

/// A resource completion tagged with the page and request that initiated it.
#[derive(Debug)]
pub(crate) struct ScopedResource {
    pub scope: PageScope,
    pub request_id: u64,
    resource: SharedResource,
}

impl ScopedResource {
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.resource
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        owner: &OperationOwner,
        bytes: &[u8],
        governor: &ResourceGovernor,
    ) -> Self {
        let mut owned = Vec::new();
        owned.try_reserve_exact(bytes.len()).unwrap();
        owned.extend_from_slice(bytes);
        Self {
            scope: owner.scope.try_clone().unwrap(),
            request_id: owner.request_id,
            resource: SharedResource::try_new(owned, governor)
                .expect("test resource must fit the governor"),
        }
    }
}

/// A live update completion tagged with its owning page and request.
#[derive(Debug)]
pub(crate) struct ScopedUpdate {
    pub scope: PageScope,
    pub request_id: u64,
    pub update: UpdateMessage,
    pub(crate) projection: Option<SubscriptionRegionKey>,
    _lease: BudgetLease,
}

#[cfg(test)]
impl ScopedUpdate {
    pub(crate) fn for_test(
        owner: &OperationOwner,
        update: UpdateMessage,
        projection: SubscriptionRegionKey,
        governor: &ResourceGovernor,
    ) -> Self {
        let retained = owner
            .scope
            .origin
            .host_capacity()
            .saturating_add(update.region.capacity())
            .saturating_add(update.content.capacity());
        Self {
            scope: owner.scope.try_clone().unwrap(),
            request_id: owner.request_id,
            update,
            projection: Some(projection),
            _lease: governor
                .reserve(ResourceCategory::PendingUpdates, retained)
                .unwrap(),
        }
    }
}

impl std::ops::Deref for ScopedUpdate {
    type Target = UpdateMessage;

    fn deref(&self) -> &Self::Target {
        &self.update
    }
}

impl std::ops::Deref for ScopedResource {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.resource
    }
}

/// Client-side error.
#[derive(Debug)]
pub enum ClientError {
    /// Protocol-level error.
    Protocol(ProtocolError),
    /// Server returned an error response.
    ServerError { code: u16, message: String },
    /// Unexpected response from server.
    UnexpectedResponse(MessageType),
    /// A fetched WASM module exceeded the client-side size limit.
    ResourceTooLarge { size: usize, max: usize },
    /// A live UPDATE exceeded its narrower size limit.
    LiveUpdateTooLarge { size: usize, max: usize },
    /// A server pushed too many UPDATEs while withholding a response.
    TooManyPendingUpdates,
    /// The peer presented a certificate other than the pinned one. Never a
    /// prompt: by this point the only explanations are a re-keyed server and
    /// an interception, and nothing here can tell them apart.
    CertificateMismatch(String),
    /// No authority vouches for this peer and nothing is pinned for it. The
    /// connection was not established. Carries what a user needs to decide.
    TrustDecisionRequired {
        host: String,
        port: u16,
        fingerprint: crate::trust::Fingerprint,
        reason: String,
    },
    /// The trust store could not be read or written. Fatal rather than
    /// advisory: trust on first use that does not persist the pin is
    /// `--insecure` wearing a better name, and would claim to be pinned in
    /// the status bar while re-trusting whatever it was handed next time.
    Trust(crate::trust::TrustError),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Protocol(e) => write!(f, "{e}"),
            ClientError::ServerError { code, message } => {
                write!(f, "{}: {message}", server_condition(*code))
            }
            ClientError::UnexpectedResponse(mt) => write!(f, "unexpected response: {mt:?}"),
            ClientError::ResourceTooLarge { size, max } => {
                write!(f, "resource too large: {size} bytes (max {max})")
            }
            ClientError::LiveUpdateTooLarge { size, max } => {
                write!(f, "live update too large: {size} bytes (max {max})")
            }
            ClientError::TooManyPendingUpdates => {
                write!(f, "too many live updates pending")
            }
            ClientError::CertificateMismatch(message) => write!(f, "{message}"),
            ClientError::TrustDecisionRequired {
                host,
                port,
                fingerprint,
                reason,
            } => write!(
                f,
                "{host}:{port} presented a certificate that could not be verified ({reason}); \
                 its fingerprint is {fingerprint}. Connect once with --tofu to pin it."
            ),
            ClientError::Trust(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for ClientError {}

/// What an ERROR frame's code means, in words.
///
/// The wire keeps HTTP's numbers -- `docs/spec/02-protocol.md` lists them --
/// because a frame wants a compact discriminant that an existing table already
/// defines. A page wants the opposite: "404" is legible only to a reader who
/// has that table memorised, and ATP is not HTTP, so borrowing its numerals on
/// screen implies a kinship that does not hold. Every code the spec defines is
/// named here, and the name is what a person is shown.
///
/// An unlisted code is reported as-is rather than guessed at from its leading
/// digit. A server that invents a code has said something this client does not
/// understand, and saying so is more honest than rounding it to a category.
fn server_condition(code: u16) -> std::borrow::Cow<'static, str> {
    use std::borrow::Cow;
    match code {
        400 => Cow::Borrowed("the site could not read the request"),
        401 => Cow::Borrowed("this page needs you to sign in"),
        403 => Cow::Borrowed("this page is not yours to read"),
        404 => Cow::Borrowed("the site has no page at this address"),
        429 => Cow::Borrowed("the site is limiting requests"),
        500 => Cow::Borrowed("the site failed while answering"),
        503 => Cow::Borrowed("the site is not answering right now"),
        other => Cow::Owned(format!("the site refused with code {other}")),
    }
}

impl From<ProtocolError> for ClientError {
    fn from(e: ProtocolError) -> Self {
        match e {
            ProtocolError::MessageTooLarge {
                msg_type: MessageType::Resource,
                size,
                max,
            } => ClientError::ResourceTooLarge {
                size: size as usize,
                max,
            },
            ProtocolError::MessageTooLarge {
                msg_type: MessageType::Update,
                size,
                max,
            } => ClientError::LiveUpdateTooLarge {
                size: size as usize,
                max,
            },
            other => ClientError::Protocol(other),
        }
    }
}

/// Cache of fetched binary resources (e.g. .wasm files), keyed by path.
struct ResourceCacheEntry {
    origin: Origin,
    path: String,
    resource: SharedResource,
    _key_lease: BudgetLease,
}

#[derive(Debug)]
struct SharedResourceAllocation {
    bytes: Vec<u8>,
    _lease: BudgetLease,
}

#[derive(Clone, Debug)]
struct SharedResource {
    inner: triomphe::Arc<SharedResourceAllocation>,
}

#[cfg(test)]
thread_local! {
    static REJECT_SHARED_RESOURCE_OWNER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn shared_resource_owner_allocation_allowed() -> bool {
    #[cfg(test)]
    {
        REJECT_SHARED_RESOURCE_OWNER.with(|rejected| !rejected.get())
    }
    #[cfg(not(test))]
    {
        true
    }
}

impl SharedResource {
    fn allocation_byte_cost(data_capacity: usize) -> Option<usize> {
        // `triomphe` is pinned because this mirrors its repr(C)
        // `ArcInner { count: AtomicUsize, data: T }` allocation layout. The
        // response Vec keeps its original buffer, so its capacity is the only
        // separate allocation that must be added to the control block.
        let arc_layout = std::alloc::Layout::new::<std::sync::atomic::AtomicUsize>()
            .extend(std::alloc::Layout::new::<SharedResourceAllocation>())
            .ok()?
            .0
            .pad_to_align()
            .size();
        data_capacity.checked_add(arc_layout)
    }

    #[cfg(test)]
    fn try_new(data: Vec<u8>, governor: &ResourceGovernor) -> Result<Self, ClientError> {
        let byte_cost = Self::allocation_byte_cost(data.capacity()).ok_or_else(|| {
            ClientError::Protocol(ProtocolError::ResourceExhausted {
                requested: data.capacity(),
            })
        })?;
        let lease = governor
            .reserve_with_cost(ResourceCategory::ResourceCache, data.capacity(), byte_cost)
            .map_err(|_| ClientError::ResourceTooLarge {
                size: data.capacity(),
                max: MAX_RESOURCE_CACHE_BYTES,
            })?;
        Self::try_new_with_lease(data, lease, byte_cost)
    }

    fn try_new_with_lease(
        data: Vec<u8>,
        lease: BudgetLease,
        byte_cost: usize,
    ) -> Result<Self, ClientError> {
        if !shared_resource_owner_allocation_allowed() {
            return Err(ClientError::Protocol(ProtocolError::ResourceExhausted {
                requested: byte_cost,
            }));
        }
        triomphe::Arc::try_new(SharedResourceAllocation {
            bytes: data,
            _lease: lease,
        })
        .map(|inner| Self { inner })
        .map_err(|_| {
            ClientError::Protocol(ProtocolError::ResourceExhausted {
                requested: byte_cost,
            })
        })
    }

    fn len(&self) -> usize {
        self.inner.bytes.len()
    }

    #[cfg(test)]
    fn ptr_eq(&self, other: &Self) -> bool {
        triomphe::Arc::ptr_eq(&self.inner, &other.inner)
    }
}

impl std::ops::Deref for SharedResource {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        &self.inner.bytes
    }
}

pub(crate) struct ResourceCache {
    entries: ArrayVec<ResourceCacheEntry, MAX_RESOURCE_CACHE>,
    bytes: usize,
    governor: ResourceGovernor,
}

impl Default for ResourceCache {
    fn default() -> Self {
        Self::new()
    }
}

impl ResourceCache {
    pub fn new() -> Self {
        Self::with_governor(ResourceGovernor::new())
    }

    fn with_governor(governor: ResourceGovernor) -> Self {
        ResourceCache {
            entries: ArrayVec::new(),
            bytes: 0,
            governor,
        }
    }

    fn get(&mut self, origin: &Origin, path: &str) -> Option<SharedResource> {
        let index = self
            .entries
            .iter()
            .position(|entry| &entry.origin == origin && entry.path == path)?;
        let entry = self.entries.remove(index);
        self.entries.push(entry);
        self.entries.last().map(|entry| entry.resource.clone())
    }

    pub fn insert(
        &mut self,
        origin: Origin,
        path: String,
        data: Vec<u8>,
    ) -> Result<(), ClientError> {
        if data.len() > MAX_RESOURCE_CACHE_BYTES {
            return Err(ClientError::ResourceTooLarge {
                size: data.len(),
                max: MAX_RESOURCE_CACHE_BYTES,
            });
        }
        // Cache keys are remotely influenced retained storage too. Charge the
        // exact String capacities before the cache takes ownership; on
        // rejection the existing entry and LRU order remain unchanged.
        let key_bytes = origin.host_capacity().saturating_add(path.capacity());
        if reject_client_storage(ClientStorageAllocationSite::CacheKey) {
            return Err(ClientError::Protocol(ProtocolError::InvalidMessage(
                "resource cache key exceeds the remote collection budget".into(),
            )));
        }
        let key_lease = self
            .governor
            .reserve(ResourceCategory::RemoteCollections, key_bytes)
            .map_err(|_| {
                ClientError::Protocol(ProtocolError::InvalidMessage(
                    "resource cache key exceeds the remote collection budget".into(),
                ))
            })?;
        loop {
            let matching = self
                .entries
                .iter()
                .position(|entry| entry.origin == origin && entry.path == path);
            let old_len = matching
                .and_then(|index| self.entries.get(index))
                .map_or(0, |entry| entry.resource.len());
            let count_after_replace = self
                .entries
                .len()
                .saturating_sub(usize::from(matching.is_some()))
                .saturating_add(1);
            let bytes_after_replace = self
                .bytes
                .saturating_sub(old_len)
                .saturating_add(data.len());
            if count_after_replace <= MAX_RESOURCE_CACHE
                && bytes_after_replace <= MAX_RESOURCE_CACHE_BYTES
            {
                break;
            }
            let Some(index) = self
                .entries
                .iter()
                .enumerate()
                .find_map(|(index, _)| (Some(index) != matching).then_some(index))
            else {
                return Err(ClientError::ResourceTooLarge {
                    size: data.capacity(),
                    max: MAX_RESOURCE_CACHE_BYTES,
                });
            };
            let evicted = self.entries.remove(index);
            self.bytes -= evicted.resource.len();
        }
        let byte_cost = SharedResource::allocation_byte_cost(data.capacity()).ok_or_else(|| {
            ClientError::Protocol(ProtocolError::ResourceExhausted {
                requested: data.capacity(),
            })
        })?;
        let lease = loop {
            match self.governor.reserve_with_cost(
                ResourceCategory::ResourceCache,
                data.capacity(),
                byte_cost,
            ) {
                Ok(lease) => break lease,
                Err(_) => {
                    let matching = self
                        .entries
                        .iter()
                        .position(|entry| entry.origin == origin && entry.path == path);
                    let Some(index) = self
                        .entries
                        .iter()
                        .enumerate()
                        .find_map(|(index, _)| (Some(index) != matching).then_some(index))
                    else {
                        return Err(ClientError::ResourceTooLarge {
                            size: data.capacity(),
                            max: MAX_RESOURCE_CACHE_BYTES,
                        });
                    };
                    let evicted = self.entries.remove(index);
                    self.bytes -= evicted.resource.len();
                }
            }
        };
        let resource = SharedResource::try_new_with_lease(data, lease, byte_cost)?;
        if let Some(index) = self
            .entries
            .iter()
            .position(|entry| entry.origin == origin && entry.path == path)
        {
            let old = self.entries.remove(index);
            self.bytes -= old.resource.len();
        }
        self.bytes += resource.len();
        self.entries.push(ResourceCacheEntry {
            origin,
            path,
            resource,
            _key_lease: key_lease,
        });
        Ok(())
    }

    fn evict_oldest(&mut self) -> bool {
        if self.entries.is_empty() {
            return false;
        }
        let entry = self.entries.remove(0);
        self.bytes = self.bytes.saturating_sub(entry.resource.len());
        true
    }
}

/// How this client authenticates the sites it connects to.
///
/// Replaces the `(no_tls, insecure)` pair of booleans this carried before.
/// Two booleans already encoded three states with one combination meaningless,
/// and pinning makes four; an enum makes the impossible state unrepresentable
/// and forces every site that branches on transport security to say what it
/// does about each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlsMode {
    /// A certification authority must vouch for the host name.
    Verified,
    /// Pin the certificate the first time this host and port are seen, and
    /// require the same one afterwards.
    TrustOnFirstUse,
    /// Do not verify the certificate at all.
    Insecure,
    /// No TLS. Restricted to loopback by [`Origin`].
    PlaintextLoopback,
}

/// The mode plus the material it needs: extra anchors and the pin store.
#[derive(Debug)]
pub struct TlsPolicy {
    mode: TlsMode,
    extra_roots: Vec<rustls_pki_types::CertificateDer<'static>>,
    store: crate::trust::TrustStore,
}

impl TlsPolicy {
    pub fn new(mode: TlsMode, store: crate::trust::TrustStore) -> Self {
        TlsPolicy {
            mode,
            extra_roots: Vec::new(),
            store,
        }
    }

    /// CA verification with no extra anchors and no pin store. The default a
    /// connection gets when nothing else is configured.
    pub fn verified() -> Self {
        TlsPolicy::new(TlsMode::Verified, crate::trust::TrustStore::detached())
    }

    /// Plaintext, which [`Origin`] restricts to loopback. No store: nothing is
    /// authenticated, so there is nothing to pin.
    pub fn plaintext_loopback() -> Self {
        TlsPolicy::new(
            TlsMode::PlaintextLoopback,
            crate::trust::TrustStore::detached(),
        )
    }

    /// Trust the authorities in a PEM file in addition to the built-in bundle.
    pub fn with_ca_file(
        mut self,
        path: &std::path::Path,
    ) -> Result<Self, crate::trust::TrustError> {
        self.extra_roots = crate::trust::load_certificate_authorities(path)?;
        Ok(self)
    }

    pub fn mode(&self) -> TlsMode {
        self.mode
    }

    fn is_plaintext(&self) -> bool {
        self.mode == TlsMode::PlaintextLoopback
    }

    /// Whether a pin governs this host and port.
    ///
    /// A pin is consulted even in [`TlsMode::Verified`], because it records a
    /// decision this user already made. Creating one still requires
    /// `TrustOnFirstUse`, so this cannot downgrade a connection that was never
    /// pinned: it can only honour a pin that someone deliberately made.
    fn pinned(&self, host: &str, port: u16) -> Option<crate::trust::Fingerprint> {
        match self.mode {
            TlsMode::Verified | TlsMode::TrustOnFirstUse => {
                self.store.pin_for(host, port).map(|pin| pin.fingerprint)
            }
            TlsMode::Insecure | TlsMode::PlaintextLoopback => None,
        }
    }

    fn security_for(&self, host: &str, port: u16) -> TransportSecurity {
        match self.mode {
            TlsMode::PlaintextLoopback => TransportSecurity::PlaintextLoopback,
            TlsMode::Insecure => TransportSecurity::InsecureTls,
            TlsMode::TrustOnFirstUse => TransportSecurity::PinnedTls,
            TlsMode::Verified => {
                if self.pinned(host, port).is_some() {
                    TransportSecurity::PinnedTls
                } else {
                    TransportSecurity::VerifiedTls
                }
            }
        }
    }

    fn verification_for(&self, host: &str, port: u16) -> crate::transport::TlsVerification {
        if let Some(expected) = self.pinned(host, port) {
            return crate::transport::TlsVerification::Pinned(expected);
        }
        match self.mode {
            TlsMode::Insecure => crate::transport::TlsVerification::Insecure,
            TlsMode::TrustOnFirstUse => crate::transport::TlsVerification::TrustOnFirstUse,
            TlsMode::Verified | TlsMode::PlaintextLoopback => {
                crate::transport::TlsVerification::Ca {
                    extra_roots: self.extra_roots.clone(),
                }
            }
        }
    }
}

/// ATP client — manages connections and fetches pages.
pub(crate) struct AtpClient {
    conn: Option<AtpConnection>,
    current_origin: Option<Origin>,
    current_scope: Option<PageScope>,
    negotiated_version: Option<ProtocolVersion>,
    negotiated_capabilities: NegotiatedCapabilities,
    policy: TlsPolicy,
    pub(crate) resource_cache: ResourceCache,
    sessions: GovernedSessionStore,
    pub(crate) governor: ResourceGovernor,
    /// Subscriptions the client has open against the current host.
    /// Replayed by `ensure_connected` whenever it opens a fresh
    /// connection so that a server-side idle disconnect (or any other
    /// transparent reconnect) doesn't silently drop live updates.
    /// Cleared on host change and on `unsubscribe`.
    active_subscriptions: ActiveSubscriptions,
    pending_updates: PendingUpdates,
}

impl AtpClient {
    pub(crate) fn new(policy: TlsPolicy) -> Self {
        let governor = ResourceGovernor::new();
        AtpClient {
            conn: None,
            current_origin: None,
            current_scope: None,
            negotiated_version: None,
            negotiated_capabilities: NegotiatedCapabilities::default(),
            policy,
            resource_cache: ResourceCache::with_governor(governor.clone()),
            sessions: GovernedSessionStore::new(governor.clone()),
            governor,
            active_subscriptions: ActiveSubscriptions::new(),
            pending_updates: PendingUpdates::new(),
        }
    }

    /// Let this client's CA-verified sessions outlive the process.
    ///
    /// A builder step rather than a constructor argument so that every caller
    /// that does not ask for it — which is every test — keeps sessions in
    /// memory. Remembering is the default the *user* gets, decided once in
    /// [`crate::compositor`]'s viewer setup from configuration; it is
    /// deliberately not the default a *construction site* gets, so a new one
    /// cannot acquire a credential file by saying nothing.
    ///
    /// Returns how many stored sessions were restored, or the error that
    /// prevented reading them. The error is advisory: the client is usable
    /// either way, having simply started logged out.
    pub(crate) fn remembering_sessions(
        mut self,
        file: crate::session_file::SessionFile,
    ) -> (Self, Result<usize, crate::session_file::SessionFileError>) {
        let (sessions, outcome) =
            GovernedSessionStore::with_persistence(self.governor.clone(), file);
        self.sessions = sessions;
        (self, outcome)
    }

    /// Pin a peer the user has chosen to trust.
    ///
    /// Separate from the trust-on-first-use path in `ensure_connected_to`,
    /// which pins without asking. This is the deliberate answer to a prompt,
    /// and it changes the origin the next navigation computes — a pinned
    /// connection is a different security context from an unverified one, so
    /// the navigation that provoked the question has to be re-issued rather
    /// than resumed.
    pub(crate) fn pin_peer(
        &mut self,
        host: &str,
        port: u16,
        fingerprint: crate::trust::Fingerprint,
    ) -> Result<(), crate::trust::TrustError> {
        self.policy.store.record(host, port, fingerprint)
    }

    /// How the connection currently in hand was authenticated.
    ///
    /// Read from the live origin rather than from the policy, because the two
    /// can differ: the default mode verifies against authorities, but a site
    /// the user has pinned is reached by its pin, and the status bar has to
    /// say which. `None` before anything is connected.
    pub(crate) fn current_security(&self) -> Option<TransportSecurity> {
        self.current_origin.as_ref().map(|origin| origin.security())
    }

    fn transport_security(&self, host: &str, port: u16) -> TransportSecurity {
        self.policy.security_for(host, port)
    }

    pub(crate) fn active_subscription_count(&self) -> usize {
        self.active_subscriptions.len()
    }

    pub(crate) fn has_active_subscription(
        &self,
        projection: SubscriptionRegionKey,
        endpoint: &str,
        region: &str,
        mode: SubscribeMode,
    ) -> bool {
        self.active_subscriptions.iter().any(|entry| {
            entry.projection == Some(projection)
                && entry.endpoint == endpoint
                && entry.region == region
                && entry.mode == mode
        })
    }

    fn origin_for(&self, uri: &AtpUri) -> Result<Origin, ClientError> {
        #[cfg(test)]
        CANONICAL_ORIGIN_ATTEMPTS.with(|attempts| attempts.set(attempts.get() + 1));
        if !current_owner_allocation_allowed(CurrentOwnerAllocationSite::CanonicalOrigin) {
            return Err(current_owner_error(uri.host().len()));
        }
        Ok(Origin::try_from_uri(
            uri,
            self.transport_security(uri.host(), uri.port()),
        )?)
    }

    fn prepare_connection_origin(
        &self,
        uri: &AtpUri,
    ) -> Result<PreparedConnectionOrigin, ClientError> {
        let target = self.origin_for(uri)?;
        let retained_target = if self.current_origin.as_ref() == Some(&target) {
            None
        } else {
            Some(try_clone_current_origin(&target)?)
        };
        Ok(PreparedConnectionOrigin {
            target,
            retained_target,
        })
    }

    /// Resolve the exact security-partitioned origin a viewer must use when
    /// issuing an operation owner for `uri`.
    pub(crate) fn request_origin(&self, uri: &AtpUri) -> Result<Origin, ClientError> {
        self.origin_for(uri)
    }

    pub(crate) fn current_scope(&self) -> Option<&PageScope> {
        self.current_scope.as_ref()
    }

    pub(crate) fn evict_oldest_resource(&mut self) -> bool {
        self.resource_cache.evict_oldest()
    }

    /// Re-establish transport for the current page without changing its
    /// generation. `ensure_connected` replays only subscriptions whose full
    /// owner scope still matches this page.
    pub(crate) async fn reconnect_current_page(&mut self, uri: &AtpUri) -> Result<(), ClientError> {
        let mut prepared = self.prepare_connection_origin(uri)?;
        if self.current_scope.as_ref().map(|scope| &scope.origin) != Some(&prepared.target) {
            return Err(ClientError::Protocol(ProtocolError::InvalidMessage(
                "reconnect target does not own the current page".into(),
            )));
        }
        self.close_connection().await;
        self.ensure_connected_to(uri, &mut prepared).await
    }

    /// Activate a page scope issued by the viewer and retire all work owned by
    /// the preceding page. This does not allocate or rewrite the generation.
    pub(crate) async fn activate_page_scope(&mut self, scope: PageScope) {
        if self.current_scope.as_ref() == Some(&scope) {
            return;
        }
        if self
            .current_scope
            .as_ref()
            .is_some_and(|current| current.origin != scope.origin)
        {
            self.retire_origin().await;
        } else {
            if self.active_subscriptions.len() != 0 && self.negotiated_capabilities.live_updates() {
                let _ = self.unsubscribe().await;
            }
            self.active_subscriptions.clear_and_release();
            self.pending_updates.clear();
        }
        self.current_scope = Some(scope);
    }

    /// Retire subscriptions and queued updates owned by exactly `scope`.
    ///
    /// The transport remains available for a subsequent same-origin page.
    /// A stale retirement effect must not disturb work belonging to the
    /// currently active generation.
    pub(crate) async fn retire_page_work(&mut self, scope: &PageScope) {
        let retires_current = self.current_scope.as_ref() == Some(scope);
        let has_server_subscriptions = self
            .active_subscriptions
            .iter()
            .any(|entry| &entry.owner.scope == scope);

        if retires_current && has_server_subscriptions {
            // UNSUBSCRIBE is all-or-nothing on the wire. Clear local ownership
            // even if the best-effort notification cannot be delivered.
            let _ = self.unsubscribe().await;
        } else {
            self.active_subscriptions
                .entries
                .iter_mut()
                .for_each(|entry| {
                    if entry
                        .as_ref()
                        .is_some_and(|entry| entry.owner.scope == *scope)
                    {
                        *entry = None;
                    }
                });
            self.retire_queued_updates(scope);
        }

        if retires_current {
            self.current_scope = None;
        }
    }

    fn retire_queued_updates(&mut self, scope: &PageScope) {
        self.pending_updates.retire_scope(scope);
    }

    /// Find a session token matching the given URI.
    fn session_for_origin(&self, origin: &Origin, path: &str) -> Option<String> {
        // Never send session tokens over plaintext connections
        if self.policy.is_plaintext() || !self.negotiated_capabilities.sessions() {
            return None;
        }
        self.sessions
            .find_token(origin, path)
            .map(|t| t.token.clone())
    }

    fn session_for_current_path(&self, path: &str) -> Option<String> {
        if self.policy.is_plaintext() || !self.negotiated_capabilities.sessions() {
            return None;
        }
        let origin = self.current_origin.as_ref()?;
        self.sessions
            .find_token(origin, path)
            .map(|token| token.token.clone())
    }

    /// Process session directives from a PAGE response.
    fn apply_session_directives(&mut self, origin: &Origin, page: &PageMessage) {
        if !self.negotiated_capabilities.sessions() {
            return;
        }
        for directive in &page.session_directives {
            self.apply_session_directive(origin, directive);
        }
    }

    fn apply_session_directive(
        &mut self,
        origin: &Origin,
        directive: &crate::session::SessionDirective,
    ) -> bool {
        self.sessions.apply_directive(origin, directive).is_ok()
    }

    pub(crate) fn sessions(&self) -> &SessionStore {
        self.sessions.as_store()
    }

    pub(crate) fn clear_sessions_for_storage_key(&mut self, storage_key: &str) {
        self.sessions.clear_storage_key(storage_key);
    }

    pub(crate) fn clear_all_sessions(&mut self) {
        self.sessions.clear_all();
    }

    /// Whether sessions are being remembered across launches, for `:sessions`.
    pub(crate) fn sessions_are_persistent(&self) -> bool {
        self.sessions.is_persistent()
    }

    /// The most recent session-store write failure, cleared by reading it.
    pub(crate) fn take_session_persistence_error(
        &mut self,
    ) -> Option<crate::session_file::SessionFileError> {
        self.sessions.take_persistence_error()
    }

    /// Fetch exactly one page response using an identity issued by the viewer.
    ///
    /// This API never creates a scope or request ID and never follows a
    /// redirect. A redirect is returned with the supplied owner so the reducer
    /// can transition and issue the owner for the follow-up request.
    pub(crate) async fn fetch(
        &mut self,
        owner: OperationOwner,
        uri: &AtpUri,
    ) -> Result<NavigationResponse, ClientError> {
        let mut prepared = self.prepare_navigation(&owner, uri).await?;
        match self.fetch_once(&owner, uri, &mut prepared).await {
            Err(ClientError::Protocol(ProtocolError::ConnectionClosed))
            | Err(ClientError::Protocol(ProtocolError::Io(_))) => {
                self.close_connection().await;
                self.fetch_once(&owner, uri, &mut prepared).await
            }
            result => result,
        }
    }

    async fn prepare_navigation(
        &mut self,
        owner: &OperationOwner,
        uri: &AtpUri,
    ) -> Result<PreparedConnectionOrigin, ClientError> {
        let prepared = self.prepare_connection_origin(uri)?;
        if owner.scope.origin != prepared.target {
            return Err(ClientError::Protocol(ProtocolError::InvalidMessage(
                "operation owner origin does not match request URI".into(),
            )));
        }
        let retained_scope = try_clone_current_scope(&owner.scope)?;
        self.activate_page_scope(retained_scope).await;
        Ok(prepared)
    }

    async fn fetch_once(
        &mut self,
        owner: &OperationOwner,
        uri: &AtpUri,
        prepared: &mut PreparedConnectionOrigin,
    ) -> Result<NavigationResponse, ClientError> {
        self.ensure_connected_to(uri, prepared).await?;

        let (path, query) = uri
            .try_path_query()
            .map_err(|_| current_owner_error(uri.path().len()))?;
        let get = GetMessage {
            path,
            query,
            referrer: None,
            session: self.session_for_origin(&prepared.target, uri.path()),
        };
        let frame = RawFrame {
            msg_type: MessageType::Get,
            flags: 0,
            body: get.serialize()?.into_bytes(),
        };
        self.send_frame(&frame).await?;

        // UPDATEs may be interleaved on the same connection. Queue them and
        // keep waiting for this request's response.
        let response = self.recv_response().await?;

        match response.msg_type {
            MessageType::Page => {
                let page = PageMessage::decode_body(&response.body, response.flags)?;
                let scope = owner
                    .scope
                    .try_clone()
                    .map_err(|_| current_owner_error(owner.scope.origin.host_capacity()))?;
                let final_uri = match page_location(uri, &page) {
                    Some(named) => named,
                    None => uri
                        .try_clone()
                        .map_err(|_| current_owner_error(uri.path().len()))?,
                };
                self.apply_session_directives(&prepared.target, &page);
                Ok(NavigationResponse::Page(FetchResult {
                    scope,
                    request_id: owner.request_id,
                    aml_content: page.content,
                    final_uri,
                }))
            }
            MessageType::Redirect => {
                let body = decode_control_body(&response.body)?;
                let redirect = RedirectMessage::parse(body)?;
                let scope = owner
                    .scope
                    .try_clone()
                    .map_err(|_| current_owner_error(owner.scope.origin.host_capacity()))?;
                Ok(NavigationResponse::Redirect(ScopedRedirect {
                    scope,
                    request_id: owner.request_id,
                    target: AtpUri::parse(&redirect.target)?,
                }))
            }
            MessageType::Error => {
                let body = decode_control_body(&response.body)?;
                let err = ErrorMessage::parse(body)?;
                Err(ClientError::ServerError {
                    code: err.code,
                    message: err.message.unwrap_or_default(),
                })
            }
            other => Err(ClientError::UnexpectedResponse(other)),
        }
    }

    /// Ensure we have an active connection to the URI's host:port.
    /// Reuses existing connection if same host:port.
    ///
    /// On a fresh connection (server-idle reconnect, host change, or
    /// first contact), replays any `active_subs` after the handshake so
    /// that subscriptions the caller registered earlier remain live
    /// across transparent reconnects. On host change, `active_subs` is
    /// cleared first — old subs belonged to the old origin.
    #[cfg(test)]
    async fn ensure_connected(&mut self, uri: &AtpUri) -> Result<(), ClientError> {
        let mut prepared = self.prepare_connection_origin(uri)?;
        self.ensure_connected_to(uri, &mut prepared).await
    }

    async fn ensure_connected_to(
        &mut self,
        uri: &AtpUri,
        prepared: &mut PreparedConnectionOrigin,
    ) -> Result<(), ClientError> {
        let target = &prepared.target;
        if let Some(ref current) = self.current_origin {
            if current == target && self.conn.is_some() {
                return Ok(());
            }
            if current != target {
                self.retire_origin().await;
            }
        }

        // Need new connection
        self.close_connection().await;

        let verification = self.policy.verification_for(uri.host(), uri.port());
        let already_pinned = self.policy.pinned(uri.host(), uri.port()).is_some();
        let connect = async {
            if self.policy.is_plaintext() {
                AtpConnection::connect_plain(uri.host(), uri.port())
                    .await
                    .map(|conn| (conn, None))
                    .map_err(crate::transport::TlsConnectError::from)
            } else {
                AtpConnection::connect_tls(uri.host(), uri.port(), &verification).await
            }
        };
        let (conn, observed) = match tokio::time::timeout(NETWORK_TIMEOUT, connect)
            .await
            .map_err(|_| ClientError::Protocol(ProtocolError::Timeout))?
        {
            Ok(established) => established,
            Err(crate::transport::TlsConnectError::Protocol(error)) => {
                return Err(ClientError::Protocol(error));
            }
            Err(crate::transport::TlsConnectError::PinMismatch(message)) => {
                return Err(ClientError::CertificateMismatch(message));
            }
            // No authority vouches for this peer and nothing is pinned for it.
            // Reported rather than refused outright so a caller with a terminal
            // can put the decision to the user; a caller without one sees an
            // error naming the fingerprint and the reason, and connects to
            // nothing in the meantime.
            Err(crate::transport::TlsConnectError::Unverified(peer)) => {
                return Err(ClientError::TrustDecisionRequired {
                    host: uri.host().to_owned(),
                    port: uri.port(),
                    fingerprint: peer.fingerprint,
                    reason: peer.reason,
                });
            }
        };

        // Trust on first use, and only on first use: a pin is recorded when
        // one did not already exist. When it did, the verifier has already
        // required a match, so there is nothing to learn and nothing to
        // overwrite — a mismatch never reaches here.
        if self.policy.mode == TlsMode::TrustOnFirstUse
            && !already_pinned
            && let Some(fingerprint) = observed
        {
            self.policy
                .store
                .record(uri.host(), uri.port(), fingerprint)
                .map_err(ClientError::Trust)?;
        }

        self.conn = Some(conn);
        if let Some(retained_target) = prepared.retained_target.take() {
            self.current_origin = Some(retained_target);
        }
        // Perform HELLO/WELCOME handshake
        self.do_handshake().await?;

        // Replay any subscriptions registered before this reconnect.
        // We don't go through `subscribe()` here because that would
        // double-append to the retained slots; send each frame directly.
        // Indexed rather than iterated: the body sends frames, which borrows
        // `self` mutably, so the immutable borrow of the table cannot be held
        // across it. `get` keeps the access checked.
        for index in 0..MAX_ACTIVE_SUBSCRIPTIONS {
            let Some((endpoint, region, mode)) = self
                .active_subscriptions
                .entries
                .get(index)
                .and_then(Option::as_ref)
                .filter(|entry| {
                    self.current_scope.as_ref() == Some(&entry.owner.scope)
                        && entry.owner.scope.origin == *target
                })
                .map(|entry| (entry.endpoint.clone(), entry.region.clone(), entry.mode))
            else {
                continue;
            };
            let session = self.session_for_current_path(&endpoint);
            let body = SubscribeMessage::try_serialize_parts(
                &endpoint,
                &region,
                mode,
                session.as_deref(),
            )?;
            let frame = RawFrame {
                msg_type: MessageType::Subscribe,
                flags: 0,
                body: body.into_bytes(),
            };
            self.send_frame(&frame).await?;
        }

        Ok(())
    }

    async fn do_handshake(&mut self) -> Result<(), ClientError> {
        let hello = HelloMessage {
            protocol_version: PROTOCOL_VERSION.to_string(),
            terminal_size: None,
            color_support: None,
            client: Some(format!("Dustnet/{}", env!("CARGO_PKG_VERSION"))),
            capabilities: SUPPORTED_CAPABILITIES
                .iter()
                .map(|capability| (*capability).to_string())
                .collect(),
        };
        let frame = RawFrame {
            msg_type: MessageType::Hello,
            flags: 0,
            body: hello.serialize()?.into_bytes(),
        };
        self.send_frame(&frame).await?;

        let response = self.recv_response().await?;
        if response.msg_type != MessageType::Welcome {
            return Err(ClientError::UnexpectedResponse(response.msg_type));
        }

        let negotiated = self
            .conn
            .as_ref()
            .and_then(AtpConnection::negotiated)
            .ok_or_else(|| {
                ClientError::Protocol(ProtocolError::InvalidMessage(
                    "WELCOME did not establish negotiated protocol state".into(),
                ))
            })?;
        if negotiated.version != ProtocolVersion::parse(PROTOCOL_VERSION)? {
            return Err(ClientError::Protocol(ProtocolError::invalid_message(
                format_args!(
                    "unsupported server protocol version: {}",
                    negotiated.version
                ),
            )));
        }
        self.negotiated_capabilities = negotiated.capabilities;
        self.negotiated_version = Some(negotiated.version);
        Ok(())
    }

    /// Submit one form request with viewer-issued ownership.
    pub(crate) async fn submit(
        &mut self,
        owner: OperationOwner,
        uri: &AtpUri,
        path: &str,
        form_data: &str,
    ) -> Result<NavigationResponse, ClientError> {
        let submit_uri = uri.resolve(path)?;
        let mut prepared = self.prepare_navigation(&owner, &submit_uri).await?;
        self.ensure_connected_to(&submit_uri, &mut prepared).await?;

        // Build a URI for the submission path to look up session tokens

        let mut input_path = String::new();
        input_path
            .try_reserve_exact(path.len())
            .map_err(|_| current_owner_error(path.len()))?;
        input_path.push_str(path);
        let mut owned_form_data = String::new();
        owned_form_data
            .try_reserve_exact(form_data.len())
            .map_err(|_| current_owner_error(form_data.len()))?;
        owned_form_data.push_str(form_data);
        let input = InputMessage {
            path: input_path,
            form_data: owned_form_data,
            session: self.session_for_origin(&prepared.target, submit_uri.path()),
        };
        let frame = RawFrame {
            msg_type: MessageType::Input,
            flags: 0,
            body: input.serialize()?.into_bytes(),
        };
        self.send_frame(&frame).await?;

        let response = self.recv_response().await?;
        match response.msg_type {
            MessageType::Page => {
                let page = PageMessage::decode_body(&response.body, response.flags)?;
                self.apply_session_directives(&prepared.target, &page);
                let final_uri = page_location(&submit_uri, &page).unwrap_or(submit_uri);
                Ok(NavigationResponse::Page(FetchResult {
                    scope: owner.scope,
                    request_id: owner.request_id,
                    aml_content: page.content,
                    final_uri,
                }))
            }
            MessageType::Redirect => {
                let body = decode_control_body(&response.body)?;
                let redirect = RedirectMessage::parse(body)?;
                Ok(NavigationResponse::Redirect(ScopedRedirect {
                    scope: owner.scope,
                    request_id: owner.request_id,
                    target: AtpUri::parse(&redirect.target)?,
                }))
            }
            MessageType::Error => {
                let body = decode_control_body(&response.body)?;
                let err = ErrorMessage::parse(body)?;
                Err(ClientError::ServerError {
                    code: err.code,
                    message: err.message.unwrap_or_default(),
                })
            }
            other => Err(ClientError::UnexpectedResponse(other)),
        }
    }

    pub(crate) async fn subscribe_for_projection(
        &mut self,
        owner: OperationOwner,
        projection: SubscriptionRegionKey,
        endpoint: &str,
        region: &str,
        mode: SubscribeMode,
    ) -> Result<(), ClientError> {
        self.subscribe_with_projection(owner, Some(projection), endpoint, region, mode)
            .await
    }

    async fn subscribe_with_projection(
        &mut self,
        owner: OperationOwner,
        projection: Option<SubscriptionRegionKey>,
        endpoint: &str,
        region: &str,
        mode: SubscribeMode,
    ) -> Result<(), ClientError> {
        if !self.negotiated_capabilities.live_updates() {
            return Err(ClientError::Protocol(ProtocolError::InvalidMessage(
                "live-updates was not negotiated".into(),
            )));
        }
        self.validate_current_owner(&owner)?;
        if let Some(active) = self
            .active_subscriptions
            .iter()
            .find(|active| active.region == region)
        {
            if active.owner == owner
                && active.projection == projection
                && active.endpoint == endpoint
                && active.mode == mode
            {
                return Ok(());
            }
            return Err(ClientError::Protocol(ProtocolError::invalid_message(
                format_args!("live region {region:?} already has different subscription metadata"),
            )));
        }
        if self.active_subscriptions.len() >= MAX_ACTIVE_SUBSCRIPTIONS {
            return Err(ClientError::Protocol(ProtocolError::invalid_message(
                format_args!("maximum active subscriptions exceeded ({MAX_ACTIVE_SUBSCRIPTIONS})"),
            )));
        }
        let session = self.session_for_current_path(endpoint);
        if !subscription_allocation_allowed(SubscriptionAllocationSite::Body) {
            return Err(ClientError::Protocol(ProtocolError::ResourceExhausted {
                requested: MAX_CONTROL_MESSAGE_SIZE,
            }));
        }
        let body =
            SubscribeMessage::try_serialize_parts(endpoint, region, mode, session.as_deref())?;
        let upper_bound = subscription_retained_upper_bound(&owner);
        let mut lease = self
            .governor
            .reserve(ResourceCategory::RemoteCollections, upper_bound)
            .map_err(|_| {
                ClientError::Protocol(ProtocolError::ResourceExhausted {
                    requested: upper_bound,
                })
            })?;
        let retained_endpoint =
            try_owned_subscription_string(endpoint, SubscriptionAllocationSite::Endpoint)?;
        let retained_region =
            try_owned_subscription_string(region, SubscriptionAllocationSite::Region)?;
        let retained_bytes = retained_endpoint
            .capacity()
            .checked_add(retained_region.capacity())
            .and_then(|bytes| bytes.checked_add(owner.scope.origin.host_capacity()))
            .ok_or({
                ClientError::Protocol(ProtocolError::ResourceExhausted {
                    requested: usize::MAX,
                })
            })?;
        lease.shrink_to(retained_bytes);
        let frame = RawFrame {
            msg_type: MessageType::Subscribe,
            flags: 0,
            body: body.into_bytes(),
        };
        self.send_frame(&frame).await?;
        self.active_subscriptions.insert(ActiveSubscription {
            endpoint: retained_endpoint,
            region: retained_region,
            mode,
            owner,
            projection,
            _lease: lease,
        });
        Ok(())
    }

    /// Send an UNSUBSCRIBE to cancel all active subscriptions.
    pub(crate) async fn unsubscribe(&mut self) -> Result<(), ClientError> {
        let result = if let Some(conn) = self.conn.as_mut() {
            let frame = RawFrame {
                msg_type: MessageType::Unsubscribe,
                flags: 0,
                body: Vec::new(),
            };
            tokio::time::timeout(NETWORK_TIMEOUT, conn.send_frame(&frame))
                .await
                .map_err(|_| ClientError::Protocol(ProtocolError::Timeout))?
                .map_err(ClientError::Protocol)
        } else {
            Ok(())
        };
        self.active_subscriptions.clear_and_release();
        self.pending_updates.clear();
        result
    }

    /// Send a liveness PING, if a connection is open.
    ///
    /// The server resets its idle deadline on any inbound frame, so this is
    /// what lets a reading-but-not-clicking viewer hold its connection instead
    /// of being dropped and having to redial TLS. A closed connection is not an
    /// error: there is nothing to keep alive, and the next navigation redials.
    pub(crate) async fn send_ping(&mut self) -> Result<(), ClientError> {
        let Some(conn) = self.conn.as_mut() else {
            return Ok(());
        };
        let frame = RawFrame {
            msg_type: MessageType::Ping,
            flags: 0,
            body: Vec::new(),
        };
        tokio::time::timeout(NETWORK_TIMEOUT, conn.send_frame(&frame))
            .await
            .map_err(|_| ClientError::Protocol(ProtocolError::Timeout))?
            .map_err(ClientError::Protocol)
    }

    /// Fetch a binary resource using an identity issued by the viewer model.
    pub(crate) async fn fetch_resource(
        &mut self,
        owner: OperationOwner,
        uri: &AtpUri,
        path: &str,
    ) -> Result<ScopedResource, ClientError> {
        let mut prepared = self.prepare_connection_origin(uri)?;
        if owner.scope.origin != prepared.target {
            return Err(ClientError::Protocol(ProtocolError::InvalidMessage(
                "resource owner origin does not match the request URI".into(),
            )));
        }
        self.validate_current_owner(&owner)?;
        // Complete every retained origin copy before transport mutation.
        let cache_origin = prepared.target.try_clone().map_err(|_| {
            ClientError::Protocol(ProtocolError::ResourceExhausted {
                requested: prepared.target.host_capacity(),
            })
        })?;
        self.ensure_connected_to(uri, &mut prepared).await?;
        if !self.negotiated_capabilities.wasm_effects() {
            return Err(ClientError::Protocol(ProtocolError::InvalidMessage(
                "wasm-effects was not negotiated".into(),
            )));
        }
        // Check cache only after capability negotiation, preserving the rule
        // that cached WASM is unusable unless this connection permits it.
        if let Some(resource) = self.resource_cache.get(&prepared.target, path) {
            return Ok(ScopedResource {
                scope: owner.scope,
                request_id: owner.request_id,
                resource,
            });
        }

        // Resource paths never carry query strings or session tokens.
        let get = GetMessage {
            path: try_owned_client_string(path)?,
            query: None,
            referrer: None,
            session: None,
        };
        let frame = RawFrame {
            msg_type: MessageType::Get,
            flags: 0,
            body: get.serialize()?.into_bytes(),
        };
        self.send_frame(&frame).await?;

        let response = self.recv_response().await?;
        match response.msg_type {
            MessageType::Resource => {
                if response.body.len() > MAX_WASM_MODULE_SIZE {
                    return Err(ClientError::ResourceTooLarge {
                        size: response.body.len(),
                        max: MAX_WASM_MODULE_SIZE,
                    });
                }
                let cached_bytes = response.body.len();
                self.resource_cache
                    .insert(cache_origin, get.path, response.body)?;
                // The insert above succeeded, so the entry is present unless
                // eviction raced it. Report exhaustion rather than aborting:
                // both the body and the eviction pressure are remote-driven.
                let Some(resource) = self.resource_cache.get(&prepared.target, path) else {
                    return Err(ClientError::Protocol(ProtocolError::ResourceExhausted {
                        requested: cached_bytes,
                    }));
                };
                Ok(ScopedResource {
                    scope: owner.scope,
                    request_id: owner.request_id,
                    resource,
                })
            }
            MessageType::Error => {
                let body = decode_control_body(&response.body)?;
                let err = ErrorMessage::parse(body)?;
                Err(ClientError::ServerError {
                    code: err.code,
                    message: err.message.unwrap_or_default(),
                })
            }
            other => Err(ClientError::UnexpectedResponse(other)),
        }
    }

    /// Poll for an UPDATE frame with a short timeout. Returns None if no update available.
    pub(crate) async fn poll_update(
        &mut self,
        timeout: std::time::Duration,
    ) -> Result<Option<ScopedUpdate>, ClientError> {
        while let Some(pending) = self.pending_updates.pop_front() {
            if self.current_scope.as_ref() == Some(&pending.owner.scope) {
                return Ok(Some(pending.into_scoped()));
            }
        }
        if self.conn.is_none() {
            return Ok(None);
        }
        let Some(conn) = self.conn.as_mut() else {
            return Ok(None);
        };

        match tokio::time::timeout(timeout, conn.recv_frame()).await {
            Ok(Ok(frame)) => match frame.msg_type {
                MessageType::Update => {
                    let update = Self::decode_update(frame)?;
                    let subscription = self.owner_for_update(&update)?;
                    let pending = PendingUpdate::try_new(
                        &self.governor,
                        update,
                        &subscription.owner,
                        subscription.projection,
                    )?;
                    Ok(Some(pending.into_scoped()))
                }
                MessageType::ServerBye => {
                    self.conn = None;
                    self.pending_updates.clear();
                    Ok(None)
                }
                // The keepalive round-trip completed. There is no update to
                // report, but the connection is proven live.
                MessageType::Pong => Ok(None),
                MessageType::Error => {
                    let body = decode_control_body(&frame.body)?;
                    let err = ErrorMessage::parse(body)?;
                    Err(ClientError::ServerError {
                        code: err.code,
                        message: err.message.unwrap_or_default(),
                    })
                }
                _ => Ok(None),
            },
            Ok(Err(ProtocolError::ConnectionClosed)) => {
                self.conn = None;
                self.pending_updates.clear();
                Ok(None)
            }
            Ok(Err(e)) => Err(ClientError::Protocol(e)),
            Err(_elapsed) => Ok(None), // timeout — no update available
        }
    }

    /// Gracefully disconnect from the current server.
    pub(crate) async fn disconnect(&mut self) {
        self.retire_origin().await;
    }

    /// Close only the transport. The origin and its subscriptions survive so
    /// a transparent same-origin reconnect can replay them safely.
    async fn close_connection(&mut self) {
        if let Some(ref mut conn) = self.conn {
            let _ = tokio::time::timeout(std::time::Duration::from_millis(500), async {
                let bye = RawFrame {
                    msg_type: MessageType::Bye,
                    flags: 0,
                    body: Vec::new(),
                };
                let _ = conn.send_frame(&bye).await;
                let _ = conn.recv_frame().await;
                let _ = conn.shutdown().await;
            })
            .await;
        }
        self.conn = None;
        self.negotiated_version = None;
        self.negotiated_capabilities = NegotiatedCapabilities::default();
        self.pending_updates.clear();
    }

    /// Retire all state owned by the active page/origin.
    async fn retire_origin(&mut self) {
        self.close_connection().await;
        self.current_origin = None;
        self.current_scope = None;
        self.active_subscriptions.clear_and_release();
        self.pending_updates.clear();
    }

    async fn send_frame(&mut self, frame: &RawFrame) -> Result<(), ClientError> {
        let conn = self
            .conn
            .as_mut()
            .ok_or(ClientError::Protocol(ProtocolError::ConnectionClosed))?;
        tokio::time::timeout(NETWORK_TIMEOUT, conn.send_frame(frame))
            .await
            .map_err(|_| ClientError::Protocol(ProtocolError::Timeout))??;
        Ok(())
    }

    async fn recv_response(&mut self) -> Result<RawFrame, ClientError> {
        tokio::time::timeout(NETWORK_TIMEOUT, async {
            loop {
                let conn = self
                    .conn
                    .as_mut()
                    .ok_or(ClientError::Protocol(ProtocolError::ConnectionClosed))?;
                let frame = conn.recv_frame().await?;
                match frame.msg_type {
                    MessageType::Update => {
                        if !self.pending_updates.can_push(0) {
                            return Err(ClientError::TooManyPendingUpdates);
                        }
                        let update = Self::decode_update(frame)?;
                        let wire_bytes = update
                            .region
                            .len()
                            .checked_add(update.content.len())
                            .ok_or(ClientError::TooManyPendingUpdates)?;
                        if !self.pending_updates.can_push(wire_bytes) {
                            return Err(ClientError::TooManyPendingUpdates);
                        }
                        let subscription = self.owner_for_update(&update)?;
                        let pending = PendingUpdate::try_new(
                            &self.governor,
                            update,
                            &subscription.owner,
                            subscription.projection,
                        )?;
                        self.pending_updates.push_back(pending).map_err(|_| {
                            ClientError::Protocol(ProtocolError::InvalidMessage(
                                "pending UPDATE queue changed after preflight".into(),
                            ))
                        })?;
                    }
                    // A keepalive reply is not the response we are waiting for.
                    // Without this arm the wildcard below would hand a PONG
                    // back to a caller expecting a PAGE.
                    MessageType::Pong => {}
                    MessageType::ServerBye => {
                        return Err(ClientError::Protocol(ProtocolError::ConnectionClosed));
                    }
                    _ => return Ok(frame),
                }
            }
        })
        .await
        .map_err(|_| ClientError::Protocol(ProtocolError::Timeout))?
    }

    fn decode_update(frame: RawFrame) -> Result<UpdateMessage, ClientError> {
        if frame.body.len() > MAX_LIVE_UPDATE_SIZE {
            return Err(ClientError::LiveUpdateTooLarge {
                size: frame.body.len(),
                max: MAX_LIVE_UPDATE_SIZE,
            });
        }
        let body = String::from_utf8(frame.body).map_err(|e| {
            ProtocolError::invalid_message(format_args!("invalid UPDATE UTF-8: {e}"))
        })?;
        Ok(UpdateMessage::parse(&body)?.with_flags(frame.flags))
    }

    fn update_retained_bytes(update: &UpdateMessage) -> usize {
        update
            .region
            .capacity()
            .saturating_add(update.content.capacity())
    }

    fn validate_current_owner(&self, owner: &OperationOwner) -> Result<(), ClientError> {
        if self.current_scope.as_ref() == Some(&owner.scope) {
            return Ok(());
        }
        Err(ClientError::Protocol(ProtocolError::InvalidMessage(
            "operation owner does not match the current page scope".into(),
        )))
    }

    fn owner_for_update(&self, update: &UpdateMessage) -> Result<&ActiveSubscription, ClientError> {
        self.active_subscriptions
            .iter()
            .find(|entry| entry.region == update.region)
            .ok_or_else(|| {
                ClientError::Protocol(ProtocolError::invalid_message(format_args!(
                    "UPDATE targets unsubscribed live region {:?}",
                    update.region
                )))
            })
    }
}

#[cfg(test)]
mod tests {

    /// The whole shape of the trust prompt, minus the prompt itself: an
    /// unverifiable site is reported rather than refused outright, pinning it
    /// moves it into a different security context, and the same fetch then
    /// succeeds.
    ///
    /// The origin changing is the reason the runner re-issues the navigation
    /// instead of retrying it. `prepare_navigation` rejects an owner whose
    /// origin disagrees with the request URI, so a navigation created before
    /// the pin cannot be resumed after it — this test pins that invariant down
    /// alongside the behaviour that depends on it.
    #[tokio::test]
    async fn an_unverifiable_site_is_reported_then_reachable_once_pinned() {
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

        let store_dir = tempfile::tempdir().unwrap();
        let store =
            crate::trust::TrustStore::load_from(&store_dir.path().join("known_sites")).unwrap();
        let mut client = AtpClient::new(TlsPolicy::new(TlsMode::Verified, store));
        let uri = AtpUri::parse(&format!("atp://127.0.0.1:{}/index.aml", address.port())).unwrap();

        // Before any decision the site is an ordinary CA-verified origin, and
        // reaching it fails with something a user could act on.
        let before = client.origin_for(&uri).unwrap();
        assert_eq!(before.security(), TransportSecurity::VerifiedTls);

        let owner = OperationOwner::new(
            PageScope {
                origin: client.origin_for(&uri).unwrap(),
                generation: 1,
            },
            1,
        );
        let fingerprint = match client.fetch(owner, &uri).await {
            Err(ClientError::TrustDecisionRequired {
                host,
                port,
                fingerprint,
                reason,
            }) => {
                assert_eq!(host, "127.0.0.1");
                assert_eq!(port, address.port());
                assert!(!reason.is_empty());
                fingerprint
            }
            Err(other) => panic!("expected a trust decision, got {other}"),
            Ok(_) => panic!("a self-signed site must not be fetched unasked"),
        };

        // Answering the prompt.
        client
            .pin_peer("127.0.0.1", address.port(), fingerprint)
            .unwrap();

        // The site is now a different origin, which is exactly why the old
        // navigation cannot be resumed.
        let after = client.origin_for(&uri).unwrap();
        assert_eq!(after.security(), TransportSecurity::PinnedTls);
        assert_ne!(before, after);
        let stale = OperationOwner::new(
            PageScope {
                origin: before,
                generation: 1,
            },
            2,
        );
        assert!(
            client.fetch(stale, &uri).await.is_err(),
            "an owner carrying the pre-pin origin must not be honoured"
        );

        // A navigation created after the decision reaches the site.
        let fresh = OperationOwner::new(
            PageScope {
                origin: client.origin_for(&uri).unwrap(),
                generation: 2,
            },
            3,
        );
        client
            .fetch(fresh, &uri)
            .await
            .expect("the pinned site is reachable");

        shutdown.send(true).unwrap();
        task.await.unwrap();
    }

    /// The trust decision table, in one place.
    ///
    /// `security_for` is what the client *claims* about a connection — it
    /// becomes part of origin identity and drives the status bar — and
    /// `verification_for` is what it actually checks. A disagreement between
    /// them is the dangerous kind of bug: state partitioned as pinned while
    /// the handshake was verified some other way, or the reverse.
    #[test]
    fn the_trust_decision_table_claims_exactly_what_it_checks() {
        use crate::transport::TlsVerification;
        let fingerprint = crate::trust::Fingerprint::of_certificate(b"pinned");

        let pinned_store = || {
            let mut store = crate::trust::TrustStore::detached();
            store.record("example.com", 1985, fingerprint).unwrap();
            store
        };

        let cases = [
            (
                TlsMode::Verified,
                false,
                TransportSecurity::VerifiedTls,
                "ca",
            ),
            (
                TlsMode::Verified,
                true,
                TransportSecurity::PinnedTls,
                "pinned",
            ),
            (
                TlsMode::TrustOnFirstUse,
                false,
                TransportSecurity::PinnedTls,
                "tofu",
            ),
            // A pin already exists, so first use is over: the connection is
            // held to the pin rather than allowed to learn a new certificate.
            (
                TlsMode::TrustOnFirstUse,
                true,
                TransportSecurity::PinnedTls,
                "pinned",
            ),
            // Explicitly asking for no verification ignores pins entirely,
            // and is partitioned away from them.
            (
                TlsMode::Insecure,
                true,
                TransportSecurity::InsecureTls,
                "insecure",
            ),
            (
                TlsMode::PlaintextLoopback,
                true,
                TransportSecurity::PlaintextLoopback,
                "ca",
            ),
        ];

        for (mode, has_pin, expected_security, expected_check) in cases {
            let store = if has_pin {
                pinned_store()
            } else {
                crate::trust::TrustStore::detached()
            };
            let policy = TlsPolicy::new(mode, store);
            assert_eq!(
                policy.security_for("example.com", 1985),
                expected_security,
                "{mode:?} with pin={has_pin} claims the wrong security level"
            );
            let check = match policy.verification_for("example.com", 1985) {
                TlsVerification::Ca { .. } => "ca",
                TlsVerification::Pinned(seen) => {
                    assert_eq!(seen, fingerprint, "the wrong certificate was required");
                    "pinned"
                }
                TlsVerification::TrustOnFirstUse => "tofu",
                TlsVerification::Insecure => "insecure",
            };
            assert_eq!(
                check, expected_check,
                "{mode:?} with pin={has_pin} checks the wrong thing"
            );
        }
    }

    /// A pin for one host and port says nothing about any other.
    #[test]
    fn a_pin_does_not_reach_past_the_host_and_port_it_was_made_for() {
        let mut store = crate::trust::TrustStore::detached();
        store
            .record(
                "example.com",
                1985,
                crate::trust::Fingerprint::of_certificate(b"x"),
            )
            .unwrap();
        let policy = TlsPolicy::new(TlsMode::Verified, store);

        assert_eq!(
            policy.security_for("example.com", 1985),
            TransportSecurity::PinnedTls
        );
        for (host, port) in [("example.com", 9000), ("other.example", 1985)] {
            assert_eq!(
                policy.security_for(host, port),
                TransportSecurity::VerifiedTls,
                "{host}:{port} inherited a pin it was never given"
            );
        }
    }
    use super::*;
    use dustnet_server::{StaticServer, StaticServerConfig};

    fn test_uri(address: std::net::SocketAddr, path: &str) -> AtpUri {
        AtpUri::parse(&format!("atp://127.0.0.1:{}{path}", address.port())).unwrap()
    }

    fn reducer_owner(client: &AtpClient, uri: &AtpUri) -> OperationOwner {
        let origin = client.request_origin(uri).unwrap();
        let mut viewer = crate::viewer::ViewerModel::new(80, 24);
        viewer
            .reduce(crate::viewer::ViewerEvent::InitialNavigation {
                uri: uri.try_clone().unwrap(),
                origin,
            })
            .into_iter()
            .find_map(|effect| match effect {
                crate::viewer::ViewerEffect::Fetch { owner, .. } => Some(owner),
                _ => None,
            })
            .expect("initial navigation must issue reducer-owned fetch work")
    }

    async fn fetch_page_owned(
        client: &mut AtpClient,
        uri: &AtpUri,
    ) -> Result<FetchResult, ClientError> {
        let owner = reducer_owner(client, uri);
        match client.fetch(owner, uri).await? {
            NavigationResponse::Page(page) => Ok(page),
            NavigationResponse::Redirect(_) => panic!("test expected PAGE, not REDIRECT"),
        }
    }

    #[tokio::test]
    async fn current_scope_preparation_rejection_precedes_transport_mutation() {
        let mut client = AtpClient::new(TlsPolicy::plaintext_loopback());
        let old_uri = AtpUri::parse("atp://127.0.0.1/old").unwrap();
        let old_scope = PageScope {
            origin: client.origin_for(&old_uri).unwrap(),
            generation: 4,
        };
        let old_host_ptr = old_scope.origin.host().as_ptr();
        client.current_scope = Some(old_scope);
        let owner = OperationOwner::new(
            PageScope {
                origin: client.origin_for(&old_uri).unwrap(),
                generation: 10,
            },
            14,
        );
        REJECT_CURRENT_OWNER_ALLOCATION
            .with(|rejected| rejected.set(Some(CurrentOwnerAllocationSite::Scope)));
        assert!(matches!(
            client.prepare_navigation(&owner, &old_uri).await,
            Err(ClientError::Protocol(
                ProtocolError::ResourceExhausted { .. }
            ))
        ));
        let retained = client.current_scope.as_ref().unwrap();
        assert_eq!(retained.generation, 4);
        assert_eq!(retained.origin.host().as_ptr(), old_host_ptr);
    }

    #[tokio::test]
    async fn current_origin_preparation_rejection_preserves_existing_client_state() {
        let mut client = AtpClient::new(TlsPolicy::plaintext_loopback());
        let old_uri = AtpUri::parse("atp://localhost/old").unwrap();
        let target_uri = AtpUri::parse("atp://127.0.0.1:9/new").unwrap();
        let old_origin = client.origin_for(&old_uri).unwrap();
        let old_host_ptr = old_origin.host().as_ptr();
        client.current_origin = Some(old_origin.try_clone().unwrap());
        let current_origin_ptr = client.current_origin.as_ref().unwrap().host().as_ptr();
        client.current_scope = Some(PageScope {
            origin: old_origin,
            generation: 5,
        });

        REJECT_CURRENT_OWNER_ALLOCATION
            .with(|rejected| rejected.set(Some(CurrentOwnerAllocationSite::Origin)));
        assert!(matches!(
            client.ensure_connected(&target_uri).await,
            Err(ClientError::Protocol(
                ProtocolError::ResourceExhausted { .. }
            ))
        ));
        assert!(client.conn.is_none());
        assert_eq!(client.current_origin.as_ref().unwrap().host(), "localhost");
        assert_eq!(
            client.current_origin.as_ref().unwrap().host().as_ptr(),
            current_origin_ptr
        );
        assert_eq!(client.current_scope.as_ref().unwrap().generation, 5);
        assert_eq!(
            client
                .current_scope
                .as_ref()
                .unwrap()
                .origin
                .host()
                .as_ptr(),
            old_host_ptr
        );
    }

    #[tokio::test]
    async fn canonical_origin_rejection_precedes_connection_and_owner_mutation() {
        let mut client = AtpClient::new(TlsPolicy::plaintext_loopback());
        let old_uri = AtpUri::parse("atp://localhost/old").unwrap();
        let target_uri = AtpUri::parse("atp://127.0.0.1:9/new").unwrap();
        let old_origin = client.origin_for(&old_uri).unwrap();
        client.current_origin = Some(old_origin.try_clone().unwrap());
        client.current_scope = Some(PageScope {
            origin: old_origin,
            generation: 7,
        });
        let origin_ptr = client.current_origin.as_ref().unwrap().host().as_ptr();
        let scope_ptr = client
            .current_scope
            .as_ref()
            .unwrap()
            .origin
            .host()
            .as_ptr();

        REJECT_CURRENT_OWNER_ALLOCATION.with(|rejected| {
            rejected.set(Some(CurrentOwnerAllocationSite::CanonicalOrigin));
        });
        assert!(matches!(
            client.ensure_connected(&target_uri).await,
            Err(ClientError::Protocol(
                ProtocolError::ResourceExhausted { .. }
            )),
        ));
        assert!(client.conn.is_none());
        assert_eq!(
            client.current_origin.as_ref().unwrap().host().as_ptr(),
            origin_ptr,
        );
        assert_eq!(
            client
                .current_scope
                .as_ref()
                .unwrap()
                .origin
                .host()
                .as_ptr(),
            scope_ptr,
        );
        assert_eq!(client.current_scope.as_ref().unwrap().generation, 7);
    }

    #[tokio::test]
    async fn same_origin_reconnect_reuses_exact_retained_owners() {
        let (_dir, addr) = setup_test_server().await;
        let uri = AtpUri::parse(&format!("atp://127.0.0.1:{}/hello", addr.port())).unwrap();
        let mut client = AtpClient::new(TlsPolicy::plaintext_loopback());
        fetch_page_owned(&mut client, &uri).await.unwrap();
        let origin_ptr = client.current_origin.as_ref().unwrap().host().as_ptr();
        let scope_ptr = client
            .current_scope
            .as_ref()
            .unwrap()
            .origin
            .host()
            .as_ptr();
        let scope_generation = client.current_scope.as_ref().unwrap().generation;

        CANONICAL_ORIGIN_ATTEMPTS.with(|attempts| attempts.set(0));
        client.reconnect_current_page(&uri).await.unwrap();

        assert!(client.conn.is_some());
        assert_eq!(
            client.current_origin.as_ref().unwrap().host().as_ptr(),
            origin_ptr
        );
        let scope = client.current_scope.as_ref().unwrap();
        assert_eq!(scope.origin.host().as_ptr(), scope_ptr);
        assert_eq!(scope.generation, scope_generation);
        CANONICAL_ORIGIN_ATTEMPTS.with(|attempts| assert_eq!(attempts.get(), 1));
    }

    fn install_test_subscription(
        client: &mut AtpClient,
        owner: OperationOwner,
        endpoint: &str,
        region: &str,
    ) {
        let endpoint = endpoint.to_owned();
        let region = region.to_owned();
        let retained = endpoint.capacity() + region.capacity() + owner.scope.origin.host_capacity();
        let lease = client
            .governor
            .reserve(ResourceCategory::RemoteCollections, retained)
            .unwrap();
        client.active_subscriptions.insert(ActiveSubscription {
            endpoint,
            region,
            mode: SubscribeMode::Replace,
            owner,
            projection: None,
            _lease: lease,
        });
    }

    fn install_test_pending_update(
        client: &mut AtpClient,
        owner: OperationOwner,
        update: UpdateMessage,
    ) {
        let pending = PendingUpdate::try_new(&client.governor, update, &owner, None).unwrap();
        client
            .pending_updates
            .push_back(pending)
            .unwrap_or_else(|_| panic!("test pending UPDATE must fit"));
    }

    #[test]
    fn pending_update_queue_is_fixed_atomic_and_releases_exactly() {
        let mut client = AtpClient::new(TlsPolicy::plaintext_loopback());
        let uri = AtpUri::parse("atp://127.0.0.1:4040/current").unwrap();
        let scope = PageScope {
            origin: client.request_origin(&uri).unwrap(),
            generation: 21,
        };
        let baseline_used = client.governor.used(ResourceCategory::PendingUpdates);
        let baseline_count = client.governor.count(ResourceCategory::PendingUpdates);
        let mut expected_wire_bytes = 0usize;
        let mut expected_retained_bytes = 0usize;

        for index in 0..MAX_PENDING_UPDATES {
            let update = UpdateMessage {
                region: format!("region-{index}"),
                content: format!("tick-{index}"),
                flags: Default::default(),
            };
            expected_wire_bytes += update.region.len() + update.content.len();
            let pending = PendingUpdate::try_new(
                &client.governor,
                update,
                &OperationOwner::new(scope.clone(), index as u64),
                None,
            )
            .unwrap();
            expected_retained_bytes += pending._lease.amount();
            client.pending_updates.push_back(pending).unwrap();
        }

        assert_eq!(client.pending_updates.len(), MAX_PENDING_UPDATES);
        assert_eq!(client.pending_updates.bytes(), expected_wire_bytes);
        assert!(!client.pending_updates.can_push(0));
        assert_eq!(
            client.governor.used(ResourceCategory::PendingUpdates),
            baseline_used + expected_retained_bytes
        );
        assert_eq!(
            client.governor.count(ResourceCategory::PendingUpdates),
            baseline_count + MAX_PENDING_UPDATES
        );

        let first = client.pending_updates.pop_front().unwrap();
        assert_eq!(first.update.region, "region-0");
        drop(first);
        let replacement = PendingUpdate::try_new(
            &client.governor,
            UpdateMessage {
                region: "replacement".into(),
                content: "last".into(),
                flags: Default::default(),
            },
            &OperationOwner::new(scope, 99),
            None,
        )
        .unwrap();
        client.pending_updates.push_back(replacement).unwrap();
        assert_eq!(
            client
                .pending_updates
                .get(MAX_PENDING_UPDATES - 1)
                .unwrap()
                .update
                .region,
            "replacement"
        );

        client.pending_updates.clear();
        assert!(client.pending_updates.is_empty());
        assert_eq!(client.pending_updates.bytes(), 0);
        assert_eq!(
            client.governor.used(ResourceCategory::PendingUpdates),
            baseline_used
        );
        assert_eq!(
            client.governor.count(ResourceCategory::PendingUpdates),
            baseline_count
        );
    }

    #[test]
    fn pending_update_owner_allocation_rejection_is_atomic() {
        let client = AtpClient::new(TlsPolicy::plaintext_loopback());
        let uri = AtpUri::parse("atp://127.0.0.1:4040/current").unwrap();
        let owner = OperationOwner::new(
            PageScope {
                origin: client.request_origin(&uri).unwrap(),
                generation: 22,
            },
            7,
        );
        let baseline_used = client.governor.used(ResourceCategory::PendingUpdates);
        let baseline_count = client.governor.count(ResourceCategory::PendingUpdates);
        REJECT_PENDING_UPDATE_ALLOCATION
            .with(|rejected| rejected.set(Some(PendingUpdateAllocationSite::Owner)));
        let result = PendingUpdate::try_new(
            &client.governor,
            UpdateMessage {
                region: "clock".into(),
                content: "tick".into(),
                flags: Default::default(),
            },
            &owner,
            None,
        );
        REJECT_PENDING_UPDATE_ALLOCATION.with(|rejected| rejected.set(None));

        assert!(matches!(
            result,
            Err(ClientError::Protocol(
                ProtocolError::ResourceExhausted { .. }
            ))
        ));
        assert!(client.pending_updates.is_empty());
        assert_eq!(
            client.governor.used(ResourceCategory::PendingUpdates),
            baseline_used
        );
        assert_eq!(
            client.governor.count(ResourceCategory::PendingUpdates),
            baseline_count
        );
    }
    use std::io::Write;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    struct AtpServer(StaticServer);

    impl AtpServer {
        fn new(config: StaticServerConfig) -> Self {
            Self(StaticServer::new(config))
        }

        async fn run(&mut self) -> Result<(), ProtocolError> {
            self.0.run().await
        }
    }

    struct RawPeer(TcpStream);

    impl RawPeer {
        async fn accept(listener: &TcpListener) -> Self {
            Self(listener.accept().await.unwrap().0)
        }

        async fn recv_frame(&mut self) -> Result<RawFrame, ProtocolError> {
            let mut header = [0_u8; crate::protocol::HEADER_SIZE];
            self.0.read_exact(&mut header).await?;
            let (length, msg_type, flags) = crate::protocol::frame::decode_header(&header)?;
            let mut body = crate::protocol::frame::allocate_frame_body(length as usize)?;
            self.0.read_exact(&mut body).await?;
            Ok(RawFrame {
                msg_type,
                flags,
                body,
            })
        }

        async fn send_frame(&mut self, frame: &RawFrame) -> Result<(), ProtocolError> {
            self.0
                .write_all(&crate::protocol::frame::encode_frame(frame)?)
                .await?;
            Ok(())
        }
    }

    #[test]
    fn subscriptions_use_the_session_matching_their_endpoint() {
        let mut client = AtpClient::new(TlsPolicy::verified());
        let uri = AtpUri::parse("atp://example.com/").unwrap();
        let origin = client.origin_for(&uri).unwrap();
        client.current_origin = Some(origin.clone());
        client.negotiated_capabilities =
            NegotiatedCapabilities::negotiate(&["sessions".to_string()], &["sessions".to_string()]);
        assert!(client.apply_session_directive(
            &origin,
            &crate::session::SessionDirective::Set {
                token: "root".into(),
                scope: "/".into(),
                expires: None,
            },
        ));
        assert!(client.apply_session_directive(
            &origin,
            &crate::session::SessionDirective::Set {
                token: "private".into(),
                scope: "/private".into(),
                expires: None,
            },
        ));
        assert_eq!(
            client.governor.used(ResourceCategory::Sessions),
            client.sessions.retained_capacity_bytes()
        );
        assert_eq!(
            client.session_for_current_path("/private/feed").as_deref(),
            Some("private")
        );
        assert_eq!(
            client.session_for_current_path("/public").as_deref(),
            Some("root")
        );
        client.current_origin =
            Some(Origin::from_uri(&uri, TransportSecurity::InsecureTls).unwrap());
        assert!(client.session_for_current_path("/private/feed").is_none());
        client.policy = TlsPolicy::plaintext_loopback();
        assert!(client.session_for_current_path("/private/feed").is_none());
    }

    #[tokio::test]
    async fn same_origin_transport_loss_preserves_only_replayable_state() {
        let mut client = AtpClient::new(TlsPolicy::plaintext_loopback());
        let uri = AtpUri::parse("atp://127.0.0.1/").unwrap();
        client.current_origin = Some(client.origin_for(&uri).unwrap());
        let scope = PageScope {
            origin: client.current_origin.clone().unwrap(),
            generation: 1,
        };
        install_test_subscription(
            &mut client,
            OperationOwner::new(scope.clone(), 1),
            "/ticker",
            "clock",
        );
        install_test_pending_update(
            &mut client,
            OperationOwner::new(scope, 1),
            UpdateMessage {
                region: "clock".into(),
                content: "stale".into(),
                flags: Default::default(),
            },
        );

        client.close_connection().await;

        assert!(client.current_origin.is_some());
        assert_eq!(client.active_subscriptions.len(), 1);
        assert!(client.pending_updates.is_empty());
        assert_eq!(client.pending_updates.bytes(), 0);
    }

    #[tokio::test]
    async fn retiring_an_origin_discards_subscriptions_and_updates() {
        let mut client = AtpClient::new(TlsPolicy::plaintext_loopback());
        let uri = AtpUri::parse("atp://127.0.0.1/").unwrap();
        client.current_origin = Some(client.origin_for(&uri).unwrap());
        let scope = PageScope {
            origin: client.current_origin.clone().unwrap(),
            generation: 1,
        };
        install_test_subscription(
            &mut client,
            OperationOwner::new(scope.clone(), 1),
            "/ticker",
            "clock",
        );
        install_test_pending_update(
            &mut client,
            OperationOwner::new(scope, 1),
            UpdateMessage {
                region: "clock".into(),
                content: "stale".into(),
                flags: Default::default(),
            },
        );

        client.retire_origin().await;

        assert!(client.current_origin.is_none());
        assert_eq!(client.active_subscriptions.len(), 0);
        assert!(client.pending_updates.is_empty());
    }

    async fn setup_test_server() -> (tempfile::TempDir, std::net::SocketAddr) {
        let dir = tempfile::tempdir().unwrap();

        // Create test pages
        let mut f = std::fs::File::create(dir.path().join("hello.aml")).unwrap();
        write!(
            f,
            r#"[page mode=document title="Hello"]
[text]Hello World[/text]
[link href="atp://127.0.0.1/other"]Other page[/link]
[/page]"#
        )
        .unwrap();

        let mut f2 = std::fs::File::create(dir.path().join("other.aml")).unwrap();
        write!(
            f2,
            r#"[page mode=document title="Other"]
[text]Other page content[/text]
[/page]"#
        )
        .unwrap();

        let root = dir.path().to_path_buf();
        let config = StaticServerConfig::bind_plaintext_loopback(root, "127.0.0.1", 0)
            .await
            .unwrap();
        let addr = config.local_addr().unwrap();
        tokio::spawn(async move {
            let mut server = AtpServer::new(config);
            let _ = server.run().await;
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        (dir, addr)
    }

    #[tokio::test]
    async fn fetch_page() {
        let (_dir, addr) = setup_test_server().await;

        let mut client = AtpClient::new(TlsPolicy::plaintext_loopback());
        let uri = test_uri(addr, "/hello");

        let result = fetch_page_owned(&mut client, &uri).await.unwrap();
        assert!(result.aml_content.contains("Hello World"));
        assert_eq!(result.final_uri.path(), "/hello");
    }

    #[tokio::test]
    async fn owned_fetch_preserves_viewer_scope_and_request_id() {
        let (_dir, addr) = setup_test_server().await;
        let uri = AtpUri::parse(&format!("atp://127.0.0.1:{}/hello", addr.port())).unwrap();
        let mut client = AtpClient::new(TlsPolicy::plaintext_loopback());
        let owner = OperationOwner::new(
            PageScope {
                origin: client.origin_for(&uri).unwrap(),
                generation: 91,
            },
            407,
        );
        CANONICAL_ORIGIN_ATTEMPTS.with(|attempts| attempts.set(0));

        let response = client.fetch(owner.clone(), &uri).await.unwrap();
        let NavigationResponse::Page(page) = response else {
            panic!("expected PAGE response");
        };
        assert_eq!(page.scope, owner.scope);
        assert_eq!(page.request_id, owner.request_id);
        assert_eq!(client.current_scope(), Some(&owner.scope));
        CANONICAL_ORIGIN_ATTEMPTS.with(|attempts| assert_eq!(attempts.get(), 1));
    }

    #[tokio::test]
    async fn owned_submit_preserves_viewer_scope_and_request_id() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut peer = RawPeer::accept(&listener).await;
            assert_eq!(
                peer.recv_frame().await.unwrap().msg_type,
                MessageType::Hello
            );
            peer.send_frame(&RawFrame {
                msg_type: MessageType::Welcome,
                flags: 0,
                body: b"WELCOME/0.2\nServer: form-test\n".to_vec(),
            })
            .await
            .unwrap();
            assert_eq!(
                peer.recv_frame().await.unwrap().msg_type,
                MessageType::Input
            );
            peer.send_frame(&RawFrame {
                msg_type: MessageType::Page,
                flags: 0,
                body: b"[page][text]submitted[/text][/page]".to_vec(),
            })
            .await
            .unwrap();
        });

        let uri = AtpUri::parse(&format!("atp://127.0.0.1:{}/form", address.port())).unwrap();
        let mut client = AtpClient::new(TlsPolicy::plaintext_loopback());
        let owner = OperationOwner::new(
            PageScope {
                origin: client.origin_for(&uri).unwrap(),
                generation: 92,
            },
            408,
        );
        CANONICAL_ORIGIN_ATTEMPTS.with(|attempts| attempts.set(0));
        let response = client
            .submit(owner.clone(), &uri, "/submit", "name=dustnet")
            .await
            .unwrap();
        let NavigationResponse::Page(page) = response else {
            panic!("expected PAGE response");
        };
        assert_eq!(page.scope, owner.scope);
        assert_eq!(page.request_id, owner.request_id);
        assert_eq!(page.final_uri.path(), "/submit");
        CANONICAL_ORIGIN_ATTEMPTS.with(|attempts| assert_eq!(attempts.get(), 1));
        server.await.unwrap();
    }

    /// A submission answered with a page that names itself lands the client on
    /// that page. This is the whole point of the field: without it the location
    /// stays at the form's action and a reload resubmits.
    #[tokio::test]
    async fn a_named_page_relabels_where_a_submission_landed() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut peer = RawPeer::accept(&listener).await;
            assert_eq!(
                peer.recv_frame().await.unwrap().msg_type,
                MessageType::Hello
            );
            peer.send_frame(&RawFrame {
                msg_type: MessageType::Welcome,
                flags: 0,
                body: b"WELCOME/0.2\nCapabilities: page-path\n".to_vec(),
            })
            .await
            .unwrap();
            assert_eq!(
                peer.recv_frame().await.unwrap().msg_type,
                MessageType::Input
            );
            peer.send_frame(&RawFrame {
                msg_type: MessageType::Page,
                flags: 0x04,
                body: b"Path: /index?item=12\n\n[page][text]posted[/text][/page]".to_vec(),
            })
            .await
            .unwrap();
        });

        let uri = AtpUri::parse(&format!("atp://127.0.0.1:{}/login", address.port())).unwrap();
        let mut client = AtpClient::new(TlsPolicy::plaintext_loopback());
        let owner = OperationOwner::new(
            PageScope {
                origin: client.origin_for(&uri).unwrap(),
                generation: 93,
            },
            409,
        );
        let response = client
            .submit(owner, &uri, "/login", "name=dusty")
            .await
            .unwrap();
        let NavigationResponse::Page(page) = response else {
            panic!("expected PAGE response");
        };
        assert_eq!(page.final_uri.path(), "/index");
        assert_eq!(page.final_uri.query(), Some("item=12"));
        assert_eq!(page.final_uri.host(), "127.0.0.1");
        server.await.unwrap();
    }

    /// A `Path` names a path on the site that sent it. One that does not parse
    /// as such is ignored, not fatal — the page is intact, and refusing to show
    /// it because its label was malformed is the worse answer.
    #[tokio::test]
    async fn an_unusable_page_path_leaves_the_location_alone() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut peer = RawPeer::accept(&listener).await;
            assert_eq!(
                peer.recv_frame().await.unwrap().msg_type,
                MessageType::Hello
            );
            peer.send_frame(&RawFrame {
                msg_type: MessageType::Welcome,
                flags: 0,
                body: b"WELCOME/0.2\nCapabilities: page-path\n".to_vec(),
            })
            .await
            .unwrap();
            assert_eq!(
                peer.recv_frame().await.unwrap().msg_type,
                MessageType::Input
            );
            // `//elsewhere` would resolve to another host if it were treated as
            // a URI reference. The decoder refuses it before it gets that far,
            // so the frame is rejected outright rather than followed.
            peer.send_frame(&RawFrame {
                msg_type: MessageType::Page,
                flags: 0x04,
                body: b"Path: //elsewhere.example/x\n\n[page][/page]".to_vec(),
            })
            .await
            .unwrap();
        });

        let uri = AtpUri::parse(&format!("atp://127.0.0.1:{}/login", address.port())).unwrap();
        let mut client = AtpClient::new(TlsPolicy::plaintext_loopback());
        let owner = OperationOwner::new(
            PageScope {
                origin: client.origin_for(&uri).unwrap(),
                generation: 94,
            },
            410,
        );
        let result = client.submit(owner, &uri, "/login", "name=dusty").await;
        assert!(
            result.is_err(),
            "a cross-origin Path must not be accepted as a location"
        );
        let _ = server.await;
    }

    #[tokio::test]
    async fn fetch_404() {
        let (_dir, addr) = setup_test_server().await;

        let mut client = AtpClient::new(TlsPolicy::plaintext_loopback());
        let uri = test_uri(addr, "/nonexistent");

        let result = fetch_page_owned(&mut client, &uri).await;
        assert!(matches!(
            result,
            Err(ClientError::ServerError { code: 404, .. })
        ));
    }

    /// The wire keeps the number; the reader is not shown it. A code the spec
    /// does not define is reported verbatim rather than rounded to whichever
    /// category its leading digit suggests.
    #[test]
    fn error_codes_are_reported_as_words() {
        let not_found = ClientError::ServerError {
            code: 404,
            message: "not found".into(),
        };
        assert_eq!(
            not_found.to_string(),
            "the site has no page at this address: not found"
        );
        assert!(!not_found.to_string().contains("404"));

        for code in [400, 401, 403, 429, 500, 503] {
            let named = server_condition(code);
            assert!(!named.contains(&code.to_string()), "{code}: {named}");
        }

        assert_eq!(server_condition(418), "the site refused with code 418");
    }

    #[tokio::test]
    async fn connection_reuse() {
        let (_dir, addr) = setup_test_server().await;

        let mut client = AtpClient::new(TlsPolicy::plaintext_loopback());
        let uri1 = test_uri(addr, "/hello");
        let uri2 = test_uri(addr, "/other");

        // Fetch first page
        let r1 = fetch_page_owned(&mut client, &uri1).await.unwrap();
        assert!(r1.aml_content.contains("Hello World"));

        // Fetch second page — should reuse connection
        let r2 = fetch_page_owned(&mut client, &uri2).await.unwrap();
        assert!(r2.aml_content.contains("Other page content"));

        // Connection should still be to same host
        assert_eq!(client.current_origin.as_ref().unwrap().host(), "127.0.0.1");
        assert_eq!(client.current_origin.as_ref().unwrap().port(), addr.port());
    }

    #[tokio::test]
    async fn graceful_disconnect() {
        let (_dir, addr) = setup_test_server().await;

        let mut client = AtpClient::new(TlsPolicy::plaintext_loopback());
        let uri = test_uri(addr, "/hello");

        fetch_page_owned(&mut client, &uri).await.unwrap();
        client.disconnect().await;
        assert!(client.conn.is_none());
        assert!(client.current_origin.is_none());
    }

    #[tokio::test]
    async fn live_region_end_to_end() {
        let dir = tempfile::tempdir().unwrap();

        // Create a page with a live region
        std::fs::write(
            dir.path().join("live.aml"),
            r#"[page mode=document title="Live"]
[live id="clock" endpoint="/ticker"]
[text]Connecting...[/text]
[/live]
[/page]"#,
        )
        .unwrap();

        // Create the live content file
        std::fs::write(dir.path().join("ticker.aml"), "[text]LIVE DATA[/text]").unwrap();

        let root = dir.path().to_path_buf();
        let config = StaticServerConfig::bind_plaintext_loopback(root, "127.0.0.1", 0)
            .await
            .unwrap();
        let addr = config.local_addr().unwrap();
        tokio::spawn(async move {
            let mut server = AtpServer::new(config);
            let _ = server.run().await;
        });
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = AtpClient::new(TlsPolicy::plaintext_loopback());
        let uri = test_uri(addr, "/live");

        // Fetch the page
        let result = fetch_page_owned(&mut client, &uri).await.unwrap();
        // Parse and layout
        let mut scanner = crate::scanner::Scanner::new(result.aml_content.as_bytes()).unwrap();
        let tokens = scanner.scan_all().unwrap();
        let doc = crate::parser::parse(tokens).document.unwrap();
        let mut scene = crate::compositor::scene::build::from_document(&doc);
        let layout = crate::compositor::layout::engine::layout_scene(
            &mut scene,
            80,
            24,
            crate::color::ColorSupport::Truecolor,
            crate::compositor::layout::text::WidthConfig::default(),
        );
        let lives: Vec<_> = layout.live_regions().collect();
        assert!(!lives.is_empty(), "should have live regions in layout");
        let lr = lives[0];
        assert_eq!(lr.id, "clock");
        let endpoint = match &lr.kind {
            crate::compositor::layout::engine::PlacedKind::Live { endpoint, .. } => {
                endpoint.clone()
            }
            _ => panic!("expected live kind"),
        };
        assert_eq!(endpoint, "/ticker");

        // Subscribe
        let owner = OperationOwner::new(result.scope.clone(), 777);
        let projection = SubscriptionRegionKey::from_placed_index(3).unwrap();
        client
            .subscribe_for_projection(
                owner.clone(),
                projection,
                &endpoint,
                &lr.id,
                SubscribeMode::Replace,
            )
            .await
            .unwrap();
        assert_eq!(client.active_subscriptions.len(), 1);
        let active = client.active_subscriptions.iter().next().unwrap();
        let retained_subscription_bytes = active.endpoint.capacity()
            + active.region.capacity()
            + active.owner.scope.origin.host_capacity();
        assert_eq!(
            client.governor.used(ResourceCategory::RemoteCollections),
            retained_subscription_bytes
        );
        client
            .subscribe_for_projection(
                owner.clone(),
                projection,
                &endpoint,
                &lr.id,
                SubscribeMode::Replace,
            )
            .await
            .unwrap();
        assert_eq!(client.active_subscriptions.len(), 1);
        assert_eq!(
            client.governor.used(ResourceCategory::RemoteCollections),
            retained_subscription_bytes
        );
        assert!(
            client
                .subscribe_for_projection(
                    owner.clone(),
                    projection,
                    &endpoint,
                    &lr.id,
                    SubscribeMode::Delta,
                )
                .await
                .is_err()
        );
        assert_eq!(client.active_subscriptions.len(), 1);

        // Poll for initial update
        let update = client
            .poll_update(Duration::from_secs(3))
            .await
            .expect("poll should not error")
            .expect("should receive initial UPDATE");

        assert_eq!(update.region, "clock");
        assert_eq!(update.scope, owner.scope);
        assert_eq!(update.request_id, owner.request_id);
        assert_eq!(update.projection, Some(projection));
        assert!(
            update.content.contains("LIVE DATA"),
            "update content was: {:?}",
            update.content
        );
        client.unsubscribe().await.unwrap();
        assert_eq!(client.active_subscriptions.len(), 0);
        assert_eq!(client.governor.used(ResourceCategory::RemoteCollections), 0);
    }

    #[tokio::test]
    async fn live_region_from_actual_file() {
        // Test with the exact live-region fixture used by the client suite.
        let content =
            std::fs::read_to_string(crate::repository_root().join("tests/fixtures/site/live.aml"))
                .unwrap();
        let mut scanner = crate::scanner::Scanner::new(content.as_bytes()).unwrap();
        let tokens = scanner.scan_all().unwrap();
        let doc = crate::parser::parse(tokens).document.unwrap();
        let mut scene = crate::compositor::scene::build::from_document(&doc);
        let layout = crate::compositor::layout::engine::layout_scene(
            &mut scene,
            80,
            24,
            crate::color::ColorSupport::Truecolor,
            crate::compositor::layout::text::WidthConfig::default(),
        );
        let lives: Vec<_> = layout.live_regions().collect();
        assert!(
            !lives.is_empty(),
            "live.aml should have live regions, got: {:?}",
            layout.placed,
        );
        let lr = lives[0];
        assert_eq!(lr.id, "clock");
        let endpoint = match &lr.kind {
            crate::compositor::layout::engine::PlacedKind::Live { endpoint, .. } => {
                endpoint.clone()
            }
            _ => panic!("expected live kind"),
        };
        assert_eq!(endpoint, "/clock");
        eprintln!(
            "Live region: x={} y={} w={} h={}",
            lr.rect.x, lr.rect.y, lr.rect.w, lr.rect.h,
        );
    }

    #[tokio::test]
    async fn subscribe_and_poll_update() {
        let (dir, addr) = setup_test_server().await;

        // Create a live content file
        let live_path = dir.path().join("ticker.aml");
        std::fs::write(&live_path, "[text]initial content[/text]").unwrap();

        let mut client = AtpClient::new(TlsPolicy::plaintext_loopback());
        let uri = test_uri(addr, "/hello");

        // Fetch a page first (establishes connection)
        let page = fetch_page_owned(&mut client, &uri).await.unwrap();

        // Subscribe
        client
            .subscribe_for_projection(
                OperationOwner::new(page.scope, page.request_id.wrapping_add(1)),
                SubscriptionRegionKey::from_placed_index(0).unwrap(),
                "/ticker",
                "my-region",
                SubscribeMode::Replace,
            )
            .await
            .unwrap();

        // Should receive the initial UPDATE
        let update = client
            .poll_update(Duration::from_secs(3))
            .await
            .expect("poll_update should not error");
        assert!(update.is_some(), "should receive initial UPDATE");
        let update = update.unwrap();
        assert_eq!(update.region, "my-region");
        assert!(
            update.content.contains("initial content"),
            "initial update content: {:?}",
            update.content
        );
    }

    #[tokio::test]
    async fn fetch_resource_wasm() {
        let dir = tempfile::tempdir().unwrap();

        // Create an AML page referencing a WASM file
        std::fs::write(
            dir.path().join("index.aml"),
            r#"[page mode=document]
[animate id="tw" src="/effects/typewriter.wasm" fps=10]
    [text]Hello[/text]
[/animate]
[/page]"#,
        )
        .unwrap();

        // Create the effects directory with a WASM file
        let effects_dir = dir.path().join("effects");
        std::fs::create_dir(&effects_dir).unwrap();
        let wasm_path = "effects/typewriter/target/wasm32-unknown-unknown/release/typewriter.wasm";
        if let Ok(wasm_bytes) = std::fs::read(wasm_path) {
            std::fs::write(effects_dir.join("typewriter.wasm"), &wasm_bytes).unwrap();
        } else {
            eprintln!("skipping fetch_resource_wasm: typewriter.wasm not built");
            return;
        }

        let root = dir.path().to_path_buf();
        let config = StaticServerConfig::bind_plaintext_loopback(root, "127.0.0.1", 0)
            .await
            .unwrap();
        let addr = config.local_addr().unwrap();
        tokio::spawn(async move {
            let mut server = AtpServer::new(config);
            let _ = server.run().await;
        });
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = AtpClient::new(TlsPolicy::plaintext_loopback());
        let uri = test_uri(addr, "/");

        // Fetch the page first (establishes connection)
        let result = fetch_page_owned(&mut client, &uri).await.unwrap();
        assert!(result.aml_content.contains("typewriter.wasm"));

        // Now fetch the WASM resource
        let resource_owner = OperationOwner::new(result.scope.clone(), 2);
        let wasm = client
            .fetch_resource(resource_owner, &uri, "/effects/typewriter.wasm")
            .await
            .unwrap();
        assert!(!wasm.is_empty(), "WASM resource should not be empty");
        // WASM magic: \0asm
        assert_eq!(&wasm[0..4], b"\x00asm", "should be valid WASM");

        // Parse the page and create animations using from_document
        let doc = crate::parser::parse(
            crate::scanner::Scanner::new(result.aml_content.as_bytes())
                .unwrap()
                .scan_all()
                .unwrap(),
        )
        .document
        .unwrap();

        let mut scene = crate::compositor::scene::build::from_document(&doc);
        let lo = crate::compositor::layout::engine::layout_scene(
            &mut scene,
            40,
            10,
            crate::color::ColorSupport::Truecolor,
            crate::compositor::layout::text::WidthConfig::default(),
        );
        // Hydrate scene buffers + placements so animation nodes know
        // their rect when `from_scene` reads them.
        for p in &lo.placed {
            if let Some(node_id) = scene.find_by_aml_id(&p.id)
                && !p.rect.is_empty()
            {
                scene.allocate_buffer(node_id, p.rect.w, p.rect.h);
                scene.update_placement(
                    node_id,
                    crate::compositor::layout::engine::Placement {
                        rect: p.rect,
                        flow_advance: p.rect.h,
                        bbox: p.rect,
                    },
                );
            }
        }

        let mut prepared = std::collections::HashMap::new();
        prepared.insert(
            "/effects/typewriter.wasm".to_string(),
            std::sync::Arc::<[u8]>::from(wasm.bytes()),
        );
        let rt = crate::compositor::animate::AnimationRuntime::from_scene_with_prepared_wasm(
            &mut scene,
            crate::color::ColorSupport::Truecolor,
            crate::compositor::layout::text::WidthConfig::default(),
            &client.governor,
            &prepared,
        )
        .await
        .unwrap();

        assert!(rt.has_animations(), "should have WASM animation");
        // Verify the typewriter animation is present by id.
        let found_tw = rt.animations.iter().any(|a| a.id() == "tw");
        assert!(found_tw, "typewriter adapter should be in the runtime");
    }

    #[tokio::test]
    async fn oversized_resource_is_rejected_before_sending_or_caching() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut conn = RawPeer::accept(&listener).await;
            let hello = conn.recv_frame().await.unwrap();
            assert_eq!(hello.msg_type, MessageType::Hello);
            conn.send_frame(&RawFrame {
                msg_type: MessageType::Welcome,
                flags: 0,
                body: b"WELCOME/0.2\nServer: test\nCapabilities: wasm-effects\n".to_vec(),
            })
            .await
            .unwrap();
            let get = conn.recv_frame().await.unwrap();
            assert_eq!(get.msg_type, MessageType::Get);
            conn.send_frame(&RawFrame {
                msg_type: MessageType::Resource,
                flags: 0,
                body: vec![0; MAX_WASM_MODULE_SIZE + 1],
            })
            .await
            .unwrap();
        });

        let uri = test_uri(addr, "/");
        let mut client = AtpClient::new(TlsPolicy::plaintext_loopback());
        let owner = reducer_owner(&client, &uri);
        client.activate_page_scope(owner.scope.clone()).await;
        let error = client
            .fetch_resource(owner, &uri, "/oversized.wasm")
            .await
            .unwrap_err();
        assert!(matches!(error, ClientError::ResourceTooLarge { .. }));
        let origin = client.origin_for(&uri).unwrap();
        assert!(
            client
                .resource_cache
                .get(&origin, "/oversized.wasm")
                .is_none()
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn update_for_an_unsubscribed_region_is_rejected() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut conn = RawPeer::accept(&listener).await;
            assert_eq!(
                conn.recv_frame().await.unwrap().msg_type,
                MessageType::Hello
            );
            conn.send_frame(&RawFrame {
                msg_type: MessageType::Welcome,
                flags: 0,
                body: b"WELCOME/0.2\nServer: test\nCapabilities: live-updates\n".to_vec(),
            })
            .await
            .unwrap();
            assert_eq!(conn.recv_frame().await.unwrap().msg_type, MessageType::Get);

            let update = UpdateMessage {
                region: "clock".into(),
                content: "[text]tick[/text]".into(),
                flags: Default::default(),
            };
            conn.send_frame(&RawFrame {
                msg_type: MessageType::Update,
                flags: 0,
                body: update.serialize().unwrap().into_bytes(),
            })
            .await
            .unwrap();
            conn.send_frame(&RawFrame {
                msg_type: MessageType::Page,
                flags: 0,
                body: b"[page mode=document][text]page[/text][/page]".to_vec(),
            })
            .await
            .unwrap();
        });

        let uri = AtpUri::parse(&format!("atp://127.0.0.1:{}/", addr.port())).unwrap();
        let mut client = AtpClient::new(TlsPolicy::plaintext_loopback());
        let error = fetch_page_owned(&mut client, &uri).await.unwrap_err();
        assert!(matches!(
            error,
            ClientError::Protocol(ProtocolError::InvalidMessage(message))
                if message.contains("unsubscribed live region")
        ));
        assert_eq!(client.governor.used(ResourceCategory::PendingUpdates), 0);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn incompatible_welcome_version_is_rejected() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut conn = RawPeer::accept(&listener).await;
            conn.recv_frame().await.unwrap();
            conn.send_frame(&RawFrame {
                msg_type: MessageType::Welcome,
                flags: 0,
                body: b"WELCOME/2.0\n".to_vec(),
            })
            .await
            .unwrap();
        });

        let uri = AtpUri::parse(&format!("atp://127.0.0.1:{}/", addr.port())).unwrap();
        let mut client = AtpClient::new(TlsPolicy::plaintext_loopback());
        let owner = reducer_owner(&client, &uri);
        let error = client.fetch(owner, &uri).await.unwrap_err();
        assert!(matches!(
            error,
            ClientError::Protocol(ProtocolError::InvalidMessage(_))
        ));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn owned_fetch_returns_redirect_without_synthesizing_follow_up_owner() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut peer = RawPeer::accept(&listener).await;
            assert_eq!(
                peer.recv_frame().await.unwrap().msg_type,
                MessageType::Hello
            );
            peer.send_frame(&RawFrame {
                msg_type: MessageType::Welcome,
                flags: 0,
                body: b"WELCOME/0.2\nServer: redirect-test\n".to_vec(),
            })
            .await
            .unwrap();
            assert_eq!(peer.recv_frame().await.unwrap().msg_type, MessageType::Get);
            peer.send_frame(&RawFrame {
                msg_type: MessageType::Redirect,
                flags: 0,
                body: format!("REDIRECT 302 atp://127.0.0.1:{}/final\n", address.port())
                    .into_bytes(),
            })
            .await
            .unwrap();
        });

        let start = AtpUri::parse(&format!("atp://127.0.0.1:{}/start", address.port())).unwrap();
        let mut client = AtpClient::new(TlsPolicy::plaintext_loopback());
        let owner = OperationOwner::new(
            PageScope {
                origin: client.origin_for(&start).unwrap(),
                generation: 55,
            },
            89,
        );
        let response = client.fetch(owner.clone(), &start).await.unwrap();
        let NavigationResponse::Redirect(redirect) = response else {
            panic!("expected REDIRECT response");
        };
        assert_eq!(redirect.scope, owner.scope);
        assert_eq!(redirect.request_id, owner.request_id);
        assert_eq!(redirect.target.path(), "/final");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn owned_navigation_rejects_an_owner_for_another_origin() {
        let uri = AtpUri::parse("atp://127.0.0.1:9/page").unwrap();
        let other = AtpUri::parse("atp://localhost:9/page").unwrap();
        let mut client = AtpClient::new(TlsPolicy::plaintext_loopback());
        let owner = OperationOwner::new(
            PageScope {
                origin: client.origin_for(&other).unwrap(),
                generation: 7,
            },
            11,
        );

        let error = client.fetch(owner, &uri).await.unwrap_err();
        assert!(matches!(
            error,
            ClientError::Protocol(ProtocolError::InvalidMessage(message))
                if message.contains("owner origin")
        ));
        assert!(client.current_scope().is_none());
    }

    #[tokio::test]
    async fn cross_origin_redirect_is_returned_to_the_reducer() {
        let first_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let first_address = first_listener.local_addr().unwrap();
        let second_address = "127.0.0.1:6553".parse::<std::net::SocketAddr>().unwrap();

        let first_server = tokio::spawn(async move {
            let mut peer = RawPeer::accept(&first_listener).await;
            assert_eq!(
                peer.recv_frame().await.unwrap().msg_type,
                MessageType::Hello
            );
            peer.send_frame(&RawFrame {
                msg_type: MessageType::Welcome,
                flags: 0,
                body: b"WELCOME/0.2\nServer: first\n".to_vec(),
            })
            .await
            .unwrap();
            assert_eq!(peer.recv_frame().await.unwrap().msg_type, MessageType::Get);
            peer.send_frame(&RawFrame {
                msg_type: MessageType::Redirect,
                flags: 0,
                body: format!(
                    "REDIRECT 302 atp://127.0.0.1:{}/destination\n",
                    second_address.port()
                )
                .into_bytes(),
            })
            .await
            .unwrap();
        });
        let start =
            AtpUri::parse(&format!("atp://127.0.0.1:{}/start", first_address.port())).unwrap();
        let mut client = AtpClient::new(TlsPolicy::plaintext_loopback());
        let owner = reducer_owner(&client, &start);
        let response = client.fetch(owner.clone(), &start).await.unwrap();
        let NavigationResponse::Redirect(redirect) = response else {
            panic!("expected redirect completion");
        };
        assert_eq!(redirect.scope, owner.scope);
        assert_eq!(redirect.request_id, owner.request_id);
        assert_eq!(redirect.target.port(), second_address.port());
        assert_eq!(client.current_scope(), Some(&owner.scope));
        first_server.await.unwrap();
    }

    #[test]
    fn oversized_live_update_is_rejected() {
        let error = AtpClient::decode_update(RawFrame {
            msg_type: MessageType::Update,
            flags: 0,
            body: vec![b'x'; MAX_LIVE_UPDATE_SIZE + 1],
        })
        .unwrap_err();
        assert!(matches!(error, ClientError::LiveUpdateTooLarge { .. }));
    }

    #[test]
    fn resource_cache_promotes_hits_before_eviction() {
        let mut cache = ResourceCache::new();
        let uri = AtpUri::parse("atp://example.com/").unwrap();
        let origin = Origin::from_uri(&uri, TransportSecurity::VerifiedTls).unwrap();
        for index in 0..MAX_RESOURCE_CACHE {
            cache
                .insert(origin.clone(), format!("/{index}"), vec![index as u8])
                .unwrap();
        }
        assert_eq!(
            cache.get(&origin, "/0").map(|resource| resource.to_vec()),
            Some(vec![0])
        );
        cache
            .insert(origin.clone(), "/new".into(), vec![255])
            .unwrap();
        assert!(cache.get(&origin, "/0").is_some());
        assert!(cache.get(&origin, "/1").is_none());
    }

    #[test]
    fn resource_cache_hits_share_the_allocation_lease_through_completion_drop() {
        let mut cache = ResourceCache::new();
        let uri = AtpUri::parse("atp://example.com/").unwrap();
        let origin = Origin::from_uri(&uri, TransportSecurity::VerifiedTls).unwrap();
        cache
            .insert(origin.clone(), "/effect.wasm".into(), vec![1, 2, 3, 4])
            .unwrap();
        let governor = cache.governor.clone();
        let resource = cache.get(&origin, "/effect.wasm").unwrap();

        assert!(cache.evict_oldest());
        assert_eq!(&*resource, &[1, 2, 3, 4]);
        assert_eq!(governor.used(ResourceCategory::ResourceCache), 4);
        drop(resource);
        assert_eq!(governor.used(ResourceCategory::ResourceCache), 0);
    }

    #[test]
    fn shared_resource_owner_accounts_capacity_header_and_final_clone() {
        for requested_capacity in [0, 37] {
            let governor = ResourceGovernor::new();
            let mut data = Vec::new();
            data.try_reserve_exact(requested_capacity).unwrap();
            if requested_capacity > 0 {
                data.extend_from_slice(&[1, 2, 3, 4]);
            }
            let capacity = data.capacity();
            let data_ptr = data.as_ptr();
            let expected_cost = SharedResource::allocation_byte_cost(capacity).unwrap();
            let resource = SharedResource::try_new(data, &governor).unwrap();

            assert_eq!(resource.as_ptr(), data_ptr, "the response Vec must move");
            assert_eq!(governor.used(ResourceCategory::ResourceCache), capacity);
            assert_eq!(governor.total_used(), expected_cost);
            assert_eq!(governor.count(ResourceCategory::ResourceCache), 1);

            let clone = resource.clone();
            assert!(resource.ptr_eq(&clone));
            drop(resource);
            assert_eq!(governor.total_used(), expected_cost);
            drop(clone);
            assert_eq!(governor.used(ResourceCategory::ResourceCache), 0);
            assert_eq!(governor.total_used(), 0);
            assert_eq!(governor.count(ResourceCategory::ResourceCache), 0);
        }
    }

    #[test]
    fn shared_resource_owner_rejection_preserves_same_cache_entry() {
        let mut cache = ResourceCache::new();
        let uri = AtpUri::parse("atp://example.com/").unwrap();
        let origin = Origin::from_uri(&uri, TransportSecurity::VerifiedTls).unwrap();
        cache
            .insert(origin.clone(), "/effect.wasm".into(), vec![1, 2, 3, 4])
            .unwrap();
        let original = cache.get(&origin, "/effect.wasm").unwrap();
        let used = cache.governor.total_used();
        let count = cache.governor.count(ResourceCategory::ResourceCache);

        REJECT_SHARED_RESOURCE_OWNER.with(|rejected| rejected.set(true));
        let result = cache.insert(origin.clone(), "/effect.wasm".into(), vec![9, 8, 7]);
        REJECT_SHARED_RESOURCE_OWNER.with(|rejected| rejected.set(false));

        assert!(matches!(
            result,
            Err(ClientError::Protocol(
                ProtocolError::ResourceExhausted { .. }
            ))
        ));
        let retained = cache.get(&origin, "/effect.wasm").unwrap();
        assert!(retained.ptr_eq(&original));
        assert_eq!(&*retained, &[1, 2, 3, 4]);
        assert_eq!(cache.governor.total_used(), used);
        assert_eq!(cache.governor.count(ResourceCategory::ResourceCache), count);
    }

    #[test]
    fn resource_cache_keys_hold_exact_remote_collection_leases() {
        let mut cache = ResourceCache::new();
        let uri = AtpUri::parse("atp://example.com/").unwrap();
        let origin = Origin::from_uri(&uri, TransportSecurity::VerifiedTls).unwrap();
        let mut path = String::new();
        path.try_reserve_exact(37).unwrap();
        path.push_str("/effect.wasm");
        let expected = origin.host_capacity() + path.capacity();
        let governor = cache.governor.clone();

        cache.insert(origin, path, vec![1]).unwrap();
        assert_eq!(governor.used(ResourceCategory::RemoteCollections), expected);
        assert!(cache.evict_oldest());
        assert_eq!(governor.used(ResourceCategory::RemoteCollections), 0);
    }

    #[test]
    fn resource_cache_key_budget_rejection_preserves_existing_entry() {
        let mut cache = ResourceCache::new();
        let uri = AtpUri::parse("atp://example.com/").unwrap();
        let origin = Origin::from_uri(&uri, TransportSecurity::VerifiedTls).unwrap();
        cache
            .insert(origin.clone(), "/existing".into(), vec![1])
            .unwrap();
        let used = cache.governor.total_used();

        // Refuse the key specifically, with the budget otherwise empty: the
        // existing entry and the LRU order must survive a refusal that is not
        // caused by the cache being full.
        {
            let _rejection = ClientStorageRejectionGuard::at(ClientStorageAllocationSite::CacheKey);
            assert!(
                cache
                    .insert(origin.clone(), "/named".into(), vec![2])
                    .is_err()
            );
            assert!(cache.get(&origin, "/existing").is_some());
            assert!(cache.get(&origin, "/named").is_none());
            assert_eq!(cache.governor.total_used(), used);
        }
        let _pressure = cache
            .governor
            .reserve(
                ResourceCategory::AstStrings,
                crate::resource::MAX_REMOTE_MEMORY - used,
            )
            .unwrap();

        assert!(
            cache
                .insert(origin.clone(), "/replacement".into(), vec![2])
                .is_err()
        );
        assert!(cache.get(&origin, "/existing").is_some());
        assert!(cache.get(&origin, "/replacement").is_none());
    }

    #[test]
    fn resource_cache_partitions_security_context_and_evicts_by_bytes() {
        let uri = AtpUri::parse("atp://example.com/").unwrap();
        let verified = Origin::from_uri(&uri, TransportSecurity::VerifiedTls).unwrap();
        let insecure = Origin::from_uri(&uri, TransportSecurity::InsecureTls).unwrap();
        let mut cache = ResourceCache::new();
        cache
            .insert(verified.clone(), "/effect.wasm".into(), vec![1])
            .unwrap();
        assert!(cache.get(&insecure, "/effect.wasm").is_none());

        let chunk = MAX_RESOURCE_CACHE_BYTES / 2 + 1;
        cache
            .insert(verified.clone(), "/large-a".into(), vec![2; chunk])
            .unwrap();
        cache
            .insert(verified.clone(), "/large-b".into(), vec![3; chunk])
            .unwrap();
        assert!(cache.get(&verified, "/large-a").is_none());
        assert!(cache.get(&verified, "/large-b").is_some());
        assert!(cache.bytes <= MAX_RESOURCE_CACHE_BYTES);
    }

    #[test]
    fn explicit_pressure_eviction_releases_the_shared_lease() {
        let mut client = AtpClient::new(TlsPolicy::verified());
        let uri = AtpUri::parse("atp://example.com/").unwrap();
        let origin = Origin::from_uri(&uri, TransportSecurity::VerifiedTls).unwrap();
        client
            .resource_cache
            .insert(origin, "/effect.wasm".into(), vec![7; 4096])
            .unwrap();
        assert!(client.governor.total_used() >= 4096);
        assert!(client.evict_oldest_resource());
        assert_eq!(client.governor.used(ResourceCategory::ResourceCache), 0);
        assert!(!client.evict_oldest_resource());
    }

    #[test]
    fn control_messages_reject_invalid_utf8() {
        assert!(matches!(
            decode_control_body(&[0xff]),
            Err(ClientError::Protocol(ProtocolError::InvalidMessage(_)))
        ));
    }

    #[tokio::test]
    async fn subscription_admission_and_send_failure_preserve_exact_empty_state() {
        let mut client = AtpClient::new(TlsPolicy::plaintext_loopback());
        let uri = AtpUri::parse("atp://127.0.0.1:4040/live").unwrap();
        let scope = PageScope {
            origin: client.request_origin(&uri).unwrap(),
            generation: 7,
        };
        client.current_scope = Some(scope.clone());
        client.negotiated_capabilities = NegotiatedCapabilities::negotiate(
            &["live-updates".to_string()],
            &["live-updates".to_string()],
        );
        let oversized = "x".repeat(MAX_CONTROL_MESSAGE_SIZE);
        assert!(matches!(
            client
                .subscribe_for_projection(
                    OperationOwner::new(scope.clone(), 8),
                    SubscriptionRegionKey::from_placed_index(0).unwrap(),
                    &oversized,
                    "clock",
                    SubscribeMode::Replace,
                )
                .await,
            Err(ClientError::Protocol(ProtocolError::MessageTooLarge { .. }))
        ));
        assert_eq!(client.active_subscriptions.len(), 0);
        assert_eq!(client.governor.used(ResourceCategory::RemoteCollections), 0);
        for site in [
            SubscriptionAllocationSite::Body,
            SubscriptionAllocationSite::Endpoint,
            SubscriptionAllocationSite::Region,
        ] {
            REJECT_SUBSCRIPTION_ALLOCATION.with(|rejected| rejected.set(Some(site)));
            assert!(matches!(
                client
                    .subscribe_for_projection(
                        OperationOwner::new(scope.clone(), 8),
                        SubscriptionRegionKey::from_placed_index(0).unwrap(),
                        "/ticker",
                        "clock",
                        SubscribeMode::Replace,
                    )
                    .await,
                Err(ClientError::Protocol(
                    ProtocolError::ResourceExhausted { .. }
                ))
            ));
            REJECT_SUBSCRIPTION_ALLOCATION.with(|rejected| rejected.set(None));
            assert_eq!(client.active_subscriptions.len(), 0);
            assert_eq!(client.governor.used(ResourceCategory::RemoteCollections), 0);
            assert_eq!(
                client.governor.count(ResourceCategory::RemoteCollections),
                0
            );
        }
        let blocker = client
            .governor
            .reserve(
                ResourceCategory::RemoteCollections,
                crate::resource::MAX_REMOTE_MEMORY,
            )
            .unwrap();
        let rejected = client
            .subscribe_for_projection(
                OperationOwner::new(scope.clone(), 9),
                SubscriptionRegionKey::from_placed_index(0).unwrap(),
                "/ticker",
                "clock",
                SubscribeMode::Replace,
            )
            .await;
        assert!(matches!(
            rejected,
            Err(ClientError::Protocol(
                ProtocolError::ResourceExhausted { .. }
            ))
        ));
        assert_eq!(client.active_subscriptions.len(), 0);
        assert_eq!(
            client.governor.used(ResourceCategory::RemoteCollections),
            crate::resource::MAX_REMOTE_MEMORY
        );

        drop(blocker);
        assert!(
            client
                .subscribe_for_projection(
                    OperationOwner::new(scope.clone(), 9),
                    SubscriptionRegionKey::from_placed_index(0).unwrap(),
                    "/ticker",
                    "clock",
                    SubscribeMode::Replace,
                )
                .await
                .is_err()
        );
        assert_eq!(client.active_subscriptions.len(), 0);
        assert_eq!(client.governor.used(ResourceCategory::RemoteCollections), 0);
        assert_eq!(
            client.governor.count(ResourceCategory::RemoteCollections),
            0
        );
        for index in 0..MAX_ACTIVE_SUBSCRIPTIONS {
            install_test_subscription(
                &mut client,
                OperationOwner::new(scope.clone(), index as u64 + 20),
                &format!("/ticker/{index}"),
                &format!("clock-{index}"),
            );
        }
        let before = client.governor.used(ResourceCategory::RemoteCollections);
        assert!(
            client
                .subscribe_for_projection(
                    OperationOwner::new(scope, 99),
                    SubscriptionRegionKey::from_placed_index(0).unwrap(),
                    "/overflow",
                    "overflow",
                    SubscribeMode::Replace,
                )
                .await
                .is_err()
        );
        assert_eq!(client.active_subscriptions.len(), MAX_ACTIVE_SUBSCRIPTIONS);
        assert_eq!(
            client.governor.used(ResourceCategory::RemoteCollections),
            before
        );
        client.active_subscriptions.clear_and_release();
        assert_eq!(client.governor.used(ResourceCategory::RemoteCollections), 0);
    }

    #[tokio::test]
    async fn unsubscribe_retires_queued_update_owners_and_leases() {
        let mut client = AtpClient::new(TlsPolicy::plaintext_loopback());
        let uri = AtpUri::parse("atp://127.0.0.1:4040/current").unwrap();
        let scope = reducer_owner(&client, &uri).scope;
        client.activate_page_scope(scope.clone()).await;
        install_test_subscription(
            &mut client,
            OperationOwner::new(scope.clone(), 41),
            "/ticker",
            "clock",
        );
        let update = UpdateMessage {
            region: "clock".into(),
            content: "[text]tick[/text]".into(),
            flags: Default::default(),
        };
        install_test_pending_update(&mut client, OperationOwner::new(scope, 41), update);

        client.unsubscribe().await.unwrap();

        assert_eq!(client.active_subscriptions.len(), 0);
        assert!(client.pending_updates.is_empty());
        assert_eq!(client.pending_updates.bytes(), 0);
        assert_eq!(client.governor.used(ResourceCategory::PendingUpdates), 0);
    }

    #[tokio::test]
    async fn page_retirement_removes_only_the_exact_subscription_owner() {
        let mut client = AtpClient::new(TlsPolicy::plaintext_loopback());
        let uri = AtpUri::parse("atp://127.0.0.1:4040/current").unwrap();
        let origin = client.request_origin(&uri).unwrap();
        let retired_scope = PageScope {
            origin: origin.clone(),
            generation: 10,
        };
        let current_scope = PageScope {
            origin,
            generation: 11,
        };
        client.current_scope = Some(current_scope.clone());
        install_test_subscription(
            &mut client,
            OperationOwner::new(retired_scope.clone(), 40),
            "/old",
            "old",
        );
        install_test_subscription(
            &mut client,
            OperationOwner::new(current_scope.clone(), 41),
            "/current",
            "current",
        );

        for (scope, region) in [
            (retired_scope.clone(), "old"),
            (current_scope.clone(), "current"),
        ] {
            let update = UpdateMessage {
                region: region.into(),
                content: "tick".into(),
                flags: Default::default(),
            };
            install_test_pending_update(&mut client, OperationOwner::new(scope, 41), update);
        }
        let before = client.governor.used(ResourceCategory::PendingUpdates);

        client.retire_page_work(&retired_scope).await;

        assert_eq!(client.current_scope(), Some(&current_scope));
        assert_eq!(client.active_subscriptions.len(), 1);
        let active = client.active_subscriptions.iter().next().unwrap();
        assert_eq!(active.region, "current");
        assert_eq!(active.owner.scope, current_scope);
        assert_eq!(client.pending_updates.len(), 1);
        let pending = client.pending_updates.get(0).unwrap();
        assert_eq!(pending.update.region, "current");
        assert_eq!(pending.owner.scope, current_scope);
        assert!(client.governor.used(ResourceCategory::PendingUpdates) < before);
        assert_eq!(
            client.pending_updates.bytes(),
            pending.update.region.len() + pending.update.content.len()
        );
    }

    #[tokio::test]
    async fn current_page_retirement_drops_all_subscription_and_update_leases() {
        let mut client = AtpClient::new(TlsPolicy::plaintext_loopback());
        let uri = AtpUri::parse("atp://127.0.0.1:4040/current").unwrap();
        let scope = PageScope {
            origin: client.request_origin(&uri).unwrap(),
            generation: 12,
        };
        client.current_scope = Some(scope.clone());
        install_test_subscription(
            &mut client,
            OperationOwner::new(scope.clone(), 51),
            "/ticker",
            "clock",
        );
        let update = UpdateMessage {
            region: "clock".into(),
            content: "tick".into(),
            flags: Default::default(),
        };
        install_test_pending_update(&mut client, OperationOwner::new(scope.clone(), 51), update);

        client.retire_page_work(&scope).await;

        assert!(client.current_scope().is_none());
        assert_eq!(client.active_subscriptions.len(), 0);
        assert!(client.pending_updates.is_empty());
        assert_eq!(client.pending_updates.bytes(), 0);
        assert_eq!(client.governor.used(ResourceCategory::PendingUpdates), 0);
    }

    #[tokio::test]
    async fn viewer_issued_page_scope_is_adopted_unchanged() {
        let uri = AtpUri::parse("atp://127.0.0.1:4040/current").unwrap();
        let mut client = AtpClient::new(TlsPolicy::plaintext_loopback());
        let scope = reducer_owner(&client, &uri).scope;

        client.activate_page_scope(scope.clone()).await;

        assert_eq!(client.current_scope(), Some(&scope));
    }

    #[tokio::test]
    async fn reconnect_rejects_a_different_page_origin_before_network_io() {
        let mut client = AtpClient::new(TlsPolicy::plaintext_loopback());
        let current = AtpUri::parse("atp://127.0.0.1:4040/current").unwrap();
        let scope = reducer_owner(&client, &current).scope;
        client.activate_page_scope(scope).await;
        let other = AtpUri::parse("atp://127.0.0.2:4040/other").unwrap();
        let error = client.reconnect_current_page(&other).await.unwrap_err();
        assert!(matches!(
            error,
            ClientError::Protocol(ProtocolError::InvalidMessage(_))
        ));
        assert_eq!(client.current_scope().unwrap().origin.host(), "127.0.0.1");
    }
}
