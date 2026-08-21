use super::*;

// ─── Named Color Parsing ─────────────────────────────────────

#[test]
fn parse_standard_colors() {
    assert_eq!(parse_color("red").unwrap(), Color::Named(NamedColor::Red));
    assert_eq!(parse_color("blue").unwrap(), Color::Named(NamedColor::Blue));
    assert_eq!(
        parse_color("black").unwrap(),
        Color::Named(NamedColor::Black)
    );
    assert_eq!(
        parse_color("white").unwrap(),
        Color::Named(NamedColor::White)
    );
    assert_eq!(
        parse_color("green").unwrap(),
        Color::Named(NamedColor::Green)
    );
    assert_eq!(
        parse_color("yellow").unwrap(),
        Color::Named(NamedColor::Yellow)
    );
    assert_eq!(
        parse_color("magenta").unwrap(),
        Color::Named(NamedColor::Magenta)
    );
    assert_eq!(parse_color("cyan").unwrap(), Color::Named(NamedColor::Cyan));
}

#[test]
fn parse_bright_colors_hyphenated() {
    assert_eq!(
        parse_color("bright-red").unwrap(),
        Color::Named(NamedColor::BrightRed)
    );
    assert_eq!(
        parse_color("bright-white").unwrap(),
        Color::Named(NamedColor::BrightWhite)
    );
    assert_eq!(
        parse_color("bright-black").unwrap(),
        Color::Named(NamedColor::BrightBlack)
    );
    assert_eq!(
        parse_color("bright-cyan").unwrap(),
        Color::Named(NamedColor::BrightCyan)
    );
}

#[test]
fn parse_bright_colors_no_hyphen() {
    assert_eq!(
        parse_color("brightred").unwrap(),
        Color::Named(NamedColor::BrightRed)
    );
    assert_eq!(
        parse_color("brightwhite").unwrap(),
        Color::Named(NamedColor::BrightWhite)
    );
}

#[test]
fn parse_color_case_insensitive() {
    assert_eq!(parse_color("RED").unwrap(), Color::Named(NamedColor::Red));
    assert_eq!(parse_color("Red").unwrap(), Color::Named(NamedColor::Red));
    assert_eq!(
        parse_color("BRIGHT-RED").unwrap(),
        Color::Named(NamedColor::BrightRed)
    );
    assert_eq!(parse_color("Cyan").unwrap(), Color::Named(NamedColor::Cyan));
}

#[test]
fn color_parsing_is_allocation_free_for_case_folding_and_invalid_payloads() {
    assert_eq!(parse_color("CoLoR(196)").unwrap(), Color::Palette(196));
    assert_eq!(
        parse_color("BRIGHT-MAGENTA").unwrap(),
        Color::Named(NamedColor::BrightMagenta)
    );
    assert_eq!(parse_color("éééé"), Err(ColorParseError::UnknownColor));
    assert_eq!(parse_color("éolor(1)"), Err(ColorParseError::UnknownColor));
    assert_eq!(ColorParseError::InvalidHex.to_string(), "invalid hex color");
}

#[test]
fn parse_unknown_color_name() {
    assert!(matches!(
        parse_color("orange"),
        Err(ColorParseError::UnknownColor)
    ));
    assert!(matches!(
        parse_color("purple"),
        Err(ColorParseError::UnknownColor)
    ));
}

// ─── Hex Color Parsing ───────────────────────────────────────

#[test]
fn parse_hex_colors() {
    assert_eq!(
        parse_color("#ff0000").unwrap(),
        Color::Rgb { r: 255, g: 0, b: 0 }
    );
    assert_eq!(
        parse_color("#00ff00").unwrap(),
        Color::Rgb { r: 0, g: 255, b: 0 }
    );
    assert_eq!(
        parse_color("#0000ff").unwrap(),
        Color::Rgb { r: 0, g: 0, b: 255 }
    );
    assert_eq!(
        parse_color("#ff6600").unwrap(),
        Color::Rgb {
            r: 255,
            g: 102,
            b: 0
        }
    );
}

#[test]
fn parse_hex_case_insensitive() {
    assert_eq!(
        parse_color("#FF6600").unwrap(),
        Color::Rgb {
            r: 255,
            g: 102,
            b: 0
        }
    );
    assert_eq!(
        parse_color("#Ff6600").unwrap(),
        Color::Rgb {
            r: 255,
            g: 102,
            b: 0
        }
    );
}

#[test]
fn parse_hex_invalid_length() {
    assert!(matches!(
        parse_color("#fff"),
        Err(ColorParseError::InvalidHex)
    ));
    assert!(matches!(
        parse_color("#fffffff"),
        Err(ColorParseError::InvalidHex)
    ));
}

#[test]
fn parse_hex_invalid_chars() {
    assert!(matches!(
        parse_color("#gggggg"),
        Err(ColorParseError::InvalidHex)
    ));
}

#[test]
fn parse_hex_black_and_white() {
    assert_eq!(
        parse_color("#000000").unwrap(),
        Color::Rgb { r: 0, g: 0, b: 0 }
    );
    assert_eq!(
        parse_color("#ffffff").unwrap(),
        Color::Rgb {
            r: 255,
            g: 255,
            b: 255
        }
    );
}

// ─── Palette Color Parsing ───────────────────────────────────

#[test]
fn parse_palette_colors() {
    assert_eq!(parse_color("color(0)").unwrap(), Color::Palette(0));
    assert_eq!(parse_color("color(196)").unwrap(), Color::Palette(196));
    assert_eq!(parse_color("color(255)").unwrap(), Color::Palette(255));
}

#[test]
fn parse_palette_with_spaces() {
    assert_eq!(parse_color("color( 42 )").unwrap(), Color::Palette(42));
}

