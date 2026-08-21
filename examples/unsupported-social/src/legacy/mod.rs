mod auth;
mod email;
mod plugins;

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::watch;

use crate::protocol::connection::{AtpListener, AtpServerStream};
use crate::protocol::frame::{MessageType, RawFrame};
use crate::protocol::message::{
    ErrorMessage, GetMessage, HelloMessage, InputMessage, PageFlags, SubscribeMessage,
    SubscribeMode, UpdateFlags, UpdateMessage, WelcomeMessage,
};
use crate::protocol::{
    MAX_LIVE_UPDATE_SIZE, MAX_PAGE_MESSAGE_SIZE, MAX_WASM_MODULE_SIZE, PROTOCOL_VERSION,
    ProtocolError, SUPPORTED_CAPABILITIES,
};
use crate::scanner::escape::sanitize as strip_terminal_escapes;

use plugins::{BoardPlugin, ChatPlugin, HnPlugin, ServerStats, StatsPlugin};

/// Lock a shared mutex, recovering the guard even if a previous holder
/// panicked while holding it. Without this, a single panic inside a plugin
/// handler (which runs under the lock) would poison the mutex and turn every
/// subsequent request into a panic — a permanent, server-wide outage from one
/// bad input. The guarded data here is only ever mutated within a single
/// request, so recovering a poisoned guard cannot expose a torn invariant.
fn lock_recover<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Write `content` to `path` atomically: write a sibling temp file, fsync it,
/// then rename it into place. A crash mid-write leaves the previous file
/// intact, and a concurrent reader (notably the live-region file watcher)
/// never observes a half-written file. Plugin data stores use this instead of
/// a bare truncating `std::fs::write`.
pub(crate) fn atomic_write(path: &Path, content: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    #[cfg(unix)]
    if let Some(parent) = path.parent()
        && let Ok(directory) = std::fs::File::open(parent)
    {
        let _ = directory.sync_all();
    }
    Ok(())
}

/// Maximum file size the server will serve for .aml files (1 MiB).
const MAX_FILE_SIZE: u64 = 1024 * 1024;

/// Maximum number of concurrent connections the server will handle.
const MAX_CONNECTIONS: usize = 64;

/// A single source must not monopolize the global connection budget.
const MAX_CONNECTIONS_PER_IP: usize = 4;

/// Maximum live subscriptions retained by one connection.
const MAX_SUBSCRIPTIONS_PER_CONNECTION: usize = 16;

/// Bound writes as well as reads so a client that stops consuming data cannot
/// hold a connection permit forever.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// Maximum length of a poster name (chars).
pub(crate) const MAX_NAME_LEN: usize = 30;

/// Maximum accepted password length. Bounding this also bounds Argon2 input work.
const MAX_PASSWORD_LEN: usize = 128;

/// Maximum length of a single message/comment body (chars).
pub(crate) const MAX_BODY_LEN: usize = 500;

/// Maximum length of a link title (chars).
pub(crate) const MAX_TITLE_LEN: usize = 200;

/// Maximum length of a submitted URL (chars).
pub(crate) const MAX_URL_LEN: usize = 500;

/// Minimum seconds between posts per connection.
const RATE_LIMIT_SECS: u64 = 5;

/// Maximum stored messages per board.
pub(crate) const MAX_MESSAGES: usize = 200;

/// Maximum stored links per aggregator page.
pub(crate) const MAX_LINKS: usize = 500;

/// Maximum stored comments per aggregator page.
pub(crate) const MAX_COMMENTS: usize = 2000;

/// Sanitize a string for safe logging to the server operator's terminal.
///
/// Replaces control characters and escape sequences with visible placeholders
/// so injection attempts are visible rather than executed. Truncates long
/// values to prevent log flooding.
fn sanitize_log(s: &str) -> String {
    const MAX_LOG_LEN: usize = 200;
    let truncated = if s.len() > MAX_LOG_LEN {
        &s[..s.floor_char_boundary(MAX_LOG_LEN)]
    } else {
        s
    };
    let mut out = String::with_capacity(truncated.len());
    for ch in truncated.chars() {
        match ch {
            // Safe printable ASCII and common whitespace
            ' '..='~' | '\t' => out.push(ch),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            // ESC — the critical one
            '\x1B' => out.push_str("\\x1b"),
            // Other control characters
            '\x00'..='\x1F' | '\x7F' => {
                out.push_str(&format!("\\x{:02x}", ch as u32));
            }
            // C1 control characters (8-bit CSI, OSC, etc.)
            '\u{80}'..='\u{9F}' => {
                out.push_str(&format!("\\u{{{:04x}}}", ch as u32));
            }
            // All other Unicode — safe to display
            _ => out.push(ch),
        }
    }
    if s.len() > MAX_LOG_LEN {
        out.push_str("...");
    }
    out
}

/// Escape user-submitted content for safe embedding in AML text positions.
///
/// Escapes the three metacharacters that have special meaning in AML:
/// - `[` → `[[` (literal bracket escape)
/// - `]` → `]]` (literal bracket escape)
/// - `$` → `$$` (literal dollar escape, prevents component variable substitution)
///
/// The result is safe to embed between `[text]` and `[/text]` tags.
/// MUST NOT be placed in attribute values — only text content positions.
pub(crate) fn sanitize_user_content(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + s.len() / 4);
    for ch in s.chars() {
        match ch {
            '[' => out.push_str("[["),
            ']' => out.push_str("]]"),
            '$' => out.push_str("$$"),
            _ => out.push(ch),
        }
    }
    out
}

/// Decode a URL-encoded string (handles `+` as space and `%XX` hex escapes).
///
/// Correctly reassembles multi-byte UTF-8 sequences from consecutive `%XX`
/// escapes (e.g. `%C3%A9` → `é`). Invalid UTF-8 is replaced with U+FFFD.
pub(crate) fn url_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut bytes = s.bytes();
    let mut buf: Vec<u8> = Vec::new();

    while let Some(b) = bytes.next() {
        match b {
            b'%' => {
                let hi = bytes.next().and_then(|c| (c as char).to_digit(16));
                let lo = bytes.next().and_then(|c| (c as char).to_digit(16));
                if let (Some(h), Some(l)) = (hi, lo) {
                    buf.push((h * 16 + l) as u8);
                } else {
                    // Flush any pending bytes before the malformed escape
                    out.push_str(&String::from_utf8_lossy(&buf));
                    buf.clear();
                    out.push('%');
                }
            }
            _ => {
                // Flush pending percent-decoded bytes as UTF-8
                if !buf.is_empty() {
                    out.push_str(&String::from_utf8_lossy(&buf));
                    buf.clear();
                }
                if b == b'+' {
                    out.push(' ');
                } else {
                    out.push(b as char);
                }
            }
        }
    }

    // Flush any trailing percent-decoded bytes
    if !buf.is_empty() {
        out.push_str(&String::from_utf8_lossy(&buf));
    }

    out
}

/// Ordered URL-encoded form data. Duplicate names are retained.
#[derive(Debug, Clone, Default)]
pub(crate) struct FormData {
    fields: Vec<(String, String)>,
}

impl FormData {
    pub(crate) fn get(&self, name: &str) -> Option<&String> {
        self.fields
            .iter()
            .rev()
            .find_map(|(key, value)| (key == name).then_some(value))
    }

    #[allow(dead_code)]
    pub(crate) fn get_all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.fields
            .iter()
            .filter_map(move |(key, value)| (key == name).then_some(value.as_str()))
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    #[cfg(test)]
    fn insert(&mut self, name: String, value: String) {
        self.fields.push((name, value));
    }

    #[cfg(test)]
    fn values(&self) -> impl Iterator<Item = &String> {
        self.fields.iter().map(|(_, value)| value)
    }
}

/// Parse URL-encoded form data without collapsing duplicate field names.
pub(crate) fn parse_form_data(form: &str) -> FormData {
    let mut fields = Vec::new();
    for pair in form.split('&') {
        if let Some((key, value)) = pair.split_once('=') {
            fields.push((url_decode(key), url_decode(value)));
        }
    }
    FormData { fields }
}

/// Format a Unix timestamp as a relative time string.
pub(crate) fn format_time_ago(timestamp: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let diff = now.saturating_sub(timestamp);

    if diff < 60 {
        "just now".to_string()
    } else if diff < 3600 {
        let mins = diff / 60;
        if mins == 1 {
            "1 minute ago".to_string()
        } else {
            format!("{mins} minutes ago")
        }
    } else if diff < 86400 {
        let hours = diff / 3600;
        if hours == 1 {
            "1 hour ago".to_string()
        } else {
            format!("{hours} hours ago")
        }
    } else {
        let days = diff / 86400;
        if days == 1 {
            "1 day ago".to_string()
        } else {
            format!("{days} days ago")
        }
    }
}

/// Sanitize and truncate a user-submitted field value.
/// Strips terminal escapes and replaces control characters.
pub(crate) fn sanitize_field(raw: &str, max_chars: usize) -> String {
    let cleaned = strip_terminal_escapes(raw);
    let trimmed = cleaned.trim();
    let truncated: String = trimmed.chars().take(max_chars).collect();
    truncated.replace(['\t', '\n', '\r'], " ")
}

/// Extract the domain from a URL for display purposes.
pub(crate) fn extract_domain(url: &str) -> &str {
    let after_scheme = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .or_else(|| url.strip_prefix("atp://"))
        .unwrap_or(url);
    match after_scheme.find('/') {
        Some(idx) => &after_scheme[..idx],
        None => after_scheme,
    }
}

// ─── PagePlugin trait ───────────────────────────────────

/// Server-side plugin for dynamic page features.
///
/// Each plugin claims a template marker (e.g. `{{messages}}`). When a page
/// template contains that marker, the plugin renders content to replace it
/// and handles form submissions for that page.
///
/// Plugins may be **parameterized**: their marker is a prefix (e.g.
/// `"{{render:"`) and the text up to the closing `}}` is passed to
/// `render()` as `param`.
pub(crate) trait PagePlugin: Send + Sync {
    /// Template marker this plugin claims, e.g. `"{{messages}}"`.
    /// For parameterized plugins this is the prefix, e.g. `"{{render:"`.
    fn marker(&self) -> &str;

    /// Stable key used to route form submissions on pages containing more
    /// than one input-capable plugin. Such forms set `__handler=<key>` in
    /// their action query. Render-only plugins return `None`.
    fn input_key(&self) -> Option<&str> {
        None
    }

    /// Whether this plugin uses parameterized markers.
    /// When true, the framework extracts the text between the marker prefix
    /// and `}}`, passing it to `render()` as `param`.
    fn is_parameterized(&self) -> bool {
        false
    }

    /// Whether plugin content is live and should be re-rendered once per
    /// second for subscribed clients.
    fn polls(&self) -> bool {
        false
    }

    /// Render content replacing the marker. Called on GET requests.
    /// `query` contains the query string from the URI (e.g. `"item=3&reply=7"`).
    /// `peer` is the client's address (used for vote dedup).
    /// `param` is the text captured between the marker prefix and `}}` for
    /// parameterized plugins, or `None` for fixed-marker plugins.
    /// `identity` is the verified username if the client is authenticated.
    fn render(
        &mut self,
        aml_path: &Path,
        query: Option<&str>,
        peer: SocketAddr,
        param: Option<&str>,
        site_root: &Path,
        identity: Option<&str>,
    ) -> String;

    /// Handle form submission. Returns `Ok(true)` if a change was made,
    /// `Ok(false)` if no change (e.g. empty fields), or `Err(message)` with
    /// a user-visible error explaining why the submission was rejected.
    /// `identity` is the verified username if the client is authenticated.
    fn handle_input(
        &mut self,
        aml_path: &Path,
        fields: &FormData,
        query: Option<&str>,
        identity: Option<&str>,
    ) -> Result<bool, String>;
}

/// Simple non-cryptographic hash for content prefix verification.
fn content_hash(data: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325; // FNV offset basis
    for b in data.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3); // FNV prime
    }
    h
}

// ─── Subscription Manager ───────────────────────────────

/// Event sent from the watcher thread to a subscriber's connection task.
struct SubEvent {
    region: String,
    file_path: PathBuf,
}

/// A subscriber record in the shared map.
struct Subscriber {
    id: u64,
    region: String,
    tx: tokio::sync::mpsc::Sender<SubEvent>,
}

/// An active subscription held by a connection task.
struct ActiveSub {
    id: u64,
    file_path: PathBuf,
    region: String,
    delta_mode: bool,
    /// Byte length of last content sent (for delta detection).
    last_content_len: usize,
    /// Hash of the last-sent content prefix (for verifying append-only invariant).
    last_content_hash: u64,
}

/// Shared subscriber state accessed by both the watcher callback and connection tasks.
struct SubscriberMap {
    watchers: HashMap<PathBuf, Vec<Subscriber>>,
    next_id: u64,
    last_event: HashMap<PathBuf, Instant>,
}

/// Manages filesystem watches and fan-out to subscribers.
///
/// Shared across all connection tasks via `Arc`. The `notify` watcher
/// callback fires on a background OS thread and sends events through
/// per-connection channels using `try_send` (which is not async).
struct SubscriptionManager {
    subscribers: Arc<Mutex<SubscriberMap>>,
    watcher: Mutex<notify::RecommendedWatcher>,
}

impl SubscriptionManager {
    fn new() -> Result<Arc<Self>, notify::Error> {
        let subscribers = Arc::new(Mutex::new(SubscriberMap {
            watchers: HashMap::new(),
            next_id: 0,
            last_event: HashMap::new(),
        }));

        let callback_subs = Arc::clone(&subscribers);
        let watcher = notify::recommended_watcher(move |res: Result<notify::Event, _>| {
            let Ok(event) = res else { return };
            if !event.kind.is_modify() && !event.kind.is_create() {
                return;
            }
            let mut map = lock_recover(&callback_subs);
            let now = Instant::now();
            for path in &event.paths {
                // Debounce: skip if <100ms since last event for this file
                if let Some(last) = map.last_event.get(path)
                    && now.duration_since(*last) < Duration::from_millis(100)
                {
                    continue;
                }
                map.last_event.insert(path.clone(), now);

                if let Some(subs) = map.watchers.get(path) {
                    for sub in subs {
                        let _ = sub.tx.try_send(SubEvent {
                            region: sub.region.clone(),
                            file_path: path.clone(),
                        });
                    }
                }
            }
        })?;

        Ok(Arc::new(SubscriptionManager {
            subscribers,
            watcher: Mutex::new(watcher),
        }))
    }

