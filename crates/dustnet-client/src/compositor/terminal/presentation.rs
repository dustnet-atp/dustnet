//! Terminal presentation state: viewport, input modes, and command line.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::fmt;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::compositor::layout::cell::CellBuffer;
use crate::compositor::layout::text::WidthConfig;
use crate::compositor::scene::Scene;

use super::rendering::{next_char_boundary, previous_char_boundary};

// ─── Viewer state machine ───────────────────────────────────

/// Viewer state — manages scroll position, viewport, and content
/// dimensions. Focus no longer lives here: `Scene.focus` (a
/// `NodeId`) is the sole authority. The list-index into
/// `page.focusables` is derived at render time by matching
/// `scene.focus` against each element's `node_id`.
///
/// Extracted from the viewer loop so it can be unit tested without
/// terminal I/O.
#[derive(Debug, Clone)]
pub(super) struct ViewportState {
    /// Current scroll offset (first visible row of content).
    pub scroll_offset: u16,
    /// Terminal width.
    pub term_w: u16,
    /// Terminal height.
    pub term_h: u16,
    /// Total content height in rows.
    pub content_height: u16,
    /// Rows reserved at the bottom of the viewport for sticky content.
    pub sticky_bottom_height: u16,
}

const CLIENT_HUD_ANIMATION_MS: f32 = 180.0;
pub(super) const MAX_ERROR_ENTRIES: usize = 64;
pub(super) const MAX_ERROR_MESSAGE_CHARS: usize = 512;
pub(super) const MAX_ERROR_MESSAGE_WIDTH: usize = 512;
pub(super) const MAX_COMMAND_BYTES: usize = 2_048;
pub(super) const MAX_COMMAND_HISTORY: usize = 64;
pub(super) const MAX_COMMAND_MESSAGE_BYTES: usize = 2_048;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ClientHudTab {
    History,
    Errors,
    Sessions,
}

/// One remembered session, as the HUD shows it.
///
/// Without the token, deliberately. The store file warns that anyone who can
/// read it is logged in as you; putting a token on screen would make that true
/// of anyone who can see the terminal, and the HUD is a surface people open in
/// front of other people.
#[derive(Debug, Clone)]
pub(super) struct SessionRow {
    pub(super) origin: String,
    pub(super) security: String,
    pub(super) scope: String,
    /// Seconds until expiry; negative means it has already passed.
    pub(super) expires_in: Option<i64>,
    /// Whether this one survives the client exiting.
    pub(super) persistent: bool,
}

#[derive(Debug, PartialEq)]
pub(super) enum ClientHudAction {
    None,
    Redraw,
    OpenHistory(usize),
    ClearErrors,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct ErrorEntry {
    pub(super) message: String,
    pub(super) count: u64,
}

/// Bounded, client-session error history. Messages are sanitized before they
/// reach terminal chrome, identical messages share one row, and excess unique
/// messages are counted without allowing a remote site to grow memory forever.
pub(super) struct ErrorLog {
    pub(super) entries: arrayvec::ArrayVec<ErrorEntry, MAX_ERROR_ENTRIES>,
    total_count: u64,
    omitted_count: u64,
    ever_reported: bool,
}

impl ErrorLog {
    pub(super) fn new() -> Self {
        Self {
            entries: arrayvec::ArrayVec::new(),
            total_count: 0,
            omitted_count: 0,
            ever_reported: false,
        }
    }

    /// Record one runtime failure. Returns the stored row and whether this is
    /// the first failure of the client session (the one that opens the HUD).
    pub(super) fn record(&mut self, message: &str) -> (Option<usize>, bool) {
        let first = !self.ever_reported;
        self.ever_reported = true;
        self.total_count = self.total_count.saturating_add(1);

        let Some(message) = try_sanitize_line(
            message,
            MAX_ERROR_MESSAGE_CHARS.saturating_mul(4),
            MAX_ERROR_MESSAGE_CHARS,
            MAX_ERROR_MESSAGE_WIDTH,
        ) else {
            self.omitted_count = self.omitted_count.saturating_add(1);
            return (None, first);
        };
        if let Some(index) = self
            .entries
            .iter()
            .position(|entry| entry.message == message)
        {
            if let Some(entry) = self.entries.get_mut(index) {
                entry.count = entry.count.saturating_add(1);
            }
            return (Some(index), first);
        }

        if self.entries.len() == MAX_ERROR_ENTRIES {
            self.omitted_count = self.omitted_count.saturating_add(1);
            return (None, first);
        }

        let result = self.entries.try_push(ErrorEntry { message, count: 1 });
        assert!(
            result.is_ok(),
            "checked error-table bound must leave a slot"
        );
        (Some(self.entries.len() - 1), first)
    }

