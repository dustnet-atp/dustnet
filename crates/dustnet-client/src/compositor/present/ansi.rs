use crate::color::ResolvedColor;
use crate::compositor::layout::cell::CellStyle;
use std::io::{self, Write};

/// Physical fallback used when AML leaves either color unspecified.
///
/// Cells remain transparent inside the compositor; the fallback is applied
/// only while presenting the final frame so the host terminal's theme cannot
/// bleed through the client canvas.
pub const TERMINAL_DEFAULT_SGR: &str = "\x1b[38;2;255;255;255;48;2;0;0;0m";

/// Generate the SGR (Select Graphic Rendition) escape sequence for a cell style.
///
/// Returns the full escape sequence including the ESC[ prefix and 'm' suffix.
/// Missing colors are materialized as bright white on black. This is a
/// presentation-only default: it does not change cell transparency or
/// compositing semantics.
pub fn style_to_sgr(style: &CellStyle) -> String {
    let mut params: Vec<String> = Vec::new();

    if style.bold {
        params.push("1".into());
    }
    if style.dim {
        params.push("2".into());
    }
    if style.italic {
        params.push("3".into());
    }
    if style.underline {
        params.push("4".into());
    }
    if style.blink {
        params.push("5".into());
    }
    if style.strikethrough {
        params.push("9".into());
    }

    match &style.fg {
        Some(fg) => match fg {
            ResolvedColor::Named(n) => {
                params.push(n.fg_sgr().to_string());
            }
            ResolvedColor::Palette(idx) => {
                params.push(format!("38;5;{idx}"));
            }
            ResolvedColor::Rgb(r, g, b) => {
                params.push(format!("38;2;{r};{g};{b}"));
            }
        },
        None => params.push("38;2;255;255;255".into()),
    }

    match &style.bg {
        Some(bg) => match bg {
            ResolvedColor::Named(n) => {
                params.push(n.bg_sgr().to_string());
            }
            ResolvedColor::Palette(idx) => {
                params.push(format!("48;5;{idx}"));
            }
            ResolvedColor::Rgb(r, g, b) => {
                params.push(format!("48;2;{r};{g};{b}"));
            }
        },
        None => params.push("48;2;0;0;0".into()),
    }

    format!("\x1b[{}m", params.join(";"))
}

/// Write an SGR sequence without allocating an intermediate `String`.
pub fn write_style_sgr(out: &mut impl Write, style: &CellStyle) -> io::Result<()> {
    out.write_all(b"\x1b[")?;
    let mut first = true;
    macro_rules! parameter {
        ($($arg:tt)*) => {{
            if !first {
                out.write_all(b";")?;
            }
            first = false;
            write!(out, $($arg)*)?;
        }};
    }

    if style.bold {
        parameter!("1");
    }
    if style.dim {
        parameter!("2");
    }
    if style.italic {
        parameter!("3");
    }
    if style.underline {
        parameter!("4");
    }
    if style.blink {
        parameter!("5");
    }
    if style.strikethrough {
        parameter!("9");
    }

    match &style.fg {
        Some(ResolvedColor::Named(color)) => parameter!("{}", color.fg_sgr()),
        Some(ResolvedColor::Palette(index)) => parameter!("38;5;{index}"),
        Some(ResolvedColor::Rgb(red, green, blue)) => parameter!("38;2;{red};{green};{blue}"),
        None => parameter!("38;2;255;255;255"),
    }
    match &style.bg {
        Some(ResolvedColor::Named(color)) => parameter!("{}", color.bg_sgr()),
        Some(ResolvedColor::Palette(index)) => parameter!("48;5;{index}"),
        Some(ResolvedColor::Rgb(red, green, blue)) => parameter!("48;2;{red};{green};{blue}"),
        None => parameter!("48;2;0;0;0"),
    }
    debug_assert!(!first);
    out.write_all(b"m")
}

/// Write a 1-based terminal cursor position without allocating.
pub fn write_cursor(out: &mut impl Write, row: u16, col: u16) -> io::Result<()> {
    write!(out, "\x1b[{};{}H", row + 1, col + 1)
}

/// SGR reset sequence.
pub const RESET: &str = "\x1b[0m";

/// Move cursor to (row, col). Both are 1-based in ANSI.
pub fn move_cursor(row: u16, col: u16) -> String {
    format!("\x1b[{};{}H", row + 1, col + 1)
}

/// Hide the cursor.
pub const HIDE_CURSOR: &str = "\x1b[?25l";

/// Show the cursor.
pub const SHOW_CURSOR: &str = "\x1b[?25h";

/// Switch to the alternate screen buffer.
pub const ENTER_ALT_SCREEN: &str = "\x1b[?1049h";

/// Switch back to the main screen buffer.
pub const LEAVE_ALT_SCREEN: &str = "\x1b[?1049l";

/// Clear the entire screen.
pub const CLEAR_SCREEN: &str = "\x1b[2J";

