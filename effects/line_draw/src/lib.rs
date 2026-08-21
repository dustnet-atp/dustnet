//! Progressively draws authored connector paths.
//!
//! Put box-drawing glyphs in the animation's child content. Empty cells are
//! skipped, while every visible cell keeps its original glyph, colour, and
//! style. The default build reveals one cell at a time in reading order. The
//! `top-down` feature reveals a complete row per frame for a vertical wipe.
#![no_std]

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}

extern "C" {
    fn set_cell(x: i32, y: i32, codepoint: i32, fg: i32, bg: i32, style: i32);
    fn clear();
    fn get_content_cell(x: i32, y: i32) -> i64;
}

const MAX_CELLS: usize = 65_536;
const TRANSPARENT: i32 = 0;
static mut POSITIONS: [u32; MAX_CELLS] = [0; MAX_CELLS];
static mut CELLS: [i64; MAX_CELLS] = [0; MAX_CELLS];
static mut CELL_COUNT: usize = 0;
#[cfg(feature = "top-down")]
static mut ROW_COUNT: usize = 0;

#[no_mangle]
pub extern "C" fn init(width: i32, height: i32) -> i32 {
    unsafe {
        CELL_COUNT = 0;
        #[cfg(feature = "top-down")]
        {
            ROW_COUNT = 0;
        }
        let width = width.clamp(0, 256);
        let height = height.clamp(0, 256);

        for y in 0..height {
            for x in 0..width {
                let packed = get_content_cell(x, y);
                let codepoint = (packed as u64 & 0x1f_ffff) as u32;
                if packed == 0 || codepoint == 0 || codepoint == b' ' as u32 {
                    continue;
                }
                if CELL_COUNT >= MAX_CELLS {
                    break;
                }
                POSITIONS[CELL_COUNT] = x as u32 | ((y as u32) << 16);
                CELLS[CELL_COUNT] = packed;
                CELL_COUNT += 1;
                #[cfg(feature = "top-down")]
                {
                    ROW_COUNT = (y as usize) + 1;
                }
            }
        }

        // Frame zero is blank; subsequent frames reveal cells or whole rows.
        #[cfg(feature = "top-down")]
        {
            (ROW_COUNT + 1) as i32
        }
        #[cfg(not(feature = "top-down"))]
        {
            (CELL_COUNT + 1) as i32
        }
    }
}

#[no_mangle]
pub extern "C" fn tick(frame: i32) -> i32 {
    unsafe {
        clear();
        #[cfg(not(feature = "top-down"))]
        let visible = (frame.max(0) as usize).min(CELL_COUNT);
        #[cfg(feature = "top-down")]
        let visible_rows = (frame.max(0) as usize).min(ROW_COUNT);

        for index in 0..CELL_COUNT {
            let position = POSITIONS[index];
            #[cfg(not(feature = "top-down"))]
            if index >= visible {
                break;
            }
            #[cfg(feature = "top-down")]
            if ((position >> 16) as usize) >= visible_rows {
                continue;
            }
            let packed = CELLS[index] as u64;
            let x = (position & 0xffff) as i32;
            let y = (position >> 16) as i32;
            let codepoint = (packed & 0x1f_ffff) as i32;
            let fg = ((packed >> 21) & 0xffff_ffff) as i32;
            let style = ((packed >> 53) & 0x3f) as i32;
            set_cell(x, y, codepoint, fg, TRANSPARENT, style);
        }
        #[cfg(feature = "top-down")]
        {
            (visible_rows >= ROW_COUNT) as i32
        }
        #[cfg(not(feature = "top-down"))]
        {
            (visible >= CELL_COUNT) as i32
        }
    }
}

#[no_mangle]
pub extern "C" fn resize(width: i32, height: i32) {
    let _ = init(width, height);
}
