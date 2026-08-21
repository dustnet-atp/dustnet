#![allow(clippy::too_many_arguments)]

use std::io::{self, Write};

use crossterm::{
    ExecutableCommand, cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    terminal::{self, ClearType},
};

use std::sync::Arc;

use crate::client::{
    AtpClient, MAX_ACTIVE_SUBSCRIPTIONS, NavigationResponse, ScopedResource, ScopedUpdate,
};
use crate::color::ColorSupport;
use crate::compositor::animate::runtime::{PreparedWasmSource, TickResult};
use crate::compositor::animate::{AnimationRuntime, PAGE_TRANSITION_ID};
use crate::compositor::layout::Rect;
use crate::compositor::layout::cell::{Cell, CellBuffer};
use crate::compositor::layout::engine::{PlacedElement, PlacedKind};
use crate::compositor::layout::text::WidthConfig;
use crate::compositor::scene::{
    self as scene_mod, EventBinding, NodeId, NodeKind, Patch, PatchApplier, Scene,
};
use crate::config::{self, ClientConfig};
use crate::parser::ast::{self, Document, LiveScroll};
use crate::protocol::message::SubscribeMode;
use crate::protocol::uri::AtpUri;
use crate::resource::{BudgetLease, ResourceCategory, ResourceGovernor};
use crate::viewer::{
    ConnectionStatus, NavigationPhase, OperationOwner, PageScope, PresentationAction,
    PressureRetry, SubscriptionRegionKey, ViewerEffect as LifecycleEffect,
    ViewerEvent as LifecycleEvent, ViewerModel as LifecycleModel,
};

use crate::compositor::animate::PageTransitionAdapter;
use crate::compositor::composite::{Compositor, SharedFrame};
#[path = "events.rs"]
mod events;
#[path = "navigation.rs"]
mod navigation;
use super::ReducerPort;
#[cfg(test)]
use super::dispatch_event;
use super::lifecycle::ViewerError;
use super::rendering::*;
use events::*;

use crate::compositor::terminal_lifecycle::Terminal;

// Presentation state is owned by the dedicated module.
use super::presentation::*;

// ─── Viewer entry points ────────────────────────────────────

/// Interactive viewer for a local document with scrolling and resize support.
pub async fn run_viewer(
    doc: &Document,
    color_support: ColorSupport,
    wcfg: WidthConfig,
    config: &ClientConfig,
    wasm_dir: Option<&std::path::Path>,
) -> io::Result<()> {
    let mut term = Terminal::enter()?;
    let (term_w, term_h) = Terminal::size()?;
    let page = layout_page_with_admission(
        doc,
        term_w,
        term_h,
        color_support,
        wcfg,
        None,
        None,
        wasm_dir,
        None,
        None,
    )
    .await
    .map_err(|_| io::Error::other("page exceeded the client resource budget"))?;
    let runtime = TerminalRuntime::new(page, None, Vec::new(), term_w, term_h, color_support, wcfg);
    let result = viewer_main_loop(
        runtime,
        ReducerPort::new(LifecycleModel::new(term_w, term_h)),
        color_support,
        wcfg,
        config,
    )
    .await;
    term.leave()?;
    result.map_err(|e| match e {
        ViewerError::Io(e) => e,
        other => io::Error::other(other.to_string()),
    })
}

/// Interactive viewer that fetches pages over ATP.
pub async fn run_connected_viewer(
    initial_uri: &AtpUri,
    policy: crate::client::TlsPolicy,
    color_support: ColorSupport,
    wcfg: WidthConfig,
    config: &ClientConfig,
) -> Result<(), ViewerError> {
    let mut term = Terminal::enter()?;
    let mut client = AtpClient::new(policy);
    let (term_w, term_h) = Terminal::size()?;
    let mut lifecycle = ReducerPort::new(LifecycleModel::new(term_w, term_h));
    let origin = client.request_origin(initial_uri)?;
    let placeholder_uri = initial_uri
        .try_clone()
        .map_err(|_| ViewerError::ParseFailed)?;
    let navigation_uri = initial_uri
        .try_clone()
        .map_err(|_| ViewerError::ParseFailed)?;
    let placeholder_doc = parse_aml("[page][/page]").ok_or(ViewerError::ParseFailed)?;
    let placeholder = layout_page_with_admission(
        &placeholder_doc,
        term_w,
        term_h,
        color_support,
        wcfg,
        Some(&mut client),
        Some(placeholder_uri),
        None,
        None,
        None,
    )
    .await
    .map_err(|_| ViewerError::ParseFailed)?;
    let mut runtime = TerminalRuntime::new(
        placeholder,
        Some(client),
        Vec::new(),
        term_w,
        term_h,
        color_support,
        wcfg,
    );
    let mut activated = dispatch_navigation_event(
        &mut runtime,
        &mut lifecycle,
        LifecycleEvent::InitialNavigation {
            uri: navigation_uri,
            origin,
        },
    )
    .await?;
    // The very first navigation is the one most likely to meet an unknown
    // site, and it happens before the main loop exists — so the retry the loop
    // performs after a certificate is pinned has to be repeated here. Without
    // this the prompt appears, the pin is written, and the viewer exits anyway.
    if !matches!(activated, Some(ActivatedNavigation::Network { .. }))
        && let Some(uri) = runtime.retry_after_trust.take()
    {
        let origin = runtime
            .client
            .as_ref()
            .map(|client| client.request_origin(&uri))
            .transpose()?;
        if let (Some(origin), Ok(uri)) = (origin, uri.try_clone()) {
            activated = dispatch_navigation_event(
                &mut runtime,
                &mut lifecycle,
                LifecycleEvent::InitialNavigation { uri, origin },
            )
            .await?;
        }
    }
    let Some(ActivatedNavigation::Network { .. }) = activated else {
        return Err(if runtime.declined_trust {
            ViewerError::TrustDeclined
        } else if let Some(reason) = runtime.last_fetch_error.take() {
            ViewerError::InitialNavigationFailed(reason)
        } else {
            ViewerError::ParseFailed
        });
    };
    let result = viewer_main_loop(runtime, lifecycle, color_support, wcfg, config).await;
    term.leave()?;
    result
}

// ─── Page loading and layout ────────────────────────────────

/// Parse AML content into a Document, logging diagnostics to stderr.
/// Generate an AML page showing all active sessions.
fn render_sessions_page(sessions: &crate::session::SessionStore) -> String {
    let mut aml = String::from(
        "[page mode=document title=\"Sessions\"]\n\
         \x20 [heading level=1 fg=cyan]Active Sessions[/heading]\n\
         \x20 [spacer lines=1 /]\n",
    );

    if sessions.total_count() == 0 {
        aml.push_str(
            "\x20 [text dim]No active sessions.[/text]\n\
             \x20 [spacer lines=1 /]\n",
        );
    } else {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let mut sites: Vec<_> = sessions.iter_sites().collect();
        sites.sort_by(|(left, _), (right, _)| left.cmp_storage_key(right));

        for (origin, store) in &sites {
            let tokens = store.list_tokens();
            if tokens.is_empty() {
                continue;
            }
            aml.push_str(&format!(
                "\x20 [box border=single fg=white]\n\
                 \x20   [text bold fg=cyan]{site}[/text]\n",
                site = origin.storage_key_display(),
            ));

            for token in tokens {
                let expiry = match token.expires {
                    Some(exp) if exp > now => {
                        let remaining = exp - now;
                        if remaining >= 3600 {
                            format!("{}h remaining", remaining / 3600)
                        } else if remaining >= 60 {
                            format!("{}m remaining", remaining / 60)
                        } else {
                            format!("{remaining}s remaining")
                        }
                    }
                    Some(_) => "expired".to_string(),
                    None => "no expiry".to_string(),
                };
                aml.push_str(&format!(
                    "\x20   [text]  [text dim]scope:[/text] {scope}  [text dim]{expiry}[/text][/text]\n",
                    scope = token.scope,
                    expiry = expiry,
                ));
            }

            aml.push_str("\x20 [/box]\n\x20 [spacer lines=1 /]\n");
        }

        aml.push_str(&format!(
            "\x20 [text dim]{} session(s) across {} site(s)[/text]\n\
             \x20 [spacer lines=1 /]\n",
            sessions.total_count(),
            sites.iter().filter(|(_, s)| !s.is_empty()).count(),
        ));
    }

    aml.push_str(
        "\x20 [text dim]:sessions clear[/text] [text dim]— clear all[/text]\n\
         \x20 [text dim]:sessions clear <site>[/text] [text dim]— clear one site[/text]\n\
         [/page]",
    );

    aml
}

fn parse_aml(content: &str) -> Option<Document> {
    parse_aml_result(content).ok()
}

fn parse_aml_result(content: &str) -> Result<Document, RemoteParseError> {
    let bytes = content.as_bytes();
    let mut scanner = match crate::scanner::Scanner::new(bytes) {
        Ok(s) => s,
        Err(e) => {
            let classification = classify_scan_error(&e);
            eprintln!("scanner error: {e}");
            return Err(classification);
        }
    };
    let tokens = match scanner.scan_all() {
        Ok(t) => t,
        Err(e) => {
            let classification = classify_scan_error(&e);
            eprintln!("scanner error: {e}");
            return Err(classification);
        }
    };
    let result = crate::parser::parse(tokens);
    for diag in &result.diagnostics {
        eprintln!("{diag}");
    }
    if result.resource_exhausted() {
        return Err(RemoteParseError::ResourceRejected);
    }
    if result.has_errors() {
        return Err(RemoteParseError::Invalid);
    }
    result.document.ok_or(RemoteParseError::Invalid)
}

const PARSE_TRANSIENT_MULTIPLIER: usize = 16;

/// Admit conservative scanner/parser/AST storage before touching remote AML.
/// The lease is deliberately retained through scene construction and then
/// resized to the exact retained AST + scene string capacity by `layout_page`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteParseError {
    Invalid,
    ResourceRejected,
}

fn classify_scan_error(error: &crate::scanner::ScanError) -> RemoteParseError {
    match error {
        crate::scanner::ScanError::ResourceExhausted { .. } => RemoteParseError::ResourceRejected,
        _ => RemoteParseError::Invalid,
    }
}

fn parse_remote_aml(
    content: &str,
    governor: &ResourceGovernor,
) -> Result<(Document, BudgetLease), RemoteParseError> {
    let transient_bytes = content
        .len()
        .checked_mul(PARSE_TRANSIENT_MULTIPLIER)
        .ok_or(RemoteParseError::ResourceRejected)?;
    let lease = governor
        .reserve(ResourceCategory::AstStrings, transient_bytes)
        .map_err(|_| RemoteParseError::ResourceRejected)?;
    let document = parse_aml_result(content)?;
    Ok((document, lease))
}

#[derive(Debug)]
struct PagePreparationRejected {
    string_admission: Option<BudgetLease>,
}

/// Holds the loaded state for a page — everything needed to display it.
///
/// `scene` is authoritative for every piece of mutable state: panel
/// active state, details open/closed, focus, scroll, per-node
/// buffers, event bindings. The scene is built from a parsed
/// `Document` once at page load (`layout_page`) and never again —
/// `Document` is not retained on `LoadedPage`.
struct LoadedPage {
    governor: ResourceGovernor,
    /// The bounded built-in resource rejection page is client-owned and does
    /// not consume the hostile origin's exhausted budget.
    client_owned_error: bool,
    /// Aggregate leases for page-owned scene and compositor cell storage.
    _budget_leases: Vec<BudgetLease>,
    /// Exact retained-capacity charge for focus, placement, and sticky
    /// projection collections.
    _projection_lease: Option<BudgetLease>,
    /// Retained sticky projection metadata covered by `_projection_lease`.
    _sticky_regions: Vec<crate::compositor::layout::engine::StickyRegion>,
    focusables: Vec<crate::compositor::panels::FocusableElement>,
    buf: CellBuffer,
    anim_rt: AnimationRuntime,
    prepared_wasm: PreparedWasmBatch,
    wasm_dir: Option<std::path::PathBuf>,
    /// Every placed element (panels, animations, live regions) in document
    /// order. Consumers filter by `PlacedKind`.
    placed: Vec<PlacedElement>,
    /// Separate buffer for sticky=bottom content, extracted from the main buffer.
    sticky_buf: Option<CellBuffer>,
    /// Scene graph for this page. Constructed at navigation from
    /// the parsed document and hydrated with per-node buffers for
    /// panels, animations, and live regions. Authoritative for
    /// every piece of mutable display state. See
    /// `docs/internals/compositor.md`.
    scene: Scene,
    /// Fully admitted empty authored-action queue staged with this page. It is
    /// transferred into `TerminalRuntime` only when the page activates.
    prepared_event_dispatcher: Option<EventDispatcher>,
}

const RESOURCE_ERROR_AML: &str = "[page mode=document title=\"Content blocked\"]\n[text bold fg=red]Content blocked by Dustnet[/text]\n[text]This page exceeded the client resource budget and was not displayed.[/text]\n[/page]";

impl LoadedPage {
    fn panels(&self) -> impl Iterator<Item = &PlacedElement> {
        self.placed.iter().filter(|p| p.is_panel())
    }

    fn live_regions(&self) -> impl Iterator<Item = &PlacedElement> {
        self.placed.iter().filter(|p| p.is_live())
    }
}

/// A history entry: the URI, its AML content, and the transition used to arrive here.
struct HistoryEntry {
    id: crate::viewer::HistoryId,
    _retained_bytes: usize,
    _budget_lease: Option<BudgetLease>,
    /// Human-readable page title captured when the entry was loaded.
    title: Arc<str>,
    /// The transition that was used when navigating TO this page.
    transition: Option<ast::TransitionKind>,
    transition_duration_ms: u32,
}

struct PendingHistoryArtifact {
    id: Option<crate::viewer::HistoryId>,
    retained_bytes: usize,
    budget_lease: Option<BudgetLease>,
    title: Arc<str>,
    transition: Option<ast::TransitionKind>,
    transition_duration_ms: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PreparedWorkKey {
    generation: u64,
    request_id: u64,
}

impl From<&OperationOwner> for PreparedWorkKey {
    fn from(owner: &OperationOwner) -> Self {
        Self {
            generation: owner.scope.generation,
            request_id: owner.request_id,
        }
    }
}

fn retire_tick_attempt(
    attempt: &mut Option<(Option<PreparedWorkKey>, std::time::Instant)>,
    scope: &PageScope,
) {
    if attempt
        .as_ref()
        .is_some_and(|(key, _)| key.is_some_and(|key| key.generation == scope.generation))
    {
        *attempt = None;
    }
}

/// Allocation-free storage for one reducer-serialized operation artifact.
/// The payload is moved into the slot; only numeric owner identity is retained.
struct PreparedSlot<T>(Option<(PreparedWorkKey, T)>);

struct PreparedWasmArtifact {
    key: PreparedWorkKey,
    path: String,
    resource: ScopedResource,
}

/// The terminal-runtime allocations a test can force to behave as refused.
///
/// The runtime owns most of the client's retained remote memory — the page
/// canvas, the live-region table, the WASM batch, the history — and each is
/// admitted separately. Exhausting a governor refuses whichever of them the
/// budget happens to reach first, so it cannot show that refusing *this* owner
/// leaves the others intact. Naming the site can.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RunnerAllocationSite {
    /// A sub-buffer extracted from the composite: panel-transition halves and
    /// page-transition viewport snapshots.
    SubBuffer,
    /// One live region's row storage, reserved before the transactional swap.
    RegionRows,
    /// The prepared WASM batch's slot in the runtime.
    WasmBatch,
    /// The page canvas produced by splitting the laid-out buffer into its
    /// scrolling and sticky halves.
    PageCanvas,
    /// The dispatcher's fixed scheduled-action queue.
    EventQueue,
    /// A history entry's slot and retained AML.
    HistoryEntry,
}

#[cfg(test)]
thread_local! {
    static REJECT_RUNNER_ALLOCATION: std::cell::Cell<Option<RunnerAllocationSite>> =
        const { std::cell::Cell::new(None) };
}

/// Arms one runtime allocation site to refuse, and disarms it on drop.
#[cfg(test)]
pub(crate) struct RunnerRejectionGuard;

#[cfg(test)]
impl RunnerRejectionGuard {
    pub(crate) fn at(site: RunnerAllocationSite) -> Self {
        REJECT_RUNNER_ALLOCATION.with(|rejected| rejected.set(Some(site)));
        Self
    }
}

#[cfg(test)]
impl Drop for RunnerRejectionGuard {
    fn drop(&mut self) {
        REJECT_RUNNER_ALLOCATION.with(|rejected| rejected.set(None));
    }
}

#[cfg(test)]
pub(super) fn reject_runner_allocation(site: RunnerAllocationSite) -> bool {
    REJECT_RUNNER_ALLOCATION.with(|rejected| rejected.get() == Some(site))
}

/// Compiled away in release builds.
#[cfg(not(test))]
pub(super) fn reject_runner_allocation(_site: RunnerAllocationSite) -> bool {
    false
}

struct PreparedWasmBatch {
    entries: [Option<PreparedWasmArtifact>; crate::parser::MAX_WASM_INSTANCES],
    path_lease: Option<BudgetLease>,
    remaining_paths: usize,
    unassigned_path_bytes: usize,
}

impl Default for PreparedWasmBatch {
    fn default() -> Self {
        Self {
            entries: std::array::from_fn(|_| None),
            path_lease: None,
            remaining_paths: 0,
            unassigned_path_bytes: 0,
        }
    }
}

impl PreparedWasmBatch {
    fn admitted(
        path_lease: Option<BudgetLease>,
        remaining_paths: usize,
        unassigned_path_bytes: usize,
    ) -> Self {
        Self {
            path_lease,
            remaining_paths,
            unassigned_path_bytes,
            ..Self::default()
        }
    }

    fn try_store(
        &mut self,
        owner: &OperationOwner,
        path: String,
        resource: ScopedResource,
    ) -> Result<(), (String, ScopedResource)> {
        let key = PreparedWorkKey::from(owner);
        if self
            .entries
            .iter()
            .flatten()
            .any(|entry| entry.key == key || entry.path == path)
            || self.remaining_paths == 0
            || path.capacity() > self.unassigned_path_bytes
        {
            return Err((path, resource));
        }
        let Some(slot) = self.entries.iter_mut().find(|slot| slot.is_none()) else {
            return Err((path, resource));
        };
        let artifact = PreparedWasmArtifact {
            key,
            path,
            resource,
        };
        let path_capacity = artifact.path.capacity();
        *slot = Some(artifact);
        self.remaining_paths -= 1;
        self.unassigned_path_bytes -= path_capacity;
        Ok(())
    }

    fn contains_owner(&self, owner: &OperationOwner) -> bool {
        let key = PreparedWorkKey::from(owner);
        self.entries.iter().flatten().any(|entry| entry.key == key)
    }

    fn contains_path(&self, path: &str) -> bool {
        self.entries
            .iter()
            .flatten()
            .any(|entry| entry.path == path)
    }

    fn remove_owner(&mut self, owner: &OperationOwner) -> Option<PreparedWasmArtifact> {
        let key = PreparedWorkKey::from(owner);
        let slot = self
            .entries
            .iter_mut()
            .find(|slot| slot.as_ref().is_some_and(|entry| entry.key == key))?;
        slot.take()
    }

    fn reject_path(&mut self, path: &String) {
        let bytes = path.capacity();
        if self.remaining_paths == 0 || bytes > self.unassigned_path_bytes {
            return;
        }
        self.remaining_paths -= 1;
        self.unassigned_path_bytes -= bytes;
        self.release_path_bytes(bytes);
    }

    fn release_path_bytes(&mut self, bytes: usize) {
        if let Some(lease) = self.path_lease.as_mut() {
            lease.shrink_to(lease.amount().saturating_sub(bytes));
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.iter().flatten().count()
    }

    fn is_empty(&self) -> bool {
        self.entries.iter().all(Option::is_none)
    }
}

impl PreparedWasmSource for PreparedWasmBatch {
    fn get_prepared_wasm(&self, path: &str) -> Option<&[u8]> {
        self.entries
            .iter()
            .flatten()
            .find(|entry| entry.path == path)
            .map(|entry| entry.resource.bytes())
    }
}

fn try_owned_string(value: &str) -> Result<String, std::collections::TryReserveError> {
    let mut result = String::new();
    result.try_reserve_exact(value.len())?;
    result.push_str(value);
    Ok(result)
}

impl<T> Default for PreparedSlot<T> {
    fn default() -> Self {
        Self(None)
    }
}

impl<T> PreparedSlot<T> {
    fn try_store(&mut self, owner: &OperationOwner, value: T) -> Result<(), T> {
        if self.0.is_some() {
            return Err(value);
        }
        self.0 = Some((PreparedWorkKey::from(owner), value));
        Ok(())
    }

    fn get(&self, owner: &OperationOwner) -> Option<&T> {
        self.0
            .as_ref()
            .filter(|(key, _)| *key == PreparedWorkKey::from(owner))
            .map(|(_, value)| value)
    }

    fn take(&mut self, owner: &OperationOwner) -> Option<T> {
        if self
            .0
            .as_ref()
            .is_some_and(|(key, _)| *key == PreparedWorkKey::from(owner))
        {
            self.0.take().map(|(_, value)| value)
        } else {
            None
        }
    }

    fn take_key(&mut self, key: PreparedWorkKey) -> Option<T> {
        if self.0.as_ref().is_some_and(|(stored, _)| *stored == key) {
            return self.0.take().map(|(_, value)| value);
        }
        None
    }

    fn get_for_scope(&self, scope: &PageScope) -> Option<&T> {
        // Viewer generations are globally monotonic for this runtime, so the
        // generation uniquely identifies the live page after reducer dispatch.
        self.0
            .as_ref()
            .filter(|(key, _)| key.generation == scope.generation)
            .map(|(_, value)| value)
    }

    fn get_for_scope_mut(&mut self, scope: &PageScope) -> Option<&mut T> {
        self.0
            .as_mut()
            .filter(|(key, _)| key.generation == scope.generation)
            .map(|(_, value)| value)
    }

    fn clear_scope(&mut self, scope: &PageScope) {
        if self
            .0
            .as_ref()
            .is_some_and(|(key, _)| key.generation == scope.generation)
        {
            self.0 = None;
        }
    }

    fn take_for_scope(&mut self, scope: &PageScope) -> Option<T> {
        if self
            .0
            .as_ref()
            .is_some_and(|(key, _)| key.generation == scope.generation)
        {
            self.0.take().map(|(_, value)| value)
        } else {
            None
        }
    }

    fn is_some(&self) -> bool {
        self.0.is_some()
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.0.is_none()
    }

    #[cfg(test)]
    fn contains_key(&self, owner: &OperationOwner) -> bool {
        self.get(owner).is_some()
    }
}

fn active_history_id(lifecycle: &LifecycleModel) -> Option<crate::viewer::HistoryId> {
    lifecycle
        .history_position
        .and_then(|position| lifecycle.history.get(position))
        .map(|entry| entry.id)
}

fn set_active_history_transition(
    history: &mut [HistoryEntry],
    lifecycle: &LifecycleModel,
    transition: Option<ast::TransitionKind>,
    transition_duration_ms: u32,
) {
    let Some(id) = active_history_id(lifecycle) else {
        return;
    };
    let Some(artifact) = history.iter_mut().find(|artifact| artifact.id == id) else {
        return;
    };
    artifact.transition = transition;
    artifact.transition_duration_ms = transition_duration_ms;
}

fn logical_history_entry(
    lifecycle: &LifecycleModel,
    index: usize,
) -> Option<&crate::viewer::HistoryEntry> {
    lifecycle.history.get(index)
}

async fn load_cached(
    aml_content: &str,
    term_w: u16,
    term_h: u16,
    color_support: ColorSupport,
    wcfg: WidthConfig,
    client: Option<&mut AtpClient>,
    base_uri: Option<AtpUri>,
) -> Option<LoadedPage> {
    let governor = client.as_ref().map(|client| client.governor.clone());
    let (doc, parse_lease) = match governor.as_ref() {
        Some(governor) => {
            let (document, lease) = parse_remote_aml(aml_content, governor).ok()?;
            (document, Some(lease))
        }
        None => (parse_aml(aml_content)?, None),
    };
    layout_page_with_admission(
        &doc,
        term_w,
        term_h,
        color_support,
        wcfg,
        client,
        base_uri,
        None,
        parse_lease,
        None,
    )
    .await
    .ok()
}

/// Layout a parsed document into a LoadedPage.
#[cfg(test)]
async fn layout_page(
    doc: Document,
    term_w: u16,
    term_h: u16,
    color_support: ColorSupport,
    wcfg: WidthConfig,
    client: Option<&mut AtpClient>,
    base_uri: Option<AtpUri>,
    wasm_dir: Option<&std::path::Path>,
) -> LoadedPage {
    match layout_page_with_admission(
        &doc,
        term_w,
        term_h,
        color_support,
        wcfg,
        client,
        base_uri,
        wasm_dir,
        None,
        None,
    )
    .await
    {
        Ok(page) => page,
        Err(_) => {
            let error =
                parse_aml(RESOURCE_ERROR_AML).expect("built-in resource error AML must parse");
            let mut page = layout_page_with_admission(
                &error,
                term_w,
                term_h,
                color_support,
                wcfg,
                None,
                None,
                None,
                None,
                None,
            )
            .await
            .expect("fixed client error page must fit its independent budget");
            page.client_owned_error = true;
            page
        }
    }
}

async fn layout_page_with_admission(
    doc: &Document,
    term_w: u16,
    term_h: u16,
    color_support: ColorSupport,
    wcfg: WidthConfig,
    client: Option<&mut AtpClient>,
    _base_uri: Option<AtpUri>,
    wasm_dir: Option<&std::path::Path>,
    mut string_admission: Option<BudgetLease>,
    prepared_wasm: Option<&dyn PreparedWasmSource>,
) -> Result<LoadedPage, PagePreparationRejected> {
    let governor = client
        .as_ref()
        .map(|client| client.governor.clone())
        .unwrap_or_default();

    // Build and lay out before starting any remote animation/WASM. The caller
    // retains the exact parsed artifact until this complete candidate succeeds.
    let (
        mut scene,
        focusables,
        main_buf,
        sticky_buf,
        placed,
        sticky_regions,
        mut budget_leases,
        projection_lease,
    ) = {
        let ast_string_bytes = doc.retained_string_capacity();
        let (animation_regions, authored_frames) = animation_resource_counts(doc);
        let mut focusables = Vec::new();
        let mut scene = scene_mod::build::from_document_governed(doc, &governor);
        let lo = full_layout_pass(
            &mut scene,
            term_w,
            term_h,
            color_support,
            wcfg,
            &mut focusables,
            Some(&governor),
        );
        let (main_buf, sticky_buf) = split_sticky(
            lo.buffer,
            &lo.sticky_regions,
            &mut focusables,
            Some(&governor),
        );
        let layout_allocation_failed = main_buf.allocation_failed()
            || sticky_buf
                .as_ref()
                .is_some_and(CellBuffer::allocation_failed);
        hydrate_scene_buffers(&mut scene, &lo.placed);
        let transition_topology_admitted = scene.prepare_page_transition_overlay();
        let scene_cells = scene.buffer_cell_count();
        let compositor_cells = main_buf
            .cell_count()
            .saturating_add(sticky_buf.as_ref().map_or(0, CellBuffer::cell_count));
        let animation_lease =
            governor.reserve_with_cost(ResourceCategory::AnimationRegions, animation_regions, 0);
        let frame_lease =
            governor.reserve_with_cost(ResourceCategory::AuthoredFrames, authored_frames, 0);
        let retained_string_bytes =
            ast_string_bytes.saturating_add(scene.retained_string_capacity());
        let string_budget_rejected = match string_admission.as_mut() {
            Some(lease) => lease
                .try_resize_with_cost(retained_string_bytes, retained_string_bytes)
                .is_err(),
            None => match governor.reserve(ResourceCategory::AstStrings, retained_string_bytes) {
                Ok(lease) => {
                    string_admission = Some(lease);
                    false
                }
                Err(_) => true,
            },
        };
        let budget_rejected =
            animation_lease.is_err() || frame_lease.is_err() || string_budget_rejected;
        let rejected = layout_allocation_failed
            || !transition_topology_admitted
            || scene.resource_limit_exceeded()
            || scene_cells.saturating_add(compositor_cells)
                > crate::compositor::scene::tree::MAX_SCENE_CELLS
            || budget_rejected;
        if rejected {
            return Err(PagePreparationRejected { string_admission });
        }
        // `budget_rejected` above already tested both; bind through the same
        // test rather than asserting its conclusion.
        let (Ok(animation_lease), Ok(frame_lease)) = (animation_lease, frame_lease) else {
            return Err(PagePreparationRejected { string_admission });
        };
        (
            scene,
            focusables,
            main_buf,
            sticky_buf,
            lo.placed,
            lo.sticky_regions,
            vec![animation_lease, frame_lease],
            lo.projection_lease,
        )
    };
    let anim_rt = if let Some(prepared_wasm) = prepared_wasm {
        match AnimationRuntime::from_scene_with_prepared_wasm(
            &mut scene,
            color_support,
            wcfg,
            &governor,
            prepared_wasm,
        )
        .await
        {
            Ok(runtime) => runtime,
            Err(_) => return Err(PagePreparationRejected { string_admission }),
        }
    } else {
        AnimationRuntime::from_scene(&mut scene, color_support, wcfg, wasm_dir).await
    };

    if scene.resource_limit_exceeded() {
        return Err(PagePreparationRejected { string_admission });
    }
    let event_dispatcher =
        EventDispatcher::try_governed(&governor).map_err(|_| PagePreparationRejected {
            string_admission: string_admission.take(),
        })?;
    // The admission above succeeded, so the lease is present. If it somehow is
    // not, the page simply carries one fewer lease rather than aborting — the
    // governor releases what it holds either way.
    if let Some(lease) = string_admission.take() {
        budget_leases.push(lease);
    }

    Ok(LoadedPage {
        governor,
        client_owned_error: false,
        _budget_leases: budget_leases,
        _projection_lease: projection_lease,
        _sticky_regions: sticky_regions,
        focusables,
        buf: main_buf,
        anim_rt,
        prepared_wasm: PreparedWasmBatch::default(),
        wasm_dir: wasm_dir.map(std::path::Path::to_path_buf),
        placed,
        sticky_buf,
        scene,
        prepared_event_dispatcher: Some(event_dispatcher),
    })
}

fn animation_resource_counts(doc: &Document) -> (usize, usize) {
    fn visit(elements: &[ast::Element], regions: &mut usize, frames: &mut usize) {
        for element in elements {
            match element {
                ast::Element::Animate(_) => *regions += 1,
                ast::Element::Frame(_) => *frames += 1,
                _ => {}
            }
            visit(element.children(), regions, frames);
        }
    }

    let (mut regions, mut frames) = (0, 0);
    visit(&doc.page.children, &mut regions, &mut frames);
    (regions, frames)
}

fn wasm_dependency_paths(doc: &Document) -> Result<Vec<String>, std::collections::TryReserveError> {
    fn visit(
        elements: &[ast::Element],
        paths: &mut Vec<String>,
    ) -> Result<(), std::collections::TryReserveError> {
        for element in elements {
            if let ast::Element::Animate(animation) = element
                && let Some(path) = animation.src.as_ref()
                && !paths.iter().any(|existing| existing == path)
            {
                let mut owned = String::new();
                owned.try_reserve_exact(path.len())?;
                owned.push_str(path);
                paths.try_reserve(1)?;
                paths.push(owned);
            }
            visit(element.children(), paths)?;
        }
        Ok(())
    }

    let mut paths = Vec::new();
    visit(&doc.page.children, &mut paths)?;
    Ok(paths)
}

/// Advance focus to the next (or previous) focusable in
/// `page.focusables`. Looks up the current focus via
/// `current_focus_index(scene, focusables)`, computes the next index
/// with wraparound, emits `Patch::SetFocus` against the scene, and
/// scrolls the viewport to keep the new focusable visible.
///
/// `Scene.focus` is the sole authority. The render layer derives the
/// list index from it on each draw; there is no parallel `focus_index`
/// stored on `ViewportState`.
fn advance_focus(page: &mut LoadedPage, state: &mut ViewportState, forward: bool) {
    let current = current_focus_index(&page.scene, &page.focusables);
    let n = page.focusables.len();
    // `checked_sub` carries the emptiness check the explicit `is_empty` guard
    // used to: an empty list has no last index, so there is nothing to wrap to
    // and nothing to focus. It also proves `n > 0` for the `% n` below.
    let Some(last) = n.checked_sub(1) else { return };
    let next_idx = match (current, forward) {
        (Some(i), true) => (i + 1) % n,
        (Some(0), false) | (None, false) => last,
        (Some(i), false) => i.saturating_sub(1),
        (None, true) => 0,
    };
    let Some(next) = page.focusables.get(next_idx) else {
        return;
    };
    let next_node = next.node_id;
    let (is_sticky, row) = (next.is_sticky, next.row);
    PatchApplier::apply(
        &mut page.scene,
        Patch::SetFocus {
            node: Some(next_node),
        },
    );
    if !is_sticky {
        state.scroll_to_row(row);
    }
}

/// Project reducer-owned focus into the active scene without translating the
/// scene-local identity through an optional author-facing AML `id`.
fn project_scene_focus(scene: &mut Scene, focused: Option<NodeId>) -> bool {
    let target = focused.filter(|&node| scene.get(node).is_some());
    if scene.focus() == target {
        return false;
    }
    PatchApplier::apply(scene, Patch::SetFocus { node: target });
    true
}

fn authored_focus_action(
    focusable: &crate::compositor::panels::FocusableElement,
    focused: bool,
) -> PresentationAction {
    let source = focusable.id.clone();
    if focused {
        PresentationAction::Focus { source }
    } else {
        PresentationAction::Blur { source }
    }
}

/// Apply a panel state change as a `Patch::SetPanelActive` to the
/// scene. Returns `true` if the patch was applied (target state
/// exists and differs from current), `false` otherwise.
///
/// The scene's `NodeKind::Panel::active` is authoritative; layout
/// reads it directly via `kinds::panel::layout` and emits the updated
/// placement. No AST sync is needed.
fn apply_panel_patch(scene: &mut Scene, panel_id: &str, new_state_name: &str) -> bool {
    let Some(panel_node_id) = scene.find_by_aml_id(panel_id) else {
        return false;
    };
    let Some(panel_node) = scene.get(panel_node_id) else {
        return false;
    };
    let NodeKind::Panel { states, active, .. } = panel_node.kind() else {
        return false;
    };
    // Early-out: already on the requested state.
    if scene.get(*active).and_then(|n| n.aml_id()) == Some(new_state_name) {
        return false;
    }
    // Resolve the target state by matching its aml_id (the state name).
    let Some(&target) = states
        .iter()
        .find(|&&id| scene.get(id).and_then(|n| n.aml_id()) == Some(new_state_name))
    else {
        return false;
    };
    PatchApplier::apply(
        scene,
        Patch::SetPanelActive {
            panel: panel_node_id,
            active: target,
        },
    );
    true
}

/// Scene-native replacement for `PanelManager::toggle`. Advances the
/// panel's active state through `state_names` list; wraps around.
/// Returns `true` if the patch applied.
fn toggle_panel_state(scene: &mut Scene, panel_id: &str, state_names: &[String]) -> bool {
    if state_names.len() < 2 {
        return false;
    }
    let current = scene_panel_current_state(scene, panel_id);
    let current_idx = current
        .and_then(|value| state_names.iter().position(|state| state == value))
        .unwrap_or(0);
    let next_idx = (current_idx + 1) % state_names.len();
    let Some(next) = state_names.get(next_idx) else {
        return false;
    };
    apply_panel_patch(scene, panel_id, next)
}

fn toggle_panel_scene_state(scene: &mut Scene, panel_id: &str) -> bool {
    let Some(panel_node_id) = scene.find_by_aml_id(panel_id) else {
        return false;
    };
    let Some(panel) = scene.get(panel_node_id) else {
        return false;
    };
    let NodeKind::Panel { active, states, .. } = panel.kind() else {
        return false;
    };
    if states.len() < 2 {
        return false;
    }
    let current_idx = states.iter().position(|state| state == active).unwrap_or(0);
    let Some(&target) = states.get((current_idx + 1) % states.len()) else {
        return false;
    };
    PatchApplier::apply(
        scene,
        Patch::SetPanelActive {
            panel: panel_node_id,
            active: target,
        },
    );
    true
}

/// Scene-native replacement for `PanelManager::current_state`.
fn scene_panel_current_state<'a>(scene: &'a Scene, panel_id: &str) -> Option<&'a str> {
    let active = panel_active_node(scene, panel_id)?;
    scene.get(active).and_then(|n| n.aml_id())
}

