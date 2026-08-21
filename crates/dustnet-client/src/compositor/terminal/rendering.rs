#![allow(clippy::too_many_arguments)]

use std::fmt;
use std::io::{self, Write};

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::presentation::{CommandLine, CommandLineMode, ViewportState};
use crate::color::ColorSupport;
use crate::config::{self, StatusBarConfig, StatusBarVars};
use dustnet_core::protocol::origin::TransportSecurity;

#[cfg(test)]
thread_local! {
    static REJECT_TERMINAL_TEXT_ALLOCATION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
pub(super) fn reject_next_terminal_text_allocation() {
    REJECT_TERMINAL_TEXT_ALLOCATION.with(|reject| reject.set(true));
}

/// Build the variable values for status bar format expansion.
pub(super) fn build_status_vars(
    state: &ViewportState,
    focusable_count: usize,
    focus_idx: Option<usize>,
    proposed_address: Option<&str>,
    uri: &str,
    security: Option<TransportSecurity>,
    title: &str,
    help: &str,
    wasm_mem_bytes: usize,
) -> io::Result<StatusBarVars> {
    let scroll = if state.scrollable() {
        let percent = if state.max_scroll() == 0 {
            100
        } else {
            (state.scroll_offset as u32 * 100 / state.max_scroll() as u32) as u16
        };
        try_format(format_args!("{percent}%"))?
    } else {
        String::new()
    };

    let focus = if focusable_count > 0 {
        match focus_idx {
            Some(idx) => match proposed_address {
                Some(address) => {
                    try_format(format_args!("[{}/{}] {address}", idx + 1, focusable_count))?
                }
                None => try_format(format_args!("[{}/{}]", idx + 1, focusable_count))?,
            },
            None => try_copy("Tab:focus")?,
        }
    } else {
        String::new()
    };

    // Read from the live origin, so it names how *this* connection was
    // authenticated. Deriving it from the launch flags instead would be wrong
    // in the ordinary case: the default mode verifies against authorities, yet
    // a site the user pinned is reached by its pin, and that is a different
    // claim about who the peer is.
    //
    // Until this existed it was a single `no_tls` boolean, so `--insecure` —
    // encrypted, unauthenticated, and therefore the transport most able to
    // mislead — displayed nothing at all.
    let security = match security {
        // Nothing is connected — a local render — so there is no transport to
        // characterise and no claim to make.
        None | Some(TransportSecurity::VerifiedTls) => String::new(),
        // Authenticated, but by this user's own earlier decision rather than
        // by an authority anyone else shares. Worth saying, and not in red:
        // it is a weaker claim than a CA's, not a broken one.
        Some(TransportSecurity::PinnedTls) => try_copy("\x1b[33mpinned\x1b[0m")?,
        Some(TransportSecurity::InsecureTls) => try_copy("\x1b[31minsecure\x1b[0m")?,
        Some(TransportSecurity::PlaintextLoopback) => try_copy("\x1b[31mno-tls\x1b[0m")?,
    };
    let uri_display = if security.is_empty() {
        try_copy(uri)?
    } else {
        try_format(format_args!("{}:{}", security, uri))?
    };
    let mem = if wasm_mem_bytes == 0 {
        String::new()
    } else if wasm_mem_bytes >= 1024 * 1024 {
        try_format(format_args!(
            "{:.1}M",
            wasm_mem_bytes as f64 / (1024.0 * 1024.0)
        ))?
    } else if wasm_mem_bytes >= 1024 {
        try_format(format_args!("{}K", wasm_mem_bytes / 1024))?
    } else {
        try_format(format_args!("{}B", wasm_mem_bytes))?
    };

    Ok(StatusBarVars {
        uri: uri_display,
        title: try_copy(title)?,
        scroll,
        focus,
        security,
        help: try_copy(help)?,
        mem,
    })
}

/// Resolve a focused navigation target for display in the status bar.
pub(super) fn resolve_proposed_address(current_uri: &str, href: &str) -> io::Result<String> {
    if href.starts_with("atp://") {
        return match try_canonical_absolute_address(href)? {
            Some(address) => Ok(address),
            None => try_copy(href),
        };
    }
    if href.len() > crate::protocol::uri::MAX_URI_LEN
        || href.chars().any(char::is_control)
        || href.contains('#')
    {
        return try_copy(href);
    }
    let Some(after_scheme) = current_uri.strip_prefix("atp://") else {
        return try_copy(href);
    };
    let authority_end = after_scheme.find(['/', '?']).unwrap_or(after_scheme.len());
    let authority = &current_uri[.."atp://".len() + authority_end];
    let base_path_and_query = &after_scheme[authority_end..];
    let base_path = base_path_and_query.split_once('?').map_or(
        if base_path_and_query.is_empty() {
            "/"
        } else {
            base_path_and_query
        },
        |(path, _)| if path.is_empty() { "/" } else { path },
    );
    let (href_path, query) = href
        .split_once('?')
        .map_or((href, None), |(path, query)| (path, Some(query)));
    let path = if href_path.starts_with('/') {
        try_normalize_path(href_path)?
    } else if href_path.is_empty() {
        try_copy(base_path)?
    } else {
        let directory = base_path
            .rfind('/')
            .map_or("/", |index| &base_path[..=index]);
        let combined = try_format(format_args!("{directory}{href_path}"))?;
        try_normalize_path(&combined)?
    };
    match query.filter(|query| !query.is_empty()) {
        Some(query) => try_format(format_args!("{authority}{path}?{query}")),
        None => try_format(format_args!("{authority}{path}")),
    }
}

fn try_canonical_absolute_address(value: &str) -> io::Result<Option<String>> {
    if value.len() > crate::protocol::uri::MAX_URI_LEN
        || value.chars().any(char::is_control)
        || value.contains('#')
    {
        return Ok(None);
    }
    let Some(after_scheme) = value.strip_prefix("atp://") else {
        return Ok(None);
    };
    let authority_end = after_scheme.find(['/', '?']).unwrap_or(after_scheme.len());
    let host_port = &after_scheme[..authority_end];
    if host_port.is_empty()
        || host_port
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace())
        || host_port.contains(['@', '#', '?'])
    {
        return Ok(None);
    }
    let path_and_query = if authority_end == after_scheme.len() {
        "/"
    } else {
        &after_scheme[authority_end..]
    };
    let (path, query) = if let Some(query) = path_and_query.strip_prefix('?') {
        ("/", Some(query))
    } else {
        path_and_query
            .split_once('?')
            .map_or((path_and_query, None), |(path, query)| (path, Some(query)))
    };
    let (host, port, ipv6) = if let Some(rest) = host_port.strip_prefix('[') {
        let Some(end) = rest.find(']') else {
            return Ok(None);
        };
        let host = &rest[..end];
        if host.parse::<std::net::Ipv6Addr>().is_err() {
            return Ok(None);
        }
        let suffix = &rest[end + 1..];
        let port = if suffix.is_empty() {
            crate::protocol::DEFAULT_PORT
        } else if let Some(port) = suffix.strip_prefix(':') {
            let Ok(port) = port.parse::<u16>() else {
                return Ok(None);
            };
            port
        } else {
            return Ok(None);
        };
        (host, port, true)
    } else if let Some((host, port)) = host_port.rsplit_once(':') {
        if host.contains(':') || host.is_empty() {
            return Ok(None);
        }
        let Ok(port) = port.parse::<u16>() else {
            return Ok(None);
        };
        (host, port, false)
    } else {
        (host_port, crate::protocol::DEFAULT_PORT, false)
    };
    if host.is_empty() || !host.is_ascii() {
        return Ok(None);
    }
    let mut host = try_copy(host)?;
    host.make_ascii_lowercase();
    let path = try_normalize_path(path)?;
    let mut address = if ipv6 {
        try_format(format_args!("atp://[{host}]"))?
    } else {
        try_format(format_args!("atp://{host}"))?
    };
    if port != crate::protocol::DEFAULT_PORT {
        use std::fmt::Write as _;
        let required = 6usize.saturating_add(path.len());
        address
            .try_reserve(required)
            .map_err(|_| io::Error::other("terminal URI allocation failed"))?;
        write!(&mut address, ":{port}{path}")
            .map_err(|_| io::Error::other("terminal URI formatting failed"))?;
    } else {
        address
            .try_reserve(path.len())
            .map_err(|_| io::Error::other("terminal URI allocation failed"))?;
        address.push_str(&path);
    }
    if let Some(query) = query.filter(|query| !query.is_empty()) {
        address
            .try_reserve(query.len().saturating_add(1))
            .map_err(|_| io::Error::other("terminal URI allocation failed"))?;
        address.push('?');
        address.push_str(query);
    }
    Ok(Some(address))
}