    /// Register a subscriber. Returns the subscriber ID.
    /// Events are sent to the provided `tx` channel.
    fn subscribe(
        &self,
        file_path: PathBuf,
        region: String,
        tx: tokio::sync::mpsc::Sender<SubEvent>,
    ) -> Result<ActiveSub, notify::Error> {
        use notify::{RecursiveMode, Watcher};

        let mut map = lock_recover(&self.subscribers);
        let id = map.next_id;
        map.next_id += 1;

        let entry = map.watchers.entry(file_path.clone()).or_default();

        if entry.is_empty() {
            self.watcher
                .lock()
                .unwrap()
                .watch(&file_path, RecursiveMode::NonRecursive)?;
        }
        let region_clone = region.clone();
        entry.push(Subscriber { id, region, tx });

        Ok(ActiveSub {
            id,
            file_path,
            region: region_clone,
            delta_mode: false,
            last_content_len: 0,
            last_content_hash: 0,
        })
    }

    /// Remove a subscriber by ID. Unwatches the file if it was the last subscriber.
    fn unsubscribe(&self, file_path: &Path, subscriber_id: u64) {
        use notify::Watcher;

        let mut map = lock_recover(&self.subscribers);
        if let Some(subs) = map.watchers.get_mut(file_path) {
            subs.retain(|s| s.id != subscriber_id);
            if subs.is_empty() {
                map.watchers.remove(file_path);
                map.last_event.remove(file_path);
                let _ = lock_recover(&self.watcher).unwatch(file_path);
            }
        }
    }

    /// Remove all subscriptions by their IDs.
    fn unsubscribe_all(&self, subs: &[ActiveSub]) {
        for sub in subs {
            self.unsubscribe(&sub.file_path, sub.id);
        }
    }
}

// ─── Server ─────────────────────────────────────────────

/// Idle timeout before closing a connection (300 seconds per spec).
const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// Maximum time allowed for a client to complete its TLS handshake.
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

// ─── Request Handler Trait ───────────────────────────────

/// Response from a request handler.
pub enum Response {
    /// Serve an AML page, optionally with session directives.
    Page {
        content: String,
        flags: PageFlags,
        session: Vec<crate::session::SessionDirective>,
    },
    /// Serve a binary resource (e.g. .wasm).
    Resource { bytes: Vec<u8> },
    /// Return an error.
    Error { code: u16, message: String },
}

/// Handles application-level GET and INPUT requests.
///
/// Implement this trait to build custom ATP applications. The default
/// `FileHandler` serves AML files from a directory with plugin support.
pub trait RequestHandler: Send + Sync {
    /// Handle a GET request for the given path.
    /// `session` contains the session token sent by the client, if any.
    fn handle_get(
        &self,
        path: &str,
        query: Option<&str>,
        peer: SocketAddr,
        session: Option<&str>,
    ) -> Response;

    /// Handle an INPUT (form submission) request.
    /// `session` contains the session token sent by the client, if any.
    fn handle_input(
        &self,
        path: &str,
        query: Option<&str>,
        form_data: &str,
        peer: SocketAddr,
        session: Option<&str>,
    ) -> Response;

    /// Resolve a SUBSCRIBE path to a filesystem path for the watcher.
    /// Returns None if the path is invalid or not watchable.
    fn resolve_subscribe_path(&self, path: &str, session: Option<&str>) -> Option<PathBuf>;

    /// Re-render a polling plugin by name. Used for periodic live updates.
    /// Returns None if no matching poll plugin exists.
    fn render_poll_plugin(
        &self,
        name: &str,
        peer: SocketAddr,
        session: Option<&str>,
    ) -> Option<String>;
}

// ─── File Handler (default implementation) ───────────────

struct PluginSlot {
    marker: String,
    parameterized: bool,
    polls: bool,
    plugin: Mutex<Box<dyn PagePlugin>>,
}

impl PluginSlot {
    fn new(plugin: Box<dyn PagePlugin>) -> Self {
        Self {
            marker: plugin.marker().to_string(),
            parameterized: plugin.is_parameterized(),
            polls: plugin.polls(),
            plugin: Mutex::new(plugin),
        }
    }
}

/// Default request handler that serves AML files from a directory.
/// Routes dynamic page features through registered plugins.
pub struct FileHandler {
    root_dir: PathBuf,
    /// Plugins synchronize independently: a slow renderer or data-store write
    /// must not stall unrelated dynamic pages and live pollers.
    plugins: Vec<PluginSlot>,
    // Keyed by client IP, not full SocketAddr: the ephemeral source port
    // changes on every reconnect, so a port-keyed limiter is bypassed by simply
    // opening a new connection.
    last_post_times: Mutex<HashMap<IpAddr, Instant>>,
    auth: Mutex<auth::AuthSystem>,
    static_only: bool,
}

impl FileHandler {
    pub(crate) fn new(root_dir: PathBuf, stats: Arc<ServerStats>) -> Self {
        let auth = auth::AuthSystem::load(&root_dir);
        FileHandler {
            root_dir,
            plugins: vec![
                PluginSlot::new(Box::new(BoardPlugin::new())),
                PluginSlot::new(Box::new(ChatPlugin::new())),
                PluginSlot::new(Box::new(HnPlugin::new())),
                PluginSlot::new(Box::new(StatsPlugin::new(stats))),
            ],
            last_post_times: Mutex::new(HashMap::new()),
            auth: Mutex::new(auth),
            static_only: false,
        }
    }

    fn new_static(root_dir: PathBuf) -> Self {
        let auth = auth::AuthSystem::load(&root_dir);
        Self {
            root_dir,
            plugins: Vec::new(),
            last_post_times: Mutex::new(HashMap::new()),
            auth: Mutex::new(auth),
            static_only: true,
        }
    }

    /// Resolve a session token to a verified username.
    fn resolve_identity(&self, session: Option<&str>) -> Option<String> {
        let token = session?;
        lock_recover(&self.auth).resolve_session(token)
    }

    /// Handle login form submission — authenticate and issue a session token.
    fn handle_login(&self, form_data: &str, peer: SocketAddr) -> Response {
        let fields = parse_form_data(form_data);
        let raw_name = fields.get("name").map(|s| s.as_str()).unwrap_or("");
        let name = raw_name.trim().to_string();
        let password = fields.get("password").map(|s| s.as_str()).unwrap_or("");

        if !auth::validate_handle(&name)
            || password.is_empty()
            || password.chars().count() > MAX_PASSWORD_LEN
        {
            return Response::Error {
                code: 400,
                message: "Invalid username or password.".to_string(),
            };
        }

        let (candidate_username, password_hash) = {
            let mut auth = lock_recover(&self.auth);
            if let Err(msg) = auth.check_login_rate(peer.ip()) {
                return Response::Error {
                    code: 429,
                    message: msg.to_string(),
                };
            }
            auth.record_login_attempt(peer.ip());
            auth.login_challenge(&name)
        };

        // Argon2 is intentionally expensive; never hold the global auth-state
        // mutex while it runs.
        let password_valid = auth::AuthSystem::verify_login_challenge(password, &password_hash);
        let Some(username) = candidate_username.filter(|_| password_valid) else {
            eprintln!("[{peer}] login failed for {}", sanitize_log(&name));
            return Response::Error {
                code: 401,
                message: "Invalid handle or password.".to_string(),
            };
        };

        let token = match lock_recover(&self.auth).create_session(&username) {
            Ok(token) => token,
            Err(message) => {
                eprintln!(
                    "[{peer}] could not persist session: {}",
                    sanitize_log(&message)
                );
                return Response::Error { code: 500, message };
            }
        };
        eprintln!("[{peer}] login: {}", sanitize_log(&username));

        let content = format!(
            "[page mode=document title=\"Logged In\"]\n\
             \x20 [heading level=1]Welcome, {name}![/heading]\n\
             \x20 [spacer lines=1 /]\n\
             \x20 [text]You are now logged in. You can submit links and comment.[/text]\n\
             \x20 [spacer lines=1 /]\n\
             \x20 [link href=\"/\"][text fg=white]Continue[/text][/link]\n\
             [/page]",
            name = sanitize_user_content(&username),
        );

        Response::Page {
            content,
            flags: PageFlags::default(),
            session: vec![crate::session::SessionDirective::Set {
                token,
                scope: "/".to_string(),
                expires: None,
            }],
        }
    }

    /// Handle logout — destroy server-side session and clear client token.
    fn handle_logout(&self, session: Option<&str>, peer: SocketAddr) -> Response {
        if let Some(token) = session
            && let Err(message) = lock_recover(&self.auth).destroy_session(token)
        {
            eprintln!(
                "[{peer}] could not persist logout: {}",
                sanitize_log(&message)
            );
            return Response::Error { code: 500, message };
        }
        eprintln!("[{peer}] logout");

        let content = "[page mode=document title=\"Logged Out — DUSTNET\"]\n\
             \x20 [heading level=1 fg=cyan]Logged Out[/heading]\n\
             \x20 [spacer lines=1 /]\n\
             \x20 [text]You have been logged out.[/text]\n\
             \x20 [spacer lines=1 /]\n\
             \x20 [link href=\"/\"][text fg=cyan]← Back to directory[/text][/link]\n\
             [/page]"
            .to_string();

        Response::Page {
            content,
            flags: PageFlags::default(),
            session: vec![crate::session::SessionDirective::Clear {
                scope: "/".to_string(),
            }],
        }
    }

    /// Handle registration form submission.
    fn handle_register(&self, form_data: &str, peer: SocketAddr) -> Response {
        let fields = parse_form_data(form_data);
        let raw_name = fields.get("name").map(|s| s.as_str()).unwrap_or("");
        let name = raw_name.trim().to_string();
        let email_raw = fields.get("email").map(|s| s.as_str()).unwrap_or("");
        let email = email_raw.trim().to_lowercase();
        let password = fields.get("password").map(|s| s.as_str()).unwrap_or("");

        if !auth::validate_handle(&name) {
            return Response::Error {
                code: 400,
                message: "Handle must be 1–30 ASCII letters, numbers, underscores, or hyphens."
                    .to_string(),
            };
        }
        if !auth::validate_email(&email) {
            return Response::Error {
                code: 400,
                message: "A valid email address is required.".to_string(),
            };
        }
        let password_len = password.chars().count();
        if !(8..=MAX_PASSWORD_LEN).contains(&password_len) {
            return Response::Error {
                code: 400,
                message: "Password must be between 8 and 128 characters.".to_string(),
            };
        }

        // Check SMTP is configured before doing any work
        if !email::smtp_configured() {
            return Response::Error {
                code: 500,
                message: "Registration is not available — SMTP is not configured.".to_string(),
            };
        }

        {
            let mut auth = lock_recover(&self.auth);
            if let Err(msg) = auth.check_register_rate(peer.ip()) {
                return Response::Error {
                    code: 429,
                    message: msg.to_string(),
                };
            }
            auth.record_register_attempt(peer.ip());
        }

        // As with login verification, perform Argon2 hashing outside the
        // mutex so ordinary session resolution remains responsive.
        let password_hash = match auth::AuthSystem::hash_password(password) {
            Ok(hash) => hash,
            Err(message) => return Response::Error { code: 500, message },
        };
        let code = match lock_recover(&self.auth).register_hashed(
            name.clone(),
            email.clone(),
            password_hash,
        ) {
            Ok(c) => c,
            Err(msg) => {
                let message = if matches!(
                    msg.as_str(),
                    "That handle is already taken." | "That email is already registered."
                ) {
                    "Registration could not be completed with those details.".to_string()
                } else {
                    msg
                };
                return Response::Error { code: 400, message };
            }
        };

        // Send verification email
        if let Some(smtp) = email::SmtpConfig::from_env()
            && let Err(e) = smtp.send_verification(&email, &name, &code)
        {
            eprintln!("[{peer}] SMTP error: {e}");
            if let Err(cleanup_error) = lock_recover(&self.auth).cancel_registration(&code) {
                eprintln!(
                    "[{peer}] could not clean up failed registration: {}",
                    sanitize_log(&cleanup_error)
                );
            }
            return Response::Error {
                code: 500,
                message: "Failed to send verification email. Please try again later.".to_string(),
            };
        }

        eprintln!(
            "[{peer}] registered: {} <{}>",
            sanitize_log(&name),
            sanitize_log(&email)
        );

        let content = format!(
            "[page mode=document title=\"Check Your Email — DUSTNET\"]\n\
             \x20 [heading level=1 fg=cyan]Check Your Email[/heading]\n\
             \x20 [spacer lines=1 /]\n\
             \x20 [text]A verification code has been sent to [text bold]{email}[/text].[/text]\n\
             \x20 [spacer lines=1 /]\n\
             \x20 [link href=\"/verify\"][text fg=cyan]→ Enter verification code[/text][/link]\n\
             \x20 [spacer lines=1 /]\n\
             \x20 [link href=\"/\"][text fg=cyan]← Back to directory[/text][/link]\n\
             [/page]",
            email = sanitize_user_content(&email),
        );

        Response::Page {
            content,
            flags: PageFlags::default(),
            session: Vec::new(),
        }
    }