fn animation_topology_changed(page: &LoadedPage) -> bool {
    let existing_count = page
        .anim_rt
        .animations
        .iter()
        .filter(|animation| animation.id() != crate::compositor::animate::PAGE_TRANSITION_ID)
        .count();
    let desired_count = page
        .scene
        .iter_tree_order()
        .filter(|node| {
            matches!(node.kind(), NodeKind::Animation(_))
                && page.scene.is_in_active_panel_state(node.id())
                && !node.placement().rect.is_empty()
        })
        .count();
    existing_count != desired_count
        || page.scene.iter_tree_order().any(|node| {
            matches!(node.kind(), NodeKind::Animation(_))
                && page.scene.is_in_active_panel_state(node.id())
                && !node.placement().rect.is_empty()
                && node.aml_id().is_some_and(|id| {
                    !page.anim_rt.animations.iter().any(|animation| {
                        animation.id() != crate::compositor::animate::PAGE_TRANSITION_ID
                            && animation.id() == id
                    })
                })
        })
}

fn try_panel_transition_id(panel_id: &str) -> Option<String> {
    #[cfg(test)]
    if REJECT_PANEL_TRANSITION_ID_ALLOCATION.with(|reject| reject.replace(false)) {
        return None;
    }
    let capacity = "trans-".len().checked_add(panel_id.len())?;
    let mut id = String::new();
    id.try_reserve_exact(capacity).ok()?;
    id.push_str("trans-");
    id.push_str(panel_id);
    Some(id)
}

