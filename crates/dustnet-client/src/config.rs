use std::collections::TryReserveError;
use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

use crate::color::{Color, ColorSupport, ResolvedColor, parse_color};

/// Client-side configuration.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub status_bar: StatusBarConfig,
    /// Let CA-verified sessions outlive the process, so logging in once lasts.
    ///
    /// On by default. Logging in again at every launch is the kind of cost a
    /// user pays continuously and a threat model never counts, and what is
    /// stored is the site's own revocable, expiring, path-scoped token rather
    /// than the password behind it. `remember-sessions = false` restores
    /// memory-only sessions for anyone who wants them.
    ///
    /// What the default does *not* do is widen what a token reaches: only
    /// CA-verified origins with an expiry are eligible, the file is owner-only
    /// and refused when it is not, and a stored session is admitted under the
    /// same bounds a server's directive is. See [`crate::session_file`], which
    /// also explains why the file carries no security-level field.
    pub remember_sessions: bool,
}

impl Default for ClientConfig {
    fn default() -> Self {
        ClientConfig {
            status_bar: StatusBarConfig::default(),
            remember_sessions: true,
        }
    }
}

/// Status bar appearance and layout configuration.
///
/// The format string supports these variables:
///   {uri}       — current page URI (connected mode only)
///   {title}     — page title from AML `[page title="..."]`
///   {scroll}    — scroll percentage (e.g. "42%"), empty if not scrollable
///   {focus}     — focus indicator (e.g. "[1/5]" or "Tab:focus"), empty if no focusables
///   {security}  — how the current connection was authenticated
///                 ("no-tls", "insecure", "pinned"), empty for CA-verified TLS
///   {help}      — default keybinding hints
///   {mem}       — total WASM effect memory (e.g. "1.1M"), empty when none running
///   {fill}      — expands to `─` repeated to fill remaining terminal width
///
/// Literal text is passed through as-is. Unknown `{variables}` are left as-is.
#[derive(Debug, Clone)]
pub struct StatusBarConfig {
    /// Format string for connected viewer (ATP) status bar.
    pub connected_format: String,
    /// Format string for local file viewer status bar.
    pub local_format: String,
    /// Foreground color. None means use reverse video (default).
    pub fg: Option<Color>,
    /// Background color. None means use reverse video (default).
    pub bg: Option<Color>,
    /// If true (default), use reverse video instead of explicit fg/bg.
    pub reverse: bool,
}

impl Default for StatusBarConfig {
    fn default() -> Self {
        StatusBarConfig {
            // `{security}` sits against `{uri}` with no separator on purpose.
            // It is the only place the interface distinguishes a pinned or
            // unauthenticated connection from a CA-verified one, so it reads
            // as part of the address rather than as a detachable badge — and a
            // format that drops it shows the same status bar for both.
            connected_format:
                "{fill}[ {uri}{security} ]{fill}[ {scroll} {focus} ]{fill}[ {help} ]{fill}".into(),
            local_format: "{fill}[ dustnet ]{fill}[ {scroll} {focus} ]{fill}[ {help} ]{fill}"
                .into(),
            // Grey on black rather than reverse video. Reverse takes whatever
            // the terminal's foreground happens to be, which on a light theme
            // is a black bar and on a dark one is a white one; naming both
            // colours makes the bar look the same everywhere, at the cost of
            // ignoring the user's palette. `status-reverse = true` restores it.
            fg: Some(Color::Rgb {
                r: 0x88,
                g: 0x88,
                b: 0x88,
            }),
            bg: Some(Color::Rgb { r: 0, g: 0, b: 0 }),
            reverse: false,
        }
    }
}

/// Variables available for substitution in status bar format strings.
#[derive(Default)]
pub struct StatusBarVars {
    pub uri: String,
    pub title: String,
    pub scroll: String,
    pub focus: String,
    pub security: String,
    pub help: String,
    /// Total WASM effect linear memory (e.g. "1.1M"), empty when no effects
    /// are running. Not in the default format — opt in for debugging.
    pub mem: String,
}

