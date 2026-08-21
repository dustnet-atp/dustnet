use std::io::{self, Read, Write};
use std::time::Duration;

use crossterm::{ExecutableCommand, cursor, terminal};

/// Probe the terminal to determine how it renders ambiguous-width characters.
///
/// Prints a known ambiguous-width character (▒, U+2592), queries the cursor
/// position, and measures how many columns the terminal used. Cleans up after
/// itself.
///
/// Returns 1 or 2. Returns None if the probe fails (timeout, unsupported
/// terminal, etc.), in which case the caller should fall back to a heuristic.
pub fn probe_ambiguous_width() -> Option<u8> {
    // Must be in raw mode for this to work
    let was_raw = terminal::is_raw_mode_enabled().unwrap_or(false);
    if !was_raw {
        terminal::enable_raw_mode().ok()?;
    }

    let result = do_probe();

    if !was_raw {
        let _ = terminal::disable_raw_mode();
    }

    result
}

fn do_probe() -> Option<u8> {
    let mut stdout = io::stdout();
    let mut stdin = io::stdin();

    // Save cursor, move to column 1 on a known row
    // Use a high row to avoid interfering with visible content
    stdout.execute(cursor::SavePosition).ok()?;
    write!(stdout, "\x1b[999;1H").ok()?; // move to far bottom-left
    write!(stdout, "\x1b[6n").ok()?; // query position (to get actual row)
    stdout.flush().ok()?;

    let (base_row, _) = read_cursor_position(&mut stdin)?;

    // Now print an ambiguous character and query position again
    write!(stdout, "\x1b[{base_row};1H").ok()?; // move to col 1
    write!(stdout, "▒").ok()?; // ambiguous width character
    write!(stdout, "\x1b[6n").ok()?; // query position
    stdout.flush().ok()?;

    let (_, col_after) = read_cursor_position(&mut stdin)?;

    // Clean up: clear the test character and restore cursor
    write!(stdout, "\x1b[{base_row};1H").ok()?;
    write!(stdout, "  ").ok()?; // overwrite with spaces
    stdout.execute(cursor::RestorePosition).ok()?;
    stdout.flush().ok()?;

    // col_after is 1-based. If char was 1-wide, cursor is at col 2.
    // If char was 2-wide, cursor is at col 3.
    let width = (col_after - 1) as u8;
    if width == 1 || width == 2 {
        Some(width)
    } else {
        None
    }
}

/// Read a cursor position response: ESC [ row ; col R
fn read_cursor_position(stdin: &mut impl Read) -> Option<(u16, u16)> {
    let mut buf = [0u8; 32];
    let mut idx = 0;

    // Read with a timeout to avoid hanging if terminal doesn't respond
    // We read byte by byte looking for the 'R' terminator
    let deadline = std::time::Instant::now() + Duration::from_millis(500);

    loop {
        if std::time::Instant::now() > deadline {
            return None;
        }

        // Use crossterm's poll to check for available input
        if !crossterm::event::poll(Duration::from_millis(100)).ok()? {
            continue;
        }

        let slot = buf.get_mut(idx..idx + 1)?;
        let n = stdin.read(slot).ok()?;
        if n == 0 {
            continue;
        }

        if buf.get(idx).copied() == Some(b'R') {
            break;
        }

        idx += 1;
        if idx >= buf.len() {
            return None;
        }
    }

    // Parse ESC [ row ; col R
    let response = std::str::from_utf8(buf.get(..idx)?).ok()?;

    // Find the ESC[ prefix
    let coords = response.strip_prefix("\x1b[")?;
    let (row_str, col_str) = coords.split_once(';')?;
    let row: u16 = row_str.parse().ok()?;
    let col: u16 = col_str.parse().ok()?;

    Some((row, col))
}

/// Detect ambiguous width using locale heuristic.
///
/// CJK locales (ja, ko, zh) typically render ambiguous-width characters
/// as 2 columns wide. Western locales render them as 1.
pub fn locale_ambiguous_width() -> u8 {
    for var in &["LC_ALL", "LC_CTYPE", "LANG"] {
        if let Ok(val) = std::env::var(var) {
            let lower = val.to_ascii_lowercase();
            if lower.starts_with("zh") || lower.starts_with("ja") || lower.starts_with("ko") {
                return 2;
            }
            // If we found a non-empty locale, use it
            if !lower.is_empty() {
                return 1;
            }
        }
    }
    1 // default to 1
}
