//! Typewriter WASM animation effect.
//!
//! Reads content from the host's content buffer via get_content_cell(),
//! then progressively reveals characters left-to-right, top-to-bottom.
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
    fn get_content_cell(x: i32, y: i32) -> i64;
}

// ─── State ──────────────────────────────────────────────────

// Store content cell data: (codepoint, fg, style) per position.
// Layout: [codepoint, fg, style] per cell, row-major.
// Max 256x256 = 65536 cells * 3 = 196608 entries.
static mut CELL_DATA: [u32; 196608] = [0; 196608];
// Positions in reading order that have content.
static mut POSITIONS: [u32; 65536] = [0; 65536]; // packed (x | y << 16)
static mut NUM_POSITIONS: u32 = 0;
static mut WIDTH: i32 = 0;
static mut HEIGHT: i32 = 0;

// ─── Exports ────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn init(w: i32, h: i32) -> i32 {
    unsafe {
        WIDTH = w.min(256);
        HEIGHT = h.min(256);
        NUM_POSITIONS = 0;

        // Scan content buffer and record positions with content
        for y in 0..HEIGHT {
            // Find rightmost non-empty cell on this row
            let mut row_end: i32 = -1;
            for x in (0..WIDTH).rev() {
                let packed = get_content_cell(x, y);
                if packed != 0 {
                    let cp = (packed as u64) & 0x1FFFFF;
                    if cp != 0x20 {
                        // Not a space
                        row_end = x;
                        break;
                    }
                }
            }

            if row_end >= 0 {
                for x in 0..=row_end {
                    let packed = get_content_cell(x, y);
                    let cp = (packed as u64) & 0x1FFFFF;
                    let fg = ((packed as u64 >> 21) & 0xFFFFFFFF) as u32;
                    let style = ((packed as u64 >> 53) & 0x3F) as u32;

                    let idx = (y as usize * 256 + x as usize) * 3;
                    CELL_DATA[idx] = cp as u32;
                    CELL_DATA[idx + 1] = fg;
                    CELL_DATA[idx + 2] = style;

                    let pos_idx = NUM_POSITIONS as usize;
                    POSITIONS[pos_idx] = x as u32 | ((y as u32) << 16);
                    NUM_POSITIONS += 1;
                }
            }
        }

        if NUM_POSITIONS == 0 {
            return 0;
        }

        // Return total frames: 1 + num_positions (frame 0 is blank)
        (NUM_POSITIONS + 1) as i32
    }
}

#[no_mangle]
pub extern "C" fn tick(frame: i32) -> i32 {
    unsafe {
        clear();

        if frame == 0 || NUM_POSITIONS == 0 {
            // Frame 0: blank
            return if NUM_POSITIONS == 0 { 1 } else { 0 };
        }

        // Reveal up to `frame` positions
        let reveal_count = (frame as u32).min(NUM_POSITIONS);

        for i in 0..reveal_count as usize {
            let packed_pos = POSITIONS[i];
            let x = (packed_pos & 0xFFFF) as i32;
            let y = (packed_pos >> 16) as i32;

            let idx = (y as usize * 256 + x as usize) * 3;
            let cp = CELL_DATA[idx] as i32;
            let fg = CELL_DATA[idx + 1] as i32;
            let style = CELL_DATA[idx + 2] as i32;
            let bg = 0; // transparent

            set_cell(x, y, cp, fg, bg, style);
        }

        // Signal finished when all positions revealed
        if reveal_count >= NUM_POSITIONS { 1 } else { 0 }
    }
}

#[no_mangle]
pub extern "C" fn resize(w: i32, h: i32) {
    init(w, h);
}