    pub(super) fn clear(&mut self) {
        self.entries.clear();
        self.total_count = 0;
        self.omitted_count = 0;
    }

    pub(super) fn total_count(&self) -> u64 {
        self.total_count
    }

    pub(super) fn omitted_count(&self) -> u64 {
        self.omitted_count
    }
}

struct FallibleLine {
    value: String,
    max_bytes: usize,
    max_chars: usize,
    chars: usize,
    truncated: bool,
}

impl fmt::Write for FallibleLine {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        if self.truncated {
            return Ok(());
        }
        for mut ch in text.chars() {
            if matches!(ch, '\n' | '\t') {
                ch = ' ';
            }
            if self.chars == self.max_chars
                || self
                    .value
                    .len()
                    .checked_add(ch.len_utf8())
                    .is_none_or(|bytes| bytes > self.max_bytes)
            {
                self.truncated = true;
                return Ok(());
            }
            self.value
                .try_reserve_exact(ch.len_utf8())
                .map_err(|_| fmt::Error)?;
            self.value.push(ch);
            self.chars += 1;
        }
        Ok(())
    }
}

fn try_sanitize_line(
    input: &str,
    max_bytes: usize,
    max_chars: usize,
    max_width: usize,
) -> Option<String> {
    let mut output = FallibleLine {
        value: String::new(),
        max_bytes,
        max_chars,
        chars: 0,
        truncated: false,
    };
    crate::scanner::escape::sanitize_into(input, &mut output).ok()?;
    let mut width = 0usize;
    let mut byte_end = 0usize;
    for grapheme in output.value.graphemes(true) {
        let grapheme_width = UnicodeWidthStr::width(grapheme);
        if width
            .checked_add(grapheme_width)
            .is_none_or(|next| next > max_width)
        {
            break;
        }
        width += grapheme_width;
        byte_end += grapheme.len();
    }
    output.value.truncate(byte_end);
    Some(output.value)
}

/// Client-owned drop-down HUD. `progress` is retained when direction changes,
/// so pressing backtick mid-animation reverses smoothly instead of jumping.
pub(super) struct ClientHud {
    pub(super) progress: f32,
    pub(super) target_open: bool,
    pub(super) tab: ClientHudTab,
    pub(super) history_selected: usize,
    pub(super) error_selected: usize,
    pub(super) session_selected: usize,
    pub(super) last_tick: std::time::Instant,
}

impl ClientHud {
    pub(super) fn new() -> Self {
        Self {
            progress: 0.0,
            target_open: false,
            tab: ClientHudTab::History,
            history_selected: 0,
            error_selected: 0,
            session_selected: 0,
            last_tick: std::time::Instant::now(),
        }
    }

    pub(super) fn is_active(&self) -> bool {
        self.target_open || self.progress > 0.0
    }

    pub(super) fn is_animating(&self) -> bool {
        (self.target_open && self.progress < 1.0) || (!self.target_open && self.progress > 0.0)
    }

    pub(super) fn toggle(&mut self, current: usize, history_len: usize) {
        if !self.is_active() {
            self.history_selected = current.min(history_len.saturating_sub(1));
        }
        self.target_open = !self.target_open;
        self.last_tick = std::time::Instant::now();
    }

    pub(super) fn open_errors(&mut self, selected: Option<usize>) {
        self.tab = ClientHudTab::Errors;
        if let Some(selected) = selected {
            self.error_selected = selected;
        }
        self.target_open = true;
        self.last_tick = std::time::Instant::now();
    }

    pub(super) fn close(&mut self) {
        self.target_open = false;
        self.last_tick = std::time::Instant::now();
    }

    /// Advance the slide animation. Returns true when its visible extent changed.
    pub(super) fn tick(&mut self) -> bool {
        if !self.is_animating() {
            self.last_tick = std::time::Instant::now();
            return false;
        }
        let now = std::time::Instant::now();
        let delta =
            now.duration_since(self.last_tick).as_secs_f32() * 1000.0 / CLIENT_HUD_ANIMATION_MS;
        self.last_tick = now;
        let previous = self.progress;
        if self.target_open {
            self.progress = (self.progress + delta).min(1.0);
        } else {
            self.progress = (self.progress - delta).max(0.0);
        }
        (self.progress - previous).abs() > f32::EPSILON
    }

    pub(super) fn target_height(viewport_h: u16) -> u16 {
        if viewport_h < 8 {
            viewport_h
        } else {
            (viewport_h / 2).clamp(8, 16)
        }
    }

