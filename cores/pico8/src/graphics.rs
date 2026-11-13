//! Drawing into PICO-8's memory. As on PICO-8, the whole drawing state is
//! in RAM (0x5f00 on): the palettes, clip, pen, cursor, camera, fill
//! pattern, the last point of `line`, and where the sprites, the screen
//! and the map are, so a cartridge that pokes them gets what it expects,
//! and a save state is the RAM.
//!
//! Coordinates come in as whole pixels (the numbers floored).

pub const WIDTH: usize = 128;
pub const HEIGHT: usize = 128;

/// RGB: the sixteen colours, then the sixteen secret ones (128..143 in the
/// screen palette).
pub const PALETTE: [u32; 32] = [
    0x000000, 0x1d2b53, 0x7e2553, 0x008751, 0xab5236, 0x5f574f, 0xc2c3c7, 0xfff1e8, 0xff004d, 0xffa300, 0xffec27,
    0x00e436, 0x29adff, 0x83769c, 0xff77a8, 0xffccaa, 0x291814, 0x111d35, 0x422136, 0x125359, 0x742f29, 0x49333b,
    0xa28879, 0xf3ef7d, 0xbe1250, 0xff6c24, 0xa8e72e, 0x00b543, 0x065ab5, 0x754665, 0xff6e59, 0xff9d81,
];

/// PICO-8's font (CC0, Lexaloffle), in the custom-font layout of 0x5600:
/// a header (widths, height, offsets), then 8 bytes a character, a byte a
/// row, the lowest bit the leftmost pixel.
pub static FONT: &[u8; 2048] = include_bytes!("font.bin");

pub type Ram = [u8; 0x10000];

const PAL: usize = 0x5f00;
const SCREEN_PAL: usize = 0x5f10;
const CLIP: usize = 0x5f20;
const PEN: usize = 0x5f25;
const CURSOR: usize = 0x5f26;
const CAMERA: usize = 0x5f28;
const FILL: usize = 0x5f31;
const LINE_INVALID: usize = 0x5f35;
const LINE_LAST: usize = 0x5f3c;

/// The drawing state as PICO-8 starts a cartridge, and as reset() leaves it.
pub fn reset(ram: &mut Ram) {
    reset_palettes(ram);
    clip_reset(ram);
    ram[0x5f24] = 0;
    ram[PEN] = 6;
    ram[CURSOR] = 0;
    ram[CURSOR + 1] = 0;
    ram[CAMERA..CAMERA + 4].fill(0);
    ram[0x5f2c] = 0;
    ram[FILL..FILL + 4].fill(0);
    ram[LINE_INVALID] = 1;
    ram[0x5f36] = 0;
    ram[0x5f38..0x5f40].fill(0);
    ram[0x5f54] = 0x00;
    ram[0x5f55] = 0x60;
    ram[0x5f56] = 0x20;
    ram[0x5f57] = 0x80;
    ram[0x5f58] = 0;
    ram[0x5f5e] = 0xff;
    ram[0x5f5f] = 0;
}

pub fn reset_palettes(ram: &mut Ram) {
    for c in 0..16 {
        ram[PAL + c] = c as u8;
        ram[SCREEN_PAL + c] = c as u8;
        ram[0x5f60 + c] = c as u8;
    }
    ram[PAL] |= 0x10;
}

fn clip_reset(ram: &mut Ram) {
    ram[CLIP..CLIP + 4].copy_from_slice(&[0, 0, 128, 128]);
}

fn i16_at(ram: &Ram, a: usize) -> i32 {
    i32::from(i16::from_le_bytes([ram[a], ram[a + 1]]))
}
fn set_i16(ram: &mut Ram, a: usize, v: i32) {
    ram[a..a + 2].copy_from_slice(&(v as i16).to_le_bytes());
}

pub fn camera(ram: &Ram) -> (i32, i32) {
    (i16_at(ram, CAMERA), i16_at(ram, CAMERA + 2))
}

fn screen_base(ram: &Ram) -> usize {
    usize::from(ram[0x5f55]) << 8
}
fn sprite_base(ram: &Ram) -> usize {
    usize::from(ram[0x5f54]) << 8
}

/// A colour argument: the pen when there is none, and it becomes the pen.
pub fn pen(ram: &mut Ram, c: Option<i32>) -> u8 {
    match c {
        Some(c) => {
            ram[PEN] = c as u8;
            c as u8
        }
        None => ram[PEN],
    }
}

/// A colour argument as a 16.16 number (its bits): the pen, and with
/// 0x5f34 bit 0 set, a number with bit 0x1000.0000 carries a fill pattern
/// in its fraction and its transparency in bit 0x0100.0000.
pub fn pen_bits(ram: &mut Ram, c: Option<i32>) -> u8 {
    if let Some(n) = c
        && n & 0x1000_0000 != 0
        && ram[0x5f34] & 1 != 0
    {
        ram[FILL] = n as u8;
        ram[FILL + 1] = (n >> 8) as u8;
        ram[FILL + 2] = (ram[FILL + 2] & !1) | ((n >> 24) & 1) as u8;
    }
    pen(ram, c.map(|n| n >> 16))
}

/// One pixel of the screen at screen coordinates, clipped.
#[inline]
fn put(ram: &mut Ram, x: i32, y: i32, c: u8) {
    if x < i32::from(ram[CLIP]) || y < i32::from(ram[CLIP + 1]) || x >= i32::from(ram[CLIP + 2]) || y >= i32::from(ram[CLIP + 3]) {
        return;
    }
    let a = (screen_base(ram) + y as usize * 64 + x as usize / 2) & 0xffff;
    let mask = ram[0x5f5e];
    let old = ram[a];
    let (shift, keep) = if x & 1 == 0 { (0, 0xf0) } else { (4, 0x0f) };
    let mut v = c & 15;
    if mask != 0xff {
        // The bitplane masks: the high nibble which bits to read, the low
        // which to write.
        let old_c = (old >> shift) & 15;
        let read = (mask >> 4) & 15;
        let write = mask & 15;
        v = (old_c & !write) | (v & read & write);
    }
    ram[a] = (old & keep) | (v << shift);
}

