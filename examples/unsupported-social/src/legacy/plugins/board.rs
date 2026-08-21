use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::super::{
    FormData, MAX_BODY_LEN, MAX_MESSAGES, MAX_NAME_LEN, PagePlugin, format_time_ago,
    sanitize_field, sanitize_user_content,
};

/// Colors to cycle through for message boxes.
const MSG_COLORS: &[&str] = &["cyan", "green", "yellow", "magenta", "white"];
const MSG_BRIGHT_COLORS: &[&str] = &[
    "bright-cyan",
    "bright-green",
    "bright-yellow",
    "bright-magenta",
    "bright-white",
];

/// A single stored board message.
struct BoardMessage {
    timestamp: u64,
    name: String,
    body: String,
}

/// Store of messages for a single board page.
struct MessageStore {
    messages: Vec<BoardMessage>,
    file_path: PathBuf,
}

impl MessageStore {
    /// Load messages from the storage file, or create empty if it doesn't exist.
    fn load(storage_path: PathBuf) -> Self {
        let messages = if storage_path.exists() {
            std::fs::read_to_string(&storage_path)
                .unwrap_or_default()
                .lines()
                .filter_map(|line| {
                    let mut parts = line.splitn(3, '\t');
                    let ts: u64 = parts.next()?.parse().ok()?;
                    let name = parts.next()?.to_string();
                    let body = parts.next()?.to_string();
                    Some(BoardMessage {
                        timestamp: ts,
                        name,
                        body,
                    })
                })
                .collect()
        } else {
            Vec::new()
        };
        MessageStore {
            messages,
            file_path: storage_path,
        }
    }

    /// Append a message and persist to disk. Trims old messages if over limit.
    fn append(&mut self, name: String, body: String) {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        self.messages.push(BoardMessage {
            timestamp,
            name,
            body,
        });

        // Trim oldest messages if over limit
        if self.messages.len() > MAX_MESSAGES {
            let excess = self.messages.len() - MAX_MESSAGES;
            self.messages.drain(..excess);
        }

        // Persist: rewrite the full file (simple, correct, bounded size)
        let content: String = self
            .messages
            .iter()
            .map(|m| format!("{}\t{}\t{}", m.timestamp, m.name, m.body))
            .collect::<Vec<_>>()
            .join("\n");
        let _ = crate::server::atomic_write(&self.file_path, &content);
    }

    /// Render all messages as AML, newest first.
    fn render_messages(&self) -> String {
        if self.messages.is_empty() {
            return "  [text dim align=center]No messages yet. Be the first to post![/text]\n"
                .to_string();
        }

        let mut out = String::new();
        for (i, msg) in self.messages.iter().rev().enumerate() {
            let color = MSG_COLORS[i % MSG_COLORS.len()];
            let bright = MSG_BRIGHT_COLORS[i % MSG_BRIGHT_COLORS.len()];
            let time_ago = format_time_ago(msg.timestamp);

            out.push_str(&format!(
                "  [box border=single fg={color}]\n\
                 \x20   [text bold fg={bright}]{name}[/text]\n\
                 \x20   [text dim]  {time_ago}[/text]\n\
                 \x20   [hr style=dot /]\n\
                 \x20   [text]{body}[/text]\n\
                 \x20 [/box]\n\
                 \x20 [spacer lines=1 /]\n\n",
                color = color,
                bright = bright,
                name = sanitize_user_content(&msg.name),
                time_ago = time_ago,
                body = sanitize_user_content(&msg.body),
            ));
        }
        out
    }
}

/// Message board plugin. Provides `{{messages}}` marker.
pub(crate) struct BoardPlugin {
    stores: HashMap<PathBuf, MessageStore>,
}

impl BoardPlugin {
    pub(crate) fn new() -> Self {
        BoardPlugin {
            stores: HashMap::new(),
        }
    }

    fn get_store(&mut self, aml_path: &Path) -> &mut MessageStore {
        self.stores
            .entry(aml_path.to_path_buf())
            .or_insert_with(|| {
                let storage_path = aml_path.with_extension("board.tsv");
                MessageStore::load(storage_path)
            })
    }
}

impl PagePlugin for BoardPlugin {
    fn marker(&self) -> &str {
        "{{messages}}"
    }

    fn input_key(&self) -> Option<&str> {
        Some("board")
    }

    fn render(
        &mut self,
        aml_path: &Path,
        _query: Option<&str>,
        _peer: SocketAddr,
        _param: Option<&str>,
        _site_root: &Path,
        _identity: Option<&str>,
    ) -> String {
        self.get_store(aml_path).render_messages()
    }

    fn handle_input(
        &mut self,
        aml_path: &Path,
        fields: &FormData,
        _query: Option<&str>,
        _identity: Option<&str>,
    ) -> Result<bool, String> {
        let raw_name = fields.get("name").map(|s| s.as_str()).unwrap_or("");
        let raw_msg = fields.get("msg").map(|s| s.as_str()).unwrap_or("");

        let name = sanitize_field(raw_name, MAX_NAME_LEN);
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
    fn message_store_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().join("test.board.tsv");

        let mut store = MessageStore::load(storage.clone());
        assert!(store.messages.is_empty());

        store.append("alice".into(), "hello world".into());
        store.append("bob".into(), "hi there".into());
        assert_eq!(store.messages.len(), 2);

        // Reload from disk
        let store2 = MessageStore::load(storage);
        assert_eq!(store2.messages.len(), 2);
        assert_eq!(store2.messages[0].name, "alice");
        assert_eq!(store2.messages[1].name, "bob");
    }

    #[test]
    fn message_store_max_messages() {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().join("test.board.tsv");

        let mut store = MessageStore::load(storage);
        for i in 0..MAX_MESSAGES + 10 {
            store.append(format!("user{i}"), format!("msg{i}"));
        }
        assert_eq!(store.messages.len(), MAX_MESSAGES);
        // Oldest should have been trimmed
        assert_eq!(store.messages[0].name, "user10");
    }

    #[test]
    fn message_store_render_escapes_content() {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().join("test.board.tsv");

        let mut store = MessageStore::load(storage);
        store.append("hacker".into(), "[text fg=red]injected[/text]".into());

        let rendered = store.render_messages();
        // Brackets in user content should be escaped
        assert!(rendered.contains("[[text fg=red]]injected[[/text]]"));
        // The escaped version should parse as literal text, not as a tag.
        // Verify the [text fg=red] does not appear as an unescaped tag
        // by checking no line has the exact pattern without doubled brackets.
        let has_raw_tag = rendered
            .lines()
            .any(|line| line.contains("[text fg=red]") && !line.contains("[[text fg=red]]"));
        assert!(!has_raw_tag, "user content should not produce raw AML tags");
    }
}
