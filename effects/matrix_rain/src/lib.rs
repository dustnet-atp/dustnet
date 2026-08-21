//! Matrix rain WASM animation effect.
//!
//! Falling green characters in columns, each with a bright head, green trail,
//! and dim tail. Characters near the head flicker between frames.
//!
//! Build: cargo build --target wasm32-unknown-unknown --release
#![no_std]

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}

// ─── Host imports ───────────────────────────────────────────

extern "C" {
    fn set_cell(x: i32, y: i32, codepoint: i32, fg: i32, bg: i32, style: i32);
    fn clear();
    fn get_width() -> i32;
    fn get_height() -> i32;
    fn random() -> i32;
}

// ─── Color/style constants ──────────────────────────────────

// RGB color encoding: 0x01RRGGBB. Use literal black rather than the terminal's
// configurable ANSI black so light and tinted palettes cannot show through.
const BLACK_BG: i32 = 0x0100_0000_u32 as i32; // RGB(0, 0, 0)

// Named color encoding: 0x020000NN
const GREEN_FG: i32 = 0x0200_0002_u32 as i32; // NamedColor::Green = 2
const BRIGHT_GREEN_FG: i32 = 0x0200_000A_u32 as i32; // NamedColor::BrightGreen = 10
const BRIGHT_WHITE_FG: i32 = 0x0200_000F_u32 as i32; // NamedColor::BrightWhite = 15

// Style bits
const BOLD: i32 = 1;
const DIM: i32 = 1 << 4;

// Character set for rain
const CHARS: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ@#$%&*+=<>?/|~^";

// ─── State ──────────────────────────────────────────────────

// Per-column configuration (packed into a flat array for no_std)
// Layout: [speed, trail_len, period, offset] per column
static mut COL_CFG: [u32; 256 * 4] = [0; 256 * 4]; // max 256 columns
static mut CHAR_GRID: [u8; 256 * 256] = [0; 256 * 256]; // max 256x256
static mut WIDTH: i32 = 0;
static mut HEIGHT: i32 = 0;

// ─── Exports ────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn init(w: i32, h: i32) -> i32 {
    unsafe {
        WIDTH = w.min(256);
        HEIGHT = h.min(256);

        let h_third = (HEIGHT as u32 / 3).max(1);

        for col in 0..WIDTH as usize {
            let r1 = random() as u32;
            let r2 = random() as u32;
            let r3 = random() as u32;
            let r4 = random() as u32;

            let speed = if r1 % 3 == 0 { 2 } else { 1 };
            let trail_len = h_third + (r2 % h_third);
            let gap = h_third + (r3 % h_third);
            let period = HEIGHT as u32 + trail_len + gap;
            let offset = r4 % period;

            COL_CFG[col * 4] = speed;
            COL_CFG[col * 4 + 1] = trail_len;
            COL_CFG[col * 4 + 2] = period;
            COL_CFG[col * 4 + 3] = offset;

            // Fill stable character grid for this column
            for row in 0..HEIGHT as usize {
                let r = random() as u32;
                CHAR_GRID[col * 256 + row] = CHARS[(r as usize) % CHARS.len()];
            }
        }
    }

    0 // infinite frames
}

#[no_mangle]
pub extern "C" fn tick(frame: i32) -> i32 {
    let fi = frame as u32;
    unsafe {
        clear();

        for col in 0..WIDTH as usize {
            let speed = COL_CFG[col * 4];
            let trail_len = COL_CFG[col * 4 + 1];
            let period = COL_CFG[col * 4 + 2];
            let offset = COL_CFG[col * 4 + 3];

            let head_pos = (fi * speed + offset) % period;

            for row in 0..HEIGHT as usize {
                let dist_i = head_pos as i32 - row as i32;
                if dist_i < 0 || dist_i as u32 > trail_len {
                    set_cell(col as i32, row as i32, b' ' as i32, 0, BLACK_BG, 0);
                    continue;
                }
                let dist = dist_i as u32;

                // Characters near the head flicker; rest are stable
                let ch = if dist < 4 {
                    let mix = (fi as u64)
                        .wrapping_mul(7)
                        .wrapping_add(col as u64 * 13)
                        .wrapping_add(row as u64 * 37);
                    CHARS[(mix as usize) % CHARS.len()]
                } else {
                    CHAR_GRID[col * 256 + row]
                };

                let (fg, style) = if dist == 0 {
                    (BRIGHT_WHITE_FG, BOLD)
                } else if dist <= 3 {
                    (BRIGHT_GREEN_FG, 0)
                } else if dist <= trail_len * 2 / 3 {
                    (GREEN_FG, 0)
                } else {
                    (GREEN_FG, DIM)
                };

                set_cell(col as i32, row as i32, ch as i32, fg, BLACK_BG, style);
            }
        }
    }

    0 // continue
}

#[no_mangle]
pub extern "C" fn resize(w: i32, h: i32) {
    init(w, h);
}