/// Expand a format string by replacing `{name}` placeholders with values.
///
/// The special `{fill}` placeholder expands to `─` repeated to fill the
/// remaining terminal width. If multiple `{fill}` placeholders appear,
/// the available space is divided equally among them. `term_width` of 0
/// means no fill expansion (useful for tests that don't care about it).
pub fn expand_format(format: &str, vars: &StatusBarVars) -> String {
    expand_format_width(format, vars, 0)
}

/// Expand a format string with a known terminal width for `{fill}` support.
pub fn expand_format_width(format: &str, vars: &StatusBarVars, term_width: u16) -> String {
    try_expand_format_width(format, vars, term_width).unwrap_or_default()
}

/// Fallible form of status expansion used by the synchronized HUD candidate.
pub fn try_expand_format_width(
    format: &str,
    vars: &StatusBarVars,
    term_width: u16,
) -> Result<String, TryReserveError> {
    // First pass: expand all variables except {fill}, count {fill} occurrences
    let mut parts: Vec<String> = Vec::new();
    parts.try_reserve(format.len().saturating_add(1))?;
    let mut fill_count = 0usize;
    let mut chars = format.chars().peekable();
    let mut current = String::new();

    while let Some(ch) = chars.next() {
        if ch == '{' {
            let mut name = String::new();
            let mut found_close = false;
            for inner in chars.by_ref() {
                if inner == '}' {
                    found_close = true;
                    break;
                }
                name.try_reserve(inner.len_utf8())?;
                name.push(inner);
            }
            if found_close {
                match name.as_str() {
                    "fill" => {
                        parts.push(std::mem::take(&mut current));
                        let mut fill = String::new();
                        fill.try_reserve_exact("{fill}".len())?;
                        fill.push_str("{fill}");
                        parts.push(fill);
                        fill_count += 1;
                    }
                    "uri" => try_push_str(&mut current, &vars.uri)?,
                    "title" => try_push_str(&mut current, &vars.title)?,
                    "scroll" => try_push_str(&mut current, &vars.scroll)?,
                    "focus" => try_push_str(&mut current, &vars.focus)?,
                    "security" => try_push_str(&mut current, &vars.security)?,
                    "help" => try_push_str(&mut current, &vars.help)?,
                    "mem" => try_push_str(&mut current, &vars.mem)?,
                    _ => {
                        current.try_reserve(name.len().saturating_add(2))?;
                        current.push('{');
                        current.push_str(&name);
                        current.push('}');
                    }
                }
            } else {
                current.try_reserve(name.len().saturating_add(1))?;
                current.push('{');
                current.push_str(&name);
            }
        } else {
            current.try_reserve(ch.len_utf8())?;
            current.push(ch);
        }
    }
    parts.push(current);

    if fill_count == 0 || term_width == 0 {
        return try_join_parts(&parts, 0);
    }

    // Calculate how much display space non-fill parts consume
    // Strip ANSI escape sequences before measuring so embedded colors don't
    // inflate the width calculation.
    let fixed_width: usize = parts
        .iter()
        .filter(|p| *p != "{fill}")
        .map(|p| visible_width_without_ansi(p))
        .sum();
    let remaining = (term_width as usize).saturating_sub(fixed_width);
    let per_fill = remaining / fill_count.max(1);

    try_join_parts(&parts, per_fill)
}

fn try_push_str(output: &mut String, value: &str) -> Result<(), TryReserveError> {
    output.try_reserve(value.len())?;
    output.push_str(value);
    Ok(())
}

fn try_join_parts(parts: &[String], fill_width: usize) -> Result<String, TryReserveError> {
    let fill_bytes = fill_width.saturating_mul("─".len());
    let bytes = parts
        .iter()
        .try_fold(0usize, |total, part| {
            total.checked_add(if part == "{fill}" {
                fill_bytes
            } else {
                part.len()
            })
        })
        .unwrap_or(usize::MAX);
    let mut output = String::new();
    output.try_reserve_exact(bytes)?;
    for part in parts {
        if part == "{fill}" {
            for _ in 0..fill_width {
                output.push('─');
            }
        } else {
            output.push_str(part);
        }
    }
    Ok(output)
}