/// A shape's pixel at screen coordinates: the colour (with a second one
/// in its high nibble) through the draw palette and the fill pattern.
#[inline]
fn shade(ram: &mut Ram, x: i32, y: i32, col: u8) {
    let pattern = u16::from_le_bytes([ram[FILL], ram[FILL + 1]]);
    let c = if pattern == 0 {
        col & 15
    } else {
        let bit = (pattern >> (15 - ((y & 3) * 4 + (x & 3)))) & 1;
        if bit == 0 {
            col & 15
        } else if ram[FILL + 2] & 1 != 0 {
            return;
        } else {
            col >> 4
        }
    };
    let mapped = ram[PAL + usize::from(c)] & 15;
    put(ram, x, y, mapped);
}

fn hline(ram: &mut Ram, x0: i32, x1: i32, y: i32, col: u8) {
    let (x0, x1) = (x0.min(x1).max(i32::from(ram[CLIP])), x0.max(x1).min(i32::from(ram[CLIP + 2]) - 1));
    if y < i32::from(ram[CLIP + 1]) || y >= i32::from(ram[CLIP + 3]) || x0 > x1 {
        return;
    }
    let base = screen_base(ram) + y as usize * 64;
    let plain = u16::from_le_bytes([ram[FILL], ram[FILL + 1]]) == 0 && ram[0x5f5e] == 0xff;
    if plain && base + 64 <= 0x10000 {
        // One colour, no pattern or bitplanes: whole bytes, two pixels
        // each, and a nibble at an odd start or an even end.
        let c = ram[PAL + usize::from(col & 15)] & 15;
        let (mut x, end) = (x0 as usize, x1 as usize + 1);
        if x & 1 == 1 {
            ram[base + x / 2] = (ram[base + x / 2] & 0x0f) | c << 4;
            x += 1;
        }
        let full = end & !1;
        if x < full {
            ram[base + x / 2..base + full / 2].fill(c | c << 4);
        }
        if end & 1 == 1 && end > x {
            ram[base + end / 2] = (ram[base + end / 2] & 0xf0) | c;
        }
        return;
    }
    for x in x0..=x1 {
        shade(ram, x, y, col);
    }
}

pub fn pset(ram: &mut Ram, x: i32, y: i32, col: u8) {
    let (cx, cy) = camera(ram);
    shade(ram, x - cx, y - cy, col);
}

/// The screen's colour at x, y (through the camera and clip), or 0.
pub fn pget(ram: &Ram, x: i32, y: i32) -> u8 {
    let (cx, cy) = camera(ram);
    let (x, y) = (x - cx, y - cy);
    if x < i32::from(ram[CLIP]) || y < i32::from(ram[CLIP + 1]) || x >= i32::from(ram[CLIP + 2]) || y >= i32::from(ram[CLIP + 3]) {
        return 0;
    }
    let b = ram[(screen_base(ram) + y as usize * 64 + x as usize / 2) & 0xffff];
    if x & 1 == 0 { b & 15 } else { b >> 4 }
}

pub fn sget(ram: &Ram, x: i32, y: i32) -> u8 {
    if !(0..128).contains(&x) || !(0..128).contains(&y) {
        return 0;
    }
    let b = ram[(sprite_base(ram) + y as usize * 64 + x as usize / 2) & 0xffff];
    if x & 1 == 0 { b & 15 } else { b >> 4 }
}

pub fn sset(ram: &mut Ram, x: i32, y: i32, c: u8) {
    if !(0..128).contains(&x) || !(0..128).contains(&y) {
        return;
    }
    let a = (sprite_base(ram) + y as usize * 64 + x as usize / 2) & 0xffff;
    let c = ram[PAL + usize::from(c & 15)] & 15;
    ram[a] = if x & 1 == 0 { (ram[a] & 0xf0) | c } else { (ram[a] & 0x0f) | c << 4 };
}

/// The map cell's address, if the cell is in the map.
fn map_addr(ram: &Ram, x: i32, y: i32) -> Option<usize> {
    let base = usize::from(ram[0x5f56]) << 8;
    let width = match ram[0x5f57] {
        0 => 256,
        w => i32::from(w),
    };
    if x < 0 || y < 0 || x >= width {
        return None;
    }
    let a = base + (y * width + x) as usize;
    if base == 0x2000 {
        // The lower half of the map is the second half of the sprites.
        if y >= 64 {
            return None;
        }
        return Some(if a >= 0x3000 { a - 0x2000 } else { a });
    }
    (a < 0x10000).then_some(a)
}

pub fn mget(ram: &Ram, x: i32, y: i32) -> u8 {
    map_addr(ram, x, y).map_or(0, |a| ram[a])
}
pub fn mset(ram: &mut Ram, x: i32, y: i32, v: u8) {
    if let Some(a) = map_addr(ram, x, y) {
        ram[a] = v;
    }
}

pub fn cls(ram: &mut Ram, c: u8) {
    let base = screen_base(ram);
    let c = c & 15;
    for b in &mut ram[base..(base + 0x2000).min(0x10000)] {
        *b = c | c << 4;
    }
    ram[CURSOR] = 0;
    ram[CURSOR + 1] = 0;
    clip_reset(ram);
}