    /// Handle email verification form submission.
    fn handle_verify(&self, form_data: &str, peer: SocketAddr) -> Response {
        let fields = parse_form_data(form_data);
        let code = sanitize_field(fields.get("code").map(|s| s.as_str()).unwrap_or(""), 8);

        if code.is_empty() {
            return Response::Error {
                code: 400,
                message: "Verification code is required.".to_string(),
            };
        }

        let mut auth = lock_recover(&self.auth);

        if let Err(msg) = auth.check_verify_rate(peer.ip()) {
            return Response::Error {
                code: 429,
                message: msg.to_string(),
            };
        }
        auth.record_verify_attempt(peer.ip());

        match auth.verify(&code) {
            Ok(username) => {
                eprintln!("[{peer}] verified: {}", sanitize_log(&username));

                let content = format!(
                    "[page mode=document title=\"Verified — DUSTNET\"]\n\
                     \x20 [heading level=1 fg=cyan]Account Verified[/heading]\n\
                     \x20 [spacer lines=1 /]\n\
                     \x20 [text]Welcome, [text bold]{name}[/text]! Your account is now active.[/text]\n\
                     \x20 [spacer lines=1 /]\n\
                     \x20 [link href=\"/login\"][text fg=cyan]→ Log in[/text][/link]\n\
                     \x20 [spacer lines=1 /]\n\
                     \x20 [link href=\"/\"][text fg=cyan]← Back to directory[/text][/link]\n\
                     [/page]",
                    name = sanitize_user_content(&username),
                );

                Response::Page {
                    content,
                    flags: PageFlags::default(),
                    session: Vec::new(),
                }
            }
            Err(msg) => {
                eprintln!("[{peer}] verification failed: {msg}");
                Response::Error {
                    code: 400,
                    message: msg,
                }
            }
        }
    }

    /// Serve an AML page, running it through plugins if a marker is found.
    fn serve_page(
        &self,
        file_path: &Path,
        query: Option<&str>,
        peer: SocketAddr,
        session: Option<&str>,
    ) -> Response {
        match std::fs::metadata(file_path) {
            Ok(meta) => {
                if meta.len() > MAX_FILE_SIZE {
                    return Response::Error {
                        code: 500,
                        message: "File too large".to_string(),
                    };
                }
            }
            Err(_) => {
                return Response::Error {
                    code: 404,
                    message: "Page not found".to_string(),
                };
            }
        }

        let template = match std::fs::read_to_string(file_path) {
            Ok(t) => t,
            Err(_) => {
                return Response::Error {
                    code: 404,
                    message: "Page not found".to_string(),
                };
            }
        };

        let identity = self.resolve_identity(session);
        let content =
            self.apply_plugins(&template, file_path, query, peer, None, identity.as_deref());

        let flags = PageFlags {
            cacheable: false,
            has_live_regions: content.contains("[live "),
            has_session: false,
        };
        Response::Page {
            content,
            flags,
            session: Vec::new(),
        }
    }

    /// Serve a page with an optional error banner and form value preservation.
    fn serve_page_with_error(
        &self,
        file_path: &Path,
        query: Option<&str>,
        peer: SocketAddr,
        error: Option<&str>,
        preserve_fields: Option<&FormData>,
        session: Option<&str>,
    ) -> Response {
        match std::fs::metadata(file_path) {
            Ok(meta) => {
                if meta.len() > MAX_FILE_SIZE {
                    return Response::Error {
                        code: 500,
                        message: "File too large".to_string(),
                    };
                }
            }
            Err(_) => {
                return Response::Error {
                    code: 404,
                    message: "Page not found".to_string(),
                };
            }
        }

        let template = match std::fs::read_to_string(file_path) {
            Ok(t) => t,
            Err(_) => {
                return Response::Error {
                    code: 404,
                    message: "Page not found".to_string(),
                };
            }
        };

        let identity = self.resolve_identity(session);
        let mut content = self.apply_plugins(
            &template,
            file_path,
            query,
            peer,
            error,
            identity.as_deref(),
        );

        // On error, inject value attributes into input tags to preserve user input
        if let Some(fields) = preserve_fields {
            content = inject_form_values(&content, fields);
        }

        let flags = PageFlags {
            cacheable: false,
            has_live_regions: content.contains("[live "),
            has_session: false,
        };
        Response::Page {
            content,
            flags,
            session: Vec::new(),
        }
    }

    /// Find matching plugins and replace their markers in the template.
    /// If `error` is set, inject an error banner above non-parameterized plugin content.
    /// A page may contain multiple plugin markers (e.g. `{{stats}}` and `{{messages}}`).
    fn apply_plugins(
        &self,
        template: &str,
        aml_path: &Path,
        query: Option<&str>,
        peer: SocketAddr,
        error: Option<&str>,
        identity: Option<&str>,
    ) -> String {
        let mut result = template.to_string();
        for slot in &self.plugins {
            let marker = &slot.marker;
            if slot.parameterized {
                if !result.contains(marker) {
                    continue;
                }
                let mut plugin = lock_recover(&slot.plugin);
                // Parameterized: find all {{prefix...}} occurrences, extract param
                while let Some(start) = result.find(marker) {
                    let after_prefix = start + marker.len();
                    if let Some(end_offset) = result[after_prefix..].find("}}") {
                        let param = result[after_prefix..after_prefix + end_offset].to_string();
                        let full_tag_end = after_prefix + end_offset + 2;
                        let rendered = plugin.render(
                            aml_path,
                            query,
                            peer,
                            Some(&param),
                            &self.root_dir,
                            identity,
                        );
                        result = format!(
                            "{}{}{}",
                            &result[..start],
                            rendered,
                            &result[full_tag_end..]
                        );
                    } else {
                        break; // malformed tag, skip
                    }
                }
            } else {
                // Fixed marker: original behavior
                if result.contains(marker) {
                    let mut plugin = lock_recover(&slot.plugin);
                    let mut rendered = String::new();
                    if let Some(msg) = error {
                        let escaped = sanitize_user_content(msg);
                        rendered.push_str(&format!(
                            "  [box border=single fg=red w=55]\n\
                             \x20   [text bold fg=bright-red]Error:[/text]\n\
                             \x20   [text fg=red]{escaped}[/text]\n\
                             \x20 [/box]\n\
                             \x20 [spacer lines=1 /]\n\n"
                        ));
                    }
                    rendered.push_str(&plugin.render(
                        aml_path,
                        query,
                        peer,
                        None,
                        &self.root_dir,
                        identity,
                    ));
                    if slot.polls {
                        let poll_name = marker.trim_start_matches("{{").trim_end_matches("}}");
                        result = result.replace(marker, &format!(
                            "[live id=\"__poll_{poll_name}\" endpoint=\"/__poll/{poll_name}\" scroll=none]{rendered}[/live]"
                        ));
                    } else {
                        result = result.replace(marker, &rendered);
                    }
                }
            }
        }
        let account = match identity {
            Some(name) => format!(
                "[text dim]{}[/text] [text fg=white]|[/text] [link href=\"/logout\"][text fg=white]logout[/text][/link]",
                sanitize_user_content(name)
            ),
            None => "[link href=\"/register\"][text fg=white]join[/text][/link] [text fg=white]|[/text] [link href=\"/login\"][text fg=white]login[/text][/link]".to_string(),
        };
        result = result.replace("{{account}}", &account);
        result
    }
}

impl RequestHandler for FileHandler {
    fn handle_get(
        &self,
        path: &str,
        query: Option<&str>,
        peer: SocketAddr,
        session: Option<&str>,
    ) -> Response {
        eprintln!(
            "[{peer}] GET {}{}",
            sanitize_log(path),
            query
                .map(|q| format!("?{}", sanitize_log(q)))
                .unwrap_or_default()
        );

        match resolve_path(&self.root_dir, path) {
            Ok(file_path) => {
                if file_path.extension().is_some_and(|e| e == "wasm") {
                    serve_resource(&file_path)
                } else {
                    self.serve_page(&file_path, query, peer, session)
                }
            }
            Err(code) => {
                let msg = match code {
                    400 => "Bad request — path traversal rejected",
                    404 => "Page not found",
                    _ => "Error",
                };
                Response::Error {
                    code,
                    message: msg.to_string(),
                }
            }
        }
    }

    fn handle_input(
        &self,
        path: &str,
        query: Option<&str>,
        form_data: &str,
        peer: SocketAddr,
        session: Option<&str>,
    ) -> Response {
        if self.static_only {
            return Response::Error {
                code: 405,
                message: "INPUT is disabled by the static production server".into(),
            };
        }
        eprintln!(
            "[{peer}] INPUT {}{} ({} bytes)",
            sanitize_log(path),
            query
                .map(|q| format!("?{}", sanitize_log(q)))
                .unwrap_or_default(),
            form_data.len(),
        );

        // Auth routes — hardcoded, not plugin-driven
        match path {
            "/login" => return self.handle_login(form_data, peer),
            "/logout" => return self.handle_logout(session, peer),
            "/register" => return self.handle_register(form_data, peer),
            "/verify" => return self.handle_verify(form_data, peer),
            _ => {}
        }

        let file_path = match resolve_path(&self.root_dir, path) {
            Ok(p) => p,
            Err(code) => {
                return Response::Error {
                    code,
                    message: if code == 400 {
                        "Bad request"
                    } else {
                        "Page not found"
                    }
                    .to_string(),
                };
            }
        };

        let template = match std::fs::read_to_string(&file_path) {
            Ok(c) => c,
            Err(_) => {
                return Response::Error {
                    code: 404,
                    message: "Page not found".to_string(),
                };
            }
        };

        // Resolve session token → verified username for plugins
        let identity = self.resolve_identity(session);

        // Select an input-capable plugin explicitly. A page with one handler
        // remains backward compatible; pages with several must name one via
        // the reserved `__handler` action-query parameter.
        let fields = parse_form_data(form_data);
        let mut input_error: Option<String> = None;
        let requested_handler = query
            .map(parse_form_data)
            .and_then(|params| params.get("__handler").cloned());
        let candidates: Vec<&PluginSlot> = self
            .plugins
            .iter()
            .filter(|slot| {
                template.contains(&slot.marker) && lock_recover(&slot.plugin).input_key().is_some()
            })
            .collect();
        let selected = if let Some(ref key) = requested_handler {
            candidates
                .iter()
                .copied()
                .find(|slot| lock_recover(&slot.plugin).input_key() == Some(key.as_str()))
        } else if candidates.len() == 1 {
            candidates.first().copied()
        } else {
            None
        };

        if candidates.len() > 1 && requested_handler.is_none() {
            input_error = Some(
                "This page has multiple form handlers; the form action must select one.".into(),
            );
        } else if requested_handler.is_some() && selected.is_none() {
            input_error = Some("The requested form handler is not available on this page.".into());
        } else if let Some(slot) = selected {
            let rate_limited = lock_recover(&self.last_post_times)
                .get(&peer.ip())
                .is_some_and(|last| last.elapsed().as_secs() < RATE_LIMIT_SECS);
            if rate_limited {
                input_error = Some("Too many posts — please wait a moment.".into());
            } else {
                let mut plugin = lock_recover(&slot.plugin);
                match plugin.handle_input(&file_path, &fields, query, identity.as_deref()) {
                    Ok(true) => {
                        let now = Instant::now();
                        let mut times = lock_recover(&self.last_post_times);
                        // Bound growth: drop entries older than the rate-limit
                        // window (they can no longer block a post anyway).
                        times.retain(|_, last| {
                            now.duration_since(*last).as_secs() < RATE_LIMIT_SECS
                        });
                        times.insert(peer.ip(), now);
                        drop(times);
                        eprintln!("[{peer}] posted to {}", sanitize_log(path));
                    }
                    Ok(false) => {}
                    Err(msg) => {
                        eprintln!("[{peer}] rejected: {}", sanitize_log(&msg));
                        input_error = Some(msg);
                    }
                }
            }
        }
        // On error, preserve the submitted form values so the user doesn't lose input
        let preserve = if input_error.is_some() {
            Some(&fields)
        } else {
            None
        };
        self.serve_page_with_error(
            &file_path,
            query,
            peer,
            input_error.as_deref(),
            preserve,
            session,
        )
    }

    fn resolve_subscribe_path(&self, path: &str, _session: Option<&str>) -> Option<PathBuf> {
        resolve_path(&self.root_dir, path).ok()
    }

    fn render_poll_plugin(
        &self,
        name: &str,
        peer: SocketAddr,
        session: Option<&str>,
    ) -> Option<String> {
        let marker = format!("{{{{{name}}}}}");
        let identity = self.resolve_identity(session);
        for slot in &self.plugins {
            if slot.marker == marker && slot.polls {
                let mut plugin = lock_recover(&slot.plugin);
                return Some(plugin.render(
                    &self.root_dir,
                    None,
                    peer,
                    None,
                    &self.root_dir,
                    identity.as_deref(),
                ));
            }
        }
        None
    }
}

/// Inject `value` attributes into `[input]` tags to preserve form data on error.
///
/// For each `[input name="X" /]` in the AML, if `fields` contains key `X`,
/// adds or replaces `value="V"` so the client pre-fills the field.
fn inject_form_values(content: &str, fields: &FormData) -> String {
    let mut result = String::with_capacity(content.len());
    let mut rest = content;

    while let Some(start) = rest.find("[input ") {
        result.push_str(&rest[..start]);
        let tag_content = &rest[start..];

        // Find the end of this tag
        let end = tag_content.find(']').unwrap_or(tag_content.len());
        let tag = &tag_content[..=end];

        // Extract the name attribute
        if let Some(name) = extract_attr_value(tag, "name") {
            // Passwords are never reflected back into AML responses.
            if tag
                .split_whitespace()
                .any(|part| part.trim_end_matches([']', '/']) == "password")
            {
                result.push_str(tag);
            } else if let Some(val) = fields.get(name) {
                // Escape the value for safe embedding in an attribute
                let escaped = escape_aml_attribute(val);
                // Remove any existing value attribute
                let cleaned = remove_attr(tag, "value");
                // Insert value attribute before the closing /] or ]
                let insert_pos = cleaned
                    .rfind("/]")
                    .unwrap_or_else(|| cleaned.rfind(']').unwrap_or(cleaned.len()));
                result.push_str(&cleaned[..insert_pos]);
                if !result.ends_with(' ') {
                    result.push(' ');
                }
                result.push_str(&format!("value=\"{escaped}\""));
                result.push_str(&cleaned[insert_pos..]);
            } else {
                result.push_str(tag);
            }
        } else {
            result.push_str(tag);
        }
        rest = &tag_content[end + 1..];
    }
    result.push_str(rest);
    result
}

fn escape_aml_attribute(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\t' => escaped.push_str("\\t"),
            '\r' => escaped.push_str("\\n"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

/// Extract the value of a quoted attribute from an AML tag string.
/// Returns the content between quotes for `attr="value"`.
fn extract_attr_value<'a>(tag: &'a str, attr_name: &str) -> Option<&'a str> {
    let pattern = format!("{}=\"", attr_name);
    let start = tag.find(&pattern)?;
    let val_start = start + pattern.len();
    let val_end = tag[val_start..].find('"')? + val_start;
    Some(&tag[val_start..val_end])
}

