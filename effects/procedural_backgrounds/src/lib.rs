//! A family of full-viewport ANSI backgrounds compiled as separate WASM modules.
//!
//! Build all variants with `make backgrounds` from the repository root.
#![no_std]

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! { loop {} }

extern "C" {
    fn set_cell(x: i32, y: i32, codepoint: i32, fg: i32, bg: i32, style: i32);
    fn clear();
    fn random() -> i32;
}

const BLACK: i32 = 0x0200_0000_u32 as i32;
const DIM: i32 = 1 << 4;
const BOLD: i32 = 1;

const fn rgb(r: u32, g: u32, b: u32) -> i32 {
    (0x0100_0000 | (r << 16) | (g << 8) | b) as i32
}

static mut WIDTH: i32 = 0;
static mut HEIGHT: i32 = 0;
static mut SEEDS: [u32; 512] = [0; 512];

#[no_mangle]
pub extern "C" fn init(w: i32, h: i32) -> i32 {
    unsafe {
        WIDTH = w.min(256).max(1);
        HEIGHT = h.min(256).max(1);
        let mut i = 0;
        while i < 512 {
            SEEDS[i] = random() as u32;
            i += 1;
        }
    }
    0
}

#[no_mangle]
pub extern "C" fn tick(frame: i32) -> i32 {
    // Dense fields retain their previous cells and refresh one interlaced
    // column band per tick. A full redraw exceeds the host's intentional
    // 100k-instruction fuel budget at 80x24; three bands stay comfortably
    // within it while every cell still updates several times per second.
    #[cfg(not(any(
        feature = "plasma",
        feature = "lava",
        feature = "aurora",
        feature = "vortex",
        feature = "caustics",
        feature = "kaleidoscope",
        feature = "starfield",
        feature = "orbitals",
    )))]
    unsafe { clear(); }

    #[cfg(feature = "plasma")]
    draw_plasma(frame);
    #[cfg(feature = "lava")]
    draw_dense_field(frame, 1);
    #[cfg(feature = "aurora")]
    draw_dense_field(frame, 2);
    #[cfg(feature = "vortex")]
    draw_dense_field(frame, 3);
    #[cfg(feature = "caustics")]
    draw_dense_field(frame, 4);
    #[cfg(feature = "starfield")]
    draw_dense_field(frame, 6);
    #[cfg(feature = "orbitals")]
    draw_dense_field(frame, 7);
    #[cfg(feature = "kaleidoscope")]
    draw_dense_field(frame, 5);
    0
}

#[no_mangle]
pub extern "C" fn resize(w: i32, h: i32) { init(w, h); }

#[inline]
fn put(x: i32, y: i32, ch: u8, fg: i32, style: i32) {
    unsafe { set_cell(x, y, ch as i32, fg, BLACK, style); }
}

#[cfg(feature = "plasma")]
fn draw_plasma(frame: i32) {
    draw_dense_field(frame, 0);
}

