//! Content-aware particle effects for terminal glyphs.
//!
//! Build with either `materialise` or `atomise`. Both variants read the
//! animation's rendered child content via `get_content_cell` and preserve its
//! glyph, foreground colour, and style when drawing the transformed frame.
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

#[cfg(not(feature = "prompt"))]
const FRAMES: i32 = 48;
const TRANSPARENT: i32 = 0;
// Literal black (0x01RRGGBB), not the terminal's configurable ANSI black, so
// light and tinted palettes cannot show through the stage.
#[cfg(feature = "atomise")]
const BLACK: i32 = 0x0100_0000_u32 as i32;
#[cfg(any(feature = "materialise", feature = "lifecycle"))]
const DIM: i32 = 1 << 4;
#[cfg(any(feature = "materialise", feature = "lifecycle"))]
const BOLD: i32 = 1;
#[cfg(any(feature = "materialise", feature = "lifecycle"))]
const MATRIX_COLOURS: [i32; 5] = [
    0x0100_3B00, // deep phosphor green
    0x0100_7214, // dark Matrix green
    0x0100_AA22, // mid green
    0x0100_E83A, // neon green
    0x01B8_FFC2, // pale mint highlight
];
static mut WIDTH: i32 = 0;
static mut HEIGHT: i32 = 0;
#[cfg(not(feature = "prompt"))]
static mut RUN_COUNT: i32 = 0;
#[cfg(feature = "prompt")]
static mut PROMPT_START: i32 = 0;
#[cfg(feature = "prompt")]
static mut PROMPT_END: i32 = 0;
#[cfg(feature = "prompt")]
static mut PROMPT_FRAMES: i32 = 0;

#[no_mangle]
pub extern "C" fn init(w: i32, h: i32) -> i32 {
    unsafe {
        WIDTH = w.max(1).min(256);
        HEIGHT = h.max(1).min(256);
        #[cfg(feature = "lifecycle")]
        {
            RUN_COUNT = 0;
        }

        #[cfg(feature = "prompt")]
        {
            PROMPT_START = WIDTH;
            PROMPT_END = 0;
            let mut x = 0;
            while x < WIDTH {
                if get_content_cell(x, 0) != 0 {
                    if x < PROMPT_START {
                        PROMPT_START = x;
                    }
                    PROMPT_END = x + 1;
                }
                x += 1;
            }
            let mut total = 3;
            x = PROMPT_START;
            while x < PROMPT_END {
                total += 1 + (hash(x, 0) % 3 == 0) as i32;
                x += 1;
            }
            PROMPT_FRAMES = total + 5;
            return PROMPT_FRAMES;
        }
    }
    #[cfg(not(feature = "prompt"))]
    {
        FRAMES
    }
}

#[inline]
fn hash(x: i32, y: i32) -> u32 {
    let mut v =
        (x as u32).wrapping_mul(0x9E37_79B9) ^ (y as u32).wrapping_mul(0x85EB_CA6B) ^ 0xD057_3A11;
    v ^= v >> 16;
    v = v.wrapping_mul(0x7FEB_352D);
    v ^= v >> 15;
    v
}

#[inline]
unsafe fn draw_packed(x: i32, y: i32, packed: i64, codepoint: Option<i32>) {
    if packed == 0 {
        return;
    }
    let bits = packed as u64;
    let cp = codepoint.unwrap_or((bits & 0x1F_FFFF) as i32);
    let fg = ((bits >> 21) & 0xFFFF_FFFF) as i32;
    let style = ((bits >> 53) & 0x3F) as i32;
    set_cell(x, y, cp, fg, TRANSPARENT, style);
}

#[cfg(feature = "atomise")]
#[inline]
fn particle_glyph(seed: u32) -> i32 {
    const GLYPHS: &[u8] = b".+*x";
    GLYPHS[(seed as usize) % GLYPHS.len()] as i32
}

#[cfg(any(feature = "materialise", feature = "lifecycle"))]
#[inline]
fn matrix_glyph(seed: u32) -> i32 {
    const GLYPHS: &[u8] = b"01:+*#";
    GLYPHS[(seed as usize) % GLYPHS.len()] as i32
}

#[cfg(any(feature = "materialise", feature = "lifecycle"))]
#[inline]
unsafe fn draw_matrix_cell(x: i32, y: i32, seed: u32, style: i32) {
    if x < 0 || x >= WIDTH || y < 0 || y >= HEIGHT {
        return;
    }
    let colour = MATRIX_COLOURS[((seed >> 5) as usize) % MATRIX_COLOURS.len()];
    set_cell(x, y, matrix_glyph(seed), colour, TRANSPARENT, style);
}