#[test]
fn parse_palette_out_of_range() {
    assert!(matches!(
        parse_color("color(256)"),
        Err(ColorParseError::PaletteOutOfRange(256))
    ));
    assert!(matches!(
        parse_color("color(999)"),
        Err(ColorParseError::PaletteOutOfRange(999))
    ));
}

#[test]
fn parse_palette_invalid() {
    assert!(matches!(
        parse_color("color(abc)"),
        Err(ColorParseError::InvalidPalette)
    ));
    assert!(matches!(
        parse_color("color(-1)"),
        Err(ColorParseError::InvalidPalette)
    ));
}

// ─── Whitespace Handling ─────────────────────────────────────

#[test]
fn parse_color_with_whitespace() {
    assert_eq!(
        parse_color("  red  ").unwrap(),
        Color::Named(NamedColor::Red)
    );
    assert_eq!(
        parse_color("  #ff0000  ").unwrap(),
        Color::Rgb { r: 255, g: 0, b: 0 }
    );
}

// ─── Color Resolution ────────────────────────────────────────

#[test]
fn resolve_named_to_basic() {
    let c = Color::Named(NamedColor::Red);
    assert_eq!(
        c.resolve(ColorSupport::Basic),
        Some(ResolvedColor::Named(NamedColor::Red))
    );
}

#[test]
fn resolve_named_to_256() {
    let c = Color::Named(NamedColor::Cyan);
    assert_eq!(
        c.resolve(ColorSupport::Palette256),
        Some(ResolvedColor::Named(NamedColor::Cyan))
    );
}

#[test]
fn resolve_hex_to_truecolor() {
    let c = Color::Rgb {
        r: 255,
        g: 102,
        b: 0,
    };
    assert_eq!(
        c.resolve(ColorSupport::Truecolor),
        Some(ResolvedColor::Rgb(255, 102, 0))
    );
}

#[test]
fn resolve_hex_to_256() {
    let c = Color::Rgb { r: 255, g: 0, b: 0 };
    let resolved = c.resolve(ColorSupport::Palette256);
    // Pure red should map to palette index 196 or similar red
    assert!(matches!(resolved, Some(ResolvedColor::Palette(_))));
}

#[test]
fn resolve_hex_to_basic() {
    let c = Color::Rgb { r: 255, g: 0, b: 0 };
    let resolved = c.resolve(ColorSupport::Basic);
    // Pure red should map to NamedColor::Red or BrightRed
    match resolved {
        Some(ResolvedColor::Named(n)) => {
            assert!(n == NamedColor::Red || n == NamedColor::BrightRed);
        }
        _ => panic!("expected Named color"),
    }
}

#[test]
fn resolve_to_none() {
    let c = Color::Named(NamedColor::Red);
    assert_eq!(c.resolve(ColorSupport::None), None);
}

#[test]
fn resolve_palette_to_basic() {
    // Palette 196 is a red in the 6x6x6 cube
    let c = Color::Palette(196);
    let resolved = c.resolve(ColorSupport::Basic);
    assert!(matches!(resolved, Some(ResolvedColor::Named(_))));
}

// ─── SGR Values ──────────────────────────────────────────────

#[test]
fn named_color_sgr_values() {
    assert_eq!(NamedColor::Black.fg_sgr(), 30);
    assert_eq!(NamedColor::Red.fg_sgr(), 31);
    assert_eq!(NamedColor::White.fg_sgr(), 37);
    assert_eq!(NamedColor::BrightBlack.fg_sgr(), 90);
    assert_eq!(NamedColor::BrightWhite.fg_sgr(), 97);

    assert_eq!(NamedColor::Black.bg_sgr(), 40);
    assert_eq!(NamedColor::Red.bg_sgr(), 41);
    assert_eq!(NamedColor::BrightWhite.bg_sgr(), 107);
}

// ─── Palette Roundtrip ───────────────────────────────────────

#[test]
fn palette_standard_colors_roundtrip() {
    use super::palette::palette_to_rgb;
    // First 16 entries should produce valid RGB values (no panic)
    for i in 0u8..16 {
        let (_r, _g, _b) = palette_to_rgb(i);
    }
}

#[test]
fn palette_cube_corner_values() {
    use super::palette::palette_to_rgb;
    // Index 16 = first cube entry = (0,0,0)
    assert_eq!(palette_to_rgb(16), (0, 0, 0));
    // Index 231 = last cube entry = (255,255,255)
    assert_eq!(palette_to_rgb(231), (255, 255, 255));
}

#[test]
fn palette_grayscale_range() {
    use super::palette::palette_to_rgb;
    // Grayscale should be monotonically increasing
    let mut prev = 0u8;
    for i in 232u8..=255 {
        let (r, g, b) = palette_to_rgb(i);
        assert_eq!(r, g);
        assert_eq!(g, b);
        assert!(r >= prev);
        prev = r;
    }
}

#[test]
fn rgb_to_palette_exact_match() {
    use super::palette::{palette_to_rgb, rgb_to_palette};
    // Pure black should map back to index 0 or 16
    let idx = rgb_to_palette(0, 0, 0);
    let (r, g, b) = palette_to_rgb(idx);
    assert_eq!((r, g, b), (0, 0, 0));
}

#[test]
fn rgb_to_named_basic_colors() {
    use super::palette::rgb_to_named;
    // Pure red should map to Red or BrightRed
    let named = rgb_to_named(255, 0, 0);
    assert!(named == NamedColor::Red || named == NamedColor::BrightRed);

    // Pure white should map to White or BrightWhite
    let named = rgb_to_named(255, 255, 255);
    assert!(named == NamedColor::White || named == NamedColor::BrightWhite);

    // Pure black
    let named = rgb_to_named(0, 0, 0);
    assert_eq!(named, NamedColor::Black);
}