fn try_normalize_path(path: &str) -> io::Result<String> {
    let trailing_slash = path.len() > 1 && path.ends_with('/');
    let mut normalized = String::new();
    normalized
        .try_reserve(path.len().saturating_add(1))
        .map_err(|_| io::Error::other("terminal URI allocation failed"))?;
    normalized.push('/');
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if normalized.len() > 1 {
                    normalized.pop();
                    normalized.truncate(normalized.rfind('/').map_or(1, |index| index + 1));
                }
            }
            segment => {
                if normalized.len() > 1 && !normalized.ends_with('/') {
                    normalized.push('/');
                }
                normalized.push_str(segment);
            }
        }
    }
    if trailing_slash && normalized != "/" && !normalized.ends_with('/') {
        normalized.push('/');
    }
    Ok(normalized)
}

struct FallibleString(String);

impl fmt::Write for FallibleString {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        self.0.try_reserve(value.len()).map_err(|_| fmt::Error)?;
        self.0.push_str(value);
        Ok(())
    }
}

pub(super) fn try_format(arguments: fmt::Arguments<'_>) -> io::Result<String> {
    #[cfg(test)]
    if REJECT_TERMINAL_TEXT_ALLOCATION.with(|reject| reject.replace(false)) {
        return Err(io::Error::other("terminal text allocation rejected"));
    }
    let mut output = FallibleString(String::new());
    fmt::write(&mut output, arguments)
        .map_err(|_| io::Error::other("terminal text allocation failed"))?;
    Ok(output.0)
}