    pub(super) fn visible_rows(&self, viewport_h: u16) -> u16 {
        let target = Self::target_height(viewport_h) as f32;
        // Cubic ease-out gives the panel the fast, weighty Quake-console drop.
        let eased = 1.0 - (1.0 - self.progress).powi(3);
        (target * eased).ceil() as u16
    }

    pub(super) fn handle_key(
        &mut self,
        code: KeyCode,
        history_len: usize,
        error_len: usize,
        session_len: usize,
    ) -> ClientHudAction {
        match code {
            KeyCode::Char('`') => {
                self.target_open = !self.target_open;
                self.last_tick = std::time::Instant::now();
                ClientHudAction::Redraw
            }
            KeyCode::Esc => {
                self.close();
                ClientHudAction::Redraw
            }
            KeyCode::Tab => {
                self.tab = match self.tab {
                    ClientHudTab::History => ClientHudTab::Errors,
                    ClientHudTab::Errors => ClientHudTab::Sessions,
                    ClientHudTab::Sessions => ClientHudTab::History,
                };
                ClientHudAction::Redraw
            }
            KeyCode::BackTab => {
                self.tab = match self.tab {
                    ClientHudTab::History => ClientHudTab::Sessions,
                    ClientHudTab::Errors => ClientHudTab::History,
                    ClientHudTab::Sessions => ClientHudTab::Errors,
                };
                ClientHudAction::Redraw
            }
            KeyCode::Left | KeyCode::Char('h') => {
                self.tab = match self.tab {
                    ClientHudTab::History => ClientHudTab::Sessions,
                    ClientHudTab::Errors => ClientHudTab::History,
                    ClientHudTab::Sessions => ClientHudTab::Errors,
                };
                ClientHudAction::Redraw
            }
            KeyCode::Right | KeyCode::Char('l') => {
                self.tab = match self.tab {
                    ClientHudTab::History => ClientHudTab::Errors,
                    ClientHudTab::Errors => ClientHudTab::Sessions,
                    ClientHudTab::Sessions => ClientHudTab::History,
                };
                ClientHudAction::Redraw
            }
            KeyCode::Up | KeyCode::Char('k') => {
                match self.tab {
                    ClientHudTab::History => {
                        self.history_selected = self.history_selected.saturating_sub(1)
                    }
                    ClientHudTab::Errors => {
                        self.error_selected = self.error_selected.saturating_sub(1)
                    }
                    ClientHudTab::Sessions => {
                        self.session_selected = self.session_selected.saturating_sub(1)
                    }
                }
                ClientHudAction::Redraw
            }
            KeyCode::Down | KeyCode::Char('j') => {
                match self.tab {
                    ClientHudTab::History if history_len > 0 => {
                        self.history_selected = (self.history_selected + 1).min(history_len - 1);
                    }
                    ClientHudTab::Errors if error_len > 0 => {
                        self.error_selected = (self.error_selected + 1).min(error_len - 1);
                    }
                    ClientHudTab::Sessions if session_len > 0 => {
                        self.session_selected = (self.session_selected + 1).min(session_len - 1);
                    }
                    _ => {}
                }
                ClientHudAction::Redraw
            }
            KeyCode::Home => {
                match self.tab {
                    ClientHudTab::History => self.history_selected = 0,
                    ClientHudTab::Errors => self.error_selected = 0,
                    ClientHudTab::Sessions => self.session_selected = 0,
                }
                ClientHudAction::Redraw
            }
            KeyCode::End => {
                match self.tab {
                    ClientHudTab::History => self.history_selected = history_len.saturating_sub(1),
                    ClientHudTab::Errors => self.error_selected = error_len.saturating_sub(1),
                    ClientHudTab::Sessions => self.session_selected = session_len.saturating_sub(1),
                }
                ClientHudAction::Redraw
            }
            KeyCode::Enter if self.tab == ClientHudTab::History && history_len > 0 => {
                self.close();
                ClientHudAction::OpenHistory(self.history_selected.min(history_len - 1))
            }
            KeyCode::Char('c') if self.tab == ClientHudTab::Errors && error_len > 0 => {
                self.error_selected = 0;
                ClientHudAction::ClearErrors
            }
            _ => ClientHudAction::None,
        }
    }
}

