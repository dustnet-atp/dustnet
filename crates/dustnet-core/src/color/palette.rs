use super::NamedColor;

/// Convert a 256-color palette index to approximate RGB.
pub fn palette_to_rgb(idx: u8) -> (u8, u8, u8) {
    match idx {
        // Standard colors (0–7): same as NamedColor
        0 => (0, 0, 0),
        1 => (170, 0, 0),
        2 => (0, 170, 0),
        3 => (170, 170, 0),
        4 => (0, 0, 170),
        5 => (170, 0, 170),
        6 => (0, 170, 170),
        7 => (170, 170, 170),
        // Bright colors (8–15)
        8 => (85, 85, 85),
        9 => (255, 85, 85),
        10 => (85, 255, 85),
        11 => (255, 255, 85),
        12 => (85, 85, 255),
        13 => (255, 85, 255),
        14 => (85, 255, 255),
        15 => (255, 255, 255),
        // 216-color cube (16–231): 6x6x6
        16..=231 => {
            let idx = idx - 16;
            let b = idx % 6;
            let g = (idx / 6) % 6;
            let r = idx / 36;
            (
                if r == 0 { 0 } else { 55 + 40 * r },
                if g == 0 { 0 } else { 55 + 40 * g },
                if b == 0 { 0 } else { 55 + 40 * b },
            )
        }
        // Grayscale ramp (232–255): 24 shades
        232..=255 => {
            let v = 8 + 10 * (idx - 232);
            (v, v, v)
        }
    }
}

/// Find the nearest 256-color palette entry for an RGB value.
pub fn rgb_to_palette(r: u8, g: u8, b: u8) -> u8 {
    let mut best_idx: u8 = 0;
    let mut best_dist = u32::MAX;

    for idx in 0u16..=255 {
        let (pr, pg, pb) = palette_to_rgb(idx as u8);
        let dist = color_distance(r, g, b, pr, pg, pb);
        if dist < best_dist {
            best_dist = dist;
            best_idx = idx as u8;
            if dist == 0 {
                break;
            }
        }
    }

    best_idx
}

/// Find the nearest named color for an RGB value.
pub fn rgb_to_named(r: u8, g: u8, b: u8) -> NamedColor {
    const ALL_NAMED: [NamedColor; 16] = [
        NamedColor::Black,
        NamedColor::Red,
        NamedColor::Green,
        NamedColor::Yellow,
        NamedColor::Blue,
        NamedColor::Magenta,
        NamedColor::Cyan,
        NamedColor::White,
        NamedColor::BrightBlack,
        NamedColor::BrightRed,
        NamedColor::BrightGreen,
        NamedColor::BrightYellow,
        NamedColor::BrightBlue,
        NamedColor::BrightMagenta,
        NamedColor::BrightCyan,
        NamedColor::BrightWhite,
    ];

    let mut best = NamedColor::Black;
    let mut best_dist = u32::MAX;

    for named in &ALL_NAMED {
        let (nr, ng, nb) = named.to_rgb();
        let dist = color_distance(r, g, b, nr, ng, nb);
        if dist < best_dist {
            best_dist = dist;
            best = *named;
            if dist == 0 {
                break;
            }
        }
    }

    best
}

/// Weighted Euclidean distance in RGB space.
/// Uses the "redmean" approximation for perceptual distance:
/// human eyes are more sensitive to green, less to blue.
fn color_distance(r1: u8, g1: u8, b1: u8, r2: u8, g2: u8, b2: u8) -> u32 {
    let rmean = (r1 as i32 + r2 as i32) / 2;
    let dr = r1 as i32 - r2 as i32;
    let dg = g1 as i32 - g2 as i32;
    let db = b1 as i32 - b2 as i32;

    let wr = 2 + rmean / 256;
    let wg = 4;
    let wb = 2 + (255 - rmean) / 256;

    (wr * dr * dr + wg * dg * dg + wb * db * db) as u32
}
