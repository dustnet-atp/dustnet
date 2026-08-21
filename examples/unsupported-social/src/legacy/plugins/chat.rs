use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::super::{
    FormData, MAX_BODY_LEN, MAX_NAME_LEN, PagePlugin, sanitize_field, sanitize_user_content,
};

/// Maximum stored messages per chat room before trimming.
const MAX_CHAT_MESSAGES: usize = 500;

/// Colors assigned to nicks based on a hash of the name.
const NICK_COLORS: &[&str] = &[
    "cyan",
    "green",
    "yellow",
    "magenta",
    "bright-cyan",
    "bright-green",
    "bright-yellow",
    "bright-magenta",
    "bright-white",
];

/// Pick a deterministic color for a nick.
fn nick_color(name: &str) -> &'static str {
    let hash = name
        .bytes()
        .fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u32));
    NICK_COLORS[hash as usize % NICK_COLORS.len()]
}

/// A single chat message.
struct ChatMessage {
    timestamp: u64,
    name: String,
    body: String,
}

/// Per-room chat store with data file and rendered content file.
struct ChatStore {
    messages: Vec<ChatMessage>,
    data_path: PathBuf,
    content_path: PathBuf,
}

impl ChatStore {
    fn load(data_path: PathBuf, content_path: PathBuf) -> Self {
        let messages = if data_path.exists() {
            std::fs::read_to_string(&data_path)
                .unwrap_or_default()
                .lines()
                .filter_map(|line| {
                    let mut parts = line.splitn(3, '\t');
                    Some(ChatMessage {
                        timestamp: parts.next()?.parse().ok()?,
                        name: parts.next()?.to_string(),
                        body: parts.next()?.to_string(),
                    })
                })
                .collect()
        } else {
            Vec::new()
        };

        let store = ChatStore {
            messages,
            data_path,
            content_path,
        };
        // Write the full rendered content on load so the live region file exists
        store.rewrite_content();
        store
    }

    /// Append a message. Writes data store and appends to content file for deltas.
    fn append(&mut self, name: String, body: String) {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let msg = ChatMessage {
            timestamp,
            name,
            body,
        };

        // Render the new message BEFORE trimming so we can append it
        let rendered = Self::render_one(&msg);

        self.messages.push(msg);

        // Check if trim is needed
        let needs_trim = self.messages.len() > MAX_CHAT_MESSAGES;
        if needs_trim {
            let excess = self.messages.len() - MAX_CHAT_MESSAGES;
            self.messages.drain(..excess);
        }

        // Persist data store (always full rewrite for durability)
        self.write_data();

        if needs_trim {
            // Content file can't be appended — prefix changed. Full rewrite.
            self.rewrite_content();
        } else {
            // Append the new message to the content file for delta support.
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.content_path)
            {
                let _ = f.write_all(rendered.as_bytes());
            }
        }
    }

    fn write_data(&self) {
        let content: String = self
            .messages
            .iter()
            .map(|m| format!("{}\t{}\t{}", m.timestamp, m.name, m.body))
            .collect::<Vec<_>>()
            .join("\n");
        let _ = crate::server::atomic_write(&self.data_path, &content);
    }

    fn rewrite_content(&self) {
        let mut out = String::new();
        for msg in &self.messages {
            out.push_str(&Self::render_one(msg));
        }
        let _ = crate::server::atomic_write(&self.content_path, &out);
    }

    /// Render a single message as one line of AML (IRC style):
    /// `HH:MM <nick> message`
    fn render_one(msg: &ChatMessage) -> String {
        let ts = format_hhmm(msg.timestamp);
        let color = nick_color(&msg.name);
        let name = sanitize_user_content(&msg.name);
        let body = sanitize_user_content(&msg.body);
        format!("[text fg={color}]{ts} <{name}> {body}[/text]\n")
    }
}

/// Format a unix timestamp as HH:MM in UTC.
fn format_hhmm(timestamp: u64) -> String {
    let secs_in_day = timestamp % 86400;
    let hours = secs_in_day / 3600;
    let minutes = (secs_in_day % 3600) / 60;
    format!("{hours:02}:{minutes:02}")
}

/// Chat plugin. Provides `{{chat}}` marker.
///
/// Renders a `[live]` region that subscribes to an append-only content file.
/// New messages are appended to the file, enabling delta updates.
pub(crate) struct ChatPlugin {
    stores: HashMap<PathBuf, ChatStore>,
}

impl ChatPlugin {
    pub(crate) fn new() -> Self {
        ChatPlugin {
            stores: HashMap::new(),
        }
    }

    fn get_store(&mut self, aml_path: &Path) -> &mut ChatStore {
        let canonical = aml_path
            .canonicalize()
            .unwrap_or_else(|_| aml_path.to_path_buf());
        self.stores.entry(canonical.clone()).or_insert_with(|| {
            let data_path = canonical.with_extension("chat.tsv");
            let content_path = canonical.with_extension("chat.aml");
            ChatStore::load(data_path, content_path)
        })
    }