pub(super) fn try_copy(value: &str) -> io::Result<String> {
    try_format(format_args!("{value}"))
}

/// Write a fully-expanded status bar string to the terminal with styling and padding.
pub(super) fn write_status_line(
    out: &mut impl Write,
    state: &ViewportState,
    status: &str,
    sb_config: &StatusBarConfig,
    color_support: ColorSupport,
) -> io::Result<()> {
    let terminal_default = crate::compositor::present::ansi::TERMINAL_DEFAULT_SGR;
    write!(
        out,
        "\x1b[{};1H\x1b[0m{terminal_default}",
        state.viewport_height() + 1
    )?;
    config::write_status_bar_sgr(out, sb_config, color_support)?;

    let max_width = state.term_w as usize;
    let mut display_w = 0usize;
    let mut byte_end = 0usize;
    let mut in_escape = false;
    for ch in status.chars() {
        if in_escape {
            byte_end += ch.len_utf8();
            if ch.is_ascii_alphabetic() || ch == 'm' {
                in_escape = false;
            }
            continue;
        }
        if ch == '\x1b' {
            in_escape = true;
            byte_end += ch.len_utf8();
            continue;
        }
        let width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if display_w + width > max_width {
            break;
        }
        display_w += width;
        byte_end += ch.len_utf8();
    }
    let display = status.get(..byte_end).unwrap_or(status);
    let mut remainder = display;
    while let Some(index) = remainder.find("\x1b[0m") {
        out.write_all(remainder.as_bytes().get(..index).unwrap_or_default())?;
        write!(out, "\x1b[0m{terminal_default}")?;
        config::write_status_bar_sgr(out, sb_config, color_support)?;
        let resume = index.saturating_add("\x1b[0m".len());
        remainder = remainder.get(resume..).unwrap_or("");
    }
    out.write_all(remainder.as_bytes())?;
    for _ in 0..max_width.saturating_sub(display_w) {
        write!(out, " ")?;
    }
    write!(out, "\x1b[0m")
}

/// Render the command line at the very last terminal row.
pub(super) fn write_command_line(
    out: &mut impl Write,
    state: &ViewportState,
    cmd_line: &CommandLine,
) -> io::Result<()> {
    let row = state.viewport_height() + 2;
    write!(
        out,
        "\x1b[{row};1H\x1b[0m{}",
        crate::compositor::present::ansi::TERMINAL_DEFAULT_SGR
    )?;
    let max_width = state.term_w as usize;

    match cmd_line.mode {
        CommandLineMode::Idle => {
            for _ in 0..max_width {
                write!(out, " ")?;
            }
        }
        CommandLineMode::Input => {
            write!(out, ":")?;
            let (display, display_width) =
                display_width_prefix(&cmd_line.buffer, max_width.saturating_sub(1));
            write!(out, "{display}")?;
            for _ in 1 + display_width..max_width {
                write!(out, " ")?;
            }
            let cursor_col = (1 + UnicodeWidthStr::width(
                cmd_line
                    .buffer
                    .get(..cmd_line.cursor)
                    .unwrap_or(&cmd_line.buffer),
            ))
            .min(max_width.saturating_sub(1));
            write!(out, "\x1b[{row};{}H", cursor_col + 1)?;
        }
        CommandLineMode::Message => {
            if cmd_line.is_error {
                write!(out, "\x1b[31m")?;
            }
            let (display, used) = display_width_prefix(&cmd_line.message, max_width);
            write!(out, "{display}")?;
            for _ in used..max_width {
                write!(out, " ")?;
            }
            write!(out, "\x1b[0m")?;
        }
    }
    Ok(())
}

/// Truncate without splitting a grapheme cluster or exceeding terminal-cell width.
#[cfg(test)]
pub(super) fn truncate_to_display_width(input: &str, max_width: usize) -> (String, usize) {
    let (prefix, width) = display_width_prefix(input, max_width);
    (prefix.to_owned(), width)
}

pub(super) fn display_width_prefix(input: &str, max_width: usize) -> (&str, usize) {
    let mut width = 0;
    let mut byte_end = 0;
    for grapheme in input.graphemes(true) {
        let grapheme_width = UnicodeWidthStr::width(grapheme);
        if width + grapheme_width > max_width {
            break;
        }
        width += grapheme_width;
        byte_end += grapheme.len();
    }
    (&input[..byte_end], width)
}

pub(super) fn previous_char_boundary(input: &str, from: usize) -> usize {
    input
        .get(..from)
        .and_then(|prefix| prefix.char_indices().next_back().map(|(index, _)| index))
        .unwrap_or(0)
}

pub(super) fn next_char_boundary(input: &str, from: usize) -> usize {
    input
        .get(from..)
        .and_then(|suffix| suffix.chars().next().map(|ch| from + ch.len_utf8()))
        .unwrap_or(input.len())
}
