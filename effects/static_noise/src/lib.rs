//! Static noise WASM animation effect.
//!
//! Sparse, dim, flickering dots scattered across the screen — like
//! faint CRT static. A subtle sweep line drifts down periodically.
//!
//! Build: cargo build --target wasm32-unknown-unknown --release
#![no_std]

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}

// ─── Host imports ───────────────────────────────────────────

#[allow(dead_code)]
extern "C" {
    fn set_cell(x: i32, y: i32, codepoint: i32, fg: i32, bg: i32, style: i32);
    fn clear();
    fn get_width() -> i32;
    fn get_height() -> i32;
    fn random() -> i32;
}

// ─── Color/style constants ──────────────────────────────────

// Truecolor encoding: 0x01RRGGBB
const fn rgb(r: u32, g: u32, b: u32) -> i32 {
    (0x0100_0000 | (r << 16) | (g << 8) | b) as i32
}

// Literal black rather than the terminal's configurable ANSI black, so light
// and tinted palettes cannot show through the static.
const BLACK_BG: i32 = rgb(0, 0, 0);

const GREY_VERY_DIM: i32 = rgb(26, 29, 31);
const GREY_DIM: i32 = rgb(52, 58, 61);
const GREY_MID: i32 = rgb(90, 99, 102);
const _GREY_LIGHT: i32 = rgb(145, 155, 158);

// Style bits
const DIM: i32 = 1 << 4;

// Dot characters — subtle punctuation marks
const DOTS: &[u8] = b".,:`'";

// ─── State ──────────────────────────────────────────────────

static mut WIDTH: i32 = 0;
static mut HEIGHT: i32 = 0;

// ─── Exports ────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn init(w: i32, h: i32) -> i32 {
    unsafe {
        WIDTH = w.min(256);
        HEIGHT = h.min(256);
    }
    0 // infinite frames
}

#[no_mangle]
pub extern "C" fn tick(frame: i32) -> i32 {
    let fi = frame as u32;
    unsafe {
        clear();

        let total_cells = (WIDTH * HEIGHT) as u32;
        // ~3% coverage — sparse static
        let num_dots = total_cells / 35;

        for _ in 0..num_dots {
            let r1 = random() as u32;
            let r2 = random() as u32;
            let r3 = random() as u32;

            let x = (r1 % WIDTH as u32) as i32;
            let y = (r2 % HEIGHT as u32) as i32;
            let ch = DOTS[(r3 as usize) % DOTS.len()];

            // Vary brightness randomly
            let brightness = r3 % 4;
            let fg = match brightness {
                0 => GREY_VERY_DIM,
                1 => GREY_DIM,
                2 => GREY_MID,
                _ => GREY_VERY_DIM,
            };

            set_cell(x, y, ch as i32, fg, BLACK_BG, DIM);
        }

        // Sweep line — a faint horizontal band that drifts down
        if HEIGHT > 0 {
            let line_y = (fi % HEIGHT as u32) as i32;

            for x in 0..WIDTH {
                let r = random() as u32;
                // Sparse — only some cells on the sweep line
                if r % 3 == 0 {
                    let ch = DOTS[(r as usize) % DOTS.len()];
                    set_cell(x, line_y, ch as i32, GREY_MID, BLACK_BG, 0);
                }
                // Faint echo one row behind
                let echo_y = if line_y > 0 { line_y - 1 } else { HEIGHT - 1 };
                if r % 5 == 0 {
                    let ch = DOTS[((r >> 2) as usize) % DOTS.len()];
                    set_cell(x, echo_y, ch as i32, GREY_VERY_DIM, BLACK_BG, DIM);
                }
            }
        }
    }

    0 // continue
}

#[no_mangle]
pub extern "C" fn resize(w: i32, h: i32) {
    init(w, h);
}