fn visible_width_without_ansi(value: &str) -> usize {
    let mut width = 0usize;
    let mut visible_start = 0usize;
    let mut chars = value.char_indices().peekable();
    while let Some((index, ch)) = chars.next() {
        if ch == '\x1b' {
            width = width.saturating_add(unicode_width::UnicodeWidthStr::width(
                &value[visible_start..index],
            ));
            let mut escape_end = index + ch.len_utf8();
            if let Some((bracket_index, _)) = chars.next_if(|(_, next)| *next == '[') {
                escape_end = bracket_index + 1;
                for (inner_index, inner) in chars.by_ref() {
                    escape_end = inner_index + inner.len_utf8();
                    if inner.is_ascii_alphabetic() || inner == 'm' {
                        break;
                    }
                }
            }
            visible_start = escape_end;
        }
    }
    width.saturating_add(unicode_width::UnicodeWidthStr::width(
        &value[visible_start..],
    ))
}

/// Build the ANSI escape sequence to style the status bar.
///
/// Returns a pair: (open_sequence, close_sequence).
pub fn status_bar_sgr(config: &StatusBarConfig, color_support: ColorSupport) -> (String, String) {
    if config.reverse && config.fg.is_none() && config.bg.is_none() {
        return ("\x1b[7m".into(), "\x1b[0m".into());
    }

    let mut params = Vec::new();

    if let Some(ref fg) = config.fg
        && let Some(resolved) = fg.resolve(color_support)
    {
        match resolved {
            ResolvedColor::Named(n) => params.push(n.fg_sgr().to_string()),
            ResolvedColor::Palette(idx) => params.push(format!("38;5;{idx}")),
            ResolvedColor::Rgb(r, g, b) => params.push(format!("38;2;{r};{g};{b}")),
        }
    }

    if let Some(ref bg) = config.bg
        && let Some(resolved) = bg.resolve(color_support)
    {
        match resolved {
            ResolvedColor::Named(n) => params.push(n.bg_sgr().to_string()),
            ResolvedColor::Palette(idx) => params.push(format!("48;5;{idx}")),
            ResolvedColor::Rgb(r, g, b) => params.push(format!("48;2;{r};{g};{b}")),
        }
    }

    if config.reverse {
        params.push("7".into());
    }

    if params.is_empty() {
        ("\x1b[7m".into(), "\x1b[0m".into())
    } else {
        (format!("\x1b[{}m", params.join(";")), "\x1b[0m".into())
    }
}

/// Write status-bar styling without allocating an intermediate parameter list.
pub fn write_status_bar_sgr(
    out: &mut impl Write,
    config: &StatusBarConfig,
    color_support: ColorSupport,
) -> io::Result<()> {
    if config.reverse && config.fg.is_none() && config.bg.is_none() {
        return out.write_all(b"\x1b[7m");
    }
    out.write_all(b"\x1b[")?;
    let mut first = true;
    macro_rules! parameter {
        ($($arg:tt)*) => {{
            if !first { out.write_all(b";")?; }
            first = false;
            write!(out, $($arg)*)?;
        }};
    }
    if let Some(resolved) = config
        .fg
        .as_ref()
        .and_then(|color| color.resolve(color_support))
    {
        match resolved {
            ResolvedColor::Named(color) => parameter!("{}", color.fg_sgr()),
            ResolvedColor::Palette(index) => parameter!("38;5;{index}"),
            ResolvedColor::Rgb(red, green, blue) => parameter!("38;2;{red};{green};{blue}"),
        }
    }
    if let Some(resolved) = config
        .bg
        .as_ref()
        .and_then(|color| color.resolve(color_support))
    {
        match resolved {
            ResolvedColor::Named(color) => parameter!("{}", color.bg_sgr()),
            ResolvedColor::Palette(index) => parameter!("48;5;{index}"),
            ResolvedColor::Rgb(red, green, blue) => parameter!("48;2;{red};{green};{blue}"),
        }
    }
    if config.reverse {
        parameter!("7");
    }
    if first {
        out.write_all(b"7")?;
    }
    out.write_all(b"m")
}