/// Remove an attribute (key="value") from an AML tag string.
fn remove_attr(tag: &str, attr_name: &str) -> String {
    let pattern = format!("{}=\"", attr_name);
    if let Some(start) = tag.find(&pattern) {
        let val_start = start + pattern.len();
        if let Some(end_quote) = tag[val_start..].find('"') {
            let end = val_start + end_quote + 1; // past closing quote
            // Remove the attribute and any trailing space
            let mut result = tag[..start].to_string();
            let after = &tag[end..];
            result.push_str(after.strip_prefix(' ').unwrap_or(after));
            return result;
        }
    }
    tag.to_string()
}

/// Serve a binary resource file (e.g. .wasm).
fn serve_resource(file_path: &Path) -> Response {
    match std::fs::metadata(file_path) {
        Ok(meta) => {
            if meta.len() > crate::protocol::MAX_WASM_MODULE_SIZE as u64 {
                return Response::Error {
                    code: 500,
                    message: "Resource too large".to_string(),
                };
            }
        }
        Err(_) => {
            return Response::Error {
                code: 404,
                message: "Resource not found".to_string(),
            };
        }
    }

    match std::fs::read(file_path) {
        Ok(bytes) => Response::Resource { bytes },
        Err(_) => Response::Error {
            code: 500,
            message: "Failed to read resource".to_string(),
        },
    }
}

// ─── Server ─────────────────────────────────────────────

/// ATP server. Generic over the request handler.
pub struct AtpServer {
    handler: Arc<dyn RequestHandler>,
    listener: AtpListener,
    active_connections: Arc<AtomicUsize>,
    shutdown_tx: watch::Sender<bool>,
    shutdown_rx: watch::Receiver<bool>,
    subscriptions: Arc<SubscriptionManager>,
}

impl AtpServer {
    pub fn new(root_dir: PathBuf, listener: AtpListener) -> Self {
        let active_connections = Arc::new(AtomicUsize::new(0));
        let stats = Arc::new(ServerStats {
            active_connections: active_connections.clone(),
            started_at: Instant::now(),
        });
        let handler = Arc::new(FileHandler::new(root_dir, stats));
        Self::with_handler(handler, listener, active_connections)
    }

    /// Construct the production static server. Dynamic plugins, accounts,
    /// email, boards, chat, and form mutation are deliberately unavailable.
    pub fn new_static(root_dir: PathBuf, listener: AtpListener) -> Self {
        let active_connections = Arc::new(AtomicUsize::new(0));
        let handler = Arc::new(FileHandler::new_static(root_dir));
        Self::with_handler(handler, listener, active_connections)
    }

    /// Create a server with a custom request handler.
    pub fn with_handler(
        handler: Arc<dyn RequestHandler>,
        listener: AtpListener,
        active_connections: Arc<AtomicUsize>,
    ) -> Self {
        let subscriptions =
            SubscriptionManager::new().expect("failed to create filesystem watcher");
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        AtpServer {
            handler,
            listener,
            active_connections,
            shutdown_tx,
            shutdown_rx,
            subscriptions,
        }
    }

    /// Returns a sender that can trigger graceful shutdown.
    pub fn shutdown_handle(&self) -> watch::Sender<bool> {
        self.shutdown_tx.clone()
    }

    /// Run the accept loop. Spawns a task for each connection.
    pub async fn run(&mut self) -> Result<(), ProtocolError> {
        let addr = self.listener.local_addr()?;
        eprintln!("ATP server listening on {addr}");
        eprintln!("Max connections: {MAX_CONNECTIONS}");

        let semaphore = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
        let connections_by_ip = Arc::new(Mutex::new(HashMap::<IpAddr, usize>::new()));

        loop {
            tokio::select! {
                result = self.listener.accept_pending() => {
                    match result {
                        Ok(pending) => {
                            let permit = match semaphore.clone().try_acquire_owned() {
                                Ok(p) => p,
                                Err(_) => {
                                    eprintln!(
                                        "[{}] rejected: at connection limit ({MAX_CONNECTIONS})",
                                        pending.peer_addr()
                                    );
                                    continue;
                                }
                            };
                            let peer_ip = pending.peer_addr().ip();
                            {
                                let mut counts = lock_recover(&connections_by_ip);
                                let count = counts.entry(peer_ip).or_default();
                                if *count >= MAX_CONNECTIONS_PER_IP {
                                    eprintln!(
                                        "[{}] rejected: per-IP connection limit ({MAX_CONNECTIONS_PER_IP})",
                                        pending.peer_addr()
                                    );
                                    drop(permit);
                                    continue;
                                }
                                *count += 1;
                            }
                            self.active_connections.fetch_add(1, Ordering::Relaxed);

                            let handler = self.handler.clone();
                            let subs = self.subscriptions.clone();
                            let counter = self.active_connections.clone();
                            let ip_counts = connections_by_ip.clone();
                            tokio::spawn(async move {
                                match tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT, pending.handshake()).await {
                                    Ok(Ok(conn)) => handle_connection(conn, handler, subs).await,
                                    Ok(Err(e)) => eprintln!("TLS handshake failed: {e}"),
                                    Err(_) => eprintln!("TLS handshake timed out"),
                                }
                                counter.fetch_sub(1, Ordering::Relaxed);
                                let mut counts = lock_recover(&ip_counts);
                                if let Some(count) = counts.get_mut(&peer_ip) {
                                    *count -= 1;
                                    if *count == 0 {
                                        counts.remove(&peer_ip);
                                    }
                                }
                                drop(permit);
                            });
                        }
                        Err(e) => {
                            eprintln!("Accept error: {e}");
                        }
                    }
                }
                _ = self.shutdown_rx.changed() => {
                    if *self.shutdown_rx.borrow() {
                        eprintln!("Shutdown signal received, stopping accept loop");
                        break;
                    }
                }
            }
        }

        // Wait briefly for active connections to drain
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.active_connections.load(Ordering::Relaxed) > 0 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        let remaining = self.active_connections.load(Ordering::Relaxed);
        if remaining > 0 {
            eprintln!("{remaining} connections still active at shutdown");
        }

        eprintln!("Server stopped");
        Ok(())
    }
}

async fn handle_connection(
    mut conn: AtpServerStream,
    handler: Arc<dyn RequestHandler>,
    sub_mgr: Arc<SubscriptionManager>,
) {
    let peer = conn.peer_addr();
    eprintln!("[{peer}] connected");

    // 1. Expect HELLO (with timeout)
    let frame = match tokio::time::timeout(IDLE_TIMEOUT, conn.recv_frame()).await {
        Ok(Ok(f)) => f,
        Ok(Err(e)) => {
            eprintln!("[{peer}] error receiving HELLO: {e}");
            return;
        }
        Err(_) => {
            eprintln!("[{peer}] timeout waiting for HELLO");
            return;
        }
    };

    if frame.msg_type != MessageType::Hello {
        eprintln!("[{peer}] expected HELLO, got {:?}", frame.msg_type);
        send_error(&mut conn, 400, "Expected HELLO").await;
        return;
    }

    let hello_body = match std::str::from_utf8(&frame.body) {
        Ok(body) => body,
        Err(_) => {
            eprintln!("[{peer}] invalid HELLO: body is not valid UTF-8");
            send_error(&mut conn, 400, "Bad request").await;
            return;
        }
    };
    let hello = match HelloMessage::parse(hello_body) {
        Ok(h) => {
            eprintln!(
                "[{peer}] HELLO from {:?}",
                h.client.as_deref().map(sanitize_log)
            );
            h
        }
        Err(e) => {
            eprintln!("[{peer}] invalid HELLO: {}", sanitize_log(&e.to_string()));
            return;
        }
    };

    if hello.protocol_version != PROTOCOL_VERSION {
        eprintln!(
            "[{peer}] unsupported protocol version: {}",
            sanitize_log(&hello.protocol_version)
        );
        send_error(&mut conn, 400, "Unsupported protocol version").await;
        return;
    }

    // 2. Send WELCOME
    let welcome = WelcomeMessage {
        protocol_version: PROTOCOL_VERSION.to_string(),
        server: Some("Dustnet-Server/0.1".to_string()),
        site_name: None,
        capabilities: hello
            .capabilities
            .iter()
            .filter(|capability| SUPPORTED_CAPABILITIES.contains(&capability.as_str()))
            .cloned()
            .collect(),
    };
    let Ok(welcome_body) = welcome.serialize() else {
        eprintln!("[{peer}] unable to allocate WELCOME body");
        return;
    };
    let welcome_frame = RawFrame {
        msg_type: MessageType::Welcome,
        flags: 0,
        body: welcome_body.into_bytes(),
    };
    if let Err(e) = send_frame_bounded(&mut conn, &welcome_frame).await {
        eprintln!("[{peer}] error sending WELCOME: {e}");
        return;
    }

    // 3. Request loop — uses select! to wait for either client frames or subscription events
    let mut active_subs: Vec<ActiveSub> = Vec::new();
    let mut poll_subs: Vec<PollSub> = Vec::new();
    let (sub_tx, mut sub_rx) = tokio::sync::mpsc::channel::<SubEvent>(16);
    let mut last_client_activity = Instant::now();
    let mut poll_ticker = tokio::time::interval(Duration::from_secs(1));
    poll_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            result = tokio::time::timeout(IDLE_TIMEOUT, conn.recv_frame()) => {
                match result {
                    Ok(Ok(frame)) => {
                        last_client_activity = Instant::now();

                        match frame.msg_type {
                            MessageType::Get => {
                                // Client navigated away — clear subscriptions
                                sub_mgr.unsubscribe_all(&active_subs);
                                active_subs.clear();
                                poll_subs.clear();
                                dispatch_get(&mut conn, &frame, handler.clone(), peer).await;
                            }
                            MessageType::Input => {
                                dispatch_input(&mut conn, &frame, handler.clone(), peer).await;
                            }
                            MessageType::Subscribe => {
                                if active_subs.len() + poll_subs.len()
                                    >= MAX_SUBSCRIPTIONS_PER_CONNECTION
                                {
                                    send_error(
                                        &mut conn,
                                        429,
                                        "Maximum active subscriptions exceeded",
                                    )
                                    .await;
                                    continue;
                                }
                                if let Some(poll) = dispatch_subscribe(
                                    &mut conn, &frame, &*handler, peer,
                                    &sub_mgr, &mut active_subs, &sub_tx,
                                ).await {
                                    poll_subs.push(poll);
                                }
                            }
                            MessageType::Unsubscribe => {
                                eprintln!("[{peer}] UNSUBSCRIBE");
                                sub_mgr.unsubscribe_all(&active_subs);
                                active_subs.clear();
                                poll_subs.clear();
                            }
                            MessageType::Bye => {
                                eprintln!("[{peer}] BYE");
                                let bye_frame = RawFrame {
                                    msg_type: MessageType::ServerBye,
                                    flags: 0,
                                    body: Vec::new(),
                                };
                                let _ = send_frame_bounded(&mut conn, &bye_frame).await;
                                break;
                            }
                            MessageType::Hello => {
                                eprintln!("[{peer}] unexpected second HELLO");
                                send_error(&mut conn, 400, "HELLO is only valid at connection start").await;
                                break;
                            }
                            MessageType::Ping => {
                                // The frame arriving is what refreshes
                                // `last_client_activity` above; PONG just lets
                                // the client tell a live server from a socket
                                // that is silently swallowing writes.
                                let pong = RawFrame {
                                    msg_type: MessageType::Pong,
                                    flags: 0,
                                    body: Vec::new(),
                                };
                                let _ = send_frame_bounded(&mut conn, &pong).await;
                            }
                            _ => {
                                // Wrong-direction messages are rejected in the
                                // transport before their bodies are allocated.
                                unreachable!("direction validation admitted a server message")
                            }
                        }
                    }
                    Ok(Err(ProtocolError::ConnectionClosed)) => {
                        eprintln!("[{peer}] disconnected");
                        break;
                    }
                    Ok(Err(e)) => {
                        eprintln!("[{peer}] error: {e}");
                        break;
                    }
                    Err(_) => {
                        // Idle timeout — but skip if the client has active
                        // subscriptions (it's holding the connection open
                        // intentionally to receive live updates, not idling).
                        if last_client_activity.elapsed() >= IDLE_TIMEOUT {
                            if active_subs.is_empty() {
                                eprintln!("[{peer}] idle timeout, closing");
                                break;
                            } else {
                                eprintln!(
                                    "[{peer}] keep-alive (subs={}, idle={}s)",
                                    active_subs.len(),
                                    last_client_activity.elapsed().as_secs(),
                                );
                            }
                        }
                    }
                }
            }
            Some(event) = sub_rx.recv() => {
                // Drain all pending events, dedup by region
                let mut pending: HashMap<String, PathBuf> = HashMap::new();
                pending.insert(event.region, event.file_path);
                while let Ok(event) = sub_rx.try_recv() {
                    pending.insert(event.region, event.file_path);
                }
                for (region, file_path) in pending {
                    if let Some(content) = read_live_content(&file_path) {
                        // Find the ActiveSub for this region to check delta mode
                        let sub = active_subs.iter_mut().find(|s| s.region == region);
                        let (send_content, flags) = if let Some(sub) = sub {
                            if sub.delta_mode && content.len() > sub.last_content_len {
                                // File grew — verify the prefix hasn't changed
                                let prefix = &content[..sub.last_content_len];
                                if content_hash(prefix) == sub.last_content_hash {
                                    // Prefix intact — send only the new tail
                                    let delta = &content[sub.last_content_len..];
                                    sub.last_content_len = content.len();
                                    sub.last_content_hash = content_hash(&content);
                                    (delta.to_string(), UpdateFlags { delta: true })
                                } else {
                                    // Prefix changed — content was rewritten, full replace
                                    sub.last_content_len = content.len();
                                    sub.last_content_hash = content_hash(&content);
                                    (content, UpdateFlags::default())
                                }
                            } else {
                                // Rewritten, truncated, or non-delta — full replace
                                sub.last_content_len = content.len();
                                sub.last_content_hash = content_hash(&content);
                                (content, UpdateFlags::default())
                            }
                        } else {
                            (content, UpdateFlags::default())
                        };
                        match send_update(&mut conn, &region, &send_content, flags).await {
                            Ok(()) => eprintln!(
                                "[{peer}] UPDATE region={} mode={} ({} bytes)",
                                sanitize_log(&region),
                                if flags.delta { "delta" } else { "replace" },
                                send_content.len(),
                            ),
                            Err(e) => eprintln!("[{peer}] error sending UPDATE: {e}"),
                        }
                    }
                }
            }
            _ = poll_ticker.tick(), if !poll_subs.is_empty() => {
                for poll in &poll_subs {
                    if let Some(content) = handler.render_poll_plugin(
                        &poll.plugin_name,
                        peer,
                        poll.session.as_deref(),
                    ) {
                        let bytes = content.len();
                        match send_update(&mut conn, &poll.region, &content, UpdateFlags::default()).await {
                            Ok(()) => eprintln!(
                                "[{peer}] UPDATE region={} mode=poll ({} bytes)",
                                sanitize_log(&poll.region),
                                bytes,
                            ),
                            Err(e) => eprintln!("[{peer}] error sending poll UPDATE: {e}"),
                        }
                    }
                }
            }
        }
    }

    // Cleanup all subscriptions on exit
    sub_mgr.unsubscribe_all(&active_subs);
}