pub fn line(ram: &mut Ram, x0: i32, y0: i32, x1: i32, y1: i32, col: u8) {
    set_i16(ram, LINE_LAST, x1);
    set_i16(ram, LINE_LAST + 2, y1);
    ram[LINE_INVALID] = 0;
    let (cx, cy) = camera(ram);
    let (x0, y0, x1, y1) = (x0 - cx, y0 - cy, x1 - cx, y1 - cy);
    // As zepto8 measured PICO-8: along the longer axis, the other one
    // interpolated and rounded; the walk clamped to just past the screen.
    let horiz = (x1 - x0).abs() >= (y1 - y0).abs();
    let (dx, dy) = (if x0 <= x1 { 1 } else { -1 }, if y0 <= y1 { 1 } else { -1 });
    let (mut x, mut y) = (x0.clamp(-1, 128), y0.clamp(-1, 128));
    let (xend, yend) = (x1.clamp(-1, 128), y1.clamp(-1, 128));
    let lerp = |a: i32, b: i32, t: f64| (f64::from(a) + (f64::from(b) - f64::from(a)) * t).round() as i32;
    loop {
        shade(ram, x, y, col);
        if horiz {
            if x == xend {
                break;
            }
            x += dx;
            y = lerp(y0, y1, f64::from(x - x0) / f64::from(x1 - x0));
        } else {
            if y == yend {
                break;
            }
            y += dy;
            x = lerp(x0, x1, f64::from(y - y0) / f64::from(y1 - y0));
        }
    }
}

/// The last point of line(), and whether there is one.
pub fn line_last(ram: &Ram) -> Option<(i32, i32)> {
    (ram[LINE_INVALID] == 0).then(|| (i16_at(ram, LINE_LAST), i16_at(ram, LINE_LAST + 2)))
}
pub fn line_start(ram: &mut Ram, x: i32, y: i32) {
    set_i16(ram, LINE_LAST, x);
    set_i16(ram, LINE_LAST + 2, y);
    ram[LINE_INVALID] = 0;
}
pub fn line_reset(ram: &mut Ram) {
    ram[LINE_INVALID] = 1;
}

pub fn rect(ram: &mut Ram, x0: i32, y0: i32, x1: i32, y1: i32, col: u8, fill: bool) {
    let (cx, cy) = camera(ram);
    let (x0, x1) = (x0.min(x1) - cx, x0.max(x1) - cx);
    let (y0, y1) = (y0.min(y1) - cy, y0.max(y1) - cy);
    if fill {
        for y in y0.max(0)..=y1.min(127) {
            hline(ram, x0, x1, y, col);
        }
    } else {
        hline(ram, x0, x1, y0, col);
        hline(ram, x0, x1, y1, col);
        for y in (y0 + 1).max(-1)..y1.min(129) {
            shade(ram, x0, y, col);
            shade(ram, x1, y, col);
        }
    }
}

/// A rectangle with rounded corners of radius r.
#[allow(clippy::too_many_arguments)]
pub fn rrect(ram: &mut Ram, x: i32, y: i32, w: i32, h: i32, r: i32, col: u8, fill: bool) {
    if w <= 0 || h <= 0 {
        return;
    }
    let r = r.clamp(0, (w.min(h) - 1) / 2);
    let (x1, y1) = (x + w - 1, y + h - 1);
    if r == 0 {
        return rect(ram, x, y, x1, y1, col, fill);
    }
    let (cx, cy) = camera(ram);
    let spans = |ram: &mut Ram, dx: i32, dy: i32| {
        // The corners' arcs, at dx, dy from the inner corners.
        let (l, rr, t, b) = (x + r - dx - cx, x1 - r + dx - cx, y + r - dy - cy, y1 - r + dy - cy);
        if fill {
            hline(ram, l, rr, t, col);
            hline(ram, l, rr, b, col);
        } else {
            for (px, py) in [(l, t), (rr, t), (l, b), (rr, b)] {
                shade(ram, px, py, col);
            }
        }
    };
    let (mut dx, mut dy, mut err) = (r, 0, 0);
    while dx >= dy {
        spans(ram, dx, dy);
        spans(ram, dy, dx);
        dy += 1;
        err += 1 + 2 * dy;
        if 2 * (err - dx) > r + 1 {
            dx -= 1;
            err += 1 - 2 * dx;
        }
    }
    if fill {
        for yy in y + r..=y1 - r {
            hline(ram, x - cx, x1 - cx, yy - cy, col);
        }
    } else {
        hline(ram, x + r - cx, x1 - r - cx, y - cy, col);
        hline(ram, x + r - cx, x1 - r - cx, y1 - cy, col);
        for yy in y + r..=y1 - r {
            shade(ram, x - cx, yy - cy, col);
            shade(ram, x1 - cx, yy - cy, col);
        }
    }
}

pub fn circ(ram: &mut Ram, x: i32, y: i32, r: i32, col: u8, fill: bool) {
    if r < 0 {
        return;
    }
    let (cx, cy) = camera(ram);
    let (x, y) = (x - cx, y - cy);
    if x + r < 0 || x - r >= 128 || y + r < 0 || y - r >= 128 {
        return;
    }
    // The midpoint circle PICO-8 draws (zepto8's measurement).
    let (mut dx, mut dy, mut err) = (r, 0, 0);
    while dx >= dy {
        if fill {
            hline(ram, x - dx, x + dx, y - dy, col);
            hline(ram, x - dx, x + dx, y + dy, col);
            hline(ram, x - dy, x + dy, y - dx, col);
            hline(ram, x - dy, x + dy, y + dx, col);
        } else {
            for (px, py) in [(dx, dy), (dy, dx), (-dy, dx), (-dx, dy), (-dx, -dy), (-dy, -dx), (dy, -dx), (dx, -dy)] {
                shade(ram, x + px, y + py, col);
            }
        }
        dy += 1;
        if err < r - 1 {
            err += 1 + 2 * dy;
        } else {
            dx -= 1;
            err += 1 + 2 * (dy - dx);
        }
    }
}