/// Load a page from cached AML content (no network fetch).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(super) enum ViewportEvent {
    Input(KeyEvent),
    Resize { width: u16, height: u16 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ViewportEffect {
    Action(ViewerAction),
    Relayout { width: u16, height: u16 },
}

/// Derive the list-index of the currently focused element by matching
/// `Scene.focus` against `focusables[i].node_id`. Returns `None` when
/// the scene has no focus set or the focused node isn't in the
/// visible focusables list (e.g. inactive panel state, closed
/// details body — both are correctly filtered by
/// `collect_focusables_from_scene`).
pub(super) fn current_focus_index(
    scene: &Scene,
    focusables: &[crate::compositor::panels::FocusableElement],
) -> Option<usize> {
    let focused = scene.focus()?;
    focusables.iter().position(|f| f.node_id == focused)
}

/// Action returned by ViewportState methods to tell the viewer loop what to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(super) enum ViewerAction {
    /// Redraw the screen.
    Redraw,
    /// Nothing changed.
    None,
    /// Quit the viewer.
    Quit,
    /// Terminal was resized — re-layout needed.
    Resize { width: u16, height: u16 },
    /// Activate the currently focused element (Enter key).
    Activate,
    /// Focus moved to a different element (Tab / Shift-Tab) — scroll to keep it visible.
    FocusChanged,
    /// Tab pressed — viewer loop should advance focus to next focusable.
    FocusNext,
    /// Shift-Tab pressed — viewer loop should move focus to previous focusable.
    FocusPrev,
    /// Navigate back in history (Alt+Left / H).
    GoBack,
    /// Navigate forward in history (Right / l).
    GoForward,
    /// Tab pressed in input mode — advance focus to next element.
    TabNext,
    /// Shift-Tab pressed in input mode — move focus to previous element.
    TabPrev,
    /// Enter command line mode (`:` pressed).
    EnterCommandMode,
    /// Enter command line with `:open ` pre-filled (`o` pressed).
    EnterCommandModeOpen,
    /// Show the client help modal (`?` pressed).
    ShowHelp,
    /// Toggle the tabbed client-owned HUD (backtick pressed).
    ShowHud,
    /// Jump directly to a cached browser-history entry selected in the HUD.
    JumpHistory(usize),
    /// Remove focus from the current control (`Esc` pressed).
    ClearFocus,
    /// Reload the current page (`r` pressed).
    Reload,
}

// ─── Command line ───────────────────────────────────────────

/// Command line mode.
#[derive(Debug, PartialEq)]
pub(super) enum CommandLineMode {
    /// Idle / blank.
    Idle,
    /// User is typing a command.
    Input,
    /// Displaying a transient message.
    Message,
}

/// A parsed command from the command line.
pub(super) enum ParsedCommand {
    /// Navigate to a URI: `:o <uri>` or `:open <uri>`
    Open(String),
    /// Reload current page: `:r` or `:reload`
    Reload,
    /// Quit: `:q` or `:quit`
    Quit,
    /// Show active sessions: `:sessions`
    Sessions,
    /// Clear all sessions or sessions for a site: `:sessions clear [site]`
    SessionsClear(Option<String>),
    /// Show the client help modal: `:help`
    Help,
    /// Unrecognized command.
    Unknown(String),
}

pub(super) fn parse_command(input: &str) -> ParsedCommand {
    let trimmed = input.trim();
    if let Some(rest) = trimmed
        .strip_prefix("o ")
        .or_else(|| trimmed.strip_prefix("open "))
    {
        let uri = rest.trim();
        if !uri.is_empty() {
            return ParsedCommand::Open(uri.to_string());
        }
    }
    match trimmed {
        "q" | "quit" => ParsedCommand::Quit,
        "r" | "reload" => ParsedCommand::Reload,
        "sessions" | "s" => ParsedCommand::Sessions,
        "h" | "help" => ParsedCommand::Help,
        _ if trimmed.starts_with("sessions clear") || trimmed.starts_with("s clear") => {
            let Some(rest) = trimmed
                .strip_prefix("sessions clear")
                .or_else(|| trimmed.strip_prefix("s clear"))
            else {
                return ParsedCommand::Unknown(trimmed.to_string());
            };
            let rest = rest.trim();
            if rest.is_empty() {
                ParsedCommand::SessionsClear(None)
            } else {
                ParsedCommand::SessionsClear(Some(rest.to_string()))
            }
        }
        _ => ParsedCommand::Unknown(trimmed.to_string()),
    }
}

/// Vim-style command line, rendered as the last terminal row.
pub(super) struct CommandLine {
    pub(super) mode: CommandLineMode,
    /// Text buffer (excludes the leading `:` which is drawn separately).
    pub(super) buffer: String,
    /// Cursor position within `buffer`.
    pub(super) cursor: usize,
    /// Message to display (for Message mode).
    pub(super) message: String,
    /// Whether the message is an error (rendered in red) or info.
    pub(super) is_error: bool,
    /// Command history (oldest first).
    pub(super) history: arrayvec::ArrayVec<String, MAX_COMMAND_HISTORY>,
    /// Current position in history. `history.len()` means the live buffer.
    pub(super) history_index: usize,
    /// Saved live buffer when browsing history (restored on Down past end).
    pub(super) saved_buffer: String,
}