/// Move cursor to top-left.
pub const CURSOR_HOME: &str = "\x1b[H";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::NamedColor;

    #[test]
    fn default_style_uses_physical_dark_canvas() {
        let style = CellStyle::default();
        assert_eq!(style_to_sgr(&style), TERMINAL_DEFAULT_SGR);
    }

    #[test]
    fn bold_only() {
        let style = CellStyle {
            bold: true,
            ..Default::default()
        };
        assert_eq!(style_to_sgr(&style), "\x1b[1;38;2;255;255;255;48;2;0;0;0m");
    }

    #[test]
    fn dim_only() {
        let style = CellStyle {
            dim: true,
            ..Default::default()
        };
        assert_eq!(style_to_sgr(&style), "\x1b[2;38;2;255;255;255;48;2;0;0;0m");
    }

    #[test]
    fn italic_only() {
        let style = CellStyle {
            italic: true,
            ..Default::default()
        };
        assert_eq!(style_to_sgr(&style), "\x1b[3;38;2;255;255;255;48;2;0;0;0m");
    }

    #[test]
    fn underline_only() {
        let style = CellStyle {
            underline: true,
            ..Default::default()
        };
        assert_eq!(style_to_sgr(&style), "\x1b[4;38;2;255;255;255;48;2;0;0;0m");
    }

    #[test]
    fn blink_only() {
        let style = CellStyle {
            blink: true,
            ..Default::default()
        };
        assert_eq!(style_to_sgr(&style), "\x1b[5;38;2;255;255;255;48;2;0;0;0m");
    }

    #[test]
    fn strikethrough_only() {
        let style = CellStyle {
            strikethrough: true,
            ..Default::default()
        };
        assert_eq!(style_to_sgr(&style), "\x1b[9;38;2;255;255;255;48;2;0;0;0m");
    }

    #[test]
    fn named_fg_color() {
        let style = CellStyle {
            fg: Some(ResolvedColor::Named(NamedColor::Red)),
            ..Default::default()
        };
        assert_eq!(style_to_sgr(&style), "\x1b[31;48;2;0;0;0m");
    }

    #[test]
    fn named_bg_color() {
        let style = CellStyle {
            bg: Some(ResolvedColor::Named(NamedColor::Blue)),
            ..Default::default()
        };
        assert_eq!(style_to_sgr(&style), "\x1b[38;2;255;255;255;44m");
    }

    #[test]
    fn bright_fg_color() {
        let style = CellStyle {
            fg: Some(ResolvedColor::Named(NamedColor::BrightCyan)),
            ..Default::default()
        };
        assert_eq!(style_to_sgr(&style), "\x1b[96;48;2;0;0;0m");
    }

    #[test]
    fn palette_fg_color() {
        let style = CellStyle {
            fg: Some(ResolvedColor::Palette(196)),
            ..Default::default()
        };
        assert_eq!(style_to_sgr(&style), "\x1b[38;5;196;48;2;0;0;0m");
    }

    #[test]
    fn palette_bg_color() {
        let style = CellStyle {
            bg: Some(ResolvedColor::Palette(42)),
            ..Default::default()
        };
        assert_eq!(style_to_sgr(&style), "\x1b[38;2;255;255;255;48;5;42m");
    }

    #[test]
    fn rgb_fg_color() {
        let style = CellStyle {
            fg: Some(ResolvedColor::Rgb(255, 102, 0)),
            ..Default::default()
        };
        assert_eq!(style_to_sgr(&style), "\x1b[38;2;255;102;0;48;2;0;0;0m");
    }

    #[test]
    fn rgb_bg_color() {
        let style = CellStyle {
            bg: Some(ResolvedColor::Rgb(0, 0, 128)),
            ..Default::default()
        };
        assert_eq!(style_to_sgr(&style), "\x1b[38;2;255;255;255;48;2;0;0;128m");
    }

    #[test]
    fn combined_styles() {
        let style = CellStyle {
            fg: Some(ResolvedColor::Named(NamedColor::Red)),
            bg: Some(ResolvedColor::Named(NamedColor::Black)),
            bold: true,
            underline: true,
            ..Default::default()
        };
        let sgr = style_to_sgr(&style);
        assert!(sgr.starts_with("\x1b["));
        assert!(sgr.ends_with('m'));
        assert!(sgr.contains("1")); // bold
        assert!(sgr.contains("4")); // underline
        assert!(sgr.contains("31")); // red fg
        assert!(sgr.contains("40")); // black bg
    }

    #[test]
    fn all_styles_at_once() {
        let style = CellStyle {
            fg: Some(ResolvedColor::Rgb(255, 0, 0)),
            bg: Some(ResolvedColor::Rgb(0, 0, 255)),
            bold: true,
            dim: true,
            italic: true,
            underline: true,
            blink: true,
            strikethrough: true,
        };
        let sgr = style_to_sgr(&style);
        // Should contain all attributes
        assert!(sgr.contains("1")); // bold
        assert!(sgr.contains("2")); // dim
        assert!(sgr.contains("3")); // italic
        assert!(sgr.contains("4")); // underline
        assert!(sgr.contains("5")); // blink
        assert!(sgr.contains("9")); // strikethrough
        assert!(sgr.contains("38;2;255;0;0")); // fg
        assert!(sgr.contains("48;2;0;0;255")); // bg
        let mut direct = Vec::new();
        write_style_sgr(&mut direct, &style).unwrap();
        assert_eq!(direct, sgr.as_bytes());
    }

    #[test]
    fn move_cursor_values() {
        // (0,0) should produce ESC[1;1H (1-based)
        assert_eq!(move_cursor(0, 0), "\x1b[1;1H");
        assert_eq!(move_cursor(5, 10), "\x1b[6;11H");
        assert_eq!(move_cursor(23, 79), "\x1b[24;80H");
    }

    #[test]
    fn constants_are_valid_escape_sequences() {
        assert!(RESET.starts_with("\x1b["));
        assert!(HIDE_CURSOR.starts_with("\x1b["));
        assert!(SHOW_CURSOR.starts_with("\x1b["));
        assert!(ENTER_ALT_SCREEN.starts_with("\x1b["));
        assert!(LEAVE_ALT_SCREEN.starts_with("\x1b["));
        assert!(CLEAR_SCREEN.starts_with("\x1b["));
        assert!(CURSOR_HOME.starts_with("\x1b["));
    }
}