/// An ellipse in the box x0, y0 to x1, y1, as zepto8 has PICO-8's: the
/// edge stepped along x up to where its slope is 1/2, then along y.
pub fn oval(ram: &mut Ram, x0: i32, y0: i32, x1: i32, y1: i32, col: u8, fill: bool) {
    let (cx, cy) = camera(ram);
    let (x0, x1) = (x0.min(x1) - cx, x0.max(x1) - cx);
    let (y0, y1) = (y0.min(y1) - cy, y0.max(y1) - cy);
    if x1 < 0 || x0 >= 128 || y1 < 0 || y0 >= 128 || x1 - x0 > 4096 || y1 - y0 > 4096 {
        return;
    }
    let (xc, yc) = ((x0 + x1) as f32 / 2.0, (y0 + y1) as f32 / 2.0);
    let (a, b) = (((x1 - x0) as f32 / 2.0).max(1.0), ((y1 - y0) as f32 / 2.0).max(1.0));
    let (mx, my) = ((2.0 * xc) as i32, (2.0 * yc) as i32);
    let cutoff = a / (1.0 + b * b / (a * a)).sqrt();
    let mut d = 0.0f32;
    while d <= cutoff {
        let x = (xc + d).ceil() as i32;
        let y = (yc - b / a * (a * a - d * d).sqrt()).round() as i32;
        if fill {
            vline(ram, x, my - y, y, col);
            vline(ram, mx - x, my - y, y, col);
        } else {
            for (px, py) in [(x, y), (mx - x, y), (x, my - y), (mx - x, my - y)] {
                shade(ram, px, py, col);
            }
        }
        d += 1.0;
    }
    let cutoff = b / (1.0 + a * a / (b * b)).sqrt();
    let mut d = 0.0f32;
    while if fill { d <= cutoff } else { d < cutoff } {
        let y = (yc + d).ceil() as i32;
        let x = (xc - a / b * (b * b - d * d).sqrt()).round() as i32;
        if fill {
            hline(ram, mx - x, x, y, col);
            hline(ram, mx - x, x, my - y, col);
        } else {
            for (px, py) in [(x, y), (mx - x, y), (x, my - y), (mx - x, my - y)] {
                shade(ram, px, py, col);
            }
        }
        d += 1.0;
    }
}

/// A vertical span, in screen coordinates.
fn vline(ram: &mut Ram, x: i32, y0: i32, y1: i32, col: u8) {
    for y in y0.min(y1).max(-1)..=y0.max(y1).min(128) {
        shade(ram, x, y, col);
    }
}

/// Whether the draw palette makes colour c transparent.
#[inline]
fn transparent(ram: &Ram, c: u8) -> bool {
    ram[PAL + usize::from(c)] & 0x10 != 0
}