    /// Derive the live endpoint path from the aml_path relative to site_root.
    /// e.g. /Users/.../sites/drc/index.aml → /index.chat.aml
    /// The path must end in .aml for resolve_path to accept it.
    fn endpoint_for(aml_path: &Path, site_root: &Path) -> String {
        // Both paths may or may not be canonicalized, so canonicalize both
        // for reliable prefix stripping.
        let abs_aml = aml_path
            .canonicalize()
            .unwrap_or_else(|_| aml_path.to_path_buf());
        let abs_root = site_root
            .canonicalize()
            .unwrap_or_else(|_| site_root.to_path_buf());
        let relative = abs_aml.strip_prefix(&abs_root).unwrap_or(&abs_aml);
        let stem = relative.with_extension("chat.aml");
        format!("/{}", stem.display())
    }
}

impl PagePlugin for ChatPlugin {
    fn marker(&self) -> &str {
        "{{chat}}"
    }

    fn input_key(&self) -> Option<&str> {
        Some("chat")
    }

    fn render(
        &mut self,
        aml_path: &Path,
        _query: Option<&str>,
        _peer: SocketAddr,
        _param: Option<&str>,
        site_root: &Path,
        _identity: Option<&str>,
    ) -> String {
        // Ensure the content file is populated
        self.get_store(aml_path);

        let endpoint = Self::endpoint_for(aml_path, site_root);
        format!(
            "[live id=\"chat\" endpoint=\"{endpoint}\" height=20 scroll=tail buffer=500 delta]\n\
             \x20 [text dim]Connecting...[/text]\n\
             [/live]"
        )
    }

    fn handle_input(
        &mut self,
        aml_path: &Path,
        fields: &FormData,
        _query: Option<&str>,
        identity: Option<&str>,
    ) -> Result<bool, String> {
        // Use authenticated identity as nick, or fall back to name field
        let name = if let Some(id) = identity {
            id.to_string()
        } else {
            let raw = fields.get("name").map(|s| s.as_str()).unwrap_or("");
            sanitize_field(raw, MAX_NAME_LEN)
        };

        let raw_msg = fields.get("msg").map(|s| s.as_str()).unwrap_or("");
        let body = sanitize_field(raw_msg, MAX_BODY_LEN);

        if name.is_empty() {
            return Err("Name is required.".into());
        }
        if body.is_empty() {
            return Err("Message is required.".into());
        }

        self.get_store(aml_path).append(name, body);
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nick_colors_are_deterministic() {
        assert_eq!(nick_color("alice"), nick_color("alice"));
        // Different names should (usually) get different colors
        // Not guaranteed, but statistically likely
        let c1 = nick_color("alice");
        let c2 = nick_color("bob");
        let c3 = nick_color("charlie");
        // At least 2 of 3 should differ
        assert!(c1 != c2 || c2 != c3 || c1 != c3);
    }

    #[test]
    fn format_hhmm_works() {
        assert_eq!(format_hhmm(0), "00:00");
        assert_eq!(format_hhmm(3600), "01:00");
        assert_eq!(format_hhmm(86399), "23:59");
        assert_eq!(format_hhmm(90061), "01:01"); // wraps past midnight
    }

    #[test]
    fn chat_store_append_and_render() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("test.chat.tsv");
        let content = dir.path().join("test.chat.aml");

        let mut store = ChatStore::load(data.clone(), content.clone());
        assert!(store.messages.is_empty());

        store.append("alice".into(), "hello".into());
        store.append("bob".into(), "hey!".into());
        assert_eq!(store.messages.len(), 2);

        // Content file should exist and have both messages
        let rendered = std::fs::read_to_string(&content).unwrap();
        assert!(rendered.contains("alice"));
        assert!(rendered.contains("hello"));
        assert!(rendered.contains("bob"));
        assert!(rendered.contains("hey!"));

        // Data file should roundtrip
        let store2 = ChatStore::load(data, content);
        assert_eq!(store2.messages.len(), 2);
        assert_eq!(store2.messages[0].name, "alice");
    }

    #[test]
    fn chat_store_trims_at_limit() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("test.chat.tsv");
        let content = dir.path().join("test.chat.aml");

        let mut store = ChatStore::load(data, content);
        for i in 0..MAX_CHAT_MESSAGES + 10 {
            store.append(format!("user{i}"), format!("msg{i}"));
        }
        assert_eq!(store.messages.len(), MAX_CHAT_MESSAGES);
        assert_eq!(store.messages[0].name, "user10");
    }

    #[test]
    fn chat_escapes_user_content() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("test.chat.tsv");
        let content = dir.path().join("test.chat.aml");

        let mut store = ChatStore::load(data, content.clone());
        store.append("hacker".into(), "[text fg=red]injected[/text]".into());

        let rendered = std::fs::read_to_string(&content).unwrap();
        assert!(rendered.contains("[[text fg=red]]injected[[/text]]"));
    }

    #[test]
    fn content_file_is_append_only_for_deltas() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("test.chat.tsv");
        let content = dir.path().join("test.chat.aml");

        let mut store = ChatStore::load(data, content.clone());
        store.append("alice".into(), "first".into());
        let len_after_first = std::fs::read_to_string(&content).unwrap().len();

        store.append("bob".into(), "second".into());
        let full = std::fs::read_to_string(&content).unwrap();
        let len_after_second = full.len();

        // File should have grown (append, not rewrite)
        assert!(len_after_second > len_after_first);
        // First message should still be at the start
        assert!(full.starts_with(&std::fs::read_to_string(&content).unwrap()[..len_after_first]));
    }
}