/// Config file path: `~/.config/dustnet/client.conf`.
fn config_path() -> Option<PathBuf> {
    // Respect XDG_CONFIG_HOME, fall back to ~/.config
    let config_dir = if let Ok(xdg) = env::var("XDG_CONFIG_HOME") {
        PathBuf::from(xdg)
    } else {
        let home = env::var("HOME").ok()?;
        PathBuf::from(home).join(".config")
    };
    Some(config_dir.join("dustnet").join("client.conf"))
}

/// Load client config from file and environment variables.
///
/// Priority (highest to lowest):
///   1. Environment variables: DUSTNET_STATUS_FORMAT, DUSTNET_STATUS_LOCAL_FORMAT,
///      DUSTNET_STATUS_FG, DUSTNET_STATUS_BG, DUSTNET_REMEMBER_SESSIONS
///   2. Config file: ~/.config/dustnet/client.conf
///   3. Built-in defaults
pub fn load_config() -> ClientConfig {
    let mut config = ClientConfig::default();

    // Load from config file first (lower priority)
    if let Some(path) = config_path()
        && let Ok(contents) = fs::read_to_string(&path)
    {
        apply_config_file(&mut config, &contents);
    }

    // Environment variables override (higher priority)
    if let Ok(val) = env::var("DUSTNET_STATUS_FORMAT") {
        config.status_bar.connected_format = val;
    }
    if let Ok(val) = env::var("DUSTNET_STATUS_LOCAL_FORMAT") {
        config.status_bar.local_format = val;
    }
    if let Ok(val) = env::var("DUSTNET_STATUS_FG")
        && let Ok(color) = parse_color(&val)
    {
        config.status_bar.fg = Some(color);
        config.status_bar.reverse = false;
    }
    if let Ok(val) = env::var("DUSTNET_STATUS_BG")
        && let Ok(color) = parse_color(&val)
    {
        config.status_bar.bg = Some(color);
        config.status_bar.reverse = false;
    }
    if let Ok(val) = env::var("DUSTNET_REMEMBER_SESSIONS") {
        config.remember_sessions = truthy(&val);
    }

    config
}

/// The affirmative spellings accepted for a boolean setting.
fn truthy(value: &str) -> bool {
    value == "true" || value == "yes" || value == "1"
}

