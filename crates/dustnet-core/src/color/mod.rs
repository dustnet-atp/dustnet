mod palette;
#[cfg(test)]
mod tests;

use std::fmt;

/// A color value as specified in AML.
#[derive(Debug, Clone, PartialEq)]
pub enum Color {
    /// One of the 16 standard terminal colors.
    Named(NamedColor),
    /// A 256-color palette index: `color(N)` where N is 0–255.
    Palette(u8),
    /// 24-bit RGB truecolor: `#rrggbb`.
    Rgb { r: u8, g: u8, b: u8 },
}

/// The 16 standard terminal colors (8 normal + 8 bright).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NamedColor {
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    White,
    BrightBlack,
    BrightRed,
    BrightGreen,
    BrightYellow,
    BrightBlue,
    BrightMagenta,
    BrightCyan,
    BrightWhite,
}

impl NamedColor {
    /// The SGR parameter for this color as a foreground color (30–37, 90–97).
    pub fn fg_sgr(&self) -> u8 {
        match self {
            NamedColor::Black => 30,
            NamedColor::Red => 31,
            NamedColor::Green => 32,
            NamedColor::Yellow => 33,
            NamedColor::Blue => 34,
            NamedColor::Magenta => 35,
            NamedColor::Cyan => 36,
            NamedColor::White => 37,
            NamedColor::BrightBlack => 90,
            NamedColor::BrightRed => 91,
            NamedColor::BrightGreen => 92,
            NamedColor::BrightYellow => 93,
            NamedColor::BrightBlue => 94,
            NamedColor::BrightMagenta => 95,
            NamedColor::BrightCyan => 96,
            NamedColor::BrightWhite => 97,
        }
    }

    /// The SGR parameter for this color as a background color (40–47, 100–107).
    pub fn bg_sgr(&self) -> u8 {
        self.fg_sgr() + 10
    }

    /// Approximate RGB values for this named color.
    pub fn to_rgb(&self) -> (u8, u8, u8) {
        match self {
            NamedColor::Black => (0, 0, 0),
            NamedColor::Red => (170, 0, 0),
            NamedColor::Green => (0, 170, 0),
            NamedColor::Yellow => (170, 170, 0),
            NamedColor::Blue => (0, 0, 170),
            NamedColor::Magenta => (170, 0, 170),
            NamedColor::Cyan => (0, 170, 170),
            NamedColor::White => (170, 170, 170),
            NamedColor::BrightBlack => (85, 85, 85),
            NamedColor::BrightRed => (255, 85, 85),
            NamedColor::BrightGreen => (85, 255, 85),
            NamedColor::BrightYellow => (255, 255, 85),
            NamedColor::BrightBlue => (85, 85, 255),
            NamedColor::BrightMagenta => (255, 85, 255),
            NamedColor::BrightCyan => (85, 255, 255),
            NamedColor::BrightWhite => (255, 255, 255),
        }
    }
}

/// The level of color support negotiated with the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ColorSupport {
    /// No color support.
    None,
    /// 16 standard colors.
    Basic,
    /// 256-color palette.
    Palette256,
    /// 24-bit truecolor.
    Truecolor,
}

impl Color {
    /// Resolve this color to the given color support level.
    /// Downsamples truecolor → 256 → 16 as needed.
    pub fn resolve(&self, support: ColorSupport) -> Option<ResolvedColor> {
        match support {
            ColorSupport::None => None,
            ColorSupport::Truecolor => Some(match self {
                Color::Named(n) => {
                    let (r, g, b) = n.to_rgb();
                    ResolvedColor::Rgb(r, g, b)
                }
                Color::Palette(idx) => {
                    let (r, g, b) = palette::palette_to_rgb(*idx);
                    ResolvedColor::Rgb(r, g, b)
                }
                Color::Rgb { r, g, b } => ResolvedColor::Rgb(*r, *g, *b),
            }),
            ColorSupport::Palette256 => Some(match self {
                Color::Named(n) => ResolvedColor::Named(*n),
                Color::Palette(idx) => ResolvedColor::Palette(*idx),
                Color::Rgb { r, g, b } => {
                    ResolvedColor::Palette(palette::rgb_to_palette(*r, *g, *b))
                }
            }),
            ColorSupport::Basic => Some(match self {
                Color::Named(n) => ResolvedColor::Named(*n),
                Color::Palette(idx) => {
                    let (r, g, b) = palette::palette_to_rgb(*idx);
                    ResolvedColor::Named(palette::rgb_to_named(r, g, b))
                }
                Color::Rgb { r, g, b } => ResolvedColor::Named(palette::rgb_to_named(*r, *g, *b)),
            }),
        }
    }
}