/// Send a Response over the connection.
async fn send_response(conn: &mut AtpServerStream, response: Response, peer: SocketAddr) {
    match response {
        Response::Page {
            content,
            flags,
            session,
        } => {
            if content.len() > MAX_FILE_SIZE as usize {
                eprintln!("[{peer}] generated AML exceeds document size limit");
                send_error(conn, 500, "Generated page is too large").await;
                return;
            }
            let page = crate::protocol::message::PageMessage {
                content,
                flags,
                session_directives: session,
            };
            let Ok((body, flags_byte)) = page.encode_body() else {
                eprintln!("[{peer}] unable to allocate PAGE body");
                send_error(conn, 500, "Server resource limit exceeded").await;
                return;
            };
            if body.len() > MAX_PAGE_MESSAGE_SIZE {
                eprintln!("[{peer}] generated PAGE exceeds semantic size limit");
                send_error(conn, 500, "Generated page is too large").await;
                return;
            }
            let frame = RawFrame {
                msg_type: MessageType::Page,
                flags: flags_byte,
                body,
            };
            if let Err(e) = send_frame_bounded(conn, &frame).await {
                eprintln!("[{peer}] error sending PAGE: {e}");
            }
        }
        Response::Resource { bytes } => {
            if bytes.len() > MAX_WASM_MODULE_SIZE {
                eprintln!("[{peer}] generated RESOURCE exceeds semantic size limit");
                send_error(conn, 500, "Generated resource is too large").await;
                return;
            }
            let frame = RawFrame {
                msg_type: MessageType::Resource,
                flags: 0,
                body: bytes,
            };
            if let Err(e) = send_frame_bounded(conn, &frame).await {
                eprintln!("[{peer}] error sending RESOURCE: {e}");
            }
        }
        Response::Error { code, message } => {
            send_error(conn, code, &message).await;
        }
    }
}

async fn dispatch_get(
    conn: &mut AtpServerStream,
    frame: &RawFrame,
    handler: Arc<dyn RequestHandler>,
    peer: SocketAddr,
) {
    let body_str = match std::str::from_utf8(&frame.body) {
        Ok(body) => body,
        Err(_) => {
            eprintln!("[{peer}] invalid GET: body is not valid UTF-8");
            send_error(conn, 400, "Bad request").await;
            return;
        }
    };
    let get_msg = match GetMessage::parse(body_str) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("[{peer}] invalid GET: {}", sanitize_log(&e.to_string()));
            send_error(conn, 400, "Bad request").await;
            return;
        }
    };

    let path = get_msg.path.clone();
    let query = get_msg.query.clone();
    let session = get_msg.session.clone();
    let response = tokio::task::spawn_blocking(move || {
        handler.handle_get(&path, query.as_deref(), peer, session.as_deref())
    })
    .await;
    let response = match response {
        Ok(r) => r,
        Err(join_err) => {
            // The handler panicked (the panic itself is already printed by the
            // blocking thread's default hook). Recover the connection with a
            // 500 instead of propagating and killing the task.
            eprintln!("[{peer}] handler panicked: {join_err}");
            send_error(conn, 500, "Internal server error").await;
            return;
        }
    };
    send_response(conn, response, peer).await;
}

/// Resolve a request path to a filesystem path, with security checks.
///
/// Returns the resolved file path, or an error code (400 for traversal, 404 for not found).
fn resolve_path(root_dir: &Path, request_path: &str) -> Result<PathBuf, u16> {
    // Strip leading slash
    let relative = request_path.trim_start_matches('/');

    if relative.is_empty() {
        // Default to index.aml
        let index = root_dir.join("index.aml");
        if index.exists() {
            return Ok(index);
        }
        return Err(404);
    }

    // Build candidate path
    let mut candidate = root_dir.join(relative);

    // Append .aml extension if no extension present
    if candidate.extension().is_none() {
        candidate.set_extension("aml");
    }

    // Canonicalize to resolve symlinks and ..
    let canonical = match candidate.canonicalize() {
        Ok(p) => p,
        Err(_) => return Err(404),
    };

    let root_canonical = match root_dir.canonicalize() {
        Ok(p) => p,
        Err(_) => return Err(500),
    };

    // Directory traversal prevention: resolved path must be under root
    if !canonical.starts_with(&root_canonical) {
        return Err(400);
    }

    // Only serve .aml and .wasm files
    match canonical.extension().and_then(|e| e.to_str()) {
        Some("aml") | Some("wasm") => Ok(canonical),
        _ => Err(404),
    }
}

async fn dispatch_input(
    conn: &mut AtpServerStream,
    frame: &RawFrame,
    handler: Arc<dyn RequestHandler>,
    peer: SocketAddr,
) {
    let body_str = match std::str::from_utf8(&frame.body) {
        Ok(body) => body,
        Err(_) => {
            eprintln!("[{peer}] invalid INPUT: body is not valid UTF-8");
            send_error(conn, 400, "Bad request").await;
            return;
        }
    };
    let input_msg = match InputMessage::parse(body_str) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("[{peer}] invalid INPUT: {}", sanitize_log(&e.to_string()));
            send_error(conn, 400, "Bad request").await;
            return;
        }
    };

    let input_path = input_msg.path.clone();
    let form_data = input_msg.form_data.clone();
    eprintln!(
        "[{peer}] INPUT {} ({} bytes)",
        sanitize_log(&input_path),
        form_data.len(),
    );
    let route_path = input_path.split('?').next().unwrap_or(&input_path);
    if matches!(route_path, "/login" | "/logout" | "/register" | "/verify") && !conn.is_tls() {
        send_error(conn, 403, "Authentication requires TLS").await;
        return;
    }
    let session = input_msg.session.clone();
    let response = tokio::task::spawn_blocking(move || {
        // Split query from the INPUT path (form action may contain ?params)
        let (path, query) = match input_path.find('?') {
            Some(idx) => (&input_path[..idx], Some(&input_path[idx + 1..])),
            None => (input_path.as_str(), None),
        };
        handler.handle_input(path, query, &form_data, peer, session.as_deref())
    })
    .await;
    let response = match response {
        Ok(r) => r,
        Err(join_err) => {
            // The handler panicked (the panic itself is already printed by the
            // blocking thread's default hook). Recover the connection with a
            // 500 instead of propagating and killing the task.
            eprintln!("[{peer}] handler panicked: {join_err}");
            send_error(conn, 500, "Internal server error").await;
            return;
        }
    };
    send_response(conn, response, peer).await;
}

/// Send an UPDATE frame for a live region.
async fn send_update(
    conn: &mut AtpServerStream,
    region: &str,
    content: &str,
    flags: UpdateFlags,
) -> Result<(), ProtocolError> {
    let update = UpdateMessage {
        region: region.to_string(),
        content: content.to_string(),
        flags,
    };
    let frame = RawFrame {
        msg_type: MessageType::Update,
        flags: flags.to_bits(),
        body: update.serialize()?.into_bytes(),
    };
    if frame.body.len() > MAX_LIVE_UPDATE_SIZE {
        return Err(ProtocolError::FrameTooLarge(frame.body.len() as u32));
    }
    send_frame_bounded(conn, &frame).await
}

async fn send_frame_bounded(
    conn: &mut AtpServerStream,
    frame: &RawFrame,
) -> Result<(), ProtocolError> {
    tokio::time::timeout(WRITE_TIMEOUT, conn.send_frame(frame))
        .await
        .map_err(|_| ProtocolError::Timeout)?
}