/// Parse a simple `key = value` config file.
fn apply_config_file(config: &mut ClientConfig, contents: &str) {
    for line in contents.lines() {
        let line = line.trim();
        // Skip comments and blank lines
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim();
            let value = value.trim();

            match key {
                "status-format" => {
                    config.status_bar.connected_format = value.to_string();
                }
                "status-local-format" => {
                    config.status_bar.local_format = value.to_string();
                }
                "status-fg" => {
                    if let Ok(color) = parse_color(value) {
                        config.status_bar.fg = Some(color);
                        config.status_bar.reverse = false;
                    }
                }
                "status-bg" => {
                    if let Ok(color) = parse_color(value) {
                        config.status_bar.bg = Some(color);
                        config.status_bar.reverse = false;
                    }
                }
                "status-reverse" => {
                    config.status_bar.reverse = truthy(value);
                }
                "remember-sessions" => {
                    config.remember_sessions = truthy(value);
                }
                _ => {
                    // Unknown keys silently ignored (forward-compat)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remember_sessions_is_on_until_the_config_turns_it_off() {
        let mut config = ClientConfig::default();
        assert!(
            config.remember_sessions,
            "logging in once should last by default"
        );
        // Turning it off is the case that has to work: someone who wants
        // memory-only sessions is making a security decision, and a spelling
        // the parser quietly ignored would leave them believing they had.
        for value in ["false", "no", "0"] {
            let mut config = ClientConfig::default();
            apply_config_file(&mut config, &format!("remember-sessions = {value}\n"));
            assert!(!config.remember_sessions, "`{value}` should disable it");
        }
        for value in ["true", "yes", "1"] {
            let mut config = ClientConfig {
                remember_sessions: false,
                ..ClientConfig::default()
            };
            apply_config_file(&mut config, &format!("remember-sessions = {value}\n"));
            assert!(config.remember_sessions, "`{value}` should enable it");
        }
        // A comment or an unrelated key leaves the default alone.
        apply_config_file(
            &mut config,
            "# remember-sessions = false\nstatus-reverse = true\n",
        );
        assert!(config.remember_sessions);
    }

    #[test]
    fn expand_mem_variable() {
        let vars = StatusBarVars {
            mem: "1.1M".into(),
            ..Default::default()
        };
        assert_eq!(expand_format("[{mem}]", &vars), "[1.1M]");
        // Empty when no effects run, so the variable disappears.
        assert_eq!(expand_format("[{mem}]", &StatusBarVars::default()), "[]");
    }

    #[test]
    fn fill_width_treats_joined_emoji_as_one_grapheme() {
        let vars = StatusBarVars {
            title: "👩‍💻".into(),
            ..StatusBarVars::default()
        };
        assert_eq!(expand_format_width("{title}{fill}", &vars, 4), "👩‍💻──");
    }

    #[test]
    fn expand_all_variables() {
        let vars = StatusBarVars {
            uri: "atp://example.com/".into(),
            title: "Home".into(),
            scroll: "42%".into(),
            focus: "[1/3]".into(),
            security: "".into(),
            help: ":q quit".into(),
            mem: String::new(),
        };
        let result = expand_format(" {uri} {title} {scroll} {focus} {help}", &vars);
        assert_eq!(result, " atp://example.com/ Home 42% [1/3] :q quit");
    }

    #[test]
    fn expand_empty_variables() {
        let vars = StatusBarVars {
            uri: "atp://x/".into(),
            title: "".into(),
            scroll: "".into(),
            focus: "".into(),
            security: "".into(),
            help: ":q quit".into(),
            mem: String::new(),
        };
        let result = expand_format(" {uri}{security} {scroll}{focus} | {help}", &vars);
        assert_eq!(result, " atp://x/  | :q quit");
    }

    #[test]
    fn expand_unknown_variable_passthrough() {
        let vars = StatusBarVars {
            uri: "".into(),
            title: "".into(),
            scroll: "".into(),
            focus: "".into(),
            security: "".into(),
            help: "".into(),
            mem: String::new(),
        };
        let result = expand_format("{unknown} text", &vars);
        assert_eq!(result, "{unknown} text");
    }

    #[test]
    fn expand_unclosed_brace() {
        let vars = StatusBarVars {
            uri: "x".into(),
            title: "".into(),
            scroll: "".into(),
            focus: "".into(),
            security: "".into(),
            help: "".into(),
            mem: String::new(),
        };
        let result = expand_format("{uri} {oops", &vars);
        assert_eq!(result, "x {oops");
    }

    #[test]
    fn expand_literal_only() {
        let vars = StatusBarVars {
            uri: "".into(),
            title: "".into(),
            scroll: "".into(),
            focus: "".into(),
            security: "".into(),
            help: "".into(),
            mem: String::new(),
        };
        let result = expand_format(" dustnet | q quit ", &vars);
        assert_eq!(result, " dustnet | q quit ");
    }

    #[test]
    fn config_file_parsing() {
        let mut config = ClientConfig::default();
        let contents = r#"
# Status bar config
status-format = {title} | {uri} {scroll}
status-fg = cyan
status-bg = #1a1a2e
status-reverse = false
"#;
        apply_config_file(&mut config, contents);
        assert_eq!(
            config.status_bar.connected_format,
            "{title} | {uri} {scroll}"
        );
        assert_eq!(
            config.status_bar.fg,
            Some(Color::Named(crate::color::NamedColor::Cyan))
        );
        assert_eq!(
            config.status_bar.bg,
            Some(Color::Rgb {
                r: 0x1a,
                g: 0x1a,
                b: 0x2e
            })
        );
        assert!(!config.status_bar.reverse);
    }

    #[test]
    fn config_file_skips_unknown_keys() {
        let mut config = ClientConfig::default();
        apply_config_file(&mut config, "unknown-key = whatever\n");
        // Should not panic, and defaults should be unchanged
        assert_eq!(
            config.status_bar.connected_format,
            StatusBarConfig::default().connected_format
        );
    }

    #[test]
    fn default_status_bar_sgr_names_both_colours() {
        let config = StatusBarConfig::default();
        let (open, close) = status_bar_sgr(&config, ColorSupport::Truecolor);
        assert_eq!(open, "\x1b[38;2;136;136;136;48;2;0;0;0m");
        assert_eq!(close, "\x1b[0m");
        let mut direct = Vec::new();
        write_status_bar_sgr(&mut direct, &config, ColorSupport::Truecolor).unwrap();
        assert_eq!(direct, open.as_bytes());
    }

    /// Reverse video is no longer the default, but it is still reachable —
    /// it is the only setting that respects a terminal's own palette.
    #[test]
    fn reverse_is_still_available() {
        // Reverse alone, which is what the default used to be: it takes the
        // terminal's own foreground and background rather than naming either.
        let config = StatusBarConfig {
            reverse: true,
            fg: None,
            bg: None,
            ..StatusBarConfig::default()
        };
        let (open, _) = status_bar_sgr(&config, ColorSupport::Truecolor);
        assert_eq!(open, "\x1b[7m");
    }

    /// The default format must keep `{security}`.
    ///
    /// It is the only place the interface distinguishes a pinned or
    /// unauthenticated connection from a CA-verified one, so a default that
    /// dropped it would show the same status bar for an `--insecure` link as
    /// for a verified one — and would do it silently, which is the failure
    /// mode worth a test rather than a comment.
    #[test]
    fn the_default_connected_format_reports_transport_security() {
        let config = StatusBarConfig::default();
        assert!(
            config.connected_format.contains("{security}"),
            "default status bar drops the transport indicator: {}",
            config.connected_format
        );

        let vars = StatusBarVars {
            uri: "atp://example.com/".into(),
            security: "pinned".into(),
            ..StatusBarVars::default()
        };
        let rendered = expand_format(&config.connected_format, &vars);
        assert!(
            rendered.contains("pinned"),
            "the indicator is in the format but not the output: {rendered}"
        );
    }

    #[test]
    fn custom_colors_sgr() {
        let config = StatusBarConfig {
            fg: Some(Color::Named(crate::color::NamedColor::White)),
            bg: Some(Color::Named(crate::color::NamedColor::Blue)),
            reverse: false,
            ..StatusBarConfig::default()
        };
        let (open, _close) = status_bar_sgr(&config, ColorSupport::Basic);
        assert!(open.contains("37")); // white fg
        assert!(open.contains("44")); // blue bg
        let mut direct = Vec::new();
        write_status_bar_sgr(&mut direct, &config, ColorSupport::Basic).unwrap();
        assert_eq!(direct, open.as_bytes());
    }

    #[test]
    fn reverse_with_colors_sgr() {
        let config = StatusBarConfig {
            fg: Some(Color::Named(crate::color::NamedColor::Cyan)),
            bg: None,
            reverse: true,
            ..StatusBarConfig::default()
        };
        let (open, _) = status_bar_sgr(&config, ColorSupport::Basic);
        assert!(open.contains("36")); // cyan fg
        assert!(open.contains("7")); // reverse
    }
}