#[cfg(test)]
thread_local! {
    static REJECT_PANEL_TRANSITION_ID_ALLOCATION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
#[test]
fn panel_transition_id_construction_is_bounded_and_fallible() {
    assert_eq!(
        try_panel_transition_id("main").as_deref(),
        Some("trans-main")
    );
    REJECT_PANEL_TRANSITION_ID_ALLOCATION.with(|reject| reject.set(true));
    assert!(try_panel_transition_id("main").is_none());
    assert_eq!(
        try_panel_transition_id("main").as_deref(),
        Some("trans-main")
    );
}

fn panel_active_node(scene: &Scene, panel_id: &str) -> Option<NodeId> {
    let panel_id = scene.find_by_aml_id(panel_id)?;
    let panel = scene.get(panel_id)?;
    let NodeKind::Panel { active, .. } = panel.kind() else {
        return None;
    };
    Some(*active)
}

/// Hydrate scene per-node buffers from a layout's placed regions.
/// nodes with buffers sized to their post-layout rects. Per-kind matches are
/// by `aml_id`, which every placed element carries (`PlacedElement.id`).
///
/// Buffers are opaque-empty (space cells with no bg); subsystems fill them
/// in during their respective writes. Cells that remain "transparent" (space
/// without decorations) pass through to lower compositor layers, which is
/// how a dialog over a running animation reveals the animation through its
/// gaps without a blending model (see compositor.md "Cells are binary-
/// present").
/// Hydrate scene per-node buffers from a layout's placed regions. For
/// each `PlacedElement` whose kind owns a buffer (Panel, Animation,
/// LiveRegion), (re)allocate a buffer of the right size on the
/// matching scene node and update its placement.
///
/// Safe to call repeatedly: `allocate_buffer` resizes in place.
///
/// After Phase A of the LayoutAccum refactor, `layout_scene` writes
/// `Node.placement` authoritatively during the layout pass — this
/// function no longer needs to call `update_placement`. Its sole job
/// is buffer allocation for Panel/Animation/Live kinds, which is
/// separate from placement: the buffer's size must match the rect
/// that layout produced, so the `PlacedElement` list is a natural
/// driver (it's already filtered to the buffer-owning kinds).
fn hydrate_scene_buffers(scene: &mut Scene, placed: &[PlacedElement]) {
    for p in placed {
        if let Some(node_id) = scene.find_by_aml_id(&p.id) {
            let kind_matches = match &p.kind {
                PlacedKind::Panel | PlacedKind::Animation { .. } | PlacedKind::Live { .. } => true,
            };
            if kind_matches && !p.rect.is_empty() {
                scene.ensure_buffer(node_id, p.rect.w, p.rect.h);
            }
        }
    }
}

// ─── Render pipeline stages ──────────────────────────────────
//
// The viewer loop's per-tick shape mirrors the 7-stage flow from
// `docs/internals/compositor.md`:
//
//   1. drain input events    → patches
//   2. advance time          → patches + buffer writes (anim_rt.tick)
//   3. drain protocol events → live-region deltas
//   4. apply patches         → scene mutation + Invalidation
//   5. layout pass           → scene-native layout (scoped via
//                              `layout_pass_invalidated` +
//                              `full_layout_pass` at nav/resize)
//   6. composite pass        → layer stack → output buffer
//   7. present pass          → ANSI emit
//   8. sleep to tick boundary
//
// Stages 1/2/3/4 run inline inside the main loop with comment
// markers; stages 5/6/7 are named helpers (`layout_pass_invalidated`,
// `composite_pass`, `present_pass`).

#[allow(clippy::too_many_arguments)]
fn present_pass(
    stdout: &mut impl Write,
    compositor: &mut Compositor,
    page: &mut LoadedPage,
    state: &ViewportState,
    status_uri: &str,
    security: Option<dustnet_core::protocol::origin::TransportSecurity>,
    connected: bool,
    config: &ClientConfig,
    color_support: ColorSupport,
    input_mode: &InputMode,
    command_line: &CommandLine,
    help_visible: bool,
    client_hud: &ClientHud,
    error_log: &ErrorLog,
    history: &[HistoryEntry],
    logical_history: &[crate::viewer::HistoryEntry],
    history_idx: usize,
) -> io::Result<bool> {
    // Stages 6 + 7 in sequence: composite layers to a single buffer,
    // then emit ANSI. Page transitions flow through the composite walk
    // like any other animation — they paint into a `NodeKind::Overlay`
    // node (see `PageTransitionAdapter` in `animate/page_transition.rs`)
    // that the walk picks up at Phase D. No bypass path.
    let Some(render_buf) = composite_pass(compositor, page, state) else {
        return Ok(false);
    };
    refresh_sticky_buffer(page, &render_buf);
    let page_title = page.scene.title.as_deref().unwrap_or("");
    let focus_idx = current_focus_index(&page.scene, &page.focusables);
    draw_viewer_frame(
        stdout,
        compositor,
        &render_buf,
        state,
        &page.focusables,
        focus_idx,
        status_uri,
        security,
        config,
        color_support,
        page_title,
        connected,
        input_mode,
        command_line,
        &page.sticky_buf,
        help_visible,
        client_hud,
        error_log,
        history,
        logical_history,
        history_idx,
        page.anim_rt.total_wasm_memory(),
    )?;
    Ok(true)
}

/// Stage 6 — composite pass. Walks the scene (or returns the
/// cached frame when the scene's composite invalidation is empty),
/// producing the frame for `draw_viewer_frame`.
///
/// Post Phase 3 of the composite-unification migration, the
/// `Compositor` retains the previous output; callers must clear
/// `scene.invalidation.{composite,present}` after this pass (and
/// after present has consumed them) so the next tick's short-circuit
/// check is accurate. See `compositor/composite.rs` for the ordering
/// rules.
fn composite_pass(
    compositor: &mut Compositor,
    page: &mut LoadedPage,
    state: &ViewportState,
) -> Option<SharedFrame> {
    compositor.set_governor(page.governor.clone());
    // During transitions the compositor must be at least viewport-
    // height so full-view effects aren't clipped.
    let full_page_height = page
        .buf
        .height
        .saturating_add(page.sticky_buf.as_ref().map_or(0, |buffer| buffer.height));
    let comp_h = if page.anim_rt.has_transitions() {
        full_page_height.max(state.viewport_height())
    } else {
        full_page_height
    };
    compositor.resize(state.term_w, comp_h);
    compositor.composite_at(&page.scene, &page.anim_rt, state.scroll_offset)
}

/// Mutable terminal-side authority. The reducer remains outside this type;
/// transport, loaded artifacts, projection state, and compositor state live
/// here for the lifetime of the ordered event loop.
pub(super) struct TerminalRuntime {
    page: LoadedPage,
    client: Option<AtpClient>,
    history: Vec<HistoryEntry>,
    compositor: Compositor,
    state: ViewportState,
    needs_redraw: bool,
    render_authorized: bool,
    input_mode: InputMode,
    event_dispatcher: EventDispatcher,
    deferred_navigation: Option<DeferredNavigation>,
    deferred_proposal: Option<DeferredProposal>,
    resumed_navigation: PreparedSlot<crate::compositor::panels::FocusAction>,
    pending_tick: Option<TickResult>,
    pending_tick_attempt: Option<(Option<PreparedWorkKey>, std::time::Instant)>,
    pending_page_transition: Option<PendingPageTransition>,
    local_page_activated: bool,
    #[cfg(test)]
    last_local_page_aml_ptr: Option<usize>,
    command_line: CommandLine,
    help_visible: bool,
    /// Set when the user has just pinned a certificate. The navigation that
    /// provoked the prompt cannot be resumed — its owner carries the origin
    /// computed before the pin existed, and pinning changes the origin — so it
    /// is re-issued from the top with a freshly derived one.
    retry_after_trust: Option<AtpUri>,
    /// Set when the user answered "no" to the trust prompt, so the initial
    /// navigation can report a declined certificate as the deliberate choice
    /// it was rather than as a failure to parse anything.
    declined_trust: bool,
    /// Why the last fetch failed, kept so the first navigation can report it.
    last_fetch_error: Option<String>,
    client_hud: ClientHud,
    error_log: ErrorLog,
    showing_overlay: bool,
    region_buffers: RegionBuffers,
    prepared_layout: Option<(PreparedWorkKey, PreparedLayout)>,
    prepared_wasm: PreparedSlot<()>,
    wasm_resources: PreparedSlot<PreparedWasmBatch>,
    fetched_pages: PreparedSlot<(AtpUri, String)>,
    parsed_pages: PreparedSlot<ParsedPage>,
    prepared_navigation: PreparedSlot<NavigationMetadata>,
    pending_history_artifact: Option<(PreparedWorkKey, PendingHistoryArtifact)>,
    activated_navigation: PreparedSlot<ActivatedNavigation>,
    pending_redirect_depth: Option<u8>,
    pending_updates: PreparedSlot<ScopedUpdate>,
    color_support: ColorSupport,
    wcfg: WidthConfig,
}

enum PreparedLayout {
    Page(Box<LoadedPage>),
    Projection,
}

struct DeferredProposal {
    generation: u64,
    wait_for: String,
    action: crate::compositor::panels::FocusAction,
}

impl TerminalRuntime {
    fn prepared_layout(&self, owner: &OperationOwner) -> Option<&PreparedLayout> {
        self.prepared_layout
            .as_ref()
            .filter(|(key, _)| *key == PreparedWorkKey::from(owner))
            .map(|(_, layout)| layout)
    }

    fn store_prepared_layout(&mut self, owner: &OperationOwner, layout: PreparedLayout) {
        self.prepared_layout = Some((PreparedWorkKey::from(owner), layout));
    }

    fn take_prepared_layout(&mut self, owner: &OperationOwner) -> Option<PreparedLayout> {
        if self
            .prepared_layout
            .as_ref()
            .is_some_and(|(key, _)| *key == PreparedWorkKey::from(owner))
        {
            self.prepared_layout.take().map(|(_, layout)| layout)
        } else {
            None
        }
    }

    fn take_pending_history_artifact(
        &mut self,
        owner: &OperationOwner,
    ) -> Option<PendingHistoryArtifact> {
        if self
            .pending_history_artifact
            .as_ref()
            .is_some_and(|(key, _)| *key == PreparedWorkKey::from(owner))
        {
            self.pending_history_artifact
                .take()
                .map(|(_, artifact)| artifact)
        } else {
            None
        }
    }

    fn store_pending_history_artifact(
        &mut self,
        owner: &OperationOwner,
        artifact: PendingHistoryArtifact,
    ) {
        self.pending_history_artifact = Some((PreparedWorkKey::from(owner), artifact));
    }
}

enum PresentationFailure {
    RetryOriginal(String),
    ResumeAuthored(String),
}

impl From<String> for PresentationFailure {
    fn from(message: String) -> Self {
        Self::RetryOriginal(message)
    }
}

impl From<&str> for PresentationFailure {
    fn from(message: &str) -> Self {
        Self::RetryOriginal(message.to_string())
    }
}

struct ParsedPage {
    document: Document,
    parse_lease: BudgetLease,
    final_uri: AtpUri,
    aml_content: Option<String>,
    cached_entry: Option<crate::viewer::HistoryEntry>,
}

enum NavigationMetadata {
    Network { final_uri: AtpUri },
    Cached { entry: crate::viewer::HistoryEntry },
}

enum ActivatedNavigation {
    Network { final_uri: AtpUri },
    Cached { entry: crate::viewer::HistoryEntry },
}

impl TerminalRuntime {
    fn new(
        mut page: LoadedPage,
        client: Option<AtpClient>,
        history: Vec<HistoryEntry>,
        term_w: u16,
        term_h: u16,
        color_support: ColorSupport,
        wcfg: WidthConfig,
    ) -> Self {
        let compositor = Compositor::with_governor(term_w, page.buf.height, page.governor.clone());
        let state = ViewportState::with_sticky(term_w, term_h, page.buf.height, &page.sticky_buf);
        let event_dispatcher = page
            .prepared_event_dispatcher
            .take()
            .unwrap_or_else(EventDispatcher::unadmitted);
        Self {
            page,
            client,
            history,
            compositor,
            state,
            needs_redraw: true,
            render_authorized: false,
            input_mode: InputMode {
                active: false,
                cursor_pos: 0,
                current_value: String::new(),
                current_node: None,
                maxlen: 0,
                password: false,
                field_col: 0,
                field_row: 0,
                field_is_sticky: false,
                wcfg,
            },
            event_dispatcher,
            deferred_navigation: None,
            deferred_proposal: None,
            resumed_navigation: PreparedSlot::default(),
            pending_tick: None,
            pending_tick_attempt: None,
            pending_page_transition: None,
            local_page_activated: false,
            #[cfg(test)]
            last_local_page_aml_ptr: None,
            command_line: CommandLine::new(),
            help_visible: false,
            retry_after_trust: None,
            declined_trust: false,
            last_fetch_error: None,
            client_hud: ClientHud::new(),
            error_log: ErrorLog::new(),
            showing_overlay: false,
            region_buffers: RegionBuffers::new(),
            prepared_layout: None,
            prepared_wasm: PreparedSlot::default(),
            pending_updates: PreparedSlot::default(),
            wasm_resources: PreparedSlot::default(),
            fetched_pages: PreparedSlot::default(),
            parsed_pages: PreparedSlot::default(),
            prepared_navigation: PreparedSlot::default(),
            pending_history_artifact: None,
            activated_navigation: PreparedSlot::default(),
            pending_redirect_depth: None,
            color_support,
            wcfg,
        }
    }

    fn take_activated_navigation(
        &mut self,
        scope: Option<&PageScope>,
    ) -> Option<ActivatedNavigation> {
        scope.and_then(|scope| self.activated_navigation.take_for_scope(scope))
    }

    fn prepare_resize_projection(
        &mut self,
        owner: OperationOwner,
        width: u16,
        height: u16,
    ) -> Vec<LifecycleEvent> {
        let rejected = |owner| {
            vec![LifecycleEvent::PresentationFailed {
                message: "resize projection exceeded the client resource budget".into(),
                retry: Some(PressureRetry::ResizeProjection {
                    owner,
                    width,
                    height,
                }),
            }]
        };
        if width != self.state.term_w || height != self.state.term_h {
            if !self.page.scene.begin_relayout_transaction() {
                return rejected(owner);
            }
            let mut focusables = Vec::new();
            let lo = full_layout_pass(
                &mut self.page.scene,
                width,
                height,
                self.color_support,
                self.wcfg,
                &mut focusables,
                Some(&self.page.governor),
            );
            let (main_buf, sticky_buf) = split_sticky(
                lo.buffer,
                &lo.sticky_regions,
                &mut focusables,
                Some(&self.page.governor),
            );
            let allocation_failed = main_buf.allocation_failed()
                || sticky_buf
                    .as_ref()
                    .is_some_and(CellBuffer::allocation_failed)
                || self.page.scene.resource_limit_exceeded();
            if allocation_failed {
                self.page.scene.rollback_relayout_transaction();
                return rejected(owner);
            }
            hydrate_scene_buffers(&mut self.page.scene, &lo.placed);
            if self.page.scene.resource_limit_exceeded() {
                self.page.scene.rollback_relayout_transaction();
                return rejected(owner);
            }
            let animation_resize = match self.page.anim_rt.prepare_resize(&self.page.scene) {
                Ok(prepared) => prepared,
                Err(_) => {
                    self.page.scene.rollback_relayout_transaction();
                    return rejected(owner);
                }
            };

            let old_scroll = self.state.scroll_offset;
            self.page.scene.commit_relayout_transaction();
            self.page
                .anim_rt
                .cancel_page_transition(&mut self.page.scene);
            self.page.anim_rt.cancel_transitions_for_resize();
            self.pending_page_transition = None;
            self.page.buf = main_buf;
            self.page.sticky_buf = sticky_buf;
            self.page.focusables = focusables;
            self.page.placed = lo.placed;
            self.page._sticky_regions = lo.sticky_regions;
            self.page._projection_lease = lo.projection_lease;
            self.page
                .anim_rt
                .commit_resize(&mut self.page.scene, animation_resize);
            self.state = ViewportState::with_sticky(
                width,
                height,
                self.page.buf.height,
                &self.page.sticky_buf,
            );
            self.state.scroll_offset = old_scroll.min(self.state.max_scroll());
        }
        let content_height = u32::from(self.page.buf.height);
        self.store_prepared_layout(&owner, PreparedLayout::Projection);
        vec![LifecycleEvent::ResizeProjectionPrepared {
            owner,
            content_height,
        }]
    }

    /// Execute one reducer-issued mutation against terminal-owned state.
    /// Completions are returned to the outer dispatcher; this method never
    /// mutates `ViewerModel`.
    pub(super) async fn execute(
        &mut self,
        effect: LifecycleEffect,
        model: &LifecycleModel,
    ) -> Result<Vec<LifecycleEvent>, ViewerError> {
        let effect = match navigation::classify(effect) {
            navigation::ClassifiedEffect::Recovery(effect) => {
                return navigation::execute_renderer_recovery(self, model, effect).await;
            }
            navigation::ClassifiedEffect::Other(effect) => effect,
        };
        match effect {
            LifecycleEffect::Close => {
                if let Some(client) = self.client.as_mut() {
                    client.disconnect().await;
                }
                Ok(Vec::new())
            }
            LifecycleEffect::ApplyPresentationAction { scope, action } if model.scope == scope => {
                if matches!(&action, PresentationAction::ActivateLocalPage { .. }) {
                    return match self.apply_presentation_action(action).await {
                        Ok(()) => Ok(Vec::new()),
                        Err(failure) => Ok(vec![LifecycleEvent::PresentationFailed {
                            message: match failure {
                                PresentationFailure::RetryOriginal(message)
                                | PresentationFailure::ResumeAuthored(message) => message,
                            },
                            retry: None,
                        }]),
                    };
                }
                let retry_action = match action.try_clone() {
                    Ok(retry) => retry,
                    Err(_) => {
                        return Ok(vec![LifecycleEvent::PresentationFailed {
                            message: "presentation retry allocation failed".into(),
                            retry: Some(PressureRetry::Presentation { scope, action }),
                        }]);
                    }
                };
                match self.apply_presentation_action(action).await {
                    Ok(()) => Ok(Vec::new()),
                    Err(failure) => Ok(vec![LifecycleEvent::PresentationFailed {
                        message: match &failure {
                            PresentationFailure::RetryOriginal(message)
                            | PresentationFailure::ResumeAuthored(message) => message.clone(),
                        },
                        retry: Some(PressureRetry::Presentation {
                            scope,
                            action: match failure {
                                PresentationFailure::RetryOriginal(_) => retry_action,
                                PresentationFailure::ResumeAuthored(_) => {
                                    PresentationAction::TickPendingActions
                                }
                            },
                        }),
                    }]),
                }
            }
            LifecycleEffect::ApplyPresentationAction { .. } => Ok(Vec::new()),
            LifecycleEffect::AdmitHistoryArtifact {
                owner,
                id,
                replacing,
            } if model.scope.as_ref() == Some(&owner.scope) => {
                let Some(mut pending) = self.take_pending_history_artifact(&owner) else {
                    return Ok(Vec::new());
                };
                if pending.id.is_some_and(|pending_id| pending_id != id) {
                    return Ok(Vec::new());
                }
                pending.id = Some(id);
                let admitted = if replacing {
                    if let Some(existing) = self.history.iter_mut().find(|entry| entry.id == id) {
                        if let Some(lease) = existing._budget_lease.as_mut() {
                            if lease
                                .try_resize_with_cost(
                                    pending.retained_bytes,
                                    pending.retained_bytes,
                                )
                                .is_ok()
                            {
                                pending.budget_lease = existing._budget_lease.take();
                                true
                            } else {
                                false
                            }
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                } else {
                    !reject_runner_allocation(RunnerAllocationSite::HistoryEntry)
                        && self.history.try_reserve(1).is_ok()
                        && self.client.as_ref().is_some_and(|client| {
                            match client
                                .governor
                                .reserve(ResourceCategory::History, pending.retained_bytes)
                            {
                                Ok(lease) => {
                                    pending.budget_lease = Some(lease);
                                    true
                                }
                                Err(_) => false,
                            }
                        })
                };
                self.store_pending_history_artifact(&owner, pending);
                if admitted {
                    Ok(vec![LifecycleEvent::HistoryCommitted { owner, id }])
                } else {
                    Ok(vec![LifecycleEvent::PresentationFailed {
                        message: "history retention exceeded the client resource budget".into(),
                        retry: Some(PressureRetry::HistoryArtifact {
                            owner,
                            id,
                            replacing,
                        }),
                    }])
                }
            }
            LifecycleEffect::AdmitHistoryArtifact { .. } => Ok(Vec::new()),
            LifecycleEffect::InstallHistoryArtifact { owner, id }
                if model.scope.as_ref() == Some(&owner.scope) =>
            {
                let content_height = match self.prepared_layout(&owner) {
                    Some(PreparedLayout::Page(page)) => u32::from(page.buf.height),
                    _ => return Ok(Vec::new()),
                };
                let Some(pending) = self.take_pending_history_artifact(&owner) else {
                    return Ok(Vec::new());
                };
                if pending.id != Some(id) || pending.budget_lease.is_none() {
                    return Ok(Vec::new());
                }
                assert!(
                    self.history.iter().all(|artifact| artifact.id != id),
                    "history artifact installation must follow exact reducer release"
                );
                self.history.push(HistoryEntry {
                    id,
                    _retained_bytes: pending.retained_bytes,
                    _budget_lease: pending.budget_lease,
                    title: pending.title,
                    transition: pending.transition,
                    transition_duration_ms: pending.transition_duration_ms,
                });
                Ok(vec![LifecycleEvent::LayoutPrepared {
                    owner,
                    content_height,
                }])
            }
            LifecycleEffect::InstallHistoryArtifact { .. } => Ok(Vec::new()),
            LifecycleEffect::ActivateLayout { owner }
                if model.scope.as_ref() == Some(&owner.scope) =>
            {
                if self.activated_navigation.is_some() {
                    return Ok(Vec::new());
                }
                let Some(prepared) = self.take_prepared_layout(&owner) else {
                    return Ok(Vec::new());
                };
                if let PreparedLayout::Page(page) = prepared {
                    let mut page = *page;
                    self.event_dispatcher = page
                        .prepared_event_dispatcher
                        .take()
                        .unwrap_or_else(EventDispatcher::unadmitted);
                    self.region_buffers.clear();
                    self.page = page;
                    self.state = ViewportState::with_sticky(
                        self.state.term_w,
                        self.state.term_h,
                        self.page.buf.height,
                        &self.page.sticky_buf,
                    );
                    invalidate_compositor_for_new_scene(&mut self.compositor);
                }
                if let Some(metadata) = self.prepared_navigation.take(&owner) {
                    let activated = match metadata {
                        NavigationMetadata::Network { final_uri } => {
                            ActivatedNavigation::Network { final_uri }
                        }
                        NavigationMetadata::Cached { entry } => {
                            ActivatedNavigation::Cached { entry }
                        }
                    };
                    let stored = self.activated_navigation.try_store(&owner, activated);
                    assert!(
                        stored.is_ok(),
                        "empty activated-navigation slot must accept one value"
                    );
                }
                Ok(vec![LifecycleEvent::LayoutActivated { owner }])
            }
            LifecycleEffect::ActivateWasm { owner }
                if model.owns_operation(&owner) && self.prepared_wasm.take(&owner).is_some() =>
            {
                Ok(vec![LifecycleEvent::WasmActivated { owner }])
            }
            LifecycleEffect::ActivateLayout { .. } | LifecycleEffect::ActivateWasm { .. } => {
                Ok(Vec::new())
            }
            LifecycleEffect::RestoreTerminal => Ok(Vec::new()),
            LifecycleEffect::Connect { owner } => {
                // Initial navigation connects as part of its owned fetch. A
                // standalone Connect while Ready is a transport recovery.
                if model.phase == NavigationPhase::Connecting {
                    return Ok(vec![LifecycleEvent::Connected { owner }]);
                }
                let Some(uri) = model.current_uri.as_ref() else {
                    return Ok(vec![LifecycleEvent::ConnectionFailed {
                        owner,
                        message: "cannot reconnect without a current URI".into(),
                    }]);
                };
                let Some(client) = self.client.as_mut() else {
                    return Ok(vec![LifecycleEvent::ConnectionFailed {
                        owner,
                        message: "cannot reconnect a local viewer".into(),
                    }]);
                };
                match client.reconnect_current_page(uri).await {
                    Ok(()) => Ok(vec![LifecycleEvent::Connected { owner }]),
                    Err(error) => Ok(vec![LifecycleEvent::ConnectionFailed {
                        owner,
                        message: error.to_string(),
                    }]),
                }
            }
            LifecycleEffect::Fetch { owner, uri } => {
                let Some(client) = self.client.as_mut() else {
                    return Ok(vec![LifecycleEvent::FetchFailed {
                        owner,
                        message: "cannot fetch from a local viewer".into(),
                    }]);
                };
                let Ok(request_owner) = owner.try_clone() else {
                    return Ok(vec![LifecycleEvent::FetchFailed {
                        owner,
                        message: "fetch owner exhausted client memory".into(),
                    }]);
                };
                let attempt = client.fetch(request_owner, &uri).await;
                // No authority vouches for this site and nothing is pinned for
                // it. Put it to the user before anything else happens: the
                // borrow of `client` ends here so the prompt can pin through
                // it, and the navigation is failed either way — accepting
                // changes the origin, so it is re-issued rather than resumed.
                if let Err(crate::client::ClientError::TrustDecisionRequired {
                    host,
                    port,
                    fingerprint,
                    reason,
                }) = &attempt
                {
                    let trusted = prompt_for_trust(&self.state, host, *port, fingerprint, reason)?;
                    // The modal painted straight to the terminal, over the
                    // composited page. Force a repaint of whatever is beneath.
                    self.compositor.invalidate_presented();
                    let message = if trusted {
                        let (host, port, fingerprint) = (host.clone(), *port, *fingerprint);
                        match self
                            .client
                            .as_mut()
                            .map(|client| client.pin_peer(&host, port, fingerprint))
                        {
                            Some(Ok(())) => {
                                self.retry_after_trust = uri.try_clone().ok();
                                "certificate pinned; reconnecting".to_owned()
                            }
                            Some(Err(error)) => format!("cannot record the pin: {error}"),
                            None => "cannot pin from a local viewer".to_owned(),
                        }
                    } else {
                        self.declined_trust = true;
                        "cancelled: certificate not trusted".to_owned()
                    };
                    return Ok(vec![LifecycleEvent::FetchFailed { owner, message }]);
                }
                let response = match attempt {
                    Ok(response) => response,
                    Err(error) => {
                        let message = error.to_string();
                        self.last_fetch_error = Some(message.clone());
                        return Ok(vec![LifecycleEvent::FetchFailed { owner, message }]);
                    }
                };
                match response {
                    NavigationResponse::Page(result)
                        if result.scope == owner.scope && result.request_id == owner.request_id =>
                    {
                        self.pending_redirect_depth = None;
                        if self
                            .fetched_pages
                            .try_store(&owner, (result.final_uri, result.aml_content))
                            .is_err()
                        {
                            return Ok(vec![LifecycleEvent::FetchFailed {
                                owner,
                                message: "serialized fetched-page slot was already occupied".into(),
                            }]);
                        }
                        Ok(vec![LifecycleEvent::FetchCompleted { owner }])
                    }
                    NavigationResponse::Redirect(redirect)
                        if redirect.scope == owner.scope
                            && redirect.request_id == owner.request_id =>
                    {
                        let depth = self
                            .pending_redirect_depth
                            .take()
                            .unwrap_or(0)
                            .saturating_add(1);
                        if depth > 5 {
                            return Ok(vec![LifecycleEvent::FetchFailed {
                                owner,
                                message: "too many redirects".into(),
                            }]);
                        }
                        self.pending_redirect_depth = Some(depth);
                        match client.request_origin(&redirect.target) {
                            Ok(origin) => Ok(vec![LifecycleEvent::Redirect {
                                uri: redirect.target,
                                origin,
                            }]),
                            Err(error) => Ok(vec![LifecycleEvent::FetchFailed {
                                owner,
                                message: error.to_string(),
                            }]),
                        }
                    }
                    _ => Ok(vec![LifecycleEvent::FetchFailed {
                        owner,
                        message: "transport returned a completion for the wrong owner".into(),
                    }]),
                }
            }
            LifecycleEffect::Submit {
                owner,
                uri,
                path,
                form_data,
            } => {
                let Some(client) = self.client.as_mut() else {
                    return Ok(vec![LifecycleEvent::FetchFailed {
                        owner,
                        message: "cannot submit from a local viewer".into(),
                    }]);
                };
                let Ok(request_owner) = owner.try_clone() else {
                    return Ok(vec![LifecycleEvent::FetchFailed {
                        owner,
                        message: "submit owner exhausted client memory".into(),
                    }]);
                };
                let response = match client.submit(request_owner, &uri, &path, &form_data).await {
                    Ok(response) => response,
                    Err(error) => {
                        return Ok(vec![LifecycleEvent::FetchFailed {
                            owner,
                            message: error.to_string(),
                        }]);
                    }
                };
                match response {
                    NavigationResponse::Page(result)
                        if result.scope == owner.scope && result.request_id == owner.request_id =>
                    {
                        self.pending_redirect_depth = None;
                        if self
                            .fetched_pages
                            .try_store(&owner, (result.final_uri, result.aml_content))
                            .is_err()
                        {
                            return Ok(vec![LifecycleEvent::FetchFailed {
                                owner,
                                message: "serialized fetched-page slot was already occupied".into(),
                            }]);
                        }
                        Ok(vec![LifecycleEvent::FetchCompleted { owner }])
                    }
                    NavigationResponse::Redirect(redirect)
                        if redirect.scope == owner.scope
                            && redirect.request_id == owner.request_id =>
                    {
                        let depth = self
                            .pending_redirect_depth
                            .take()
                            .unwrap_or(0)
                            .saturating_add(1);
                        if depth > 5 {
                            return Ok(vec![LifecycleEvent::FetchFailed {
                                owner,
                                message: "too many redirects".into(),
                            }]);
                        }
                        self.pending_redirect_depth = Some(depth);
                        match client.request_origin(&redirect.target) {
                            Ok(origin) => Ok(vec![LifecycleEvent::Redirect {
                                uri: redirect.target,
                                origin,
                            }]),
                            Err(error) => Ok(vec![LifecycleEvent::FetchFailed {
                                owner,
                                message: error.to_string(),
                            }]),
                        }
                    }
                    _ => Ok(vec![LifecycleEvent::FetchFailed {
                        owner,
                        message: "transport returned a completion for the wrong owner".into(),
                    }]),
                }
            }
            LifecycleEffect::Subscribe { owner, region } => {
                let Some(placed) = self.page.placed.get(region.placed_index()) else {
                    return Ok(vec![LifecycleEvent::SubscriptionFailed { owner }]);
                };
                let PlacedKind::Live {
                    endpoint,
                    scroll: _,
                    buffer: _,
                    delta,
                } = &placed.kind
                else {
                    return Ok(vec![LifecycleEvent::SubscriptionFailed { owner }]);
                };
                let mode = if *delta {
                    SubscribeMode::Delta
                } else {
                    SubscribeMode::Replace
                };
                let Some(client) = self.client.as_mut() else {
                    return Ok(vec![LifecycleEvent::SubscriptionFailed { owner }]);
                };
                let Ok(request_owner) = owner.try_clone() else {
                    return Ok(vec![LifecycleEvent::SubscriptionFailed { owner }]);
                };
                match client
                    .subscribe_for_projection(request_owner, region, endpoint, &placed.id, mode)
                    .await
                {
                    Ok(()) => Ok(vec![LifecycleEvent::SubscriptionCompleted {
                        owner,
                        region,
                    }]),
                    Err(error) => {
                        eprintln!("subscribe error: {error}");
                        Ok(vec![LifecycleEvent::SubscriptionFailed { owner }])
                    }
                }
            }
            LifecycleEffect::RetireSubscriptions { scope } => {
                if model.scope.as_ref() != Some(&scope) {
                    return Ok(Vec::new());
                }
                if let Some(client) = self.client.as_mut()
                    && client.active_subscription_count() != 0
                {
                    let _ = client.unsubscribe().await;
                }
                self.region_buffers.clear();
                Ok(Vec::new())
            }
            LifecycleEffect::Parse { owner } => {
                let Some((final_uri, fetched_aml)) = self.fetched_pages.take(&owner) else {
                    return Ok(Vec::new());
                };
                let Some(client) = self.client.as_ref() else {
                    let restored = self
                        .fetched_pages
                        .try_store(&owner, (final_uri, fetched_aml));
                    assert!(restored.is_ok(), "taken fetched slot must restore exactly");
                    return Ok(vec![LifecycleEvent::ParseFailed {
                        owner,
                        message: "cannot parse a remote page without its resource governor".into(),
                    }]);
                };
                let (document, parse_lease) = match parse_remote_aml(&fetched_aml, &client.governor)
                {
                    Ok(parsed) => parsed,
                    Err(RemoteParseError::Invalid) => {
                        return Ok(vec![LifecycleEvent::ParseFailed {
                            owner,
                            message: "server returned an invalid page".into(),
                        }]);
                    }
                    Err(RemoteParseError::ResourceRejected) => {
                        let restored = self
                            .fetched_pages
                            .try_store(&owner, (final_uri, fetched_aml));
                        assert!(restored.is_ok(), "taken fetched slot must restore exactly");
                        return Ok(vec![LifecycleEvent::PresentationFailed {
                            message: "page parsing exceeded the client resource budget".into(),
                            retry: Some(PressureRetry::Parse { owner }),
                        }]);
                    }
                };
                let paths = match wasm_dependency_paths(&document) {
                    Ok(paths) => paths,
                    Err(_) => {
                        let restored = self
                            .fetched_pages
                            .try_store(&owner, (final_uri, fetched_aml));
                        assert!(restored.is_ok(), "taken fetched slot must restore exactly");
                        return Ok(vec![LifecycleEvent::PresentationFailed {
                            message: "WASM dependency discovery exhausted client memory".into(),
                            retry: Some(PressureRetry::Parse { owner }),
                        }]);
                    }
                };
                let path_bytes = match paths
                    .iter()
                    .try_fold(0usize, |total, path| total.checked_add(path.capacity()))
                {
                    Some(bytes) => bytes,
                    None => {
                        let restored = self
                            .fetched_pages
                            .try_store(&owner, (final_uri, fetched_aml));
                        assert!(restored.is_ok(), "taken fetched slot must restore exactly");
                        return Ok(vec![LifecycleEvent::PresentationFailed {
                            message: "WASM dependency metadata exceeded client limits".into(),
                            retry: Some(PressureRetry::Parse { owner }),
                        }]);
                    }
                };
                let path_lease = if path_bytes == 0 {
                    None
                } else {
                    match client
                        .governor
                        .reserve(ResourceCategory::RemoteCollections, path_bytes)
                        .map_err(Some)
                        .and_then(|lease| {
                            if reject_runner_allocation(RunnerAllocationSite::WasmBatch) {
                                Err(None)
                            } else {
                                Ok(lease)
                            }
                        }) {
                        Ok(lease) => Some(lease),
                        Err(_) => {
                            let restored = self
                                .fetched_pages
                                .try_store(&owner, (final_uri, fetched_aml));
                            assert!(restored.is_ok(), "taken fetched slot must restore exactly");
                            return Ok(vec![LifecycleEvent::PresentationFailed {
                                message:
                                    "WASM dependency metadata exceeded the client resource budget"
                                        .into(),
                                retry: Some(PressureRetry::Parse { owner }),
                            }]);
                        }
                    }
                };
                let event_owner = match owner.try_clone() {
                    Ok(owner) => owner,
                    Err(_) => {
                        let restored = self
                            .fetched_pages
                            .try_store(&owner, (final_uri, fetched_aml));
                        assert!(restored.is_ok(), "taken fetched slot must restore exactly");
                        return Ok(vec![LifecycleEvent::PresentationFailed {
                            message: "WASM dependency owner exhausted client memory".into(),
                            retry: Some(PressureRetry::Parse { owner }),
                        }]);
                    }
                };
                if self
                    .wasm_resources
                    .try_store(
                        &owner,
                        PreparedWasmBatch::admitted(path_lease, paths.len(), path_bytes),
                    )
                    .is_err()
                {
                    let restored = self
                        .fetched_pages
                        .try_store(&owner, (final_uri, fetched_aml));
                    assert!(restored.is_ok(), "taken fetched slot must restore exactly");
                    return Ok(vec![LifecycleEvent::ParseFailed {
                        owner,
                        message: "serialized WASM batch slot was already occupied".into(),
                    }]);
                }
                if self
                    .parsed_pages
                    .try_store(
                        &owner,
                        ParsedPage {
                            document,
                            parse_lease,
                            final_uri,
                            aml_content: Some(fetched_aml),
                            cached_entry: None,
                        },
                    )
                    .is_err()
                {
                    let removed = self.wasm_resources.take(&owner);
                    assert!(
                        removed.is_some(),
                        "failed parse storage must release its WASM batch"
                    );
                    return Ok(vec![LifecycleEvent::ParseFailed {
                        owner,
                        message: "serialized parsed-page slot was already occupied".into(),
                    }]);
                }
                Ok(vec![
                    LifecycleEvent::WasmDependenciesDiscovered {
                        owner: event_owner,
                        paths,
                    },
                    LifecycleEvent::ParseCompleted { owner },
                ])
            }
            LifecycleEffect::PrepareResizeProjection {
                owner,
                width,
                height,
            } if model.owns_resize_projection(&owner, width, height) => {
                if matches!(
                    self.prepared_layout(&owner),
                    Some(PreparedLayout::Projection)
                ) {
                    return Ok(vec![LifecycleEvent::ResizeProjectionPrepared {
                        owner,
                        content_height: u32::from(self.page.buf.height),
                    }]);
                }
                Ok(self.prepare_resize_projection(owner, width, height))
            }
            LifecycleEffect::PrepareResizeProjection { .. } => Ok(Vec::new()),
            LifecycleEffect::PrepareLayout { owner } => {
                let Some(mut parsed) = self.parsed_pages.take(&owner) else {
                    return Ok(Vec::new());
                };
                let base_uri = match parsed.final_uri.try_clone() {
                    Ok(uri) => uri,
                    Err(_) => {
                        let restored = self.parsed_pages.try_store(&owner, parsed);
                        assert!(restored.is_ok(), "taken parsed slot must restore exactly");
                        return Ok(vec![LifecycleEvent::PresentationFailed {
                            message: "page URI preparation exhausted client memory".into(),
                            retry: Some(PressureRetry::PrepareLayout { owner }),
                        }]);
                    }
                };
                let prepared_final_uri = if parsed.cached_entry.is_none() {
                    match parsed.final_uri.try_clone() {
                        Ok(uri) => Some(uri),
                        Err(_) => {
                            let restored = self.parsed_pages.try_store(&owner, parsed);
                            assert!(restored.is_ok(), "taken parsed slot must restore exactly");
                            return Ok(vec![LifecycleEvent::PresentationFailed {
                                message: "navigation metadata exhausted client memory".into(),
                                retry: Some(PressureRetry::PrepareLayout { owner }),
                            }]);
                        }
                    }
                } else {
                    None
                };
                let prepared_wasm = self.wasm_resources.get_for_scope(&owner.scope);
                let mut page = match layout_page_with_admission(
                    &parsed.document,
                    self.state.term_w,
                    self.state.term_h,
                    self.color_support,
                    self.wcfg,
                    self.client.as_mut(),
                    Some(base_uri),
                    None,
                    Some(parsed.parse_lease),
                    prepared_wasm.map(|batch| batch as &dyn PreparedWasmSource),
                )
                .await
                {
                    Ok(page) => page,
                    Err(PagePreparationRejected { string_admission }) => {
                        let Some(returned_lease) = string_admission else {
                            return Ok(vec![LifecycleEvent::PresentationFailed {
                                message: "page preparation lost its parse lease".into(),
                                retry: None,
                            }]);
                        };
                        parsed.parse_lease = returned_lease;
                        let restored = self.parsed_pages.try_store(&owner, parsed);
                        assert!(restored.is_ok(), "taken parsed slot must restore exactly");
                        return Ok(vec![LifecycleEvent::PresentationFailed {
                            message: "page preparation exceeded the client resource budget".into(),
                            retry: Some(PressureRetry::PrepareLayout { owner }),
                        }]);
                    }
                };
                page.prepared_wasm = self
                    .wasm_resources
                    .take_for_scope(&owner.scope)
                    .unwrap_or_default();
                let content_height = u32::from(page.buf.height);
                let title = page.scene.title.clone().unwrap_or_else(|| Arc::from(""));
                if let Some(entry) = parsed.cached_entry {
                    let stored = self
                        .prepared_navigation
                        .try_store(&owner, NavigationMetadata::Cached { entry });
                    assert!(stored.is_ok(), "serialized navigation slot must be empty");
                } else {
                    if let Some(final_uri) = prepared_final_uri {
                        let stored = self
                            .prepared_navigation
                            .try_store(&owner, NavigationMetadata::Network { final_uri });
                        debug_assert!(stored.is_ok(), "serialized navigation slot must be empty");
                    }
                }
                self.store_prepared_layout(&owner, PreparedLayout::Page(Box::new(page)));
                if matches!(
                    self.prepared_navigation.get(&owner),
                    Some(NavigationMetadata::Network { .. })
                ) && let Some(retained_aml) = parsed.aml_content
                {
                    self.store_pending_history_artifact(
                        &owner,
                        PendingHistoryArtifact {
                            id: None,
                            retained_bytes: retained_aml.capacity().saturating_add(title.len()),
                            budget_lease: None,
                            title,
                            transition: None,
                            transition_duration_ms: 0,
                        },
                    );
                    return Ok(vec![LifecycleEvent::HistoryAdmissionRequested {
                        owner,
                        uri: parsed.final_uri,
                        retained_aml,
                    }]);
                }
                Ok(vec![LifecycleEvent::LayoutPrepared {
                    owner,
                    content_height,
                }])
            }
            LifecycleEffect::LoadWasm { owner, path } if model.owns_operation(&owner) => {
                if self.wasm_resources.get_for_scope(&owner.scope).is_none() {
                    return Ok(vec![LifecycleEvent::WasmRejected { owner }]);
                }
                if self.prepared_wasm.is_some()
                    && let Some(batch) = self.wasm_resources.get_for_scope(&owner.scope)
                {
                    if batch.contains_owner(&owner) {
                        return Ok(Vec::new());
                    }
                    if batch.contains_path(&path) {
                        return Ok(vec![LifecycleEvent::WasmRejected { owner }]);
                    }
                    if let Some(batch) = self.wasm_resources.get_for_scope_mut(&owner.scope) {
                        batch.reject_path(&path);
                    }
                    return Ok(vec![LifecycleEvent::WasmRejected { owner }]);
                }
                let Some(base_uri) = self
                    .parsed_pages
                    .get_for_scope(&owner.scope)
                    .and_then(|page| page.final_uri.try_clone().ok())
                else {
                    if let Some(batch) = self.wasm_resources.get_for_scope_mut(&owner.scope) {
                        batch.reject_path(&path);
                    }
                    return Ok(vec![LifecycleEvent::WasmRejected { owner }]);
                };
                let Some(client) = self.client.as_mut() else {
                    if let Some(batch) = self.wasm_resources.get_for_scope_mut(&owner.scope) {
                        batch.reject_path(&path);
                    }
                    return Ok(vec![LifecycleEvent::WasmRejected { owner }]);
                };
                let fetch_owner = match owner.try_clone() {
                    Ok(owner) => owner,
                    Err(_) => {
                        if let Some(batch) = self.wasm_resources.get_for_scope_mut(&owner.scope) {
                            batch.reject_path(&path);
                        }
                        return Ok(vec![LifecycleEvent::WasmRejected { owner }]);
                    }
                };
                match client.fetch_resource(fetch_owner, &base_uri, &path).await {
                    Ok(resource)
                        if resource.scope == owner.scope
                            && resource.request_id == owner.request_id =>
                    {
                        let Some(batch) = self.wasm_resources.get_for_scope_mut(&owner.scope)
                        else {
                            return Ok(vec![LifecycleEvent::WasmRejected { owner }]);
                        };
                        if let Err((path, _resource)) = batch.try_store(&owner, path, resource) {
                            if batch.contains_owner(&owner) {
                                return Ok(Vec::new());
                            }
                            if batch.contains_path(&path) {
                                return Ok(vec![LifecycleEvent::WasmRejected { owner }]);
                            }
                            batch.reject_path(&path);
                            return Ok(vec![LifecycleEvent::WasmRejected { owner }]);
                        }
                        if self.prepared_wasm.try_store(&owner, ()).is_err() {
                            if let Some(artifact) = batch.remove_owner(&owner) {
                                batch.release_path_bytes(artifact.path.capacity());
                            }
                            return Ok(vec![LifecycleEvent::WasmRejected { owner }]);
                        }
                        Ok(vec![LifecycleEvent::WasmPrepared { owner }])
                    }
                    _ => {
                        if let Some(batch) = self.wasm_resources.get_for_scope_mut(&owner.scope) {
                            batch.reject_path(&path);
                        }
                        Ok(vec![LifecycleEvent::WasmRejected { owner }])
                    }
                }
            }
            LifecycleEffect::LoadWasm { .. } => Ok(Vec::new()),
            LifecycleEffect::ActivateCachedHistory { owner, entry }
                if model.owns_operation(&owner) =>
            {
                if self.parsed_pages.is_some() {
                    return Ok(vec![LifecycleEvent::ParseFailed {
                        owner,
                        message: "serialized parsed-page slot was already occupied".into(),
                    }]);
                }
                let activated_scope = match owner.scope.origin.try_clone() {
                    Ok(origin) => PageScope {
                        origin,
                        generation: owner.scope.generation,
                    },
                    Err(_) => {
                        return Ok(vec![LifecycleEvent::PresentationFailed {
                            message: "cached navigation owner exhausted client memory".into(),
                            retry: Some(PressureRetry::ActivateCachedHistory { owner, entry }),
                        }]);
                    }
                };
                let final_uri = match entry.uri.try_clone() {
                    Ok(uri) => uri,
                    Err(_) => {
                        return Ok(vec![LifecycleEvent::PresentationFailed {
                            message: "cached navigation URI exhausted client memory".into(),
                            retry: Some(PressureRetry::ActivateCachedHistory { owner, entry }),
                        }]);
                    }
                };
                let Some(governor) = self.client.as_ref().map(|client| client.governor.clone())
                else {
                    return Ok(vec![LifecycleEvent::ParseFailed {
                        owner,
                        message: "cannot activate remote history in a local viewer".into(),
                    }]);
                };
                let (document, parse_lease) = match parse_remote_aml(&entry.retained_aml, &governor)
                {
                    Ok(parsed) => parsed,
                    Err(RemoteParseError::Invalid) => {
                        return Ok(vec![LifecycleEvent::ParseFailed {
                            owner,
                            message: "cached page could not be parsed".into(),
                        }]);
                    }
                    Err(RemoteParseError::ResourceRejected) => {
                        return Ok(vec![LifecycleEvent::PresentationFailed {
                            message: "cached page parsing exceeded the client resource budget"
                                .into(),
                            retry: Some(PressureRetry::ActivateCachedHistory { owner, entry }),
                        }]);
                    }
                };
                let paths = match wasm_dependency_paths(&document) {
                    Ok(paths) => paths,
                    Err(_) => {
                        return Ok(vec![LifecycleEvent::PresentationFailed {
                            message: "WASM dependency discovery exhausted client memory".into(),
                            retry: Some(PressureRetry::ActivateCachedHistory { owner, entry }),
                        }]);
                    }
                };
                let path_bytes = match paths
                    .iter()
                    .try_fold(0usize, |total, path| total.checked_add(path.capacity()))
                {
                    Some(bytes) => bytes,
                    None => {
                        return Ok(vec![LifecycleEvent::PresentationFailed {
                            message: "cached WASM dependency metadata exceeded client limits"
                                .into(),
                            retry: Some(PressureRetry::ActivateCachedHistory { owner, entry }),
                        }]);
                    }
                };
                let path_lease = if path_bytes == 0 {
                    None
                } else {
                    match governor
                        .reserve(ResourceCategory::RemoteCollections, path_bytes)
                        .map_err(Some)
                        .and_then(|lease| {
                            if reject_runner_allocation(RunnerAllocationSite::WasmBatch) {
                                Err(None)
                            } else {
                                Ok(lease)
                            }
                        }) {
                        Ok(lease) => Some(lease),
                        Err(_) => {
                            return Ok(vec![LifecycleEvent::PresentationFailed {
                                message: "cached WASM dependency metadata exceeded the client resource budget"
                                    .into(),
                                retry: Some(PressureRetry::ActivateCachedHistory { owner, entry }),
                            }]);
                        }
                    }
                };
                let event_owner = match owner.try_clone() {
                    Ok(owner) => owner,
                    Err(_) => {
                        return Ok(vec![LifecycleEvent::PresentationFailed {
                            message: "cached WASM dependency owner exhausted client memory".into(),
                            retry: Some(PressureRetry::ActivateCachedHistory { owner, entry }),
                        }]);
                    }
                };
                if self
                    .wasm_resources
                    .try_store(
                        &owner,
                        PreparedWasmBatch::admitted(path_lease, paths.len(), path_bytes),
                    )
                    .is_err()
                {
                    return Ok(vec![LifecycleEvent::ParseFailed {
                        owner,
                        message: "serialized WASM batch slot was already occupied".into(),
                    }]);
                }
                if self
                    .parsed_pages
                    .try_store(
                        &owner,
                        ParsedPage {
                            document,
                            parse_lease,
                            final_uri,
                            aml_content: None,
                            cached_entry: Some(entry),
                        },
                    )
                    .is_err()
                {
                    let removed = self.wasm_resources.take(&owner);
                    assert!(
                        removed.is_some(),
                        "failed cached parse storage must release its WASM batch"
                    );
                    return Ok(vec![LifecycleEvent::ParseFailed {
                        owner,
                        message: "serialized parsed-page slot was already occupied".into(),
                    }]);
                }
                if let Some(client) = self.client.as_mut() {
                    client.activate_page_scope(activated_scope).await;
                }
                Ok(vec![
                    LifecycleEvent::WasmDependenciesDiscovered {
                        owner: event_owner,
                        paths,
                    },
                    LifecycleEvent::ParseCompleted { owner },
                ])
            }
            LifecycleEffect::ActivateCachedHistory { .. } => Ok(Vec::new()),
            LifecycleEffect::TickWasm { owner: Some(owner) } if model.owns_operation(&owner) => {
                let attempt_key = Some(PreparedWorkKey::from(&owner));
                let now = match self.pending_tick_attempt.as_ref() {
                    Some((pending_key, now)) if pending_key == &attempt_key => *now,
                    _ => {
                        let now = std::time::Instant::now();
                        self.pending_tick_attempt = Some((attempt_key, now));
                        now
                    }
                };
                let tick = self.page.anim_rt.tick(
                    &mut self.page.scene,
                    now,
                    self.state.scroll_offset,
                    self.state.viewport_height(),
                );
                if tick.allocation_failed {
                    return Ok(vec![LifecycleEvent::PresentationFailed {
                        message: "animation tick exceeded the client resource budget".into(),
                        retry: Some(PressureRetry::TickWasm { owner: Some(owner) }),
                    }]);
                }
                self.pending_tick_attempt = None;
                self.finish_animation_tick(tick);
                let stored = self.prepared_wasm.try_store(&owner, ());
                assert!(
                    stored.is_ok(),
                    "serialized WASM activation slot must be empty"
                );
                Ok(vec![LifecycleEvent::WasmPrepared { owner }])
            }
            LifecycleEffect::TickWasm { owner: None } if model.scope.is_none() => {
                let attempt_key = None;
                let now = match self.pending_tick_attempt.as_ref() {
                    Some((pending_key, now)) if pending_key == &attempt_key => *now,
                    _ => {
                        let now = std::time::Instant::now();
                        self.pending_tick_attempt = Some((attempt_key, now));
                        now
                    }
                };
                let tick = self.page.anim_rt.tick(
                    &mut self.page.scene,
                    now,
                    self.state.scroll_offset,
                    self.state.viewport_height(),
                );
                if tick.allocation_failed {
                    return Ok(vec![LifecycleEvent::PresentationFailed {
                        message: "animation tick exceeded the client resource budget".into(),
                        retry: Some(PressureRetry::TickWasm { owner: None }),
                    }]);
                }
                self.pending_tick_attempt = None;
                self.finish_animation_tick(tick);
                Ok(Vec::new())
            }
            LifecycleEffect::TickWasm { .. } => Ok(Vec::new()),
            LifecycleEffect::ApplyUpdate { owner, region } => {
                let Some(update) = self.pending_updates.get(&owner) else {
                    return Ok(Vec::new());
                };
                if update.projection != Some(region)
                    || self
                        .page
                        .placed
                        .get(region.placed_index())
                        .is_none_or(|placed| !placed.is_live() || placed.id != update.region)
                {
                    self.pending_updates.take(&owner);
                    return Ok(Vec::new());
                }
                let governor = self.page.governor.clone();
                if !apply_live_update(
                    update,
                    region,
                    &self.page.placed,
                    &mut self.page.scene,
                    self.color_support,
                    self.wcfg,
                    &mut self.region_buffers,
                    &governor,
                ) {
                    return Ok(vec![LifecycleEvent::PresentationFailed {
                        message: "live update exceeded the client resource budget".into(),
                        retry: Some(PressureRetry::Update { owner, region }),
                    }]);
                }
                self.pending_updates.take(&owner);
                self.needs_redraw = true;
                Ok(Vec::new())
            }
            LifecycleEffect::ProjectInput { token, value } => {
                if model.scope.as_ref() == Some(&token.scope) && self.input_mode.active {
                    sync_input_value(&mut self.page.scene, self.input_mode.current_node, value);
                    self.needs_redraw = true;
                }
                Ok(Vec::new())
            }
            LifecycleEffect::ProjectFocus { token, focused } => {
                if model.scope.as_ref() != Some(&token.scope) {
                    return Ok(Vec::new());
                }
                if project_scene_focus(&mut self.page.scene, focused) {
                    self.needs_redraw = true;
                }
                Ok(Vec::new())
            }
            LifecycleEffect::DeferNavigation { owner }
                if model.scope.as_ref() == Some(&owner.scope) =>
            {
                if self
                    .deferred_proposal
                    .as_ref()
                    .is_some_and(|proposal| proposal.generation == owner.scope.generation)
                    && let Some(DeferredProposal {
                        wait_for, action, ..
                    }) = self.deferred_proposal.take()
                {
                    if self.page.anim_rt.trigger_start(&wait_for) {
                        self.deferred_navigation = Some(DeferredNavigation {
                            scope: owner.scope,
                            request_id: owner.request_id,
                            wait_for,
                            action,
                            ready: false,
                            final_frame_presented: false,
                        });
                        self.needs_redraw = true;
                    } else {
                        return Ok(vec![LifecycleEvent::DeferredNavigationRejected { owner }]);
                    }
                }
                Ok(Vec::new())
            }
            LifecycleEffect::DeferNavigation { owner } => {
                if self
                    .deferred_proposal
                    .as_ref()
                    .is_some_and(|proposal| proposal.generation == owner.scope.generation)
                {
                    self.deferred_proposal = None;
                }
                Ok(Vec::new())
            }
            LifecycleEffect::ResumeNavigation { owner } => {
                if self.deferred_navigation.as_ref().is_some_and(|pending| {
                    pending.scope == owner.scope && pending.request_id == owner.request_id
                }) && let Some(pending) = self.deferred_navigation.take()
                {
                    let stored = self.resumed_navigation.try_store(&owner, pending.action);
                    assert!(
                        stored.is_ok(),
                        "serialized resumed-navigation slot must be empty"
                    );
                }
                Ok(Vec::new())
            }
            LifecycleEffect::RenderTerminal
            | LifecycleEffect::EvictResource { .. }
            | LifecycleEffect::EvictHistory { .. }
            | LifecycleEffect::ReleaseHistoryArtifact { .. }
            | LifecycleEffect::RetirePageWork { .. }
            | LifecycleEffect::ActivateErrorPage { .. } => {
                unreachable!("recovery effects are classified before execution")
            }
        }
    }

    async fn apply_presentation_action(
        &mut self,
        action: PresentationAction,
    ) -> Result<(), PresentationFailure> {
        let authored_event = match &action {
            PresentationAction::PageLoad => Some((ast::EventKind::PageLoad, None)),
            PresentationAction::AnimationEnd { source } => {
                Some((ast::EventKind::AnimationEnd, Some(source.as_str())))
            }
            PresentationAction::Focus { source } => {
                Some((ast::EventKind::Focus, source.as_deref()))
            }
            PresentationAction::Blur { source } => Some((ast::EventKind::Blur, source.as_deref())),
            PresentationAction::StateChange { source } => {
                Some((ast::EventKind::StateChange, Some(source.as_str())))
            }
            _ => None,
        };
        if let Some((kind, source)) = authored_event {
            let bindings = std::mem::take(&mut self.page.scene.event_bindings);
            let result = async {
                let prepared = self
                    .event_dispatcher
                    .prepare_fire(&bindings, kind, source, 0)
                    .map_err(|error| PresentationFailure::RetryOriginal(error.to_string()))?;
                self.event_dispatcher.commit(prepared);
                execute_on_actions(
                    &bindings,
                    &mut self.page,
                    &mut self.state,
                    self.color_support,
                    self.wcfg,
                    self.client.as_mut(),
                    &mut self.event_dispatcher,
                    &mut self.needs_redraw,
                )
                .await
                .map_err(PresentationFailure::ResumeAuthored)
            }
            .await;
            self.page.scene.event_bindings = bindings;
            return result;
        }

        match action {
            PresentationAction::SetPanel { panel_id, state } => {
                let old_panel = capture_panel_transition_source(&self.page, &panel_id);
                let old_active = panel_active_node(&self.page.scene, &panel_id);
                if let Some(old_active) = old_active {
                    if !self.page.scene.begin_relayout_transaction() {
                        return Err(
                            "panel relayout transaction exceeded the client resource budget".into(),
                        );
                    }
                    if !apply_panel_patch(&mut self.page.scene, &panel_id, &state) {
                        self.page.scene.rollback_relayout_transaction();
                        return Ok(());
                    }
                    let committed = relayout_panels_for(
                        &mut self.page,
                        &mut self.state,
                        self.color_support,
                        self.wcfg,
                        Some(&panel_id),
                        old_panel,
                        self.client.as_mut(),
                    )
                    .await;
                    if committed {
                        Box::pin(
                            self.apply_presentation_action(PresentationAction::StateChange {
                                source: panel_id,
                            }),
                        )
                        .await?;
                        self.needs_redraw = true;
                    } else {
                        let panel = self
                            .page
                            .scene
                            .find_by_aml_id(&panel_id)
                            .unwrap_or_default();
                        PatchApplier::apply(
                            &mut self.page.scene,
                            Patch::SetPanelActive {
                                panel,
                                active: old_active,
                            },
                        );
                        self.page.scene.rollback_relayout_transaction();
                        return Err("panel relayout exceeded the client resource budget".into());
                    }
                }
            }
            PresentationAction::TogglePanel { panel_id, states } => {
                let old_panel = capture_panel_transition_source(&self.page, &panel_id);
                let old_active = panel_active_node(&self.page.scene, &panel_id);
                if let Some(old_active) = old_active {
                    if !self.page.scene.begin_relayout_transaction() {
                        return Err(
                            "panel relayout transaction exceeded the client resource budget".into(),
                        );
                    }
                    if !toggle_panel_state(&mut self.page.scene, &panel_id, &states) {
                        self.page.scene.rollback_relayout_transaction();
                        return Ok(());
                    }
                    let committed = relayout_panels_for(
                        &mut self.page,
                        &mut self.state,
                        self.color_support,
                        self.wcfg,
                        Some(&panel_id),
                        old_panel,
                        self.client.as_mut(),
                    )
                    .await;
                    if committed {
                        Box::pin(
                            self.apply_presentation_action(PresentationAction::StateChange {
                                source: panel_id,
                            }),
                        )
                        .await?;
                        self.needs_redraw = true;
                    } else {
                        let panel = self
                            .page
                            .scene
                            .find_by_aml_id(&panel_id)
                            .unwrap_or_default();
                        PatchApplier::apply(
                            &mut self.page.scene,
                            Patch::SetPanelActive {
                                panel,
                                active: old_active,
                            },
                        );
                        self.page.scene.rollback_relayout_transaction();
                        return Err("panel relayout exceeded the client resource budget".into());
                    }
                }
            }
            PresentationAction::ToggleDetails { index } => {
                if let Some(node) = self.page.scene.find_details_by_index(index) {
                    if !self.page.scene.begin_relayout_transaction() {
                        return Err(
                            "details relayout transaction exceeded the client resource budget"
                                .into(),
                        );
                    }
                    PatchApplier::apply(&mut self.page.scene, Patch::ToggleDetails { node });
                    let committed = relayout_panels_for(
                        &mut self.page,
                        &mut self.state,
                        self.color_support,
                        self.wcfg,
                        None,
                        None,
                        self.client.as_mut(),
                    )
                    .await;
                    if committed {
                        self.needs_redraw = true;
                    } else {
                        PatchApplier::apply(&mut self.page.scene, Patch::ToggleDetails { node });
                        self.page.scene.rollback_relayout_transaction();
                        return Err("details relayout exceeded the client resource budget".into());
                    }
                }
            }
            PresentationAction::AdvanceFocus { forward } => {
                advance_focus(&mut self.page, &mut self.state, forward);
                self.needs_redraw = true;
            }
            PresentationAction::AdvanceSelect { node } => {
                advance_select(&mut self.page.scene, node);
                self.needs_redraw = true;
            }
            PresentationAction::SetScroll { offset } => {
                self.state.scroll_offset = offset.min(self.state.max_scroll());
                self.needs_redraw = true;
            }
            PresentationAction::ScrollToRow { row } => {
                self.state.scroll_to_row(row);
                self.needs_redraw = true;
            }
            PresentationAction::ClearFocus => {
                PatchApplier::apply(&mut self.page.scene, Patch::SetFocus { node: None });
                self.needs_redraw = true;
            }
            PresentationAction::SkipAnimations => {
                let skipped = self.page.anim_rt.skip_all(&mut self.page.scene);
                if skipped.allocation_failed {
                    return Err("animation skip exceeded the client resource budget".into());
                }
                self.finish_animation_tick(TickResult::from_skip(skipped));
            }
            PresentationAction::DrainLayout => {
                if !drain_invalidated_layout_transactionally(
                    &mut self.page,
                    self.color_support,
                    self.wcfg,
                ) {
                    return Err(
                        "invalidation layout drain exceeded the client resource budget".into(),
                    );
                }
            }
            PresentationAction::CapturePageTransition => {
                let governor = self
                    .client
                    .as_ref()
                    .map(|client| client.governor.clone())
                    .unwrap_or_else(|| self.page.governor.clone());
                let Some(captured) = capture_viewport_snapshot(
                    &self.page,
                    &mut self.compositor,
                    &self.state,
                    &governor,
                ) else {
                    return Err(
                        "page-transition capture exceeded the client resource budget".into(),
                    );
                };
                self.pending_page_transition = Some(captured);
            }
            PresentationAction::StartPageTransition { kind, duration_ms } => {
                if kind != ast::TransitionKind::Cut && duration_ms > 0 {
                    if self.pending_page_transition.is_some()
                        && !try_install_page_transition(
                            &mut self.page,
                            &mut self.compositor,
                            &self.state,
                            &mut self.pending_page_transition,
                            kind,
                            duration_ms,
                        )
                    {
                        return Err(
                            "page-transition install exceeded the client resource budget".into(),
                        );
                    }
                    self.needs_redraw |= self.page.anim_rt.has_page_transition();
                } else {
                    self.pending_page_transition = None;
                }
            }
            PresentationAction::InvalidateFull => {
                self.page.scene.invalidation.mark_composite(Rect::new(
                    0,
                    0,
                    self.state.term_w,
                    self.state.term_h,
                ));
                self.needs_redraw = true;
            }
            PresentationAction::ActivateLocalPage { aml, uri, overlay } => {
                self.local_page_activated = false;
                #[cfg(test)]
                {
                    self.last_local_page_aml_ptr = Some(aml.as_ptr() as usize);
                }
                if let Some(new_page) = load_cached(
                    &aml,
                    self.state.term_w,
                    self.state.term_h,
                    self.color_support,
                    self.wcfg,
                    self.client.as_mut(),
                    uri,
                )
                .await
                {
                    let mut new_page = new_page;
                    self.event_dispatcher = new_page
                        .prepared_event_dispatcher
                        .take()
                        .unwrap_or_else(EventDispatcher::unadmitted);
                    self.region_buffers.clear();
                    self.page = new_page;
                    reset_input_mode(&mut self.input_mode);
                    self.state = ViewportState::with_sticky(
                        self.state.term_w,
                        self.state.term_h,
                        self.page.buf.height,
                        &self.page.sticky_buf,
                    );
                    self.showing_overlay = overlay;
                    invalidate_compositor_for_new_scene(&mut self.compositor);
                    self.needs_redraw = true;
                    self.local_page_activated = true;
                }
            }
            PresentationAction::FlushPendingActions => {
                self.event_dispatcher.flush_pending_now();
                Box::pin(self.apply_presentation_action(PresentationAction::TickPendingActions))
                    .await?;
            }
            PresentationAction::TickPendingActions => {
                let bindings = std::mem::take(&mut self.page.scene.event_bindings);
                let result = execute_on_actions(
                    &bindings,
                    &mut self.page,
                    &mut self.state,
                    self.color_support,
                    self.wcfg,
                    self.client.as_mut(),
                    &mut self.event_dispatcher,
                    &mut self.needs_redraw,
                )
                .await
                .map_err(PresentationFailure::ResumeAuthored);
                self.page.scene.event_bindings = bindings;
                result?;
            }
            PresentationAction::PageLoad
            | PresentationAction::AnimationEnd { .. }
            | PresentationAction::Focus { .. }
            | PresentationAction::Blur { .. }
            | PresentationAction::StateChange { .. } => unreachable!("handled above"),
        }
        Ok(())
    }

    fn finish_animation_tick(&mut self, mut tick: TickResult) {
        if tick.changed {
            self.page.anim_rt.paint_into_scene(&mut self.page.scene);
            self.needs_redraw = true;
            for node_id in &tick.wrote_buffers {
                if let Some(node) = self.page.scene.get(*node_id) {
                    let rect = node.placement().rect;
                    if !rect.is_empty() {
                        self.page.scene.invalidation.mark_composite(rect);
                    }
                }
            }
        }
        for patch in tick.patches.drain(..) {
            PatchApplier::apply(&mut self.page.scene, patch);
        }
        if tick
            .newly_finished
            .iter()
            .any(|id| id == PAGE_TRANSITION_ID)
        {
            self.page
                .anim_rt
                .finish_page_transition(&mut self.page.scene);
        }
        self.pending_tick = Some(tick);
    }
}

async fn dispatch_navigation_event(
    runtime: &mut TerminalRuntime,
    lifecycle: &mut ReducerPort,
    event: LifecycleEvent,
) -> Result<Option<ActivatedNavigation>, ViewerError> {
    super::dispatch_runtime_events(runtime, lifecycle, [event]).await?;
    Ok(runtime.take_activated_navigation(lifecycle.scope.as_ref()))
}

async fn dispatch_presentation_action(
    runtime: &mut TerminalRuntime,
    lifecycle: &mut ReducerPort,
    action: PresentationAction,
) -> Result<(), ViewerError> {
    let Some(scope) = lifecycle.try_scope_clone() else {
        return Ok(());
    };
    super::dispatch_runtime_events(
        runtime,
        lifecycle,
        [LifecycleEvent::PresentationActionRequested { scope, action }],
    )
    .await
}

// ─── Unified viewer loop ─────────────────────────────────────
/// Whether the controlling terminal is still there to draw into.
///
/// A terminal that has gone away does not report an error — the descriptor
/// stays open and `TIOCGWINSZ` answers `Ok((0, 0))`. Treating only `Err` as
/// loss misses the case entirely, which is how an orphaned viewer ends up
/// spinning on a dead descriptor indefinitely.
///
/// But `Ok((0, 0))` is not by itself loss, and reading it as such was a
/// defect of its own: a pty that nobody has sized answers exactly the same
/// way, which is the normal state under a test harness with no controlling
/// terminal. A viewer that quits there quits before it can be driven at all.
/// The two cases are distinguishable only by history, so this carries some.
#[derive(Default)]
struct TerminalPresence {
    /// Whether a non-zero size has ever been observed.
    was_sized: bool,
}

impl TerminalPresence {
    /// Records one size observation and reports whether to keep running.
    fn observe(&mut self, size: std::io::Result<(u16, u16)>) -> bool {
        match size {
            // The ioctl itself failed: the descriptor is gone, sized or not.
            Err(_) => false,
            Ok((width, height)) if width > 0 && height > 0 => {
                self.was_sized = true;
                true
            }
            // Zero after a real size is loss. Zero before one is a terminal
            // nobody has sized yet, which is not the same thing.
            Ok(_) => !self.was_sized,
        }
    }
}

/// How long the main loop gets to unwind after a termination signal before
/// the watchdog ends the process itself.
///
/// Comfortably longer than the loop's own worst-case poll timeout (250ms), so
/// a viewer with a live terminal always restores it the ordinary way and the
/// watchdog never fires.
#[cfg(unix)]
const TERMINATION_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

/// How often the viewer proves it is still there.
///
/// The server resets its 30s idle deadline on any inbound frame, so a third of
/// that deadline means two pings must be lost before a connection is dropped.
/// This is deliberately *not* tied to whether the page has live regions: an
/// idle connection is exactly the one at risk, and the protocol-drain stage
/// below is skipped when there are no subscriptions.
const KEEPALIVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);

/// Ends the process if a termination signal goes unanswered.
///
/// The main loop checks `requested` at the top of every iteration, which is
/// enough whenever it keeps iterating. It does not when the controlling
/// terminal goes away: the dead tty is permanently readable-at-EOF, so
/// crossterm's `read` spins inside itself and never returns. The signal is
/// delivered — SIGHUP from the kernel when the pty master closes, or a
/// SIGTERM someone sends in frustration — and nothing is left running that
/// could act on it. Orphaned viewers were observed burning a core for three
/// days in exactly that state.
///
/// Skipping terminal restoration is the cost, and it is not a real one here:
/// a viewer that outlives its terminal has nowhere to write the restore
/// sequence. A viewer whose terminal is alive exits through the loop long
/// before this fires.
#[cfg(unix)]
fn spawn_termination_watchdog(requested: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    std::thread::spawn(move || {
        while !requested.load(std::sync::atomic::Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        std::thread::sleep(TERMINATION_GRACE);
        // Still here: the loop never came back to read the flag.
        std::process::exit(0);
    });
}

async fn viewer_main_loop(
    mut runtime: TerminalRuntime,
    mut lifecycle: ReducerPort,
    color_support: ColorSupport,
    _wcfg: WidthConfig,
    config: &ClientConfig,
) -> Result<(), ViewerError> {
    #[cfg(unix)]
    let termination_requested = {
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;
        let requested = Arc::new(AtomicBool::new(false));
        for signal in [
            signal_hook::consts::SIGINT,
            signal_hook::consts::SIGTERM,
            signal_hook::consts::SIGHUP,
        ] {
            signal_hook::flag::register(signal, requested.clone())?;
        }
        spawn_termination_watchdog(Arc::clone(&requested));
        requested
    };
    let mut stdout = io::stdout();
    // Subscribe to live regions if present.
    resubscribe_live(&mut runtime, &mut lifecycle).await?;

    // Fire page-load events.
    dispatch_presentation_action(&mut runtime, &mut lifecycle, PresentationAction::PageLoad)
        .await?;

    let mut presence = TerminalPresence::default();
    let mut last_ping = std::time::Instant::now();
    loop {
        #[cfg(unix)]
        if termination_requested.load(std::sync::atomic::Ordering::Relaxed) {
            return Ok(());
        }
        // A terminal that reports itself gone lets the viewer exit the clean
        // way: unwind, restore, return. That is worth taking when it is
        // available, but it is not the mechanism the orphan case rests on.
        //
        // Measured on macOS: after the pty master is killed, `size()` still
        // answers `Ok((80, 24))` and `is_raw_mode_enabled()` still answers
        // `Ok(true)`. The slave descriptor stays fully functional from this
        // process's side, so no ioctl distinguishes a dead terminal from a
        // live one. What catches the orphan is `spawn_termination_watchdog`,
        // which acts on the SIGHUP the kernel sends without needing this loop
        // to run at all — necessary because the spin lives *inside*
        // crossterm's read and control never comes back here.
        if !presence.observe(terminal::size()) {
            return Ok(());
        }
        // A certificate was just pinned in answer to the trust prompt. The
        // navigation that asked the question was failed rather than resumed,
        // because pinning moves the site into a different security context and
        // the old navigation's owner carries the origin from before it. Re-issue
        // it here, where the origin is derived afresh and now resolves to a
        // pinned one.
        if let Some(uri) = runtime.retry_after_trust.take() {
            let origin = runtime
                .client
                .as_ref()
                .map(|client| client.request_origin(&uri));
            match (origin, uri.try_clone()) {
                (Some(Ok(origin)), Ok(uri)) => {
                    dispatch_navigation_event(
                        &mut runtime,
                        &mut lifecycle,
                        LifecycleEvent::Navigate { uri, origin },
                    )
                    .await?;
                }
                _ => {
                    runtime
                        .command_line
                        .set_message("error: cannot reconnect after pinning", true);
                    runtime.needs_redraw = true;
                }
            }
            continue;
        }
        // Logical history position is reducer-owned. This mutable value is
        // only the current rendering projection; navigation completions below
        // refresh it from the reducer before it is observed again.
        let mut history_idx = lifecycle
            .history_position
            .unwrap_or_else(|| lifecycle.history.len().saturating_sub(1));
        debug_assert_eq!(
            lifecycle
                .history
                .get(history_idx)
                .and_then(|logical| runtime
                    .history
                    .iter()
                    .find(|artifact| artifact.id == logical.id))
                .map(|entry| entry.id),
            lifecycle
                .history_position
                .and_then(|position| lifecycle.history.get(position))
                .map(|entry| entry.id),
            "terminal history artifacts must project reducer-owned history"
        );
        if runtime.client_hud.tick() {
            // The HUD is painted directly to stdout. Repainting the underlying
            // site on each animation step reveals clean rows as it retracts.
            runtime.compositor.invalidate_presented();
            runtime.needs_redraw = true;
        }

        let status_uri = lifecycle
            .current_uri
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_default();

        // Keepalive. The loop is guaranteed to come round at least every 250ms,
        // so elapsed-time bookkeeping is enough and no separate timer task is
        // needed. A send failure is not handled here: the connection is already
        // gone, and the next navigation redials through the existing reconnect
        // path rather than this one growing its own error handling.
        if last_ping.elapsed() >= KEEPALIVE_INTERVAL {
            last_ping = std::time::Instant::now();
            if let Some(client) = runtime.client.as_mut() {
                let _ = client.send_ping().await;
            }
        }

        let has_live = runtime
            .client
            .as_ref()
            .is_some_and(|client| client.active_subscription_count() != 0);
        let poll_timeout = if runtime.client_hud.is_animating() {
            std::time::Duration::from_millis(16)
        } else if runtime.page.anim_rt.has_animations()
            || runtime.page.anim_rt.has_transitions()
            || runtime.event_dispatcher.has_pending()
        {
            std::time::Duration::from_millis(crate::compositor::animate::wasm::TICK_MS)
        } else if has_live {
            std::time::Duration::from_millis(100)
        } else {
            std::time::Duration::from_millis(250)
        };

        if runtime.needs_redraw && !runtime.render_authorized {
            runtime.needs_redraw = false;
            super::dispatch_runtime_events(
                &mut runtime,
                &mut lifecycle,
                [LifecycleEvent::PresentationRequested],
            )
            .await?;
        }

        if runtime.render_authorized {
            // ─── Stage 7: present pass ───────────────────────────
            // Emit ANSI. Composite walks the scene (which now owns
            // any in-flight page-transition overlay); `draw_viewer_frame`
            // diffs against the last-presented frame and emits.
            // Read fresh every frame: navigating to a differently trusted site
            // must change what the status bar claims.
            let security = runtime
                .client
                .as_ref()
                .and_then(|client| client.current_security());
            let presented = present_pass(
                &mut stdout,
                &mut runtime.compositor,
                &mut runtime.page,
                &runtime.state,
                &status_uri,
                security,
                lifecycle.connection == ConnectionStatus::Connected,
                config,
                color_support,
                &runtime.input_mode,
                &runtime.command_line,
                runtime.help_visible,
                &runtime.client_hud,
                &runtime.error_log,
                &runtime.history,
                &lifecycle.history,
                history_idx,
            )?;
            if !presented && !runtime.page.client_owned_error {
                let Some(retired_scope) = lifecycle.try_scope_clone() else {
                    continue;
                };
                if let Some(scope) = retired_scope {
                    super::dispatch_runtime_events(
                        &mut runtime,
                        &mut lifecycle,
                        [LifecycleEvent::SubscriptionsRetired { scope }],
                    )
                    .await?;
                }
                runtime.compositor.invalidate_cache();
                runtime.compositor.invalidate_presented();
                super::dispatch_runtime_events(
                    &mut runtime,
                    &mut lifecycle,
                    [LifecycleEvent::PresentationFailed {
                        message: "compositor frame exceeded the client resource budget".into(),
                        retry: None,
                    }],
                )
                .await?;
                continue;
            }
            runtime.render_authorized = false;
            runtime.needs_redraw = false;
            // Both composite and present have consumed their dirty
            // sets for this tick; clear them so the next tick's
            // short-circuit check sees a clean slate. Layout
            // invalidation is drained separately in stage 5.
            runtime.page.scene.invalidation.composite.clear();
            runtime.page.scene.invalidation.present.clear();
            if let Some(pending) = runtime.deferred_navigation.as_mut() {
                pending.mark_final_frame_presented();
            }
        }

        // ─── Stage 1: drain input events ──────────────────────
        if runtime.deferred_navigation.as_ref().is_some_and(|pending| {
            !runtime
                .page
                .anim_rt
                .animations
                .iter()
                .any(|animation| animation.id() == pending.wait_for)
        }) {
            runtime.deferred_navigation = None;
        }
        // Read one terminal event (key/resize/mouse) up to
        // `poll_timeout`. Handlers emit `ViewerAction` values; some
        // (focus Tab, scroll arrows) end up as `Patch::SetFocus` /
        // `Patch::SetScroll` on the scene via `sync_scene_focus`.
        let old_focus_index = current_focus_index(&runtime.page.scene, &runtime.page.focusables);
        let mut command_action: Option<ParsedCommand> = None;
        let mut deferred_activation = None;
        let deferred_can_resume = runtime
            .deferred_navigation
            .as_ref()
            .is_some_and(|pending| pending.can_resume(lifecycle.scope.as_ref()));
        let mut action = if deferred_can_resume {
            // Do not block waiting for terminal input once the completed exit
            // frame has been presented; resume navigation in this iteration.
            ViewerAction::None
        } else if event::poll(poll_timeout)? {
            match event::read()? {
                Event::Key(KeyEvent {
                    code: KeyCode::Char('c'),
                    modifiers: KeyModifiers::CONTROL,
                    ..
                }) => ViewerAction::Quit,
                Event::Key(key) if runtime.help_visible => {
                    if matches!(key.code, KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q')) {
                        runtime.help_visible = false;
                        // The modal is painted directly to stdout, outside the
                        // composited page buffer. Force the site to repaint it.
                        runtime.compositor.invalidate_presented();
                        ViewerAction::Redraw
                    } else {
                        ViewerAction::None
                    }
                }
                Event::Key(key) if runtime.client_hud.is_active() => {
                    match runtime.client_hud.handle_key(
                        key.code,
                        lifecycle.history.len(),
                        runtime.error_log.entries.len(),
                    ) {
                        ClientHudAction::None => ViewerAction::None,
                        ClientHudAction::Redraw => {
                            runtime.compositor.invalidate_presented();
                            ViewerAction::Redraw
                        }
                        ClientHudAction::OpenHistory(target) => ViewerAction::JumpHistory(target),
                        ClientHudAction::ClearErrors => {
                            runtime.error_log.clear();
                            runtime.compositor.invalidate_presented();
                            ViewerAction::Redraw
                        }
                    }
                }
                Event::Key(key) if runtime.command_line.mode == CommandLineMode::Input => {
                    let action = match runtime.command_line.handle_key(key.code) {
                        Some(cmd) => {
                            command_action = Some(cmd);
                            ViewerAction::Redraw
                        }
                        None => ViewerAction::Redraw,
                    };
                    if let Some(token) = lifecycle.control_token() {
                        let value = runtime.command_line.buffer.clone();
                        super::dispatch_runtime_events(
                            &mut runtime,
                            &mut lifecycle,
                            [LifecycleEvent::Input { token, value }],
                        )
                        .await?;
                    }
                    action
                }
                Event::Key(key) if runtime.input_mode.active => {
                    let (action, value) =
                        handle_input_key(key, &mut runtime.input_mode, &mut runtime.needs_redraw);
                    if let Some(token) = lifecycle.control_token()
                        && let Some(value) = value
                    {
                        super::dispatch_runtime_events(
                            &mut runtime,
                            &mut lifecycle,
                            [LifecycleEvent::Input { token, value }],
                        )
                        .await?;
                    }
                    action
                }
                Event::Key(key) => {
                    if runtime.command_line.clear_message_if_needed() {
                        runtime.needs_redraw = true;
                    }
                    // `f` (fast-forward): skip the entire page-load
                    // cascade to its settled final state. We engage
                    // whenever either a foreground animation is
                    // running OR the event dispatcher has pending
                    // delayed actions — the latter covers the gap
                    // between page-load and the first animation
                    // actually starting (e.g. `page-load → set panel
                    // visible @ 800ms → state-change → animate`).
                    // Background animations (matrix rain etc.) don't
                    // count — they're perpetual by intent.
                    let skipping = matches!(key.code, KeyCode::Char('f'))
                        && (runtime.page.anim_rt.has_foreground_animations()
                            || runtime.event_dispatcher.has_pending());
                    if skipping {
                        // Fixed-point loop: skipping an animation
                        // may fire `animation-end` which schedules
                        // a delayed `set:panel visible` action;
                        // flushing pending actions runs that set,
                        // which fires `state-change` and schedules
                        // the next cascade step; which may launch
                        // another animation we also want to skip.
                        // Bound the loop to avoid runaway cascades
                        // (malformed AML with cyclic event wiring).
                        let mut guard = 0;
                        loop {
                            guard += 1;
                            if guard > 16 {
                                break;
                            }

                            dispatch_presentation_action(
                                &mut runtime,
                                &mut lifecycle,
                                PresentationAction::SkipAnimations,
                            )
                            .await?;
                            let skip = runtime.pending_tick.take().unwrap_or_default();

                            // Fire animation-end for each skipped
                            // animation so event-driven reveals
                            // (set:dir-box visible on tagline end)
                            // engage instead of stranding panels.
                            for id in skip.newly_finished {
                                if let Some(pending) = runtime.deferred_navigation.as_mut() {
                                    pending.mark_animation_finished(&id);
                                }
                                dispatch_presentation_action(
                                    &mut runtime,
                                    &mut lifecycle,
                                    PresentationAction::AnimationEnd { source: id },
                                )
                                .await?;
                            }

                            // Collapse all authored delays so the
                            // cascade settles in one iteration.
                            dispatch_presentation_action(
                                &mut runtime,
                                &mut lifecycle,
                                PresentationAction::FlushPendingActions,
                            )
                            .await?;

                            // Stable state reached?
                            if !runtime.page.anim_rt.has_foreground_animations()
                                && !runtime.event_dispatcher.has_pending()
                            {
                                break;
                            }
                        }
                        // Force a full composite walk on the next
                        // present: the cascade of relayouts above
                        // mutated panel active states, and the
                        // per-patch `mark_composite` calls only
                        // stamped the *old* rects — which are empty
                        // when a hidden panel flips to visible, so
                        // `DirtyRegions::add` drops them. Without
                        // this stamp the compositor cache would
                        // return a stale pre-cascade frame.
                        dispatch_presentation_action(
                            &mut runtime,
                            &mut lifecycle,
                            PresentationAction::InvalidateFull,
                        )
                        .await?;
                        runtime.needs_redraw = true;
                        ViewerAction::None
                    } else {
                        let mut projected_state = runtime.state.clone();
                        let action = projected_state
                            .transition(ViewportEvent::Input(key))
                            .into_iter()
                            .find_map(|effect| match effect {
                                ViewportEffect::Action(action) => Some(action),
                                _ => None,
                            })
                            .unwrap_or(ViewerAction::None);
                        if projected_state.scroll_offset != runtime.state.scroll_offset {
                            dispatch_presentation_action(
                                &mut runtime,
                                &mut lifecycle,
                                PresentationAction::SetScroll {
                                    offset: projected_state.scroll_offset,
                                },
                            )
                            .await?;
                        }
                        action
                    }
                }
                Event::Resize(new_w, new_h) => {
                    if new_w == runtime.state.term_w && new_h == runtime.state.term_h {
                        ViewerAction::None
                    } else {
                        super::dispatch_runtime_events(
                            &mut runtime,
                            &mut lifecycle,
                            [LifecycleEvent::Resize {
                                width: new_w,
                                height: new_h,
                            }],
                        )
                        .await?;
                        ViewerAction::Redraw
                    }
                }
                _ => ViewerAction::None,
            }
        } else {
            ViewerAction::None
        };

        // ─── Stage 2: advance time ────────────────────────────
        // `anim_rt.tick` returns a `TickResult` with patches to
        // apply (tweens) and buffer-write flags (frame/wasm). The
        // runtime has already `mem::swap`ed WASM output into the
        // scene's buffer during tick; `paint_into_scene` below
        // handles non-WASM paints.
        if runtime.page.anim_rt.has_animations() {
            super::dispatch_runtime_events(&mut runtime, &mut lifecycle, [LifecycleEvent::Timer])
                .await?;
        }
        let tick_result = runtime.pending_tick.take().unwrap_or_default();
        // The page-transition overlay owns the terminal while it is active.
        // Its final snapshot can differ from the now-live page underneath
        // because page-load animations continued advancing during the
        // transition. Do not rely on a scoped diff when the overlay is
        // removed: force the first unobscured frame through the same atomic
        // clear-and-full-render path used by reload. This prevents occasional
        // old-page cells surviving inside transparent entrance animations.
        let page_transition_finished = tick_result
            .newly_finished
            .iter()
            .any(|id| id == crate::compositor::animate::PAGE_TRANSITION_ID);
        // Retain adapter failures (e.g. an effect stopped by the WASM memory
        // limit) in client chrome, where site rendering cannot overwrite them.
        // The first failure of the session opens the Errors tab; later failures
        // update its count without repeatedly stealing keyboard focus.
        for notice in &tick_result.notices {
            if record_runtime_notice(&mut runtime.error_log, &mut runtime.client_hud, notice) {
                runtime.help_visible = false;
                runtime.compositor.invalidate_presented();
            }
            runtime.needs_redraw = true;
        }
        // Animation paints and patches were applied inside the reducer-issued
        // tick effect. Only completion metadata crosses back into the loop.
        if page_transition_finished {
            // Match the reload path exactly: erase and flush the physical
            // terminal before exposing a destination scene that can contain
            // transparent, fixed-size entrance placeholders. Merely dropping
            // the presenter's cached frame leaves the clear buffered until
            // the next presentation and has allowed cells from the departing
            // page to survive inside those reserved regions on some terminals.
            clear_terminal_for_full_redraw(&mut stdout, &mut runtime.compositor)?;
            runtime.needs_redraw = true;
        }

        // Fire animation-end events for newly finished animations.
        {
            for id in tick_result.newly_finished {
                if let Some(pending) = runtime.deferred_navigation.as_mut() {
                    pending.mark_animation_finished(&id);
                }
                dispatch_presentation_action(
                    &mut runtime,
                    &mut lifecycle,
                    PresentationAction::AnimationEnd { source: id },
                )
                .await?;
            }
        }

        // Tick delayed event actions
        dispatch_presentation_action(
            &mut runtime,
            &mut lifecycle,
            PresentationAction::TickPendingActions,
        )
        .await?;

        // Page transitions tick through `anim_rt.tick` above — the
        // `PageTransitionAdapter` is an ordinary `Animation`, so no
        // separate lifecycle block is needed.

        // ─── Stage 3: drain protocol events ───────────────────
        // Re-subscribe if panel state changes revealed new [live]
        // elements, then poll the client for any region updates and
        // apply them directly to the owning live-region scene buffer.
        if runtime
            .client
            .as_ref()
            .is_some_and(|client| !client_subscriptions_match_page(client, &runtime.page))
        {
            resubscribe_live(&mut runtime, &mut lifecycle).await?;
        }
        if let Some(ref mut c) = runtime.client {
            // Poll for live updates (short timeout when animations are running to avoid lag)
            if c.active_subscription_count() != 0
                && !runtime.page.anim_rt.has_transitions()
                && !runtime.page.anim_rt.has_page_transition()
            {
                let update_timeout = if runtime.page.anim_rt.has_animations() {
                    std::time::Duration::from_millis(1)
                } else {
                    std::time::Duration::from_millis(200)
                };
                match c.poll_update(update_timeout).await {
                    Ok(Some(update)) => {
                        let Ok(scope) = update.scope.try_clone() else {
                            continue;
                        };
                        let owner = OperationOwner::new(scope, update.request_id);
                        let owner_key = PreparedWorkKey::from(&owner);
                        let Some(region) = update.projection else {
                            continue;
                        };
                        if runtime.pending_updates.try_store(&owner, update).is_err() {
                            continue;
                        }
                        super::dispatch_runtime_events(
                            &mut runtime,
                            &mut lifecycle,
                            [LifecycleEvent::LiveUpdate { owner, region }],
                        )
                        .await?;
                        // A stale or unsolicited completion produces no
                        // ApplyUpdate effect, so discard its buffered payload.
                        runtime.pending_updates.take_key(owner_key);
                    }
                    Ok(None) => {}
                    Err(_) => {
                        runtime.needs_redraw = true;
                        if let Some(origin) = c
                            .current_scope()
                            .and_then(|scope| scope.origin.try_clone().ok())
                        {
                            runtime.region_buffers.clear();
                            super::dispatch_runtime_events(
                                &mut runtime,
                                &mut lifecycle,
                                [LifecycleEvent::TransportLost { origin }],
                            )
                            .await?;
                        }
                    }
                }
            }
        }

        // A deferred link resumes as the exact navigation action captured at
        // activation time; focus may have moved while its exit animation ran.
        if runtime
            .deferred_navigation
            .as_ref()
            .is_some_and(|pending| pending.can_resume(lifecycle.scope.as_ref()))
            && matches!(action, ViewerAction::None | ViewerAction::Redraw)
        {
            let owner = runtime.deferred_navigation.as_ref().and_then(|pending| {
                pending
                    .scope
                    .try_clone()
                    .ok()
                    .map(|scope| OperationOwner::new(scope, pending.request_id))
            });
            if let Some(owner) = owner {
                super::dispatch_runtime_events(
                    &mut runtime,
                    &mut lifecycle,
                    [LifecycleEvent::DeferredNavigationCompleted { owner }],
                )
                .await?;
            }
            if let Some(resumed) = lifecycle
                .scope
                .as_ref()
                .and_then(|scope| runtime.resumed_navigation.take_for_scope(scope))
            {
                deferred_activation = Some(resumed);
                action = ViewerAction::Activate;
            }
        }

        match action {
            ViewerAction::Quit => break,
            ViewerAction::FocusChanged => {
                // Fire blur on old focus
                if let Some(old_idx) = old_focus_index
                    && let Some(f) = runtime.page.focusables.get(old_idx)
                {
                    let action = authored_focus_action(f, false);
                    dispatch_presentation_action(&mut runtime, &mut lifecycle, action).await?;
                }
                // Fire focus on new focus
                if let Some(idx) =
                    current_focus_index(&runtime.page.scene, &runtime.page.focusables)
                    && let Some(f) = runtime.page.focusables.get(idx)
                {
                    let row = f.row;
                    let is_sticky = f.is_sticky;
                    let action = authored_focus_action(f, true);
                    if !is_sticky {
                        dispatch_presentation_action(
                            &mut runtime,
                            &mut lifecycle,
                            PresentationAction::ScrollToRow { row },
                        )
                        .await?;
                    }
                    dispatch_presentation_action(&mut runtime, &mut lifecycle, action).await?;
                }
                runtime.needs_redraw = true;
            }
            ViewerAction::Redraw => {
                runtime.needs_redraw = true;
            }
            ViewerAction::GoBack => {
                // Dismiss a local overlay (e.g. :sessions) or a client-owned
                // navigation error by restoring the still-current history
                // entry. Failed destinations are never committed to history,
                // so an ordinary history step cannot recover this page.
                if runtime.showing_overlay || runtime.page.client_owned_error {
                    let cached = logical_history_entry(&lifecycle, history_idx)
                        .and_then(|entry| entry.try_clone().ok())
                        .map(|entry| (entry.retained_aml, entry.uri));
                    if let Some((retained_aml, uri)) = cached {
                        dispatch_presentation_action(
                            &mut runtime,
                            &mut lifecycle,
                            PresentationAction::ActivateLocalPage {
                                aml: retained_aml,
                                uri: Some(uri),
                                overlay: false,
                            },
                        )
                        .await?;
                        if runtime.local_page_activated {
                            resubscribe_live(&mut runtime, &mut lifecycle).await?;
                        }
                    }
                    runtime.showing_overlay = false;
                    continue;
                }
                if runtime.client.is_some() && history_idx > 0 {
                    dispatch_presentation_action(
                        &mut runtime,
                        &mut lifecycle,
                        PresentationAction::CapturePageTransition,
                    )
                    .await?;
                    // Use the current page's arrival transition, reversed
                    let back_artifact = runtime.history.iter().find(|artifact| {
                        lifecycle
                            .history
                            .get(history_idx)
                            .is_some_and(|entry| artifact.id == entry.id)
                    });
                    let back_kind = back_artifact
                        .and_then(|artifact| artifact.transition)
                        .map(reverse_transition);
                    let back_duration =
                        back_artifact.map_or(0, |artifact| artifact.transition_duration_ms);
                    let target = history_idx - 1;
                    if let Some(ActivatedNavigation::Cached { entry, .. }) =
                        dispatch_navigation_event(
                            &mut runtime,
                            &mut lifecycle,
                            LifecycleEvent::Back,
                        )
                        .await?
                    {
                        history_idx = lifecycle.history_position.unwrap_or(target);
                        debug_assert_eq!(lifecycle.current_uri.as_ref(), Some(&entry.uri));
                        reset_input_mode(&mut runtime.input_mode);
                        resubscribe_live(&mut runtime, &mut lifecycle).await?;
                        dispatch_presentation_action(
                            &mut runtime,
                            &mut lifecycle,
                            PresentationAction::PageLoad,
                        )
                        .await?;
                        dispatch_presentation_action(
                            &mut runtime,
                            &mut lifecycle,
                            PresentationAction::StartPageTransition {
                                kind: back_kind.unwrap_or(ast::TransitionKind::Cut),
                                duration_ms: back_duration,
                            },
                        )
                        .await?;
                        invalidate_compositor_for_new_scene(&mut runtime.compositor);
                        runtime.needs_redraw = true;
                    }
                }
            }
            ViewerAction::GoForward => {
                runtime.showing_overlay = false;
                if runtime.client.is_some() && history_idx + 1 < lifecycle.history.len() {
                    dispatch_presentation_action(
                        &mut runtime,
                        &mut lifecycle,
                        PresentationAction::CapturePageTransition,
                    )
                    .await?;
                    let target = history_idx + 1;
                    // Replay the transition that was originally used to arrive at the target
                    let target_artifact = runtime.history.iter().find(|artifact| {
                        lifecycle
                            .history
                            .get(target)
                            .is_some_and(|entry| artifact.id == entry.id)
                    });
                    let fwd_kind = target_artifact.and_then(|artifact| artifact.transition);
                    let fwd_duration =
                        target_artifact.map_or(0, |artifact| artifact.transition_duration_ms);
                    if let Some(ActivatedNavigation::Cached { entry, .. }) =
                        dispatch_navigation_event(
                            &mut runtime,
                            &mut lifecycle,
                            LifecycleEvent::Forward,
                        )
                        .await?
                    {
                        history_idx = lifecycle.history_position.unwrap_or(target);
                        debug_assert_eq!(lifecycle.current_uri.as_ref(), Some(&entry.uri));
                        reset_input_mode(&mut runtime.input_mode);
                        resubscribe_live(&mut runtime, &mut lifecycle).await?;
                        dispatch_presentation_action(
                            &mut runtime,
                            &mut lifecycle,
                            PresentationAction::PageLoad,
                        )
                        .await?;
                        dispatch_presentation_action(
                            &mut runtime,
                            &mut lifecycle,
                            PresentationAction::StartPageTransition {
                                kind: fwd_kind.unwrap_or(ast::TransitionKind::Cut),
                                duration_ms: fwd_duration,
                            },
                        )
                        .await?;
                        invalidate_compositor_for_new_scene(&mut runtime.compositor);
                        runtime.needs_redraw = true;
                    }
                }
            }
            ViewerAction::Activate => {
                runtime.showing_overlay = false;
                let current_idx =
                    current_focus_index(&runtime.page.scene, &runtime.page.focusables);
                if deferred_activation.is_some() || current_idx.is_some() {
                    // Non-navigation activation branches still use this index;
                    // deferred activations are always captured Navigate actions.
                    let idx = current_idx.unwrap_or(0);
                    let action = match deferred_activation.take() {
                        Some(action) => Some(action),
                        None => match current_idx
                            .and_then(|current| runtime.page.focusables.get(current))
                        {
                            Some(focusable) => match focusable.action.try_clone() {
                                Ok(action) => Some(action),
                                Err(_) => {
                                    runtime.command_line.set_message(
                                        "error: action preparation exhausted client memory",
                                        true,
                                    );
                                    runtime.needs_redraw = true;
                                    continue;
                                }
                            },
                            None => None,
                        },
                    };
                    if let Some(action) = action {
                        match action {
                            crate::compositor::panels::FocusAction::Navigate {
                                href,
                                transition: link_transition,
                                transition_duration_ms: link_duration,
                                defer_animation,
                            } => {
                                let mut navigation_deferred = false;
                                if let Some(wait_for) = defer_animation {
                                    if runtime.deferred_navigation.is_some() {
                                        navigation_deferred = true;
                                    } else if lifecycle.scope.is_some() {
                                        let proposal_href = match try_owned_string(&href) {
                                            Ok(href) => href,
                                            Err(_) => {
                                                runtime.command_line.set_message(
                                                    "error: deferred navigation exhausted client memory",
                                                    true,
                                                );
                                                runtime.needs_redraw = true;
                                                continue;
                                            }
                                        };
                                        // No current scope means there is
                                        // nothing to defer against. Skip the
                                        // proposal rather than inventing a
                                        // generation: `can_resume` compares
                                        // generations, so a substituted value
                                        // could wrongly match a real one.
                                        let Some(current_scope) = lifecycle.scope.as_ref() else {
                                            continue;
                                        };
                                        runtime.deferred_proposal = Some(DeferredProposal {
                                            generation: current_scope.generation,
                                            wait_for,
                                            action:
                                                crate::compositor::panels::FocusAction::Navigate {
                                                    href: proposal_href,
                                                    transition: link_transition,
                                                    transition_duration_ms: link_duration,
                                                    defer_animation: None,
                                                },
                                        });
                                        super::dispatch_runtime_events(
                                            &mut runtime,
                                            &mut lifecycle,
                                            [LifecycleEvent::DeferredNavigationRequested],
                                        )
                                        .await?;
                                        navigation_deferred = runtime.deferred_navigation.is_some();
                                    }
                                }

                                if !navigation_deferred && runtime.client.is_some() {
                                    let new_uri = match lifecycle
                                        .current_uri
                                        .as_ref()
                                        .and_then(|u| u.resolve(&href).ok())
                                    {
                                        Some(u) => u,
                                        None => continue,
                                    };

                                    // Capture old viewport snapshot before loading new page
                                    dispatch_presentation_action(
                                        &mut runtime,
                                        &mut lifecycle,
                                        PresentationAction::CapturePageTransition,
                                    )
                                    .await?;

                                    let Some(Ok(origin)) = runtime
                                        .client
                                        .as_ref()
                                        .map(|client| client.request_origin(&new_uri))
                                    else {
                                        continue;
                                    };
                                    if let Some(ActivatedNavigation::Network { .. }) =
                                        dispatch_navigation_event(
                                            &mut runtime,
                                            &mut lifecycle,
                                            LifecycleEvent::Navigate {
                                                uri: new_uri,
                                                origin,
                                            },
                                        )
                                        .await?
                                    {
                                        // Determine transition: link override > new page default > Cut
                                        let (trans_kind, trans_duration) = resolve_transition(
                                            link_transition,
                                            link_duration,
                                            runtime.page.scene.transition,
                                            runtime.page.scene.transition_duration_ms,
                                        );

                                        set_active_history_transition(
                                            &mut runtime.history,
                                            &lifecycle,
                                            trans_kind,
                                            trans_duration,
                                        );
                                        reset_input_mode(&mut runtime.input_mode);
                                        resubscribe_live(&mut runtime, &mut lifecycle).await?;
                                        dispatch_presentation_action(
                                            &mut runtime,
                                            &mut lifecycle,
                                            PresentationAction::PageLoad,
                                        )
                                        .await?;

                                        dispatch_presentation_action(
                                            &mut runtime,
                                            &mut lifecycle,
                                            PresentationAction::StartPageTransition {
                                                kind: trans_kind
                                                    .unwrap_or(ast::TransitionKind::Cut),
                                                duration_ms: trans_duration,
                                            },
                                        )
                                        .await?;
                                        invalidate_compositor_for_new_scene(
                                            &mut runtime.compositor,
                                        );
                                        runtime.needs_redraw = true;
                                    }
                                }
                            }
                            crate::compositor::panels::FocusAction::Toggle {
                                ref panel_id,
                                ref states,
                            } => {
                                dispatch_presentation_action(
                                    &mut runtime,
                                    &mut lifecycle,
                                    PresentationAction::TogglePanel {
                                        panel_id: panel_id.clone(),
                                        states: states.clone(),
                                    },
                                )
                                .await?;
                            }
                            crate::compositor::panels::FocusAction::Set {
                                ref panel_id,
                                ref state_name,
                            } => {
                                dispatch_presentation_action(
                                    &mut runtime,
                                    &mut lifecycle,
                                    PresentationAction::SetPanel {
                                        panel_id: panel_id.clone(),
                                        state: state_name.clone(),
                                    },
                                )
                                .await?;
                            }
                            crate::compositor::panels::FocusAction::ToggleDetails {
                                details_index,
                            } => {
                                dispatch_presentation_action(
                                    &mut runtime,
                                    &mut lifecycle,
                                    PresentationAction::ToggleDetails {
                                        index: details_index,
                                    },
                                )
                                .await?;
                            }
                            crate::compositor::panels::FocusAction::EditInput {
                                maxlen,
                                password,
                                ..
                            } => {
                                if lifecycle.connection == ConnectionStatus::Connected {
                                    let field = runtime.page.focusables.get(idx);
                                    let node = field.map(|field| field.node_id);
                                    let value = node
                                        .and_then(|node_id| runtime.page.scene.get(node_id))
                                        .and_then(|node| match node.kind() {
                                            NodeKind::Input(data) => data.value.as_deref(),
                                            _ => None,
                                        })
                                        .unwrap_or_default();
                                    let (col, row, sticky) = field
                                        .map(|field| (field.col, field.row, field.is_sticky))
                                        .unwrap_or_default();
                                    if runtime.input_mode.try_activate(
                                        node,
                                        value,
                                        maxlen,
                                        password,
                                        (col, row, sticky),
                                    ) {
                                        runtime.needs_redraw = true;
                                    }
                                }
                            }
                            crate::compositor::panels::FocusAction::EditSelect { .. } => {
                                if let Some(node) =
                                    runtime.page.focusables.get(idx).map(|f| f.node_id)
                                {
                                    dispatch_presentation_action(
                                        &mut runtime,
                                        &mut lifecycle,
                                        PresentationAction::AdvanceSelect { node },
                                    )
                                    .await?;
                                }
                            }
                            crate::compositor::panels::FocusAction::Submit { ref target, form }
                                if runtime.client.is_some() =>
                            {
                                let form_data = match form {
                                    Some(id) => collect_form_values(&runtime.page.scene, id)
                                        .and_then(|values| url_encode_form(&values)),
                                    None => Ok(String::new()),
                                };
                                let form_data = match form_data {
                                    Ok(form_data) => form_data,
                                    Err(_) => {
                                        runtime.command_line.set_message(
                                            "error: form serialization exhausted client memory",
                                            true,
                                        );
                                        runtime.needs_redraw = true;
                                        continue;
                                    }
                                };
                                let action = if target.is_empty() {
                                    form.and_then(|id| form_action(&runtime.page.scene, id))
                                        .unwrap_or_default()
                                } else {
                                    target.clone()
                                };
                                if !action.is_empty() {
                                    let submitted = if let Some(uri) = lifecycle
                                        .current_uri
                                        .as_ref()
                                        .and_then(|uri| uri.try_clone().ok())
                                    {
                                        match runtime
                                            .client
                                            .as_ref()
                                            .map(|client| client.request_origin(&uri))
                                        {
                                            Some(Ok(origin)) => dispatch_navigation_event(
                                                &mut runtime,
                                                &mut lifecycle,
                                                LifecycleEvent::FormSubmitted {
                                                    uri,
                                                    origin,
                                                    path: action.clone(),
                                                    form_data: form_data.clone(),
                                                },
                                            )
                                            .await
                                            .map_err(|error| error.to_string()),
                                            Some(Err(error)) => Err(error.to_string()),
                                            None => Err("not connected".into()),
                                        }
                                    } else {
                                        Err("not connected".into())
                                    };
                                    match submitted {
                                        Ok(Some(ActivatedNavigation::Network { .. })) => {
                                            reset_input_mode(&mut runtime.input_mode);
                                            resubscribe_live(&mut runtime, &mut lifecycle).await?;
                                            dispatch_presentation_action(
                                                &mut runtime,
                                                &mut lifecycle,
                                                PresentationAction::PageLoad,
                                            )
                                            .await?;
                                        }
                                        Ok(_) => runtime.command_line.set_message(
                                            "error: server returned an invalid page",
                                            true,
                                        ),
                                        Err(message) => runtime.command_line.set_message_args(
                                            format_args!("error: {message}"),
                                            true,
                                        ),
                                    }
                                }
                                runtime.needs_redraw = true;
                            }
                            _ => {}
                        }
                    }
                }
            }
            ViewerAction::TabNext | ViewerAction::FocusNext => {
                dispatch_presentation_action(
                    &mut runtime,
                    &mut lifecycle,
                    PresentationAction::AdvanceFocus { forward: true },
                )
                .await?;
            }
            ViewerAction::TabPrev | ViewerAction::FocusPrev => {
                dispatch_presentation_action(
                    &mut runtime,
                    &mut lifecycle,
                    PresentationAction::AdvanceFocus { forward: false },
                )
                .await?;
            }
            ViewerAction::EnterCommandMode => {
                runtime.command_line.activate("");
                runtime.needs_redraw = true;
            }
            ViewerAction::EnterCommandModeOpen => {
                runtime.command_line.activate("open ");
                runtime.needs_redraw = true;
            }
            ViewerAction::ShowHelp => {
                runtime.help_visible = true;
                runtime.needs_redraw = true;
            }
            ViewerAction::ShowHud => {
                runtime.help_visible = false;
                runtime
                    .client_hud
                    .toggle(history_idx, lifecycle.history.len());
                runtime.compositor.invalidate_presented();
                runtime.needs_redraw = true;
            }
            ViewerAction::JumpHistory(target) => {
                runtime.showing_overlay = false;
                if target < lifecycle.history.len()
                    && target != history_idx
                    && runtime.client.is_some()
                    && let Some(ActivatedNavigation::Cached { entry, .. }) =
                        dispatch_navigation_event(
                            &mut runtime,
                            &mut lifecycle,
                            LifecycleEvent::JumpToHistory { index: target },
                        )
                        .await?
                {
                    history_idx = lifecycle.history_position.unwrap_or(target);
                    debug_assert_eq!(lifecycle.current_uri.as_ref(), Some(&entry.uri));
                    reset_input_mode(&mut runtime.input_mode);
                    runtime.deferred_navigation = None;
                    resubscribe_live(&mut runtime, &mut lifecycle).await?;
                    dispatch_presentation_action(
                        &mut runtime,
                        &mut lifecycle,
                        PresentationAction::PageLoad,
                    )
                    .await?;
                    invalidate_compositor_for_new_scene(&mut runtime.compositor);
                }
                runtime.needs_redraw = true;
            }
            ViewerAction::ClearFocus => {
                dispatch_presentation_action(
                    &mut runtime,
                    &mut lifecycle,
                    PresentationAction::ClearFocus,
                )
                .await?;
            }
            ViewerAction::Reload => {
                command_action = Some(ParsedCommand::Reload);
            }
            ViewerAction::None | ViewerAction::Resize { .. } => {}
        }

        // Execute parsed command from command line
        if let Some(cmd) = command_action.take() {
            // Help is a client modal, so it leaves a synthetic sessions page intact.
            if !matches!(cmd, ParsedCommand::Sessions | ParsedCommand::Help) {
                runtime.showing_overlay = false;
            }
            match cmd {
                ParsedCommand::Quit => break,
                ParsedCommand::Reload => {
                    if runtime.client.is_some() {
                        match dispatch_navigation_event(
                            &mut runtime,
                            &mut lifecycle,
                            LifecycleEvent::Reload,
                        )
                        .await?
                        {
                            Some(ActivatedNavigation::Network { .. }) => {
                                clear_terminal_for_full_redraw(
                                    &mut stdout,
                                    &mut runtime.compositor,
                                )?;
                                reset_input_mode(&mut runtime.input_mode);
                                resubscribe_live(&mut runtime, &mut lifecycle).await?;
                                dispatch_presentation_action(
                                    &mut runtime,
                                    &mut lifecycle,
                                    PresentationAction::PageLoad,
                                )
                                .await?;
                                runtime.needs_redraw = true;
                            }
                            _ => {
                                runtime
                                    .command_line
                                    .set_message("error: reload failed", true);
                                runtime.needs_redraw = true;
                            }
                        }
                    } else {
                        runtime
                            .command_line
                            .set_message("error: not connected", true);
                        runtime.needs_redraw = true;
                    }
                }
                ParsedCommand::Open(uri_str) => {
                    if runtime.client.is_some() {
                        let base_uri =
                            logical_history_entry(&lifecycle, history_idx).map(|entry| &entry.uri);
                        let target_uri = if uri_str.starts_with("atp://") {
                            AtpUri::parse(&uri_str).ok()
                        } else if let Some(base) = base_uri {
                            base.resolve(&uri_str).ok()
                        } else {
                            AtpUri::parse(&format!("atp://{uri_str}")).ok()
                        };

                        match target_uri {
                            Some(uri) => {
                                dispatch_presentation_action(
                                    &mut runtime,
                                    &mut lifecycle,
                                    PresentationAction::CapturePageTransition,
                                )
                                .await?;
                                let Some(Ok(origin)) = runtime
                                    .client
                                    .as_ref()
                                    .map(|client| client.request_origin(&uri))
                                else {
                                    runtime
                                        .command_line
                                        .set_message("error: invalid origin", true);
                                    runtime.needs_redraw = true;
                                    continue;
                                };
                                let navigation_uri = match uri.try_clone() {
                                    Ok(uri) => uri,
                                    Err(_) => {
                                        runtime
                                            .command_line
                                            .set_message("error: URI allocation failed", true);
                                        runtime.needs_redraw = true;
                                        continue;
                                    }
                                };
                                match dispatch_navigation_event(
                                    &mut runtime,
                                    &mut lifecycle,
                                    LifecycleEvent::Navigate {
                                        uri: navigation_uri,
                                        origin,
                                    },
                                )
                                .await?
                                {
                                    Some(ActivatedNavigation::Network { final_uri, .. }) => {
                                        let (trans_kind, trans_duration) = resolve_transition(
                                            None,
                                            0,
                                            runtime.page.scene.transition,
                                            runtime.page.scene.transition_duration_ms,
                                        );

                                        if final_uri.to_string() != uri.to_string() {
                                            runtime.command_line.set_message_args(
                                                format_args!("redirected to {final_uri}"),
                                                false,
                                            );
                                        }

                                        set_active_history_transition(
                                            &mut runtime.history,
                                            &lifecycle,
                                            trans_kind,
                                            trans_duration,
                                        );
                                        reset_input_mode(&mut runtime.input_mode);
                                        resubscribe_live(&mut runtime, &mut lifecycle).await?;
                                        dispatch_presentation_action(
                                            &mut runtime,
                                            &mut lifecycle,
                                            PresentationAction::PageLoad,
                                        )
                                        .await?;

                                        dispatch_presentation_action(
                                            &mut runtime,
                                            &mut lifecycle,
                                            PresentationAction::StartPageTransition {
                                                kind: trans_kind
                                                    .unwrap_or(ast::TransitionKind::Cut),
                                                duration_ms: trans_duration,
                                            },
                                        )
                                        .await?;
                                        invalidate_compositor_for_new_scene(
                                            &mut runtime.compositor,
                                        );
                                        runtime.needs_redraw = true;
                                    }
                                    _ => {
                                        runtime.command_line.set_message_args(
                                            format_args!("error: could not load {uri_str}"),
                                            true,
                                        );
                                        runtime.needs_redraw = true;
                                    }
                                }
                            }
                            None => {
                                runtime.command_line.set_message_args(
                                    format_args!("error: invalid URI: {uri_str}"),
                                    true,
                                );
                                runtime.needs_redraw = true;
                            }
                        }
                    } else {
                        runtime
                            .command_line
                            .set_message("error: not connected", true);
                        runtime.needs_redraw = true;
                    }
                }
                ParsedCommand::Sessions => {
                    let empty = crate::session::SessionStore::default();
                    let sessions = runtime
                        .client
                        .as_ref()
                        .map(AtpClient::sessions)
                        .unwrap_or(&empty);
                    let aml_content = render_sessions_page(sessions);
                    dispatch_presentation_action(
                        &mut runtime,
                        &mut lifecycle,
                        PresentationAction::ActivateLocalPage {
                            aml: aml_content,
                            uri: None,
                            overlay: true,
                        },
                    )
                    .await?;
                }
                ParsedCommand::SessionsClear(site) => {
                    if let Some(ref mut c) = runtime.client {
                        match site {
                            Some(ref site_key) => {
                                c.clear_sessions_for_storage_key(site_key);
                                runtime.command_line.set_message_args(
                                    format_args!("cleared sessions for {site_key}"),
                                    false,
                                );
                            }
                            None => {
                                c.clear_all_sessions();
                                runtime
                                    .command_line
                                    .set_message("cleared all sessions", false);
                            }
                        }
                    } else {
                        runtime
                            .command_line
                            .set_message("no active sessions", false);
                    }
                    runtime.needs_redraw = true;
                }
                ParsedCommand::Help => {
                    runtime.help_visible = true;
                    runtime.needs_redraw = true;
                }
                ParsedCommand::Unknown(s) => {
                    runtime
                        .command_line
                        .set_message_args(format_args!("error: unknown command: {s}"), true);
                    runtime.needs_redraw = true;
                }
            }
        }

        // ─── Stage 5: layout pass (invalidation drain) ────────
        // Scope-layout the subtrees flagged in `scene.invalidation.layout`
        // and clear the invalidation sets so they don't accumulate across
        // ticks. Event-triggered paths (panel toggle, details toggle,
        // resize, navigation) still call `relayout_panels_for` /
        // `full_layout_pass` directly for full-page rebuilds; this stage
        // is the per-tick steady-state drain.
        dispatch_presentation_action(
            &mut runtime,
            &mut lifecycle,
            PresentationAction::DrainLayout,
        )
        .await?;
        let focused = runtime.page.scene.focus();
        if let Some(token) = lifecycle.control_token() {
            super::dispatch_runtime_events(
                &mut runtime,
                &mut lifecycle,
                [LifecycleEvent::FocusChanged { token, focused }],
            )
            .await?;
        }
    }

    super::dispatch_runtime_events(&mut runtime, &mut lifecycle, [LifecycleEvent::Shutdown])
        .await?;
    Ok(())
}

// ─── Panel relayout and transitions ─────────────────────────

/// Capture the panel's last laid-out static contribution before changing its
/// active state. Panel patches update scene visibility immediately, so taking
/// this snapshot inside `relayout_panels_for` would already see the new state.
fn capture_panel_transition_source(
    page: &LoadedPage,
    panel_id: &str,
) -> Option<(Rect, CellBuffer)> {
    let rect = page
        .panels()
        .find(|placed| placed.id == panel_id)
        .map(|placed| placed.rect)?;
    let static_buf = crate::compositor::composite::walk_static_governed(
        &page.scene,
        page.buf.width,
        page.buf.height,
        &page.governor,
    );
    if static_buf.allocation_failed() {
        return Some((rect, static_buf));
    }
    Some((
        rect,
        extract_sub_buffer(&static_buf, rect.x, rect.y, rect.w, rect.h, &page.governor),
    ))
}

/// Re-layout after a patch-driven scene mutation (panel state change,
/// details toggle, or similar in-page state change). The caller has
/// already applied the patch, which populated `scene.invalidation.layout`.
///
/// The invalidation drain re-lays each flagged subtree in place, cascading to
/// parents when dimensions change. Derived focus, placement, and sticky state
/// is then refreshed. The animation runtime is rebuilt only when animation
/// topology changes, and a panel transition is constructed from the snapshot
/// the caller captured before applying the state patch.
async fn relayout_panels_for(
    page: &mut LoadedPage,
    state: &mut ViewportState,
    color_support: ColorSupport,
    wcfg: WidthConfig,
    target_panel_id: Option<&str>,
    old_panel: Option<(Rect, CellBuffer)>,
    client: Option<&mut AtpClient>,
) -> bool {
    let governor = page.governor.clone();
    let Ok(mut candidate_page_buf) = page
        .buf
        .try_clone_governed(&governor, ResourceCategory::CompositorCells)
    else {
        return false;
    };
    let Some((mut next_focusables, mut next_placed, mut sticky_regions, mut next_projection_lease)) =
        reserve_projection_collections(&page.scene, Some(&governor))
    else {
        return false;
    };
    // Drain the invalidation set. Re-lays each flagged subtree in
    // place, writes screen-absolute placements back to the scene, and
    // blits new cells into `page.buf`. Cascades to parents on
    // size change — so a details-open inside a Flow container
    // bubbles up the tree as far as is needed.
    if !layout_pass_invalidated_governed(
        &mut page.scene,
        &mut candidate_page_buf,
        color_support,
        wcfg,
        &governor,
    ) {
        return false;
    }

    // Re-derive state from the (now-updated) scene.
    populate_projection_collections(
        &page.scene,
        &mut next_focusables,
        &mut next_placed,
        &mut sticky_regions,
    );
    if reconcile_projection_lease(
        &next_focusables,
        next_focusables.capacity(),
        &next_placed,
        next_placed.capacity(),
        sticky_regions.capacity(),
        &mut next_projection_lease,
    )
    .is_none()
    {
        return false;
    }
    let (main_buf, sticky_buf) = split_sticky(
        candidate_page_buf,
        &sticky_regions,
        &mut next_focusables,
        Some(&governor),
    );
    if main_buf.allocation_failed()
        || sticky_buf
            .as_ref()
            .is_some_and(CellBuffer::allocation_failed)
    {
        return false;
    }

    // Re-hydrate per-node buffers and rebuild the animation runtime.
    // Animations that appeared or vanished inside the affected subtree
    // get the right wiring; finished animations are re-marked so their
    // `animation-end` handlers don't re-fire.
    hydrate_scene_buffers(&mut page.scene, &next_placed);
    if page.scene.resource_limit_exceeded() {
        return false;
    }
    let topology_changed = animation_topology_changed(page);
    let mut next_anim_rt = if topology_changed {
        let runtime = if client.is_some() || !page.prepared_wasm.is_empty() {
            AnimationRuntime::from_scene_with_prepared_wasm(
                &mut page.scene,
                color_support,
                wcfg,
                &governor,
                &page.prepared_wasm,
            )
            .await
        } else {
            Ok(AnimationRuntime::from_scene(
                &mut page.scene,
                color_support,
                wcfg,
                page.wasm_dir.as_deref(),
            )
            .await)
        };
        let Ok(mut runtime) = runtime else {
            return false;
        };
        for animation in &page.anim_rt.animations {
            if animation.finished() {
                runtime.trigger_stop(animation.id());
            }
        }
        for placed in &next_placed {
            if placed.is_animation()
                && let Some(node) = page.scene.find_by_aml_id(&placed.id)
                && !page.scene.stage_buffer_for_relayout(node)
            {
                return false;
            }
        }
        runtime.paint_into_scene(&mut page.scene);
        if page.scene.resource_limit_exceeded() {
            return false;
        }
        Some(runtime)
    } else {
        None
    };

    // Construct a transition adapter for non-Cut panel transitions.
    let mut next_transition = None;
    if let Some(panel_id) = target_panel_id
        && let Some((kind, duration_ms)) = find_panel_transition(&page.scene, panel_id)
        && kind != ast::TransitionKind::Cut
        && duration_ms > 0
    {
        let new_region = page
            .placed
            .iter()
            .find(|p| p.is_panel() && p.id == panel_id);

        if let (Some((old_r, old_sub)), Some(new_p)) = (old_panel, new_region) {
            let new_r = new_p.rect;
            // Same pivot concern as old_buf above: `page.buf`
            // is empty post-Phase-6, so the new region must come
            // from compositing the just-updated scene.
            // Same reasoning as `old_buf` above: static composite
            // only, so the panel buffer doesn't end up holding a
            // frozen snapshot of the matrix rain underneath.
            let new_buf = crate::compositor::composite::walk_static_governed(
                &page.scene,
                main_buf.width,
                main_buf.height,
                &governor,
            );
            let new_sub =
                extract_sub_buffer(&new_buf, new_r.x, new_r.y, new_r.w, new_r.h, &governor);

            if old_sub.allocation_failed()
                || new_buf.allocation_failed()
                || new_sub.allocation_failed()
            {
                return false;
            }

            let union_x = old_r.x.min(new_r.x);
            let union_y = old_r.y.min(new_r.y);
            let union_right = old_r.right().max(new_r.right());
            let union_bottom = old_r.bottom().max(new_r.bottom());
            let target = Rect::new(
                union_x,
                union_y,
                union_right - union_x,
                union_bottom - union_y,
            );

            let node = page.scene.find_by_aml_id(panel_id).unwrap_or_default();
            let Some(transition_id) = try_panel_transition_id(panel_id) else {
                return false;
            };
            next_transition = Some(crate::compositor::animate::TransitionAdapter::new(
                transition_id,
                node,
                target,
                old_sub,
                old_r,
                new_sub,
                new_r,
                kind,
                duration_ms,
            ));
        }
    }

    if let Some(transition) = next_transition.take() {
        let accepted = if let Some(runtime) = next_anim_rt.as_mut() {
            runtime.try_push_transition(transition)
        } else if page.anim_rt.can_push_transition() {
            next_transition = Some(transition);
            true
        } else {
            false
        };
        if !accepted {
            return false;
        }
    }

    page.scene.commit_relayout_transaction();
    page.focusables = next_focusables;
    page.placed = next_placed;
    page._projection_lease = next_projection_lease;
    page._sticky_regions = sticky_regions;
    page.buf = main_buf;
    page.sticky_buf = sticky_buf;
    if let Some(runtime) = next_anim_rt {
        page.anim_rt = runtime;
    } else if let Some(transition) = next_transition {
        debug_assert!(page.anim_rt.try_push_transition(transition));
    }

    state.content_height = page.buf.height;
    state.scroll_offset = state.scroll_offset.min(state.max_scroll());
    true
}

/// Scene-native lookup of the active state's transition for a panel.
/// Reads `FlowData.state_transition` / `state_transition_duration_ms`
/// from the currently-active state node.
fn find_panel_transition(scene: &Scene, panel_id: &str) -> Option<(ast::TransitionKind, u32)> {
    use crate::compositor::scene::FlowSource;
    let panel_node_id = scene.find_by_aml_id(panel_id)?;
    let active_id = match scene.get(panel_node_id)?.kind() {
        NodeKind::Panel { active, .. } => *active,
        _ => return None,
    };
    let state_node = scene.get(active_id)?;
    match state_node.kind() {
        NodeKind::Flow(d) if matches!(d.source, FlowSource::State) => {
            let kind = d.state_transition?;
            Some((kind, d.state_transition_duration_ms))
        }
        _ => None,
    }
}

// ─── Event dispatch helpers ─────────────────────────────────

/// Resume the page's exact authored-action queue.
///
/// Each item remains queued until its mutation commits. A pressure failure
/// therefore retries the same item without replaying earlier actions or
/// enqueueing its `state-change` cascade twice.
async fn execute_on_actions(
    bindings: &[EventBinding],
    page: &mut LoadedPage,
    state: &mut ViewportState,
    color_support: ColorSupport,
    wcfg: WidthConfig,
    mut client: Option<&mut AtpClient>,
    event_dispatcher: &mut EventDispatcher,
    needs_redraw: &mut bool,
) -> Result<(), String> {
    while let Some((pending_index, scheduled)) = event_dispatcher.next_ready() {
        if scheduled.cascade_depth >= MAX_CASCADE_DEPTH {
            event_dispatcher.complete(pending_index, scheduled);
            continue;
        }
        let Some(action) = bindings.get(scheduled.binding_index) else {
            event_dispatcher.complete(pending_index, scheduled);
            continue;
        };
        match action.action {
            ast::ActionKind::Animate => {
                // Start or restart a named animation.
                if page.anim_rt.trigger_start(&action.target) {
                    *needs_redraw = true;
                }
            }
            ast::ActionKind::Stop => {
                if page.anim_rt.trigger_stop(&action.target) {
                    *needs_redraw = true;
                }
            }
            ast::ActionKind::Set => {
                if let Some(ref to) = action.to {
                    let old_panel = capture_panel_transition_source(page, &action.target);
                    let old_active = panel_active_node(&page.scene, &action.target);
                    if let Some(old_active) = old_active {
                        let cascade = event_dispatcher
                            .prepare_fire(
                                bindings,
                                ast::EventKind::StateChange,
                                Some(&action.target),
                                scheduled.cascade_depth.saturating_add(1),
                            )
                            .map_err(|error| error.to_string())?;
                        if !page.scene.begin_relayout_transaction() {
                            return Err(
                                "authored panel relayout transaction exceeded the client resource budget"
                                    .into(),
                            );
                        }
                        if !apply_panel_patch(&mut page.scene, &action.target, to) {
                            page.scene.rollback_relayout_transaction();
                            // Nothing to apply is a settled action, not a
                            // pending one: `apply_panel_patch` answers `false`
                            // for a panel that already holds the requested
                            // state, which any duplicated `set` produces —
                            // fast-forward re-fires `animation-end`, and a
                            // cached page restored by history arrives already
                            // settled. `next_ready` only peeks, so retiring it
                            // here is what stops the queue handing the same
                            // no-op back forever at 100% of a core.
                            event_dispatcher.complete(pending_index, scheduled);
                            continue;
                        }
                        let committed = relayout_panels_for(
                            page,
                            state,
                            color_support,
                            wcfg,
                            Some(&action.target),
                            old_panel,
                            client.as_deref_mut(),
                        )
                        .await;
                        if !committed {
                            let panel = page
                                .scene
                                .find_by_aml_id(&action.target)
                                .unwrap_or_default();
                            PatchApplier::apply(
                                &mut page.scene,
                                Patch::SetPanelActive {
                                    panel,
                                    active: old_active,
                                },
                            );
                            page.scene.rollback_relayout_transaction();
                            return Err(
                                "authored panel relayout exceeded the client resource budget"
                                    .into(),
                            );
                        }
                        *needs_redraw = true;
                        event_dispatcher.commit(cascade);
                    }
                }
            }
            ast::ActionKind::Toggle => {
                // Check that the panel exists in the scene.
                if scene_panel_current_state(&page.scene, &action.target).is_some() {
                    let old_panel = capture_panel_transition_source(page, &action.target);
                    let old_active = panel_active_node(&page.scene, &action.target);
                    if let Some(old_active) = old_active {
                        let cascade = event_dispatcher
                            .prepare_fire(
                                bindings,
                                ast::EventKind::StateChange,
                                Some(&action.target),
                                scheduled.cascade_depth.saturating_add(1),
                            )
                            .map_err(|error| error.to_string())?;
                        if !page.scene.begin_relayout_transaction() {
                            return Err(
                                "authored panel relayout transaction exceeded the client resource budget"
                                    .into(),
                            );
                        }
                        if !toggle_panel_scene_state(&mut page.scene, &action.target) {
                            page.scene.rollback_relayout_transaction();
                            // Same retirement rule as `Set` above: a panel
                            // with fewer than two states has nothing to
                            // toggle, and leaving that action queued spins
                            // the dispatch loop instead of moving past it.
                            event_dispatcher.complete(pending_index, scheduled);
                            continue;
                        }
                        let committed = relayout_panels_for(
                            page,
                            state,
                            color_support,
                            wcfg,
                            Some(&action.target),
                            old_panel,
                            client.as_deref_mut(),
                        )
                        .await;
                        if !committed {
                            let panel = page
                                .scene
                                .find_by_aml_id(&action.target)
                                .unwrap_or_default();
                            PatchApplier::apply(
                                &mut page.scene,
                                Patch::SetPanelActive {
                                    panel,
                                    active: old_active,
                                },
                            );
                            page.scene.rollback_relayout_transaction();
                            return Err(
                                "authored panel relayout exceeded the client resource budget"
                                    .into(),
                            );
                        }
                        *needs_redraw = true;
                        event_dispatcher.commit(cascade);
                    }
                }
            }
        }
        event_dispatcher.complete(pending_index, scheduled);
    }
    Ok(())
}

// ─── Live regions ───────────────────────────────────────────

/// Accumulated cell rows for a live region, supporting scroll and buffer trim.
struct RegionBuffer {
    /// Row-major cells. Flattening removes nested row-header allocations.
    rows: Vec<Cell>,
    row_count: usize,
    width: u16,
    visible_height: u16,
    buffer_limit: u32,
    governor: ResourceGovernor,
    cell_lease: Option<BudgetLease>,
}

impl RegionBuffer {
    fn new(width: u16, visible_height: u16, buffer_limit: u32, governor: ResourceGovernor) -> Self {
        RegionBuffer {
            rows: Vec::new(),
            row_count: 0,
            width,
            visible_height,
            buffer_limit: buffer_limit.max(visible_height as u32),
            governor,
            cell_lease: None,
        }
    }

    /// Replace all content with the given cell buffer.
    #[cfg(test)]
    fn replace(&mut self, mini: &CellBuffer) -> bool {
        self.rebuild(mini, RegionBufferUpdate::Replace)
    }

    /// Append rows from a cell buffer at the bottom.
    #[cfg(test)]
    fn append(&mut self, mini: &CellBuffer) -> bool {
        self.rebuild(mini, RegionBufferUpdate::Append)
    }

    /// Copy the visible portion to the main buffer at the given position.
    fn copy_visible_to(&self, buf: &mut CellBuffer, x: u16, y: u16, scroll: LiveScroll) {
        let total = self.row_count;
        let vis = self.visible_height as usize;

        let start = match scroll {
            LiveScroll::Tail | LiveScroll::Manual => total.saturating_sub(vis),
            LiveScroll::Prepend | LiveScroll::None => 0,
        };

        // Clear the region first
        for ry in 0..self.visible_height {
            for rx in 0..self.width {
                buf.set(x.saturating_add(rx), y.saturating_add(ry), Cell::empty());
            }
        }

        // Copy visible rows
        for (i, row) in (start..total).take(vis).enumerate() {
            let ry = i as u16;
            for rx in 0..self.width {
                let index = row
                    .saturating_mul(usize::from(self.width))
                    .saturating_add(usize::from(rx));
                if let Some(cell) = self.rows.get(index) {
                    buf.set(x.saturating_add(rx), y.saturating_add(ry), cell.clone());
                }
            }
        }
    }

    fn rebuild(&mut self, mini: &CellBuffer, update: RegionBufferUpdate) -> bool {
        let old_len = self.row_count;
        let incoming_len = usize::from(mini.content_height());
        let limit = self.buffer_limit as usize;
        let total = match update {
            RegionBufferUpdate::Replace => incoming_len,
            RegionBufferUpdate::Append | RegionBufferUpdate::Prepend => {
                old_len.saturating_add(incoming_len)
            }
        };
        let final_len = if self.width == 0 { 0 } else { total.min(limit) };
        let start = match update {
            RegionBufferUpdate::Append => total.saturating_sub(final_len),
            RegionBufferUpdate::Replace | RegionBufferUpdate::Prepend => 0,
        };
        let Some(final_cells) = final_len.checked_mul(usize::from(self.width)) else {
            return false;
        };
        let Some(cell_bytes) = final_cells.checked_mul(std::mem::size_of::<Cell>()) else {
            return false;
        };
        // Hold the previous lease while reserving and building the replacement:
        // both generations coexist until the transactional swap below.
        let mut new_lease = if final_cells == 0 {
            None
        } else {
            match self.governor.reserve_with_cost(
                ResourceCategory::SceneCells,
                final_cells,
                cell_bytes,
            ) {
                Ok(lease) => Some(lease),
                Err(_) => return false,
            }
        };

        let mut rebuilt = Vec::new();
        if reject_runner_allocation(RunnerAllocationSite::RegionRows)
            || rebuilt.try_reserve_exact(final_cells).is_err()
        {
            return false;
        }
        for logical in start..start.saturating_add(final_len) {
            let source = match update {
                RegionBufferUpdate::Replace => RegionRowSource::Incoming(logical),
                RegionBufferUpdate::Append if logical < old_len => {
                    RegionRowSource::Existing(logical)
                }
                RegionBufferUpdate::Append => RegionRowSource::Incoming(logical - old_len),
                RegionBufferUpdate::Prepend if logical < incoming_len => {
                    RegionRowSource::Incoming(logical)
                }
                RegionBufferUpdate::Prepend => RegionRowSource::Existing(logical - incoming_len),
            };
            match source {
                RegionRowSource::Existing(index) => {
                    let Some(start) = index.checked_mul(usize::from(self.width)) else {
                        return false;
                    };
                    let Some(end) = start.checked_add(usize::from(self.width)) else {
                        return false;
                    };
                    let Some(existing) = self.rows.get(start..end) else {
                        return false;
                    };
                    rebuilt.extend(existing.iter().cloned());
                }
                RegionRowSource::Incoming(y) => {
                    let Ok(y) = u16::try_from(y) else {
                        return false;
                    };
                    for x in 0..self.width {
                        rebuilt.push(mini.get(x, y).cloned().unwrap_or_else(Cell::empty));
                    }
                }
            }
        }
        let actual_cells = rebuilt.capacity();
        let Some(actual_bytes) = actual_cells.checked_mul(std::mem::size_of::<Cell>()) else {
            return false;
        };
        if let Some(lease) = new_lease.as_mut()
            && lease
                .try_resize_with_cost(actual_cells, actual_bytes)
                .is_err()
        {
            return false;
        }
        self.rows = rebuilt;
        self.row_count = final_len;
        self.cell_lease = new_lease;
        true
    }
}

struct RegionBufferEntry {
    key: SubscriptionRegionKey,
    buffer: RegionBuffer,
}

struct RegionBuffers {
    entries: [Option<RegionBufferEntry>; MAX_ACTIVE_SUBSCRIPTIONS],
}

impl RegionBuffers {
    fn new() -> Self {
        Self {
            entries: std::array::from_fn(|_| None),
        }
    }

    fn get(&self, key: SubscriptionRegionKey) -> Option<&RegionBuffer> {
        self.entries
            .iter()
            .flatten()
            .find(|entry| entry.key == key)
            .map(|entry| &entry.buffer)
    }

    fn update(
        &mut self,
        key: SubscriptionRegionKey,
        width: u16,
        visible_height: u16,
        buffer_limit: u32,
        governor: &ResourceGovernor,
        mini: &CellBuffer,
        update: RegionBufferUpdate,
    ) -> bool {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .flatten()
            .find(|entry| entry.key == key)
        {
            if entry.buffer.width == width
                && entry.buffer.visible_height == visible_height
                && entry.buffer.buffer_limit == buffer_limit.max(u32::from(visible_height))
            {
                return entry.buffer.rebuild(mini, update);
            }
            let mut replacement =
                RegionBuffer::new(width, visible_height, buffer_limit, governor.clone());
            if !replacement.rebuild(mini, update) {
                return false;
            }
            entry.buffer = replacement;
            return true;
        }
        let Some(slot) = self.entries.iter_mut().find(|slot| slot.is_none()) else {
            return false;
        };
        let mut buffer = RegionBuffer::new(width, visible_height, buffer_limit, governor.clone());
        if !buffer.rebuild(mini, update) {
            return false;
        }
        *slot = Some(RegionBufferEntry { key, buffer });
        true
    }

    fn clear(&mut self) {
        for entry in &mut self.entries {
            *entry = None;
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.iter().flatten().count()
    }
}

#[derive(Clone, Copy)]
enum RegionBufferUpdate {
    Replace,
    Append,
    Prepend,
}

enum RegionRowSource {
    Existing(usize),
    Incoming(usize),
}

/// Clear old live subscriptions and subscribe to new page's live regions.
fn client_subscriptions_match_page(client: &AtpClient, page: &LoadedPage) -> bool {
    let unique_live_count = page
        .live_regions()
        .enumerate()
        .filter(|(index, placed)| {
            !page
                .live_regions()
                .take(*index)
                .any(|earlier| earlier.id == placed.id)
        })
        .count();
    unique_live_count == client.active_subscription_count()
        && page.live_regions().enumerate().all(|(live_index, placed)| {
            if page
                .live_regions()
                .take(live_index)
                .any(|earlier| earlier.id == placed.id)
            {
                return true;
            }
            let Some(placed_index) = page
                .placed
                .iter()
                .position(|candidate| std::ptr::eq(candidate, placed))
            else {
                return false;
            };
            let Some(projection) = SubscriptionRegionKey::from_placed_index(placed_index) else {
                return false;
            };
            let PlacedKind::Live {
                endpoint, delta, ..
            } = &placed.kind
            else {
                return false;
            };
            let mode = if *delta {
                SubscribeMode::Delta
            } else {
                SubscribeMode::Replace
            };
            client.has_active_subscription(projection, endpoint, &placed.id, mode)
        })
}

async fn resubscribe_live(
    runtime: &mut TerminalRuntime,
    lifecycle: &mut ReducerPort,
) -> Result<(), ViewerError> {
    let Some(retired_scope) = lifecycle.try_scope_clone() else {
        return Ok(());
    };
    if let Some(scope) = retired_scope {
        super::dispatch_runtime_events(
            runtime,
            lifecycle,
            [LifecycleEvent::SubscriptionsRetired { scope }],
        )
        .await?;
    }

    for index in 0..runtime.page.placed.len() {
        let Some(placed) = runtime.page.placed.get(index) else {
            break;
        };
        if !placed.is_live()
            || runtime
                .page
                .placed
                .get(..index)
                .unwrap_or_default()
                .iter()
                .any(|earlier| earlier.is_live() && earlier.id == placed.id)
        {
            continue;
        }
        let Some(region) = SubscriptionRegionKey::from_placed_index(index) else {
            continue;
        };
        super::dispatch_runtime_events(
            runtime,
            lifecycle,
            [LifecycleEvent::SubscribeRequested { region }],
        )
        .await?;
    }
    Ok(())
}

/// Apply a live UPDATE to its scene-owned live-region buffer, handling
/// delta and scroll modes.
fn apply_live_update(
    update: &crate::protocol::message::UpdateMessage,
    region_key: SubscriptionRegionKey,
    placed: &[PlacedElement],
    scene: &mut Scene,
    color_support: ColorSupport,
    wcfg: WidthConfig,
    region_buffers: &mut RegionBuffers,
    governor: &ResourceGovernor,
) -> bool {
    if let Some(region) = placed
        .get(region_key.placed_index())
        .filter(|placed| placed.is_live() && placed.id == update.region)
    {
        let PlacedKind::Live {
            scroll,
            buffer,
            delta: _,
            endpoint: _,
        } = &region.kind
        else {
            return false;
        };
        // Use the region's visible height as the layout constraint.
        // For scroll modes with accumulation (Tail/Manual/Prepend), the
        // RegionBuffer handles content taller than the visible area.
        let layout_height = region.rect.h;

        let prefix = "[page mode=document]";
        let suffix = "[/page]";
        let Some(aml_len) = prefix
            .len()
            .checked_add(update.content.len())
            .and_then(|len| len.checked_add(suffix.len()))
        else {
            return false;
        };
        let mut aml = String::new();
        if aml.try_reserve_exact(aml_len).is_err() {
            return false;
        }
        aml.push_str(prefix);
        aml.push_str(&update.content);
        aml.push_str(suffix);

        if let Some(doc) = parse_aml(&aml) {
            let mut mini_scene =
                crate::compositor::scene::build::from_document_governed(&doc, governor);
            let layout = crate::compositor::layout::engine::layout_scene_governed(
                &mut mini_scene,
                region.rect.w,
                layout_height,
                color_support,
                wcfg,
                governor,
            );
            if layout.buffer.allocation_failed() || mini_scene.resource_limit_exceeded() {
                return false;
            }
            let mut mini = crate::compositor::composite::walk_governed(
                &mini_scene,
                &AnimationRuntime::empty(),
                region.rect.w,
                layout_height,
                governor,
            );
            if mini.allocation_failed() {
                return false;
            }

            let Some(node_id) = scene.find_by_aml_id(&region.id) else {
                return true;
            };
            let inherited_bg = inherited_background(scene, node_id);
            if inherited_bg.is_some() {
                for y in 0..mini.height {
                    for x in 0..mini.width {
                        if let Some(cell) = mini.get_mut(x, y)
                            && cell.style.bg.is_none()
                        {
                            cell.style.bg = inherited_bg;
                        }
                    }
                }
            }
            let Some(buf) = scene.live_buffer_mut(node_id) else {
                return true;
            };

            match *scroll {
                LiveScroll::None => {
                    // Simple replacement. Clear stale cells from a previous,
                    // larger update before copying the new frame.
                    for y in 0..buf.height {
                        for x in 0..buf.width {
                            buf.set(x, y, Cell::empty());
                        }
                    }
                    for y in 0..mini.height.min(region.rect.h) {
                        for x in 0..mini.width.min(region.rect.w) {
                            if let Some(cell) = mini.get(x, y) {
                                buf.set(x, y, cell.clone());
                            }
                        }
                    }
                }
                scroll @ (LiveScroll::Tail | LiveScroll::Manual) => {
                    let update = if update.flags.delta {
                        RegionBufferUpdate::Append
                    } else {
                        RegionBufferUpdate::Replace
                    };
                    if !region_buffers.update(
                        region_key,
                        region.rect.w,
                        region.rect.h,
                        *buffer,
                        governor,
                        &mini,
                        update,
                    ) {
                        return false;
                    }
                    let Some(installed) = region_buffers.get(region_key) else {
                        return false;
                    };
                    installed.copy_visible_to(buf, 0, 0, scroll);
                }
                LiveScroll::Prepend => {
                    let update = if update.flags.delta {
                        RegionBufferUpdate::Prepend
                    } else {
                        RegionBufferUpdate::Replace
                    };
                    if !region_buffers.update(
                        region_key,
                        region.rect.w,
                        region.rect.h,
                        *buffer,
                        governor,
                        &mini,
                        update,
                    ) {
                        return false;
                    }
                    let Some(installed) = region_buffers.get(region_key) else {
                        return false;
                    };
                    installed.copy_visible_to(buf, 0, 0, LiveScroll::Prepend);
                }
            }
            let rect = scene.get(node_id).map(|node| node.placement().rect);
            if let Some(rect) = rect {
                scene.invalidation.mark_composite(rect);
            }
        }
    }
    true
}

/// Resolve the nearest ancestor background at a live region's origin.
///
/// UPDATE payloads are laid out in an isolated mini-scene, so their cells do
/// not naturally inherit styling from an enclosing box. Carrying that color
/// forward prevents updated text from punching holes through the box surface.
fn inherited_background(
    scene: &Scene,
    node_id: crate::compositor::scene::NodeId,
) -> Option<crate::color::ResolvedColor> {
    let node = scene.get(node_id)?;
    let origin = node.placement().rect;
    let mut parent = node.parent();

    while let Some(parent_id) = parent {
        let ancestor = scene.get(parent_id)?;
        let rect = ancestor.placement().rect;
        if let Some(buffer) = ancestor.buffer()
            && origin.x >= rect.x
            && origin.y >= rect.y
        {
            let local_x = origin.x - rect.x;
            let local_y = origin.y - rect.y;
            if let Some(bg) = buffer.get(local_x, local_y).and_then(|cell| cell.style.bg) {
                return Some(bg);
            }
        }
        parent = ancestor.parent();
    }

    None
}

// ─── Input mode key handling ────────────────────────────────

/// Handle a key event while in input mode.
fn handle_input_key(
    key: KeyEvent,
    input: &mut InputMode,
    needs_redraw: &mut bool,
) -> (ViewerAction, Option<String>) {
    let action = match key.code {
        KeyCode::Esc => {
            input.active = false;
            ViewerAction::ClearFocus
        }
        KeyCode::Enter => {
            // Exit input mode but don't submit — only the Submit button submits
            input.active = false;
            ViewerAction::Redraw
        }
        KeyCode::Tab => {
            // Exit input mode, advance focus to next field
            input.active = false;
            ViewerAction::TabNext
        }
        KeyCode::BackTab => {
            // Exit input mode, move focus to previous field
            input.active = false;
            ViewerAction::TabPrev
        }
        KeyCode::Backspace => {
            if input.cursor_pos > 0 {
                let mut candidate = match try_copy_input_value(
                    &input.current_value,
                    0,
                    InputValueAllocationSite::Growth,
                ) {
                    Some(candidate) => candidate,
                    None => return (ViewerAction::None, None),
                };
                let end = grapheme_byte_offset(&candidate, input.cursor_pos);
                let cursor_pos = input.cursor_pos - 1;
                let start = grapheme_byte_offset(&candidate, cursor_pos);
                candidate.replace_range(start..end, "");
                let Some(projection) = try_copy_input_value(
                    &candidate,
                    input.maxlen,
                    InputValueAllocationSite::Projection,
                ) else {
                    return (ViewerAction::None, None);
                };
                input.current_value = candidate;
                input.cursor_pos = cursor_pos;
                *needs_redraw = true;
                return (ViewerAction::None, Some(projection));
            }
            ViewerAction::None
        }
        KeyCode::Left => {
            if input.cursor_pos > 0 {
                input.cursor_pos -= 1;
            }
            ViewerAction::None
        }
        KeyCode::Right => {
            let val = &input.current_value;
            if input.cursor_pos
                < unicode_segmentation::UnicodeSegmentation::graphemes(val.as_str(), true).count()
            {
                input.cursor_pos += 1;
            }
            ViewerAction::None
        }
        KeyCode::Char(ch) => {
            let mut candidate = match try_copy_input_value(
                &input.current_value,
                0,
                InputValueAllocationSite::Growth,
            ) {
                Some(candidate) => candidate,
                None => return (ViewerAction::None, None),
            };
            let additional = ch.len_utf8();
            let byte_offset = grapheme_byte_offset(&candidate, input.cursor_pos);
            if candidate
                .len()
                .checked_add(additional)
                .is_none_or(|len| len > MAX_INPUT_VALUE_BYTES)
                || candidate.try_reserve(additional).is_err()
            {
                return (ViewerAction::None, None);
            }
            candidate.insert(byte_offset, ch);
            let grapheme_count =
                unicode_segmentation::UnicodeSegmentation::graphemes(candidate.as_str(), true)
                    .count();
            if input.maxlen != 0 && grapheme_count > input.maxlen as usize {
                return (ViewerAction::None, None);
            }
            let inserted_end = byte_offset + additional;
            let cursor_pos = unicode_segmentation::UnicodeSegmentation::graphemes(
                &candidate[..inserted_end],
                true,
            )
            .count();
            let Some(projection) = try_copy_input_value(
                &candidate,
                input.maxlen,
                InputValueAllocationSite::Projection,
            ) else {
                return (ViewerAction::None, None);
            };
            input.current_value = candidate;
            input.cursor_pos = cursor_pos;
            *needs_redraw = true;
            return (ViewerAction::None, Some(projection));
        }
        _ => ViewerAction::None,
    };
    let projection = try_copy_input_value(
        &input.current_value,
        input.maxlen,
        InputValueAllocationSite::Projection,
    );
    (action, projection)
}

fn grapheme_byte_offset(value: &str, grapheme_index: usize) -> usize {
    use unicode_segmentation::UnicodeSegmentation;
    value
        .grapheme_indices(true)
        .nth(grapheme_index)
        .map(|(offset, _)| offset)
        .unwrap_or(value.len())
}

fn sync_input_value(
    scene: &mut Scene,
    node: Option<crate::compositor::scene::NodeId>,
    value: String,
) {
    if let Some(node) = node {
        PatchApplier::apply(scene, Patch::SetInputValue { node, value });
    }
}

fn reset_input_mode(input: &mut InputMode) {
    input.active = false;
    input.cursor_pos = 0;
    input.current_value.clear();
    input.current_node = None;
    input.maxlen = 0;
    input.password = false;
}

fn advance_select(scene: &mut Scene, node: crate::compositor::scene::NodeId) {
    let Some((current, option_count)) = scene.get(node).and_then(|select| {
        let NodeKind::Select(data) = select.kind() else {
            return None;
        };
        let count = select
            .children()
            .iter()
            .filter(|&&child| {
                scene
                    .get(child)
                    .is_some_and(|node| matches!(node.kind(), NodeKind::OptionLeaf(_)))
            })
            .count();
        Some((data.selected_index, count))
    }) else {
        return;
    };
    if option_count > 0 {
        PatchApplier::apply(
            scene,
            Patch::SetSelectIndex {
                node,
                index: (current + 1) % option_count,
            },
        );
    }
}

fn form_action(scene: &Scene, form: crate::compositor::scene::NodeId) -> Option<String> {
    let node = scene.get(form)?;
    let NodeKind::Flow(data) = node.kind() else {
        return None;
    };
    if !matches!(data.source, crate::compositor::scene::FlowSource::Form) {
        return None;
    }
    data.form_action.clone()
}

/// Read only the controls structurally owned by one form, in document order.
/// Keeping duplicate names is intentional and matches standard URL-encoded
/// form semantics.
fn collect_form_values(
    scene: &Scene,
    form: crate::compositor::scene::NodeId,
) -> Result<Vec<(String, String)>, std::collections::TryReserveError> {
    fn owned(value: &str) -> Result<String, std::collections::TryReserveError> {
        let mut result = String::new();
        result.try_reserve_exact(value.len())?;
        result.push_str(value);
        Ok(result)
    }

    let mut values = Vec::new();
    for node in scene.iter_subtree(form) {
        match node.kind() {
            NodeKind::Input(data) => {
                let name = owned(&data.name)?;
                let value = owned(data.value.as_deref().unwrap_or_default())?;
                values.try_reserve(1)?;
                values.push((name, value));
            }
            NodeKind::Select(data) => {
                let selected = node
                    .children()
                    .iter()
                    .filter_map(|&child| match scene.get(child)?.kind() {
                        NodeKind::OptionLeaf(option) => Some(option.value.as_str()),
                        _ => None,
                    })
                    .nth(data.selected_index)
                    .unwrap_or_default();
                let name = owned(&data.name)?;
                let selected = owned(selected)?;
                values.try_reserve(1)?;
                values.push((name, selected));
            }
            _ => {}
        }
    }
    Ok(values)
}

// ─── URL encoding ───────────────────────────────────────────

/// URL-encode form values into key=value&key2=value2 format.
fn url_encode_form(
    values: &[(String, String)],
) -> Result<String, std::collections::TryReserveError> {
    let mut encoded = String::new();
    for (index, (key, value)) in values.iter().enumerate() {
        let key = url_encode(key)?;
        let value = url_encode(value)?;
        let separator = usize::from(index != 0);
        encoded.try_reserve(separator + key.len() + 1 + value.len())?;
        if index != 0 {
            encoded.push('&');
        }
        encoded.push_str(&key);
        encoded.push('=');
        encoded.push_str(&value);
    }
    Ok(encoded)
}

/// Simple URL encoding.
fn url_encode(s: &str) -> Result<String, std::collections::TryReserveError> {
    let requested = s.len().saturating_mul(3);
    let mut out = String::new();
    out.try_reserve_exact(requested)?;
    for ch in s.chars() {
        match ch {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => out.push(ch),
            ' ' => out.push('+'),
            _ => {
                let mut buf = [0u8; 4];
                let bytes = ch.encode_utf8(&mut buf);
                for &b in bytes.as_bytes() {
                    const HEX: &[u8; 16] = b"0123456789ABCDEF";
                    // Both indices are nibbles, so always < 16; `get` states
                    // that rather than relying on the reader to derive it.
                    let hi = HEX.get(usize::from(b >> 4)).copied().unwrap_or(b'0');
                    let lo = HEX.get(usize::from(b & 0x0f)).copied().unwrap_or(b'0');
                    out.push('%');
                    out.push(char::from(hi));
                    out.push(char::from(lo));
                }
            }
        }
    }
    Ok(out)
}

// ─── Buffer utilities ───────────────────────────────────────

/// Extract a sub-region of a CellBuffer into a new buffer.
fn extract_sub_buffer(
    buf: &CellBuffer,
    x: u16,
    y: u16,
    w: u16,
    h: u16,
    governor: &ResourceGovernor,
) -> CellBuffer {
    let sub = match CellBuffer::try_new_governed(w, h, governor, ResourceCategory::CompositorCells)
    {
        Ok(sub) if !reject_runner_allocation(RunnerAllocationSite::SubBuffer) => sub,
        _ => {
            let mut fallback = CellBuffer::new(1, 1);
            fallback.record_allocation_failure();
            fallback
        }
    };
    fill_sub_buffer(buf, x, y, w, h, sub)
}

fn extract_sub_buffer_with_lease(
    buf: &CellBuffer,
    x: u16,
    y: u16,
    w: u16,
    h: u16,
    lease: BudgetLease,
) -> CellBuffer {
    let sub = match CellBuffer::try_new_with_lease(w, h, lease) {
        Ok(sub) if !reject_runner_allocation(RunnerAllocationSite::SubBuffer) => sub,
        _ => {
            let mut fallback = CellBuffer::new(1, 1);
            fallback.record_allocation_failure();
            fallback
        }
    };
    fill_sub_buffer(buf, x, y, w, h, sub)
}

fn fill_sub_buffer(
    buf: &CellBuffer,
    x: u16,
    y: u16,
    w: u16,
    h: u16,
    mut sub: CellBuffer,
) -> CellBuffer {
    if sub.allocation_failed() {
        return sub;
    }
    for dy in 0..h {
        for dx in 0..w {
            if let Some(cell) = buf.get(x.saturating_add(dx), y.saturating_add(dy)) {
                sub.set(dx, dy, cell.clone());
            }
        }
    }
    sub
}

/// Split a layout buffer into scrollable content and sticky-bottom content.
/// Remaps focusable positions so sticky elements have coordinates relative to the sticky buffer.
///
/// Only the last (lowest) sticky=bottom region is used. It must extend to the
/// end of the content — any content after the sticky region is included in the
/// sticky buffer, not the scrollable buffer.
fn split_sticky(
    buffer: CellBuffer,
    sticky_regions: &[crate::compositor::layout::engine::StickyRegion],
    focusables: &mut [crate::compositor::panels::FocusableElement],
    governor: Option<&ResourceGovernor>,
) -> (CellBuffer, Option<CellBuffer>) {
    // Find the lowest sticky=bottom region (closest to document end)
    let bottom_sticky = sticky_regions
        .iter()
        .filter(|s| s.position == crate::parser::ast::StickyPosition::Bottom)
        .max_by_key(|s| s.y);

    if let Some(sr) = bottom_sticky {
        // LayoutResult.buffer is dimensions-only. Split its dimensions here;
        // `refresh_sticky_buffer` extracts the actual pixels from the
        // composited scene immediately before presentation.
        let sticky_h = sr.h;
        if sticky_h == 0 {
            return (buffer, None);
        }
        let make_buffer = |width, height| {
            if reject_runner_allocation(RunnerAllocationSite::PageCanvas) {
                return Err(());
            }
            match governor {
                Some(governor) => CellBuffer::try_new_governed(
                    width,
                    height,
                    governor,
                    ResourceCategory::CompositorCells,
                )
                .map_err(|_| ()),
                None => CellBuffer::try_new(width, height).map_err(|_| ()),
            }
        };
        let sticky = make_buffer(buffer.width, sticky_h);
        let main = make_buffer(buffer.width, sr.y);
        let (Ok(mut sticky), Ok(mut main)) = (sticky, main) else {
            let mut buffer = buffer;
            buffer.record_allocation_failure();
            return (buffer, None);
        };
        // The split replaces the laid-out buffer with two fresh ones, so an
        // allocation failure recorded during layout has to be carried across
        // explicitly. Without this it is dropped on the floor for every page
        // with a bottom sticky region, and `layout_allocation_failed` reads
        // false for a page whose admission was refused.
        if buffer.allocation_failed() {
            main.record_allocation_failure();
            sticky.record_allocation_failure();
        }
        for f in focusables.iter_mut() {
            if f.row >= sr.y {
                f.row -= sr.y;
                f.is_sticky = true;
            }
        }
        (main, Some(sticky))
    } else {
        (buffer, None)
    }
}

/// Refresh the fixed-position sticky surface from the authoritative scene
/// composite. Sticky node placements remain in document coordinates; the
/// presentation layer extracts that tail and paints it at the viewport edge.
fn refresh_sticky_buffer(page: &mut LoadedPage, composited: &CellBuffer) {
    let Some(sticky) = page.sticky_buf.as_mut() else {
        return;
    };
    let sticky_start = page.buf.height;
    sticky.clear_transparent();
    for y in 0..sticky.height {
        for x in 0..sticky.width.min(composited.width) {
            if let Some(cell) = composited.get(x, sticky_start.saturating_add(y)) {
                sticky.set(x, y, cell.clone());
            }
        }
    }
}

// ─── Page transition helpers ────────────────────────────────

/// Capture the current composited viewport as a snapshot for page transitions.
///
/// Takes `page` so the scene walk has its authority; the `Compositor`
/// is now stateless beyond dimensions, so the snapshot must be
/// recomposited here rather than read from a cached layer stack.
struct PendingPageTransition {
    old_snapshot: CellBuffer,
}

fn capture_viewport_snapshot(
    page: &LoadedPage,
    compositor: &mut Compositor,
    state: &ViewportState,
    governor: &ResourceGovernor,
) -> Option<PendingPageTransition> {
    let viewport_cells =
        usize::from(state.term_w).checked_mul(usize::from(state.viewport_height()))?;
    let cell_bytes = std::mem::size_of::<Cell>();
    let viewport_bytes = viewport_cells.checked_mul(cell_bytes)?;
    // Capture owns only the source snapshot. Destination snapshot and overlay
    // storage are admitted by Start against the destination page's governor.
    let old_snapshot_lease = governor
        .reserve(ResourceCategory::CompositorCells, viewport_bytes)
        .ok()?;
    let document_height = page
        .buf
        .height
        .saturating_add(page.sticky_buf.as_ref().map_or(0, |buf| buf.height));
    compositor.resize(state.term_w, document_height.max(state.viewport_height()));
    let composited = compositor.composite_at(&page.scene, &page.anim_rt, state.scroll_offset)?;
    let mut buf = extract_sub_buffer_with_lease(
        &composited,
        0,
        state.scroll_offset,
        state.term_w,
        state.viewport_height(),
        old_snapshot_lease,
    );
    make_opaque(&mut buf);
    (!buf.allocation_failed()).then_some(PendingPageTransition { old_snapshot: buf })
}

/// Build a snapshot of the new page's first frame (base + initial animations).
fn build_new_page_snapshot(
    page: &LoadedPage,
    compositor: &mut Compositor,
    state: &ViewportState,
    lease: BudgetLease,
) -> Option<CellBuffer> {
    let document_height = page
        .buf
        .height
        .saturating_add(page.sticky_buf.as_ref().map_or(0, |buf| buf.height));
    compositor.resize(state.term_w, document_height.max(state.viewport_height()));
    // The compositor cache belongs to the previous Scene when navigation
    // replaces `LoadedPage`. Equal-sized pages must still be walked or the
    // transition receives two copies of the old snapshot and appears to be
    // an immediate cut.
    compositor.invalidate_cache();
    let composited = compositor.composite(&page.scene, &page.anim_rt)?;
    let mut buf = extract_sub_buffer_with_lease(
        &composited,
        0,
        0,
        state.term_w,
        state.viewport_height(),
        lease,
    );
    make_opaque(&mut buf);
    (!buf.allocation_failed()).then_some(buf)
}

fn try_install_page_transition(
    page: &mut LoadedPage,
    compositor: &mut Compositor,
    state: &ViewportState,
    pending: &mut Option<PendingPageTransition>,
    kind: ast::TransitionKind,
    duration_ms: u32,
) -> bool {
    if pending.is_none() || !page.anim_rt.can_push_page_transition() {
        return false;
    }
    let viewport_cells =
        match usize::from(state.term_w).checked_mul(usize::from(state.viewport_height())) {
            Some(cells) => cells,
            None => return false,
        };
    let viewport_bytes = match viewport_cells.checked_mul(std::mem::size_of::<Cell>()) {
        Some(bytes) => bytes,
        None => return false,
    };
    let overlay_w = state.term_w.max(1);
    let overlay_h = state.viewport_height().max(1);
    let overlay_cells = match usize::from(overlay_w).checked_mul(usize::from(overlay_h)) {
        Some(cells) => cells,
        None => return false,
    };
    let overlay_bytes = match overlay_cells.checked_mul(std::mem::size_of::<Cell>()) {
        Some(bytes) => bytes,
        None => return false,
    };
    let new_snapshot_lease = match page
        .governor
        .reserve(ResourceCategory::CompositorCells, viewport_bytes)
    {
        Ok(lease) => lease,
        Err(_) => return false,
    };
    let overlay_lease = match page.governor.reserve_with_cost(
        ResourceCategory::SceneCells,
        overlay_cells,
        overlay_bytes,
    ) {
        Ok(lease) => lease,
        Err(_) => return false,
    };
    let Some(new_snapshot) = build_new_page_snapshot(page, compositor, state, new_snapshot_lease)
    else {
        return false;
    };
    if page.scene.buffer_cell_count().saturating_add(overlay_cells)
        > crate::compositor::scene::tree::MAX_SCENE_CELLS
        || !page
            .scene
            .can_activate_page_transition_overlay(overlay_cells)
    {
        return false;
    }
    let Ok(overlay_buffer) =
        CellBuffer::try_new_opaque_with_lease(overlay_w, overlay_h, overlay_lease)
    else {
        return false;
    };
    let Some(overlay_id) = page.scene.page_transition_overlay_slot() else {
        return false;
    };
    let Some(old_snapshot) = pending.take().map(|pending| pending.old_snapshot) else {
        return false;
    };
    let adapter = Box::new(PageTransitionAdapter::new(
        overlay_id,
        old_snapshot,
        new_snapshot,
        kind,
        duration_ms,
    ));
    let activated = page.scene.activate_page_transition_overlay(
        Rect::new(0, 0, state.term_w, state.viewport_height()),
        overlay_buffer,
    );
    debug_assert_eq!(activated, Some(overlay_id));
    let installed = page.anim_rt.try_push_page_transition(adapter);
    debug_assert!(installed, "page-transition slot was preflighted");
    installed
}

/// Fill any transparent cells in a buffer with opaque black space.
///
/// Page transition snapshots must be fully opaque so that slide/fade effects
/// don't leave ghost artifacts from the previous terminal state bleeding through.
fn make_opaque(buf: &mut CellBuffer) {
    let opaque_cell = crate::compositor::layout::cell::Cell {
        ch: ' ',
        grapheme: None,
        style: crate::compositor::layout::cell::CellStyle {
            // Literal black: SGR 40 is a theme-controlled palette slot, so
            // naming it would tint the snapshot on themed terminals.
            bg: Some(crate::color::ResolvedColor::Rgb(0, 0, 0)),
            ..Default::default()
        },
    };
    for y in 0..buf.height {
        for x in 0..buf.width {
            if let Some(cell) = buf.get(x, y)
                && cell.is_transparent()
            {
                buf.set(x, y, opaque_cell.clone());
            }
        }
    }
}

/// Resolve which transition to use: link override > page default > None.
fn resolve_transition(
    link_transition: Option<ast::TransitionKind>,
    link_duration: u32,
    page_transition: Option<ast::TransitionKind>,
    page_duration: u32,
) -> (Option<ast::TransitionKind>, u32) {
    if let Some(kind) = link_transition {
        (Some(kind), link_duration)
    } else if let Some(kind) = page_transition {
        (Some(kind), page_duration)
    } else {
        (None, 0)
    }
}

/// Reverse a transition direction for back-navigation.
/// Slide-left becomes slide-right, slide-up becomes slide-down, etc.
/// Non-directional transitions (fade, dissolve) stay the same.
fn reverse_transition(kind: ast::TransitionKind) -> ast::TransitionKind {
    match kind {
        ast::TransitionKind::SlideLeft => ast::TransitionKind::SlideRight,
        ast::TransitionKind::SlideRight => ast::TransitionKind::SlideLeft,
        ast::TransitionKind::SlideUp => ast::TransitionKind::SlideDown,
        ast::TransitionKind::SlideDown => ast::TransitionKind::SlideUp,
        other => other,
    }
}

// ─── Frame drawing and status bar ───────────────────────────

/// Clear the physical terminal and forget the compositor's last emitted
/// frame so the next presentation repaints every cell from a clean slate.
fn clear_terminal_for_full_redraw(
    out: &mut impl Write,
    compositor: &mut Compositor,
) -> io::Result<()> {
    write!(
        out,
        "{}",
        crate::compositor::present::ansi::TERMINAL_DEFAULT_SGR
    )?;
    out.execute(terminal::Clear(ClearType::All))?;
    out.execute(cursor::MoveTo(0, 0))?;
    out.flush()?;
    invalidate_compositor_for_new_scene(compositor);
    Ok(())
}

/// A `LoadedPage` replacement changes the scene identity even when its
/// dimensions match the previous page. Drop both caches so the first frame
/// comes entirely from the new scene and is emitted as a full repaint.
fn invalidate_compositor_for_new_scene(compositor: &mut Compositor) {
    compositor.invalidate_cache();
    compositor.invalidate_presented();
}

/// Store one adapter failure and open the Errors tab only for the first
/// failure of this client session. Returns whether the HUD was opened.
fn record_runtime_notice(
    error_log: &mut ErrorLog,
    client_hud: &mut ClientHud,
    notice: &str,
) -> bool {
    let (selected, first) = error_log.record(notice);
    if let Some(selected) = selected {
        client_hud.error_selected = selected;
    }
    if first {
        client_hud.open_errors(selected);
    }
    first
}

fn write_repeated(out: &mut impl Write, value: &str, count: usize) -> io::Result<()> {
    for _ in 0..count {
        out.write_all(value.as_bytes())?;
    }
    Ok(())
}

const HELP_LINES: &[&str] = &[
    "Navigate sites with the keyboard; commands open on the bottom row.",
    "PAGE",
    "  Tab / Shift-Tab       Focus next / previous control",
    "  Enter                 Activate a link, button, or field",
    "  j / k, Up / Down      Scroll one line",
    "  Space / PageDown, PageUp   Scroll by page",
    "  g / Home, G / End     Jump to top / bottom",
    "  h / Left, l / Right   Go back / forward",
    "  `                     Open History / Errors HUD",
    "  f                     Finish foreground animations",
    "",
    "FORMS",
    "  Enter edits a field; Tab leaves it and moves focus.",
    "  Esc stops editing. Ctrl-C exits immediately.",
    "",
    "COMMANDS",
    "  o / :open <atp://...> Open an ATP address",
    "  r / :reload           Reload the current page",
    "  :sessions             Show active login sessions",
    "  :sessions clear [site] Clear saved sessions",
    "  ? / :h / :help        Show this window",
    "  :quit                 Exit the client",
];

/// Paint client help directly over the current viewport. This is deliberately
/// outside the site scene: remote content cannot obscure or redefine it.
/// Paint a centred, bordered modal over the viewport.
///
/// Shared by the help overlay and the trust prompt. Both paint straight to the
/// terminal rather than into the composited page buffer, which is why callers
/// invalidate the presentation to force a repaint when the modal closes.
fn write_modal(
    out: &mut impl Write,
    state: &ViewportState,
    title: &str,
    short_title: &str,
    lines: &[&str],
    footer: &str,
    short_footer: &str,
) -> io::Result<()> {
    let viewport_h = state.viewport_height();
    if state.term_w < 4 || viewport_h < 3 {
        return Ok(());
    }

    let width = state.term_w.saturating_sub(2).clamp(4, 72);
    let height = viewport_h.min((lines.len() as u16).saturating_add(2));
    let left = (state.term_w.saturating_sub(width)) / 2;
    let top = (viewport_h.saturating_sub(height)) / 2;
    let inner_w = width.saturating_sub(2) as usize;
    let content_rows = height.saturating_sub(2) as usize;

    let title = if inner_w >= title.chars().count() {
        title
    } else {
        short_title
    };
    let title_width = title.chars().count().min(inner_w);
    let remaining = inner_w.saturating_sub(title_width);
    let title_left = remaining / 2;
    let title_right = remaining - title_left;
    write!(out, "\x1b[{};{}H\x1b[0;1;37;40m┌", top + 1, left + 1)?;
    write_repeated(out, "─", title_left)?;
    for ch in title.chars().take(title_width) {
        write!(out, "{ch}")?;
    }
    write_repeated(out, "─", title_right)?;
    write!(out, "┐\x1b[0m")?;

    for row in 0..content_rows {
        let line = lines.get(row).copied().unwrap_or("");
        let display = line
            .get(
                ..line
                    .char_indices()
                    .nth(inner_w.saturating_sub(1))
                    .map_or(line.len(), |(index, _)| index),
            )
            .unwrap_or(line);
        let padding = inner_w
            .saturating_sub(1)
            .saturating_sub(display.chars().count());
        write!(
            out,
            "\x1b[{};{}H\x1b[0;1;37;40m│\x1b[0;37;40m {display}",
            top + 2 + row as u16,
            left + 1
        )?;
        write_repeated(out, " ", padding)?;
        write!(out, "\x1b[0;1;37;40m│\x1b[0m")?;
    }

    let footer = if inner_w >= footer.chars().count() {
        footer
    } else {
        short_footer
    };
    let footer_width = footer.chars().count().min(inner_w);
    let remaining = inner_w.saturating_sub(footer_width);
    let footer_left = remaining / 2;
    let footer_right = remaining - footer_left;
    write!(out, "\x1b[{};{}H\x1b[0;1;37;40m└", top + height, left + 1)?;
    write_repeated(out, "─", footer_left)?;
    for ch in footer.chars().take(footer_width) {
        write!(out, "{ch}")?;
    }
    write_repeated(out, "─", footer_right)?;
    write!(out, "┘\x1b[0m")?;

    Ok(())
}

/// Ask whether to trust a site no authority vouches for.
///
/// Painted and answered inside the effect that provoked it, so the decision is
/// genuinely modal: nothing else advances until the user answers, which is the
/// point. The fingerprint is shown in full rather than abbreviated — the only
/// use for it is comparing against something the operator published, and a
/// truncated digest cannot be compared.
///
/// Returns whether the user chose to trust the site.
fn prompt_for_trust(
    state: &ViewportState,
    host: &str,
    port: u16,
    fingerprint: &crate::trust::Fingerprint,
    reason: &str,
) -> io::Result<bool> {
    let site = try_format(format_args!("  {host}:{port}"))?;
    let why = try_format(format_args!("  reason: {reason}"))?;
    let digest = try_format(format_args!("  {fingerprint}"))?;
    let lines: Vec<&str> = vec![
        "",
        "This site's certificate could not be verified.",
        &site,
        &why,
        "",
        "  sha256 fingerprint:",
        &digest,
        "",
        "Trusting it pins this exact certificate. Any",
        "later change will be refused, not re-asked.",
        "",
        "Only trust it if this fingerprint matches one",
        "the site's operator published.",
        "",
    ];

    let mut out = io::stdout();
    write_modal(
        &mut out,
        state,
        " UNKNOWN SITE ",
        " UNKNOWN ",
        &lines,
        " [t] trust and pin   [n] cancel ",
        " t trust  n cancel ",
    )?;
    out.flush()?;

    loop {
        match event::read()? {
            Event::Key(KeyEvent {
                code: KeyCode::Char('c'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }) => return Ok(false),
            Event::Key(key) => match key.code {
                // Deliberately not Enter or space: trusting a site is a
                // decision, and the keys that dismiss every other modal in
                // this client must not make it by accident.
                KeyCode::Char('t') | KeyCode::Char('T') => return Ok(true),
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Char('q') | KeyCode::Esc => {
                    return Ok(false);
                }
                _ => {}
            },
            _ => {}
        }
    }
}

fn write_help_modal(out: &mut impl Write, state: &ViewportState) -> io::Result<()> {
    write_modal(
        out,
        state,
        " DUSTNET CLIENT HELP ",
        " HELP ",
        HELP_LINES,
        " Esc, Enter, or q to close ",
        " Esc/q close ",
    )
}

/// Paint the tabbed client HUD directly over the top of the viewport. The
/// number of rows comes from the animation progress, producing a clipped
/// console that appears to descend from the terminal's top edge.
fn write_client_hud(
    out: &mut impl Write,
    state: &ViewportState,
    hud: &ClientHud,
    error_log: &ErrorLog,
    history: &[HistoryEntry],
    logical_history: &[crate::viewer::HistoryEntry],
    history_idx: usize,
) -> io::Result<()> {
    let height = hud.visible_rows(state.viewport_height());
    let width = state.term_w;
    if width < 4 || height == 0 {
        return Ok(());
    }

    let inner_w = width.saturating_sub(2) as usize;
    let title = match hud.tab {
        ClientHudTab::History => try_format(format_args!(
            " DUSTNET HUD  [HISTORY]  ERRORS ({}) ",
            error_log.total_count()
        ))?,
        ClientHudTab::Errors => try_format(format_args!(
            " DUSTNET HUD   HISTORY  [ERRORS ({})] ",
            error_log.total_count()
        ))?,
    };
    let (title, title_width) = display_width_prefix(&title, inner_w);
    let remaining = inner_w.saturating_sub(title_width);
    let title_left = remaining / 2;
    let title_right = remaining - title_left;
    write!(out, "\x1b[1;1H\x1b[0;1;37;40m╔")?;
    write_repeated(out, "═", title_left)?;
    write!(out, "{title}")?;
    write_repeated(out, "═", title_right)?;
    write!(out, "╗\x1b[0m")?;

    if height == 1 {
        return Ok(());
    }

    let list_rows = height.saturating_sub(2) as usize;
    let (selected, item_count) = match hud.tab {
        ClientHudTab::History => (
            hud.history_selected
                .min(logical_history.len().saturating_sub(1)),
            logical_history.len(),
        ),
        ClientHudTab::Errors => (
            hud.error_selected
                .min(error_log.entries.len().saturating_sub(1)),
            error_log.entries.len(),
        ),
    };
    let start = selected
        .saturating_add(1)
        .saturating_sub(list_rows)
        .min(item_count.saturating_sub(list_rows));

    for row in 0..list_rows {
        let item_pos = start + row;
        let (content, selected_row, error_row) = match hud.tab {
            ClientHudTab::History => {
                if let Some(logical) = logical_history.get(item_pos) {
                    let artifact = history.iter().find(|artifact| artifact.id == logical.id);
                    let cursor = if item_pos == selected { ">" } else { " " };
                    let current = if item_pos == history_idx { "◆" } else { " " };
                    let titled = artifact
                        .map(|entry| &entry.title)
                        .filter(|title| !title.is_empty());
                    let label = match titled {
                        Some(title) => try_format(format_args!("{}  ·  {}", title, logical.uri))?,
                        None => try_format(format_args!("{}", logical.uri))?,
                    };
                    (
                        try_format(format_args!(
                            " {cursor}{current} {:02}  {label}",
                            item_pos + 1
                        ))?,
                        item_pos == selected,
                        false,
                    )
                } else if logical_history.is_empty() && row == 0 {
                    (try_copy("  No connected-page history yet.")?, false, false)
                } else {
                    (String::new(), false, false)
                }
            }
            ClientHudTab::Errors => {
                if let Some(entry) = error_log.entries.get(item_pos) {
                    let cursor = if item_pos == selected { ">" } else { " " };
                    (
                        try_format(format_args!(
                            " {cursor} ×{}  {}",
                            entry.count, entry.message
                        ))?,
                        item_pos == selected,
                        true,
                    )
                } else if error_log.entries.is_empty() && row == 0 {
                    (try_copy("  No runtime errors this session.")?, false, false)
                } else {
                    (String::new(), false, false)
                }
            }
        };

        let (display, display_width) = display_width_prefix(&content, inner_w);
        let padding = inner_w.saturating_sub(display_width);
        let content_sgr = if selected_row {
            if error_row {
                "\x1b[0;1;37;41m"
            } else {
                "\x1b[0;1;37;100m"
            }
        } else if error_row {
            "\x1b[0;31;40m"
        } else {
            "\x1b[0;37;40m"
        };
        write!(
            out,
            "\x1b[{};1H\x1b[0;1;37;40m║{}{display}",
            row + 2,
            content_sgr
        )?;
        write_repeated(out, " ", padding)?;
        write!(out, "\x1b[0;1;37;40m║\x1b[0m")?;
    }

    let footer = match (hud.tab, inner_w >= 62) {
        (ClientHudTab::History, true) => {
            " ` / Esc close  ·  Tab / ←→ switch  ·  ↑↓ select  ·  Enter open "
        }
        (ClientHudTab::History, false) => " Tab switch · ↑↓ select · Enter open · ` close ",
        (ClientHudTab::Errors, true) => {
            " ` / Esc close  ·  Tab / ←→ switch  ·  ↑↓ select  ·  c clear "
        }
        (ClientHudTab::Errors, false) => " Tab switch · ↑↓ select · c clear · ` close ",
    };
    let omitted = error_log.omitted_count();
    let footer = if hud.tab == ClientHudTab::Errors && omitted > 0 {
        try_format(format_args!(
            " {omitted} additional errors omitted · c clear · ` close "
        ))?
    } else {
        try_copy(footer)?
    };
    let (footer, footer_width) = display_width_prefix(&footer, inner_w);
    let remaining = inner_w.saturating_sub(footer_width);
    let footer_left = remaining / 2;
    let footer_right = remaining - footer_left;
    write!(out, "\x1b[{};1H\x1b[0;1;37;40m╚", height)?;
    write_repeated(out, "═", footer_left)?;
    write!(out, "{footer}")?;
    write_repeated(out, "═", footer_right)?;
    write!(out, "╝\x1b[0m")?;

    Ok(())
}

/// Draw one viewer frame. HUD frames are staged before they reach the terminal
/// and wrapped in DEC synchronized-update markers. Without that transaction a
/// terminal may display the page diff before the stdout-only HUD overlay that
/// follows it, briefly exposing animated background cells through the panel.
#[allow(clippy::too_many_arguments)]
fn draw_viewer_frame(
    out: &mut impl Write,
    compositor: &mut Compositor,
    buf: &SharedFrame,
    state: &ViewportState,
    focusables: &[crate::compositor::panels::FocusableElement],
    focus_idx: Option<usize>,
    uri: &str,
    security: Option<dustnet_core::protocol::origin::TransportSecurity>,
    config: &ClientConfig,
    color_support: ColorSupport,
    title: &str,
    connected: bool,
    input_mode: &InputMode,
    cmd_line: &CommandLine,
    sticky_buf: &Option<CellBuffer>,
    help_visible: bool,
    client_hud: &ClientHud,
    error_log: &ErrorLog,
    history: &[HistoryEntry],
    logical_history: &[crate::viewer::HistoryEntry],
    history_idx: usize,
    wasm_mem_bytes: usize,
) -> io::Result<()> {
    if !client_hud.is_active() {
        return draw_viewer_frame_inner(
            out,
            compositor,
            buf,
            state,
            focusables,
            focus_idx,
            uri,
            security,
            config,
            color_support,
            title,
            connected,
            input_mode,
            cmd_line,
            sticky_buf,
            help_visible,
            client_hud,
            error_log,
            history,
            logical_history,
            history_idx,
            wasm_mem_bytes,
        );
    }

    // Keep the complete synchronized frame local until it is ready, but make
    // every growth attempt fallible. A failed formatting write discards this
    // unpublished candidate and leaves the terminal on its previous frame.
    let presentation = compositor.presentation_checkpoint();
    let mut frame = FallibleFrame::default();
    let result = draw_viewer_frame_inner(
        &mut frame,
        compositor,
        buf,
        state,
        focusables,
        focus_idx,
        uri,
        security,
        config,
        color_support,
        title,
        connected,
        input_mode,
        cmd_line,
        sticky_buf,
        help_visible,
        client_hud,
        error_log,
        history,
        logical_history,
        history_idx,
        wasm_mem_bytes,
    )
    .and_then(|()| write_synchronized_update(out, frame.as_slice()));
    if result.is_err() {
        compositor.restore_presentation(presentation);
    }
    result
}

#[derive(Default)]
struct FallibleFrame(Vec<u8>);

#[cfg(test)]
thread_local! {
    static REJECT_TERMINAL_FRAME_ALLOCATION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn reject_next_terminal_frame_allocation() {
    REJECT_TERMINAL_FRAME_ALLOCATION.with(|reject| reject.set(true));
}

impl FallibleFrame {
    fn as_slice(&self) -> &[u8] {
        &self.0
    }
}

impl Write for FallibleFrame {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        #[cfg(test)]
        if REJECT_TERMINAL_FRAME_ALLOCATION.with(|reject| reject.replace(false)) {
            return Err(io::Error::other("terminal frame allocation rejected"));
        }
        self.0
            .try_reserve(bytes.len())
            .map_err(|_| io::Error::other("terminal frame allocation failed"))?;
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn write_synchronized_update(out: &mut impl Write, frame: &[u8]) -> io::Result<()> {
    // Unsupported terminals harmlessly ignore these private-mode sequences;
    // supporting terminals defer painting until the complete HUD frame has
    // arrived, avoiding a visible page/HUD intermediate state.
    out.write_all(b"\x1b[?2026h")?;
    out.write_all(frame)?;
    out.write_all(b"\x1b[?2026l")?;
    out.flush()
}

#[allow(clippy::too_many_arguments)]
fn draw_viewer_frame_inner(
    out: &mut impl Write,
    compositor: &mut Compositor,
    buf: &SharedFrame,
    state: &ViewportState,
    focusables: &[crate::compositor::panels::FocusableElement],
    focus_idx: Option<usize>,
    uri: &str,
    security: Option<dustnet_core::protocol::origin::TransportSecurity>,
    config: &ClientConfig,
    color_support: ColorSupport,
    title: &str,
    connected: bool,
    input_mode: &InputMode,
    cmd_line: &CommandLine,
    sticky_buf: &Option<CellBuffer>,
    help_visible: bool,
    client_hud: &ClientHud,
    error_log: &ErrorLog,
    history: &[HistoryEntry],
    logical_history: &[crate::viewer::HistoryEntry],
    history_idx: usize,
    wasm_mem_bytes: usize,
) -> io::Result<()> {
    // Render scrollable content. Phase 4 of
    // the composite-unification migration: `present_main` diffs
    // against the previously-presented frame when safe, emitting
    // only changed cells. Falls back to a full emit on dimension
    // mismatch, offset change, or first frame.
    compositor.present_main(out, buf, state.scroll_offset, state.scroll_height())?;

    // Render sticky content at viewport bottom
    if let Some(sb) = sticky_buf {
        crate::compositor::present::render_at_offset(out, sb, state.scroll_height())?;
    }

    // Compute the span of cells the focus highlight *will* occupy
    // this frame (if any). The highlight is a stdout-only overlay
    // that `present_main`'s cell-buffer diff cannot see, so we also
    // need to actively wipe the previous span before painting — see
    // the wipe block just below.
    let new_focus_span: Option<crate::compositor::composite::FocusSpan> = focus_idx
        .and_then(|idx| focusables.get(idx))
        .and_then(|focusable| {
            let screen_row = if focusable.is_sticky {
                (state.scroll_height() + focusable.row) as i32
            } else {
                focusable.row as i32 - state.scroll_offset as i32
            };
            if screen_row < 0 || screen_row >= state.viewport_height() as i32 {
                return None;
            }
            let cell_buf: &CellBuffer = if focusable.is_sticky {
                sticky_buf.as_ref().unwrap_or(buf)
            } else {
                buf
            };
            let buf_row = focusable.row;
            let mut end = focusable.col.saturating_add(focusable.width);
            while end > focusable.col {
                if let Some(cell) = cell_buf.get(end - 1, buf_row)
                    && (cell.ch == ' ' || cell.ch == '\0')
                {
                    end -= 1;
                    continue;
                }
                break;
            }
            if end <= focusable.col {
                return None;
            }
            Some(crate::compositor::composite::FocusSpan {
                screen_row: screen_row as u16,
                buf_row,
                col_start: focusable.col,
                col_end: end,
                is_sticky: focusable.is_sticky,
            })
        });

    // Wipe the previous highlight if the span moved: repaint those
    // cells from the current buffer with their plain styles. This
    // overwrites the stale `\x1b[7m` reverse-video that
    // `render_diff` skipped (the buffer cells themselves didn't
    // change, so diff emitted nothing).
    let prev_focus_span = compositor.last_focus_span();
    if prev_focus_span != new_focus_span
        && let Some(prev) = prev_focus_span
    {
        let prev_buf: Option<&CellBuffer> = if prev.is_sticky {
            sticky_buf.as_ref()
        } else {
            Some(buf.as_ref())
        };
        if let Some(pbuf) = prev_buf {
            for col in prev.col_start..prev.col_end {
                if let Some(cell) = pbuf.get(col, prev.buf_row) {
                    write!(out, "\x1b[{};{}H\x1b[0m", prev.screen_row + 1, col + 1)?;
                    crate::compositor::present::ansi::write_style_sgr(out, &cell.style)?;
                    if cell.ch == '\0' {
                        write!(out, " ")?;
                    } else {
                        cell.write_glyph(out)?;
                    }
                    write!(out, "\x1b[0m")?;
                }
            }
        }
    }

    // Focus indicator: highlight the focused element's text with reverse video
    if let Some(idx) = focus_idx
        && let Some(focusable) = focusables.get(idx)
    {
        // Sticky elements: screen row relative to sticky area at viewport bottom.
        // Normal elements: screen row relative to scroll offset.
        let screen_row = if focusable.is_sticky {
            (state.scroll_height() + focusable.row) as i32
        } else {
            focusable.row as i32 - state.scroll_offset as i32
        };
        if screen_row >= 0 && screen_row < state.viewport_height() as i32 {
            // Read cell data from the appropriate buffer
            let cell_buf: &CellBuffer = if focusable.is_sticky {
                sticky_buf.as_ref().unwrap_or(buf)
            } else {
                buf
            };
            let buf_row = focusable.row;
            // Trim trailing whitespace so highlights don't bleed into padding
            let mut end = focusable.col.saturating_add(focusable.width);
            while end > focusable.col {
                if let Some(cell) = cell_buf.get(end - 1, buf_row)
                    && (cell.ch == ' ' || cell.ch == '\0')
                {
                    end -= 1;
                    continue;
                }
                break;
            }

            // Use underline for the actively edited input field, reverse for everything else
            let is_active_input = input_mode.active
                && focusable.col == input_mode.field_col
                && focusable.row == input_mode.field_row;
            let highlight = if is_active_input {
                "\x1b[4m"
            } else {
                "\x1b[7m"
            };

            // Find the dominant foreground color from the first non-space character,
            // with dim forced off so the highlight is always bright/readable
            let dominant_style = {
                let mut dominant = None;
                for col in focusable.col..end {
                    if let Some(cell) = cell_buf.get(col, buf_row)
                        && cell.ch != '\0'
                        && cell.ch != ' '
                        && cell.ch != '_'
                    {
                        let mut style = cell.style.clone();
                        style.dim = false;
                        dominant = Some(style);
                        break;
                    }
                }
                dominant
            };

            for col in focusable.col..end {
                if let Some(cell) = cell_buf.get(col, buf_row) {
                    write!(out, "\x1b[{};{}H", screen_row + 1, col + 1)?;
                    if let Some(style) = &dominant_style {
                        crate::compositor::present::ansi::write_style_sgr(out, style)?;
                    }
                    write!(out, "{highlight}")?;
                    if cell.ch == '\0' {
                        write!(out, " ")?;
                    } else {
                        cell.write_glyph(out)?;
                    }
                    write!(out, "\x1b[0m")?;
                }
            }

            // Position the terminal cursor at the edit point when in input mode
            if is_active_input {
                let value = input_mode.current_value.as_str();
                let byte_offset = grapheme_byte_offset(value, input_mode.cursor_pos);
                let cursor_width = if input_mode.password {
                    input_mode.cursor_pos as u16
                } else {
                    crate::compositor::layout::text::display_width(
                        &value[..byte_offset],
                        input_mode.wcfg,
                    ) as u16
                };
                let cursor_col = input_mode.field_col + 1 + cursor_width;
                write!(out, "\x1b[{};{}H", screen_row + 1, cursor_col + 1,)?;
            }
        }
    }

    // Remember where we painted the highlight so the *next* frame
    // can wipe it if focus moves.
    compositor.set_last_focus_span(new_focus_span);

    if client_hud.is_active() {
        write_client_hud(
            out,
            state,
            client_hud,
            error_log,
            history,
            logical_history,
            history_idx,
        )?;
    }

    if help_visible {
        write_help_modal(out, state)?;
    }

    // Show/hide terminal cursor based on input mode or command line mode
    if !client_hud.is_active() && (input_mode.active || cmd_line.mode == CommandLineMode::Input) {
        write!(out, "\x1b[?25h")?; // show cursor
    } else {
        write!(out, "\x1b[?25l")?; // hide cursor
    }

    let error_count = error_log.total_count();
    let help = if error_count == 0 {
        try_copy(":h help  :q quit")?
    } else if error_count == 1 {
        try_copy("! 1 error  ·  ` HUD  ·  :h help  :q quit")?
    } else {
        try_format(format_args!(
            "! {error_count} errors  ·  ` HUD  ·  :h help  :q quit"
        ))?
    };
    let proposed_href = focus_idx
        .and_then(|idx| focusables.get(idx))
        .and_then(|focusable| match &focusable.action {
            crate::compositor::panels::FocusAction::Navigate { href, .. } => Some(href.as_str()),
            _ => None,
        });
    let proposed_address = proposed_href
        .map(|href| resolve_proposed_address(uri, href))
        .transpose()?;
    let vars = build_status_vars(
        state,
        focusables.len(),
        focus_idx,
        proposed_address.as_deref(),
        uri,
        security,
        title,
        &help,
        wasm_mem_bytes,
    )?;
    let format = if connected {
        &config.status_bar.connected_format
    } else {
        &config.status_bar.local_format
    };
    let status = config::try_expand_format_width(format, &vars, state.term_w)
        .map_err(|_| io::Error::other("status bar allocation failed"))?;
    write_status_line(out, state, &status, &config.status_bar, color_support)?;
    write_command_line(out, state, cmd_line)?;
    out.flush()
}

// ─── Layout helpers ─────────────────────────────────────────

struct LayoutOutput {
    buffer: CellBuffer,
    /// Every placed element (panels, animations, live regions), in document
    /// order. Consumers filter by `PlacedKind`.
    placed: Vec<PlacedElement>,
    sticky_regions: Vec<crate::compositor::layout::engine::StickyRegion>,
    projection_lease: Option<BudgetLease>,
}

type ProjectionCollections = (
    Vec<crate::compositor::panels::FocusableElement>,
    Vec<PlacedElement>,
    Vec<crate::compositor::layout::engine::StickyRegion>,
    Option<BudgetLease>,
);

fn reserve_projection_collections(
    scene: &Scene,
    governor: Option<&ResourceGovernor>,
) -> Option<ProjectionCollections> {
    let capacity = scene.iter_tree_order().count();
    let item_bytes = std::mem::size_of::<crate::compositor::panels::FocusableElement>()
        .checked_add(std::mem::size_of::<PlacedElement>())?
        .checked_add(std::mem::size_of::<
            crate::compositor::layout::engine::StickyRegion,
        >())?;
    let placed_string_bound = scene.placed_storage_requirements(true)?.1;
    let focusable_payload_bound =
        crate::compositor::panels::focusable_storage_requirements(scene)?.1;
    let admitted_bytes = capacity
        .checked_mul(item_bytes)?
        .checked_add(placed_string_bound)?
        .checked_add(focusable_payload_bound)?;
    let lease = match (governor, admitted_bytes) {
        (_, 0) | (None, _) => None,
        (Some(governor), bytes) => Some(
            governor
                .reserve(ResourceCategory::RemoteCollections, bytes)
                .ok()?,
        ),
    };
    let mut focusables = Vec::new();
    focusables.try_reserve_exact(capacity).ok()?;
    let mut placed = Vec::new();
    placed.try_reserve_exact(capacity).ok()?;
    let mut sticky = Vec::new();
    sticky.try_reserve_exact(capacity).ok()?;
    Some((focusables, placed, sticky, lease))
}

fn reconcile_projection_lease(
    focusables: &[crate::compositor::panels::FocusableElement],
    focusable_capacity: usize,
    placed: &[PlacedElement],
    placed_capacity: usize,
    sticky_capacity: usize,
    lease: &mut Option<BudgetLease>,
) -> Option<()> {
    let retained_bytes = focusable_capacity
        .checked_mul(std::mem::size_of::<
            crate::compositor::panels::FocusableElement,
        >())?
        .checked_add(placed_capacity.checked_mul(std::mem::size_of::<PlacedElement>())?)?
        .checked_add(sticky_capacity.checked_mul(std::mem::size_of::<
            crate::compositor::layout::engine::StickyRegion,
        >())?)?
        .checked_add(placed.iter().try_fold(0usize, |total, placed| {
            total.checked_add(placed.retained_string_capacity())
        })?)?
        .checked_add(focusables.iter().try_fold(0usize, |total, focusable| {
            total.checked_add(focusable.retained_payload_capacity()?)
        })?)?;
    if let Some(lease) = lease.as_mut() {
        lease
            .try_resize_with_cost(retained_bytes, retained_bytes)
            .ok()?;
    }
    Some(())
}

fn populate_projection_collections(
    scene: &Scene,
    focusables: &mut Vec<crate::compositor::panels::FocusableElement>,
    placed: &mut Vec<PlacedElement>,
    sticky: &mut Vec<crate::compositor::layout::engine::StickyRegion>,
) {
    crate::compositor::panels::collect_focusables_from_scene_into(scene, focusables);
    placed.clear();
    placed.extend(scene.iter_placed());
    sticky.clear();
    sticky.extend(scene.iter_sticky());
}

/// Stage 5 (per-tick) — consume `scene.invalidation.layout`.
///
/// Drains the invalidation set by re-laying each flagged subtree **in
/// place** (at its current `placement.rect` origin), writing
/// screen-absolute placements back to the scene and blitting new
/// cells into `page_buf`.
///
/// Dispatches per-`NodeKind` implicitly through `layout_node`: a
/// Panel re-walks its active state, a Flow container re-flows its
/// children, a Text leaf re-wraps its runs, and so on. Every kind
/// the layout dispatcher knows how to handle is scopable here.
///
/// **Size-change cascade.** After each in-place relayout, if the
/// node's new rect differs from its old rect, the parent is added to
/// the queue. This captures the case where a child's height change
/// shifts its siblings and requires re-laying the parent. The queue
/// is processed until it drains (each NodeId processed at most once).
///
/// Invariant: after stage 5 runs, `scene.invalidation` is empty.
#[cfg(test)]
fn layout_pass_invalidated(
    scene: &mut crate::compositor::scene::Scene,
    page_buf: &mut CellBuffer,
    color_support: ColorSupport,
    wcfg: WidthConfig,
) -> bool {
    layout_pass_invalidated_inner(scene, page_buf, color_support, wcfg, None)
}

fn layout_pass_invalidated_governed(
    scene: &mut crate::compositor::scene::Scene,
    page_buf: &mut CellBuffer,
    color_support: ColorSupport,
    wcfg: WidthConfig,
    governor: &ResourceGovernor,
) -> bool {
    layout_pass_invalidated_inner(scene, page_buf, color_support, wcfg, Some(governor))
}

/// Retryable stage-5 drain. Patches have already changed the authoritative
/// scene, so this transaction protects only the derived layout and rendered
/// projection. In particular, it does not claim to restore structural patch
/// topology or `NodeId` allocation.
fn drain_invalidated_layout_transactionally(
    page: &mut LoadedPage,
    color_support: ColorSupport,
    wcfg: WidthConfig,
) -> bool {
    if page.scene.invalidation.layout.is_empty() {
        return true;
    }
    if !page.scene.begin_relayout_transaction() {
        return false;
    }
    let governor = page.governor.clone();
    let Ok(mut candidate_page_buf) = page
        .buf
        .try_clone_governed(&governor, ResourceCategory::CompositorCells)
    else {
        page.scene.rollback_relayout_transaction();
        return false;
    };
    if !layout_pass_invalidated_governed(
        &mut page.scene,
        &mut candidate_page_buf,
        color_support,
        wcfg,
        &governor,
    ) {
        page.scene.rollback_relayout_transaction();
        return false;
    }
    page.scene.commit_relayout_transaction();
    page.buf = candidate_page_buf;
    true
}

fn layout_pass_invalidated_inner(
    scene: &mut crate::compositor::scene::Scene,
    page_buf: &mut CellBuffer,
    color_support: ColorSupport,
    wcfg: WidthConfig,
    governor: Option<&ResourceGovernor>,
) -> bool {
    use crate::compositor::layout::engine::relayout_in_place;
    use crate::compositor::scene::NodeId;
    use std::collections::HashSet;

    // Seed the work queue from the layout invalidation set.
    let work_items = scene.iter_tree_order().count();
    let Some(admitted_bytes) = work_items
        .checked_mul(std::mem::size_of::<NodeId>())
        .and_then(|one_collection| one_collection.checked_mul(2))
    else {
        page_buf.record_allocation_failure();
        return false;
    };
    let _lease = match (governor, admitted_bytes) {
        (_, 0) | (None, _) => None,
        (Some(governor), bytes) => {
            let Ok(lease) = governor.reserve(ResourceCategory::RemoteCollections, bytes) else {
                page_buf.record_allocation_failure();
                return false;
            };
            Some(lease)
        }
    };
    let mut queue = Vec::new();
    if queue.try_reserve_exact(work_items).is_err() {
        page_buf.record_allocation_failure();
        return false;
    }
    queue.extend(scene.invalidation.layout.iter().copied());
    let mut processed: HashSet<NodeId> = HashSet::new();
    if processed.try_reserve(work_items).is_err() {
        page_buf.record_allocation_failure();
        return false;
    }
    scene.invalidation.layout.clear();

    while let Some(node_id) = queue.pop() {
        if !processed.insert(node_id) {
            continue;
        }

        // Capture the node's pre-relayout rect for cascade detection.
        let old_rect = match scene.get(node_id).map(|n| n.placement().rect) {
            Some(r) if !r.is_empty() => r,
            _ => continue,
        };

        // Re-lay the subtree in place. Writes screen-absolute
        // placements and blits into page_buf at (rect.x, rect.y).
        if !relayout_in_place(scene, page_buf, node_id, color_support, wcfg) {
            return false;
        }

        // Cascade: if this node's size changed, its parent's layout
        // may be affected (subsequent siblings shift). Re-add parent
        // to the queue.
        let new_rect = scene
            .get(node_id)
            .map(|n| n.placement().rect)
            .unwrap_or(old_rect);
        if (new_rect.w != old_rect.w || new_rect.h != old_rect.h)
            && let Some(parent) = scene.get(node_id).and_then(|n| n.parent())
            && !processed.contains(&parent)
        {
            queue.push(parent);
        }
    }

    // Only layout invalidation is drained here. `composite`/`present`
    // invalidation (populated by `mark_composite` during patch
    // application) must survive to the composite pass — that's the
    // whole point of Phase 3. The composite pass clears
    // `invalidation.composite` and `invalidation.present` after
    // consuming them.
    //
    // Layout was already cleared at the top of this fn; nothing else
    // to do here.
    !page_buf.allocation_failed() && !scene.resource_limit_exceeded()
}

/// Stage 5 — full layout pass. Runs `layout_scene` against the
/// (already-mutated) scene. Scene is authoritative; no AST read or
/// rebuild happens here — the caller's prior patches are honored
/// because layout reads from `NodeKind::Panel::active`,
/// `FlowData.details_open`, etc. directly.
fn full_layout_pass(
    scene: &mut Scene,
    term_w: u16,
    term_h: u16,
    color_support: ColorSupport,
    wcfg: WidthConfig,
    focusables: &mut Vec<crate::compositor::panels::FocusableElement>,
    governor: Option<&ResourceGovernor>,
) -> LayoutOutput {
    let mut result = match governor {
        Some(governor) => crate::compositor::layout::engine::layout_scene_governed(
            scene,
            term_w,
            term_h,
            color_support,
            wcfg,
            governor,
        ),
        None => crate::compositor::layout::engine::layout_scene(
            scene,
            term_w,
            term_h,
            color_support,
            wcfg,
        ),
    };

    // Scene-authoritative: layout wrote `Node.placement` and
    // `Node.focusable_screen_rect` for every applicable node, so
    // `collect_focusables_from_scene` reads col/row/width directly
    // from the scene without zipping against a parallel `Vec`.
    let Some((mut next_focusables, mut placed, mut sticky_regions, mut projection_lease)) =
        reserve_projection_collections(scene, governor)
    else {
        result.buffer.record_allocation_failure();
        focusables.clear();
        return LayoutOutput {
            buffer: result.buffer,
            placed: Vec::new(),
            sticky_regions: Vec::new(),
            projection_lease: None,
        };
    };
    populate_projection_collections(
        scene,
        &mut next_focusables,
        &mut placed,
        &mut sticky_regions,
    );
    if reconcile_projection_lease(
        &next_focusables,
        next_focusables.capacity(),
        &placed,
        placed.capacity(),
        sticky_regions.capacity(),
        &mut projection_lease,
    )
    .is_none()
    {
        result.buffer.record_allocation_failure();
        focusables.clear();
        return LayoutOutput {
            buffer: result.buffer,
            placed: Vec::new(),
            sticky_regions: Vec::new(),
            projection_lease: None,
        };
    }
    *focusables = next_focusables;

    LayoutOutput {
        buffer: result.buffer,
        placed,
        sticky_regions,
        projection_lease,
    }
}

#[cfg(test)]
#[path = "../terminal_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "../parity_tests.rs"]
mod parity;