#[no_mangle]
pub extern "C" fn tick(frame: i32) -> i32 {
    unsafe {
        clear();

        #[cfg(feature = "prompt")]
        {
            let mut budget = (frame - 3).max(0);
            let mut cursor_x = PROMPT_START;
            let mut x = PROMPT_START;
            while x < PROMPT_END {
                let packed = get_content_cell(x, 0);
                let cost = 1 + (hash(x, 0) % 3 == 0) as i32;
                if budget < cost {
                    cursor_x = x;
                    break;
                }
                draw_packed(x, 0, packed, None);
                budget -= cost;
                cursor_x = x + 1;
                x += 1;
            }
            if frame < PROMPT_FRAMES - 2 && frame % 4 != 1 && cursor_x < WIDTH {
                let style_source = if cursor_x < PROMPT_END {
                    get_content_cell(cursor_x, 0)
                } else {
                    get_content_cell((PROMPT_END - 1).max(0), 0)
                };
                draw_packed(cursor_x, 0, style_source, Some(b'_' as i32));
            }
            return if frame >= PROMPT_FRAMES - 1 { 1 } else { 0 };
        }

        #[cfg(not(feature = "prompt"))]
        {
            let progress = frame.clamp(0, FRAMES - 1);

            #[cfg(feature = "lifecycle")]
            if frame == 0 {
                RUN_COUNT += 1;
            }

            #[cfg(any(feature = "atomise", feature = "lifecycle"))]
            if cfg!(feature = "atomise") || RUN_COUNT > 1 {
                // Completion is transparent, not an opaque black hold. The
                // compositor can then reveal the authored background for one
                // full frame before deferred navigation resumes.
                if frame >= FRAMES - 1 {
                    return 1;
                }

                // Standalone atomise effects retain their black stage. The
                // lifecycle variant used by the DUSTNET title stays transparent
                // so the live Matrix background remains visible through every
                // cell vacated by the departing logo.
                #[cfg(feature = "atomise")]
                {
                    for y in 0..HEIGHT {
                        for x in 0..WIDTH {
                            set_cell(x, y, b' ' as i32, 0, BLACK, 0);
                        }
                    }
                }
            }

            for y in 0..HEIGHT {
                for x in 0..WIDTH {
                    let packed = get_content_cell(x, y);
                    if packed == 0 {
                        continue;
                    }
                    let seed = hash(x, y);

                    #[cfg(any(feature = "materialise", feature = "lifecycle"))]
                    {
                        if !cfg!(feature = "lifecycle") || RUN_COUNT <= 1 {
                            // Resolve the authored glyphs from the bottom row upward.
                            // Each cell is preceded by a short, vertically falling
                            // stream of coloured code so the logo feels printed by
                            // digital rain instead of randomly dissolved into view.
                            let row_from_bottom = HEIGHT - 1 - y;
                            let settle_frame = 9 + row_from_bottom * 4 + (seed % 7) as i32;
                            if progress >= settle_frame {
                                draw_packed(x, y, packed, None);
                            } else {
                                let distance = settle_frame - progress;
                                let head_y = y - distance;

                                // Bright head and two dimmer characters above it make
                                // a recognisable Matrix-style vertical stream.
                                draw_matrix_cell(
                                    x,
                                    head_y,
                                    seed.wrapping_add(progress as u32),
                                    BOLD,
                                );
                                draw_matrix_cell(
                                    x,
                                    head_y - 1,
                                    seed.wrapping_add(progress as u32 + 17),
                                    DIM,
                                );
                                draw_matrix_cell(
                                    x,
                                    head_y - 2,
                                    seed.wrapping_add(progress as u32 + 31),
                                    DIM,
                                );

                                // Flicker briefly in the destination cell before the
                                // real logo glyph locks into place.
                                if distance <= 2 {
                                    draw_matrix_cell(
                                        x,
                                        y,
                                        seed.wrapping_add(progress as u32 + 47),
                                        if distance == 1 { BOLD } else { DIM },
                                    );
                                }
                            }
                        }
                    }

                    #[cfg(feature = "lifecycle")]
                    {
                        if RUN_COUNT > 1 {
                            // Reverse the entrance: dismantle the logo from its
                            // top row downward, turning each authored cell back
                            // into a rising trail of green Matrix code.
                            let dissolve_frame = 7 + y * 3 + (seed % 6) as i32;
                            if progress < dissolve_frame {
                                draw_packed(x, y, packed, None);
                            } else {
                                let age = progress - dissolve_frame;
                                let head_y = y - 1 - age / 2;
                                draw_matrix_cell(x, head_y, seed.wrapping_add(age as u32), BOLD);
                                draw_matrix_cell(
                                    x,
                                    head_y + 1,
                                    seed.wrapping_add(age as u32 + 17),
                                    DIM,
                                );
                                draw_matrix_cell(
                                    x,
                                    head_y + 2,
                                    seed.wrapping_add(age as u32 + 31),
                                    DIM,
                                );
                            }
                        }
                    }

                    #[cfg(feature = "atomise")]
                    {
                        // Standalone atomise retains its free-form particle
                        // breakup rather than the title's mirrored Matrix exit.
                        let atomise_span = (FRAMES - HEIGHT * 2 - 4).max(1) as u32;
                        let atomise_threshold = 3 + (seed % atomise_span) as i32;
                        if progress < atomise_threshold {
                            draw_packed(x, y, packed, None);
                        } else {
                            let age = progress - atomise_threshold;
                            let drift = match seed % 3 {
                                0 => -1,
                                1 => 0,
                                _ => 1,
                            };
                            let px = x + drift * (age / 5);
                            let py = y - 1 - age / 2;
                            if px >= 0 && px < WIDTH && py >= 0 {
                                draw_packed(
                                    px,
                                    py,
                                    packed,
                                    Some(particle_glyph(seed + age as u32)),
                                );
                            }
                        }
                    }
                }
            }
            if frame >= FRAMES - 1 {
                1
            } else {
                0
            }
        }
    }
}

#[no_mangle]
pub extern "C" fn resize(w: i32, h: i32) {
    let _ = init(w, h);
}