/// A color that has been resolved to a specific terminal capability level.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ResolvedColor {
    Named(NamedColor),
    Palette(u8),
    Rgb(u8, u8, u8),
}

/// Parse a color string from AML.
///
/// Supported formats:
/// - Named: `red`, `cyan`, `bright-white`, etc.
/// - 256-color: `color(196)`
/// - Truecolor hex: `#ff6600`
pub fn parse_color(s: &str) -> Result<Color, ColorParseError> {
    let s = s.trim();

    // Hex color: #rrggbb
    if let Some(hex) = s.strip_prefix('#') {
        return parse_hex_color(hex);
    }

    // Palette color: color(N)
    if s.as_bytes()
        .get(..6)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"color("))
        && s.ends_with(')')
    {
        return parse_palette_color(&s[6..s.len() - 1]);
    }

    // Named color
    parse_named_color(s)
}

fn parse_hex_color(hex: &str) -> Result<Color, ColorParseError> {
    // Must be exactly 6 ASCII hex digits — check char count and ASCII-ness
    // to avoid panics when slicing multi-byte UTF-8 by byte index.
    if hex.len() != 6 || !hex.is_ascii() {
        return Err(ColorParseError::InvalidHex);
    }

    let r = u8::from_str_radix(&hex[0..2], 16).map_err(|_| ColorParseError::InvalidHex)?;
    let g = u8::from_str_radix(&hex[2..4], 16).map_err(|_| ColorParseError::InvalidHex)?;
    let b = u8::from_str_radix(&hex[4..6], 16).map_err(|_| ColorParseError::InvalidHex)?;

    Ok(Color::Rgb { r, g, b })
}

fn parse_palette_color(inner: &str) -> Result<Color, ColorParseError> {
    let idx: u16 = inner
        .trim()
        .parse()
        .map_err(|_| ColorParseError::InvalidPalette)?;

    if idx > 255 {
        return Err(ColorParseError::PaletteOutOfRange(idx));
    }

    Ok(Color::Palette(idx as u8))
}

fn parse_named_color(name: &str) -> Result<Color, ColorParseError> {
    let named = [
        ("black", NamedColor::Black),
        ("red", NamedColor::Red),
        ("green", NamedColor::Green),
        ("yellow", NamedColor::Yellow),
        ("blue", NamedColor::Blue),
        ("magenta", NamedColor::Magenta),
        ("cyan", NamedColor::Cyan),
        ("white", NamedColor::White),
        ("bright-black", NamedColor::BrightBlack),
        ("brightblack", NamedColor::BrightBlack),
        ("bright-red", NamedColor::BrightRed),
        ("brightred", NamedColor::BrightRed),
        ("bright-green", NamedColor::BrightGreen),
        ("brightgreen", NamedColor::BrightGreen),
        ("bright-yellow", NamedColor::BrightYellow),
        ("brightyellow", NamedColor::BrightYellow),
        ("bright-blue", NamedColor::BrightBlue),
        ("brightblue", NamedColor::BrightBlue),
        ("bright-magenta", NamedColor::BrightMagenta),
        ("brightmagenta", NamedColor::BrightMagenta),
        ("bright-cyan", NamedColor::BrightCyan),
        ("brightcyan", NamedColor::BrightCyan),
        ("bright-white", NamedColor::BrightWhite),
        ("brightwhite", NamedColor::BrightWhite),
    ]
    .into_iter()
    .find_map(|(candidate, color)| name.eq_ignore_ascii_case(candidate).then_some(color))
    .ok_or(ColorParseError::UnknownColor)?;
    Ok(Color::Named(named))
}

/// Errors that can occur when parsing a color value.
#[derive(Debug, Clone, PartialEq)]
pub enum ColorParseError {
    InvalidHex,
    InvalidPalette,
    PaletteOutOfRange(u16),
    UnknownColor,
}

impl fmt::Display for ColorParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ColorParseError::InvalidHex => write!(f, "invalid hex color"),
            ColorParseError::InvalidPalette => write!(f, "invalid palette index"),
            ColorParseError::PaletteOutOfRange(n) => {
                write!(f, "palette index {n} out of range (0–255)")
            }
            ColorParseError::UnknownColor => write!(f, "unknown color name"),
        }
    }
}

impl std::error::Error for ColorParseError {}