#[cfg(test)]
thread_local! {
    static COMMAND_HISTORY_COPY_REJECTION: std::cell::Cell<Option<usize>> = const {
        std::cell::Cell::new(None)
    };
}

#[cfg(test)]
pub(super) fn reject_command_history_copy_after(successful_copies: usize) {
    COMMAND_HISTORY_COPY_REJECTION.with(|site| site.set(Some(successful_copies)));
}

fn try_copy_command(value: &str) -> Option<String> {
    #[cfg(test)]
    if COMMAND_HISTORY_COPY_REJECTION.with(|site| match site.get() {
        Some(0) => {
            site.set(None);
            true
        }
        Some(remaining) => {
            site.set(Some(remaining - 1));
            false
        }
        None => false,
    }) {
        return None;
    }

    let mut copy = String::new();
    copy.try_reserve_exact(value.len()).ok()?;
    copy.push_str(value);
    Some(copy)
}

impl CommandLine {
    pub(super) fn new() -> Self {
        CommandLine {
            mode: CommandLineMode::Idle,
            buffer: String::new(),
            cursor: 0,
            message: String::new(),
            is_error: false,
            history: arrayvec::ArrayVec::new(),
            history_index: 0,
            saved_buffer: String::new(),
        }
    }

    /// Enter command input mode with optional pre-filled text.
    pub(super) fn activate(&mut self, prefill: &str) {
        self.mode = CommandLineMode::Input;
        self.buffer.clear();
        let bounded_len = floor_char_boundary(prefill, prefill.len().min(MAX_COMMAND_BYTES));
        if self.buffer.try_reserve_exact(bounded_len).is_ok() {
            self.buffer.push_str(&prefill[..bounded_len]);
        }
        self.cursor = self.buffer.len();
        self.message.clear();
        self.history_index = self.history.len();
        self.saved_buffer.clear();
    }

    /// Cancel command input, return to idle.
    pub(super) fn cancel(&mut self) {
        self.mode = CommandLineMode::Idle;
        self.buffer.clear();
        self.cursor = 0;
        self.history_index = self.history.len();
        self.saved_buffer.clear();
    }

    /// Set a transient message (info or error).
    pub(super) fn set_message(&mut self, msg: &str, is_error: bool) {
        let Some(message) = try_sanitize_line(
            msg,
            MAX_COMMAND_MESSAGE_BYTES,
            MAX_COMMAND_MESSAGE_BYTES,
            usize::MAX,
        ) else {
            return;
        };
        self.mode = CommandLineMode::Message;
        self.message = message;
        self.is_error = is_error;
    }

    /// Format and sanitize a bounded message without constructing an
    /// unbounded intermediate `String` at the call site.
    pub(super) fn set_message_args(&mut self, args: fmt::Arguments<'_>, is_error: bool) {
        let mut raw = FallibleLine {
            value: String::new(),
            max_bytes: MAX_COMMAND_MESSAGE_BYTES,
            max_chars: MAX_COMMAND_MESSAGE_BYTES,
            chars: 0,
            truncated: false,
        };
        if fmt::Write::write_fmt(&mut raw, args).is_err() {
            return;
        }
        self.set_message(&raw.value, is_error);
    }

    /// Clear any displayed message on keypress. Returns true if it was showing.
    pub(super) fn clear_message_if_needed(&mut self) -> bool {
        if self.mode == CommandLineMode::Message {
            self.mode = CommandLineMode::Idle;
            self.message.clear();
            true
        } else {
            false
        }
    }

