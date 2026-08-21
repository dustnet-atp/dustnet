//! Terminal raw-mode and alternate-screen ownership.

use std::io::{self, Write};

use crossterm::{ExecutableCommand, cursor, event, terminal};

use crate::color::ColorSupport;

pub fn detect_color_support() -> ColorSupport {
    if let Ok(value) = std::env::var("COLORTERM")
        && matches!(value.as_str(), "truecolor" | "24bit")
    {
        return ColorSupport::Truecolor;
    }
    if let Ok(value) = std::env::var("TERM") {
        if value.contains("256color") {
            return ColorSupport::Palette256;
        }
        if value == "dumb" {
            return ColorSupport::None;
        }
    }
    ColorSupport::Basic
}

/// RAII terminal state manager. Dropping it restores the terminal during
/// normal return, error propagation, and panic unwinding.
pub struct Terminal {
    raw_enabled: bool,
    alternate_screen: bool,
    cursor_hidden: bool,
    mouse_capture: bool,
}

impl Terminal {
    pub fn enter() -> io::Result<Self> {
        let mut stdout = io::stdout();
        terminal::enable_raw_mode()?;
        // Construct the guard immediately after the first state mutation so
        // every later setup failure unwinds through restoration.
        let mut guard = Self {
            raw_enabled: true,
            alternate_screen: false,
            cursor_hidden: false,
            mouse_capture: false,
        };
        stdout.execute(terminal::EnterAlternateScreen)?;
        guard.alternate_screen = true;
        write!(
            stdout,
            "{}",
            crate::compositor::present::ansi::TERMINAL_DEFAULT_SGR
        )?;
        stdout.execute(cursor::Hide)?;
        guard.cursor_hidden = true;
        stdout.execute(event::EnableMouseCapture)?;
        guard.mouse_capture = true;
        stdout.execute(terminal::Clear(terminal::ClearType::All))?;
        Ok(guard)
    }

    pub fn leave(&mut self) -> io::Result<()> {
        let mut first_error = None;
        let mut remember = |result: io::Result<()>| {
            if let Err(error) = result
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        };
        let mut stdout = io::stdout();
        if self.mouse_capture {
            let result = stdout.execute(event::DisableMouseCapture).map(|_| ());
            if result.is_ok() {
                self.mouse_capture = false;
            }
            remember(result);
        }
        if self.cursor_hidden {
            let result = stdout.execute(cursor::Show).map(|_| ());
            if result.is_ok() {
                self.cursor_hidden = false;
            }
            remember(result);
        }
        remember(write!(
            stdout,
            "{}",
            crate::compositor::present::ansi::RESET
        ));
        if self.alternate_screen {
            let result = stdout.execute(terminal::LeaveAlternateScreen).map(|_| ());
            if result.is_ok() {
                self.alternate_screen = false;
            }
            remember(result);
        }
        if self.raw_enabled {
            let result = terminal::disable_raw_mode();
            if result.is_ok() {
                self.raw_enabled = false;
            }
            remember(result);
        }
        if let Some(error) = first_error {
            Err(error)
        } else {
            Ok(())
        }
    }

    pub fn size() -> io::Result<(u16, u16)> {
        terminal::size()
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.leave();
    }
}
