use arrayvec::ArrayVec;

#[cfg(test)]
use std::cell::Cell;
#[cfg(test)]
use std::collections::HashMap;

use dustnet_core::protocol::origin::Origin;
use dustnet_core::protocol::uri::AtpUri;

pub type RequestId = u64;
pub type HistoryId = u64;
pub const MAX_HISTORY_ENTRIES: usize = 128;
pub const MAX_HISTORY_AML_BYTES: usize = 16 * 1024 * 1024;
pub type HistoryEntries = ArrayVec<HistoryEntry, MAX_HISTORY_ENTRIES>;
/// Hard ceiling for reducer-issued work owned by the current page generation.
///
/// This exceeds the maximum authored WASM and subscription fan-out together,
/// with room for navigation, resize, animation tick, deferred navigation, and
/// reconnect work. The inline table cannot allocate or clone a page scope.
pub const MAX_OUTSTANDING_REQUESTS: usize = 64;

/// Allocation-free identity for one live-region projection in the active page.
///
/// Only the viewer runtime creates these keys. Reducer consumers may inspect
/// the placed-element index without being able to mint a competing identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SubscriptionRegionKey(u32);

impl SubscriptionRegionKey {
    pub(crate) fn from_placed_index(index: usize) -> Option<Self> {
        u32::try_from(index).ok().map(Self)
    }

    pub const fn placed_index(self) -> usize {
        self.0 as usize
    }
}

/// Viewer-issued identity for one page generation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PageScope {
    pub(crate) origin: Origin,
    pub(crate) generation: u64,
}

impl PageScope {
    pub fn origin(&self) -> &Origin {
        &self.origin
    }

    pub const fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn try_clone(&self) -> Result<Self, std::collections::TryReserveError> {
        Ok(Self {
            origin: self.origin.try_clone()?,
            generation: self.generation,
        })
    }
}

/// Viewer-issued identity for one page-scoped asynchronous operation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OperationOwner {
    pub(crate) scope: PageScope,
    pub(crate) request_id: RequestId,
}

impl OperationOwner {
    pub(crate) const fn new(scope: PageScope, request_id: RequestId) -> Self {
        Self { scope, request_id }
    }

    pub fn scope(&self) -> &PageScope {
        &self.scope
    }

    pub const fn request_id(&self) -> RequestId {
        self.request_id
    }

    pub(crate) fn try_clone(&self) -> Result<Self, std::collections::TryReserveError> {
        Ok(Self {
            scope: self.scope.try_clone()?,
            request_id: self.request_id,
        })
    }
}

#[cfg(test)]
thread_local! {
    static PREPARATION_REJECT_AFTER: Cell<Option<usize>> = const { Cell::new(None) };
}

#[cfg(test)]
fn preparation_rejected() -> bool {
    PREPARATION_REJECT_AFTER.with(|site| match site.get() {
        Some(0) => {
            site.set(None);
            true
        }
        Some(remaining) => {
            site.set(Some(remaining - 1));
            false
        }
        None => false,
    })
}

#[cfg(not(test))]
fn preparation_rejected() -> bool {
    false
}

#[cfg(test)]
fn reject_preparation_after(successful_steps: usize) {
    PREPARATION_REJECT_AFTER.with(|site| site.set(Some(successful_steps)));
}

#[cfg(test)]
fn clear_preparation_rejection() {
    PREPARATION_REJECT_AFTER.with(|site| site.set(None));
}

/// Capability for projecting user-controlled state into the active page.
///
/// Tokens are generation scoped, so terminal input collected for a retired
/// page cannot mutate the replacement page after navigation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlToken {
    pub(crate) scope: PageScope,
}

impl ControlToken {
    pub fn scope(&self) -> &PageScope {
        &self.scope
    }
}

/// Terminal presentation mutations requested by local input or authored AML.
/// Every action is scope-checked by the reducer before the runtime may apply
/// it to the active scene.
#[derive(Debug, PartialEq, Eq)]
pub enum PresentationAction {
    PageLoad,
    AnimationEnd {
        source: String,
    },
    Focus {
        source: Option<String>,
    },
    Blur {
        source: Option<String>,
    },
    StateChange {
        source: String,
    },
    SetPanel {
        panel_id: String,
        state: String,
    },
    TogglePanel {
        panel_id: String,
        states: Vec<String>,
    },
    ToggleDetails {
        index: usize,
    },
    AdvanceFocus {
        forward: bool,
    },
    AdvanceSelect {
        node: crate::compositor::scene::NodeId,
    },
    SetScroll {
        offset: u16,
    },
    ScrollToRow {
        row: u16,
    },
    ClearFocus,
    SkipAnimations,
    DrainLayout,
    CapturePageTransition,
    StartPageTransition {
        kind: crate::parser::ast::TransitionKind,
        duration_ms: u32,
    },
    InvalidateFull,
    ActivateLocalPage {
        aml: String,
        uri: Option<AtpUri>,
        overlay: bool,
    },
    FlushPendingActions,
    TickPendingActions,
}

impl PresentationAction {
    pub(crate) fn try_clone(&self) -> Result<Self, std::collections::TryReserveError> {
        fn text(value: &str) -> Result<String, std::collections::TryReserveError> {
            let mut copy = String::new();
            copy.try_reserve_exact(value.len())?;
            copy.push_str(value);
            Ok(copy)
        }
        fn maybe(value: Option<&str>) -> Result<Option<String>, std::collections::TryReserveError> {
            value.map(text).transpose()
        }
        Ok(match self {
            Self::PageLoad => Self::PageLoad,
            Self::AnimationEnd { source } => Self::AnimationEnd {
                source: text(source)?,
            },
            Self::Focus { source } => Self::Focus {
                source: maybe(source.as_deref())?,
            },
            Self::Blur { source } => Self::Blur {
                source: maybe(source.as_deref())?,
            },
            Self::StateChange { source } => Self::StateChange {
                source: text(source)?,
            },
            Self::SetPanel { panel_id, state } => Self::SetPanel {
                panel_id: text(panel_id)?,
                state: text(state)?,
            },
            Self::TogglePanel { panel_id, states } => {
                let mut copied = Vec::new();
                copied.try_reserve_exact(states.len())?;
                for state in states {
                    copied.push(text(state)?);
                }
                Self::TogglePanel {
                    panel_id: text(panel_id)?,
                    states: copied,
                }
            }
            Self::ToggleDetails { index } => Self::ToggleDetails { index: *index },
            Self::AdvanceFocus { forward } => Self::AdvanceFocus { forward: *forward },
            Self::AdvanceSelect { node } => Self::AdvanceSelect { node: *node },
            Self::SetScroll { offset } => Self::SetScroll { offset: *offset },
            Self::ScrollToRow { row } => Self::ScrollToRow { row: *row },
            Self::ClearFocus => Self::ClearFocus,
            Self::SkipAnimations => Self::SkipAnimations,
            Self::DrainLayout => Self::DrainLayout,
            Self::CapturePageTransition => Self::CapturePageTransition,
            Self::StartPageTransition { kind, duration_ms } => Self::StartPageTransition {
                kind: *kind,
                duration_ms: *duration_ms,
            },
            Self::InvalidateFull => Self::InvalidateFull,
            Self::ActivateLocalPage { aml, uri, overlay } => Self::ActivateLocalPage {
                aml: text(aml)?,
                uri: uri.as_ref().map(AtpUri::try_clone).transpose()?,
                overlay: *overlay,
            },
            Self::FlushPendingActions => Self::FlushPendingActions,
            Self::TickPendingActions => Self::TickPendingActions,
        })
    }
}

/// Exact scoped work retained while the reducer orders pressure recovery.
/// A retry is emitted only after the corresponding cache/history lease has
/// been released, and is discarded when the page scope is retired.
#[derive(Debug, PartialEq, Eq)]
pub enum PressureRetry {
    Parse {
        owner: OperationOwner,
    },
    PrepareLayout {
        owner: OperationOwner,
    },
    HistoryArtifact {
        owner: OperationOwner,
        id: HistoryId,
        replacing: bool,
    },
    ResizeProjection {
        owner: OperationOwner,
        width: u16,
        height: u16,
    },
    TickWasm {
        owner: Option<OperationOwner>,
    },
    ActivateCachedHistory {
        owner: OperationOwner,
        entry: HistoryEntry,
    },
    Presentation {
        scope: Option<PageScope>,
        action: PresentationAction,
    },
    Update {
        owner: OperationOwner,
        region: SubscriptionRegionKey,
    },
}

impl PressureRetry {
    fn into_effect(self) -> ViewerEffect {
        match self {
            Self::Parse { owner } => ViewerEffect::Parse { owner },
            Self::PrepareLayout { owner } => ViewerEffect::PrepareLayout { owner },
            Self::HistoryArtifact {
                owner,
                id,
                replacing,
            } => ViewerEffect::AdmitHistoryArtifact {
                owner,
                id,
                replacing,
            },
            Self::ResizeProjection {
                owner,
                width,
                height,
            } => ViewerEffect::PrepareResizeProjection {
                owner,
                width,
                height,
            },
            Self::TickWasm { owner } => ViewerEffect::TickWasm { owner },
            Self::ActivateCachedHistory { owner, entry } => {
                ViewerEffect::ActivateCachedHistory { owner, entry }
            }
            Self::Presentation { scope, action } => {
                ViewerEffect::ApplyPresentationAction { scope, action }
            }
            Self::Update { owner, region } => ViewerEffect::ApplyUpdate { owner, region },
        }
    }