#[cfg(any(
    feature = "plasma",
    feature = "lava",
    feature = "aurora",
    feature = "vortex",
    feature = "caustics",
    feature = "kaleidoscope",
    feature = "starfield",
    feature = "orbitals",
))]
fn draw_dense_field(frame: i32, mode: u8) {
    let (w, h) = unsafe { (WIDTH, HEIGHT) };
    let cx = w / 2;
    let cy = h / 2;
    let (ramp, palette): (&[u8], [i32; 8]) = match mode {
        1 => (b"  ..::ooOO0Q#@", [
            rgb(30, 5, 18), rgb(75, 8, 25), rgb(135, 15, 25), rgb(205, 35, 20),
            rgb(255, 80, 18), rgb(255, 145, 25), rgb(255, 210, 65), rgb(255, 245, 180),
        ]),
        2 => (b"  .-~=+*#%@", [
            rgb(5, 22, 38), rgb(8, 55, 72), rgb(12, 105, 105), rgb(18, 175, 125),
            rgb(45, 235, 150), rgb(75, 220, 220), rgb(115, 145, 255), rgb(185, 95, 255),
        ]),
        3 => (b" .':;!+xX#@", [
            rgb(12, 8, 35), rgb(35, 15, 90), rgb(75, 25, 170), rgb(140, 35, 220),
            rgb(225, 45, 190), rgb(255, 70, 105), rgb(255, 135, 55), rgb(255, 225, 120),
        ]),
        4 => (b"  ..--==**##@@", [
            rgb(3, 18, 45), rgb(5, 38, 85), rgb(8, 70, 135), rgb(12, 115, 175),
            rgb(30, 170, 205), rgb(85, 215, 225), rgb(170, 245, 230), rgb(245, 255, 225),
        ]),
        5 => (b" .:+*xX0#%@", [
            rgb(22, 8, 55), rgb(65, 20, 125), rgb(125, 35, 205), rgb(215, 45, 205),
            rgb(255, 70, 125), rgb(255, 145, 55), rgb(245, 225, 70), rgb(85, 245, 205),
        ]),
        6 => (b"   ..:+*xX#@", [
            rgb(3, 8, 28), rgb(8, 22, 65), rgb(15, 48, 115), rgb(25, 85, 175),
            rgb(50, 135, 225), rgb(105, 190, 255), rgb(190, 230, 255), rgb(255, 255, 255),
        ]),
        7 => (b"  .:oO0Q#@", [
            rgb(22, 8, 32), rgb(65, 18, 72), rgb(125, 35, 105), rgb(200, 65, 105),
            rgb(255, 115, 75), rgb(255, 180, 65), rgb(255, 230, 125), rgb(255, 252, 220),
        ]),
        _ => (b" .,:;irsXA253hMHGS#9B&@", [
            rgb(18, 20, 58), rgb(42, 42, 130), rgb(62, 55, 205), rgb(42, 155, 235),
            rgb(25, 225, 205), rgb(245, 215, 55), rgb(255, 105, 70), rgb(240, 45, 155),
        ]),
    };

    // The classic plasma recipe: add several phase-shifted waves, including
    // a radial field, then quantize the resulting luminance to ASCII glyphs.
    let column_phase = frame.rem_euclid(3);
    let lava_ax = cx + tri(frame, (w / 4).max(1));
    let lava_bx = cx + tri(frame + 53, (w / 3).max(1));
    let lava_ay = cy + tri(frame + 20, h / 3);
    let lava_by = cy + tri(frame + 75, h / 3);
    let orbit_ax = cx + tri(frame, (w / 3).max(1));
    let orbit_ay = cy + tri(frame + 64, (h / 3).max(1));
    let orbit_bx = cx + tri(frame + 43, (w / 4).max(1));
    let orbit_by = cy + tri(frame + 101, (h / 3).max(1));
    for y in 0..h {
        let mut x = column_phase;
        while x < w {
            let dx = x - cx;
            let dy = y - cy;
            let radial = approx_dist(dx * 2, dy * 4);
            let value = match mode {
                // Warm, slow metaball-like lobes rising through the field.
                1 => {
                    let d1 = approx_dist((x - lava_ax) * 2, (y - lava_ay) * 4);
                    let d2 = approx_dist((x - lava_bx) * 2, (y - lava_by) * 4);
                    plasma_wave(d1 * 7 - frame * 3) + plasma_wave(d2 * 6 + frame * 2)
                        + plasma_wave(y * 8 - frame * 2) + plasma_wave((x + y) * 3 + frame)
                }
                // Vertical curtains driven by waves travelling across the sky.
                2 => {
                    let curtain = plasma_wave(x * 4 + frame * 2) + plasma_wave(x * 7 - frame);
                    let distance = (dy * 18 - curtain).abs();
                    (420 - distance * 3).max(-384)
                        + plasma_wave(y * 9 + frame * 3) / 2
                        + plasma_wave((x - y) * 3 - frame)
                }
                // Radial rings crossed with a sheared angular field.
                3 => {
                    let shear = dx * dy * 12 / (radial + 4);
                    plasma_wave(radial * 9 - frame * 5) + plasma_wave(shear + frame * 4)
                        + plasma_wave((dx + dy) * 5 - frame) + plasma_wave((dx - dy) * 4 + frame * 2)
                }
                // Fast overlapping water waves; bright peaks read as caustic lines.
                4 => plasma_wave(x * 9 + frame * 4) + plasma_wave(y * 15 - frame * 3)
                    + plasma_wave((x + y) * 7 + frame * 2) + plasma_wave(radial * 6 - frame * 5),
                // Mirror both axes, then combine diamond, radial, and sheared
                // fields to create a dense eight-way animated mandala.
                5 => {
                    let ax = dx.abs();
                    let ay = (dy * 2).abs();
                    let diagonal = (ax - ay).abs();
                    let shear = ax * ay * 8 / (radial + 3);
                    plasma_wave((ax + ay) * 8 - frame * 4)
                        + plasma_wave(diagonal * 11 + frame * 3)
                        + plasma_wave(shear - frame * 2)
                        + plasma_wave(radial * 6 + frame)
                }
                // A dense hyperspace tunnel: accelerating radial bands cross
                // angular rays, producing streaks that stream from the center.
                6 => {
                    let ray = dx * dy * 18 / (radial + 3);
                    plasma_wave(radial * 13 - frame * 9)
                        + plasma_wave(ray * 3 + frame * 2)
                        + plasma_wave((dx + dy) * 9)
                        + plasma_wave((dx - dy) * 7)
                }
                // Two moving gravity wells generate nested orbital contours.
                7 => {
                    let d1 = approx_dist((x - orbit_ax) * 2, (y - orbit_ay) * 4);
                    let d2 = approx_dist((x - orbit_bx) * 2, (y - orbit_by) * 4);
                    plasma_wave(d1 * 11 - frame * 4)
                        + plasma_wave(d2 * 9 + frame * 3)
                        + plasma_wave((d1 + d2) * 5 - frame)
                        + plasma_wave((d1 - d2) * 7 + frame * 2)
                }
                _ => plasma_wave(x * 5 + frame * 3) + plasma_wave(y * 11 - frame * 2)
                    + plasma_wave((x + y * 2) * 4 + frame) + plasma_wave(radial * 5 - frame * 4),
            };
            let level = ((value + 512) * (ramp.len() as i32 - 1) / 1024)
                .max(0)
                .min(ramp.len() as i32 - 1) as usize;

            let color_phase = ((value + frame * (mode as i32 + 2) + x * 2 - y * 3) / 64).rem_euclid(8) as usize;
            let style = if level > ramp.len() * 3 / 4 { BOLD } else if level < ramp.len() / 4 { DIM } else { 0 };
            put(x, y, ramp[level], palette[color_phase], style);
            x += 3;
        }
    }
}

#[cfg(any(
    feature = "plasma",
    feature = "lava",
    feature = "aurora",
    feature = "vortex",
    feature = "caustics",
    feature = "kaleidoscope",
    feature = "starfield",
    feature = "orbitals",
))]
#[inline]
fn plasma_wave(phase: i32) -> i32 {
    // Smooth integer sine approximation in the range -128..128. Avoiding
    // floats keeps the no_std guest tiny and deterministic.
    let p = phase.rem_euclid(256);
    let triangle = if p < 64 {
        p * 2
    } else if p < 192 {
        256 - p * 2
    } else {
        p * 2 - 512
    };
    let sign = triangle.signum();
    let a = triangle.abs();
    sign * (a * (256 - a) / 128)
}

#[inline]
fn approx_dist(x: i32, y: i32) -> i32 {
    let ax = x.abs();
    let ay = y.abs();
    ax.max(ay) + ax.min(ay) / 2
}

#[inline]
fn tri(phase: i32, amplitude: i32) -> i32 {
    let p = phase.rem_euclid(128);
    let unit = if p < 32 { p } else if p < 96 { 64 - p } else { p - 128 };
    unit * amplitude / 32
}