    /// Handle a key event while in Input mode. Returns a parsed command on Enter.
    pub(super) fn handle_key(&mut self, code: KeyCode) -> Option<ParsedCommand> {
        match code {
            KeyCode::Esc => {
                self.cancel();
                None
            }
            KeyCode::Enter => {
                let input = self.buffer.clone();
                let cmd = parse_command(&input);
                // Push to history if non-empty and not a duplicate of the last entry
                let trimmed = input.trim();
                if !trimmed.is_empty() && self.history.last().is_none_or(|last| last != trimmed) {
                    let mut entry = String::new();
                    if entry.try_reserve_exact(trimmed.len()).is_ok() {
                        entry.push_str(trimmed);
                        if self.history.is_full() {
                            self.history.remove(0);
                        }
                        let result = self.history.try_push(entry);
                        assert!(
                            result.is_ok(),
                            "history eviction must leave one inline slot"
                        );
                    }
                }
                self.cancel();
                Some(cmd)
            }
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    let previous = previous_char_boundary(&self.buffer, self.cursor);
                    self.buffer.drain(previous..self.cursor);
                    self.cursor = previous;
                }
                None
            }
            KeyCode::Left => {
                if self.cursor > 0 {
                    self.cursor = previous_char_boundary(&self.buffer, self.cursor);
                }
                None
            }
            KeyCode::Right => {
                if self.cursor < self.buffer.len() {
                    self.cursor = next_char_boundary(&self.buffer, self.cursor);
                }
                None
            }
            KeyCode::Up => {
                if let Some(target) = self.history_index.checked_sub(1)
                    && let Some(entry) = self.history.get(target)
                {
                    let saved_buffer = if self.history_index == self.history.len() {
                        Some(try_copy_command(&self.buffer)?)
                    } else {
                        None
                    };
                    let buffer = try_copy_command(entry)?;
                    if let Some(saved) = saved_buffer {
                        self.saved_buffer = saved;
                    }
                    self.history_index = target;
                    self.buffer = buffer;
                    self.cursor = self.buffer.len();
                }
                None
            }
            KeyCode::Down => {
                if self.history_index < self.history.len() {
                    let target = self.history_index + 1;
                    let source = match self.history.get(target) {
                        Some(entry) if target != self.history.len() => entry,
                        _ => &self.saved_buffer,
                    };
                    let buffer = try_copy_command(source)?;
                    self.history_index = target;
                    self.buffer = buffer;
                    self.cursor = self.buffer.len();
                }
                None
            }
            KeyCode::Char(ch) => {
                let additional = ch.len_utf8();
                if self
                    .buffer
                    .len()
                    .checked_add(additional)
                    .is_some_and(|len| len <= MAX_COMMAND_BYTES)
                    && self.buffer.try_reserve(additional).is_ok()
                {
                    self.buffer.insert(self.cursor, ch);
                    self.cursor += additional;
                }
                None
            }
            _ => None,
        }
    }
}