    fn belongs_to(&self, scope: Option<&PageScope>) -> bool {
        match self {
            Self::Parse { owner, .. }
            | Self::PrepareLayout { owner }
            | Self::HistoryArtifact { owner, .. }
            | Self::ResizeProjection { owner, .. }
            | Self::ActivateCachedHistory { owner, .. } => Some(&owner.scope) == scope,
            Self::TickWasm { owner: Some(owner) } => Some(&owner.scope) == scope,
            Self::TickWasm { owner: None } => scope.is_none(),
            Self::Presentation { scope: owner, .. } => owner.as_ref() == scope,
            Self::Update { owner, .. } => Some(&owner.scope) == scope,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavigationPhase {
    Idle,
    Connecting,
    Fetching,
    Parsing,
    Layout,
    Ready,
    Failed,
    ShuttingDown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionStatus {
    Disconnected,
    Connecting,
    Connected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HistoryCommit {
    Append,
    ReplaceCurrent,
}

#[derive(Debug)]
struct PendingHistoryActivation {
    target: usize,
    target_generation: u64,
    previous_entry_scope: PageScope,
    previous_position: Option<usize>,
    previous_scope: Option<PageScope>,
    previous_uri: Option<AtpUri>,
    previous_phase: NavigationPhase,
    previous_connection: ConnectionStatus,
}

#[derive(Debug)]
struct PendingHistoryCommit {
    owner: OperationOwner,
    id: HistoryId,
    uri: AtpUri,
    retained_aml: String,
    commit: HistoryCommit,
    prepared_effects: Vec<ViewerEffect>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingResizeProjection {
    owner: OperationOwner,
    width: u16,
    height: u16,
}

#[derive(Debug, PartialEq, Eq)]
pub struct HistoryEntry {
    pub id: HistoryId,
    pub scope: PageScope,
    pub uri: AtpUri,
    pub retained_aml: String,
}

impl HistoryEntry {
    pub(crate) fn try_clone(&self) -> Result<Self, std::collections::TryReserveError> {
        let mut retained_aml = String::new();
        retained_aml.try_reserve_exact(self.retained_aml.len())?;
        retained_aml.push_str(&self.retained_aml);
        Ok(Self {
            id: self.id,
            scope: self.scope.try_clone()?,
            uri: self.uri.try_clone()?,
            retained_aml,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ViewerEvent {
    InitialNavigation {
        uri: AtpUri,
        origin: Origin,
    },
    Navigate {
        uri: AtpUri,
        origin: Origin,
    },
    Redirect {
        uri: AtpUri,
        origin: Origin,
    },
    Reload,
    Back,
    Forward,
    JumpToHistory {
        index: usize,
    },
    FormSubmitted {
        uri: AtpUri,
        origin: Origin,
        path: String,
        form_data: String,
    },
    Input {
        token: ControlToken,
        value: String,
    },
    FocusChanged {
        token: ControlToken,
        focused: Option<crate::compositor::scene::NodeId>,
    },
    Resize {
        width: u16,
        height: u16,
    },
    /// Terminal-owned projection state became dirty and requests one ordered
    /// presentation pass. The runtime may present only after the reducer
    /// returns `RenderTerminal`.
    PresentationRequested,
    PresentationActionRequested {
        scope: Option<PageScope>,
        action: PresentationAction,
    },
    Timer,
    Connected {
        owner: OperationOwner,
    },
    ConnectionFailed {
        owner: OperationOwner,
        message: String,
    },
    FetchCompleted {
        owner: OperationOwner,
    },
    HistoryCommitted {
        owner: OperationOwner,
        id: HistoryId,
    },
    HistoryAdmissionRequested {
        owner: OperationOwner,
        uri: AtpUri,
        retained_aml: String,
    },
    FetchFailed {
        owner: OperationOwner,
        message: String,
    },
    ParseCompleted {
        owner: OperationOwner,
    },
    /// Parsed page dependencies that must be fetched before layout. Effects
    /// are emitted before `PrepareLayout` because this event is reduced ahead
    /// of the matching `ParseCompleted` event.
    WasmDependenciesDiscovered {
        owner: OperationOwner,
        paths: Vec<String>,
    },
    ParseFailed {
        owner: OperationOwner,
        message: String,
    },
    LayoutPrepared {
        owner: OperationOwner,
        content_height: u32,
    },
    ResizeProjectionPrepared {
        owner: OperationOwner,
        content_height: u32,
    },
    LayoutActivated {
        owner: OperationOwner,
    },
    LayoutFailed {
        owner: OperationOwner,
        message: String,
    },
    /// Adopt a bounded client-authored rejection page without issuing a new
    /// remote page generation.
    ErrorPageActivated {
        content_height: u32,
    },
    /// The active projection could not be presented within local resource
    /// limits. Recovery remains reducer-ordered even though the failure was
    /// detected by the terminal runtime.
    PresentationFailed {
        message: String,
        retry: Option<PressureRetry>,
    },
    /// Ask the reducer to evict its oldest non-current history entry. The
    /// renderer releases the matching projected artifact from the returned
    /// effect before later recovery work continues.
    ResourceEvictionCompleted {
        message: String,
        evicted: bool,
    },
    HistoryEvictionRequested {
        message: String,
    },
    WasmRequested {
        path: String,
    },
    WasmPrepared {
        owner: OperationOwner,
    },
    WasmActivated {
        owner: OperationOwner,
    },
    WasmRejected {
        owner: OperationOwner,
    },
    SubscriptionCompleted {
        owner: OperationOwner,
        region: SubscriptionRegionKey,
    },
    SubscriptionFailed {
        owner: OperationOwner,
    },
    SubscribeRequested {
        region: SubscriptionRegionKey,
    },
    DeferredNavigationRequested,
    DeferredNavigationCompleted {
        owner: OperationOwner,
    },
    DeferredNavigationRejected {
        owner: OperationOwner,
    },
    SubscriptionsRetired {
        scope: PageScope,
    },
    LiveUpdate {
        owner: OperationOwner,
        region: SubscriptionRegionKey,
    },
    TransportLost {
        origin: Origin,
    },
    Shutdown,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ViewerEffect {
    Connect {
        owner: OperationOwner,
    },
    Close,
    Fetch {
        owner: OperationOwner,
        uri: AtpUri,
    },
    Submit {
        owner: OperationOwner,
        uri: AtpUri,
        path: String,
        form_data: String,
    },
    Subscribe {
        owner: OperationOwner,
        region: SubscriptionRegionKey,
    },
    Parse {
        owner: OperationOwner,
    },
    PrepareLayout {
        owner: OperationOwner,
    },
    PrepareResizeProjection {
        owner: OperationOwner,
        width: u16,
        height: u16,
    },
    AdmitHistoryArtifact {
        owner: OperationOwner,
        id: HistoryId,
        replacing: bool,
    },
    InstallHistoryArtifact {
        owner: OperationOwner,
        id: HistoryId,
    },
    ActivateLayout {
        owner: OperationOwner,
    },
    LoadWasm {
        owner: OperationOwner,
        path: String,
    },
    ActivateWasm {
        owner: OperationOwner,
    },
    ActivateCachedHistory {
        owner: OperationOwner,
        entry: HistoryEntry,
    },
    TickWasm {
        owner: Option<OperationOwner>,
    },
    ApplyUpdate {
        owner: OperationOwner,
        region: SubscriptionRegionKey,
    },
    ProjectInput {
        token: ControlToken,
        value: String,
    },
    ProjectFocus {
        token: ControlToken,
        focused: Option<crate::compositor::scene::NodeId>,
    },
    ApplyPresentationAction {
        scope: Option<PageScope>,
        action: PresentationAction,
    },
    RetireSubscriptions {
        scope: PageScope,
    },
    DeferNavigation {
        owner: OperationOwner,
    },
    ResumeNavigation {
        owner: OperationOwner,
    },
    RenderTerminal,
    EvictResource {
        message: String,
    },
    EvictHistory {
        message: String,
    },
    ReleaseHistoryArtifact {
        id: HistoryId,
    },
    RetirePageWork {
        scope: PageScope,
    },
    ActivateErrorPage {
        message: String,
    },
    RestoreTerminal,
}

#[derive(Debug)]
struct SubscriptionOwnership {
    entries: [Option<(RequestId, SubscriptionRegionKey)>; crate::client::MAX_ACTIVE_SUBSCRIPTIONS],
}

impl SubscriptionOwnership {
    const fn new() -> Self {
        Self {
            entries: [None; crate::client::MAX_ACTIVE_SUBSCRIPTIONS],
        }
    }

    fn len(&self) -> usize {
        self.entries.iter().flatten().count()
    }

    fn insert(&mut self, request_id: RequestId, region: SubscriptionRegionKey) -> bool {
        let Some(slot) = self.entries.iter_mut().find(|slot| slot.is_none()) else {
            return false;
        };
        *slot = Some((request_id, region));
        true
    }

    fn contains(&self, request_id: RequestId, region: SubscriptionRegionKey) -> bool {
        self.entries.contains(&Some((request_id, region)))
    }

    fn remove_exact(&mut self, request_id: RequestId, region: SubscriptionRegionKey) -> bool {
        let Some(slot) = self
            .entries
            .iter_mut()
            .find(|slot| **slot == Some((request_id, region)))
        else {
            return false;
        };
        *slot = None;
        true
    }

    fn remove_request(&mut self, request_id: RequestId) -> bool {
        let Some(slot) = self
            .entries
            .iter_mut()
            .find(|slot| slot.is_some_and(|(pending_id, _)| pending_id == request_id))
        else {
            return false;
        };
        *slot = None;
        true
    }

    fn contains_request(&self, request_id: RequestId) -> bool {
        self.entries
            .iter()
            .flatten()
            .any(|(pending_id, _)| *pending_id == request_id)
    }

    fn clear(&mut self) {
        self.entries.fill(None);
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.entries.iter().all(Option::is_none)
    }
}

#[derive(Debug)]
struct OutstandingRequests {
    entries: [Option<RequestId>; MAX_OUTSTANDING_REQUESTS],
}

impl OutstandingRequests {
    const fn new() -> Self {
        Self {
            entries: [None; MAX_OUTSTANDING_REQUESTS],
        }
    }

    fn try_insert(&mut self, first_candidate: RequestId) -> Option<RequestId> {
        let slot = self.entries.iter().position(Option::is_none)?;
        let mut candidate = first_candidate;
        for _ in 0..MAX_OUTSTANDING_REQUESTS {
            if !self.contains(candidate) {
                *self.entries.get_mut(slot)? = Some(candidate);
                return Some(candidate);
            }
            candidate = candidate.wrapping_add(1);
        }
        None
    }

    fn contains(&self, request_id: RequestId) -> bool {
        self.entries.contains(&Some(request_id))
    }

    fn remove(&mut self, request_id: &RequestId) -> bool {
        let Some(slot) = self
            .entries
            .iter_mut()
            .find(|slot| **slot == Some(*request_id))
        else {
            return false;
        };
        *slot = None;
        true
    }

    fn retain(&mut self, mut keep: impl FnMut(RequestId) -> bool) {
        for slot in &mut self.entries {
            if slot.is_some_and(|request_id| !keep(request_id)) {
                *slot = None;
            }
        }
    }

    fn clear(&mut self) {
        self.entries.fill(None);
    }

    fn len(&self) -> usize {
        self.entries.iter().flatten().count()
    }

    fn available(&self) -> usize {
        MAX_OUTSTANDING_REQUESTS - self.len()
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.entries.iter().all(Option::is_none)
    }
}

/// Sole mutable authority for navigation and viewer-owned asynchronous work.
#[derive(Debug)]
pub struct ViewerModel {
    pub(crate) phase: NavigationPhase,
    pub(crate) scope: Option<PageScope>,
    pub(crate) current_uri: Option<AtpUri>,
    pub(crate) connection: ConnectionStatus,
    pub(crate) viewport: (u16, u16),
    pub(crate) content_height: u32,
    pub(crate) history: HistoryEntries,
    pub(crate) history_position: Option<usize>,
    pub(crate) focused: Option<crate::compositor::scene::NodeId>,
    subscriptions: SubscriptionOwnership,
    outstanding: OutstandingRequests,
    pending_subscriptions: SubscriptionOwnership,
    generation: u64,
    next_request_id: RequestId,
    next_history_id: HistoryId,
    reconnect_request: Option<RequestId>,
    history_commit: HistoryCommit,
    pending_history: Option<PendingHistoryActivation>,
    pending_history_commit: Option<PendingHistoryCommit>,
    pending_resize_projection: Option<PendingResizeProjection>,
    pressure_retry: Option<PressureRetry>,
}

impl ViewerModel {
    pub fn new(width: u16, height: u16) -> Self {
        Self {
            phase: NavigationPhase::Idle,
            scope: None,
            current_uri: None,
            connection: ConnectionStatus::Disconnected,
            viewport: (width, height),
            content_height: 0,
            history: HistoryEntries::new(),
            history_position: None,
            focused: None,
            subscriptions: SubscriptionOwnership::new(),
            outstanding: OutstandingRequests::new(),
            pending_subscriptions: SubscriptionOwnership::new(),
            generation: 0,
            next_request_id: 1,
            next_history_id: 1,
            reconnect_request: None,
            history_commit: HistoryCommit::Append,
            pending_history: None,
            pending_history_commit: None,
            pending_resize_projection: None,
            pressure_retry: None,
        }
    }

    pub const fn phase(&self) -> NavigationPhase {
        self.phase
    }

    pub fn scope(&self) -> Option<&PageScope> {
        self.scope.as_ref()
    }

    pub fn current_uri(&self) -> Option<&AtpUri> {
        self.current_uri.as_ref()
    }

    pub const fn connection(&self) -> ConnectionStatus {
        self.connection
    }

    pub const fn viewport(&self) -> (u16, u16) {
        self.viewport
    }

    pub const fn content_height(&self) -> u32 {
        self.content_height
    }

    pub const fn focused(&self) -> Option<crate::compositor::scene::NodeId> {
        self.focused
    }

    fn request(&mut self) -> Option<RequestId> {
        let id = self.outstanding.try_insert(self.next_request_id)?;
        self.next_request_id = id.wrapping_add(1);
        Some(id)
    }

    fn owns(&self, scope: &PageScope, request_id: RequestId) -> bool {
        self.scope.as_ref() == Some(scope) && self.outstanding.contains(request_id)
    }

    pub(crate) fn owns_operation(&self, owner: &OperationOwner) -> bool {
        self.owns(&owner.scope, owner.request_id)
    }

    pub(crate) fn try_scope_clone(&self) -> Option<Option<PageScope>> {
        if preparation_rejected() {
            return None;
        }
        self.scope
            .as_ref()
            .map(PageScope::try_clone)
            .transpose()
            .ok()
    }

    fn try_current_scope(&self) -> Option<PageScope> {
        self.try_scope_clone().flatten()
    }

    pub(crate) fn owns_resize_projection(
        &self,
        owner: &OperationOwner,
        width: u16,
        height: u16,
    ) -> bool {
        self.pending_resize_projection
            .as_ref()
            .is_some_and(|pending| {
                pending.owner == *owner && pending.width == width && pending.height == height
            })
    }

    pub fn control_token(&self) -> Option<ControlToken> {
        self.try_current_scope().map(|scope| ControlToken { scope })
    }

    pub fn history(&self) -> &[HistoryEntry] {
        &self.history
    }

    pub fn history_position(&self) -> Option<usize> {
        self.history_position
    }

    fn begin_navigation(
        &mut self,
        uri: AtpUri,
        origin: Origin,
        history_commit: HistoryCommit,
    ) -> Vec<ViewerEffect> {
        let origin_changed = self
            .scope
            .as_ref()
            .is_some_and(|scope| scope.origin != origin);
        let generation = self.generation.wrapping_add(1);
        let scope = PageScope { origin, generation };
        let Ok(model_scope) = (!preparation_rejected())
            .then(|| scope.try_clone())
            .transpose()
        else {
            return Vec::new();
        };
        let Some(model_scope) = model_scope else {
            return Vec::new();
        };
        let Ok(connect_scope) = (!preparation_rejected())
            .then(|| scope.try_clone())
            .transpose()
        else {
            return Vec::new();
        };
        let Some(connect_scope) = connect_scope else {
            return Vec::new();
        };
        let Ok(fetch_scope) = (!preparation_rejected())
            .then(|| scope.try_clone())
            .transpose()
        else {
            return Vec::new();
        };
        let Some(fetch_scope) = fetch_scope else {
            return Vec::new();
        };
        let Ok(current_uri) = (!preparation_rejected())
            .then(|| uri.try_clone())
            .transpose()
        else {
            return Vec::new();
        };
        let Some(current_uri) = current_uri else {
            return Vec::new();
        };
        let effect_capacity = 2 + usize::from(self.scope.is_some()) + usize::from(origin_changed);
        let mut effects = Vec::new();
        if preparation_rejected() || effects.try_reserve_exact(effect_capacity).is_err() {
            return Vec::new();
        }

        let previous_scope = self.scope.take();
        self.generation = generation;
        self.scope = Some(model_scope);
        self.current_uri = Some(current_uri);
        self.phase = NavigationPhase::Connecting;
        self.connection = ConnectionStatus::Connecting;
        self.focused = None;
        self.subscriptions.clear();
        self.pending_subscriptions.clear();
        self.outstanding.clear();
        self.reconnect_request = None;
        self.history_commit = history_commit;
        self.pending_history = None;
        self.pending_history_commit = None;
        self.pending_resize_projection = None;
        self.pressure_retry = None;
        let Some(request_id) = self.request() else {
            return Vec::new();
        };
        if let Some(previous_scope) = previous_scope {
            effects.push(ViewerEffect::RetirePageWork {
                scope: previous_scope,
            });
        }
        if origin_changed {
            effects.push(ViewerEffect::Close);
        }
        effects.push(ViewerEffect::Connect {
            owner: OperationOwner::new(connect_scope, request_id),
        });
        effects.push(ViewerEffect::Fetch {
            owner: OperationOwner::new(fetch_scope, request_id),
            uri,
        });
        effects
    }

    fn fail_owned_operation(
        &mut self,
        scope: PageScope,
        request_id: RequestId,
        message: String,
    ) -> Vec<ViewerEffect> {
        if !self.owns(&scope, request_id) {
            return Vec::new();
        }
        if let Some(pending) = self
            .pending_history
            .take_if(|pending| pending.target_generation == scope.generation)
        {
            if let Some(entry) = self.history.get_mut(pending.target) {
                entry.scope = pending.previous_entry_scope;
            }
            self.history_position = pending.previous_position;
            self.scope = pending.previous_scope;
            self.current_uri = pending.previous_uri;
            self.phase = pending.previous_phase;
            self.connection = pending.previous_connection;
            self.outstanding.clear();
            self.pending_subscriptions.clear();
            self.reconnect_request = None;
            return vec![
                ViewerEffect::RetirePageWork { scope },
                ViewerEffect::RenderTerminal,
            ];
        }
        self.phase = NavigationPhase::Failed;
        self.connection = ConnectionStatus::Disconnected;
        self.outstanding.clear();
        self.reconnect_request = None;
        self.subscriptions.clear();
        self.pending_subscriptions.clear();
        self.pending_history_commit = None;
        self.pending_resize_projection = None;
        vec![
            ViewerEffect::RetirePageWork { scope },
            ViewerEffect::ActivateErrorPage { message },
            ViewerEffect::RenderTerminal,
        ]
    }

    fn activate_history(&mut self, index: usize) -> Vec<ViewerEffect> {
        let Some(entry) = self.history.get(index) else {
            return Vec::new();
        };
        if self.history_position == Some(index) {
            return Vec::new();
        }
        if preparation_rejected() {
            return Vec::new();
        }
        let Ok(entry) = entry.try_clone() else {
            return Vec::new();
        };
        let previous_phase = self.phase;
        let previous_connection = self.connection;
        let previous_position = self.history_position;
        let origin_changed = self
            .scope
            .as_ref()
            .is_some_and(|scope| scope.origin != entry.scope.origin);
        let generation = self.generation.wrapping_add(1);
        let HistoryEntry {
            id,
            scope: prior_entry_copy,
            uri,
            retained_aml,
        } = entry;
        let scope = PageScope {
            origin: prior_entry_copy.origin,
            generation,
        };
        if preparation_rejected() {
            return Vec::new();
        }
        let Ok(model_scope) = scope.try_clone() else {
            return Vec::new();
        };
        if preparation_rejected() {
            return Vec::new();
        }
        let Ok(history_scope) = scope.try_clone() else {
            return Vec::new();
        };
        if preparation_rejected() {
            return Vec::new();
        }
        let Ok(entry_scope) = scope.try_clone() else {
            return Vec::new();
        };
        if preparation_rejected() {
            return Vec::new();
        }
        let Ok(owner_scope) = scope.try_clone() else {
            return Vec::new();
        };
        if preparation_rejected() {
            return Vec::new();
        }
        let Ok(current_uri) = uri.try_clone() else {
            return Vec::new();
        };
        if preparation_rejected() {
            return Vec::new();
        }
        let previous_scope_for_effect = match self.scope.as_ref().map(PageScope::try_clone) {
            Some(Ok(scope)) => Some(scope),
            Some(Err(_)) => return Vec::new(),
            None => None,
        };
        let effect_capacity =
            1 + usize::from(previous_scope_for_effect.is_some()) + usize::from(origin_changed);
        let mut effects = Vec::new();
        if preparation_rejected() || effects.try_reserve_exact(effect_capacity).is_err() {
            return Vec::new();
        }

        if self.history.get(index).is_none() {
            return Vec::new();
        }
        self.generation = generation;
        let previous_scope = self.scope.replace(model_scope);
        let previous_uri = self.current_uri.replace(current_uri);
        let previous_entry_scope = match self.history.get_mut(index) {
            Some(entry) => std::mem::replace(&mut entry.scope, history_scope),
            None => return Vec::new(),
        };
        self.phase = NavigationPhase::Parsing;
        self.connection = if origin_changed {
            ConnectionStatus::Disconnected
        } else {
            self.connection
        };
        self.focused = None;
        self.subscriptions.clear();
        self.pending_subscriptions.clear();
        self.outstanding.clear();
        self.reconnect_request = None;
        self.pending_history_commit = None;
        self.pending_resize_projection = None;
        let Some(request_id) = self.request() else {
            return Vec::new();
        };
        self.pending_history = Some(PendingHistoryActivation {
            target: index,
            target_generation: generation,
            previous_entry_scope,
            previous_position,
            previous_scope,
            previous_uri,
            previous_phase,
            previous_connection,
        });
        if let Some(previous_scope) = previous_scope_for_effect {
            effects.push(ViewerEffect::RetirePageWork {
                scope: previous_scope,
            });
        }
        if origin_changed {
            effects.push(ViewerEffect::Close);
        }
        effects.push(ViewerEffect::ActivateCachedHistory {
            owner: OperationOwner::new(owner_scope, request_id),
            entry: HistoryEntry {
                id,
                scope: entry_scope,
                uri,
                retained_aml,
            },
        });
        effects
    }

    pub fn reduce(&mut self, event: ViewerEvent) -> Vec<ViewerEffect> {
        match event {
            ViewerEvent::InitialNavigation { uri, origin }
            | ViewerEvent::Navigate { uri, origin } => {
                self.begin_navigation(uri, origin, HistoryCommit::Append)
            }
            ViewerEvent::Redirect { uri, origin } => {
                let history_commit = self.history_commit;
                self.begin_navigation(uri, origin, history_commit)
            }
            ViewerEvent::Reload => {
                let Some(uri) = self.current_uri.as_ref() else {
                    return Vec::new();
                };
                if preparation_rejected() {
                    return Vec::new();
                }
                let Ok(uri) = uri.try_clone() else {
                    return Vec::new();
                };
                if preparation_rejected() {
                    return Vec::new();
                }
                let Some(origin) = self.scope.as_ref().map(|scope| scope.origin.try_clone()) else {
                    return Vec::new();
                };
                let Ok(origin) = origin else {
                    return Vec::new();
                };
                self.begin_navigation(uri, origin, HistoryCommit::ReplaceCurrent)
            }
            ViewerEvent::Back => {
                let Some(position) = self.history_position else {
                    return Vec::new();
                };
                let Some(target) = position.checked_sub(1) else {
                    return Vec::new();
                };
                self.activate_history(target)
            }
            ViewerEvent::Forward => {
                let Some(position) = self.history_position else {
                    return Vec::new();
                };
                let target = position.saturating_add(1);
                self.activate_history(target)
            }
            ViewerEvent::JumpToHistory { index } => self.activate_history(index),
            ViewerEvent::FormSubmitted {
                uri,
                origin,
                path,
                form_data,
            } => {
                if preparation_rejected() {
                    return Vec::new();
                }
                let Ok(submit_uri) = uri.try_clone() else {
                    return Vec::new();
                };
                let mut effects = self.begin_navigation(uri, origin, HistoryCommit::Append);
                let Some(ViewerEffect::Fetch { owner, .. }) = effects.pop() else {
                    return effects;
                };
                effects.push(ViewerEffect::Submit {
                    owner,
                    uri: submit_uri,
                    path,
                    form_data,
                });
                effects
            }
            ViewerEvent::Connected { owner } if self.owns_operation(&owner) => {
                let OperationOwner { request_id, .. } = owner;
                self.connection = ConnectionStatus::Connected;
                if self.reconnect_request == Some(request_id) {
                    self.reconnect_request = None;
                    self.outstanding.remove(&request_id);
                    return Vec::new();
                }
                self.phase = NavigationPhase::Fetching;
                Vec::new()
            }
            ViewerEvent::ConnectionFailed { owner, message } if self.owns_operation(&owner) => {
                let OperationOwner { scope, request_id } = owner;
                self.connection = ConnectionStatus::Disconnected;
                if self.reconnect_request == Some(request_id) {
                    self.reconnect_request = None;
                    self.outstanding.remove(&request_id);
                    return vec![ViewerEffect::RenderTerminal];
                }
                self.fail_owned_operation(scope, request_id, message)
            }
            ViewerEvent::FetchCompleted { owner } if self.owns_operation(&owner) => {
                self.phase = NavigationPhase::Parsing;
                vec![ViewerEffect::Parse { owner }]
            }
            ViewerEvent::HistoryAdmissionRequested {
                owner,
                uri,
                retained_aml,
            } if self.owns_operation(&owner) => {
                if retained_aml.len() > MAX_HISTORY_AML_BYTES {
                    return self.fail_owned_operation(
                        owner.scope,
                        owner.request_id,
                        "history entry exceeds the retained AML limit".into(),
                    );
                }
                let replaceable = matches!(self.history_commit, HistoryCommit::ReplaceCurrent)
                    .then(|| self.history_position)
                    .flatten()
                    .and_then(|position| self.history.get(position))
                    .map(|entry| entry.id);
                let (id, replacing) = match replaceable {
                    Some(existing) => (existing, true),
                    None => (self.next_history_id, false),
                };
                let Ok(pending_owner) = owner.try_clone() else {
                    return self.fail_owned_operation(
                        owner.scope,
                        owner.request_id,
                        "history owner allocation rejected".into(),
                    );
                };
                let mut prepared_effects = Vec::new();
                if prepared_effects
                    .try_reserve_exact(MAX_HISTORY_ENTRIES.saturating_add(1))
                    .is_err()
                {
                    return self.fail_owned_operation(
                        owner.scope,
                        owner.request_id,
                        "history effect allocation rejected".into(),
                    );
                }
                self.pending_history_commit = Some(PendingHistoryCommit {
                    owner: pending_owner,
                    id,
                    uri,
                    retained_aml,
                    commit: self.history_commit,
                    prepared_effects,
                });
                vec![ViewerEffect::AdmitHistoryArtifact {
                    owner,
                    id,
                    replacing,
                }]
            }
            ViewerEvent::HistoryCommitted { owner, id }
                if self.owns_operation(&owner)
                    && self
                        .pending_history_commit
                        .as_ref()
                        .is_some_and(|pending| pending.owner == owner && pending.id == id) =>
            {
                let Some(pending) = self
                    .pending_history_commit
                    .take_if(|pending| pending.owner == owner && pending.id == id)
                else {
                    return Vec::new();
                };
                let PendingHistoryCommit {
                    owner: pending_owner,
                    id: _,
                    uri,
                    retained_aml,
                    commit,
                    mut prepared_effects,
                } = pending;
                let scope = pending_owner.scope;
                let mut removed_ids = ArrayVec::<HistoryId, MAX_HISTORY_ENTRIES>::new();
                // Resolve the replaceable slot once. The length guard and the
                // index were the same question asked twice; `get_mut` asks it
                // once and hands back the slot.
                let replace_slot = matches!(commit, HistoryCommit::ReplaceCurrent)
                    .then(|| self.history_position)
                    .flatten()
                    .filter(|&position| {
                        self.history
                            .get(position)
                            .is_some_and(|entry| entry.id == id)
                    });
                match replace_slot {
                    Some(position) => {
                        removed_ids.push(id);
                        if let Some(slot) = self.history.get_mut(position) {
                            *slot = HistoryEntry {
                                id,
                                scope,
                                uri,
                                retained_aml,
                            };
                        }
                    }
                    None => {
                        if let Some(position) = self.history_position {
                            removed_ids.extend(
                                self.history
                                    .get(position.saturating_add(1)..)
                                    .unwrap_or_default()
                                    .iter()
                                    .map(|entry| entry.id),
                            );
                            self.history.truncate(position.saturating_add(1));
                        } else {
                            removed_ids.extend(self.history.iter().map(|entry| entry.id));
                            self.history.clear();
                        }
                        if self.history.len() == MAX_HISTORY_ENTRIES {
                            let removed = self.history.remove(0);
                            removed_ids.push(removed.id);
                            self.history_position = self
                                .history_position
                                .map(|position| position.saturating_sub(1));
                        }
                        self.next_history_id = self.next_history_id.wrapping_add(1);
                        self.history.push(HistoryEntry {
                            id,
                            scope,
                            uri,
                            retained_aml,
                        });
                        self.history_position = self.history.len().checked_sub(1);
                    }
                }
                self.history_commit = HistoryCommit::Append;
                let mut retained_bytes = self
                    .history
                    .iter()
                    .map(|entry| entry.retained_aml.len())
                    .sum::<usize>();
                while self.history.len() > 1
                    && (self.history.len() > MAX_HISTORY_ENTRIES
                        || retained_bytes > MAX_HISTORY_AML_BYTES)
                {
                    let remove_index = usize::from(self.history_position == Some(0));
                    retained_bytes = retained_bytes.saturating_sub(
                        self.history
                            .get(remove_index)
                            .map_or(0, |entry| entry.retained_aml.len()),
                    );
                    let removed = self.history.remove(remove_index);
                    removed_ids.push(removed.id);
                    if let Some(position) = self.history_position
                        && remove_index < position
                    {
                        self.history_position = Some(position - 1);
                    }
                }
                for removed_id in removed_ids {
                    if !prepared_effects.iter().any(|effect| {
                        matches!(effect, ViewerEffect::ReleaseHistoryArtifact { id } if *id == removed_id)
                    }) {
                        prepared_effects.push(ViewerEffect::ReleaseHistoryArtifact {
                            id: removed_id,
                        });
                    }
                }
                prepared_effects.push(ViewerEffect::InstallHistoryArtifact { owner, id });
                prepared_effects
            }
            ViewerEvent::FetchFailed { owner, message }
            | ViewerEvent::ParseFailed { owner, message }
            | ViewerEvent::LayoutFailed { owner, message } => {
                self.fail_owned_operation(owner.scope, owner.request_id, message)
            }
            ViewerEvent::ErrorPageActivated { content_height } => {
                self.phase = NavigationPhase::Failed;
                self.content_height = content_height;
                self.focused = None;
                self.subscriptions.clear();
                self.pending_subscriptions.clear();
                self.outstanding.clear();
                self.reconnect_request = None;
                self.pressure_retry = None;
                self.pending_history_commit = None;
                self.pending_resize_projection = None;
                Vec::new()
            }
            ViewerEvent::PresentationFailed { message, retry } => {
                if retry
                    .as_ref()
                    .is_some_and(|retry| !retry.belongs_to(self.scope.as_ref()))
                {
                    return Vec::new();
                }
                if let Some(PressureRetry::ResizeProjection {
                    owner,
                    width,
                    height,
                }) = retry.as_ref()
                    && !self
                        .pending_resize_projection
                        .as_ref()
                        .is_some_and(|pending| {
                            pending.owner == *owner
                                && pending.width == *width
                                && pending.height == *height
                        })
                {
                    return Vec::new();
                }
                self.pressure_retry = retry;
                vec![ViewerEffect::EvictResource { message }]
            }
            ViewerEvent::ResourceEvictionCompleted { message, evicted } => {
                if evicted {
                    let mut effects = Vec::new();
                    if let Some(retry) = self.pressure_retry.take() {
                        effects.push(retry.into_effect());
                    }
                    effects.push(ViewerEffect::RenderTerminal);
                    effects
                } else {
                    vec![ViewerEffect::EvictHistory { message }]
                }
            }
            ViewerEvent::HistoryEvictionRequested { message } => {
                let current = self
                    .history_position
                    .and_then(|position| self.history.get(position))
                    .map(|entry| entry.id);
                let pending = self
                    .pending_history
                    .as_ref()
                    .and_then(|activation| self.history.get(activation.target))
                    .map(|entry| entry.id);
                if let Some(index) = self
                    .history
                    .iter()
                    .position(|entry| Some(entry.id) != current && Some(entry.id) != pending)
                {
                    let removed = self.history.remove(index);
                    if let Some(position) = self.history_position
                        && index < position
                    {
                        self.history_position = Some(position - 1);
                    }
                    let mut effects = vec![ViewerEffect::ReleaseHistoryArtifact { id: removed.id }];
                    if let Some(retry) = self.pressure_retry.take() {
                        effects.push(retry.into_effect());
                    }
                    effects.push(ViewerEffect::RenderTerminal);
                    return effects;
                }
                if self.pressure_retry.as_ref().is_some_and(|retry| {
                    matches!(
                        retry,
                        PressureRetry::Parse { .. }
                            | PressureRetry::PrepareLayout { .. }
                            | PressureRetry::HistoryArtifact { .. }
                            | PressureRetry::ActivateCachedHistory { .. }
                    )
                }) {
                    let Some(
                        PressureRetry::Parse { owner }
                        | PressureRetry::PrepareLayout { owner }
                        | PressureRetry::HistoryArtifact { owner, .. }
                        | PressureRetry::ActivateCachedHistory { owner, .. },
                    ) = self.pressure_retry.take()
                    else {
                        unreachable!("matched an owned pressure retry")
                    };
                    return self.fail_owned_operation(owner.scope, owner.request_id, message);
                }
                let retired_scope = if self.scope.is_some() {
                    let Some(scope) = self.try_current_scope() else {
                        return Vec::new();
                    };
                    Some(scope)
                } else {
                    None
                };
                self.pressure_retry = None;
                self.phase = NavigationPhase::Failed;
                self.connection = ConnectionStatus::Disconnected;
                self.focused = None;
                self.subscriptions.clear();
                self.pending_subscriptions.clear();
                self.outstanding.clear();
                self.reconnect_request = None;
                self.pending_resize_projection = None;
                let mut effects = Vec::new();
                if let Some(scope) = retired_scope {
                    effects.push(ViewerEffect::RetirePageWork { scope });
                }
                effects.extend([
                    ViewerEffect::ActivateErrorPage { message },
                    ViewerEffect::RenderTerminal,
                ]);
                effects
            }
            ViewerEvent::ParseCompleted { owner } if self.owns_operation(&owner) => {
                self.phase = NavigationPhase::Layout;
                vec![ViewerEffect::PrepareLayout { owner }]
            }
            ViewerEvent::WasmDependenciesDiscovered { owner, paths }
                if self.owns_operation(&owner) =>
            {
                if paths.len() > self.outstanding.available() {
                    return self.fail_owned_operation(
                        owner.scope,
                        owner.request_id,
                        "WASM dependency batch exceeded the outstanding request limit".into(),
                    );
                }
                let mut effects = Vec::new();
                let mut scopes = Vec::new();
                if effects.try_reserve_exact(paths.len()).is_err()
                    || scopes.try_reserve_exact(paths.len()).is_err()
                {
                    return self.fail_owned_operation(
                        owner.scope,
                        owner.request_id,
                        "WASM dependency batch exhausted client memory".into(),
                    );
                }
                for _ in 0..paths.len() {
                    let Ok(scope) = owner.scope.try_clone() else {
                        return self.fail_owned_operation(
                            owner.scope,
                            owner.request_id,
                            "WASM dependency owner exhausted client memory".into(),
                        );
                    };
                    scopes.push(scope);
                }
                for (path, scope) in paths.into_iter().zip(scopes) {
                    // The batch is preflighted, so a slot should be free.
                    // If one is not, emit nothing for this path rather than
                    // aborting: the batch is remote-sized.
                    let Some(request_id) = self.request() else {
                        break;
                    };
                    effects.push(ViewerEffect::LoadWasm {
                        owner: OperationOwner::new(scope, request_id),
                        path,
                    });
                }
                effects
            }
            ViewerEvent::LayoutPrepared {
                owner,
                content_height,
            } if self.owns_operation(&owner) => {
                self.content_height = content_height;
                vec![ViewerEffect::ActivateLayout { owner }]
            }
            ViewerEvent::ResizeProjectionPrepared {
                owner,
                content_height,
            } if self.owns_operation(&owner)
                && self
                    .pending_resize_projection
                    .as_ref()
                    .is_some_and(|pending| pending.owner == owner) =>
            {
                self.pending_resize_projection = None;
                self.content_height = content_height;
                vec![ViewerEffect::ActivateLayout { owner }]
            }
            ViewerEvent::LayoutActivated { owner } if self.owns_operation(&owner) => {
                let OperationOwner { scope, request_id } = owner;
                self.phase = NavigationPhase::Ready;
                if self
                    .pending_history
                    .as_ref()
                    .is_some_and(|pending| pending.target_generation == scope.generation)
                    && let Some(pending) = self.pending_history.take()
                {
                    self.history_position = Some(pending.target);
                }
                self.outstanding.remove(&request_id);
                vec![ViewerEffect::RenderTerminal]
            }
            ViewerEvent::WasmRequested { path } => {
                self.try_current_scope().map_or_else(Vec::new, |scope| {
                    let Some(request_id) = self.request() else {
                        return Vec::new();
                    };
                    vec![ViewerEffect::LoadWasm {
                        owner: OperationOwner::new(scope, request_id),
                        path,
                    }]
                })
            }
            ViewerEvent::SubscriptionCompleted { owner, region } if self.owns_operation(&owner) => {
                let request_id = owner.request_id;
                if !self.pending_subscriptions.remove_exact(request_id, region) {
                    return Vec::new();
                }
                let inserted = self.subscriptions.insert(request_id, region);
                debug_assert!(inserted);
                self.outstanding.remove(&request_id);
                Vec::new()
            }
            ViewerEvent::SubscriptionFailed { owner } if self.owns_operation(&owner) => {
                let request_id = owner.request_id;
                self.pending_subscriptions.remove_request(request_id);
                self.outstanding.remove(&request_id);
                Vec::new()
            }
            ViewerEvent::SubscribeRequested { region }
                if self.connection == ConnectionStatus::Connected =>
            {
                if self.subscriptions.len() + self.pending_subscriptions.len()
                    >= crate::client::MAX_ACTIVE_SUBSCRIPTIONS
                {
                    return Vec::new();
                }
                self.try_current_scope().map_or_else(Vec::new, |scope| {
                    let Some(request_id) = self.request() else {
                        return Vec::new();
                    };
                    let inserted = self.pending_subscriptions.insert(request_id, region);
                    debug_assert!(inserted);
                    vec![ViewerEffect::Subscribe {
                        owner: OperationOwner::new(scope, request_id),
                        region,
                    }]
                })
            }
            ViewerEvent::DeferredNavigationRequested => {
                self.try_current_scope().map_or_else(Vec::new, |scope| {
                    let Some(request_id) = self.request() else {
                        return Vec::new();
                    };
                    vec![ViewerEffect::DeferNavigation {
                        owner: OperationOwner::new(scope, request_id),
                    }]
                })
            }
            ViewerEvent::DeferredNavigationCompleted { owner } if self.owns_operation(&owner) => {
                self.outstanding.remove(&owner.request_id);
                vec![ViewerEffect::ResumeNavigation { owner }]
            }
            ViewerEvent::DeferredNavigationRejected { owner } if self.owns_operation(&owner) => {
                self.outstanding.remove(&owner.request_id);
                Vec::new()
            }
            ViewerEvent::SubscriptionsRetired { scope } if self.scope.as_ref() == Some(&scope) => {
                self.subscriptions.clear();
                self.outstanding
                    .retain(|request_id| !self.pending_subscriptions.contains_request(request_id));
                self.pending_subscriptions.clear();
                vec![ViewerEffect::RetireSubscriptions { scope }]
            }
            ViewerEvent::LiveUpdate { owner, region }
                if self.scope.as_ref() == Some(&owner.scope)
                    && self.subscriptions.contains(owner.request_id, region) =>
            {
                vec![ViewerEffect::ApplyUpdate { owner, region }]
            }
            ViewerEvent::WasmPrepared { owner } if self.owns_operation(&owner) => {
                vec![ViewerEffect::ActivateWasm { owner }]
            }
            ViewerEvent::WasmActivated { owner } if self.owns_operation(&owner) => {
                self.outstanding.remove(&owner.request_id);
                vec![ViewerEffect::RenderTerminal]
            }
            ViewerEvent::WasmRejected { owner } if self.owns_operation(&owner) => {
                self.outstanding.remove(&owner.request_id);
                vec![ViewerEffect::RenderTerminal]
            }
            ViewerEvent::Resize { width, height } => {
                let (effect_scope, pending_scope) = match self.try_scope_clone() {
                    None => return Vec::new(),
                    Some(None) => {
                        self.viewport = (width, height);
                        return Vec::new();
                    }
                    Some(Some(effect_scope)) => {
                        let Some(pending_scope) = self.try_current_scope() else {
                            return Vec::new();
                        };
                        (effect_scope, pending_scope)
                    }
                };
                self.viewport = (width, height);
                if let Some(pending) = self.pending_resize_projection.take() {
                    self.outstanding.remove(&pending.owner.request_id);
                    if self.pressure_retry.as_ref().is_some_and(|retry| {
                        matches!(retry, PressureRetry::ResizeProjection { owner, .. } if owner == &pending.owner)
                    }) {
                        self.pressure_retry = None;
                    }
                }
                let Some(request_id) = self.request() else {
                    return Vec::new();
                };
                self.pending_resize_projection = Some(PendingResizeProjection {
                    owner: OperationOwner::new(pending_scope, request_id),
                    width,
                    height,
                });
                vec![ViewerEffect::PrepareResizeProjection {
                    owner: OperationOwner::new(effect_scope, request_id),
                    width,
                    height,
                }]
            }
            ViewerEvent::Input { token, value } if self.scope.as_ref() == Some(&token.scope) => {
                vec![ViewerEffect::ProjectInput { token, value }]
            }
            ViewerEvent::FocusChanged { token, focused }
                if self.scope.as_ref() == Some(&token.scope) =>
            {
                self.focused = focused;
                vec![ViewerEffect::ProjectFocus { token, focused }]
            }
            ViewerEvent::PresentationRequested => vec![ViewerEffect::RenderTerminal],
            ViewerEvent::PresentationActionRequested { scope, action } if self.scope == scope => {
                vec![ViewerEffect::ApplyPresentationAction { scope, action }]
            }
            ViewerEvent::Timer
                if self.phase == NavigationPhase::Ready
                    || (self.phase == NavigationPhase::Idle && self.scope.is_none()) =>
            {
                match self.try_scope_clone() {
                    None => Vec::new(),
                    Some(None) => vec![ViewerEffect::TickWasm { owner: None }],
                    Some(Some(scope)) => {
                        let Some(request_id) = self.request() else {
                            return Vec::new();
                        };
                        vec![ViewerEffect::TickWasm {
                            owner: Some(OperationOwner::new(scope, request_id)),
                        }]
                    }
                }
            }
            ViewerEvent::TransportLost { origin }
                if self
                    .scope
                    .as_ref()
                    .is_some_and(|scope| scope.origin == origin) =>
            {
                if self.connection == ConnectionStatus::Connecting {
                    return Vec::new();
                }
                let Some(scope) = self.try_current_scope() else {
                    return Vec::new();
                };
                let Some(request_id) = self.request() else {
                    return Vec::new();
                };
                self.connection = ConnectionStatus::Connecting;
                self.reconnect_request = Some(request_id);
                vec![ViewerEffect::Connect {
                    owner: OperationOwner::new(scope, request_id),
                }]
            }
            ViewerEvent::Shutdown => {
                self.phase = NavigationPhase::ShuttingDown;
                self.subscriptions.clear();
                self.pending_subscriptions.clear();
                self.outstanding.clear();
                self.reconnect_request = None;
                vec![ViewerEffect::Close, ViewerEffect::RestoreTerminal]
            }
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dustnet_core::protocol::origin::TransportSecurity;

    fn subscription_region(index: usize) -> SubscriptionRegionKey {
        SubscriptionRegionKey::from_placed_index(index).unwrap()
    }

    fn target(value: &str) -> (AtpUri, Origin) {
        let uri = AtpUri::parse(value).unwrap();
        let origin = Origin::from_uri(&uri, TransportSecurity::VerifiedTls).unwrap();
        (uri, origin)
    }

    fn seed_active_page(model: &mut ViewerModel, scope: PageScope, content_height: u32) {
        model.generation = model.generation.max(scope.generation);
        model.scope = Some(scope);
        model.phase = NavigationPhase::Ready;
        model.connection = ConnectionStatus::Connected;
        model.content_height = content_height;
        model.focused = None;
        model.subscriptions.clear();
        model.pending_subscriptions.clear();
        model.outstanding.clear();
        model.reconnect_request = None;
        model.pending_history = None;
    }

    #[test]
    fn history_entry_try_clone_fallibly_owns_every_payload() {
        let (uri, origin) = target("atp://one.example/cached");
        let mut retained_aml = String::with_capacity(1024);
        retained_aml.push_str("[page title=Cached][/page]");
        let original_ptr = retained_aml.as_ptr();
        let entry = HistoryEntry {
            id: 7,
            scope: PageScope {
                origin,
                generation: 3,
            },
            uri,
            retained_aml,
        };

        let cloned = entry.try_clone().unwrap();

        assert_eq!(cloned, entry);
        assert_eq!(entry.retained_aml.as_ptr(), original_ptr);
        assert_ne!(cloned.retained_aml.as_ptr(), original_ptr);
    }

    fn admit_history(
        model: &mut ViewerModel,
        owner: OperationOwner,
        uri: AtpUri,
        retained_aml: String,
    ) -> Vec<ViewerEffect> {
        let effects = model.reduce(ViewerEvent::HistoryAdmissionRequested {
            owner: owner.clone(),
            uri,
            retained_aml,
        });
        let id = match effects.as_slice() {
            [
                ViewerEffect::AdmitHistoryArtifact {
                    owner: admitted,
                    id,
                    ..
                },
            ] if admitted == &owner => *id,
            _ => panic!("expected exact history admission"),
        };
        model.reduce(ViewerEvent::HistoryCommitted { owner, id })
    }

    #[test]
    fn navigation_retires_page_owned_state_and_stale_completions() {
        let mut model = ViewerModel::new(80, 24);
        let (first_uri, first_origin) = target("atp://one.example/");
        let first = model.reduce(ViewerEvent::Navigate {
            uri: first_uri,
            origin: first_origin,
        });
        let (old_scope, old_request) = match &first[0] {
            ViewerEffect::Connect { owner } => (owner.scope.clone(), owner.request_id),
            _ => panic!("expected connect"),
        };
        assert!(
            model
                .subscriptions
                .insert(old_request, subscription_region(0))
        );
        let (second_uri, second_origin) = target("atp://two.example/");
        let effects = model.reduce(ViewerEvent::Navigate {
            uri: second_uri,
            origin: second_origin,
        });
        assert!(matches!(effects[0], ViewerEffect::RetirePageWork { .. }));
        assert!(matches!(effects[1], ViewerEffect::Close));
        assert!(model.subscriptions.is_empty());
        assert!(
            model
                .reduce(ViewerEvent::FetchCompleted {
                    owner: OperationOwner::new(old_scope, old_request),
                })
                .is_empty()
        );
    }

    #[test]
    fn presentation_requires_a_reducer_issued_render_effect() {
        let mut model = ViewerModel::new(80, 24);
        assert_eq!(
            model.reduce(ViewerEvent::PresentationRequested),
            vec![ViewerEffect::RenderTerminal]
        );
    }

    #[test]
    fn stale_preparations_and_control_tokens_cannot_mutate_the_active_page() {
        let mut model = ViewerModel::new(80, 24);
        let (first_uri, first_origin) = target("atp://one.example/");
        let first = model.reduce(ViewerEvent::Navigate {
            uri: first_uri,
            origin: first_origin,
        });
        let old_owner = match &first[0] {
            ViewerEffect::Connect { owner } => owner.clone(),
            _ => panic!("expected connect"),
        };
        let old_token = model.control_token().unwrap();

        let (second_uri, second_origin) = target("atp://two.example/");
        model.reduce(ViewerEvent::Navigate {
            uri: second_uri,
            origin: second_origin,
        });
        let active_scope = model.scope.clone();

        assert!(
            model
                .reduce(ViewerEvent::LayoutPrepared {
                    owner: old_owner.clone(),
                    content_height: 999,
                })
                .is_empty()
        );
        assert!(
            model
                .reduce(ViewerEvent::WasmPrepared {
                    owner: old_owner.clone(),
                })
                .is_empty()
        );
        model.reduce(ViewerEvent::Input {
            token: old_token.clone(),
            value: "stale".into(),
        });
        model.reduce(ViewerEvent::FocusChanged {
            token: old_token,
            focused: Some(crate::compositor::scene::NodeId::default()),
        });
        assert!(
            model
                .reduce(ViewerEvent::PresentationActionRequested {
                    scope: Some(old_owner.scope),
                    action: PresentationAction::ClearFocus,
                })
                .is_empty()
        );

        assert_eq!(model.scope, active_scope);
        assert_eq!(model.content_height, 0);
        assert!(model.focused.is_none());
    }

    #[test]
    fn focus_projection_preserves_exact_scoped_node_identity() {
        let mut model = ViewerModel::new(80, 24);
        let (uri, origin) = target("atp://one.example/");
        model.reduce(ViewerEvent::Navigate { uri, origin });
        let token = model.control_token().unwrap();
        let focused = crate::compositor::scene::NodeId::default();

        assert_eq!(
            model.reduce(ViewerEvent::FocusChanged {
                token: token.clone(),
                focused: Some(focused),
            }),
            vec![ViewerEffect::ProjectFocus {
                token,
                focused: Some(focused),
            }]
        );
        assert_eq!(model.focused, Some(focused));
    }

    #[test]
    fn same_origin_disconnect_reconnects_and_replays_current_subscriptions() {
        let mut model = ViewerModel::new(80, 24);
        let (uri, origin) = target("atp://one.example/");
        let effects = model.reduce(ViewerEvent::Navigate { uri, origin });
        let (scope, initial_request) = match &effects[0] {
            ViewerEffect::Connect { owner } => (owner.scope.clone(), owner.request_id),
            _ => panic!("expected initial connect"),
        };
        model.reduce(ViewerEvent::Connected {
            owner: OperationOwner::new(scope.clone(), initial_request),
        });
        model.reduce(ViewerEvent::LayoutPrepared {
            owner: OperationOwner::new(scope.clone(), initial_request),
            content_height: 40,
        });
        model.reduce(ViewerEvent::LayoutActivated {
            owner: OperationOwner::new(scope.clone(), initial_request),
        });
        let clock = subscription_region(0);
        assert!(model.subscriptions.insert(77, clock));

        let reconnect = model.reduce(ViewerEvent::TransportLost {
            origin: scope.origin.clone(),
        });
        let reconnect_request = match &reconnect[0] {
            ViewerEffect::Connect { owner } => {
                assert_eq!(&owner.scope, &scope);
                owner.request_id
            }
            _ => panic!("expected reconnect"),
        };
        let replay = model.reduce(ViewerEvent::Connected {
            owner: OperationOwner::new(scope.clone(), reconnect_request),
        });
        assert!(replay.is_empty());
        assert!(model.subscriptions.contains(77, clock));
        assert_eq!(model.phase, NavigationPhase::Ready);
        assert_eq!(model.connection, ConnectionStatus::Connected);
    }

    #[test]
    fn transport_loss_for_another_security_partition_is_ignored() {
        let mut model = ViewerModel::new(80, 24);
        let (uri, origin) = target("atp://one.example/");
        model.reduce(ViewerEvent::Navigate { uri, origin });
        let (_, other) = target("atp://two.example/");
        assert!(
            model
                .reduce(ViewerEvent::TransportLost { origin: other })
                .is_empty()
        );
    }

    #[test]
    fn subscriptions_are_effect_owned_and_failures_release_requests() {
        let mut model = ViewerModel::new(80, 24);
        let (_, origin) = target("atp://one.example/");
        let scope = PageScope {
            origin,
            generation: 7,
        };
        seed_active_page(&mut model, scope.clone(), 24);
        let effects = model.reduce(ViewerEvent::SubscribeRequested {
            region: subscription_region(0),
        });
        let request_id = match &effects[0] {
            ViewerEffect::Subscribe { owner, region } => {
                assert_eq!(&owner.scope, &scope);
                assert_eq!(*region, subscription_region(0));
                owner.request_id
            }
            _ => panic!("expected subscribe effect"),
        };
        assert!(
            model
                .reduce(ViewerEvent::SubscriptionCompleted {
                    owner: OperationOwner::new(scope.clone(), request_id),
                    region: subscription_region(1),
                })
                .is_empty()
        );
        assert!(model.outstanding.contains(request_id));
        assert!(
            model
                .pending_subscriptions
                .contains(request_id, subscription_region(0))
        );
        model.reduce(ViewerEvent::SubscriptionFailed {
            owner: OperationOwner::new(scope, request_id),
        });
        assert!(model.outstanding.is_empty());
        assert!(model.subscriptions.is_empty());
    }

    #[test]
    fn deferred_navigation_completion_requires_its_exact_page_owner() {
        let mut model = ViewerModel::new(80, 24);
        let (_, origin) = target("atp://one.example/");
        let scope = PageScope {
            origin,
            generation: 7,
        };
        seed_active_page(&mut model, scope.clone(), 24);
        let request = model.reduce(ViewerEvent::DeferredNavigationRequested);
        let request_id = match request.as_slice() {
            [ViewerEffect::DeferNavigation { owner }] => {
                assert_eq!(&owner.scope, &scope);
                owner.request_id
            }
            _ => panic!("expected deferred-navigation owner"),
        };
        assert!(matches!(
            model.reduce(ViewerEvent::DeferredNavigationCompleted {
                owner: OperationOwner::new(scope.clone(), request_id),
            })[..],
            [ViewerEffect::ResumeNavigation { .. }]
        ));

        let second = model.reduce(ViewerEvent::DeferredNavigationRequested);
        let stale_request = match second[0] {
            ViewerEffect::DeferNavigation { ref owner } => owner.request_id,
            _ => unreachable!(),
        };
        let stale_scope = scope.clone();
        let current_scope = PageScope {
            origin: scope.origin,
            generation: 8,
        };
        seed_active_page(&mut model, current_scope, 24);
        assert!(
            model
                .reduce(ViewerEvent::DeferredNavigationCompleted {
                    owner: OperationOwner::new(stale_scope, stale_request),
                })
                .is_empty()
        );
    }

    #[test]
    fn live_updates_require_current_scope_and_owned_subscription() {
        let mut model = ViewerModel::new(80, 24);
        let (_, origin) = target("atp://one.example/");
        let scope = PageScope {
            origin,
            generation: 9,
        };
        seed_active_page(&mut model, scope.clone(), 24);
        let clock = subscription_region(0);
        assert!(model.subscriptions.insert(17, clock));
        assert!(
            model
                .reduce(ViewerEvent::LiveUpdate {
                    owner: OperationOwner::new(scope.clone(), 16),
                    region: clock,
                })
                .is_empty()
        );
        assert!(matches!(
            model.reduce(ViewerEvent::LiveUpdate {
                owner: OperationOwner::new(scope.clone(), 17),
                region: clock,
            })[..],
            [ViewerEffect::ApplyUpdate { ref owner, .. }] if owner.request_id == 17
        ));
        assert!(
            model
                .reduce(ViewerEvent::LiveUpdate {
                    owner: OperationOwner::new(scope.clone(), 17),
                    region: subscription_region(1),
                })
                .is_empty()
        );
        let stale = PageScope {
            origin: scope.origin,
            generation: 8,
        };
        assert!(
            model
                .reduce(ViewerEvent::LiveUpdate {
                    owner: OperationOwner::new(stale, 18),
                    region: clock,
                })
                .is_empty()
        );
    }

    #[test]
    fn subscription_request_bound_is_atomic_and_does_not_advance_ids() {
        let mut model = ViewerModel::new(80, 24);
        let (_, origin) = target("atp://one.example/");
        seed_active_page(
            &mut model,
            PageScope {
                origin,
                generation: 10,
            },
            24,
        );
        for index in 0..crate::client::MAX_ACTIVE_SUBSCRIPTIONS {
            assert!(matches!(
                model.reduce(ViewerEvent::SubscribeRequested {
                    region: subscription_region(index),
                })[..],
                [ViewerEffect::Subscribe { .. }]
            ));
        }
        let next_request_id = model.next_request_id;
        assert!(
            model
                .reduce(ViewerEvent::SubscribeRequested {
                    region: subscription_region(crate::client::MAX_ACTIVE_SUBSCRIPTIONS),
                })
                .is_empty()
        );
        assert_eq!(model.next_request_id, next_request_id);
        assert_eq!(
            model.pending_subscriptions.len(),
            crate::client::MAX_ACTIVE_SUBSCRIPTIONS
        );
    }

    #[test]
    fn outstanding_request_bound_is_atomic_and_reuses_released_slots() {
        let mut model = ViewerModel::new(80, 24);
        let (_, origin) = target("atp://one.example/");
        let scope = PageScope {
            origin,
            generation: 11,
        };
        seed_active_page(&mut model, scope.clone(), 24);

        let mut first_owner = None;
        for index in 0..MAX_OUTSTANDING_REQUESTS {
            let effects = model.reduce(ViewerEvent::WasmRequested {
                path: format!("/effect-{index}.wasm"),
            });
            let [ViewerEffect::LoadWasm { owner, .. }] = effects.as_slice() else {
                panic!("request {index} was not admitted");
            };
            first_owner.get_or_insert_with(|| owner.clone());
        }
        assert_eq!(model.outstanding.len(), MAX_OUTSTANDING_REQUESTS);

        let next_request_id = model.next_request_id;
        assert!(
            model
                .reduce(ViewerEvent::WasmRequested {
                    path: "/rejected.wasm".into(),
                })
                .is_empty()
        );
        assert_eq!(model.next_request_id, next_request_id);
        assert_eq!(model.outstanding.len(), MAX_OUTSTANDING_REQUESTS);

        assert!(
            model
                .reduce(ViewerEvent::TransportLost {
                    origin: scope.origin.clone(),
                })
                .is_empty()
        );
        assert_eq!(model.connection, ConnectionStatus::Connected);
        assert_eq!(model.reconnect_request, None);
        assert_eq!(model.next_request_id, next_request_id);

        model.reduce(ViewerEvent::WasmRejected {
            owner: first_owner.unwrap(),
        });
        assert_eq!(model.outstanding.len(), MAX_OUTSTANDING_REQUESTS - 1);
        assert!(matches!(
            model.reduce(ViewerEvent::WasmRequested {
                path: "/replacement.wasm".into(),
            })[..],
            [ViewerEffect::LoadWasm { ref owner, .. }] if owner.request_id == next_request_id
        ));
        assert_eq!(model.outstanding.len(), MAX_OUTSTANDING_REQUESTS);
        assert_eq!(model.next_request_id, next_request_id.wrapping_add(1));
    }

    #[test]
    fn wasm_dependency_batch_rejection_is_all_or_nothing() {
        let mut model = ViewerModel::new(80, 24);
        let (_, origin) = target("atp://one.example/");
        seed_active_page(
            &mut model,
            PageScope {
                origin,
                generation: 12,
            },
            24,
        );
        let source_owner = match model.reduce(ViewerEvent::WasmRequested {
            path: "/source.wasm".into(),
        })[0]
        {
            ViewerEffect::LoadWasm { ref owner, .. } => owner.clone(),
            _ => panic!("expected source request"),
        };
        while model.outstanding.available() > 1 {
            assert!(model.request().is_some());
        }
        let next_request_id = model.next_request_id;

        let rejected = model.reduce(ViewerEvent::WasmDependenciesDiscovered {
            owner: source_owner,
            paths: vec!["/one.wasm".into(), "/two.wasm".into()],
        });
        assert!(matches!(
            rejected.as_slice(),
            [
                ViewerEffect::RetirePageWork { .. },
                ViewerEffect::ActivateErrorPage { .. },
                ViewerEffect::RenderTerminal,
            ]
        ));
        assert_eq!(model.outstanding.len(), 0);
        assert_eq!(model.next_request_id, next_request_id);
    }

    #[test]
    fn subscription_retirement_preserves_unrelated_page_requests() {
        let mut model = ViewerModel::new(80, 24);
        let (_, origin) = target("atp://one.example/");
        let scope = PageScope {
            origin,
            generation: 13,
        };
        seed_active_page(&mut model, scope.clone(), 24);
        let wasm_request = match model.reduce(ViewerEvent::WasmRequested {
            path: "/effect.wasm".into(),
        })[0]
        {
            ViewerEffect::LoadWasm { ref owner, .. } => owner.request_id,
            _ => panic!("expected WASM request"),
        };
        let subscription_request = match model.reduce(ViewerEvent::SubscribeRequested {
            region: subscription_region(0),
        })[0]
        {
            ViewerEffect::Subscribe { ref owner, .. } => owner.request_id,
            _ => panic!("expected subscription request"),
        };

        model.reduce(ViewerEvent::SubscriptionsRetired {
            scope: scope.clone(),
        });

        assert!(model.outstanding.contains(wasm_request));
        assert_eq!(model.scope.as_ref(), Some(&scope));
        assert!(!model.outstanding.contains(subscription_request));
        assert!(model.pending_subscriptions.is_empty());
        assert!(model.subscriptions.is_empty());
    }

    #[test]
    fn redirect_and_reload_issue_fresh_model_owned_scopes() {
        let mut model = ViewerModel::new(80, 24);
        let (first_uri, first_origin) = target("atp://one.example/start");
        model.reduce(ViewerEvent::InitialNavigation {
            uri: first_uri,
            origin: first_origin,
        });
        let first_scope = model.scope.clone().unwrap();

        let (redirect_uri, redirect_origin) = target("atp://one.example/final");
        let redirect = model.reduce(ViewerEvent::Redirect {
            uri: redirect_uri,
            origin: redirect_origin,
        });
        let redirect_scope = model.scope.clone().unwrap();
        assert!(redirect_scope.generation > first_scope.generation);
        assert!(matches!(
            redirect[0],
            ViewerEffect::RetirePageWork { ref scope } if scope == &first_scope
        ));

        let reload = model.reduce(ViewerEvent::Reload);
        assert!(model.scope.as_ref().unwrap().generation > redirect_scope.generation);
        assert!(matches!(
            reload.last(),
            Some(ViewerEffect::Fetch { uri, .. }) if uri.path() == "/final"
        ));
    }

    #[test]
    fn owned_failures_retire_work_and_ignore_stale_completions() {
        for failure_kind in 0..3 {
            let mut model = ViewerModel::new(80, 24);
            let (uri, origin) = target("atp://one.example/");
            let effects = model.reduce(ViewerEvent::Navigate { uri, origin });
            let (scope, request_id) = match &effects[0] {
                ViewerEffect::Connect { owner } => (owner.scope.clone(), owner.request_id),
                _ => panic!("expected connect"),
            };
            assert!(model.subscriptions.insert(77, subscription_region(0)));
            let failure = model.reduce(match failure_kind {
                0 => ViewerEvent::FetchFailed {
                    owner: OperationOwner::new(scope.clone(), request_id),
                    message: "fetch failed".into(),
                },
                1 => ViewerEvent::ParseFailed {
                    owner: OperationOwner::new(scope.clone(), request_id),
                    message: "invalid AML".into(),
                },
                _ => ViewerEvent::LayoutFailed {
                    owner: OperationOwner::new(scope.clone(), request_id),
                    message: "layout failed".into(),
                },
            });
            assert_eq!(model.phase, NavigationPhase::Failed);
            assert!(model.subscriptions.is_empty());
            assert!(model.outstanding.is_empty());
            assert!(matches!(
                failure.as_slice(),
                [
                    ViewerEffect::RetirePageWork { .. },
                    ViewerEffect::ActivateErrorPage { .. },
                    ViewerEffect::RenderTerminal
                ]
            ));
            assert!(
                model
                    .reduce(ViewerEvent::LayoutFailed {
                        owner: OperationOwner::new(scope, request_id),
                        message: "stale".into(),
                    })
                    .is_empty()
            );
        }
    }

    #[test]
    fn history_pressure_evicts_oldest_non_current_entry_and_protects_current() {
        let mut model = ViewerModel::new(80, 24);
        let (first_uri, origin) = target("atp://one.example/first");
        let (second_uri, _) = target("atp://one.example/second");
        let scope = PageScope {
            origin,
            generation: 1,
        };
        model.history = vec![
            HistoryEntry {
                id: 11,
                scope: scope.clone(),
                uri: first_uri,
                retained_aml: String::from("first"),
            },
            HistoryEntry {
                id: 12,
                scope,
                uri: second_uri,
                retained_aml: String::from("second"),
            },
        ]
        .into_iter()
        .collect();
        model.history_position = Some(1);

        assert_eq!(
            model.reduce(ViewerEvent::HistoryEvictionRequested {
                message: "budget exceeded".into(),
            }),
            vec![
                ViewerEffect::ReleaseHistoryArtifact { id: 11 },
                ViewerEffect::RenderTerminal,
            ]
        );
        assert_eq!(model.history.len(), 1);
        assert_eq!(model.history[0].id, 12);
        assert_eq!(model.history_position, Some(0));

        assert_eq!(
            model.reduce(ViewerEvent::HistoryEvictionRequested {
                message: "budget exceeded".into(),
            }),
            vec![
                ViewerEffect::ActivateErrorPage {
                    message: "budget exceeded".into(),
                },
                ViewerEffect::RenderTerminal,
            ]
        );
        assert_eq!(model.history[0].id, 12);
        assert_eq!(model.history_position, Some(0));
    }

    #[test]
    fn presentation_failure_orders_pressure_recovery_before_error_activation() {
        let mut model = ViewerModel::new(80, 24);
        let (uri, origin) = target("atp://one.example/current");
        let scope = PageScope {
            origin,
            generation: 7,
        };
        model.scope = Some(scope.clone());
        model.current_uri = Some(uri);
        model.phase = NavigationPhase::Ready;
        model.connection = ConnectionStatus::Connected;

        assert_eq!(
            model.reduce(ViewerEvent::PresentationFailed {
                message: "frame budget exceeded".into(),
                retry: None,
            }),
            vec![ViewerEffect::EvictResource {
                message: "frame budget exceeded".into(),
            }]
        );
        assert_eq!(model.phase, NavigationPhase::Ready);
        assert_eq!(model.connection, ConnectionStatus::Connected);

        assert_eq!(
            model.reduce(ViewerEvent::ResourceEvictionCompleted {
                message: "frame budget exceeded".into(),
                evicted: false,
            }),
            vec![ViewerEffect::EvictHistory {
                message: "frame budget exceeded".into(),
            }]
        );
        assert_eq!(model.phase, NavigationPhase::Ready);

        assert_eq!(
            model.reduce(ViewerEvent::HistoryEvictionRequested {
                message: "frame budget exceeded".into(),
            }),
            vec![
                ViewerEffect::RetirePageWork {
                    scope: scope.clone(),
                },
                ViewerEffect::ActivateErrorPage {
                    message: "frame budget exceeded".into(),
                },
                ViewerEffect::RenderTerminal,
            ]
        );
        assert_eq!(model.phase, NavigationPhase::Failed);
        assert_eq!(model.connection, ConnectionStatus::Disconnected);
    }

    #[test]
    fn pressure_recovery_retries_exact_scoped_action_after_each_release() {
        let mut model = ViewerModel::new(80, 24);
        let (uri, origin) = target("atp://one.example/current");
        let scope = PageScope {
            origin,
            generation: 7,
        };
        let action = PresentationAction::SetPanel {
            panel_id: "tabs".into(),
            state: "second".into(),
        };
        model.scope = Some(scope.clone());
        model.current_uri = Some(uri.try_clone().unwrap());
        model.phase = NavigationPhase::Ready;
        model.connection = ConnectionStatus::Connected;
        model.history = vec![
            HistoryEntry {
                id: 11,
                scope: scope.clone(),
                uri: uri.try_clone().unwrap(),
                retained_aml: String::from("old"),
            },
            HistoryEntry {
                id: 12,
                scope: scope.clone(),
                uri,
                retained_aml: String::from("current"),
            },
        ]
        .into_iter()
        .collect();
        model.history_position = Some(1);

        assert_eq!(
            model.reduce(ViewerEvent::PresentationFailed {
                message: "budget exceeded".into(),
                retry: Some(PressureRetry::Presentation {
                    scope: Some(scope.clone()),
                    action: action.try_clone().unwrap(),
                }),
            }),
            vec![ViewerEffect::EvictResource {
                message: "budget exceeded".into(),
            }]
        );
        assert_eq!(
            model.reduce(ViewerEvent::ResourceEvictionCompleted {
                message: "budget exceeded".into(),
                evicted: false,
            }),
            vec![ViewerEffect::EvictHistory {
                message: "budget exceeded".into(),
            }]
        );
        assert_eq!(
            model.reduce(ViewerEvent::HistoryEvictionRequested {
                message: "budget exceeded".into(),
            }),
            vec![
                ViewerEffect::ReleaseHistoryArtifact { id: 11 },
                ViewerEffect::ApplyPresentationAction {
                    scope: Some(scope),
                    action,
                },
                ViewerEffect::RenderTerminal,
            ]
        );
        assert_eq!(model.history.len(), 1);
        assert_eq!(model.history[0].id, 12);
        assert_eq!(model.history_position, Some(0));
        assert_eq!(model.phase, NavigationPhase::Ready);
        assert_eq!(model.connection, ConnectionStatus::Connected);
    }

    #[test]
    fn drain_layout_retry_is_discarded_when_its_scope_is_retired() {
        let mut model = ViewerModel::new(80, 24);
        let (current, origin) = target("atp://one.example/current");
        let scope = PageScope {
            origin,
            generation: 7,
        };
        model.scope = Some(scope.clone());
        model.current_uri = Some(current);
        model.phase = NavigationPhase::Ready;
        model.connection = ConnectionStatus::Connected;
        assert_eq!(
            model.reduce(ViewerEvent::PresentationFailed {
                message: "layout drain budget exceeded".into(),
                retry: Some(PressureRetry::Presentation {
                    scope: Some(scope.clone()),
                    action: PresentationAction::DrainLayout,
                }),
            }),
            vec![ViewerEffect::EvictResource {
                message: "layout drain budget exceeded".into(),
            }]
        );

        let (replacement, replacement_origin) = target("atp://two.example/replacement");
        model.reduce(ViewerEvent::Navigate {
            uri: replacement,
            origin: replacement_origin,
        });
        assert_ne!(model.scope.as_ref(), Some(&scope));
        assert_eq!(
            model.reduce(ViewerEvent::ResourceEvictionCompleted {
                message: "layout drain budget exceeded".into(),
                evicted: true,
            }),
            vec![ViewerEffect::RenderTerminal]
        );
        assert!(model.pressure_retry.is_none());
    }

    #[test]
    fn animation_tick_pressure_retries_the_exact_owner_and_discards_it_on_navigation() {
        let mut model = ViewerModel::new(80, 24);
        let (current, origin) = target("atp://one.example/current");
        let scope = PageScope {
            origin,
            generation: 7,
        };
        model.scope = Some(scope.clone());
        model.current_uri = Some(current);
        model.phase = NavigationPhase::Ready;
        model.connection = ConnectionStatus::Connected;
        let owner = match model.reduce(ViewerEvent::Timer).as_slice() {
            [ViewerEffect::TickWasm { owner: Some(owner) }] => owner.clone(),
            effects => panic!("expected an owned animation tick, got {effects:?}"),
        };

        assert_eq!(
            model.reduce(ViewerEvent::PresentationFailed {
                message: "animation tick budget exceeded".into(),
                retry: Some(PressureRetry::TickWasm {
                    owner: Some(owner.clone()),
                }),
            }),
            vec![ViewerEffect::EvictResource {
                message: "animation tick budget exceeded".into(),
            }]
        );
        assert_eq!(
            model.reduce(ViewerEvent::ResourceEvictionCompleted {
                message: "animation tick budget exceeded".into(),
                evicted: true,
            }),
            vec![
                ViewerEffect::TickWasm {
                    owner: Some(owner.clone()),
                },
                ViewerEffect::RenderTerminal,
            ]
        );

        model.reduce(ViewerEvent::PresentationFailed {
            message: "animation tick budget exceeded".into(),
            retry: Some(PressureRetry::TickWasm {
                owner: Some(owner.clone()),
            }),
        });
        let (replacement, replacement_origin) = target("atp://two.example/replacement");
        model.reduce(ViewerEvent::Navigate {
            uri: replacement,
            origin: replacement_origin,
        });
        assert_eq!(
            model.reduce(ViewerEvent::ResourceEvictionCompleted {
                message: "animation tick budget exceeded".into(),
                evicted: true,
            }),
            vec![ViewerEffect::RenderTerminal]
        );
        assert!(model.pressure_retry.is_none());
        assert!(!model.owns_operation(&owner));
    }

    #[test]
    fn preparation_pressure_retries_exact_payload_and_discards_it_on_navigation() {
        let mut model = ViewerModel::new(80, 24);
        let (uri, origin) = target("atp://one.example/candidate");
        let effects = model.reduce(ViewerEvent::Navigate {
            uri: uri.try_clone().unwrap(),
            origin: origin.clone(),
        });
        let owner = match effects.last().unwrap() {
            ViewerEffect::Fetch { owner, .. } => owner.clone(),
            _ => panic!("expected fetch"),
        };
        assert_eq!(
            model.reduce(ViewerEvent::PresentationFailed {
                message: "parse budget exceeded".into(),
                retry: Some(PressureRetry::Parse {
                    owner: owner.clone(),
                }),
            }),
            vec![ViewerEffect::EvictResource {
                message: "parse budget exceeded".into(),
            }]
        );
        assert_eq!(
            model.reduce(ViewerEvent::ResourceEvictionCompleted {
                message: "parse budget exceeded".into(),
                evicted: true,
            }),
            vec![
                ViewerEffect::Parse {
                    owner: owner.clone(),
                },
                ViewerEffect::RenderTerminal,
            ]
        );

        model.reduce(ViewerEvent::PresentationFailed {
            message: "layout budget exceeded".into(),
            retry: Some(PressureRetry::PrepareLayout {
                owner: owner.clone(),
            }),
        });
        let (replacement, replacement_origin) = target("atp://one.example/replacement");
        model.reduce(ViewerEvent::Navigate {
            uri: replacement,
            origin: replacement_origin,
        });
        assert_eq!(
            model.reduce(ViewerEvent::ResourceEvictionCompleted {
                message: "layout budget exceeded".into(),
                evicted: true,
            }),
            vec![ViewerEffect::RenderTerminal]
        );
        assert_ne!(model.scope.as_ref(), Some(&owner.scope));
    }

    #[test]
    fn resize_pressure_retries_exact_projection_after_cache_and_history_release() {
        let mut model = ViewerModel::new(80, 24);
        let (uri, origin) = target("atp://one.example/current");
        model.reduce(ViewerEvent::Navigate {
            uri: uri.try_clone().unwrap(),
            origin: origin.clone(),
        });
        model.phase = NavigationPhase::Ready;
        model.history = vec![
            HistoryEntry {
                id: 11,
                scope: PageScope {
                    origin: origin.clone(),
                    generation: 0,
                },
                uri: uri.try_clone().unwrap(),
                retained_aml: String::from("old"),
            },
            HistoryEntry {
                id: 12,
                scope: model.scope.clone().unwrap(),
                uri,
                retained_aml: String::from("current"),
            },
        ]
        .into_iter()
        .collect();
        model.history_position = Some(1);

        let effects = model.reduce(ViewerEvent::Resize {
            width: 47,
            height: 19,
        });
        let owner = match effects.as_slice() {
            [
                ViewerEffect::PrepareResizeProjection {
                    owner,
                    width: 47,
                    height: 19,
                },
            ] => owner.clone(),
            _ => panic!("expected exact resize projection"),
        };
        let retry = PressureRetry::ResizeProjection {
            owner: owner.clone(),
            width: 47,
            height: 19,
        };
        assert_eq!(
            model.reduce(ViewerEvent::PresentationFailed {
                message: "resize pressure".into(),
                retry: Some(PressureRetry::ResizeProjection {
                    owner: owner.clone(),
                    width: 47,
                    height: 19,
                }),
            }),
            vec![ViewerEffect::EvictResource {
                message: "resize pressure".into(),
            }]
        );
        assert_eq!(
            model.reduce(ViewerEvent::ResourceEvictionCompleted {
                message: "resize pressure".into(),
                evicted: true,
            }),
            vec![
                ViewerEffect::PrepareResizeProjection {
                    owner: owner.clone(),
                    width: 47,
                    height: 19,
                },
                ViewerEffect::RenderTerminal,
            ]
        );

        model.reduce(ViewerEvent::PresentationFailed {
            message: "resize pressure".into(),
            retry: Some(retry),
        });
        model.reduce(ViewerEvent::ResourceEvictionCompleted {
            message: "resize pressure".into(),
            evicted: false,
        });
        assert_eq!(
            model.reduce(ViewerEvent::HistoryEvictionRequested {
                message: "resize pressure".into(),
            }),
            vec![
                ViewerEffect::ReleaseHistoryArtifact { id: 11 },
                ViewerEffect::PrepareResizeProjection {
                    owner,
                    width: 47,
                    height: 19,
                },
                ViewerEffect::RenderTerminal,
            ]
        );
        assert_eq!(model.viewport, (47, 19));
        assert_eq!(
            model
                .history
                .iter()
                .map(|entry| entry.id)
                .collect::<Vec<_>>(),
            vec![12]
        );
    }

    #[test]
    fn newer_resize_discards_the_stale_exact_retry_and_completion() {
        let mut model = ViewerModel::new(80, 24);
        let (uri, origin) = target("atp://one.example/current");
        model.reduce(ViewerEvent::Navigate { uri, origin });
        model.phase = NavigationPhase::Ready;
        let first = model.reduce(ViewerEvent::Resize {
            width: 70,
            height: 20,
        });
        let first_owner = match &first[0] {
            ViewerEffect::PrepareResizeProjection { owner, .. } => owner.clone(),
            _ => panic!("expected first resize"),
        };
        model.reduce(ViewerEvent::PresentationFailed {
            message: "resize pressure".into(),
            retry: Some(PressureRetry::ResizeProjection {
                owner: first_owner.clone(),
                width: 70,
                height: 20,
            }),
        });
        model.reduce(ViewerEvent::Resize {
            width: 60,
            height: 18,
        });

        assert_eq!(
            model.reduce(ViewerEvent::ResourceEvictionCompleted {
                message: "resize pressure".into(),
                evicted: true,
            }),
            vec![ViewerEffect::RenderTerminal]
        );
        assert!(
            model
                .reduce(ViewerEvent::ResizeProjectionPrepared {
                    owner: first_owner,
                    content_height: 999,
                })
                .is_empty()
        );
        assert_ne!(model.content_height, 999);
        assert_eq!(model.viewport, (60, 18));
    }

    #[test]
    fn exhausted_preparation_recovery_fails_the_owned_candidate() {
        let mut model = ViewerModel::new(80, 24);
        let (uri, origin) = target("atp://one.example/candidate");
        let effects = model.reduce(ViewerEvent::Navigate { uri, origin });
        let owner = match effects.last().unwrap() {
            ViewerEffect::Fetch { owner, .. } => owner.clone(),
            _ => panic!("expected fetch"),
        };
        model.reduce(ViewerEvent::PresentationFailed {
            message: "layout budget exceeded".into(),
            retry: Some(PressureRetry::PrepareLayout {
                owner: owner.clone(),
            }),
        });
        model.reduce(ViewerEvent::ResourceEvictionCompleted {
            message: "layout budget exceeded".into(),
            evicted: false,
        });

        assert_eq!(
            model.reduce(ViewerEvent::HistoryEvictionRequested {
                message: "layout budget exceeded".into(),
            }),
            vec![
                ViewerEffect::RetirePageWork {
                    scope: owner.scope.clone(),
                },
                ViewerEffect::ActivateErrorPage {
                    message: "layout budget exceeded".into(),
                },
                ViewerEffect::RenderTerminal,
            ]
        );
        assert_eq!(model.phase, NavigationPhase::Failed);
    }

    #[test]
    fn cached_preparation_pressure_protects_target_and_restores_previous_page() {
        let mut model = ViewerModel::new(80, 24);
        let (first_uri, origin) = target("atp://one.example/first");
        let (second_uri, _) = target("atp://one.example/second");
        let previous_scope = PageScope {
            origin: origin.clone(),
            generation: 2,
        };
        model.scope = Some(previous_scope.clone());
        model.current_uri = Some(second_uri.try_clone().unwrap());
        model.phase = NavigationPhase::Ready;
        model.connection = ConnectionStatus::Connected;
        model.history = vec![
            HistoryEntry {
                id: 11,
                scope: PageScope {
                    origin: origin.clone(),
                    generation: 1,
                },
                uri: first_uri,
                retained_aml: String::from("first"),
            },
            HistoryEntry {
                id: 12,
                scope: previous_scope.clone(),
                uri: second_uri.try_clone().unwrap(),
                retained_aml: String::from("second"),
            },
        ]
        .into_iter()
        .collect();
        model.history_position = Some(1);
        let previous_scope_host = model.scope.as_ref().unwrap().origin.host().as_ptr();
        let previous_uri_path = model.current_uri.as_ref().unwrap().path().as_ptr();
        let target_scope_host = model.history[0].scope.origin.host().as_ptr();
        let effects = model.reduce(ViewerEvent::Back);
        let pending = model.pending_history.as_ref().unwrap();
        assert_eq!(
            pending
                .previous_scope
                .as_ref()
                .unwrap()
                .origin
                .host()
                .as_ptr(),
            previous_scope_host,
        );
        assert_eq!(
            pending.previous_uri.as_ref().unwrap().path().as_ptr(),
            previous_uri_path,
        );
        assert_eq!(
            pending.previous_entry_scope.origin.host().as_ptr(),
            target_scope_host,
        );
        let owner = match effects.last().unwrap() {
            ViewerEffect::ActivateCachedHistory { owner, .. } => owner.clone(),
            _ => panic!("expected cached activation"),
        };
        model.reduce(ViewerEvent::PresentationFailed {
            message: "cached layout budget exceeded".into(),
            retry: Some(PressureRetry::PrepareLayout {
                owner: owner.clone(),
            }),
        });
        model.reduce(ViewerEvent::ResourceEvictionCompleted {
            message: "cached layout budget exceeded".into(),
            evicted: false,
        });

        assert_eq!(
            model.reduce(ViewerEvent::HistoryEvictionRequested {
                message: "cached layout budget exceeded".into(),
            }),
            vec![
                ViewerEffect::RetirePageWork { scope: owner.scope },
                ViewerEffect::RenderTerminal,
            ]
        );
        assert_eq!(model.history.len(), 2);
        assert_eq!(model.history[0].id, 11);
        assert_eq!(model.history[1].id, 12);
        assert_eq!(model.history_position, Some(1));
        assert_eq!(model.scope, Some(previous_scope));
        assert_eq!(model.current_uri, Some(second_uri));
        assert_eq!(
            model.scope.as_ref().unwrap().origin.host().as_ptr(),
            previous_scope_host,
        );
        assert_eq!(
            model.current_uri.as_ref().unwrap().path().as_ptr(),
            previous_uri_path,
        );
        assert_eq!(
            model.history[0].scope.origin.host().as_ptr(),
            target_scope_host,
        );
        assert_eq!(model.phase, NavigationPhase::Ready);
    }

    #[test]
    fn navigation_preparation_rejection_preserves_exact_reducer_state() {
        for rejected_step in 0..5 {
            let mut model = ViewerModel::new(80, 24);
            let (uri, origin) = target("atp://one.example/new");
            reject_preparation_after(rejected_step);
            assert!(
                model
                    .reduce(ViewerEvent::Navigate { uri, origin })
                    .is_empty()
            );
            clear_preparation_rejection();
            assert_eq!(model.generation, 0);
            assert_eq!(model.next_request_id, 1);
            assert!(model.scope.is_none());
            assert!(model.current_uri.is_none());
            assert!(model.outstanding.is_empty());
            assert_eq!(model.phase, NavigationPhase::Idle);
        }

        let mut model = ViewerModel::new(80, 24);
        let (current_uri, current_origin) = target("atp://one.example/current?keep=yes");
        let current = model.reduce(ViewerEvent::Navigate {
            uri: current_uri,
            origin: current_origin,
        });
        assert!(!current.is_empty());
        let generation = model.generation;
        let next_request_id = model.next_request_id;
        let scope_host = model.scope.as_ref().unwrap().origin.host().as_ptr();
        let uri_path = model.current_uri.as_ref().unwrap().path().as_ptr();
        let uri_query = model
            .current_uri
            .as_ref()
            .unwrap()
            .query()
            .unwrap()
            .as_ptr();

        reject_preparation_after(0);
        assert!(model.reduce(ViewerEvent::Reload).is_empty());
        clear_preparation_rejection();
        assert_eq!(model.generation, generation);
        assert_eq!(model.next_request_id, next_request_id);
        assert_eq!(
            model.scope.as_ref().unwrap().origin.host().as_ptr(),
            scope_host
        );
        assert_eq!(
            model.current_uri.as_ref().unwrap().path().as_ptr(),
            uri_path
        );
        assert_eq!(
            model
                .current_uri
                .as_ref()
                .unwrap()
                .query()
                .unwrap()
                .as_ptr(),
            uri_query,
        );

        let (submit_uri, submit_origin) = target("atp://one.example/submit");
        reject_preparation_after(0);
        assert!(
            model
                .reduce(ViewerEvent::FormSubmitted {
                    uri: submit_uri,
                    origin: submit_origin,
                    path: "/submit".into(),
                    form_data: "a=b".into(),
                })
                .is_empty()
        );
        clear_preparation_rejection();
        assert_eq!(model.generation, generation);
        assert_eq!(model.next_request_id, next_request_id);
        assert_eq!(
            model.scope.as_ref().unwrap().origin.host().as_ptr(),
            scope_host
        );
        assert_eq!(
            model.current_uri.as_ref().unwrap().path().as_ptr(),
            uri_path
        );
    }

    #[test]
    fn current_scope_copy_rejection_precedes_reducer_mutation() {
        let mut model = ViewerModel::new(80, 24);
        let (_, origin) = target("atp://one.example/current");
        let scope = PageScope {
            origin: origin.try_clone().unwrap(),
            generation: 7,
        };
        seed_active_page(&mut model, scope, 24);
        let scope_host = model.scope.as_ref().unwrap().origin.host().as_ptr();

        reject_preparation_after(0);
        assert!(model.control_token().is_none());
        clear_preparation_rejection();
        assert_eq!(
            model.scope.as_ref().unwrap().origin.host().as_ptr(),
            scope_host
        );

        let next_request_id = model.next_request_id;
        for (label, event) in [
            (
                "wasm",
                ViewerEvent::WasmRequested {
                    path: "/module.wasm".into(),
                },
            ),
            (
                "subscribe",
                ViewerEvent::SubscribeRequested {
                    region: subscription_region(0),
                },
            ),
            ("defer", ViewerEvent::DeferredNavigationRequested),
            ("timer", ViewerEvent::Timer),
            (
                "transport",
                ViewerEvent::TransportLost {
                    origin: origin.try_clone().unwrap(),
                },
            ),
        ] {
            reject_preparation_after(0);
            assert!(model.reduce(event).is_empty(), "{label}");
            clear_preparation_rejection();
            assert_eq!(model.next_request_id, next_request_id);
            assert!(model.outstanding.is_empty());
            assert!(model.pending_subscriptions.is_empty());
            assert!(model.reconnect_request.is_none());
            assert_eq!(model.connection, ConnectionStatus::Connected);
            assert_eq!(
                model.scope.as_ref().unwrap().origin.host().as_ptr(),
                scope_host
            );
        }

        let viewport = model.viewport;
        reject_preparation_after(1);
        assert!(
            model
                .reduce(ViewerEvent::Resize {
                    width: 100,
                    height: 40,
                })
                .is_empty()
        );
        clear_preparation_rejection();
        assert_eq!(model.viewport, viewport);
        assert_eq!(model.next_request_id, next_request_id);
        assert!(model.pending_resize_projection.is_none());

        model.pressure_retry = Some(PressureRetry::TickWasm { owner: None });
        reject_preparation_after(0);
        assert!(
            model
                .reduce(ViewerEvent::HistoryEvictionRequested {
                    message: "no recovery owner".into(),
                })
                .is_empty()
        );
        clear_preparation_rejection();
        assert_eq!(
            model.pressure_retry,
            Some(PressureRetry::TickWasm { owner: None })
        );
        assert_eq!(model.phase, NavigationPhase::Ready);
        assert_eq!(model.connection, ConnectionStatus::Connected);
        assert_eq!(
            model.scope.as_ref().unwrap().origin.host().as_ptr(),
            scope_host
        );
    }

    #[test]
    fn cached_history_preparation_rejection_preserves_exact_reducer_state() {
        for rejected_step in 0..8 {
            let mut model = ViewerModel::new(80, 24);
            let (first_uri, origin) = target("atp://one.example/first?entry=one");
            let (second_uri, _) = target("atp://one.example/second?entry=two");
            let previous_scope = PageScope {
                origin: origin.try_clone().unwrap(),
                generation: 2,
            };
            model.generation = 2;
            model.scope = Some(previous_scope);
            model.current_uri = Some(second_uri.try_clone().unwrap());
            model.phase = NavigationPhase::Ready;
            model.connection = ConnectionStatus::Connected;
            model.history = vec![
                HistoryEntry {
                    id: 11,
                    scope: PageScope {
                        origin: origin.try_clone().unwrap(),
                        generation: 1,
                    },
                    uri: first_uri,
                    retained_aml: "first".into(),
                },
                HistoryEntry {
                    id: 12,
                    scope: model.scope.as_ref().unwrap().try_clone().unwrap(),
                    uri: second_uri,
                    retained_aml: "second".into(),
                },
            ]
            .into_iter()
            .collect();
            model.history_position = Some(1);
            let scope_host = model.scope.as_ref().unwrap().origin.host().as_ptr();
            let uri_path = model.current_uri.as_ref().unwrap().path().as_ptr();
            let history_scope_host = model.history[0].scope.origin.host().as_ptr();
            let history_aml = model.history[0].retained_aml.as_ptr();
            let next_request_id = model.next_request_id;

            reject_preparation_after(rejected_step);
            assert!(model.reduce(ViewerEvent::Back).is_empty());
            clear_preparation_rejection();
            assert_eq!(model.generation, 2);
            assert_eq!(model.next_request_id, next_request_id);
            assert_eq!(model.history_position, Some(1));
            assert!(model.pending_history.is_none());
            assert!(model.outstanding.is_empty());
            assert_eq!(
                model.scope.as_ref().unwrap().origin.host().as_ptr(),
                scope_host
            );
            assert_eq!(
                model.current_uri.as_ref().unwrap().path().as_ptr(),
                uri_path
            );
            assert_eq!(
                model.history[0].scope.origin.host().as_ptr(),
                history_scope_host,
            );
            assert_eq!(model.history[0].retained_aml.as_ptr(), history_aml);
            assert_eq!(model.phase, NavigationPhase::Ready);
            assert_eq!(model.connection, ConnectionStatus::Connected);
        }
    }

    #[test]
    fn history_contents_and_position_are_reducer_owned() {
        let mut model = ViewerModel::new(80, 24);
        let (first_uri, first_origin) = target("atp://one.example/first");
        let first = model.reduce(ViewerEvent::Navigate {
            uri: first_uri.try_clone().unwrap(),
            origin: first_origin,
        });
        let (first_scope, first_request) = match &first[0] {
            ViewerEffect::Connect { owner } => (owner.scope.clone(), owner.request_id),
            _ => panic!("expected connect"),
        };
        admit_history(
            &mut model,
            OperationOwner::new(first_scope, first_request),
            first_uri,
            "first".into(),
        );

        let (second_uri, second_origin) = target("atp://one.example/second");
        let second = model.reduce(ViewerEvent::Navigate {
            uri: second_uri.try_clone().unwrap(),
            origin: second_origin,
        });
        let (second_scope, second_request) = match second.last().unwrap() {
            ViewerEffect::Fetch { owner, .. } => (owner.scope.clone(), owner.request_id),
            _ => panic!("expected fetch"),
        };
        admit_history(
            &mut model,
            OperationOwner::new(second_scope, second_request),
            second_uri,
            "second".into(),
        );
        assert_eq!(model.history_position, Some(1));
        let second_history_id = model.history[1].id;

        let reload = model.reduce(ViewerEvent::Reload);
        let (reload_scope, reload_request) = match reload.last().unwrap() {
            ViewerEffect::Fetch { owner, .. } => (owner.scope.clone(), owner.request_id),
            _ => panic!("expected reload fetch"),
        };
        let reload_uri = model
            .current_uri
            .as_ref()
            .map(AtpUri::try_clone)
            .transpose()
            .unwrap()
            .unwrap();
        admit_history(
            &mut model,
            OperationOwner::new(reload_scope, reload_request),
            reload_uri,
            "second-reloaded".into(),
        );
        assert_eq!(model.history.len(), 2);
        assert_eq!(model.history_position, Some(1));
        assert_eq!(model.history[1].id, second_history_id);
        assert_eq!(model.history[1].retained_aml, "second-reloaded");

        let back = model.reduce(ViewerEvent::Back);
        assert_eq!(model.history_position, Some(1));
        assert!(matches!(
            back.last(),
            Some(ViewerEffect::ActivateCachedHistory { entry, .. })
                if entry.retained_aml == "first"
        ));
        let (back_scope, back_request) = match back.last().unwrap() {
            ViewerEffect::ActivateCachedHistory { owner, .. } => {
                (owner.scope.clone(), owner.request_id)
            }
            _ => unreachable!(),
        };
        model.reduce(ViewerEvent::ParseCompleted {
            owner: OperationOwner::new(back_scope.clone(), back_request),
        });
        model.reduce(ViewerEvent::LayoutPrepared {
            owner: OperationOwner::new(back_scope.clone(), back_request),
            content_height: 10,
        });
        model.reduce(ViewerEvent::LayoutActivated {
            owner: OperationOwner::new(back_scope, back_request),
        });
        assert_eq!(model.history_position, Some(0));
        let activated_scope = model.scope.clone().unwrap();
        assert_eq!(model.history[0].scope, activated_scope);

        assert!(model.reduce(ViewerEvent::Back).is_empty());
        let forward = model.reduce(ViewerEvent::Forward);
        assert!(!forward.is_empty());
        assert_eq!(model.history_position, Some(0));
        let (forward_scope, forward_request) = match forward.last().unwrap() {
            ViewerEffect::ActivateCachedHistory { owner, .. } => {
                (owner.scope.clone(), owner.request_id)
            }
            _ => unreachable!(),
        };
        model.reduce(ViewerEvent::ParseFailed {
            owner: OperationOwner::new(forward_scope, forward_request),
            message: "bad cached page".into(),
        });
        assert_eq!(model.history_position, Some(0));
        assert_eq!(model.scope, Some(activated_scope));
        assert!(
            model
                .reduce(ViewerEvent::JumpToHistory { index: 99 })
                .is_empty()
        );
    }

    #[test]
    fn history_admission_is_an_exact_barrier_before_logical_commit() {
        let mut model = ViewerModel::new(80, 24);
        let (uri, origin) = target("atp://one.example/candidate");
        let effects = model.reduce(ViewerEvent::Navigate {
            uri: uri.try_clone().unwrap(),
            origin,
        });
        let owner = match effects.last().unwrap() {
            ViewerEffect::Fetch { owner, .. } => owner.clone(),
            _ => panic!("expected fetch"),
        };
        let aml = String::from("candidate");
        let aml_ptr = aml.as_ptr();

        assert_eq!(
            model.reduce(ViewerEvent::HistoryAdmissionRequested {
                owner: owner.clone(),
                uri: uri.try_clone().unwrap(),
                retained_aml: aml,
            }),
            vec![ViewerEffect::AdmitHistoryArtifact {
                owner: owner.clone(),
                id: 1,
                replacing: false,
            }]
        );
        assert!(model.history.is_empty());
        assert_eq!(model.history_position, None);

        assert_eq!(
            model.reduce(ViewerEvent::PresentationFailed {
                message: "history full".into(),
                retry: Some(PressureRetry::HistoryArtifact {
                    owner: owner.clone(),
                    id: 1,
                    replacing: false,
                }),
            }),
            vec![ViewerEffect::EvictResource {
                message: "history full".into(),
            }]
        );
        assert_eq!(
            model.reduce(ViewerEvent::ResourceEvictionCompleted {
                message: "history full".into(),
                evicted: true,
            }),
            vec![
                ViewerEffect::AdmitHistoryArtifact {
                    owner: owner.clone(),
                    id: 1,
                    replacing: false,
                },
                ViewerEffect::RenderTerminal,
            ]
        );
        assert!(model.history.is_empty());

        assert_eq!(
            model.reduce(ViewerEvent::HistoryCommitted {
                owner: owner.clone(),
                id: 1,
            }),
            vec![ViewerEffect::InstallHistoryArtifact {
                owner: owner.clone(),
                id: 1,
            }]
        );
        assert_eq!(model.history.len(), 1);
        assert_eq!(model.history[0].id, 1);
        assert_eq!(model.history[0].retained_aml.as_ptr(), aml_ptr);

        let (replacement, replacement_origin) = target("atp://one.example/replacement");
        model.reduce(ViewerEvent::Navigate {
            uri: replacement,
            origin: replacement_origin,
        });
        assert!(
            model
                .reduce(ViewerEvent::HistoryCommitted { owner, id: 1 })
                .is_empty()
        );
        assert_eq!(model.history.len(), 1);
    }

    #[test]
    fn append_after_back_releases_exact_forward_artifacts_before_install() {
        let mut model = ViewerModel::new(80, 24);
        for index in 0..3 {
            let (uri, origin) = target(&format!("atp://one.example/{index}"));
            let effects = model.reduce(ViewerEvent::Navigate {
                uri: uri.try_clone().unwrap(),
                origin,
            });
            let owner = match effects.last().unwrap() {
                ViewerEffect::Fetch { owner, .. } => owner.clone(),
                _ => panic!("expected fetch"),
            };
            admit_history(&mut model, owner, uri, String::from("x"));
        }
        model.history_position = Some(0);
        let (uri, origin) = target("atp://one.example/replacement");
        let effects = model.reduce(ViewerEvent::Navigate {
            uri: uri.try_clone().unwrap(),
            origin,
        });
        let owner = match effects.last().unwrap() {
            ViewerEffect::Fetch { owner, .. } => owner.clone(),
            _ => panic!("expected fetch"),
        };
        let effects = model.reduce(ViewerEvent::HistoryAdmissionRequested {
            owner: owner.clone(),
            uri,
            retained_aml: String::from("replacement"),
        });
        let id = match effects[0] {
            ViewerEffect::AdmitHistoryArtifact { id, .. } => id,
            _ => panic!("expected admission"),
        };
        assert!(
            model
                .pending_history_commit
                .as_ref()
                .is_some_and(|pending| pending.prepared_effects.capacity() > MAX_HISTORY_ENTRIES)
        );
        assert_eq!(
            model.reduce(ViewerEvent::HistoryCommitted {
                owner: owner.clone(),
                id,
            }),
            vec![
                ViewerEffect::ReleaseHistoryArtifact { id: 2 },
                ViewerEffect::ReleaseHistoryArtifact { id: 3 },
                ViewerEffect::InstallHistoryArtifact { owner, id },
            ]
        );
        assert_eq!(
            model
                .history
                .iter()
                .map(|entry| entry.id)
                .collect::<Vec<_>>(),
            vec![1, 4]
        );
    }

    #[test]
    fn reducer_bounds_history_and_rejects_an_oversized_entry() {
        let mut model = ViewerModel::new(80, 24);
        for index in 0..MAX_HISTORY_ENTRIES + 5 {
            let (uri, origin) = target(&format!("atp://one.example/{index}"));
            let effects = model.reduce(ViewerEvent::Navigate {
                uri: uri.try_clone().unwrap(),
                origin,
            });
            let (scope, request_id) = match effects.last().unwrap() {
                ViewerEffect::Fetch { owner, .. } => (owner.scope.clone(), owner.request_id),
                _ => panic!("expected fetch"),
            };
            admit_history(
                &mut model,
                OperationOwner::new(scope, request_id),
                uri,
                String::from("x"),
            );
        }
        assert_eq!(model.history.len(), MAX_HISTORY_ENTRIES);
        assert_eq!(model.history_position, Some(MAX_HISTORY_ENTRIES - 1));
        assert_eq!(
            model.history.last().unwrap().uri.path(),
            format!("/{}", MAX_HISTORY_ENTRIES + 4)
        );

        let oversized = "x".repeat(MAX_HISTORY_AML_BYTES + 1);
        let (uri, origin) = target("atp://one.example/current");
        let effects = model.reduce(ViewerEvent::Navigate {
            uri: uri.try_clone().unwrap(),
            origin,
        });
        let (scope, request_id) = match effects.last().unwrap() {
            ViewerEffect::Fetch { owner, .. } => (owner.scope.clone(), owner.request_id),
            _ => panic!("expected fetch"),
        };
        let owner = OperationOwner::new(scope, request_id);
        assert!(matches!(
            model.reduce(ViewerEvent::HistoryAdmissionRequested {
                owner: owner.clone(),
                uri,
                retained_aml: oversized,
            })[..],
            [
                ViewerEffect::RetirePageWork { .. },
                ViewerEffect::ActivateErrorPage { .. },
                ViewerEffect::RenderTerminal,
            ]
        ));
        assert_eq!(model.history.len(), MAX_HISTORY_ENTRIES);
        assert_eq!(model.history_position, Some(MAX_HISTORY_ENTRIES - 1));
        assert!(model.pending_history_commit.is_none());
    }

    #[test]
    fn full_inline_history_evicts_before_push_and_orders_release_before_install() {
        let mut model = ViewerModel::new(80, 24);
        assert_eq!(model.history.capacity(), MAX_HISTORY_ENTRIES);
        for index in 0..MAX_HISTORY_ENTRIES {
            let (uri, origin) = target(&format!("atp://one.example/{index}"));
            let effects = model.reduce(ViewerEvent::Navigate {
                uri: uri.try_clone().unwrap(),
                origin,
            });
            let owner = match effects.last().unwrap() {
                ViewerEffect::Fetch { owner, .. } => owner.clone(),
                _ => panic!("expected fetch"),
            };
            admit_history(&mut model, owner, uri, String::from("x"));
        }
        let backing = model.history.as_ptr();
        let (uri, origin) = target("atp://one.example/next");
        let effects = model.reduce(ViewerEvent::Navigate {
            uri: uri.try_clone().unwrap(),
            origin,
        });
        let owner = match effects.last().unwrap() {
            ViewerEffect::Fetch { owner, .. } => owner.clone(),
            _ => panic!("expected fetch"),
        };
        let effects = model.reduce(ViewerEvent::HistoryAdmissionRequested {
            owner: owner.clone(),
            uri,
            retained_aml: String::from("next"),
        });
        let id = match effects[0] {
            ViewerEffect::AdmitHistoryArtifact { id, .. } => id,
            _ => panic!("expected admission"),
        };
        assert_eq!(
            model.reduce(ViewerEvent::HistoryCommitted {
                owner: owner.clone(),
                id,
            }),
            vec![
                ViewerEffect::ReleaseHistoryArtifact { id: 1 },
                ViewerEffect::InstallHistoryArtifact { owner, id },
            ]
        );
        assert_eq!(model.history.len(), MAX_HISTORY_ENTRIES);
        assert_eq!(model.history.capacity(), MAX_HISTORY_ENTRIES);
        assert_eq!(model.history.as_ptr(), backing);
        assert_eq!(model.history.first().unwrap().id, 2);
        assert_eq!(model.history.last().unwrap().id, id);
        assert_eq!(model.history_position, Some(MAX_HISTORY_ENTRIES - 1));
    }

    #[test]
    fn replacement_trims_oldest_noncurrent_history_and_keeps_current() {
        let mut model = ViewerModel::new(80, 24);
        let (current_uri, origin) = target("atp://one.example/current");
        let (other_uri, _) = target("atp://one.example/other");
        let scope = PageScope {
            origin,
            generation: 1,
        };
        let candidate = "c".repeat(MAX_HISTORY_AML_BYTES / 2 + 1);
        let candidate_ptr = candidate.as_ptr();
        model.scope = Some(scope.clone());
        model.current_uri = Some(current_uri.try_clone().unwrap());
        model.history = vec![
            HistoryEntry {
                id: 1,
                scope: scope.clone(),
                uri: current_uri.try_clone().unwrap(),
                retained_aml: String::from("old"),
            },
            HistoryEntry {
                id: 2,
                scope,
                uri: other_uri,
                retained_aml: "o".repeat(MAX_HISTORY_AML_BYTES / 2),
            },
        ]
        .into_iter()
        .collect();
        model.history_position = Some(0);

        let effects = model.reduce(ViewerEvent::Reload);
        let owner = match effects.last().unwrap() {
            ViewerEffect::Fetch { owner, .. } => owner.clone(),
            _ => panic!("expected reload fetch"),
        };
        assert!(matches!(
            model.reduce(ViewerEvent::HistoryAdmissionRequested {
                owner: owner.clone(),
                uri: current_uri,
                retained_aml: candidate,
            })[..],
            [ViewerEffect::AdmitHistoryArtifact {
                id: 1,
                replacing: true,
                ..
            }]
        ));
        assert_eq!(
            model.reduce(ViewerEvent::HistoryCommitted {
                owner: owner.clone(),
                id: 1,
            }),
            vec![
                ViewerEffect::ReleaseHistoryArtifact { id: 1 },
                ViewerEffect::ReleaseHistoryArtifact { id: 2 },
                ViewerEffect::InstallHistoryArtifact { owner, id: 1 },
            ]
        );
        assert_eq!(model.history.len(), 1);
        assert_eq!(model.history[0].id, 1);
        assert_eq!(model.history[0].retained_aml.as_ptr(), candidate_ptr);
        assert_eq!(model.history_position, Some(0));
    }

    /// A deliberately small navigation oracle.  It does not call reducer helpers or
    /// inspect reducer effects: the harness delivers the same external event to both
    /// state machines and compares their observable ownership state afterwards.
    #[derive(Debug)]
    struct ReferenceNavigation {
        phase: NavigationPhase,
        connection: ConnectionStatus,
        scope: Option<PageScope>,
        uri: Option<AtpUri>,
        history: Vec<HistoryEntry>,
        history_position: Option<usize>,
        outstanding: HashMap<RequestId, PageScope>,
        generation: u64,
        next_request_id: RequestId,
        next_history_id: HistoryId,
        reconnect_request: Option<RequestId>,
        history_commit: HistoryCommit,
        pending_history: Option<PendingHistoryActivation>,
    }

    impl ReferenceNavigation {
        fn new() -> Self {
            Self {
                phase: NavigationPhase::Idle,
                connection: ConnectionStatus::Disconnected,
                scope: None,
                uri: None,
                history: Vec::new(),
                history_position: None,
                outstanding: HashMap::new(),
                generation: 0,
                next_request_id: 1,
                next_history_id: 1,
                reconnect_request: None,
                history_commit: HistoryCommit::Append,
                pending_history: None,
            }
        }

        fn issue(&mut self, scope: &PageScope) -> RequestId {
            let request_id = self.next_request_id;
            self.next_request_id = self.next_request_id.wrapping_add(1);
            self.outstanding.insert(request_id, scope.clone());
            request_id
        }

        fn owner(&self) -> Option<(PageScope, RequestId)> {
            let scope = self.scope.clone()?;
            self.outstanding.iter().find_map(|(request_id, owner)| {
                (owner == &scope).then(|| (scope.clone(), *request_id))
            })
        }

        fn begin(&mut self, uri: AtpUri, origin: Origin, history_commit: HistoryCommit) {
            self.generation = self.generation.wrapping_add(1);
            let scope = PageScope {
                origin,
                generation: self.generation,
            };
            self.scope = Some(scope.clone());
            self.uri = Some(uri);
            self.phase = NavigationPhase::Connecting;
            self.connection = ConnectionStatus::Connecting;
            self.outstanding.clear();
            self.reconnect_request = None;
            self.history_commit = history_commit;
            self.pending_history = None;
            self.issue(&scope);
        }

        fn connected(&mut self, scope: &PageScope, request_id: RequestId) {
            if self.outstanding.get(&request_id) != Some(scope)
                || self.scope.as_ref() != Some(scope)
            {
                return;
            }
            self.connection = ConnectionStatus::Connected;
            if self.reconnect_request == Some(request_id) {
                self.reconnect_request = None;
                self.outstanding.remove(&request_id);
            } else {
                self.phase = NavigationPhase::Fetching;
            }
        }

        fn commit(&mut self, scope: &PageScope, request_id: RequestId, retained_aml: String) {
            if self.outstanding.get(&request_id) != Some(scope)
                || self.scope.as_ref() != Some(scope)
            {
                return;
            }
            let uri = self
                .uri
                .as_ref()
                .map(AtpUri::try_clone)
                .transpose()
                .unwrap()
                .expect("an owned request has a URI");
            match (self.history_commit, self.history_position) {
                (HistoryCommit::ReplaceCurrent, Some(position))
                    if position < self.history.len() =>
                {
                    let id = self.history[position].id;
                    self.history[position] = HistoryEntry {
                        id,
                        scope: scope.clone(),
                        uri,
                        retained_aml,
                    };
                }
                _ => {
                    if let Some(position) = self.history_position {
                        self.history.truncate(position.saturating_add(1));
                    } else {
                        self.history.clear();
                    }
                    let id = self.next_history_id;
                    self.next_history_id = self.next_history_id.wrapping_add(1);
                    self.history.push(HistoryEntry {
                        id,
                        scope: scope.clone(),
                        uri,
                        retained_aml,
                    });
                    self.history_position = self.history.len().checked_sub(1);
                }
            }
            self.history_commit = HistoryCommit::Append;
        }

        fn activate_history(&mut self, index: usize) {
            if self.history_position == Some(index) || index >= self.history.len() {
                return;
            }
            let previous_origin = self.scope.as_ref().map(|scope| scope.origin.clone());
            let previous_scope = self.scope.clone();
            let previous_uri = self
                .uri
                .as_ref()
                .map(AtpUri::try_clone)
                .transpose()
                .unwrap();
            let previous_phase = self.phase;
            let previous_connection = self.connection;
            let previous_position = self.history_position;
            let previous_entry_scope = self.history[index].scope.clone();
            self.generation = self.generation.wrapping_add(1);
            let scope = PageScope {
                origin: self.history[index].scope.origin.clone(),
                generation: self.generation,
            };
            self.history[index].scope = scope.clone();
            self.uri = Some(self.history[index].uri.try_clone().unwrap());
            self.phase = NavigationPhase::Parsing;
            if previous_origin.as_ref() != Some(&scope.origin) {
                self.connection = ConnectionStatus::Disconnected;
            }
            self.scope = Some(scope.clone());
            self.outstanding.clear();
            self.reconnect_request = None;
            self.pending_history = Some(PendingHistoryActivation {
                target: index,
                target_generation: scope.generation,
                previous_entry_scope,
                previous_position,
                previous_scope,
                previous_uri,
                previous_phase,
                previous_connection,
            });
            self.issue(&scope);
        }

        fn assert_matches(&self, actual: &ViewerModel, context: &str) {
            assert_eq!(actual.phase, self.phase, "phase after {context}");
            assert_eq!(
                actual.connection, self.connection,
                "connection after {context}"
            );
            assert_eq!(actual.scope, self.scope, "scope after {context}");
            assert_eq!(actual.current_uri, self.uri, "URI after {context}");
            assert_eq!(
                actual.history.as_slice(),
                self.history.as_slice(),
                "history after {context}"
            );
            assert_eq!(
                actual.history_position, self.history_position,
                "history position after {context}"
            );
            assert_eq!(
                actual.outstanding.len(),
                self.outstanding.len(),
                "owner count after {context}"
            );
            for (request_id, scope) in &self.outstanding {
                assert!(
                    actual.outstanding.contains(*request_id)
                        && actual.scope.as_ref() == Some(scope),
                    "owner {request_id} after {context}"
                );
            }
        }
    }

    struct NavigationHarness {
        actual: ViewerModel,
        reference: ReferenceNavigation,
    }

    impl NavigationHarness {
        fn new() -> Self {
            Self {
                actual: ViewerModel::new(80, 24),
                reference: ReferenceNavigation::new(),
            }
        }

        fn check(&self, context: &str) {
            self.reference.assert_matches(&self.actual, context);
        }

        fn navigate(&mut self, value: &str, redirect: bool) {
            let (uri, origin) = target(value);
            let history_commit = if redirect {
                self.reference.history_commit
            } else {
                HistoryCommit::Append
            };
            self.reference
                .begin(uri.try_clone().unwrap(), origin.clone(), history_commit);
            let event = if redirect {
                ViewerEvent::Redirect { uri, origin }
            } else {
                ViewerEvent::Navigate { uri, origin }
            };
            self.actual.reduce(event);
            self.check(if redirect { "redirect" } else { "navigation" });
        }

        fn reload(&mut self) {
            if let (Some(uri), Some(scope)) = (
                self.reference
                    .uri
                    .as_ref()
                    .map(AtpUri::try_clone)
                    .transpose()
                    .unwrap(),
                self.reference.scope.clone(),
            ) {
                self.reference
                    .begin(uri, scope.origin, HistoryCommit::ReplaceCurrent);
            }
            self.actual.reduce(ViewerEvent::Reload);
            self.check("reload");
        }

        fn finish(&mut self, label: &str) {
            let Some((scope, request_id)) = self.reference.owner() else {
                return;
            };
            if self.reference.phase == NavigationPhase::Connecting {
                self.reference.connected(&scope, request_id);
                self.actual.reduce(ViewerEvent::Connected {
                    owner: OperationOwner::new(scope.clone(), request_id),
                });
                self.check("connect completion");
            }
            if self.reference.phase == NavigationPhase::Fetching {
                self.reference.phase = NavigationPhase::Parsing;
                self.actual.reduce(ViewerEvent::FetchCompleted {
                    owner: OperationOwner::new(scope.clone(), request_id),
                });
                self.check("fetch completion");

                self.reference.commit(&scope, request_id, label.into());
                admit_history(
                    &mut self.actual,
                    OperationOwner::new(scope.clone(), request_id),
                    self.reference
                        .uri
                        .as_ref()
                        .map(AtpUri::try_clone)
                        .transpose()
                        .unwrap()
                        .unwrap(),
                    label.into(),
                );
                self.check("history commit");
            }
            if self.reference.phase == NavigationPhase::Parsing {
                self.reference.phase = NavigationPhase::Layout;
                self.actual.reduce(ViewerEvent::ParseCompleted {
                    owner: OperationOwner::new(scope.clone(), request_id),
                });
                self.check("parse completion");
            }
            if self.reference.phase == NavigationPhase::Layout {
                self.reference.phase = NavigationPhase::Ready;
                if self
                    .reference
                    .pending_history
                    .as_ref()
                    .is_some_and(|pending| pending.target_generation == scope.generation)
                    && let Some(pending) = self.reference.pending_history.take()
                {
                    self.reference.history_position = Some(pending.target);
                }
                self.reference.outstanding.remove(&request_id);
                self.actual.reduce(ViewerEvent::LayoutPrepared {
                    owner: OperationOwner::new(scope.clone(), request_id),
                    content_height: 40,
                });
                self.actual.reduce(ViewerEvent::LayoutActivated {
                    owner: OperationOwner::new(scope, request_id),
                });
                self.check("layout completion");
            }
        }

        fn fail(&mut self) {
            let Some((scope, request_id)) = self.reference.owner() else {
                return;
            };
            if self
                .reference
                .pending_history
                .as_ref()
                .is_some_and(|pending| pending.target_generation == scope.generation)
            {
                let pending = self.reference.pending_history.take().unwrap();
                self.reference.history[pending.target].scope = pending.previous_entry_scope;
                self.reference.history_position = pending.previous_position;
                self.reference.scope = pending.previous_scope;
                self.reference.uri = pending.previous_uri;
                self.reference.phase = pending.previous_phase;
                self.reference.connection = pending.previous_connection;
            } else {
                self.reference.phase = NavigationPhase::Failed;
                self.reference.connection = ConnectionStatus::Disconnected;
            }
            self.reference.outstanding.clear();
            self.reference.reconnect_request = None;
            self.actual.reduce(ViewerEvent::FetchFailed {
                owner: OperationOwner::new(scope, request_id),
                message: "reference failure".into(),
            });
            self.check("owned failure");
        }

        fn reconnect(&mut self) {
            let Some(scope) = self.reference.scope.clone() else {
                return;
            };
            if self.reference.connection == ConnectionStatus::Connecting {
                self.actual.reduce(ViewerEvent::TransportLost {
                    origin: scope.origin,
                });
                self.check("ignored repeated transport loss");
                return;
            }
            self.reference.connection = ConnectionStatus::Connecting;
            let request_id = self.reference.issue(&scope);
            self.reference.reconnect_request = Some(request_id);
            self.actual.reduce(ViewerEvent::TransportLost {
                origin: scope.origin.clone(),
            });
            self.check("transport loss");
            self.reference.connected(&scope, request_id);
            self.actual.reduce(ViewerEvent::Connected {
                owner: OperationOwner::new(scope, request_id),
            });
            self.check("reconnect completion");
        }

        fn back(&mut self) {
            if let Some(position) = self.reference.history_position
                && let Some(target) = position.checked_sub(1)
            {
                self.reference.activate_history(target);
            }
            self.actual.reduce(ViewerEvent::Back);
            self.check("back");
        }

        fn forward(&mut self) {
            if let Some(position) = self.reference.history_position {
                self.reference.activate_history(position.saturating_add(1));
            }
            self.actual.reduce(ViewerEvent::Forward);
            self.check("forward");
        }

        fn jump(&mut self, index: usize) {
            self.reference.activate_history(index);
            self.actual.reduce(ViewerEvent::JumpToHistory { index });
            self.check("history jump");
        }
    }

    #[test]
    fn navigation_matches_independent_reference_model_for_complete_sequence() {
        let mut harness = NavigationHarness::new();
        harness.navigate("atp://one.example/first", false);
        harness.finish("first");
        harness.navigate("atp://one.example/redirecting", false);
        harness.navigate("atp://two.example/second", true);
        harness.finish("second");
        harness.back();
        harness.finish("cached first");
        harness.forward();
        harness.finish("cached second");
        harness.reconnect();
        harness.reload();
        harness.fail();
        harness.jump(0);
        harness.finish("cached first again");
        harness.jump(usize::MAX);
    }

    #[test]
    fn generated_navigation_sequences_match_independent_reference_model() {
        for seed in 0_u64..128 {
            let mut state = seed.wrapping_add(1);
            let mut harness = NavigationHarness::new();
            for step in 0..96 {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                let site = if state & 1 == 0 { "one" } else { "two" };
                let value = format!("atp://{site}.example/page-{}", (state >> 8) % 7);
                match (state >> 32) % 9 {
                    0 => harness.navigate(&value, false),
                    1 => harness.navigate(&value, true),
                    2 => harness.finish(&format!("seed-{seed}-step-{step}")),
                    3 => harness.fail(),
                    4 => harness.reconnect(),
                    5 => harness.back(),
                    6 => harness.forward(),
                    7 => harness.jump(((state >> 16) % 6) as usize),
                    _ => harness.reload(),
                }
            }
        }
    }
}