fn read_live_content(path: &Path) -> Option<String> {
    use std::io::Read;

    let metadata = std::fs::metadata(path).ok()?;
    if metadata.len() > MAX_LIVE_UPDATE_SIZE as u64 {
        return None;
    }
    let file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_LIVE_UPDATE_SIZE as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > MAX_LIVE_UPDATE_SIZE {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// A timer-based poll subscription for live-updating plugins.
struct PollSub {
    region: String,
    plugin_name: String,
    session: Option<String>,
}

async fn dispatch_subscribe(
    conn: &mut AtpServerStream,
    frame: &RawFrame,
    handler: &dyn RequestHandler,
    peer: SocketAddr,
    sub_mgr: &SubscriptionManager,
    active_subs: &mut Vec<ActiveSub>,
    sub_tx: &tokio::sync::mpsc::Sender<SubEvent>,
) -> Option<PollSub> {
    let body_str = match std::str::from_utf8(&frame.body) {
        Ok(body) => body,
        Err(_) => {
            eprintln!("[{peer}] invalid SUBSCRIBE: body is not valid UTF-8");
            send_error(conn, 400, "Bad request").await;
            return None;
        }
    };
    let sub_msg = match SubscribeMessage::parse(body_str) {
        Ok(m) => m,
        Err(e) => {
            eprintln!(
                "[{peer}] invalid SUBSCRIBE: {}",
                sanitize_log(&e.to_string())
            );
            send_error(conn, 400, "Bad request").await;
            return None;
        }
    };

    let delta_mode = sub_msg.mode == SubscribeMode::Delta;
    eprintln!(
        "[{peer}] SUBSCRIBE {} region={} mode={}",
        sanitize_log(&sub_msg.path),
        sanitize_log(&sub_msg.region),
        if delta_mode { "delta" } else { "replace" },
    );

    // Poll-based subscription for live-updating plugins (always full replace)
    if let Some(plugin_name) = sub_msg.path.strip_prefix("/__poll/") {
        let plugin_name = plugin_name.to_string();
        if let Some(content) =
            handler.render_poll_plugin(&plugin_name, peer, sub_msg.session.as_deref())
        {
            let bytes = content.len();
            match send_update(conn, &sub_msg.region, &content, UpdateFlags::default()).await {
                Ok(()) => eprintln!(
                    "[{peer}] UPDATE region={} mode=poll ({} bytes)",
                    sanitize_log(&sub_msg.region),
                    bytes,
                ),
                Err(e) => {
                    eprintln!("[{peer}] error sending initial poll UPDATE: {e}");
                    return None;
                }
            }
            return Some(PollSub {
                region: sub_msg.region,
                plugin_name,
                session: sub_msg.session,
            });
        }
        send_error(conn, 404, "Endpoint not found").await;
        return None;
    }

    let file_path = match handler.resolve_subscribe_path(&sub_msg.path, sub_msg.session.as_deref())
    {
        Some(p) => p,
        None => {
            send_error(conn, 404, "Endpoint not found").await;
            return None;
        }
    };

    // Send the initial content immediately (always full replace)
    let (initial_len, initial_hash) = if let Some(content) = read_live_content(&file_path) {
        let len = content.len();
        let hash = content_hash(&content);
        match send_update(conn, &sub_msg.region, &content, UpdateFlags::default()).await {
            Ok(()) => eprintln!(
                "[{peer}] UPDATE region={} mode=replace ({} bytes)",
                sanitize_log(&sub_msg.region),
                len,
            ),
            Err(e) => {
                eprintln!("[{peer}] error sending initial UPDATE: {e}");
                return None;
            }
        }
        (len, hash)
    } else {
        (0, 0)
    };

    match sub_mgr.subscribe(file_path.clone(), sub_msg.region.clone(), sub_tx.clone()) {
        Ok(sub) => {
            active_subs.push(ActiveSub {
                id: sub.id,
                file_path: sub.file_path,
                region: sub_msg.region,
                delta_mode,
                last_content_len: initial_len,
                last_content_hash: initial_hash,
            });
        }
        Err(e) => {
            eprintln!("[{peer}] failed to watch file: {e}");
            send_error(conn, 500, "Server error").await;
        }
    }
    None
}

async fn send_error(conn: &mut AtpServerStream, code: u16, message: &str) {
    let err = ErrorMessage {
        code,
        message: Some(message.to_string()),
    };
    let Ok(body) = err.serialize() else {
        return;
    };
    let frame = RawFrame {
        msg_type: MessageType::Error,
        flags: 0,
        body: body.into_bytes(),
    };
    let _ = send_frame_bounded(conn, &frame).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::connection::AtpConnection;
    use std::io::Write;
    use tempfile::TempDir;

    async fn setup_server() -> (TempDir, std::net::SocketAddr) {
        let dir = tempfile::tempdir().unwrap();

        // Create test .aml files
        let hello_path = dir.path().join("hello.aml");
        let mut f = std::fs::File::create(&hello_path).unwrap();
        write!(
            f,
            r#"[page mode=document title="Hello"]
[text]Hello World[/text]
[/page]"#
        )
        .unwrap();

        let sub_dir = dir.path().join("sub");
        std::fs::create_dir(&sub_dir).unwrap();
        let sub_page = sub_dir.join("page.aml");
        let mut f2 = std::fs::File::create(&sub_page).unwrap();
        write!(f2, r#"[page mode=document][text]Sub page[/text][/page]"#).unwrap();

        // Seed auth: create a test user and pre-authenticated sessions.
        // The server loads .auth/ on startup, so these must exist before AtpServer::new.
        let auth_dir = dir.path().join(".auth");
        std::fs::create_dir_all(&auth_dir).unwrap();
        // Create a verified test user (password doesn't matter — tests use sessions directly)
        let test_hash = auth::AuthSystem::hash_password("testpass").unwrap();
        std::fs::write(
            auth_dir.join("users.tsv"),
            format!("alice\talice@test.com\t{test_hash}\ttrue\t1000000000\nbob\tbob@test.com\t{test_hash}\ttrue\t1000000000"),
        ).unwrap();
        // Create sessions that map test tokens to usernames
        let far_future = 9999999999u64;
        std::fs::write(
            auth_dir.join("sessions.tsv"),
            format!("test-token\talice\t1000000000\t{far_future}\ntest-token-2\tbob\t1000000000\t{far_future}"),
        ).unwrap();

        let listener = AtpListener::bind_plain("127.0.0.1", 0).await.unwrap();
        let addr = listener.local_addr().unwrap();

        let root = dir.path().to_path_buf();
        tokio::spawn(async move {
            let mut server = AtpServer::new(root, listener);
            let _ = server.run().await;
        });

        // Give server a moment to start
        tokio::time::sleep(Duration::from_millis(50)).await;

        (dir, addr)
    }

    async fn do_handshake(conn: &mut AtpConnection) {
        let hello = HelloMessage {
            protocol_version: PROTOCOL_VERSION.into(),
            terminal_size: Some("80x24".into()),
            color_support: Some("truecolor".into()),
            client: Some("test".into()),
            capabilities: SUPPORTED_CAPABILITIES
                .iter()
                .map(|capability| (*capability).into())
                .collect(),
        };
        let frame = RawFrame {
            msg_type: MessageType::Hello,
            flags: 0,
            body: hello.serialize().unwrap().into_bytes(),
        };
        conn.send_frame(&frame).await.unwrap();

        let welcome = conn.recv_frame().await.unwrap();
        assert_eq!(welcome.msg_type, MessageType::Welcome);
    }

    async fn send_get(conn: &mut AtpConnection, path: &str) -> RawFrame {
        send_get_with_query(conn, path, None).await
    }

    async fn send_get_with_session(
        conn: &mut AtpConnection,
        path: &str,
        query: Option<&str>,
        session: Option<&str>,
    ) -> RawFrame {
        let get = GetMessage {
            path: path.into(),
            query: query.map(|s| s.to_string()),
            referrer: None,
            session: session.map(|s| s.to_string()),
        };
        let frame = RawFrame {
            msg_type: MessageType::Get,
            flags: 0,
            body: get.serialize().unwrap().into_bytes(),
        };
        conn.send_frame(&frame).await.unwrap();
        conn.recv_frame().await.unwrap()
    }

    async fn send_get_with_query(
        conn: &mut AtpConnection,
        path: &str,
        query: Option<&str>,
    ) -> RawFrame {
        let get = GetMessage {
            path: path.into(),
            query: query.map(|s| s.to_string()),
            referrer: None,
            session: None,
        };
        let frame = RawFrame {
            msg_type: MessageType::Get,
            flags: 0,
            body: get.serialize().unwrap().into_bytes(),
        };
        conn.send_frame(&frame).await.unwrap();
        conn.recv_frame().await.unwrap()
    }

    #[tokio::test]
    async fn handshake() {
        let (_dir, addr) = setup_server().await;
        let mut conn = AtpConnection::connect_plain("127.0.0.1", addr.port())
            .await
            .unwrap();
        do_handshake(&mut conn).await;
    }

    #[tokio::test]
    async fn concurrent_clients_complete_independent_requests() {
        let (_dir, addr) = setup_server().await;
        let clients = (0..MAX_CONNECTIONS_PER_IP).map(|_| async move {
            let mut conn = AtpConnection::connect_plain("127.0.0.1", addr.port())
                .await
                .unwrap();
            do_handshake(&mut conn).await;
            let response = send_get(&mut conn, "/hello").await;
            assert_eq!(response.msg_type, MessageType::Page);
        });
        tokio::time::timeout(
            Duration::from_secs(3),
            futures_util::future::join_all(clients),
        )
        .await
        .expect("concurrent clients stalled");
    }

    #[tokio::test]
    async fn graceful_shutdown_stops_accept_loop() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("index.aml"),
            "[page mode=document][text]ok[/text][/page]",
        )
        .unwrap();
        let listener = AtpListener::bind_plain("127.0.0.1", 0).await.unwrap();
        let mut server = AtpServer::new_static(directory.path().to_path_buf(), listener);
        let shutdown = server.shutdown_handle();
        let task = tokio::spawn(async move { server.run().await });
        tokio::task::yield_now().await;
        shutdown.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("server did not stop before its graceful-shutdown deadline")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn invalid_utf8_control_message_is_rejected() {
        let (_dir, addr) = setup_server().await;
        let mut conn = AtpConnection::connect_plain("127.0.0.1", addr.port())
            .await
            .unwrap();
        do_handshake(&mut conn).await;
        conn.send_frame(&RawFrame {
            msg_type: MessageType::Get,
            flags: 0,
            body: vec![0xff],
        })
        .await
        .unwrap();
        let response = conn.recv_frame().await.unwrap();
        assert_eq!(response.msg_type, MessageType::Error);
    }

    #[tokio::test]
    async fn second_hello_is_rejected_as_invalid_state() {
        let (_dir, addr) = setup_server().await;
        let mut conn = AtpConnection::connect_plain("127.0.0.1", addr.port())
            .await
            .unwrap();
        do_handshake(&mut conn).await;
        conn.send_frame(&RawFrame {
            msg_type: MessageType::Hello,
            flags: 0,
            body: b"HELLO/0.2\nClient: duplicate\n".to_vec(),
        })
        .await
        .unwrap();
        let response = conn.recv_frame().await.unwrap();
        assert_eq!(response.msg_type, MessageType::Error);
        assert_eq!(
            ErrorMessage::parse(&String::from_utf8_lossy(&response.body))
                .unwrap()
                .code,
            400
        );
    }

    #[tokio::test]
    async fn handshake_rejects_incompatible_protocol_version() {
        let (_dir, addr) = setup_server().await;
        let mut conn = AtpConnection::connect_plain("127.0.0.1", addr.port())
            .await
            .unwrap();
        conn.send_frame(&RawFrame {
            msg_type: MessageType::Hello,
            flags: 0,
            body: b"HELLO/2.0\nClient: incompatible\n".to_vec(),
        })
        .await
        .unwrap();
        let response = conn.recv_frame().await.unwrap();
        assert_eq!(response.msg_type, MessageType::Error);
        let error = ErrorMessage::parse(&String::from_utf8(response.body).unwrap()).unwrap();
        assert_eq!(error.code, 400);
    }

    #[tokio::test]
    async fn get_valid_file() {
        let (_dir, addr) = setup_server().await;
        let mut conn = AtpConnection::connect_plain("127.0.0.1", addr.port())
            .await
            .unwrap();
        do_handshake(&mut conn).await;

        let response = send_get(&mut conn, "/hello").await;
        assert_eq!(response.msg_type, MessageType::Page);
        let content = String::from_utf8(response.body).unwrap();
        assert!(content.contains("Hello World"));
    }

    #[tokio::test]
    async fn get_missing_file_returns_error_404() {
        let (_dir, addr) = setup_server().await;
        let mut conn = AtpConnection::connect_plain("127.0.0.1", addr.port())
            .await
            .unwrap();
        do_handshake(&mut conn).await;

        let response = send_get(&mut conn, "/nonexistent").await;
        assert_eq!(response.msg_type, MessageType::Error);
        let body = String::from_utf8(response.body).unwrap();
        let err = ErrorMessage::parse(&body).unwrap();
        assert_eq!(err.code, 404);
    }

    #[tokio::test]
    async fn directory_traversal_rejected() {
        let (_dir, addr) = setup_server().await;
        let mut conn = AtpConnection::connect_plain("127.0.0.1", addr.port())
            .await
            .unwrap();
        do_handshake(&mut conn).await;

        let response = send_get(&mut conn, "/../../../etc/passwd").await;
        assert!(
            response.msg_type == MessageType::Error,
            "expected ERROR for traversal, got {:?}",
            response.msg_type
        );
        let body = String::from_utf8(response.body).unwrap();
        let err = ErrorMessage::parse(&body).unwrap();
        // Should be 400 (traversal) or 404 (file not found / no .aml ext)
        assert!(err.code == 400 || err.code == 404);
    }

    #[tokio::test]
    async fn bye_exchange() {
        let (_dir, addr) = setup_server().await;
        let mut conn = AtpConnection::connect_plain("127.0.0.1", addr.port())
            .await
            .unwrap();
        do_handshake(&mut conn).await;

        let bye = RawFrame {
            msg_type: MessageType::Bye,
            flags: 0,
            body: Vec::new(),
        };
        conn.send_frame(&bye).await.unwrap();

        let response = conn.recv_frame().await.unwrap();
        assert_eq!(response.msg_type, MessageType::ServerBye);
    }

    #[tokio::test]
    async fn multiple_gets_same_connection() {
        let (_dir, addr) = setup_server().await;
        let mut conn = AtpConnection::connect_plain("127.0.0.1", addr.port())
            .await
            .unwrap();
        do_handshake(&mut conn).await;

        // First GET
        let r1 = send_get(&mut conn, "/hello").await;
        assert_eq!(r1.msg_type, MessageType::Page);

        // Second GET — same connection (keep-alive)
        let r2 = send_get(&mut conn, "/hello").await;
        assert_eq!(r2.msg_type, MessageType::Page);

        // GET subdirectory
        let r3 = send_get(&mut conn, "/sub/page").await;
        assert_eq!(r3.msg_type, MessageType::Page);
        let content = String::from_utf8(r3.body).unwrap();
        assert!(content.contains("Sub page"));
    }

    #[test]
    fn resolve_path_basic() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.aml");
        std::fs::write(&file, "content").unwrap();

        let resolved = resolve_path(dir.path(), "/test").unwrap();
        assert_eq!(resolved, file.canonicalize().unwrap());
    }

    #[test]
    fn resolve_path_with_extension() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.aml");
        std::fs::write(&file, "content").unwrap();

        let resolved = resolve_path(dir.path(), "/test.aml").unwrap();
        assert_eq!(resolved, file.canonicalize().unwrap());
    }

    #[test]
    fn resolve_path_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let result = resolve_path(dir.path(), "/../../../etc/passwd");
        assert!(result.is_err());
    }

    #[test]
    fn resolve_path_non_aml_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("secret.txt");
        std::fs::write(&file, "secret").unwrap();

        let result = resolve_path(dir.path(), "/secret.txt");
        assert_eq!(result.err(), Some(404));
    }

    #[tokio::test]
    async fn input_returns_page() {
        let (_dir, addr) = setup_server().await;
        let mut conn = AtpConnection::connect_plain("127.0.0.1", addr.port())
            .await
            .unwrap();
        do_handshake(&mut conn).await;

        // Send INPUT to /hello (which exists)
        let input = InputMessage {
            path: "/hello".into(),
            form_data: "name=Alice&msg=Hi".into(),
            session: None,
        };
        let frame = RawFrame {
            msg_type: MessageType::Input,
            flags: 0,
            body: input.serialize().unwrap().into_bytes(),
        };
        conn.send_frame(&frame).await.unwrap();
        let response = conn.recv_frame().await.unwrap();
        assert_eq!(response.msg_type, MessageType::Page);
        let content = String::from_utf8(response.body).unwrap();
        assert!(content.contains("Hello World"));
    }

    #[tokio::test]
    async fn input_missing_page_returns_error() {
        let (_dir, addr) = setup_server().await;
        let mut conn = AtpConnection::connect_plain("127.0.0.1", addr.port())
            .await
            .unwrap();
        do_handshake(&mut conn).await;

        let input = InputMessage {
            path: "/nonexistent".into(),
            form_data: "key=val".into(),
            session: None,
        };
        let frame = RawFrame {
            msg_type: MessageType::Input,
            flags: 0,
            body: input.serialize().unwrap().into_bytes(),
        };
        conn.send_frame(&frame).await.unwrap();
        let response = conn.recv_frame().await.unwrap();
        assert_eq!(response.msg_type, MessageType::Error);
    }

    #[tokio::test]
    async fn subscribe_and_receive_update() {
        let (dir, addr) = setup_server().await;

        // Create a file to watch
        let live_path = dir.path().join("ticker.aml");
        std::fs::write(&live_path, "[text]initial[/text]").unwrap();

        let mut conn = AtpConnection::connect_plain("127.0.0.1", addr.port())
            .await
            .unwrap();
        do_handshake(&mut conn).await;

        // Subscribe to the ticker
        let sub = SubscribeMessage {
            path: "/ticker".into(),
            region: "clock".into(),
            mode: SubscribeMode::Replace,
            session: None,
        };
        let frame = RawFrame {
            msg_type: MessageType::Subscribe,
            flags: 0,
            body: sub.serialize().unwrap().into_bytes(),
        };
        conn.send_frame(&frame).await.unwrap();

        // Should receive the initial UPDATE immediately
        let initial = tokio::time::timeout(Duration::from_secs(3), conn.recv_frame())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(initial.msg_type, MessageType::Update);
        let initial_body = String::from_utf8(initial.body).unwrap();
        let initial_update = UpdateMessage::parse(&initial_body).unwrap();
        assert_eq!(initial_update.region, "clock");
        assert!(initial_update.content.contains("initial"));

        // Now modify the file and expect a second UPDATE
        std::thread::sleep(Duration::from_millis(100));
        std::fs::write(&live_path, "[text]updated[/text]").unwrap();

        let response = conn.recv_frame().await.unwrap();
        assert_eq!(response.msg_type, MessageType::Update);
        let body = String::from_utf8(response.body).unwrap();
        let update = UpdateMessage::parse(&body).unwrap();
        assert_eq!(update.region, "clock");
        assert!(update.content.contains("updated"));
    }

    #[tokio::test]
    async fn subscriptions_are_bounded_per_connection() {
        let (dir, addr) = setup_server().await;
        std::fs::write(dir.path().join("ticker.aml"), "[text]initial[/text]").unwrap();

        let mut conn = AtpConnection::connect_plain("127.0.0.1", addr.port())
            .await
            .unwrap();
        do_handshake(&mut conn).await;

        for index in 0..MAX_SUBSCRIPTIONS_PER_CONNECTION {
            let sub = SubscribeMessage {
                path: "/ticker".into(),
                region: format!("region-{index}"),
                mode: SubscribeMode::Replace,
                session: None,
            };
            conn.send_frame(&RawFrame {
                msg_type: MessageType::Subscribe,
                flags: 0,
                body: sub.serialize().unwrap().into_bytes(),
            })
            .await
            .unwrap();
            assert_eq!(
                conn.recv_frame().await.unwrap().msg_type,
                MessageType::Update
            );
        }

        let excess = SubscribeMessage {
            path: "/ticker".into(),
            region: "excess".into(),
            mode: SubscribeMode::Replace,
            session: None,
        };
        conn.send_frame(&RawFrame {
            msg_type: MessageType::Subscribe,
            flags: 0,
            body: excess.serialize().unwrap().into_bytes(),
        })
        .await
        .unwrap();
        let response = conn.recv_frame().await.unwrap();
        assert_eq!(response.msg_type, MessageType::Error);
        let error = ErrorMessage::parse(&String::from_utf8(response.body).unwrap()).unwrap();
        assert_eq!(error.code, 429);
    }

    #[test]
    fn live_file_reads_enforce_update_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.aml");
        std::fs::write(&path, vec![b'x'; MAX_LIVE_UPDATE_SIZE + 1]).unwrap();
        assert!(read_live_content(&path).is_none());
    }

    // ─── sanitize_log ──────────────────────────────────────

    #[test]
    fn sanitize_log_plain_text() {
        assert_eq!(sanitize_log("hello world"), "hello world");
    }

    #[test]
    fn sanitize_log_strips_ansi_escape() {
        assert_eq!(
            sanitize_log("before\x1b[31mred\x1b[0mafter"),
            "before\\x1b[31mred\\x1b[0mafter"
        );
    }

    #[test]
    fn sanitize_log_strips_osc_title() {
        assert_eq!(sanitize_log("a\x1b]0;pwned\x07b"), "a\\x1b]0;pwned\\x07b");
    }

    #[test]
    fn sanitize_log_strips_c1_csi() {
        assert_eq!(sanitize_log("a\u{9b}31mb"), "a\\u{009b}31mb");
    }

    #[test]
    fn sanitize_log_truncates_long_input() {
        let long = "a".repeat(300);
        let result = sanitize_log(&long);
        assert!(result.ends_with("..."));
        assert!(result.len() < 210);
    }

    #[test]
    fn sanitize_log_preserves_unicode() {
        assert_eq!(sanitize_log("café 日本語 🎉"), "café 日本語 🎉");
    }

    // ─── sanitize_user_content ─────────────────────────────

    #[test]
    fn user_content_plain_text() {
        assert_eq!(sanitize_user_content("hello world"), "hello world");
    }

    #[test]
    fn user_content_escapes_brackets() {
        assert_eq!(sanitize_user_content("a [b] c"), "a [[b]] c");
    }

    #[test]
    fn user_content_escapes_dollar() {
        assert_eq!(sanitize_user_content("$100"), "$$100");
    }

    #[test]
    fn user_content_prevents_tag_injection() {
        assert_eq!(
            sanitize_user_content("[text fg=red]evil[/text]"),
            "[[text fg=red]]evil[[/text]]"
        );
    }

    #[test]
    fn user_content_prevents_link_injection() {
        assert_eq!(
            sanitize_user_content("[link href=\"atp://evil.site\"]click me[/link]"),
            "[[link href=\"atp://evil.site\"]]click me[[/link]]"
        );
    }

    #[test]
    fn user_content_empty() {
        assert_eq!(sanitize_user_content(""), "");
    }

    #[test]
    fn user_content_preserves_unicode() {
        assert_eq!(sanitize_user_content("café 🎉 日本語"), "café 🎉 日本語");
    }

    // ─── url_decode / parse_form_data ──────────────────────

    #[test]
    fn url_decode_plain() {
        assert_eq!(url_decode("hello"), "hello");
    }

    #[test]
    fn url_decode_plus_as_space() {
        assert_eq!(url_decode("hello+world"), "hello world");
    }

    #[test]
    fn url_decode_percent() {
        assert_eq!(url_decode("hello%20world"), "hello world");
        assert_eq!(url_decode("%5B%5D"), "[]");
    }

    #[test]
    fn url_decode_multibyte_utf8() {
        // é = U+00E9 = UTF-8 bytes C3 A9
        assert_eq!(url_decode("%C3%A9"), "é");
        // ñ = U+00F1 = UTF-8 bytes C3 B1
        assert_eq!(url_decode("ja%C3%B1o"), "jaño");
        // 日 = U+65E5 = UTF-8 bytes E6 97 A5
        assert_eq!(url_decode("%E6%97%A5"), "日");
        // 🦀 = U+1F980 = UTF-8 bytes F0 9F A6 80
        assert_eq!(url_decode("%F0%9F%A6%80"), "🦀");
    }

    #[test]
    fn url_decode_invalid_utf8() {
        // Lone continuation byte → replacement character
        assert_eq!(url_decode("%80"), "\u{FFFD}");
        // Truncated sequence: C3 without continuation → replacement
        assert_eq!(url_decode("%C3x"), "\u{FFFD}x");
    }

    #[test]
    fn parse_form_data_basic() {
        let fields = parse_form_data("name=Alice&msg=Hello+World");
        assert_eq!(fields.get("name").unwrap(), "Alice");
        assert_eq!(fields.get("msg").unwrap(), "Hello World");
    }

    #[test]
    fn parse_form_data_preserves_duplicate_names() {
        let fields = parse_form_data("tag=one&tag=two&tag=three");
        assert_eq!(
            fields.get_all("tag").collect::<Vec<_>>(),
            vec!["one", "two", "three"]
        );
        assert_eq!(fields.get("tag").map(String::as_str), Some("three"));
    }

    #[test]
    fn parse_form_data_empty() {
        let fields = parse_form_data("");
        assert!(fields.is_empty() || fields.values().all(|v| v.is_empty()));
    }

    // ─── Board integration ─────────────────────────────────

    #[test]
    fn board_page_replaces_marker() {
        let dir = tempfile::tempdir().unwrap();
        let aml = dir.path().join("board.aml");
        std::fs::write(&aml, "[page mode=document]\n{{messages}}\n[/page]").unwrap();

        let stats = Arc::new(ServerStats {
            active_connections: Arc::new(AtomicUsize::new(0)),
            started_at: Instant::now(),
        });
        let handler = FileHandler::new(dir.path().to_path_buf(), stats);
        let peer: SocketAddr = "127.0.0.1:9999".parse().unwrap();
        let template = std::fs::read_to_string(&aml).unwrap();
        let rendered = handler.apply_plugins(&template, &aml, None, peer, None, None);

        assert!(!rendered.contains("{{messages}}"));
        assert!(rendered.contains("No messages yet"));
    }

    #[test]
    fn account_marker_reflects_session_identity() {
        let dir = tempfile::tempdir().unwrap();
        let aml = dir.path().join("account.aml");
        let template = "[page mode=document]{{account}}[/page]";
        std::fs::write(&aml, template).unwrap();
        let stats = Arc::new(ServerStats {
            active_connections: Arc::new(AtomicUsize::new(0)),
            started_at: Instant::now(),
        });
        let handler = FileHandler::new(dir.path().to_path_buf(), stats);
        let peer: SocketAddr = "127.0.0.1:9999".parse().unwrap();

        let anonymous = handler.apply_plugins(template, &aml, None, peer, None, None);
        assert!(anonymous.contains("/register"));
        assert!(anonymous.contains("/login"));
        assert!(!anonymous.contains("/logout"));

        let signed_in = handler.apply_plugins(template, &aml, None, peer, None, Some("alice"));
        assert!(signed_in.contains("alice"));
        assert!(signed_in.contains("/logout"));
        assert!(!signed_in.contains("/login"));
        assert!(!signed_in.contains("{{account}}"));
    }

    #[tokio::test]
    async fn board_input_stores_and_renders_message() {
        let (dir, addr) = setup_server().await;

        // Create a board page with the marker
        let board_path = dir.path().join("board.aml");
        std::fs::write(
            &board_path,
            "[page mode=document]\n{{messages}}\n[form action=\"/board\"][input name=\"name\" /][input name=\"msg\" /][button action=submit]Post[/button][/form]\n[/page]",
        ).unwrap();

        let mut conn = AtpConnection::connect_plain("127.0.0.1", addr.port())
            .await
            .unwrap();
        do_handshake(&mut conn).await;

        // Submit a message
        let input = InputMessage {
            path: "/board".into(),
            form_data: "name=alice&msg=Hello+board".into(),
            session: None,
        };
        let frame = RawFrame {
            msg_type: MessageType::Input,
            flags: 0,
            body: input.serialize().unwrap().into_bytes(),
        };
        conn.send_frame(&frame).await.unwrap();
        let response = conn.recv_frame().await.unwrap();
        assert_eq!(response.msg_type, MessageType::Page);
        let content = String::from_utf8(response.body).unwrap();

        // The page should contain our message
        assert!(content.contains("alice"), "should contain poster name");
        assert!(
            content.contains("Hello board"),
            "should contain message body"
        );
        assert!(
            !content.contains("{{messages}}"),
            "marker should be replaced"
        );
    }

    #[tokio::test]
    async fn board_input_escapes_injection() {
        let (dir, addr) = setup_server().await;

        let board_path = dir.path().join("board.aml");
        std::fs::write(
            &board_path,
            "[page mode=document]\n{{messages}}\n[form action=\"/board\"][input name=\"name\" /][input name=\"msg\" /][button action=submit]Post[/button][/form]\n[/page]",
        ).unwrap();

        let mut conn = AtpConnection::connect_plain("127.0.0.1", addr.port())
            .await
            .unwrap();
        do_handshake(&mut conn).await;

        // Try to inject AML tags
        let input = InputMessage {
            path: "/board".into(),
            form_data:
                "name=%5Bevil%5D&msg=%5Blink+href%3D%22atp%3A%2F%2Fbad%22%5Dclick%5B%2Flink%5D"
                    .into(),
            session: None,
        };
        let frame = RawFrame {
            msg_type: MessageType::Input,
            flags: 0,
            body: input.serialize().unwrap().into_bytes(),
        };
        conn.send_frame(&frame).await.unwrap();
        let response = conn.recv_frame().await.unwrap();
        let content = String::from_utf8(response.body).unwrap();

        // Brackets should be escaped in the output
        assert!(
            content.contains("[[evil]]"),
            "name brackets should be escaped"
        );
        assert!(
            content.contains("[[link"),
            "tag injection should be escaped"
        );
        // Verify no unescaped [link tag appears (check that every occurrence
        // of [link is preceded by another [, making it [[link)
        let has_raw_link = content
            .lines()
            .any(|line| line.contains("[link href=") && !line.contains("[[link href="));
        assert!(
            !has_raw_link,
            "raw link tag should not appear in user content"
        );
    }

    #[tokio::test]
    async fn board_rejects_empty_fields_with_error() {
        let (dir, addr) = setup_server().await;

        let board_path = dir.path().join("board.aml");
        std::fs::write(
            &board_path,
            "[page mode=document]\n{{messages}}\n[form action=\"/board\"][input name=\"name\" /][input name=\"msg\" /][button action=submit]Post[/button][/form]\n[/page]",
        ).unwrap();

        let mut conn = AtpConnection::connect_plain("127.0.0.1", addr.port())
            .await
            .unwrap();
        do_handshake(&mut conn).await;

        // Submit with empty message
        let input = InputMessage {
            path: "/board".into(),
            form_data: "name=alice&msg=".into(),
            session: None,
        };
        let frame = RawFrame {
            msg_type: MessageType::Input,
            flags: 0,
            body: input.serialize().unwrap().into_bytes(),
        };
        conn.send_frame(&frame).await.unwrap();
        let response = conn.recv_frame().await.unwrap();
        assert_eq!(response.msg_type, MessageType::Page);
        let content = String::from_utf8(response.body).unwrap();
        assert!(
            content.contains("Message is required"),
            "should show error: {content}"
        );
        assert!(
            content.contains("No messages yet"),
            "message should not have been posted"
        );
    }

    // ─── HN Plugin integration ────────────────────────────

    #[tokio::test]
    async fn hn_plugin_rejects_empty_fields_with_error() {
        let (dir, addr) = setup_server().await;

        let links_path = dir.path().join("links.aml");
        std::fs::write(
            &links_path,
            "[page mode=document]\n{{links}}\n\
             [form action=\"/links\"][input name=\"title\" /][input name=\"url\" /][button action=submit]Submit[/button][/form]\n\
             [/page]",
        ).unwrap();

        let mut conn = AtpConnection::connect_plain("127.0.0.1", addr.port())
            .await
            .unwrap();
        do_handshake(&mut conn).await;

        // Submit with missing title (authenticated via test-token → alice)
        let input = InputMessage {
            path: "/links".into(),
            form_data: "title=&url=example.com".into(),
            session: Some("test-token".into()),
        };
        let frame = RawFrame {
            msg_type: MessageType::Input,
            flags: 0,
            body: input.serialize().unwrap().into_bytes(),
        };
        conn.send_frame(&frame).await.unwrap();
        let response = conn.recv_frame().await.unwrap();
        assert_eq!(response.msg_type, MessageType::Page);
        let content = String::from_utf8(response.body).unwrap();
        assert!(
            content.contains("Title is required"),
            "should show error message: {content}"
        );
        assert!(
            content.contains("No links yet"),
            "link should not have been added"
        );
        // Previously entered values should be preserved in the form
        assert!(
            content.contains("value=\"example.com\""),
            "url field should be preserved in form: {content}"
        );
    }

    #[tokio::test]
    async fn hn_plugin_full_flow() {
        let (dir, addr) = setup_server().await;

        // Create a links page
        let links_path = dir.path().join("links.aml");
        std::fs::write(
            &links_path,
            "[page mode=document title=\"Links\"]\n\
             [heading level=1 fg=cyan]Link Aggregator[/heading]\n\
             {{links}}\n\
             [hr style=heavy fg=cyan /]\n\
             [box border=rounded fg=cyan w=55 title=\"Submit a Link\"]\n\
               [form action=\"/links\"]\n\
                 [text fg=cyan]Name:[/text]\n\
                 [input name=\"name\" placeholder=\"your handle\" maxlen=20 /]\n\
                 [spacer lines=1 /]\n\
                 [text fg=cyan]Title:[/text]\n\
                 [input name=\"title\" placeholder=\"link title\" maxlen=200 /]\n\
                 [spacer lines=1 /]\n\
                 [text fg=cyan]URL:[/text]\n\
                 [input name=\"url\" placeholder=\"https://...\" maxlen=500 /]\n\
                 [spacer lines=1 /]\n\
                 [button action=submit]Submit[/button]\n\
               [/form]\n\
             [/box]\n\
             [/page]",
        )
        .unwrap();

        let mut conn = AtpConnection::connect_plain("127.0.0.1", addr.port())
            .await
            .unwrap();
        do_handshake(&mut conn).await;

        // GET /links — should show empty state
        let response = send_get(&mut conn, "/links").await;
        let content = String::from_utf8(response.body).unwrap();
        assert!(content.contains("No links yet"), "empty state");

        // Submit a link (authenticated via test-token → alice)
        let input = InputMessage {
            path: "/links".into(),
            form_data: "title=Cool+Site&url=https%3A%2F%2Fexample.com".into(),
            session: Some("test-token".into()),
        };
        let frame = RawFrame {
            msg_type: MessageType::Input,
            flags: 0,
            body: input.serialize().unwrap().into_bytes(),
        };
        conn.send_frame(&frame).await.unwrap();
        let response = conn.recv_frame().await.unwrap();
        assert_eq!(response.msg_type, MessageType::Page);
        let content = String::from_utf8(response.body).unwrap();
        assert!(
            content.contains("Cool Site"),
            "submitted link should appear"
        );
        assert!(content.contains("alice"), "submitter name should appear");
        assert!(content.contains("example.com"), "domain should appear");
    }

    #[tokio::test]
    async fn hn_plugin_comment_via_query() {
        let (dir, addr) = setup_server().await;

        let links_path = dir.path().join("links.aml");
        std::fs::write(&links_path, "[page mode=document]\n{{links}}\n[/page]").unwrap();

        // Connection 1: submit a link (authenticated)
        let mut conn1 = AtpConnection::connect_plain("127.0.0.1", addr.port())
            .await
            .unwrap();
        do_handshake(&mut conn1).await;

        let input = InputMessage {
            path: "/links".into(),
            form_data: "title=Test&url=https%3A%2F%2Fexample.com".into(),
            session: Some("test-token".into()),
        };
        let frame = RawFrame {
            msg_type: MessageType::Input,
            flags: 0,
            body: input.serialize().unwrap().into_bytes(),
        };
        conn1.send_frame(&frame).await.unwrap();
        let _ = conn1.recv_frame().await.unwrap();

        // View item page via query (authenticated — should see comment form)
        let response =
            send_get_with_session(&mut conn1, "/links", Some("item=1"), Some("test-token")).await;
        let content = String::from_utf8(response.body).unwrap();
        assert!(content.contains("Test"), "item page should show title");
        assert!(
            content.contains("Add a Comment"),
            "should have comment form when logged in"
        );

        // Posting limits are keyed by IP, so reconnecting must not bypass the cooldown.
        tokio::time::sleep(Duration::from_secs(RATE_LIMIT_SECS)).await;

        // Connection 2: submit a comment as another authenticated user.
        let mut conn2 = AtpConnection::connect_plain("127.0.0.1", addr.port())
            .await
            .unwrap();
        do_handshake(&mut conn2).await;

        let comment_input = InputMessage {
            path: "/links?item=1".into(),
            form_data: "msg=Great+post!".into(),
            session: Some("test-token-2".into()),
        };
        let frame = RawFrame {
            msg_type: MessageType::Input,
            flags: 0,
            body: comment_input.serialize().unwrap().into_bytes(),
        };
        conn2.send_frame(&frame).await.unwrap();
        let response = conn2.recv_frame().await.unwrap();
        assert_eq!(response.msg_type, MessageType::Page);

        // View item page again — should show the comment
        let response =
            send_get_with_session(&mut conn1, "/links", Some("item=1"), Some("test-token")).await;
        let content = String::from_utf8(response.body).unwrap();
        assert!(content.contains("bob"), "comment author should appear");
        assert!(
            content.contains("Great post!"),
            "comment body should appear"
        );

        // Front page should show comment count
        let response = send_get(&mut conn1, "/links").await;
        let content = String::from_utf8(response.body).unwrap();
        assert!(
            content.contains("1 comment"),
            "front page should show comment count"
        );
    }

    // ─── extract_domain ────────────────────────────────────

    #[test]
    fn extract_domain_basic() {
        assert_eq!(extract_domain("https://example.com/page"), "example.com");
        assert_eq!(
            extract_domain("http://blog.example.com/neat"),
            "blog.example.com"
        );
        assert_eq!(extract_domain("atp://neon.city/board"), "neon.city");
    }

    #[test]
    fn extract_domain_no_path() {
        assert_eq!(extract_domain("https://example.com"), "example.com");
    }

    // ─── inject_form_values ─────────────────────────────────

    #[test]
    fn inject_form_values_basic() {
        let content =
            r#"[input name="name" placeholder="handle" /][input name="msg" placeholder="text" /]"#;
        let mut fields = FormData::default();
        fields.insert("name".into(), "alice".into());
        fields.insert("msg".into(), "hello world".into());

        let result = inject_form_values(content, &fields);
        assert!(
            result.contains(r#"value="alice""#),
            "name should be injected: {result}"
        );
        assert!(
            result.contains(r#"value="hello world""#),
            "msg should be injected: {result}"
        );
    }

    #[test]
    fn inject_form_values_escapes_quotes() {
        let content = r#"[input name="msg" placeholder="text" /]"#;
        let mut fields = FormData::default();
        fields.insert("msg".into(), r#"say "hi""#.into());

        let result = inject_form_values(content, &fields);
        assert!(
            result.contains(r#"value="say \"hi\"""#),
            "quotes should be escaped: {result}"
        );
    }

    #[test]
    fn inject_form_values_never_reflects_passwords() {
        let content = r#"[input name="password" password /][input name="note" /]"#;
        let fields = FormData {
            fields: vec![
                ("password".into(), "super-secret".into()),
                ("note".into(), "line\\break\nnext".into()),
            ],
        };
        let result = inject_form_values(content, &fields);
        assert!(!result.contains("super-secret"));
        assert!(result.contains(r#"value="line\\break\nnext""#));
    }

    #[test]
    fn input_routing_ignores_render_only_plugins_and_rejects_ambiguity() {
        let dir = tempfile::tempdir().unwrap();
        let stats = Arc::new(ServerStats {
            active_connections: Arc::new(AtomicUsize::new(0)),
            started_at: Instant::now(),
        });
        let handler = FileHandler::new(dir.path().to_path_buf(), stats);
        let peer: SocketAddr = "127.0.0.1:9999".parse().unwrap();

        std::fs::write(
            dir.path().join("board.aml"),
            "[page mode=document][pre]TITLE[/pre]{{messages}}[/page]",
        )
        .unwrap();
        let response = handler.handle_input("/board", None, "name=alice&msg=hello", peer, None);
        assert!(matches!(response, Response::Page { .. }));
        assert!(dir.path().join("board.board.tsv").exists());

        std::fs::write(
            dir.path().join("multi.aml"),
            "[page mode=document]{{messages}}{{chat}}[/page]",
        )
        .unwrap();
        let response = handler.handle_input("/multi", None, "name=a&msg=b", peer, None);
        let Response::Page { content, .. } = response else {
            panic!("ambiguous routing should return the page with an error");
        };
        assert!(content.contains("multiple form handlers"));
    }

    #[test]
    fn static_server_has_no_plugins_and_rejects_input() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.aml"), "[page mode=document /]").unwrap();
        let handler = FileHandler::new_static(dir.path().to_path_buf());
        assert!(handler.plugins.is_empty());
        let peer: SocketAddr = "127.0.0.1:9999".parse().unwrap();
        let response = handler.handle_input("/", None, "name=value", peer, None);
        assert!(matches!(response, Response::Error { code: 405, .. }));
    }

    // ─── poll-based live subscriptions ────────────────────

    #[tokio::test]
    async fn stats_page_wraps_in_live_region() {
        let dir = tempfile::tempdir().unwrap();
        let aml = dir.path().join("status.aml");
        std::fs::write(&aml, "[page mode=document]\n{{stats}}\n[/page]").unwrap();

        let stats = Arc::new(ServerStats {
            active_connections: Arc::new(AtomicUsize::new(0)),
            started_at: Instant::now(),
        });
        let handler = FileHandler::new(dir.path().to_path_buf(), stats);
        let peer: SocketAddr = "127.0.0.1:9999".parse().unwrap();
        let template = std::fs::read_to_string(&aml).unwrap();
        let rendered = handler.apply_plugins(&template, &aml, None, peer, None, None);

        assert!(!rendered.contains("{{stats}}"), "marker should be replaced");
        assert!(
            rendered.contains("[live id=\"__poll_stats\" endpoint=\"/__poll/stats\" scroll=none]"),
            "stats should be wrapped in [live]: {rendered}"
        );
        assert!(rendered.contains("[/live]"), "should have closing [/live]");
        assert!(
            rendered.contains("clients:"),
            "should contain clients label"
        );
        assert!(rendered.contains("uptime:"), "should contain uptime label");
        assert!(rendered.contains("clock:"), "should contain clock label");
        // No blank line between the [live] opening tag and the first stats row.
        assert!(
            rendered.contains("scroll=none][row gap=1]"),
            "no newline between [live] and content: {rendered}"
        );
    }

    #[tokio::test]
    async fn poll_subscribe_sends_updates() {
        let (dir, addr) = setup_server().await;

        // Create a page with the stats marker
        let status_path = dir.path().join("status.aml");
        std::fs::write(&status_path, "[page mode=document]\n{{stats}}\n[/page]").unwrap();

        let mut conn = AtpConnection::connect_plain("127.0.0.1", addr.port())
            .await
            .unwrap();
        do_handshake(&mut conn).await;

        // Fetch the page — should contain [live] wrapper
        let response = send_get(&mut conn, "/status").await;
        assert_eq!(response.msg_type, MessageType::Page);
        let content = String::from_utf8(response.body).unwrap();
        assert!(
            content.contains("__poll_stats"),
            "page should have poll live region: {content}"
        );

        // Subscribe to the poll endpoint
        let sub = SubscribeMessage {
            path: "/__poll/stats".into(),
            region: "__poll_stats".into(),
            mode: SubscribeMode::Replace,
            session: None,
        };
        let frame = RawFrame {
            msg_type: MessageType::Subscribe,
            flags: 0,
            body: sub.serialize().unwrap().into_bytes(),
        };
        conn.send_frame(&frame).await.unwrap();

        // Should receive initial UPDATE immediately
        let initial = tokio::time::timeout(Duration::from_secs(3), conn.recv_frame())
            .await
            .expect("should receive initial update within 3s")
            .expect("frame should parse");
        assert_eq!(initial.msg_type, MessageType::Update);
        let body = String::from_utf8(initial.body).unwrap();
        let update = UpdateMessage::parse(&body).unwrap();
        assert_eq!(update.region, "__poll_stats");
        assert!(
            update.content.contains("clock:"),
            "initial update should have clock: {}",
            update.content
        );

        // Wait for a periodic poll update (should arrive within ~2 seconds)
        let periodic = tokio::time::timeout(Duration::from_secs(3), conn.recv_frame())
            .await
            .expect("should receive periodic update within 3s")
            .expect("frame should parse");
        assert_eq!(periodic.msg_type, MessageType::Update);
        let body2 = String::from_utf8(periodic.body).unwrap();
        let update2 = UpdateMessage::parse(&body2).unwrap();
        assert_eq!(update2.region, "__poll_stats");
        assert!(
            update2.content.contains("clock:"),
            "periodic update should have clock"
        );
    }

    // ─── query passthrough ─────────────────────────────────

    #[tokio::test]
    async fn query_passed_to_handler() {
        let (dir, addr) = setup_server().await;

        let links_path = dir.path().join("links.aml");
        std::fs::write(&links_path, "[page mode=document]\n{{links}}\n[/page]").unwrap();

        let mut conn = AtpConnection::connect_plain("127.0.0.1", addr.port())
            .await
            .unwrap();
        do_handshake(&mut conn).await;

        // GET with query — should not error
        let response = send_get_with_query(&mut conn, "/links", Some("vote=999")).await;
        assert_eq!(response.msg_type, MessageType::Page);
        let content = String::from_utf8(response.body).unwrap();
        // Vote for non-existent link is a no-op, page should render fine
        assert!(content.contains("No links yet"));
    }

    #[tokio::test]
    async fn plaintext_auth_submission_is_rejected() {
        let (_dir, addr) = setup_server().await;
        let mut conn = AtpConnection::connect_plain("127.0.0.1", addr.port())
            .await
            .unwrap();
        do_handshake(&mut conn).await;

        let input = InputMessage {
            path: "/login".into(),
            form_data: "name=alice&password=testpass".into(),
            session: None,
        };
        conn.send_frame(&RawFrame {
            msg_type: MessageType::Input,
            flags: 0,
            body: input.serialize().unwrap().into_bytes(),
        })
        .await
        .unwrap();

        let response = conn.recv_frame().await.unwrap();
        assert_eq!(response.msg_type, MessageType::Error);
        let error = ErrorMessage::parse(&String::from_utf8(response.body).unwrap()).unwrap();
        assert_eq!(error.code, 403);
        assert_eq!(
            error.message.as_deref(),
            Some("Authentication requires TLS")
        );
    }
}