fn floor_char_boundary(value: &str, mut index: usize) -> usize {
    while !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

// ─── Form input mode ────────────────────────────────────────

/// Input mode state for form field editing.
pub(super) struct InputMode {
    pub(super) active: bool,
    /// Current cursor position within the field value.
    pub(super) cursor_pos: usize,
    /// Value of the one field currently being edited. Authoritative state is
    /// mirrored into the current scene node through `SetInputValue` patches.
    pub(super) current_value: String,
    /// Scene node that owns the currently edited field.
    pub(super) current_node: Option<crate::compositor::scene::NodeId>,
    /// Max length for the current field.
    pub(super) maxlen: u32,
    /// Whether the current field is a password.
    pub(super) password: bool,
    /// Row and col of the field in the buffer.
    pub(super) field_col: u16,
    pub(super) field_row: u16,
    /// Whether the current field is in the sticky buffer.
    pub(super) field_is_sticky: bool,
    /// Width policy used to place the terminal cursor after graphemes.
    pub(super) wcfg: WidthConfig,
}

pub(super) const MAX_INPUT_VALUE_BYTES: usize = crate::protocol::MAX_INPUT_MESSAGE_SIZE;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum InputValueAllocationSite {
    Activation,
    Growth,
    Projection,
}

#[cfg(test)]
thread_local! {
    static INPUT_VALUE_ALLOCATION_REJECTION: std::cell::Cell<Option<InputValueAllocationSite>> = const {
        std::cell::Cell::new(None)
    };
}

#[cfg(test)]
pub(super) fn reject_next_input_value_allocation(site: InputValueAllocationSite) {
    INPUT_VALUE_ALLOCATION_REJECTION.with(|rejected| rejected.set(Some(site)));
}

pub(super) fn input_value_allocation_allowed(site: InputValueAllocationSite) -> bool {
    #[cfg(test)]
    {
        INPUT_VALUE_ALLOCATION_REJECTION.with(|rejected| {
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

impl InputMode {
    pub(super) fn try_activate(
        &mut self,
        node: Option<crate::compositor::scene::NodeId>,
        value: &str,
        maxlen: u32,
        password: bool,
        field: (u16, u16, bool),
    ) -> bool {
        let Some(current_value) =
            try_copy_input_value(value, maxlen, InputValueAllocationSite::Activation)
        else {
            return false;
        };
        let cursor_pos = current_value.graphemes(true).count();

        self.active = true;
        self.current_node = node;
        self.maxlen = maxlen;
        self.password = password;
        self.current_value = current_value;
        self.cursor_pos = cursor_pos;
        self.field_col = field.0;
        self.field_row = field.1;
        self.field_is_sticky = field.2;
        true
    }
}

pub(super) fn try_copy_input_value(
    value: &str,
    maxlen: u32,
    site: InputValueAllocationSite,
) -> Option<String> {
    let mut bounded_len = 0;
    for (graphemes, (offset, grapheme)) in value.grapheme_indices(true).enumerate() {
        let end = offset.checked_add(grapheme.len())?;
        if end > MAX_INPUT_VALUE_BYTES || maxlen != 0 && graphemes >= maxlen as usize {
            break;
        }
        bounded_len = end;
    }
    if !input_value_allocation_allowed(site) {
        return None;
    }
    let mut copy = String::new();
    copy.try_reserve_exact(bounded_len).ok()?;
    copy.push_str(&value[..bounded_len]);
    Some(copy)
}

// ─── Event dispatcher ───────────────────────────────────────

impl ViewportState {
    #[allow(dead_code)]
    pub fn new(term_w: u16, term_h: u16, content_height: u16) -> Self {
        ViewportState {
            scroll_offset: 0,
            term_w,
            term_h,
            content_height,
            sticky_bottom_height: 0,
        }
    }

    pub fn with_sticky(
        term_w: u16,
        term_h: u16,
        content_height: u16,
        sticky_buf: &Option<CellBuffer>,
    ) -> Self {
        let sticky_bottom_height = sticky_buf.as_ref().map_or(0, |b| b.height);
        ViewportState {
            scroll_offset: 0,
            term_w,
            term_h,
            content_height,
            sticky_bottom_height,
        }
    }

    /// Pure state transition entry point. Async I/O, layout and terminal
    /// writes are deliberately represented as effects for the runner.
    pub fn transition(&mut self, event: ViewportEvent) -> Vec<ViewportEffect> {
        match event {
            ViewportEvent::Input(key) => vec![ViewportEffect::Action(self.handle_key(key))],
            ViewportEvent::Resize { width, height } => {
                if width == self.term_w && height == self.term_h {
                    Vec::new()
                } else {
                    self.term_w = width;
                    self.term_h = height;
                    self.scroll_offset = self.scroll_offset.min(self.max_scroll());
                    vec![ViewportEffect::Relayout { width, height }]
                }
            }
        }
    }

    /// Full viewport height (terminal height minus status bar and command line).
    pub fn viewport_height(&self) -> u16 {
        self.term_h.saturating_sub(2)
    }

    /// Scrollable viewport height (excludes sticky regions).
    pub fn scroll_height(&self) -> u16 {
        self.viewport_height()
            .saturating_sub(self.sticky_bottom_height)
    }

    /// Maximum scroll offset (based on scrollable area, not full viewport).
    pub fn max_scroll(&self) -> u16 {
        self.content_height.saturating_sub(self.scroll_height())
    }

    /// Whether content is taller than the viewport.
    pub fn scrollable(&self) -> bool {
        self.content_height > self.viewport_height()
    }

    /// Handle a resize event. Updates dimensions and clamps scroll.
    #[allow(dead_code)]
    pub fn handle_resize(&mut self, new_w: u16, new_h: u16, new_content_height: u16) {
        self.term_w = new_w;
        self.term_h = new_h;
        self.content_height = new_content_height;
        self.scroll_offset = self.scroll_offset.min(self.max_scroll());
    }

    /// Scroll down by one line. Returns Redraw if position changed.
    pub fn scroll_down(&mut self) -> ViewerAction {
        if self.scrollable() && self.scroll_offset < self.max_scroll() {
            self.scroll_offset += 1;
            ViewerAction::Redraw
        } else {
            ViewerAction::None
        }
    }

    /// Scroll up by one line.
    pub fn scroll_up(&mut self) -> ViewerAction {
        if self.scrollable() && self.scroll_offset > 0 {
            self.scroll_offset -= 1;
            ViewerAction::Redraw
        } else {
            ViewerAction::None
        }
    }

    /// Scroll down by one page.
    pub fn page_down(&mut self) -> ViewerAction {
        if self.scrollable() {
            let new = (self.scroll_offset + self.scroll_height()).min(self.max_scroll());
            if new != self.scroll_offset {
                self.scroll_offset = new;
                return ViewerAction::Redraw;
            }
        }
        ViewerAction::None
    }

    /// Scroll up by one page.
    pub fn page_up(&mut self) -> ViewerAction {
        if self.scrollable() {
            let new = self.scroll_offset.saturating_sub(self.scroll_height());
            if new != self.scroll_offset {
                self.scroll_offset = new;
                return ViewerAction::Redraw;
            }
        }
        ViewerAction::None
    }

    /// Jump to the top.
    pub fn scroll_home(&mut self) -> ViewerAction {
        if self.scroll_offset != 0 {
            self.scroll_offset = 0;
            ViewerAction::Redraw
        } else {
            ViewerAction::None
        }
    }

    /// Jump to the bottom.
    pub fn scroll_end(&mut self) -> ViewerAction {
        let max = self.max_scroll();
        if self.scroll_offset != max {
            self.scroll_offset = max;
            ViewerAction::Redraw
        } else {
            ViewerAction::None
        }
    }

    /// Handle a key event. Returns the appropriate action.
    pub fn handle_key(&mut self, key: KeyEvent) -> ViewerAction {
        match key {
            KeyEvent {
                code: KeyCode::Char('c'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => ViewerAction::Quit,

            KeyEvent {
                code: KeyCode::Char('q'),
                modifiers: KeyModifiers::NONE,
                ..
            } => ViewerAction::Quit,

            KeyEvent {
                code: KeyCode::Down,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('j'),
                ..
            } => self.scroll_down(),

            KeyEvent {
                code: KeyCode::Up, ..
            }
            | KeyEvent {
                code: KeyCode::Char('k'),
                ..
            } => self.scroll_up(),

            KeyEvent {
                code: KeyCode::PageDown,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char(' '),
                ..
            } => self.page_down(),

            KeyEvent {
                code: KeyCode::PageUp,
                ..
            } => self.page_up(),

            KeyEvent {
                code: KeyCode::Home,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('g'),
                ..
            } => self.scroll_home(),

            KeyEvent {
                code: KeyCode::End, ..
            }
            | KeyEvent {
                code: KeyCode::Char('G'),
                ..
            } => self.scroll_end(),

            // Left / h — go back in history
            KeyEvent {
                code: KeyCode::Left,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('h'),
                ..
            } => ViewerAction::GoBack,

            // Right / l — go forward in history
            KeyEvent {
                code: KeyCode::Right,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('l'),
                ..
            } => ViewerAction::GoForward,

            // Tab — focus next element. The viewer loop handles the
            // actual patch emission against the scene; ViewportState no
            // longer stores the focus index.
            KeyEvent {
                code: KeyCode::Tab,
                modifiers: KeyModifiers::NONE,
                ..
            } => ViewerAction::FocusNext,

            // Shift-Tab — focus previous element.
            KeyEvent {
                code: KeyCode::BackTab,
                ..
            } => ViewerAction::FocusPrev,

            // Enter — activate focused element
            KeyEvent {
                code: KeyCode::Enter,
                ..
            } => ViewerAction::Activate,

            // `:` — enter command line mode
            KeyEvent {
                code: KeyCode::Char(':'),
                modifiers: KeyModifiers::NONE,
                ..
            } => ViewerAction::EnterCommandMode,

            // `o` — open URI shortcut (pre-fills `:open `)
            KeyEvent {
                code: KeyCode::Char('o'),
                modifiers: KeyModifiers::NONE,
                ..
            } => ViewerAction::EnterCommandModeOpen,

            // `?` — show client help
            KeyEvent {
                code: KeyCode::Char('?'),
                ..
            } => ViewerAction::ShowHelp,

            // Backtick — toggle the Quake-style client HUD.
            KeyEvent {
                code: KeyCode::Char('`'),
                modifiers: KeyModifiers::NONE,
                ..
            } => ViewerAction::ShowHud,

            // Escape — clear the currently focused control
            KeyEvent {
                code: KeyCode::Esc, ..
            } => ViewerAction::ClearFocus,

            // `r` — reload the current page
            KeyEvent {
                code: KeyCode::Char('r'),
                modifiers: KeyModifiers::NONE,
                ..
            } => ViewerAction::Reload,

            _ => ViewerAction::None,
        }
    }

    /// Scroll the viewport to ensure the given row is visible.
    /// Called after focus changes to keep the focused element on screen.
    pub fn scroll_to_row(&mut self, row: u16) {
        let vh = self.scroll_height();
        if row < self.scroll_offset {
            self.scroll_offset = row;
        } else if row >= self.scroll_offset + vh {
            self.scroll_offset = (row + 1).saturating_sub(vh);
        }
        self.scroll_offset = self.scroll_offset.min(self.max_scroll());
    }
}