/// Part of the sprite sheet onto the screen, scaled, maybe flipped.
#[allow(clippy::too_many_arguments)]
pub fn sspr(ram: &mut Ram, sx: i32, sy: i32, sw: i32, sh: i32, dx: i32, dy: i32, dw: i32, dh: i32, fx: bool, fy: bool) {
    // A negative size flips, about the first pixel (zepto8's PICO-8).
    let (fx, dx, dw) = if dw < 0 { (!fx, dx + dw + 1, -dw) } else { (fx, dx, dw) };
    let (fy, dy, dh) = if dh < 0 { (!fy, dy + dh + 1, -dh) } else { (fy, dy, dh) };
    if sw <= 0 || sh <= 0 || dw <= 0 || dh <= 0 || dw > 4096 || dh > 4096 {
        return;
    }
    let (cx, cy) = camera(ram);
    let (dx, dy) = (dx - cx, dy - cy);
    let (x0, y0) = (i32::from(ram[CLIP]).max(dx), i32::from(ram[CLIP + 1]).max(dy));
    let (x1, y1) = (i32::from(ram[CLIP + 2]).min(dx + dw), i32::from(ram[CLIP + 3]).min(dy + dh));
    for y in y0..y1 {
        let v = y - dy;
        let v = if fy { dh - 1 - v } else { v };
        let py = sy + v * sh / dh;
        for x in x0..x1 {
            let u = x - dx;
            let u = if fx { dw - 1 - u } else { u };
            let c = sget(ram, sx + u * sw / dw, py);
            if !transparent(ram, c) {
                let mapped = ram[PAL + usize::from(c)] & 15;
                put(ram, x, y, mapped);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn spr(ram: &mut Ram, n: i32, x: i32, y: i32, w: f64, h: f64, fx: bool, fy: bool) {
    let n = n & 0xff;
    let (pw, ph) = ((w * 8.0) as i32, (h * 8.0) as i32);
    sspr(ram, (n % 16) * 8, (n / 16) * 8, pw, ph, x, y, pw, ph, fx, fy);
}

#[allow(clippy::too_many_arguments)]
pub fn map(ram: &mut Ram, mx: i32, my: i32, sx: i32, sy: i32, w: i32, h: i32, layers: u8) {
    let show_zero = ram[0x5f36] & 8 != 0;
    for j in 0..h.min(256) {
        for i in 0..w.min(256) {
            let s = mget(ram, mx + i, my + j);
            if s == 0 && !show_zero {
                continue;
            }
            // Layers: a sprite with any of their flags.
            if layers != 0 && ram[0x3000 + usize::from(s)] & layers == 0 {
                continue;
            }
            spr(ram, i32::from(s), sx + i * 8, sy + j * 8, 1.0, 1.0, false, false);
        }
    }
}

/// A line textured from the map: mx, my (in cells, 16.16) step by mdx,
/// mdy each pixel; as zepto8 has PICO-8's: the cell position wrapped by the
/// masks at 0x5f38 and 0x5f39 (128 when 0), moved by the offsets at 0x5f3a
/// and 0x5f3b and wrapped around the map, the pixels placed as line() does.
#[allow(clippy::too_many_arguments)]
pub fn tline(ram: &mut Ram, x0: i32, y0: i32, x1: i32, y1: i32, mx: i32, my: i32, mdx: i32, mdy: i32, layers: u8) {
    let (cx, cy) = camera(ram);
    let (x0, y0, x1, y1) = (x0 - cx, y0 - cy, x1 - cx, y1 - cy);
    let loop_of = |m: u8| if m == 0 { 128i64 << 16 } else { i64::from(m) << 16 };
    let (loopx, loopy) = (loop_of(ram[0x5f38]), loop_of(ram[0x5f39]));
    let (off_x, off_y) = (i64::from(ram[0x5f3a]), i64::from(ram[0x5f3b]));
    let width = if ram[0x5f57] == 0 { 256 } else { i64::from(ram[0x5f57]) };
    let rows = (if ram[0x5f56] >= 0x80 { (0x100 - i64::from(ram[0x5f56])) << 8 } else { 8192 }) / width;
    let (mut mx, mut my) = (i64::from(mx).rem_euclid(loopx), i64::from(my).rem_euclid(loopy));
    let horiz = (x1 - x0).abs() >= (y1 - y0).abs();
    let (dx, dy) = (if x0 <= x1 { 1 } else { -1 }, if y0 <= y1 { 1 } else { -1 });
    let (mut x, mut y) = (x0.clamp(-1, 128), y0.clamp(-1, 128));
    let (xend, yend) = (x1.clamp(-1, 128), y1.clamp(-1, 128));
    // The texture moved on for the pixels the clamp skipped.
    let skipped = i64::from(if horiz { (x - x0).abs() } else { (y - y0).abs() });
    mx = (mx + i64::from(mdx) * skipped).rem_euclid(loopx);
    my = (my + i64::from(mdy) * skipped).rem_euclid(loopy);
    let show_zero = ram[0x5f36] & 8 != 0;
    let lerp = |a: i32, b: i32, t: f64| (f64::from(a) + (f64::from(b) - f64::from(a)) * t).round() as i32;
    loop {
        let sx = (off_x + (mx >> 16)).rem_euclid(width);
        let sy = (off_y + (my >> 16)).rem_euclid(rows.max(1));
        let s = mget(ram, sx as i32, sy as i32);
        if (s != 0 || show_zero) && (layers == 0 || ram[0x3000 + usize::from(s)] & layers != 0) {
            let px = ((mx >> 13) & 7) as i32 + i32::from(s % 16) * 8;
            let py = ((my >> 13) & 7) as i32 + i32::from(s / 16) * 8;
            let c = sget(ram, px, py);
            if !transparent(ram, c) {
                let mapped = ram[PAL + usize::from(c)] & 15;
                put(ram, x, y, mapped);
            }
        }
        mx = (mx + i64::from(mdx)).rem_euclid(loopx);
        my = (my + i64::from(mdy)).rem_euclid(loopy);
        if horiz {
            if x == xend {
                break;
            }
            x += dx;
            y = lerp(y0, y1, f64::from(x - x0) / f64::from(x1 - x0));
        } else {
            if y == yend {
                break;
            }
            y += dy;
            x = lerp(x0, x1, f64::from(y - y0) / f64::from(y1 - y0));
        }
    }
}

pub fn clip(ram: &mut Ram, r: Option<(i32, i32, i32, i32)>, intersect: bool) -> [i32; 4] {
    let old = [i32::from(ram[CLIP]), i32::from(ram[CLIP + 1]), i32::from(ram[CLIP + 2]), i32::from(ram[CLIP + 3])];
    match r {
        None => clip_reset(ram),
        Some((x, y, w, h)) => {
            let (mut x0, mut y0, mut x1, mut y1) = (x, y, x + w, y + h);
            if intersect {
                x0 = x0.max(old[0]);
                y0 = y0.max(old[1]);
                x1 = x1.min(old[2]);
                y1 = y1.min(old[3]);
            }
            let c = |v: i32| v.clamp(0, 128) as u8;
            let (x0, y0) = (c(x0), c(y0));
            ram[CLIP..CLIP + 4].copy_from_slice(&[x0, y0, c(x1).max(x0), c(y1).max(y0)]);
        }
    }
    [old[0], old[1], old[2] - old[0], old[3] - old[1]]
}

pub fn set_camera(ram: &mut Ram, x: i32, y: i32) -> (i32, i32) {
    let old = camera(ram);
    set_i16(ram, CAMERA, x);
    set_i16(ram, CAMERA + 2, y);
    old
}

// Text.

/// A P8SCII parameter character: 0-9, then a-z for 10 on.
fn param(c: u8) -> i32 {
    match c {
        b'0'..=b'9' => i32::from(c - b'0'),
        b'a'..=b'z' => i32::from(c - b'a') + 10,
        b'A'..=b'Z' => i32::from(c - b'A') + 10,
        _ => 0,
    }
}

struct Pen {
    x: i32,
    y: i32,
    home: (i32, i32),
    fg: u8,
    bg: Option<u8>,
    wide: bool,
    tall: bool,
    invert: bool,
    custom: bool,
    width: Option<i32>,
    height: Option<i32>,
    tab: i32,
    right: i32,
}

/// Draws a glyph's rows at x, y (screen coordinates): 8 rows of 8 bits.
fn glyph(ram: &mut Ram, rows: [u8; 8], w: i32, h: i32, x: i32, y: i32, pen: &Pen) {
    let (sx, sy) = (if pen.wide { 2 } else { 1 }, if pen.tall { 2 } else { 1 });
    let fg = ram[PAL + usize::from(pen.fg & 15)] & 15;
    if let Some(bg) = pen.bg {
        let bg = ram[PAL + usize::from(bg & 15)] & 15;
        for yy in -1..h * sy {
            for xx in -1..w * sx - 1 {
                put(ram, x + xx, y + yy, bg);
            }
        }
    }
    for (row, &bits) in rows.iter().enumerate().take(h.clamp(0, 8) as usize) {
        for col in 0..w.clamp(0, 8) {
            let on = bits >> col & 1 != 0;
            if on != pen.invert {
                for py in 0..sy {
                    for px in 0..sx {
                        put(ram, x + col * sx + px, y + row as i32 * sy + py, fg);
                    }
                }
            }
        }
    }
}

/// Prints P8SCII text with its control codes from x, y (before the
/// camera); returns the x after it and the y of the line after it.
pub fn print(ram: &mut Ram, s: &[u8], x: i32, y: i32, col: u8) -> (i32, i32) {
    let attrs = ram[0x5f58];
    let mut pen = Pen {
        x,
        y,
        home: (x, y),
        fg: col,
        bg: None,
        wide: attrs & 0x81 == 0x81 && attrs & 4 != 0,
        tall: attrs & 0x81 == 0x81 && attrs & 8 != 0,
        invert: false,
        custom: attrs & 0x81 == 0x81 && attrs & 0x80 != 0 && ram[0x5600] != 0,
        width: None,
        height: None,
        tab: 16,
        right: 128,
    };
    let (cx, cy) = camera(ram);
    let mut line_h = 0;
    let mut repeat = 1;
    let mut i = 0;
    let at = |i: usize| s.get(i).copied().unwrap_or(0);
    while i < s.len() {
        let c = s[i];
        i += 1;
        let font: [u8; 8] = if pen.custom { ram[0x5600..0x5608].try_into().unwrap() } else { FONT[..8].try_into().unwrap() };
        let base_h = pen.height.unwrap_or(i32::from(font[2]));
        let step_h = (base_h + 1) * if pen.tall { 2 } else { 1 };
        match c {
            0 => break,
            1 => {
                repeat = param(at(i)).max(0);
                i += 1;
                continue;
            }
            2 => {
                pen.bg = Some(param(at(i)) as u8);
                i += 1;
            }
            3 => {
                pen.x += param(at(i)) - 16;
                i += 1;
            }
            4 => {
                pen.y += param(at(i)) - 16;
                i += 1;
            }
            5 => {
                pen.x += param(at(i)) - 16;
                pen.y += param(at(i + 1)) - 16;
                i += 2;
            }
            6 => {
                let cmd = at(i);
                i += 1;
                let mut on = true;
                let mut cmd = cmd;
                if cmd == b'-' {
                    on = false;
                    cmd = at(i);
                    i += 1;
                }
                match cmd {
                    b'1'..=b'9' => {}
                    b'c' => {
                        cls(ram, param(at(i)) as u8);
                        i += 1;
                        pen.x = 0;
                        pen.y = 0;
                    }
                    b'd' | b'r' | b's' | b'x' | b'y' => {
                        let v = param(at(i));
                        i += 1;
                        match cmd {
                            b'r' => pen.right = v * 4,
                            b's' => pen.tab = v * 4,
                            b'x' => pen.width = Some(v),
                            b'y' => pen.height = Some(v),
                            _ => {}
                        }
                    }
                    b'g' => (pen.x, pen.y) = pen.home,
                    b'h' => pen.home = (pen.x, pen.y),
                    b'j' => {
                        pen.x = param(at(i)) * 4;
                        pen.y = param(at(i + 1)) * 4;
                        i += 2;
                    }
                    b'w' => pen.wide = on,
                    b't' => pen.tall = on,
                    b'p' => {
                        pen.wide = on;
                        pen.tall = on;
                    }
                    b'i' => pen.invert = on,
                    b'#' => {
                        if !on {
                            pen.bg = None;
                        }
                    }
                    b'.' => {
                        // A one-off 8x8 glyph, its 8 bytes after.
                        let rows: [u8; 8] = std::array::from_fn(|k| at(i + k));
                        i += 8;
                        glyph(ram, rows, 8, 8, pen.x - cx, pen.y - cy, &pen);
                        pen.x += 9;
                        line_h = line_h.max(9);
                    }
                    b':' => {
                        let rows: [u8; 8] = std::array::from_fn(|k| (param(at(i + 2 * k)) << 4 | param(at(i + 2 * k + 1))) as u8);
                        i += 16;
                        glyph(ram, rows, 8, 8, pen.x - cx, pen.y - cy, &pen);
                        pen.x += 9;
                        line_h = line_h.max(9);
                    }
                    b'o' => i += 3,
                    b'@' | b'!' => {
                        // A poke from the text: address, length, then data.
                        let hex = |k: usize| param(at(k));
                        let addr = (0..4).fold(0, |a, k| a << 4 | hex(i + k)) as usize;
                        let n = (0..4).fold(0, |a, k| a << 4 | hex(i + 4 + k)) as usize;
                        i += 8;
                        for k in 0..n.min(s.len().saturating_sub(i)) {
                            ram[(addr + k) & 0xffff] = at(i + k);
                        }
                        i += n;
                    }
                    _ => {}
                }
            }
            7 => {
                // \a: a sound to play; its description runs to a space.
                while i < s.len() && s[i] != b' ' && s[i] >= 32 {
                    i += 1;
                }
            }
            8 => pen.x -= if pen.wide { 8 } else { 4 },
            9 => {
                let rel = pen.x - pen.home.0;
                pen.x = pen.home.0 + (rel / pen.tab + 1) * pen.tab;
            }
            10 => {
                pen.x = pen.home.0;
                pen.y += line_h.max(step_h);
                line_h = 0;
            }
            11 => {
                // \v: a character drawn over the last, at an offset.
                let off = param(at(i));
                let ch = at(i + 1);
                i += 2;
                let (ox, oy) = ((off % 4) - 2, (off / 4) - 8);
                draw_char(ram, ch, pen.x - 4 + ox - cx, pen.y + oy - cy, &pen);
            }
            12 => {
                pen.fg = param(at(i)) as u8;
                i += 1;
            }
            13 => pen.x = pen.home.0,
            14 => pen.custom = ram[0x5600] != 0,
            15 => pen.custom = false,
            _ => {
                for _ in 0..repeat {
                    let w = draw_char(ram, c, pen.x - cx, pen.y - cy, &pen);
                    pen.x += w;
                    if pen.x > pen.right && attrs & 0x81 == 0x81 && attrs & 0x40 != 0 {
                        pen.x = pen.home.0;
                        pen.y += step_h;
                    }
                }
                repeat = 1;
                line_h = line_h.max(step_h);
            }
        }
    }
    let font_h = if pen.custom { i32::from(ram[0x5602]) } else { i32::from(FONT[2]) };
    let step = (pen.height.unwrap_or(font_h) + 1) * if pen.tall { 2 } else { 1 };
    // The x where the text ended, and the y of the line after it.
    (pen.x, pen.y + line_h.max(step))
}

/// Draws a printable character; its advance.
fn draw_char(ram: &mut Ram, c: u8, x: i32, y: i32, pen: &Pen) -> i32 {
    let font = if pen.custom { 0x5600 } else { usize::MAX };
    let byte = |k: usize| if font == usize::MAX { FONT[k] } else { ram[(font + k) & 0xffff] };
    let wide_char = c >= 128;
    let mut w = i32::from(if wide_char { byte(1) } else { byte(0) });
    let h = i32::from(byte(2));
    if let Some(pw) = pen.width {
        w = pw;
    }
    if pen.custom && byte(5) & 1 != 0 && c >= 16 {
        // Each character's width adjustment, a nibble each.
        let nib = byte(8 + usize::from(c - 16) / 2) >> ((c & 1) * 4) & 15;
        let adj = i32::from(nib & 7) - if nib & 7 >= 4 { 8 } else { 0 };
        w += adj;
    }
    let rows: [u8; 8] = std::array::from_fn(|k| byte(usize::from(c) * 8 + k));
    let gw = if wide_char { 8 } else { w.min(8) };
    if c > 32 || (16..32).contains(&c) {
        let (ox, oy) = if pen.custom { (i32::from(byte(3) as i8), i32::from(byte(4) as i8)) } else { (0, 0) };
        glyph(ram, rows, gw, pen.height.unwrap_or(h), x + ox, y + oy, pen);
    } else if pen.bg.is_some() {
        glyph(ram, [0; 8], gw, pen.height.unwrap_or(h), x, y, pen);
    }
    w * if pen.wide { 2 } else { 1 }
}

/// Prints at the cursor, as print() without coordinates: below the last
/// line, scrolling the screen up at the bottom.
/// Prints at the cursor, as print() without coordinates: the cursor goes
/// to the line after, and when that line would not fit, the screen scrolls
/// up 6 pixels (as zepto8 has PICO-8 do it, after printing).
pub fn print_at_cursor(ram: &mut Ram, s: &[u8], col: u8) -> (i32, i32) {
    let (x, y) = (i32::from(ram[CURSOR]), i32::from(ram[CURSOR + 1]));
    let (end_x, mut next_y) = print(ram, s, x, y, col);
    if next_y + 6 > 128 && ram[0x5f36] & 0x40 == 0 {
        let base = screen_base(ram);
        if base + 0x2000 <= 0x10000 {
            ram.copy_within(base + 6 * 64..base + 0x2000, base);
            ram[base + 0x2000 - 6 * 64..base + 0x2000].fill(0);
        }
        next_y -= 6;
    }
    ram[CURSOR] = x as u8;
    ram[CURSOR + 1] = next_y.clamp(0, 255) as u8;
    (end_x, next_y)
}

pub fn set_cursor(ram: &mut Ram, x: i32, y: i32) -> (i32, i32) {
    let old = (i32::from(ram[CURSOR]), i32::from(ram[CURSOR + 1]));
    ram[CURSOR] = x as u8;
    ram[CURSOR + 1] = y as u8;
    old
}

/// The picture on the display: each pixel through the screen palette
/// (0..31, the secret colours from 16), in the screen mode of 0x5f2c.
pub fn present(ram: &Ram, out: &mut [u8; WIDTH * HEIGHT]) {
    let base = screen_base(ram) & 0xe000;
    let mut colours = [0u8; 16];
    for (c, o) in colours.iter_mut().enumerate() {
        let v = ram[SCREEN_PAL + c];
        *o = (v & 15) | if v & 0x80 != 0 { 16 } else { 0 };
    }
    let mode = ram[0x5f2c];
    if mode == 0 {
        // The usual screen: two pixels a byte, row after row.
        let mut pair = [0u16; 256];
        for (b, p) in pair.iter_mut().enumerate() {
            *p = u16::from(colours[b & 15]) | u16::from(colours[b >> 4]) << 8;
        }
        for (i, o) in out.chunks_exact_mut(2).enumerate() {
            let [lo, hi] = pair[usize::from(ram[(base + i) & 0xffff])].to_le_bytes();
            o[0] = lo;
            o[1] = hi;
        }
        return;
    }
    let px = |x: usize, y: usize| -> u8 {
        let b = ram[(base + y * 64 + x / 2) & 0xffff];
        colours[usize::from(if x & 1 == 0 { b & 15 } else { b >> 4 })]
    };
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let (sx, sy) = match mode {
                1 => (x / 2, y),
                2 => (x, y / 2),
                3 => (x / 2, y / 2),
                5 => (if x < 64 { x } else { 127 - x }, y),
                6 => (x, if y < 64 { y } else { 127 - y }),
                7 => (if x < 64 { x } else { 127 - x }, if y < 64 { y } else { 127 - y }),
                129 => (127 - x, y),
                130 => (x, 127 - y),
                131 => (127 - x, 127 - y),
                133 => (y, 127 - x),
                134 => (127 - x, 127 - y),
                135 => (127 - y, x),
                _ => (x, y),
            };
            out[y * WIDTH + x] = px(sx, sy);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fix::ONE;

    fn ram() -> Box<Ram> {
        let mut r = Box::new([0u8; 0x10000]);
        reset(&mut r);
        r
    }

    #[test]
    fn pixels_camera_clip_and_palette() {
        let mut r = ram();
        pset(&mut r, 3, 4, 8);
        assert_eq!(pget(&r, 3, 4), 8);
        assert_eq!(r[0x6000 + 4 * 64 + 1], 0x80);
        set_camera(&mut r, 1, 1);
        pset(&mut r, 3, 4, 9);
        assert_eq!(pget(&r, 3, 4), 9);
        set_camera(&mut r, 0, 0);
        assert_eq!(pget(&r, 2, 3), 9);
        clip(&mut r, Some((0, 0, 2, 2)), false);
        pset(&mut r, 5, 5, 7);
        clip(&mut r, None, false);
        assert_eq!(pget(&r, 5, 5), 0);
        r[0x5f00 + 7] = 12;
        pset(&mut r, 0, 0, 7);
        assert_eq!(pget(&r, 0, 0), 12);
    }

    #[test]
    fn sprites_skip_transparent_colour_zero() {
        let mut r = ram();
        sset(&mut r, 0, 0, 5);
        sset(&mut r, 1, 0, 0);
        rect(&mut r, 0, 0, 7, 7, 3, true);
        spr(&mut r, 0, 0, 0, 1.0, 1.0, false, false);
        assert_eq!(pget(&r, 0, 0), 5);
        assert_eq!(pget(&r, 1, 0), 3);
        spr(&mut r, 0, 10, 0, 1.0, 1.0, true, false);
        assert_eq!(pget(&r, 17, 0), 5);
    }

    #[test]
    fn text_uses_the_font_and_returns_its_end() {
        let mut r = ram();
        let (x, y) = print(&mut r, b"hi", 0, 0, 7);
        assert_eq!((x, y), (8, 6));
        let lit = (0..8).flat_map(|x| (0..6).map(move |y| (x, y))).filter(|&(x, y)| pget(&r, x, y) == 7).count();
        assert!(lit > 6);
        let (x, _) = print(&mut r, b"\x8b", 0, 10, 7);
        assert_eq!(x, 8);
    }

    #[test]
    fn tline_reads_the_map_with_masks_and_offsets() {
        let mut r = ram();
        // Sprite 1: its top row 0..7 coloured 1..8 (colour 8 at x=7).
        for x in 0..8 {
            sset(&mut r, 8 + x, 0, (x + 1) as u8);
        }
        mset(&mut r, 2, 0, 1);
        // From cell 2, an eighth of a cell a pixel: the sprite's row.
        tline(&mut r, 0, 0, 7, 0, 2 << 16, 0, ONE / 8, 0, 0);
        assert_eq!((0..8).map(|x| pget(&r, x, 0)).collect::<Vec<_>>(), [1, 2, 3, 4, 5, 6, 7, 8]);
        // A mask of 2 cells wraps cell 4 to 0; an offset of 2 moves it to 2.
        r[0x5f38] = 2;
        r[0x5f3a] = 2;
        tline(&mut r, 0, 1, 7, 1, 4 << 16, 0, ONE / 8, 0, 0);
        assert_eq!(pget(&r, 7, 1), 8);
        // Layers: a sprite without the flag is not drawn.
        r[0x5f38] = 0;
        r[0x5f3a] = 0;
        tline(&mut r, 0, 2, 7, 2, 2 << 16, 0, ONE / 8, 0, 4);
        assert_eq!(pget(&r, 0, 2), 0);
    }

    #[test]
    fn fill_pattern_and_shapes() {
        let mut r = ram();
        r[FILL] = 0xff;
        r[FILL + 1] = 0x00;
        rect(&mut r, 0, 0, 3, 3, 0x17, true);
        assert_eq!(pget(&r, 0, 0), 7);
        assert_eq!(pget(&r, 0, 3), 1);
        r[FILL] = 0;
        circ(&mut r, 64, 64, 10, 8, true);
        assert_eq!(pget(&r, 64, 64), 8);
        assert_eq!(pget(&r, 74, 64), 8);
        assert_eq!(pget(&r, 75, 64), 0);
        oval(&mut r, 0, 100, 20, 110, 9, false);
        assert_eq!(pget(&r, 10, 100), 9);
        assert_eq!(pget(&r, 10, 105), 0);
    }
}
