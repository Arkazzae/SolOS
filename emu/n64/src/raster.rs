//! The RDP's drawing: triangles and rectangles into the color image (and
//! the depth image) in RDRAM, through the pixel pipeline: texturing, the
//! color combiner, alpha compare, the depth test and the blender. A
//! software rasterizer for what games use, after angrylion's (the reference
//! software RDP) where the chip's arithmetic shows (texture coordinates,
//! the depth format, the blender's sums), with deliberate simplifications:
//!
//! - Every drawn pixel counts as fully covered: no anti-aliasing, and the
//!   blender blends only where force_blend asks it to. A pixel is drawn
//!   when, on any of its line's four subscanlines, the span covers the
//!   pixel's center (half-open, so triangles sharing an edge leave no gaps
//!   and do not overlap there).
//! - No dithering, no texture LOD (mipmaps), no chroma key, no YUV.
//! - The depth image keeps the RDP's compressed 16-bit format, without
//!   the two hidden bits of RDRAM's ninth bits.

use crate::N64;
use crate::rdp::{Rdp, Tile};

/// What drawing reaches: the RDP's state, RDRAM, and the CPU's cycle count
/// when the command came (the dithered alpha's and the combiner's noise use
/// it). The machine's own, or a copy of the RDP's on another thread.
pub(crate) struct Raster<'a> {
    pub rdp: &'a mut Rdp,
    pub rdram: &'a mut [u8],
    pub cycles: u64,
    pub lines: Lines,
}

/// The lines a Raster draws: y with y % every == first (one of several
/// drawing threads draws those).
#[derive(Clone, Copy)]
pub(crate) struct Lines {
    pub first: u32,
    pub every: u32,
}

impl Lines {
    pub const ALL: Lines = Lines { first: 0, every: 1 };

    /// Its lines from y0 to y1 (exclusive).
    fn within(self, y0: u32, y1: u32) -> std::iter::StepBy<std::ops::Range<u32>> {
        let from = y0 + (self.first + self.every - y0 % self.every) % self.every;
        (from..y1.max(from)).step_by(self.every as usize)
    }
}

impl N64 {
    /// Drawing's view of this machine.
    pub(crate) fn raster(&mut self) -> Raster<'_> {
        Raster { rdp: &mut self.rdp, rdram: &mut self.rdram, cycles: self.cycles, lines: Lines::ALL }
    }
}

// Cycle types (1-cycle is 0).
const TWO_CYCLE: u32 = 1;
const COPY: u32 = 2;
const FILL: u32 = 3;

/// What a primitive gives each pixel, at a point and per pixel to the
/// right: shade color (r, g, b, a in s15.16), texture coordinates (s, t,
/// w with their top 16 bits s10.5) and depth (s15.16).
#[derive(Clone, Copy, Default)]
struct Attrs {
    v: [i32; 8],
}

const R: usize = 0;
const S: usize = 4;
const T: usize = 5;
const W: usize = 6;
const Z: usize = 7;

impl Attrs {
    fn step(&mut self, d: &Attrs) {
        for i in 0..8 {
            self.v[i] = self.v[i].wrapping_add(d.v[i]);
        }
    }

    fn add_scaled(&self, d: &Attrs, n: i32) -> Attrs {
        Attrs { v: std::array::from_fn(|i| self.v[i].wrapping_add(d.v[i].wrapping_mul(n))) }
    }
}

/// What a primitive draws with: its texture tile, whether it has shade,
/// texture coordinates and depth, and the depth slope for the depth test.
#[derive(Clone, Copy)]
struct Primitive {
    tile: usize,
    texture: bool,
    persp: bool,
    dz: u32,
}

/// An RGBA color, each channel 0-255 (or a little beyond, where the
/// combiner's 9-bit arithmetic goes).
type Color = [i32; 4];

/// The z-buffer's 14-bit format: an exponent (how many leading ones the
/// 18-bit depth has, up to 7) and 11 bits of mantissa (angrylion,
/// zbuffer.c).
fn z_compress(z: u32) -> u16 {
    let z = z & 0x3ffff;
    let exponent = (!(z << 14)).leading_zeros().min(7);
    // 6, 5, 4, 3, 2, 1, 0, 0 by the exponent.
    let shift = 6u32.saturating_sub(exponent);
    (exponent << 11 | (z >> shift) & 0x7ff) as u16
}

fn z_decompress(compressed: u16) -> u32 {
    const DECODE: [(u32, u32); 8] =
        [(6, 0), (5, 0x20000), (4, 0x30000), (3, 0x38000), (2, 0x3c000), (1, 0x3e000), (0, 0x3f000), (0, 0x3f800)];
    let (shift, add) = DECODE[(compressed >> 11 & 7) as usize];
    ((u32::from(compressed & 0x7ff) << shift) + add) & 0x3ffff
}

/// The depth test (`depth_test`'s) on the stored 16 bits, with the pixel's depth also compressed. In
/// front (or over the cleared maximum) compares the codes: z_compress gives the largest code whose depth
/// is at most z and z_decompress grows with the code, so z < z_decompress(code) exactly when
/// z_compress(z) < code. Decals need the depths themselves.
#[inline(always)]
fn depth_passes(stored: u16, z: u32, compressed: u16, dz: u32, z_mode: u64) -> bool {
    let code = stored >> 2;
    if z_mode == 3 {
        let old = z_decompress(code);
        let max = old == 0x3ffff;
        let dz_memory = 1u32 << ((stored & 3) << 2);
        let delta = (dz | dz_memory).checked_ilog2().map_or(0, |b| 1 << b) << 3;
        z + delta >= old && z as i64 - delta as i64 <= old as i64 && !max
    } else {
        code == 0x3fff || compressed < code
    }
}

/// The 4-bit exponent the depth slope is stored as.
fn dz_compress(value: u32) -> u32 {
    let mut j = 0;
    if value & 0xff00 != 0 {
        j |= 8;
    }
    if value & 0xf0f0 != 0 {
        j |= 4;
    }
    if value & 0xcccc != 0 {
        j |= 2;
    }
    if value & 0xaaaa != 0 {
        j |= 1;
    }
    j
}

/// The combiner's 9-bit result clamped to a channel: above 255 to 255,
/// negative (the top of the 9 bits' range) to 0. A table, as the branches
/// were mispredicted pixel after pixel.
fn clamp9(value: i32) -> i32 {
    i32::from(CLAMP9[(value & 0x1ff) as usize])
}

static CLAMP9: [u8; 512] = {
    let mut table = [0; 512];
    let mut v = 0;
    while v < 512 {
        table[v] = if v & 0x100 == 0 { v as u8 } else if v & 0x80 != 0 { 0 } else { 255 };
        v += 1;
    }
    table
};

#[cfg(test)]
fn clamp9_branches(value: i32) -> i32 {
    let v = value & 0x1ff;
    if v & 0x100 == 0 {
        v
    } else if v & 0x80 != 0 {
        0
    } else {
        255
    }
}

/// Structural dependency on framebuffer color/coverage. The first
/// two-cycle stage always blends; the final stage blends only when
/// force_blend is set. Keep every possibly selected memory input even
/// where particular coefficient or color values might cancel it out.
fn blender_needs_memory(other: u64, two_cycle: bool) -> bool {
    let force = other >> 14 & 1 != 0;
    let cycle = |n: u32, blends: bool| {
        other >> (30 - 2 * n) & 3 == 1
            || blends && (other >> (22 - 2 * n) & 3 == 1 || other >> (18 - 2 * n) & 3 == 1)
    };
    cycle(0, two_cycle || force) || two_cycle && cycle(1, force)
}

/// Derived only from one cycle's selector fields, never serialized.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CombinePlan {
    rgb: u8,
    alpha: u8,
    rgb_source: u8,
    alpha_source: u8,
}

pub(crate) fn classify_combiner(m: u64, cycle: usize) -> Option<CombinePlan> {
    let bits = |shift: u32, width: u32| (m >> shift & ((1 << width) - 1)) as usize;
    let (a, b, c, d) = if cycle == 0 {
        (bits(52, 4), bits(28, 4), bits(47, 5), bits(15, 3))
    } else {
        (bits(37, 4), bits(24, 4), bits(32, 5), bits(6, 3))
    };
    let (aa, ab, ac, ad) = if cycle == 0 {
        (bits(44, 3), bits(12, 3), bits(41, 3), bits(9, 3))
    } else {
        (bits(21, 3), bits(3, 3), bits(18, 3), bits(0, 3))
    };
    // 0: direct D, 1/2: texel0/1 * shade,
    // 3/4: (primitive - shade) * texel0/1 + shade, 5/6: texel0/1 * primitive.
    let rgb = if c >= 16 || (a == b && a <= 5) || (a >= 8 && b >= 8) {
        0
    } else {
        match (a, b, c, d) {
            (1..=2, 8..=15, 4, 7) => a,
            (4, 8..=15, 1..=2, 7) => c,
            (3, 4, 1..=2, 4) => c + 2,
            (1..=2, 8..=15, 3, 7) => a + 4,
            (3, 8..=15, 1..=2, 7) => c + 4,
            _ => return None,
        }
    };
    let alpha = if ac == 7 || aa == ab {
        0
    } else {
        match (aa, ab, ac, ad) {
            (1..=2, 7, 4, 7) => aa,
            (4, 7, 1..=2, 7) => ac,
            (3, 4, 1..=2, 4) => ac + 2,
            (1..=2, 7, 3, 7) => aa + 4,
            (3, 7, 1..=2, 7) => ac + 4,
            _ => return None,
        }
    };
    Some(CombinePlan {
        rgb: rgb as u8, alpha: alpha as u8,
        rgb_source: d as u8, alpha_source: ad as u8,
    })
}

fn expand5(c: u32) -> i32 {
    (c << 3 | c >> 2) as i32
}

/// The perspective divide of texture coordinates (s and t by w, all s10.5
/// in their top 16 bits), as the RDP's reciprocal table does it
/// (angrylion, tcoord.c); bits 17 and 18 flag a result out of range.
fn perspective(s: i32, t: i32, w: i32) -> (i32, i32) {
    perspective_with(&PERSPECTIVE, s, t, w)
}

/// `perspective`, with the reciprocal table already at hand.
#[inline(always)]
fn perspective_with(table: &[i32], s: i32, t: i32, w: i32) -> (i32, i32) {
    let w_carry = (w as i16) <= 0;
    let entry = table[(w & 0x7fff) as usize];
    let shift = entry & 15;
    let rcp = entry >> 4;
    let divide = |c: i32| {
        let product = i32::from(c as i16) * rcp;
        let mask = ((1 << 30) - 1) & -((1 << 29) >> shift);
        let out = product & mask;
        let value = if shift != 14 { product >> (13 - shift) } else { product << 1 };
        let mut flags = match out {
            0 => 0,
            _ if out == mask => 0,
            _ if product & (1 << 29) == 0 => 2 << 17,
            _ => 1 << 17,
        };
        if w_carry {
            flags |= 2 << 17;
        }
        (value & 0x1ffff) | flags
    };
    (divide(s), divide(t))
}

static PERSPECTIVE: std::sync::LazyLock<Vec<i32>> = std::sync::LazyLock::new(|| {
    const POINT: [i32; 64] = [
        0x4000, 0x3f04, 0x3e10, 0x3d22, 0x3c3c, 0x3b5d, 0x3a83, 0x39b1, 0x38e4, 0x381c, 0x375a, 0x369d, 0x35e5, 0x3532,
        0x3483, 0x33d9, 0x3333, 0x3291, 0x31f4, 0x3159, 0x30c3, 0x3030, 0x2fa1, 0x2f15, 0x2e8c, 0x2e06, 0x2d83, 0x2d03,
        0x2c86, 0x2c0b, 0x2b93, 0x2b1e, 0x2aab, 0x2a3a, 0x29cc, 0x2960, 0x28f6, 0x288e, 0x2828, 0x27c4, 0x2762, 0x2702,
        0x26a4, 0x2648, 0x25ed, 0x2594, 0x253d, 0x24e7, 0x2492, 0x243f, 0x23ee, 0x239e, 0x234f, 0x2302, 0x22b6, 0x226c,
        0x2222, 0x21da, 0x2193, 0x214d, 0x2108, 0x20c5, 0x2082, 0x2041,
    ];
    const SLOPE: [i32; 64] = [
        0xf03, 0xf0b, 0xf11, 0xf19, 0xf20, 0xf25, 0xf2d, 0xf32, 0xf37, 0xf3d, 0xf42, 0xf47, 0xf4c, 0xf50, 0xf55, 0xf59,
        0xf5d, 0xf62, 0xf64, 0xf69, 0xf6c, 0xf70, 0xf73, 0xf76, 0xf79, 0xf7c, 0xf7f, 0xf82, 0xf84, 0xf87, 0xf8a, 0xf8c,
        0xf8e, 0xf91, 0xf93, 0xf95, 0xf97, 0xf99, 0xf9b, 0xf9d, 0xf9f, 0xfa1, 0xfa3, 0xfa4, 0xfa6, 0xfa8, 0xfa9, 0xfaa,
        0xfac, 0xfae, 0xfaf, 0xfb0, 0xfb2, 0xfb3, 0xfb5, 0xfb5, 0xfb7, 0xfb8, 0xfb9, 0xfba, 0xfbc, 0xfbc, 0xfbe, 0xfbe,
    ];
    (0..0x8000)
        .map(|i: i32| {
            let mut k = 1;
            while k <= 14 && (i << k) & 0x8000 == 0 {
                k += 1;
            }
            let shift = k - 1;
            let normout = (i << shift) & 0x3fff;
            let wnorm = (normout & 0xff) << 2;
            let index = (normout >> 8) as usize;
            let slope = (SLOPE[index] | !0x3ff) + 1;
            let rcp = (((slope * wnorm) >> 10) + POINT[index]) & 0x7fff;
            shift | rcp << 4
        })
        .collect()
});

impl Raster<'_> {
    /// A drawing command: a triangle, Texture Rectangle (and Flip) or Fill
    /// Rectangle.
    pub(crate) fn draw(&mut self, words: &[u64]) {
        match words[0] >> 56 & 0x3f {
            0x08..=0x0f => self.triangle(words),
            0x24 | 0x25 => self.texture_rectangle(words),
            _ => {
                // Fill Rectangle in fill mode: the fill color over the
                // rectangle (and its last pixel each way, as fill and copy
                // modes include it); in the other modes, as a flat-shaded
                // rectangle through the pipeline.
                let w = words[0];
                let field = |shift: u32| (w >> shift & 0xfff) as u32;
                let (lrx, lry, ulx, uly) = (field(44), field(32), field(12), field(0));
                self.draw_rectangle([ulx, uly, lrx, lry], None);
            }
        }
    }

    /// A rectangle (upper left and lower right, 10.2): Fill Rectangle, or
    /// with texture coordinates (s, t in s10.5, their steps per pixel in
    /// s5.10, flipped or not), Texture Rectangle.
    pub(crate) fn draw_rectangle(&mut self, rect: [u32; 4], texture: Option<(usize, [i32; 4], bool)>) {
        let cycle = self.rdp.cycle_type();
        let [ulx, uly, lrx, lry] = rect;
        // Fill and copy modes draw the lower right edge's pixels too.
        let inclusive = u32::from(cycle >= COPY);
        let [x0, y0, mut x1, y1] = self.clip(ulx >> 2, uly >> 2, (lrx >> 2) + inclusive, (lry >> 2) + inclusive);
        // There the right edge clipped by the scissor keeps its pixel too:
        // a line runs to the pixel the edge, at the scissor's, lies in. A
        // rectangle that starts at the scissor's right edge or beyond it
        // has no line (both its edges lie there).
        if cycle >= COPY && ulx < self.rdp.scissor[2] {
            x1 = ((lrx >> 2) + 1).min((self.rdp.scissor[2] >> 2) + 1).min(self.rdp.color_image.width);
        }
        if cycle == FILL {
            if self.rdp.profiling {
                let rows = self.lines.within(y0, y1).len() as u64;
                self.rdp.spans += rows;
                self.rdp.pixels += rows * u64::from(x1.saturating_sub(x0));
            }
            self.fill_span_rect(x0, y0, x1, y1);
            return;
        }
        let mut start = Attrs::default();
        let (mut dx, mut dy) = (Attrs::default(), Attrs::default());
        let (tile, texture) = match texture {
            Some((tile, [s, t, dsdx, dtdy], flip)) => {
                // s10.5 into the top half; the steps from s5.10.
                let (s, t) = (s << 16, t << 16);
                let (mut dsdx, dtdy) = (dsdx << 11, dtdy << 11);
                if cycle == COPY {
                    dsdx >>= 2;
                }
                // Coordinates from the rectangle's corner (with its
                // fraction) to the first pixel drawn.
                let (fx, fy) = ((x0 as i32) - (ulx >> 2) as i32, (y0 as i32) - (uly >> 2) as i32);
                if flip {
                    start.v[S] = s.wrapping_add(dsdx.wrapping_mul(fy));
                    start.v[T] = t.wrapping_add(dtdy.wrapping_mul(fx));
                    dy.v[S] = dsdx;
                    dx.v[T] = dtdy;
                } else {
                    start.v[S] = s.wrapping_add(dsdx.wrapping_mul(fx));
                    start.v[T] = t.wrapping_add(dtdy.wrapping_mul(fy));
                    dx.v[S] = dsdx;
                    dy.v[T] = dtdy;
                }
                (tile, true)
            }
            None => (0, false),
        };
        let prim = Primitive { tile, texture, persp: false, dz: self.rdp.prim_dz };
        for y in self.lines.within(y0, y1) {
            let line = start.add_scaled(&dy, (y - y0) as i32);
            self.draw_span(y, x0, x1, line, &dx, &prim);
        }
    }

    fn fill_span_rect(&mut self, x0: u32, y0: u32, x1: u32, y1: u32) {
        let image = self.rdp.color_image;
        let color = self.rdp.fill_color;
        for y in self.lines.within(y0, y1) {
            // A row that does not wrap around RDRAM's end is the fill color's bytes over and over, from
            // the byte its first pixel takes (by x, as below: 16-bit pixels take its halves in turn, 8-bit
            // ones its bytes): the same stores, four bytes at a time.
            let first = (y * image.width + x0) as usize;
            let (bytes, mask) = match image.size {
                3 => (4, 0x7f_fffc),
                2 => (2, 0x7f_fffe),
                _ => (1, 0x7f_ffff),
            };
            let at = (image.addr as usize + first * bytes) & mask;
            let end = at + (x1.saturating_sub(x0) as usize) * bytes;
            if self.rdp.accelerated && end <= self.rdram.len() {
                let phase = if bytes == 4 { 0 } else { (x0 as usize * bytes) & 3 };
                let pattern = color.rotate_left(8 * phase as u32).to_be_bytes();
                let mut words = self.rdram[at..end].chunks_exact_mut(4);
                for word in &mut words {
                    word.copy_from_slice(&pattern);
                }
                let rest = words.into_remainder();
                let n = rest.len();
                rest.copy_from_slice(&pattern[..n]);
                continue;
            }
            for x in x0..x1 {
                let i = (y * image.width + x) as usize;
                match image.size {
                    3 => self.store32(image.addr as usize + i * 4, color),
                    2 => self.store16(image.addr as usize + i * 2, (color >> if x & 1 == 0 { 16 } else { 0 }) as u16),
                    _ => self.store8(image.addr as usize + i, (color >> (24 - 8 * (x & 3))) as u8),
                }
            }
        }
    }

    /// Texture Rectangle (and Flip, with s and t's directions swapped).
    pub(crate) fn texture_rectangle(&mut self, words: &[u64]) {
        let (w0, w1) = (words[0], words[1]);
        let field = |w: u64, shift: u32| (w >> shift & 0xfff) as u32;
        let rect = [field(w0, 12), field(w0, 0), field(w0, 44), field(w0, 32)];
        let tile = (w0 >> 24 & 7) as usize;
        let half = |shift: u32| i32::from((w1 >> shift) as u16 as i16);
        let coordinates = [half(48), half(32), half(16), half(0)];
        let flip = w0 >> 56 & 0x3f == 0x25;
        self.draw_rectangle(rect, Some((tile, coordinates, flip)));
    }

    /// A triangle: its three edges (the major edge H from top to bottom,
    /// the minor edges M then L, x in s15.16 at whole scanlines, y in
    /// s11.2), then shade, texture and depth coefficients as the command
    /// has them.
    pub(crate) fn triangle(&mut self, words: &[u64]) {
        let command = words[0] >> 56 & 0x3f;
        let (shade, texture, depth) = (command & 4 != 0, command & 2 != 0, command & 1 != 0);
        let w0 = words[0];
        let left_major = w0 >> 55 & 1 != 0;
        let tile = (w0 >> 48 & 7) as usize;
        let sign14 = |v: u64| ((v as i32) << 18) >> 18;
        let (yl, ym, yh) = (sign14(w0 >> 32), sign14(w0 >> 16), sign14(w0));
        let edge = |w: u64| (((w >> 32) as i32) << 4 >> 4, (w as i32) << 2 >> 2);
        let (xl, dxldy) = edge(words[1]);
        let (xh, dxhdy) = edge(words[2]);
        let (xm, dxmdy) = edge(words[3]);

        // The coefficients: values at (xh, floor(yh)), their change per
        // pixel to the right (dx) and along the major edge per line (de).
        let (mut start, mut dx, mut de) = (Attrs::default(), Attrs::default(), Attrs::default());
        let mut at = 4;
        // A value of 16-bit integer and 16-bit fraction halves, in words
        // a and b (lane i of four).
        let combine = |a: u64, b: u64, i: u32| {
            let shift = 48 - 16 * i;
            ((a >> shift & 0xffff) << 16 | (b >> shift & 0xffff)) as u32 as i32
        };
        if shade {
            let s = &words[at..at + 8];
            for i in 0..4 {
                start.v[R + i] = combine(s[0], s[2], i as u32);
                dx.v[R + i] = combine(s[1], s[3], i as u32);
                de.v[R + i] = combine(s[4], s[6], i as u32);
            }
            at += 8;
        }
        if texture {
            let s = &words[at..at + 8];
            for i in 0..3 {
                start.v[S + i] = combine(s[0], s[2], i as u32);
                dx.v[S + i] = combine(s[1], s[3], i as u32);
                de.v[S + i] = combine(s[4], s[6], i as u32);
            }
            at += 8;
        }
        let mut dz_pixel = 0;
        if depth {
            let s = &words[at..at + 2];
            start.v[Z] = (s[0] >> 32) as u32 as i32;
            dx.v[Z] = s[0] as u32 as i32;
            de.v[Z] = (s[1] >> 32) as u32 as i32;
            let dzdy = s[1] as u32 as i32;
            // The depth slope: the integer parts of dz/dx and dz/dy.
            let magnitude = |d: i32| {
                let v = (d >> 16) & 0xffff;
                if v & 0x8000 != 0 { !v & 0x7fff } else { v }
            };
            dz_pixel = normalize_dz(((magnitude(dzdy) + magnitude(dx.v[Z])) & 0xffff) as u32);
        }
        let other = self.rdp.other_modes;
        let prim = Primitive {
            tile,
            texture,
            persp: other >> 51 & 1 != 0,
            dz: if other >> 2 & 1 != 0 { self.rdp.prim_dz } else { dz_pixel },
        };
        // Copy mode fetches four texels (of 16 bits) for each step of the
        // texture coordinates, so a pixel moves them a quarter of it.
        if self.rdp.cycle_type() == COPY {
            dx.v[S] >>= 2;
            dx.v[T] >>= 2;
        }

        let [clip_x0, clip_y0, clip_x1, clip_y1] = self.clip(0, 0, u32::MAX, u32::MAX);
        let first = (yh >> 2).max(clip_y0 as i32);
        let last = ((yl + 3) >> 2).min(clip_y1 as i32);
        let top = yh >> 2;
        // The edges' sums fit 32 bits when the slopes times the quarter lines do (with room for x and
        // the rounding): then they are worked in i32, which gives the same.
        let quarters_max = i64::from((yl - (yh & !3)).max(0) + 4);
        let fits = |slope: i32, quarters: i64| i64::from(slope).abs() * quarters < 1 << 29;
        let accelerated = self.rdp.accelerated;
        let narrow = accelerated && fits(dxhdy, quarters_max) && fits(dxmdy, quarters_max)
            && fits(dxldy, i64::from((yl - ym).max(0) + 4));
        #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
        let neon = accelerated && crate::raster_neon::available();
        for line in self.lines.within(first.max(0) as u32, last.max(0) as u32) {
            let line = line as i32;
            // The major and minor edges' x on each subscanline (quarter
            // line) that lies inside the triangle; each covers the pixels
            // whose centers lie between them.
            macro_rules! extent {
                ($t:ty) => {{
                    let (mut lo, mut hi) = (<$t>::MAX, <$t>::MIN);
                    for sub in 0..4 {
                        let k = line * 4 + sub;
                        if k < yh || k >= yl {
                            continue;
                        }
                        let quarters = <$t>::from(k - (yh & !3));
                        let major = <$t>::from(xh) + <$t>::from(dxhdy) * quarters / 4;
                        let minor = if k < ym {
                            <$t>::from(xm) + <$t>::from(dxmdy) * quarters / 4
                        } else {
                            <$t>::from(xl) + <$t>::from(dxldy) * <$t>::from(k - ym) / 4
                        };
                        let (left, right) = if left_major { (major, minor) } else { (minor, major) };
                        if right <= left {
                            continue;
                        }
                        // Pixels x with left <= x + 0.5 < right.
                        let first_x = (left - 0x8000 + 0xffff) >> 16;
                        let last_x = (right - 0x8000 + 0xffff) >> 16;
                        lo = lo.min(first_x);
                        hi = hi.max(last_x);
                    }
                    (lo, hi)
                }};
            }
            let (lo, hi) = if narrow {
                let (lo, hi) = extent!(i32);
                (i64::from(lo), i64::from(hi))
            } else {
                extent!(i64)
            };
            if lo >= hi {
                continue;
            }
            // Clipped before they become unsigned: a span can lie wholly
            // left of the screen.
            let (x0, x1) = (lo.max(i64::from(clip_x0)), hi.min(i64::from(clip_x1)));
            if x0 >= x1 {
                continue;
            }
            let (x0, x1) = (x0 as u32, x1 as u32);
            // Values on this line at the major edge, then at x0.
            let lines = line - top;
            let major_x = i64::from(xh) + i64::from(dxhdy) * i64::from(lines);
            let offset = (i64::from(x0) << 16) - major_x;
            #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
            if neon && offset == i64::from(offset as i32) {
                let v = crate::raster_neon::attributes_at(&start.v, &de.v, &dx.v, lines, offset as i32);
                self.draw_span(line as u32, x0, x1, Attrs { v }, &dx, &prim);
                continue;
            }
            let along = start.add_scaled(&de, lines);
            let at_x0 = Attrs {
                v: std::array::from_fn(|i| {
                    (i64::from(along.v[i]).wrapping_add(i64::from(dx.v[i]).wrapping_mul(offset) >> 16)) as i32
                }),
            };
            self.draw_span(line as u32, x0, x1, at_x0, &dx, &prim);
        }
    }

    /// Draws pixels x0 to x1 (exclusive) of a line, the attributes stepping
    /// by `dx` each pixel.
    fn draw_span(&mut self, y: u32, x0: u32, x1: u32, start: Attrs, dx: &Attrs, prim: &Primitive) {
        let cycle = self.rdp.cycle_type();
        // Modes and host preferences stay fixed throughout this span.
        // Compute the conservative blender dependency once, not per pixel.
        let read_memory = cycle >= COPY || !self.rdp.accelerated || !self.rdp.lazy_color_reads
            || blender_needs_memory(self.rdp.other_modes, cycle == TWO_CYCLE);
        if self.rdp.profiling {
            self.rdp.spans += 1;
            self.rdp.pixels += u64::from(x1.saturating_sub(x0));
        }
        // With NEON, span_cycles takes the one-cycle spans span_opaque leaves.
        #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
        let scalar = !crate::raster_neon::available();
        #[cfg(not(any(target_arch = "arm", target_arch = "aarch64")))]
        let scalar = true;
        if cycle <= TWO_CYCLE && self.rdp.accelerated && self.rdp.fast_spans
            && (cycle == 0
                && (self.span_opaque(y, x0, x1, start, dx, prim, read_memory)
                    || scalar && (self.span_sampled(y, x0, x1, start, dx, prim, read_memory)
                        || self.span_one_cycle(y, x0, x1, start, dx, prim, read_memory)))
                || self.span_cycles(y, x0, x1, start, dx, prim, read_memory))
        {
            return;
        }
        if cycle == COPY && self.rdp.accelerated && self.rdp.fast_spans && self.span_copy(y, x0, x1, start, dx, prim) {
            return;
        }
        let mut attrs = start;
        for x in x0..x1 {
            match cycle {
                COPY => self.copy_pixel(x, y, &attrs, prim),
                FILL => self.fill_span_rect(x, y, x + 1, y + 1),
                _ => self.pixel(x, y, &attrs, prim, read_memory),
            }
            attrs.step(dx);
        }
    }

    /// Copy mode: texels straight into the color image (16-bit, or 8-bit
    /// into 8-bit), skipping those whose alpha is clear if alpha compare
    /// is on.
    fn copy_pixel(&mut self, x: u32, y: u32, attrs: &Attrs, prim: &Primitive) {
        let rdp = &self.rdp;
        let tile = rdp.tiles[prim.tile];
        // Copy mode shifts, offsets and masks coordinates, but does not
        // clamp them.
        let axis = |coord: i32, n: usize| {
            let c = i32::from(coord as i16);
            let shift = tile.shift[n];
            let c = if shift < 11 { c >> shift } else { i32::from((c << (16 - shift)) as i16) };
            self.wrap(&tile, n, (c - (tile.lo[n] << 3) as i32) >> 5).0
        };
        let (s, t) = (axis(attrs.v[S] >> 16, 0), axis(attrs.v[T] >> 16, 1));
        let image = rdp.color_image;
        let i = (y * image.width + x) as usize;
        let tlut = rdp.other_modes >> 47 & 1 != 0;
        let alpha_compare = rdp.other_modes & 1 != 0;
        if image.size == 2 {
            let value = if tlut || tile.size < 2 {
                let index = self.texel_index(&tile, s, t);
                self.palette_entry(index)
            } else {
                self.texel16(&tile, s, t)
            };
            if alpha_compare && value & 1 == 0 {
                return;
            }
            self.store16(image.addr as usize + i * 2, value);
        } else if image.size == 1 {
            let value = self.texel_index(&tile, s, t) as u8;
            self.store8(image.addr as usize + i, value);
        }
    }

    /// Copy mode's span: copy_pixel()'s steps, the tile's axes worked out once.
    fn span_copy(&mut self, y: u32, x0: u32, x1: u32, start: Attrs, dx: &Attrs, prim: &Primitive) -> bool {
        let Raster { rdp, rdram, .. } = self;
        let (Ok(tmem), Ok(rdram)) =
            (<&[u8; 0x1000]>::try_from(rdp.tmem.as_slice()), <&mut [u8; 0x80_0000]>::try_from(&mut **rdram))
        else {
            return false;
        };
        let tile = rdp.tiles[prim.tile];
        let (axis_s, axis_t) = (Axis::new(&tile, 0), Axis::new(&tile, 1));
        let image = rdp.color_image;
        let other = rdp.other_modes;
        let (palette, alpha_compare) = (other >> 47 & 1 != 0 || tile.size < 2, other & 1 != 0);
        // Shifted, offset and masked, not clamped.
        let texel = |axis: &Axis, coord: i32| axis.wrap((axis.shifted(coord) - axis.lo) >> 5).0;
        let index = |s: i32, t: i32| {
            let at = Self::texel_address(&tile, s, t);
            let byte = u32::from(tmem[at]);
            if tile.size == 0 { tile.palette << 4 | if s & 1 == 0 { byte >> 4 } else { byte & 15 } } else { byte }
        };
        let mut attrs = start;
        for x in x0..x1 {
            let (s, t) = (texel(&axis_s, attrs.v[S] >> 16), texel(&axis_t, attrs.v[T] >> 16));
            attrs.step(dx);
            let i = y.wrapping_mul(image.width).wrapping_add(x) as usize;
            match image.size {
                2 => {
                    let value = if palette {
                        let at = 0x800 + (index(s, t) as usize & 0xff) * 8;
                        u16::from(tmem[at]) << 8 | u16::from(tmem[at + 1])
                    } else {
                        let at = Self::texel_address(&tile, s, t);
                        u16::from(tmem[at]) << 8 | u16::from(tmem[(at + 1) & 0xfff])
                    };
                    if alpha_compare && value & 1 == 0 {
                        continue;
                    }
                    let at = (image.addr as usize).wrapping_add(i.wrapping_mul(2)) & 0x7f_fffe;
                    (rdram[at], rdram[at + 1]) = ((value >> 8) as u8, value as u8);
                }
                1 => rdram[(image.addr as usize).wrapping_add(i) & 0x7f_ffff] = index(s, t) as u8,
                _ => {}
            }
        }
        true
    }

    /// One pixel through the 1- or 2-cycle pipeline.
    fn pixel(&mut self, x: u32, y: u32, attrs: &Attrs, prim: &Primitive, read_memory: bool) {
        let rdp = &self.rdp;
        let other = rdp.other_modes;
        let two_cycle = rdp.cycle_type() == TWO_CYCLE;

        // The depth, from the primitive or the attributes.
        let z_source_prim = other >> 2 & 1 != 0;
        let z = if z_source_prim {
            rdp.prim_z << 3 & 0x3ffff
        } else {
            let sz = (attrs.v[Z] >> 13) as u32;
            match sz >> 17 & 3 {
                0 | 1 => sz & 0x3ffff,
                2 => 0x3ffff,
                _ => 0,
            }
        };
        let image = rdp.color_image;
        let i = (y * image.width + x) as usize;
        let z_at = rdp.z_image as usize + i * 2;
        let (z_compare, z_update) = (other >> 4 & 1 != 0, other >> 5 & 1 != 0);
        if z_compare && !self.depth_test(z_at, z, prim.dz) {
            return;
        }

        // Texels, at the pixel's coordinates (divided by w if perspective
        // is on).
        let (texel0, texel1) = if prim.texture {
            let (s, t) = if prim.persp {
                perspective(attrs.v[S] >> 16, attrs.v[T] >> 16, attrs.v[W] >> 16)
            } else {
                ((attrs.v[S] >> 16) & 0x1ffff, (attrs.v[T] >> 16) & 0x1ffff)
            };
            let texel0 = self.sample(prim.tile, s, t);
            let texel1 = if two_cycle { self.sample((prim.tile + 1) & 7, s, t) } else { texel0 };
            (texel0, texel1)
        } else {
            ([0; 4], [0; 4])
        };
        let shade: Color = std::array::from_fn(|c| clamp9(attrs.v[R + c] >> 16));

        // The combiner: the first cycle's result feeds the second; in
        // 1-cycle mode only the second cycle's inputs count. The second of
        // two cycles takes texel 1 as its TEXEL0 and texel 0 as its TEXEL1
        // (angrylion swaps them between the cycles).
        let mut combined = if two_cycle {
            let first = self.combine(0, [0; 4], texel0, texel1, shade);
            self.combine(1, first, texel1, texel0, shade)
        } else {
            self.combine(1, [0; 4], texel0, texel1, shade)
        };
        // Coverage times alpha (cutout textures): a pixel whose coverage
        // the alpha takes to nothing is not written, as on the chip it
        // would blend away into memory. With alpha from coverage, the
        // blender sees that coverage (full, here) as the pixel's alpha.
        if other >> 12 & 1 != 0 && combined[3] >> 5 == 0 {
            return;
        }
        if other >> 13 & 1 != 0 && other >> 12 & 1 == 0 {
            combined[3] = 255;
        }

        // Alpha compare, against the blend color's alpha (or noise).
        let rdp = &self.rdp;
        if other & 1 != 0 {
            let threshold = if other >> 1 & 1 != 0 {
                (self.cycles as i32).wrapping_mul(31).wrapping_add((x as i32).wrapping_mul(17)) & 0xff
            } else {
                (rdp.blend & 0xff) as i32
            };
            if combined[3] < threshold {
                return;
            }
        }

        // The blender, with the color image's pixel as its memory color.
        let memory = if !read_memory {
            if self.rdp.profiling {
                self.rdp.skipped_color_reads += 1;
            }
            [0; 4]
        } else {
            self.read_pixel(image.addr as usize, i, image.size)
        };
        let color = self.blend(two_cycle, combined, shade[3], memory);
        self.write_pixel(image.addr as usize, i, image.size, color);
        if z_update {
            let stored = z_compress(z) << 2 | (dz_compress(prim.dz) >> 2) as u16;
            self.store16(z_at, stored);
        }
    }

    /// The depth test against the depth image, in the mode other modes
    /// set: opaque, interpenetrating and transparent surfaces draw in
    /// front (or over the cleared maximum), decals where they lie within
    /// the depth slope of what is there.
    fn depth_test(&self, at: usize, z: u32, dz: u32) -> bool {
        let Some(stored) = self.rdram.get(at & 0x7f_fffe..(at & 0x7f_fffe) + 2) else { return true };
        let stored = u16::from_be_bytes([stored[0], stored[1]]);
        let old = z_decompress(stored >> 2);
        let max = old == 0x3ffff;
        match self.rdp.other_modes >> 10 & 3 {
            3 => {
                let dz_memory = 1u32 << ((stored & 3) << 2);
                let delta = (dz | dz_memory).checked_ilog2().map_or(0, |b| 1 << b) << 3;
                let farther = z + delta >= old;
                let nearer = z as i64 - delta as i64 <= old as i64;
                farther && nearer && !max
            }
            _ => max || z < old,
        }
    }

    /// The color combiner's cycle: (A - B) * C + D for color and for
    /// alpha, from the inputs Set Combine Mode picks.
    fn combine(&self, cycle: usize, combined: Color, texel0: Color, texel1: Color, shade: Color) -> Color {
        if self.rdp.accelerated && self.rdp.fast_combiner {
            if let Some(plan) = self.rdp.combine_program[cycle] {
                return self.combine_specialized(plan, combined, texel0, texel1, shade);
            }
            return self.combine_selected(cycle, combined, texel0, texel1, shade);
        }
        self.combine_reference(cycle, combined, texel0, texel1, shade)
    }

    /// `combine_reference`'s formulas with each input picked once, as a
    /// whole color (an alpha broadcast to all four), not per channel: for
    /// the modes `classify_combiner` has no formula for.
    fn combine_selected(&self, cycle: usize, combined: Color, texel0: Color, texel1: Color, shade: Color) -> Color {
        let rdp = &self.rdp;
        let m = rdp.combine;
        let bits = |shift: u32, width: u32| (m >> shift & ((1 << width) - 1)) as usize;
        let (sub_a, mul, sub_b, add) = if cycle == 0 {
            (bits(52, 4), bits(47, 5), bits(28, 4), bits(15, 3))
        } else {
            (bits(37, 4), bits(32, 5), bits(24, 4), bits(6, 3))
        };
        let (alpha_a, alpha_mul, alpha_b, alpha_add) = if cycle == 0 {
            (bits(44, 3), bits(41, 3), bits(12, 3), bits(9, 3))
        } else {
            (bits(21, 3), bits(18, 3), bits(3, 3), bits(0, 3))
        };
        let (prim, env) = (rgba32(rdp.prim), rgba32(rdp.env));
        // As combine_reference: no mipmaps, the LOD fraction at its maximum.
        let lod_frac = 255;
        let (one, zero) = ([256; 4], [0; 4]);
        let pick = |selector: usize| match selector {
            0 => combined,
            1 => texel0,
            2 => texel1,
            3 => prim,
            4 => shade,
            5 => env,
            _ => zero,
        };
        let a = match sub_a {
            6 => one,
            7 => [((self.cycles as i32 >> 3 & 7) << 6) | 0x20; 4],
            selector => pick(selector),
        };
        let b = match sub_b {
            6 => key_center(rdp.key),
            7 => [rdp.convert[4]; 4],
            selector => pick(selector),
        };
        let c = match mul {
            0..=5 => pick(mul),
            6 => key_scale(rdp.key),
            7 => [combined[3]; 4],
            8 => [texel0[3]; 4],
            9 => [texel1[3]; 4],
            10 => [prim[3]; 4],
            11 => [shade[3]; 4],
            12 => [env[3]; 4],
            13 => [lod_frac; 4],
            14 => [rdp.prim_lod_frac as i32; 4],
            15 => [rdp.convert[5]; 4],
            _ => zero,
        };
        let d = if add == 6 { one } else { pick(add) };
        let mut out = [0; 4];
        for n in 0..3 {
            out[n] = clamp9(((a[n] - b[n]) * c[n] + (d[n] << 8) + 0x80) >> 8);
        }
        let alpha = |which: usize| if which == 6 { 256 } else { pick(which)[3] };
        let alpha_mul = match alpha_mul {
            0 => lod_frac,
            6 => rdp.prim_lod_frac as i32,
            7 => 0,
            selector => pick(selector)[3],
        };
        out[3] = clamp9(((alpha(alpha_a) - alpha(alpha_b)) * alpha_mul + (alpha(alpha_add) << 8) + 0x80) >> 8);
        out
    }

    /// Algebraic cases selected by the active cycle's selectors. Other
    /// mode bits have no bearing on these formulas. Keep clamp9 and the
    /// reference's +128 rounding even for direct source and multiply.
    fn combine_specialized(&self, plan: CombinePlan, combined: Color, texel0: Color, texel1: Color, shade: Color) -> Color {
        combine_plan(plan, rgba32(self.rdp.prim), rgba32(self.rdp.env), combined, texel0, texel1, shade)
    }

    /// The one-cycle span with a combiner formula the plan covers, which
    /// games draw nearly everything with: what the modes fix is worked out
    /// once for the span, then each pixel goes through pixel()'s steps.
    /// False where pixel() must draw it.
    #[allow(clippy::too_many_arguments)]
    fn span_one_cycle(&mut self, y: u32, x0: u32, x1: u32, start: Attrs, dx: &Attrs, prim: &Primitive,
        read_memory: bool) -> bool {
        let rdp = &self.rdp;
        let (Some(plan), true) = (rdp.combine_program[1], rdp.fast_combiner) else { return false };
        let other = rdp.other_modes;
        let z_source_prim = other >> 2 & 1 != 0;
        let prim_z = rdp.prim_z << 3 & 0x3ffff;
        let (z_compare, z_update) = (other >> 4 & 1 != 0, other >> 5 & 1 != 0);
        let stored_dz = (dz_compress(prim.dz) >> 2) as u16;
        let (cvg_times_alpha, alpha_cvg_sel) = (other >> 12 & 1 != 0, other >> 13 & 1 != 0);
        let (alpha_compare, dither_alpha) = (other & 1 != 0, other >> 1 & 1 != 0);
        let threshold = (rdp.blend & 0xff) as i32;
        let noise = (self.cycles as i32).wrapping_mul(31);
        let image = rdp.color_image;
        let (base, z_image) = (image.addr as usize, rdp.z_image as usize);
        let tile = rdp.tiles[prim.tile];
        let bilinear = other >> 45 & 1 != 0;
        let rgba16 = rdp.rgba16_sampling && tile.format == 0 && tile.size == 2;
        let (prim_color, env) = (rgba32(rdp.prim), rgba32(rdp.env));
        let (blend_color, fog) = (rgba32(rdp.blend), rgba32(rdp.fog));
        let force = other >> 14 & 1 != 0;
        let (p_sel, a_sel, m_sel, b_sel) = (other >> 30 & 3, other >> 26 & 3, other >> 22 & 3, other >> 18 & 3);
        let table = &*PERSPECTIVE;
        let profiling = rdp.profiling;
        let mut attrs = start;
        for x in x0..x1 {
            let a = attrs;
            attrs.step(dx);
            let z = if z_source_prim {
                prim_z
            } else {
                let sz = (a.v[Z] >> 13) as u32;
                match sz >> 17 & 3 {
                    0 | 1 => sz & 0x3ffff,
                    2 => 0x3ffff,
                    _ => 0,
                }
            };
            let i = (y * image.width + x) as usize;
            let z_at = z_image + i * 2;
            if z_compare && !self.depth_test(z_at, z, prim.dz) {
                continue;
            }
            let texel = if prim.texture {
                let (s, t) = if prim.persp {
                    perspective_with(table, a.v[S] >> 16, a.v[T] >> 16, a.v[W] >> 16)
                } else {
                    ((a.v[S] >> 16) & 0x1ffff, (a.v[T] >> 16) & 0x1ffff)
                };
                if profiling {
                    self.rdp.three_texel_samples += u64::from(bilinear);
                    self.rdp.rgba16_samples += u64::from(rgba16);
                }
                if rgba16 {
                    self.sample_tile::<true>(&tile, bilinear, s, t)
                } else {
                    self.sample_tile::<false>(&tile, bilinear, s, t)
                }
            } else {
                [0; 4]
            };
            let shade: Color = std::array::from_fn(|c| clamp9(a.v[R + c] >> 16));
            let mut combined = combine_plan(plan, prim_color, env, [0; 4], texel, texel, shade);
            if cvg_times_alpha && combined[3] >> 5 == 0 {
                continue;
            }
            if alpha_cvg_sel && !cvg_times_alpha {
                combined[3] = 255;
            }
            if alpha_compare {
                let threshold =
                    if dither_alpha { noise.wrapping_add((x as i32).wrapping_mul(17)) & 0xff } else { threshold };
                if combined[3] < threshold {
                    continue;
                }
            }
            let memory = if read_memory {
                self.read_pixel(base, i, image.size)
            } else {
                if profiling {
                    self.rdp.skipped_color_reads += 1;
                }
                [0; 4]
            };
            // The blender's only cycle, blending only with force_blend.
            let pick = |sel: u64| match sel {
                0 => combined,
                1 => memory,
                2 => blend_color,
                _ => fog,
            };
            let p = pick(p_sel);
            let color = if force {
                let m = pick(m_sel);
                let a = match a_sel {
                    0 => combined[3],
                    1 => fog[3],
                    2 => shade[3],
                    _ => 0,
                } & 0xff;
                let b = match b_sel {
                    0 => !a & 0xff,
                    1 => memory[3],
                    2 => 0xff,
                    _ => 0,
                };
                let (a5, b5) = (a >> 3, (b >> 3) + 1);
                let mix = |c: usize| ((p[c] * a5 + m[c] * b5) >> 5).clamp(0, 255);
                [mix(0), mix(1), mix(2), combined[3]]
            } else {
                [p[0], p[1], p[2], combined[3]]
            };
            self.write_pixel(base, i, image.size, color);
            if z_update {
                self.store16(z_at, z_compress(z) << 2 | stored_dz);
            }
        }
        if profiling {
            self.rdp.fast_span_pixels += u64::from(x1 - x0);
        }
        true
    }

    /// Any one- or two-cycle span the faster ones leave: pixel()'s steps, with what the modes fix
    /// worked out once for the span (the combiner's inputs as whole colors or alphas, the
    /// blender's selectors, the tiles' axes and texel formats), each pixel's arithmetic pixel()'s.
    /// False where pixel() must draw it.
    #[allow(clippy::too_many_arguments)]
    fn span_cycles(&mut self, y: u32, x0: u32, x1: u32, start: Attrs, dx: &Attrs, prim: &Primitive,
        read_memory: bool) -> bool {
        #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
        if crate::raster_neon::available() {
            return self.span_cycles_neon(y, x0, x1, start, dx, prim, read_memory);
        }
        let cycles = self.cycles;
        let Raster { rdp, rdram, .. } = self;
        let (Ok(tmem), Ok(rdram)) =
            (<&[u8; 0x1000]>::try_from(rdp.tmem.as_slice()), <&mut [u8; 0x80_0000]>::try_from(&mut **rdram))
        else {
            return false;
        };
        if !rdp.fast_combiner {
            return false;
        }
        let other = rdp.other_modes;
        let two_cycle = other >> 52 & 3 == TWO_CYCLE as u64;
        let z_source_prim = other >> 2 & 1 != 0;
        let prim_z = rdp.prim_z << 3 & 0x3ffff;
        let (z_compare, z_update, z_mode) = (other >> 4 & 1 != 0, other >> 5 & 1 != 0, other >> 10 & 3);
        let stored_dz = (dz_compress(prim.dz) >> 2) as u16;
        let (cvg_times_alpha, alpha_cvg_sel) = (other >> 12 & 1 != 0, other >> 13 & 1 != 0);
        let (alpha_compare, dither_alpha) = (other & 1 != 0, other >> 1 & 1 != 0);
        let threshold = (rdp.blend & 0xff) as i32;
        let noise = (cycles as i32).wrapping_mul(31);
        let image = rdp.color_image;
        let (base, z_image) = (image.addr as usize, rdp.z_image as usize);
        let (prim_color, env) = (rgba32(rdp.prim), rgba32(rdp.env));
        let (blend_color, fog) = (rgba32(rdp.blend), rgba32(rdp.fog));
        let force = other >> 14 & 1 != 0;
        // The blender's selectors in each cycle: p, a, m, b.
        let selectors = |n: u64| (other >> (30 - 2 * n) & 3, other >> (26 - 2 * n) & 3, other >> (22 - 2 * n) & 3,
            other >> (18 - 2 * n) & 3);
        let blender = [selectors(0), selectors(1)];
        // The combiner's cycles: the plan, or each input picked once.
        let combiner = [0, 1].map(|cycle| match rdp.combine_program[cycle] {
            Some(plan) => CycleInputs::Plan(plan),
            None => CycleInputs::Picked(PickedInputs::new(rdp, cycle, cycles)),
        });
        // The tiles: texel 0's and, in two-cycle mode, texel 1's.
        let (tlut, tlut_ia) = (other >> 47 & 1 != 0, other >> 46 & 1 != 0);
        let bilinear = other >> 45 & 1 != 0;
        let tile_of = |index: usize| {
            let tile = rdp.tiles[index];
            let format = if tlut && tile.size < 2 {
                TexelFormat::Palette { ia: tlut_ia }
            } else {
                match (tile.format, tile.size) {
                    (0, 3) => TexelFormat::Rgba32,
                    (0, _) => TexelFormat::Rgba16,
                    (3, 2) => TexelFormat::Ia16,
                    (3, 1) => TexelFormat::Ia8,
                    (3, 0) => TexelFormat::Ia4,
                    (_, 0) => TexelFormat::I4,
                    (_, 1) => TexelFormat::I8,
                    _ => TexelFormat::Rgba16,
                }
            };
            (tile, format, Axis::new(&tile, 0), Axis::new(&tile, 1))
        };
        let tiles = [tile_of(prim.tile), tile_of((prim.tile + 1) & 7)];
        // Each tile's sampler, its texel format and filter fixed.
        let samplers = [0, 1].map(|n| sampler(tiles[n].1, bilinear));
        // The texels the combiner's cycles take (in one cycle, texel 1 is texel 0): sampling has no
        // other effect, so the others are not sampled.
        let [uses0, uses1] = texels_used(rdp.combine, two_cycle);
        let (sample0, sample1) = (uses0 || !two_cycle && uses1, two_cycle && uses1);
        // The samples pixel() would count for a textured pixel: bilinear ones, and RGBA16 ones.
        let rgba16 = |tile: &Tile| u64::from(rdp.rgba16_sampling && tile.format == 0 && tile.size == 2);
        let samples = 1 + u64::from(two_cycle);
        let (three_each, sixteen_each) = (samples * u64::from(bilinear),
            rgba16(&tiles[0].0) + if two_cycle { rgba16(&tiles[1].0) } else { 0 });
        let table = &*PERSPECTIVE;
        let (mut skipped, mut three, mut sixteen) = (0, 0, 0);
        let mut attrs = start;
        for x in x0..x1 {
            let a = attrs;
            attrs.step(dx);
            let z = if z_source_prim {
                prim_z
            } else {
                let sz = (a.v[Z] >> 13) as u32;
                match sz >> 17 & 3 {
                    0 | 1 => sz & 0x3ffff,
                    2 => 0x3ffff,
                    _ => 0,
                }
            };
            let i = y.wrapping_mul(image.width).wrapping_add(x) as usize;
            let z_at = z_image.wrapping_add(i.wrapping_mul(2)) & 0x7f_fffe;
            let compressed = z_compress(z);
            if z_compare {
                let stored = u16::from(rdram[z_at]) << 8 | u16::from(rdram[z_at + 1]);
                if !depth_passes(stored, z, compressed, prim.dz, z_mode) {
                    continue;
                }
            }
            let (texel0, texel1) = if prim.texture {
                let (s, t) = if prim.persp {
                    perspective_with(table, a.v[S] >> 16, a.v[T] >> 16, a.v[W] >> 16)
                } else {
                    ((a.v[S] >> 16) & 0x1ffff, (a.v[T] >> 16) & 0x1ffff)
                };
                (three, sixteen) = (three + three_each, sixteen + sixteen_each);
                let texel0 = if sample0 { samplers[0](tmem, &tiles[0], s, t) } else { [0; 4] };
                let texel1 = if sample1 { samplers[1](tmem, &tiles[1], s, t) } else { texel0 };
                (texel0, texel1)
            } else {
                ([0; 4], [0; 4])
            };
            let shade: Color = std::array::from_fn(|c| clamp9(a.v[R + c] >> 16));
            let mut combined = if two_cycle {
                // The second cycle with the texels the other way round, as pixel() has it.
                let first = combiner[0].combine(prim_color, env, &[[0; 4], texel0, texel1, shade]);
                combiner[1].combine(prim_color, env, &[first, texel1, texel0, shade])
            } else {
                combiner[1].combine(prim_color, env, &[[0; 4], texel0, texel1, shade])
            };
            if cvg_times_alpha && combined[3] >> 5 == 0 {
                continue;
            }
            if alpha_cvg_sel && !cvg_times_alpha {
                combined[3] = 255;
            }
            if alpha_compare {
                let threshold =
                    if dither_alpha { noise.wrapping_add((x as i32).wrapping_mul(17)) & 0xff } else { threshold };
                if combined[3] < threshold {
                    continue;
                }
            }
            let memory = if !read_memory {
                skipped += 1;
                [0; 4]
            } else if image.size == 3 {
                let at = base.wrapping_add(i.wrapping_mul(4)) & 0x7f_fffc;
                let p = u32::from_be_bytes(rdram[at..at + 4].try_into().unwrap());
                [(p >> 24) as i32, (p >> 16 & 0xff) as i32, (p >> 8 & 0xff) as i32, (p & 0xe0) as i32]
            } else {
                let at = base.wrapping_add(i.wrapping_mul(2)) & 0x7f_fffe;
                let p = u32::from(rdram[at]) << 8 | u32::from(rdram[at + 1]);
                [expand5(p >> 11 & 31), expand5(p >> 6 & 31), expand5(p >> 1 & 31), if p & 1 != 0 { 0xe0 } else { 0 }]
            };
            // The blender's cycles, as blend() has them.
            let cycle = |n: usize, pixel: Color, always: bool| -> Color {
                let (p_sel, a_sel, m_sel, b_sel) = blender[n];
                let pick = |sel: u64| match sel {
                    0 => pixel,
                    1 => memory,
                    2 => blend_color,
                    _ => fog,
                };
                if (p_sel, a_sel, m_sel, b_sel) == (0, 3, 0, 2) {
                    // The pixel by nothing plus itself by one: the pixel (clamped where it blends).
                    return if !always && !force { [pixel[0], pixel[1], pixel[2], combined[3]] } else {
                        [pixel[0].clamp(0, 255), pixel[1].clamp(0, 255), pixel[2].clamp(0, 255), combined[3]]
                    };
                }
                let (p, m) = (pick(p_sel), pick(m_sel));
                if !always && !force {
                    return [p[0], p[1], p[2], combined[3]];
                }
                let a = match a_sel {
                    0 => combined[3],
                    1 => fog[3],
                    2 => shade[3],
                    _ => 0,
                } & 0xff;
                let b = match b_sel {
                    0 => !a & 0xff,
                    1 => memory[3],
                    2 => 0xff,
                    _ => 0,
                };
                let (a5, b5) = (a >> 3, (b >> 3) + 1);
                let mix = |c: usize| ((p[c] * a5 + m[c] * b5) >> 5).clamp(0, 255);
                [mix(0), mix(1), mix(2), combined[3]]
            };
            let color = if two_cycle { cycle(1, cycle(0, combined, true), false) } else { cycle(0, combined, false) };
            let [r, g, b, _] = color.map(|c| c.clamp(0, 255) as u32);
            match image.size {
                3 => {
                    let at = base.wrapping_add(i.wrapping_mul(4)) & 0x7f_fffc;
                    rdram[at..at + 4].copy_from_slice(&(r << 24 | g << 16 | b << 8 | 0xe0).to_be_bytes());
                }
                2 => {
                    let at = base.wrapping_add(i.wrapping_mul(2)) & 0x7f_fffe;
                    let v = (r >> 3) << 11 | (g >> 3) << 6 | (b >> 3) << 1 | 1;
                    (rdram[at], rdram[at + 1]) = ((v >> 8) as u8, v as u8);
                }
                _ => {}
            }
            if z_update {
                let v = compressed << 2 | stored_dz;
                (rdram[z_at], rdram[z_at + 1]) = ((v >> 8) as u8, v as u8);
            }
        }
        if rdp.profiling {
            rdp.three_texel_samples += three;
            rdp.rgba16_samples += sixteen;
            rdp.skipped_color_reads += skipped;
            rdp.fast_span_pixels += u64::from(x1 - x0);
        }
        true
    }

    /// span_cycles with NEON: the same depth test, alpha tests and selectors, the texels packed and
    /// filtered, the shade, both combiner cycles (each input's lanes picked by a table lookup), the
    /// blender's mix and the RGBA5551 store on four lanes.
    #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
    #[allow(clippy::too_many_arguments)]
    fn span_cycles_neon(&mut self, y: u32, x0: u32, x1: u32, start: Attrs, dx: &Attrs, prim: &Primitive,
        read_memory: bool) -> bool {
        use crate::raster_neon::{self as neon, Lanes};
        let cycles = self.cycles;
        let Raster { rdp, rdram, .. } = self;
        let (Ok(tmem), Ok(rdram)) =
            (<&[u8; 0x1000]>::try_from(rdp.tmem.as_slice()), <&mut [u8; 0x80_0000]>::try_from(&mut **rdram))
        else {
            return false;
        };
        if !rdp.fast_combiner {
            return false;
        }
        let other = rdp.other_modes;
        let two_cycle = other >> 52 & 3 == TWO_CYCLE as u64;
        let z_source_prim = other >> 2 & 1 != 0;
        let prim_z = rdp.prim_z << 3 & 0x3ffff;
        let (z_compare, z_update, z_mode) = (other >> 4 & 1 != 0, other >> 5 & 1 != 0, other >> 10 & 3);
        let stored_dz = (dz_compress(prim.dz) >> 2) as u16;
        let (cvg_times_alpha, alpha_cvg_sel) = (other >> 12 & 1 != 0, other >> 13 & 1 != 0);
        let (alpha_compare, dither_alpha) = (other & 1 != 0, other >> 1 & 1 != 0);
        let threshold = (rdp.blend & 0xff) as i32;
        let noise = (cycles as i32).wrapping_mul(31);
        let image = rdp.color_image;
        let (base, z_image) = (image.addr as usize, rdp.z_image as usize);
        let lanes = |c: Color| Lanes::pair(c[0] as u32 | (c[1] as u32) << 16, c[2] as u32 | (c[3] as u32) << 16);
        let fog_alpha = (rdp.fog & 0xff) as i32;
        let (blend_color, fog) = (lanes(rgba32(rdp.blend)), lanes(rgba32(rdp.fog)));
        let force = other >> 14 & 1 != 0;
        // The blender's selectors in each cycle: p, a, m, b.
        let selectors = |n: u64| (other >> (30 - 2 * n) & 3, other >> (26 - 2 * n) & 3, other >> (22 - 2 * n) & 3,
            other >> (18 - 2 * n) & 3);
        let blender = [selectors(0), selectors(1)];
        // The combiner's cycles, each input's lanes picked once (as combine_selected picks them, which is
        // what every plan computes too). A second cycle that gives the first's color back is left out.
        let (combiner, first_only, [uses0, uses1]) = combiner_lanes(rdp, cycles, two_cycle);
        // The tiles: texel 0's and, in two-cycle mode, texel 1's.
        let (tlut, tlut_ia) = (other >> 47 & 1 != 0, other >> 46 & 1 != 0);
        let bilinear = other >> 45 & 1 != 0;
        let tile_of = |index: usize| {
            let tile = rdp.tiles[index];
            let format = if tlut && tile.size < 2 {
                TexelFormat::Palette { ia: tlut_ia }
            } else {
                match (tile.format, tile.size) {
                    (0, 3) => TexelFormat::Rgba32,
                    (0, _) => TexelFormat::Rgba16,
                    (3, 2) => TexelFormat::Ia16,
                    (3, 1) => TexelFormat::Ia8,
                    (3, 0) => TexelFormat::Ia4,
                    (_, 0) => TexelFormat::I4,
                    (_, 1) => TexelFormat::I8,
                    _ => TexelFormat::Rgba16,
                }
            };
            (tile, format, Axis::new(&tile, 0), Axis::new(&tile, 1))
        };
        let tiles = [tile_of(prim.tile), tile_of((prim.tile + 1) & 7)];
        let samplers = [0, 1].map(|n| neon_sampler(tiles[n].1, bilinear));
        let (sample0, sample1) = (uses0 || !two_cycle && uses1, two_cycle && uses1);
        // Both texels through one sampler when the tiles share format and axes.
        let same = tiles[0].1 == tiles[1].1 && (tiles[0].2, tiles[0].3) == (tiles[1].2, tiles[1].3);
        let paired = (sample0 && sample1 && same).then(|| neon_pair_sampler(tiles[0].1, bilinear));
        let rgba16 = |tile: &Tile| u64::from(rdp.rgba16_sampling && tile.format == 0 && tile.size == 2);
        let samples = 1 + u64::from(two_cycle);
        let (three_each, sixteen_each) = (samples * u64::from(bilinear),
            rgba16(&tiles[0].0) + if two_cycle { rgba16(&tiles[1].0) } else { 0 });
        let table = &*PERSPECTIVE;
        let (shifts, top, zero) = (neon::rgba5551_shifts(), Lanes::splat(255), Lanes::splat(0));
        let (mut skipped, mut three, mut sixteen) = (0, 0, 0);
        let mut attrs = start;
        let mut shade_attrs = neon::Shade::new([R, R + 1, R + 2, R + 3].map(|c| start.v[c]),
            [R, R + 1, R + 2, R + 3].map(|c| dx.v[c]));
        for x in x0..x1 {
            let a = attrs;
            attrs.step(dx);
            let here = shade_attrs;
            shade_attrs.step();
            let z = if z_source_prim {
                prim_z
            } else {
                let sz = (a.v[Z] >> 13) as u32;
                match sz >> 17 & 3 {
                    0 | 1 => sz & 0x3ffff,
                    2 => 0x3ffff,
                    _ => 0,
                }
            };
            let i = y.wrapping_mul(image.width).wrapping_add(x) as usize;
            let z_at = z_image.wrapping_add(i.wrapping_mul(2)) & 0x7f_fffe;
            let compressed = z_compress(z);
            if z_compare {
                let stored = u16::from(rdram[z_at]) << 8 | u16::from(rdram[z_at + 1]);
                if !depth_passes(stored, z, compressed, prim.dz, z_mode) {
                    continue;
                }
            }
            let (texel0, texel1) = if prim.texture {
                let (s, t) = if prim.persp {
                    perspective_with(table, a.v[S] >> 16, a.v[T] >> 16, a.v[W] >> 16)
                } else {
                    ((a.v[S] >> 16) & 0x1ffff, (a.v[T] >> 16) & 0x1ffff)
                };
                (three, sixteen) = (three + three_each, sixteen + sixteen_each);
                if let Some(paired) = paired {
                    paired(tmem, &tiles[0], &tiles[1].0, s, t)
                } else {
                    let texel0 = if sample0 { samplers[0](tmem, &tiles[0], s, t) } else { zero };
                    let texel1 = if sample1 { samplers[1](tmem, &tiles[1], s, t) } else { texel0 };
                    (texel0, texel1)
                }
            } else {
                (zero, zero)
            };
            let shade = here.color();
            let combined = if first_only {
                neon::combine_one(&combiner[0], texel0, texel1, shade)
            } else if two_cycle {
                neon::combine_two(&combiner, texel0, texel1, shade)
            } else {
                neon::combine_one(&combiner[1], texel0, texel1, shade)
            };
            let mut alpha = combined.lane3() as i32;
            if cvg_times_alpha && alpha >> 5 == 0 {
                continue;
            }
            if alpha_cvg_sel && !cvg_times_alpha {
                alpha = 255;
            }
            if alpha_compare {
                let threshold =
                    if dither_alpha { noise.wrapping_add((x as i32).wrapping_mul(17)) & 0xff } else { threshold };
                if alpha < threshold {
                    continue;
                }
            }
            // The image's pixel, and its alpha as the blender sees it.
            let (memory, memory_alpha) = if !read_memory {
                skipped += 1;
                (zero, 0)
            } else if image.size == 3 {
                let at = base.wrapping_add(i.wrapping_mul(4)) & 0x7f_fffc;
                let p = u32::from_be_bytes(rdram[at..at + 4].try_into().unwrap());
                (neon::widen(p.swap_bytes()), (p & 0xe0) as i32)
            } else {
                let at = base.wrapping_add(i.wrapping_mul(2)) & 0x7f_fffe;
                let (first, second) = (rdram[at], rdram[at + 1]);
                (neon::widen(neon::rgba16(first, second)), if second & 1 != 0 { 0xe0 } else { 0 })
            };
            // The blender's cycles, as blend() has them. Every color it takes is 0-255 here (the
            // combiner's clamped, the first cycle's mix clamped), so its clamps change nothing.
            let cycle = |n: usize, pixel: Lanes, always: bool| -> Lanes {
                let (p_sel, a_sel, m_sel, b_sel) = blender[n];
                let pick = |sel: u64| match sel {
                    0 => pixel,
                    1 => memory,
                    2 => blend_color,
                    _ => fog,
                };
                if (p_sel, a_sel, m_sel, b_sel) == (0, 3, 0, 2) {
                    // The pixel by nothing plus itself by one: the pixel.
                    return pixel;
                }
                let (p, m) = (pick(p_sel), pick(m_sel));
                if !always && !force {
                    return p;
                }
                let a = match a_sel {
                    0 => alpha,
                    1 => fog_alpha,
                    2 => shade.lane3() as i32,
                    _ => 0,
                } & 0xff;
                let b = match b_sel {
                    0 => !a & 0xff,
                    1 => memory_alpha,
                    2 => 0xff,
                    _ => 0,
                };
                neon::mix(p, m, Lanes::pair((a >> 3) as u32 | (((b >> 3) + 1) as u32) << 16, 0), top)
            };
            let color = if two_cycle { cycle(1, cycle(0, combined, true), false) } else { cycle(0, combined, false) };
            match image.size {
                3 => {
                    let at = base.wrapping_add(i.wrapping_mul(4)) & 0x7f_fffc;
                    let (rg, ba) = color.halves();
                    let word = (rg & 0xff) << 24 | (rg >> 16 & 0xff) << 16 | (ba & 0xff) << 8 | 0xe0;
                    rdram[at..at + 4].copy_from_slice(&word.to_be_bytes());
                }
                2 => {
                    let at = base.wrapping_add(i.wrapping_mul(2)) & 0x7f_fffe;
                    // Safety: at is below RDRAM's last byte.
                    unsafe { neon::store_rgba5551(color, shifts, rdram.as_mut_ptr().add(at)) };
                }
                _ => {}
            }
            if z_update {
                let v = compressed << 2 | stored_dz;
                (rdram[z_at], rdram[z_at + 1]) = ((v >> 8) as u8, v as u8);
            }
        }
        if rdp.profiling {
            rdp.three_texel_samples += three;
            rdp.rgba16_samples += sixteen;
            rdp.skipped_color_reads += skipped;
            rdp.fast_span_pixels += u64::from(x1 - x0);
        }
        true
    }

    /// span_sampled for the kinds of span drawn most (nearly all of SM64's
    /// 3D, most of Mario Kart's): no texture, or perspective, bilinear
    /// RGBA16, IA16 or IA8 texels (no palette), and either of two blends.
    /// The first: 
    /// a 16-bit color image that the blender only writes (no memory input,
    /// and no force blend but the one that gives the combined color back:
    /// a 0, m the combined color, b 1). No alpha reaches the image then, so
    /// none is worked out. The second (BLEND): the combined color over the
    /// image's by the combined alpha (a it, m memory, b 1 - a). Both with
    /// no alpha compare or coverage times alpha, the depth from the
    /// attributes; each texel format, blend and pair of depth modes has its
    /// own loop. False where span_sampled must draw.
    #[allow(clippy::too_many_arguments)]
    fn span_opaque(&mut self, y: u32, x0: u32, x1: u32, start: Attrs, dx: &Attrs, prim: &Primitive,
        read_memory: bool) -> bool {
        let rdp = &self.rdp;
        let other = rdp.other_modes;
        let tile = &rdp.tiles[prim.tile];
        let (Some(plan), true) = (rdp.combine_program[1], rdp.fast_combiner) else { return false };
        // As texel decodes them: through the palette, when it is on, any of
        // 4 or 8 bits.
        let (tlut, rgba_palette) = (other >> 47 & 1 != 0, other >> 46 & 1 == 0);
        let texels = match (tile.format, tile.size) {
            _ if !prim.texture => None,
            (0, 2) => Some(Texels::Rgba16),
            (3, 2) => Some(Texels::Ia16),
            (_, 1) if tlut && rgba_palette => Some(Texels::Ci8),
            (_, 0) if tlut && rgba_palette => Some(Texels::Ci4),
            (3, 1) if !tlut => Some(Texels::Ia8),
            (f, 1) if !tlut && f != 0 => Some(Texels::I8),
            (f, 0) if !tlut && f != 0 && f != 3 => Some(Texels::I4),
            _ => return false,
        };
        let force = other >> 14 & 1 != 0;
        let identity = other >> 26 & 3 == 3 && other >> 22 & 3 == 0 && other >> 18 & 3 == 2;
        let blend = force && other >> 26 & 3 == 0 && other >> 22 & 3 == 1 && other >> 18 & 3 == 0;
        let fits = (!prim.texture || prim.persp && other >> 45 & 1 != 0) && (!read_memory || blend)
            && rdp.color_image.size == 2
            && other & 1 == 0 && other >> 2 & 1 == 0 && other >> 12 & 1 == 0 && (!force || identity || blend)
            && other >> 30 & 3 == 0
            && rdp.tmem.len() == 0x1000 && self.rdram.len() == 0x80_0000;
        if !fits {
            return false;
        }
        // With NEON, its loop for the colors.
        #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
        let neon = crate::raster_neon::available();
        macro_rules! with_loop {
            ($z:literal, $u:literal, $t:literal, $b:literal) => {{
                #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
                if neon {
                    self.span_opaque_neon::<$z, $u, $t, $b>(y, x0, x1, start, dx, prim, plan);
                    return true;
                }
                self.span_opaque_with::<$z, $u, $t, $b>(y, x0, x1, start, dx, prim, plan)
            }};
        }
        macro_rules! with_depth {
            ($t:literal, $b:literal) => {
                match (other >> 4 & 1 != 0, other >> 5 & 1 != 0) {
                    (false, false) => with_loop!(false, false, $t, $b),
                    (false, true) => with_loop!(false, true, $t, $b),
                    (true, false) => with_loop!(true, false, $t, $b),
                    (true, true) => with_loop!(true, true, $t, $b),
                }
            };
        }
        macro_rules! with_texels {
            ($b:literal) => {
                match texels {
                    Some(Texels::Rgba16) => with_depth!(0, $b),
                    Some(Texels::Ia16) => with_depth!(1, $b),
                    Some(Texels::Ia8) => with_depth!(2, $b),
                    Some(Texels::I4) => with_depth!(4, $b),
                    Some(Texels::I8) => with_depth!(5, $b),
                    Some(Texels::Ci8) => with_depth!(6, $b),
                    Some(Texels::Ci4) => with_depth!(7, $b),
                    None => with_depth!(3, $b),
                }
            };
        }
        if blend {
            with_texels!(true);
        } else {
            with_texels!(false);
        }
        true
    }

    /// span_opaque's loop: span_sampled's steps for its case (colors only
    /// unless BLEND); TEXELS is RGBA16 (0), IA16 (1), IA8 (2), no texture
    /// (3), I4 (4), I8 (5), or CI8 (6) and CI4 (7) through an RGBA16
    /// palette.
    #[allow(clippy::too_many_arguments)]
    fn span_opaque_with<const Z_COMPARE: bool, const Z_UPDATE: bool, const TEXELS: u8, const BLEND: bool>(
        &mut self, y: u32, x0: u32, x1: u32, start: Attrs, dx: &Attrs, prim: &Primitive, plan: CombinePlan) {
        let Raster { rdp, rdram, .. } = self;
        let tmem = <&[u8; 0x1000]>::try_from(rdp.tmem.as_slice()).unwrap();
        let rdram = <&mut [u8; 0x80_0000]>::try_from(&mut **rdram).unwrap();
        let other = rdp.other_modes;
        let tile = rdp.tiles[prim.tile];
        let z_mode = other >> 10 & 3;
        let stored_dz = (dz_compress(prim.dz) >> 2) as u16;
        let image = rdp.color_image;
        let (base, z_image) = (image.addr as usize, rdp.z_image as usize);
        let (axis_s, axis_t) = (Axis::new(&tile, 0), Axis::new(&tile, 1));
        let counted = rdp.rgba16_sampling && TEXELS == 0;
        let (prim_color, env) = (rgba32(rdp.prim), rgba32(rdp.env));
        let table = &*PERSPECTIVE;
        let (tmem_base, line) = (tile.tmem, tile.line);
        let alpha_cvg_sel = other >> 13 & 1 != 0;
        // A texel: (s, t) are wrapped texels. Its alpha is used only with
        // BLEND, and is no work otherwise.
        let palette = tile.palette;
        let fetch = |s: i32, t: i32| -> Color {
            let (s, t) = (s as u32, t as u32);
            let (odd, row) = ((t & 1) << 2, tmem_base.wrapping_add(t.wrapping_mul(line)));
            // A palette entry (each is there four times over in high TMEM).
            let entry = |index: u32| {
                let at = 0x800 + (index as usize & 0xff) * 8;
                rgba16(u32::from(tmem[at]) << 8 | u32::from(tmem[at + 1]))
            };
            if matches!(TEXELS, 4 | 7) {
                let byte = u32::from(tmem[(row.wrapping_add(s >> 1) ^ odd) as usize & 0xfff]);
                let nibble = if s & 1 == 0 { byte >> 4 } else { byte & 15 };
                if TEXELS == 7 {
                    return entry(palette << 4 | nibble);
                }
                return [nibble as i32 * 17; 4];
            }
            if matches!(TEXELS, 5 | 6) {
                let byte = u32::from(tmem[(row.wrapping_add(s) ^ odd) as usize & 0xfff]);
                if TEXELS == 6 {
                    return entry(byte);
                }
                return [byte as i32; 4];
            }
            if TEXELS == 2 {
                let v = i32::from(tmem[(row.wrapping_add(s) ^ odd) as usize & 0xfff]);
                let (i, a) = ((v >> 4) * 17, (v & 15) * 17);
                return [i, i, i, a];
            }
            let at = (row.wrapping_add(s.wrapping_mul(2)) ^ odd) as usize & 0xfff;
            if TEXELS == 1 {
                let (i, a) = (i32::from(tmem[at]), i32::from(tmem[(at + 1) & 0xfff]));
                return [i, i, i, a];
            }
            rgba16(u32::from(tmem[at]) << 8 | u32::from(tmem[(at + 1) & 0xfff]))
        };
        let mut drawn = 0;
        let mut attrs = start;
        for x in x0..x1 {
            let a = attrs;
            attrs.step(dx);
            let sz = (a.v[Z] >> 13) as u32;
            let z = match sz >> 17 & 3 {
                0 | 1 => sz & 0x3ffff,
                2 => 0x3ffff,
                _ => 0,
            };
            let i = y.wrapping_mul(image.width).wrapping_add(x) as usize;
            let z_at = z_image.wrapping_add(i.wrapping_mul(2)) & 0x7f_fffe;
            let compressed = z_compress(z);
            if Z_COMPARE {
                let stored = u16::from(rdram[z_at]) << 8 | u16::from(rdram[z_at + 1]);
                if !depth_passes(stored, z, compressed, prim.dz, z_mode) {
                    continue;
                }
            }
            drawn += 1;
            let texel: Color = if TEXELS == 3 {
                [0; 4]
            } else {
                let (s, t) = perspective_with(table, a.v[S] >> 16, a.v[T] >> 16, a.v[W] >> 16);
                let (s, sfrac) = axis_s.coordinate(s);
                let (t, tfrac) = axis_t.coordinate(t);
                let (s0, s1) = axis_s.wrap(s);
                let (t0, t1) = axis_t.wrap(t);
                // Only the three texels on the fractions' side count.
                let lower = sfrac + tfrac < 32;
                let (sa, ta, sb, tb, sc, tc) = if lower { (s0, t0, s1, t0, s0, t1) } else { (s1, t1, s0, t1, s1, t0) };
                let (fs, ft) = if lower { (sfrac, tfrac) } else { (32 - sfrac, 32 - tfrac) };
                let (corner, along_s, along_t) = (fetch(sa, ta), fetch(sb, tb), fetch(sc, tc));
                std::array::from_fn(|c| {
                    corner[c] + ((fs * (along_s[c] - corner[c]) + ft * (along_t[c] - corner[c]) + 0x10) >> 5)
                })
            };
            let shade: Color = std::array::from_fn(|c| clamp9(a.v[R + c] >> 16));
            let at = base.wrapping_add(i.wrapping_mul(2)) & 0x7f_fffe;
            let [r, g, b] = if BLEND {
                let mut combined = combine_plan(plan, prim_color, env, [0; 4], texel, texel, shade);
                if alpha_cvg_sel {
                    combined[3] = 255;
                }
                let p = u32::from(rdram[at]) << 8 | u32::from(rdram[at + 1]);
                let memory = [expand5(p >> 11 & 31), expand5(p >> 6 & 31), expand5(p >> 1 & 31)];
                let a = combined[3] & 0xff;
                let (a5, b5) = (a >> 3, ((!a & 0xff) >> 3) + 1);
                std::array::from_fn(|c| ((combined[c] * a5 + memory[c] * b5) >> 5).clamp(0, 255) as u32)
            } else {
                // combine_plan's channels are 0-255 already: as the image
                // takes them.
                let rgb = |c: Color| [c[0], c[1], c[2]];
                combine_rgb(plan, prim_color, env, rgb(texel), rgb(shade)).map(|c| c as u32)
            };
            let v = (r >> 3) << 11 | (g >> 3) << 6 | (b >> 3) << 1 | 1;
            (rdram[at], rdram[at + 1]) = ((v >> 8) as u8, v as u8);
            if Z_UPDATE {
                let v = compressed << 2 | stored_dz;
                (rdram[z_at], rdram[z_at + 1]) = ((v >> 8) as u8, v as u8);
            }
        }
        if rdp.profiling {
            if TEXELS != 3 {
                rdp.three_texel_samples += drawn;
            }
            if counted {
                rdp.rgba16_samples += drawn;
            }
            if !BLEND {
                rdp.skipped_color_reads += drawn;
            }
            rdp.fast_span_pixels += u64::from(x1 - x0);
        }
    }

    /// span_opaque_with with NEON: the same depth, perspective and tile axes, then the texels packed
    /// (raster_neon's tables), the filter, shade, combiner, blend and RGBA5551 store on four lanes.
    #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
    #[allow(clippy::too_many_arguments)]
    fn span_opaque_neon<const Z_COMPARE: bool, const Z_UPDATE: bool, const TEXELS: u8, const BLEND: bool>(
        &mut self, y: u32, x0: u32, x1: u32, start: Attrs, dx: &Attrs, prim: &Primitive, plan: CombinePlan) {
        use crate::raster_neon::{self as neon, Lanes};
        let Raster { rdp, rdram, .. } = self;
        let tmem = <&[u8; 0x1000]>::try_from(rdp.tmem.as_slice()).unwrap();
        let rdram = <&mut [u8; 0x80_0000]>::try_from(&mut **rdram).unwrap();
        let other = rdp.other_modes;
        let tile = rdp.tiles[prim.tile];
        let z_mode = other >> 10 & 3;
        let stored_dz = (dz_compress(prim.dz) >> 2) as u16;
        let image = rdp.color_image;
        let (base, z_image) = (image.addr as usize, rdp.z_image as usize);
        let (axis_s, axis_t) = (Axis::new(&tile, 0), Axis::new(&tile, 1));
        let counted = rdp.rgba16_sampling && TEXELS == 0;
        let (prim_color, env) = (rgba32(rdp.prim), rgba32(rdp.env));
        let table = &*PERSPECTIVE;
        let (tmem_base, line) = (tile.tmem, tile.line);
        let alpha_cvg_sel = other >> 13 & 1 != 0;
        // The filter's three texels packed (s and t wrapped), as span_opaque_with's fetch decodes them,
        // each load for all three before any is used (raster_neon::together). With the tile's TMEM
        // address and line even, a 16-bit texel's bytes are a halfword.
        let palette = tile.palette;
        let even = tmem_base & 1 == 0 && line & 1 == 0;
        let fetch3 = |st: [(i32, i32); 3]| -> [u32; 3] {
            // The texel's byte in TMEM (the first of two, or the one with its nibble).
            let at = st.map(|(s, t)| {
                let (s, t) = (s as u32, t as u32);
                let offset = match TEXELS {
                    4 | 7 => s >> 1,
                    2 | 5 | 6 => s,
                    _ => s.wrapping_mul(2),
                };
                (tmem_base.wrapping_add(t.wrapping_mul(line)).wrapping_add(offset) ^ (t & 1) << 2) as usize & 0xfff
            });
            // Palette entries (each is there four times over in high TMEM, at even addresses).
            let entries = |index: [u32; 3]| {
                let at = index.map(|index| 0x800 + (index as usize & 0xff) * 8);
                neon::rgba16_halves(neon::together(at.map(|at| u32::from(tmem[at]) | u32::from(tmem[at + 1]) << 8)))
            };
            if matches!(TEXELS, 0 | 1) && even {
                let half = neon::together(at.map(|at| {
                    let at = at & 0xffe;
                    u32::from(tmem[at]) | u32::from(tmem[at + 1]) << 8
                }));
                return if TEXELS == 1 {
                    half.map(|half| neon::ia16(half as u8, (half >> 8) as u8))
                } else {
                    neon::rgba16_halves(half)
                };
            }
            let first = neon::together(at.map(|at| u32::from(tmem[at])));
            match TEXELS {
                4 | 7 => {
                    let nibble = [0, 1, 2].map(|k| if st[k].0 & 1 == 0 { first[k] >> 4 } else { first[k] & 15 });
                    if TEXELS == 7 {
                        entries(nibble.map(|nibble| palette << 4 | nibble))
                    } else {
                        // The nibble times 17 in each byte.
                        nibble.map(|nibble| nibble * 0x1111_1111)
                    }
                }
                5 => first.map(|byte| byte * 0x0101_0101),
                6 => entries(first),
                2 => first.map(|byte| neon::IA8[byte as usize]),
                _ => {
                    let second = at.map(|at| tmem[(at + 1) & 0xfff]);
                    if TEXELS == 1 {
                        [0, 1, 2].map(|k| neon::ia16(first[k] as u8, second[k]))
                    } else {
                        [0, 1, 2].map(|k| neon::rgba16(first[k] as u8, second[k]))
                    }
                }
            }
        };
        let lanes = |c: Color| Lanes::pair(c[0] as u32 | (c[1] as u32) << 16, c[2] as u32 | (c[3] as u32) << 16);
        let prim_lanes = lanes(prim_color);
        // combine_rgb's direct source when it is fixed for the span (clamp9 of 256 is 255).
        let fixed = lanes(match plan.rgb_source {
            3 => prim_color,
            5 => env,
            6 => [255; 4],
            _ => [0; 4],
        });
        let (shifts, top) = (neon::rgba5551_shifts(), Lanes::splat(255));
        // Whether the combiner takes the texel at all (a direct source may be the shade or a color):
        // sampling has no other effect, so a texel nothing takes is not fetched.
        let uses_texel = TEXELS != 3 && (plan.rgb != 0 || matches!(plan.rgb_source, 1 | 2)
            || BLEND && !alpha_cvg_sel && (plan.alpha != 0 || matches!(plan.alpha_source, 1 | 2)));
        let mut drawn = 0;
        let mut attrs = start;
        let mut shade_attrs = neon::Shade::new([R, R + 1, R + 2, R + 3].map(|c| start.v[c]),
            [R, R + 1, R + 2, R + 3].map(|c| dx.v[c]));
        for x in x0..x1 {
            let a = attrs;
            attrs.step(dx);
            let here = shade_attrs;
            shade_attrs.step();
            let sz = (a.v[Z] >> 13) as u32;
            let z = match sz >> 17 & 3 {
                0 | 1 => sz & 0x3ffff,
                2 => 0x3ffff,
                _ => 0,
            };
            let i = y.wrapping_mul(image.width).wrapping_add(x) as usize;
            let z_at = z_image.wrapping_add(i.wrapping_mul(2)) & 0x7f_fffe;
            let compressed = z_compress(z);
            if Z_COMPARE {
                let stored = u16::from(rdram[z_at]) << 8 | u16::from(rdram[z_at + 1]);
                if !depth_passes(stored, z, compressed, prim.dz, z_mode) {
                    continue;
                }
            }
            drawn += 1;
            let at = base.wrapping_add(i.wrapping_mul(2)) & 0x7f_fffe;
            let texel = if !uses_texel {
                Lanes::splat(0)
            } else {
                let (s, t) = perspective_with(table, a.v[S] >> 16, a.v[T] >> 16, a.v[W] >> 16);
                let (s, sfrac) = axis_s.coordinate(s);
                let (t, tfrac) = axis_t.coordinate(t);
                let (s0, s1) = axis_s.wrap(s);
                let (t0, t1) = axis_t.wrap(t);
                // Only the three texels on the fractions' side count.
                let lower = sfrac + tfrac < 32;
                let (sa, ta, sb, tb, sc, tc) = if lower { (s0, t0, s1, t0, s0, t1) } else { (s1, t1, s0, t1, s1, t0) };
                let (fs, ft) = if lower { (sfrac, tfrac) } else { (32 - sfrac, 32 - tfrac) };
                let weights = Lanes::pair((32 - fs - ft) as u32 | (fs as u32) << 16, ft as u32);
                let [corner, along_s, along_t] = fetch3([(sa, ta), (sb, tb), (sc, tc)]);
                neon::filter3(corner, along_s, along_t, weights)
            };
            let shade = here.color();
            let depth = compressed << 2 | stored_dz;
            // combine_rgb's formulas (and, for the blend, combine_plan's alpha): every input is 0-255, so
            // their clamp9 changes nothing but the direct 256.
            let color = match plan.rgb {
                0 => match plan.rgb_source {
                    1 | 2 => texel,
                    4 => shade,
                    _ => fixed,
                },
                1 | 2 => neon::mul_round(texel, shade),
                5 | 6 => neon::mul_round(texel, prim_lanes),
                _ => neon::lerp(prim_lanes, shade, texel),
            };
            let color = if BLEND {
                let alpha = if alpha_cvg_sel {
                    255
                } else {
                    let (texel, shade) = (texel.lane3() as i32, shade.lane3() as i32);
                    match plan.alpha {
                        0 => match plan.alpha_source {
                            1 | 2 => texel,
                            3 => prim_color[3],
                            4 => shade,
                            5 => env[3],
                            6 => 255,
                            _ => 0,
                        },
                        1 | 2 => (texel * shade + 0x80) >> 8,
                        5 | 6 => (texel * prim_color[3] + 0x80) >> 8,
                        _ => ((prim_color[3] - shade) * texel + (shade << 8) + 0x80) >> 8,
                    }
                };
                let (a5, b5) = (alpha as u32 >> 3, ((!alpha & 0xff) as u32 >> 3) + 1);
                let memory = neon::widen(neon::rgba16(rdram[at], rdram[at + 1]));
                neon::mix(color, memory, Lanes::pair(a5 | b5 << 16, 0), top)
            } else {
                color
            };
            // Safety: at is below RDRAM's last byte.
            unsafe { neon::store_rgba5551(color, shifts, rdram.as_mut_ptr().add(at)) };
            if Z_UPDATE {
                (rdram[z_at], rdram[z_at + 1]) = ((depth >> 8) as u8, depth as u8);
            }
        }
        if rdp.profiling {
            if TEXELS != 3 {
                rdp.three_texel_samples += drawn;
            }
            if counted {
                rdp.rgba16_samples += drawn;
            }
            if !BLEND {
                rdp.skipped_color_reads += drawn;
            }
            rdp.fast_span_pixels += u64::from(x1 - x0);
        }
    }

    /// span_one_cycle for an untextured span or a tile of the formats
    /// most drawn (RGBA16, IA16, IA8 without a palette): the tile's axes
    /// and texel layout worked out once, TMEM and RDRAM reached without
    /// bounds checks (their sizes are fixed), the three texels' corner
    /// picked without a branch. False where span_one_cycle must draw it.
    #[allow(clippy::too_many_arguments)]
    fn span_sampled(&mut self, y: u32, x0: u32, x1: u32, start: Attrs, dx: &Attrs, prim: &Primitive,
        read_memory: bool) -> bool {
        let cycles = self.cycles;
        let Raster { rdp, rdram, .. } = self;
        let (Some(plan), true) = (rdp.combine_program[1], rdp.fast_combiner) else { return false };
        let other = rdp.other_modes;
        let tile = rdp.tiles[prim.tile];
        let texels = match (tile.format, tile.size) {
            _ if !prim.texture => Texels::Rgba16,
            (0, 2) => Texels::Rgba16,
            (3, 2) => Texels::Ia16,
            (3, 1) if other >> 47 & 1 == 0 => Texels::Ia8,
            _ => return false,
        };
        let (Ok(tmem), Ok(rdram)) =
            (<&[u8; 0x1000]>::try_from(rdp.tmem.as_slice()), <&mut [u8; 0x80_0000]>::try_from(&mut **rdram))
        else {
            return false;
        };
        let z_source_prim = other >> 2 & 1 != 0;
        let prim_z = rdp.prim_z << 3 & 0x3ffff;
        let (z_compare, z_update, z_mode) = (other >> 4 & 1 != 0, other >> 5 & 1 != 0, other >> 10 & 3);
        let stored_dz = (dz_compress(prim.dz) >> 2) as u16;
        let (cvg_times_alpha, alpha_cvg_sel) = (other >> 12 & 1 != 0, other >> 13 & 1 != 0);
        let (alpha_compare, dither_alpha) = (other & 1 != 0, other >> 1 & 1 != 0);
        let threshold = (rdp.blend & 0xff) as i32;
        let noise = (cycles as i32).wrapping_mul(31);
        let image = rdp.color_image;
        let (base, z_image) = (image.addr as usize, rdp.z_image as usize);
        let (axis_s, axis_t) = (Axis::new(&tile, 0), Axis::new(&tile, 1));
        let bilinear = other >> 45 & 1 != 0;
        let counted = rdp.rgba16_sampling && tile.format == 0 && tile.size == 2;
        let (prim_color, env) = (rgba32(rdp.prim), rgba32(rdp.env));
        let (blend_color, fog) = (rgba32(rdp.blend), rgba32(rdp.fog));
        let force = other >> 14 & 1 != 0;
        let (p_sel, a_sel, m_sel, b_sel) = (other >> 30 & 3, other >> 26 & 3, other >> 22 & 3, other >> 18 & 3);
        let table = &*PERSPECTIVE;
        // A texel of the tile, by the format: (s, t) are wrapped texels.
        let (tmem_base, line) = (tile.tmem, tile.line);
        let fetch = |s: i32, t: i32| -> Color {
            let (s, t) = (s as u32, t as u32);
            let odd = (t & 1) << 2;
            let row = tmem_base.wrapping_add(t.wrapping_mul(line));
            match texels {
                Texels::Ia8 => {
                    let v = i32::from(tmem[(row.wrapping_add(s) ^ odd) as usize & 0xfff]);
                    let (i, a) = ((v >> 4) * 17, (v & 15) * 17);
                    [i, i, i, a]
                }
                _ => {
                    let at = (row.wrapping_add(s.wrapping_mul(2)) ^ odd) as usize & 0xfff;
                    let v = u32::from(tmem[at]) << 8 | u32::from(tmem[(at + 1) & 0xfff]);
                    if texels == Texels::Ia16 {
                        let (i, a) = ((v >> 8) as i32, (v & 0xff) as i32);
                        [i, i, i, a]
                    } else {
                        rgba16(v)
                    }
                }
            }
        };
        let (mut three, mut sixteen, mut skipped) = (0, 0, 0);
        let mut attrs = start;
        for x in x0..x1 {
            let a = attrs;
            attrs.step(dx);
            let z = if z_source_prim {
                prim_z
            } else {
                let sz = (a.v[Z] >> 13) as u32;
                match sz >> 17 & 3 {
                    0 | 1 => sz & 0x3ffff,
                    2 => 0x3ffff,
                    _ => 0,
                }
            };
            let i = y.wrapping_mul(image.width).wrapping_add(x) as usize;
            let z_at = z_image.wrapping_add(i.wrapping_mul(2)) & 0x7f_fffe;
            let compressed = z_compress(z);
            if z_compare {
                let stored = u16::from(rdram[z_at]) << 8 | u16::from(rdram[z_at + 1]);
                if !depth_passes(stored, z, compressed, prim.dz, z_mode) {
                    continue;
                }
            }
            let texel = if prim.texture {
                let (s, t) = if prim.persp {
                    perspective_with(table, a.v[S] >> 16, a.v[T] >> 16, a.v[W] >> 16)
                } else {
                    ((a.v[S] >> 16) & 0x1ffff, (a.v[T] >> 16) & 0x1ffff)
                };
                three += u64::from(bilinear);
                sixteen += u64::from(counted);
                let (s, sfrac) = axis_s.coordinate(s);
                let (t, tfrac) = axis_t.coordinate(t);
                let (s0, s1) = axis_s.wrap(s);
                let (t0, t1) = axis_t.wrap(t);
                if bilinear {
                    // Only the three texels on the fractions' side count.
                    let lower = sfrac + tfrac < 32;
                    let (sa, ta, sb, tb, sc, tc) = if lower { (s0, t0, s1, t0, s0, t1) } else { (s1, t1, s0, t1, s1, t0) };
                    let (fs, ft) = if lower { (sfrac, tfrac) } else { (32 - sfrac, 32 - tfrac) };
                    let (corner, along_s, along_t) = (fetch(sa, ta), fetch(sb, tb), fetch(sc, tc));
                    std::array::from_fn(|c| {
                        corner[c] + ((fs * (along_s[c] - corner[c]) + ft * (along_t[c] - corner[c]) + 0x10) >> 5)
                    })
                } else {
                    fetch(s0, t0)
                }
            } else {
                [0; 4]
            };
            let shade: Color = std::array::from_fn(|c| clamp9(a.v[R + c] >> 16));
            let mut combined = combine_plan(plan, prim_color, env, [0; 4], texel, texel, shade);
            if cvg_times_alpha && combined[3] >> 5 == 0 {
                continue;
            }
            if alpha_cvg_sel && !cvg_times_alpha {
                combined[3] = 255;
            }
            if alpha_compare {
                let threshold =
                    if dither_alpha { noise.wrapping_add((x as i32).wrapping_mul(17)) & 0xff } else { threshold };
                if combined[3] < threshold {
                    continue;
                }
            }
            let memory = if !read_memory {
                skipped += 1;
                [0; 4]
            } else if image.size == 3 {
                let at = base.wrapping_add(i.wrapping_mul(4)) & 0x7f_fffc;
                let p = u32::from_be_bytes(rdram[at..at + 4].try_into().unwrap());
                [(p >> 24) as i32, (p >> 16 & 0xff) as i32, (p >> 8 & 0xff) as i32, (p & 0xe0) as i32]
            } else {
                let at = base.wrapping_add(i.wrapping_mul(2)) & 0x7f_fffe;
                let p = u32::from(rdram[at]) << 8 | u32::from(rdram[at + 1]);
                [expand5(p >> 11 & 31), expand5(p >> 6 & 31), expand5(p >> 1 & 31), if p & 1 != 0 { 0xe0 } else { 0 }]
            };
            // The blender's only cycle, blending only with force_blend.
            let pick = |sel: u64| match sel {
                0 => combined,
                1 => memory,
                2 => blend_color,
                _ => fog,
            };
            let p = pick(p_sel);
            let [r, g, b] = if force {
                let m = pick(m_sel);
                let a = match a_sel {
                    0 => combined[3],
                    1 => fog[3],
                    2 => shade[3],
                    _ => 0,
                } & 0xff;
                let b = match b_sel {
                    0 => !a & 0xff,
                    1 => memory[3],
                    2 => 0xff,
                    _ => 0,
                };
                let (a5, b5) = (a >> 3, (b >> 3) + 1);
                std::array::from_fn(|c| ((p[c] * a5 + m[c] * b5) >> 5).clamp(0, 255) as u32)
            } else {
                std::array::from_fn(|c| p[c].clamp(0, 255) as u32)
            };
            match image.size {
                3 => {
                    let at = base.wrapping_add(i.wrapping_mul(4)) & 0x7f_fffc;
                    rdram[at..at + 4].copy_from_slice(&(r << 24 | g << 16 | b << 8 | 0xe0).to_be_bytes());
                }
                2 => {
                    let at = base.wrapping_add(i.wrapping_mul(2)) & 0x7f_fffe;
                    let v = (r >> 3) << 11 | (g >> 3) << 6 | (b >> 3) << 1 | 1;
                    (rdram[at], rdram[at + 1]) = ((v >> 8) as u8, v as u8);
                }
                _ => {}
            }
            if z_update {
                let v = compressed << 2 | stored_dz;
                (rdram[z_at], rdram[z_at + 1]) = ((v >> 8) as u8, v as u8);
            }
        }
        if rdp.profiling {
            rdp.three_texel_samples += three;
            rdp.rgba16_samples += sixteen;
            rdp.skipped_color_reads += skipped;
            rdp.fast_span_pixels += u64::from(x1 - x0);
        }
        true
    }

    /// sample_three's texel for a tile already copied, without counting.
    #[inline(always)]
    fn sample_tile<const RGBA16: bool>(&self, tile: &Tile, bilinear: bool, s: i32, t: i32) -> Color {
        let (s, sfrac, t, tfrac) = self.texture_coordinates(tile, s, t);
        let (s0, s1) = self.wrap(tile, 0, s);
        let (t0, t1) = self.wrap(tile, 1, t);
        if !bilinear {
            return self.sample_texel::<RGBA16>(tile, s0, t0);
        }
        let (base, along_s, along_t, fs, ft) = if sfrac + tfrac < 32 {
            (self.sample_texel::<RGBA16>(tile, s0, t0), self.sample_texel::<RGBA16>(tile, s1, t0),
                self.sample_texel::<RGBA16>(tile, s0, t1), sfrac, tfrac)
        } else {
            (self.sample_texel::<RGBA16>(tile, s1, t1), self.sample_texel::<RGBA16>(tile, s0, t1),
                self.sample_texel::<RGBA16>(tile, s1, t0), 32 - sfrac, 32 - tfrac)
        };
        std::array::from_fn(|c| {
            base[c] + ((fs * (along_s[c] - base[c]) + ft * (along_t[c] - base[c]) + 0x10) >> 5)
        })
    }
}

/// How the specialized span decodes a tile's texels.
#[derive(Clone, Copy, PartialEq)]
enum Texels {
    Rgba16,
    Ia16,
    Ia8,
    /// 4 and 8 bits of intensity (I4, and CI4 with no palette; I8, CI8).
    I4,
    I8,
    /// 8 and 4 bits through an RGBA16 palette.
    Ci8,
    Ci4,
}

/// A tile's axis as texture_coordinates and wrap use it, worked out once
/// for a span.
#[derive(Clone, Copy, PartialEq)]
struct Axis {
    /// The shift, right or from 11 on left by 16 - shift into an i16, as two shifts of the coordinate's
    /// word: up, then down (arithmetic).
    up: u32,
    down: u32,
    lo: i32,
    hi: i32,
    clamp: bool,
    clamp_max: i32,
    masked: bool,
    mask: u32,
    bits: i32,
    mirror: bool,
}

impl Axis {
    fn new(tile: &Tile, n: usize) -> Axis {
        let mask = tile.mask[n];
        // Right: the coordinate's 16 bits to the top, then down 16 + shift. Left by 16 - shift: up
        // 16 + (16 - shift), then down 16, which keeps an i16's bits.
        let shift = tile.shift[n];
        let (up, down) = if shift >= 11 { (32 - shift, 16) } else { (16, 16 + shift) };
        Axis {
            up,
            down,
            lo: (tile.lo[n] << 3) as i32,
            hi: tile.hi[n] as i32,
            clamp: tile.clamp[n] || mask == 0,
            clamp_max: ((tile.hi[n] >> 2).wrapping_sub(tile.lo[n] >> 2) & 0x3ff) as i32,
            masked: mask != 0,
            mask: mask.min(10),
            bits: (1 << mask.min(10)) - 1,
            mirror: tile.mirror[n],
        }
    }

    /// The coordinate's 16 bits shifted as the tile says (texture_coordinates' i16 and shift).
    #[inline(always)]
    fn shifted(&self, coord: i32) -> i32 {
        (coord << self.up) >> self.down
    }

    /// The texel and its 5-bit fraction, as texture_coordinates has them.
    #[inline(always)]
    fn coordinate(&self, coord: i32) -> (i32, i32) {
        let overflow = coord >> 17 & 3;
        let c = self.shifted(coord);
        let max = (c >> 3) >= self.hi || overflow == 2;
        let c = c - self.lo;
        if !self.clamp {
            (c >> 5, c & 0x1f)
        } else if max {
            (self.clamp_max, 0)
        } else if c & 0x10000 != 0 || c < 0 || overflow == 1 {
            (0, 0)
        } else {
            (c >> 5, c & 0x1f)
        }
    }

    /// The texel and the next, masked and mirrored, as wrap has them.
    #[inline(always)]
    fn wrap(&self, c: i32) -> (i32, i32) {
        if !self.masked {
            return (c, c + 1);
        }
        let one = |c: i32| if self.mirror && (c >> self.mask) & 1 != 0 { !c & self.bits } else { c & self.bits };
        (one(c), one(c + 1))
    }
}

/// How span_cycles decodes a tile's texels: texel()'s cases, worked out once for the span.
#[derive(Clone, Copy, PartialEq)]
enum TexelFormat {
    /// 4 or 8 bits through the palette, its entries RGBA16 or IA16.
    Palette { ia: bool },
    Rgba32,
    Rgba16,
    Ia16,
    Ia8,
    Ia4,
    I4,
    I8,
}

/// A texel of a tile as texel() decodes it, its format worked out (s and t wrapped).
#[inline(always)]
fn decode_texel(tmem: &[u8; 0x1000], tile: &Tile, format: TexelFormat, s: i32, t: i32) -> Color {
    let (su, tu) = (s as u32, t as u32);
    let offset = match tile.size {
        0 => su >> 1,
        1 => su,
        _ => su.wrapping_mul(2),
    };
    let address = tile.tmem.wrapping_add(tu.wrapping_mul(tile.line)).wrapping_add(offset) ^ (tu & 1) << 2;
    let at = address as usize & if tile.size == 3 { 0x7ff } else { 0xfff };
    let half = || u32::from(tmem[at]) << 8 | u32::from(tmem[(at + 1) & 0xfff]);
    let index = || {
        let byte = u32::from(tmem[at]);
        if tile.size == 0 { tile.palette << 4 | if su & 1 == 0 { byte >> 4 } else { byte & 15 } } else { byte }
    };
    match format {
        TexelFormat::Palette { ia } => {
            let entry_at = 0x800 + (index() as usize & 0xff) * 8;
            let entry = u32::from(tmem[entry_at]) << 8 | u32::from(tmem[entry_at + 1]);
            if ia {
                let (i, a) = ((entry >> 8) as i32, (entry & 0xff) as i32);
                [i, i, i, a]
            } else {
                rgba16(entry)
            }
        }
        TexelFormat::Rgba32 => {
            let (r, g) = (tmem[at], tmem[(at + 1) & 0x7ff]);
            let (b, a) = (tmem[0x800 | at], tmem[0x800 | ((at + 1) & 0x7ff)]);
            [r, g, b, a].map(i32::from)
        }
        TexelFormat::Rgba16 => rgba16(half()),
        TexelFormat::Ia16 => {
            let v = half();
            let (i, a) = ((v >> 8) as i32, (v & 0xff) as i32);
            [i, i, i, a]
        }
        TexelFormat::Ia8 => {
            let v = index();
            let (i, a) = ((v >> 4 & 15) as i32 * 17, (v & 15) as i32 * 17);
            [i, i, i, a]
        }
        TexelFormat::Ia4 => {
            let v = index() & 15;
            let i = (v >> 1) as i32;
            let i = i << 5 | i << 2 | i >> 1;
            [i, i, i, if v & 1 != 0 { 255 } else { 0 }]
        }
        TexelFormat::I4 => [(index() & 15) as i32 * 17; 4],
        TexelFormat::I8 => [index() as i32; 4],
    }
}

/// A combiner cycle for span_cycles: the plan, or the inputs.
enum CycleInputs {
    Plan(CombinePlan),
    Picked(PickedInputs),
}

impl CycleInputs {
    #[inline(always)]
    fn combine(&self, prim: Color, env: Color, pixel: &[Color; 4]) -> Color {
        match self {
            CycleInputs::Plan(plan) => combine_plan(*plan, prim, env, pixel[0], pixel[1], pixel[2], pixel[3]),
            CycleInputs::Picked(inputs) => inputs.combine(pixel),
        }
    }
}

/// Whether the combiner's cycles in use (both in two-cycle mode, the second in one) take texel 0
/// and texel 1, by their selectors (the second of two cycles takes them the other way round).
fn texels_used(m: u64, two_cycle: bool) -> [bool; 2] {
    let bits = |shift: u32, width: u32| (m >> shift & ((1 << width) - 1)) as usize;
    let mut used = [false; 2];
    for cycle in if two_cycle { 0..2 } else { 1..2 } {
        let (sub_a, mul, sub_b, add, alpha) = if cycle == 0 {
            (bits(52, 4), bits(47, 5), bits(28, 4), bits(15, 3), [bits(44, 3), bits(41, 3), bits(12, 3), bits(9, 3)])
        } else {
            (bits(37, 4), bits(32, 5), bits(24, 4), bits(6, 3), [bits(21, 3), bits(18, 3), bits(3, 3), bits(0, 3)])
        };
        let swapped = usize::from(two_cycle && cycle == 1);
        for selector in [sub_a, sub_b, add, mul].into_iter().chain(alpha) {
            if selector == 1 || selector == 2 {
                used[(selector - 1) ^ swapped] = true;
            }
        }
        if mul == 8 || mul == 9 {
            used[(mul - 8) ^ swapped] = true;
        }
    }
    used
}

/// A sampler for a tile of this texel format, bilinear or not: sample_tile_texels with both fixed.
type Sampler = fn(&[u8; 0x1000], &(Tile, TexelFormat, Axis, Axis), i32, i32) -> Color;

fn sampler(format: TexelFormat, bilinear: bool) -> Sampler {
    macro_rules! each {
        ($($format:pat => $n:literal),*) => {
            match (format, bilinear) {
                $(($format, true) => sample_with::<$n, true>, ($format, false) => sample_with::<$n, false>,)*
            }
        };
    }
    each!(TexelFormat::Palette { ia: false } => 0, TexelFormat::Palette { ia: true } => 1, TexelFormat::Rgba32 => 2,
        TexelFormat::Rgba16 => 3, TexelFormat::Ia16 => 4, TexelFormat::Ia8 => 5, TexelFormat::Ia4 => 6,
        TexelFormat::I4 => 7, TexelFormat::I8 => 8)
}

fn sample_with<const FORMAT: u8, const BILINEAR: bool>(tmem: &[u8; 0x1000],
    tile: &(Tile, TexelFormat, Axis, Axis), s: i32, t: i32) -> Color {
    let format = match FORMAT {
        0 => TexelFormat::Palette { ia: false },
        1 => TexelFormat::Palette { ia: true },
        2 => TexelFormat::Rgba32,
        3 => TexelFormat::Rgba16,
        4 => TexelFormat::Ia16,
        5 => TexelFormat::Ia8,
        6 => TexelFormat::Ia4,
        7 => TexelFormat::I4,
        _ => TexelFormat::I8,
    };
    sample_tile_texels(tmem, &(tile.0, format, tile.2, tile.3), BILINEAR, s, t)
}

/// A tile's texel at texture coordinates, as sample_three has it: bilinearly filtered from the
/// three texels on the fractions' side, or point sampled.
#[inline(always)]
fn sample_tile_texels(tmem: &[u8; 0x1000], (tile, format, axis_s, axis_t): &(Tile, TexelFormat, Axis, Axis),
    bilinear: bool, s: i32, t: i32) -> Color {
    let (s, sfrac) = axis_s.coordinate(s);
    let (t, tfrac) = axis_t.coordinate(t);
    let (s0, s1) = axis_s.wrap(s);
    let (t0, t1) = axis_t.wrap(t);
    if !bilinear {
        return decode_texel(tmem, tile, *format, s0, t0);
    }
    let lower = sfrac + tfrac < 32;
    let (sa, ta, sb, tb, sc, tc) = if lower { (s0, t0, s1, t0, s0, t1) } else { (s1, t1, s0, t1, s1, t0) };
    let (fs, ft) = if lower { (sfrac, tfrac) } else { (32 - sfrac, 32 - tfrac) };
    let (corner, along_s, along_t) = (decode_texel(tmem, tile, *format, sa, ta),
        decode_texel(tmem, tile, *format, sb, tb), decode_texel(tmem, tile, *format, sc, tc));
    std::array::from_fn(|c| corner[c] + ((fs * (along_s[c] - corner[c]) + ft * (along_t[c] - corner[c]) + 0x10) >> 5))
}

/// A texel of a tile as decode_texel decodes it, packed as raster_neon takes it (r, g, b, a bytes).
#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
#[inline(always)]
fn decode_packed(tmem: &[u8; 0x1000], tile: &Tile, format: TexelFormat, s: i32, t: i32) -> u32 {
    use crate::raster_neon as neon;
    let (su, tu) = (s as u32, t as u32);
    let offset = match tile.size {
        0 => su >> 1,
        1 => su,
        _ => su.wrapping_mul(2),
    };
    let address = tile.tmem.wrapping_add(tu.wrapping_mul(tile.line)).wrapping_add(offset) ^ (tu & 1) << 2;
    let at = address as usize & if tile.size == 3 { 0x7ff } else { 0xfff };
    let index = || {
        let byte = u32::from(tmem[at]);
        if tile.size == 0 { tile.palette << 4 | if su & 1 == 0 { byte >> 4 } else { byte & 15 } } else { byte }
    };
    match format {
        TexelFormat::Palette { ia } => {
            let entry_at = 0x800 + (index() as usize & 0xff) * 8;
            let (first, second) = (tmem[entry_at], tmem[entry_at + 1]);
            if ia { neon::ia16(first, second) } else { neon::rgba16(first, second) }
        }
        TexelFormat::Rgba32 => {
            u32::from_le_bytes([tmem[at], tmem[(at + 1) & 0x7ff], tmem[0x800 | at], tmem[0x800 | ((at + 1) & 0x7ff)]])
        }
        TexelFormat::Rgba16 => neon::rgba16(tmem[at], tmem[(at + 1) & 0xfff]),
        TexelFormat::Ia16 => neon::ia16(tmem[at], tmem[(at + 1) & 0xfff]),
        TexelFormat::Ia8 => neon::IA8[index() as usize & 0xff],
        TexelFormat::Ia4 => neon::IA4[index() as usize & 15],
        // The four bits times 17 in each byte; the eight bits in each.
        TexelFormat::I4 => (index() & 15) * 0x1111_1111,
        TexelFormat::I8 => index() * 0x0101_0101,
    }
}

/// decode_packed for several texels (the filter's three, of one tile or of two), each load for all of
/// them before any is used (raster_neon::together). With a tile's TMEM address and line even and texels
/// of 16 or 32 bits, a 16-bit texel's bytes (and an RGBA32 texel's pairs) are halfwords.
#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
#[inline(always)]
fn decode_packed_n<const N: usize>(tmem: &[u8; 0x1000], tiles: [&Tile; N], format: TexelFormat,
    st: [(i32, i32); N]) -> [u32; N] {
    use crate::raster_neon as neon;
    let at: [usize; N] = std::array::from_fn(|k| {
        let (tile, (su, tu)) = (tiles[k], (st[k].0 as u32, st[k].1 as u32));
        let offset = match tile.size {
            0 => su >> 1,
            1 => su,
            _ => su.wrapping_mul(2),
        };
        let address = tile.tmem.wrapping_add(tu.wrapping_mul(tile.line)).wrapping_add(offset) ^ (tu & 1) << 2;
        address as usize & if tile.size == 3 { 0x7ff } else { 0xfff }
    });
    let even = tiles.iter().all(|tile| tile.tmem & 1 == 0 && tile.line & 1 == 0 && tile.size >= 2);
    // Two bytes as a little-endian halfword.
    let half = |at: usize| u32::from(tmem[at]) | u32::from(tmem[at + 1]) << 8;
    match format {
        TexelFormat::Rgba16 | TexelFormat::Ia16 if even => {
            let half = neon::together(at.map(|at| half(at & 0xffe)));
            if format == TexelFormat::Rgba16 {
                neon::rgba16_halves(half)
            } else {
                half.map(|half| neon::ia16(half as u8, (half >> 8) as u8))
            }
        }
        TexelFormat::Rgba32 if even => {
            let (low, high) = (at.map(|at| half(at & 0x7fe)), at.map(|at| half(0x800 | (at & 0x7fe))));
            let (low, high) = (neon::together(low), neon::together(high));
            std::array::from_fn(|k| low[k] & 0xffff | high[k] << 16)
        }
        _ => {
            let first = neon::together(at.map(|at| u32::from(tmem[at])));
            let index: [u32; N] = std::array::from_fn(|k| {
                if tiles[k].size == 0 {
                    tiles[k].palette << 4 | if st[k].0 & 1 == 0 { first[k] >> 4 } else { first[k] & 15 }
                } else {
                    first[k]
                }
            });
            let second = || at.map(|at| tmem[(at + 1) & 0xfff]);
            match format {
                TexelFormat::Palette { ia } => {
                    // Entries are at even addresses.
                    let entry = neon::together(index.map(|index| half(0x800 + (index as usize & 0xff) * 8)));
                    if ia {
                        entry.map(|entry| neon::ia16(entry as u8, (entry >> 8) as u8))
                    } else {
                        neon::rgba16_halves(entry)
                    }
                }
                TexelFormat::Rgba32 => {
                    let g = at.map(|at| tmem[(at + 1) & 0x7ff]);
                    let (b, a) = (at.map(|at| tmem[0x800 | at]), at.map(|at| tmem[0x800 | ((at + 1) & 0x7ff)]));
                    std::array::from_fn(|k| u32::from_le_bytes([first[k] as u8, g[k], b[k], a[k]]))
                }
                TexelFormat::Rgba16 => {
                    let second = second();
                    std::array::from_fn(|k| neon::rgba16(first[k] as u8, second[k]))
                }
                TexelFormat::Ia16 => {
                    let second = second();
                    std::array::from_fn(|k| neon::ia16(first[k] as u8, second[k]))
                }
                TexelFormat::Ia8 => index.map(|v| neon::IA8[v as usize & 0xff]),
                TexelFormat::Ia4 => index.map(|v| neon::IA4[v as usize & 15]),
                TexelFormat::I4 => index.map(|v| (v & 15) * 0x1111_1111),
                TexelFormat::I8 => index.map(|v| v * 0x0101_0101),
            }
        }
    }
}

/// sample_tile_texels with NEON: the texels packed and filtered on four lanes.
#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
type NeonSampler = fn(&[u8; 0x1000], &(Tile, TexelFormat, Axis, Axis), i32, i32) -> crate::raster_neon::Lanes;

#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
fn neon_sampler(format: TexelFormat, bilinear: bool) -> NeonSampler {
    macro_rules! each {
        ($($format:pat => $n:literal),*) => {
            match (format, bilinear) {
                $(($format, true) => neon_sample_with::<$n, true>, ($format, false) => neon_sample_with::<$n, false>,)*
            }
        };
    }
    each!(TexelFormat::Palette { ia: false } => 0, TexelFormat::Palette { ia: true } => 1, TexelFormat::Rgba32 => 2,
        TexelFormat::Rgba16 => 3, TexelFormat::Ia16 => 4, TexelFormat::Ia8 => 5, TexelFormat::Ia4 => 6,
        TexelFormat::I4 => 7, TexelFormat::I8 => 8)
}

#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
fn neon_sample_with<const FORMAT: u8, const BILINEAR: bool>(tmem: &[u8; 0x1000],
    (tile, _, axis_s, axis_t): &(Tile, TexelFormat, Axis, Axis), s: i32, t: i32) -> crate::raster_neon::Lanes {
    use crate::raster_neon::{self as neon, Lanes};
    let format = match FORMAT {
        0 => TexelFormat::Palette { ia: false },
        1 => TexelFormat::Palette { ia: true },
        2 => TexelFormat::Rgba32,
        3 => TexelFormat::Rgba16,
        4 => TexelFormat::Ia16,
        5 => TexelFormat::Ia8,
        6 => TexelFormat::Ia4,
        7 => TexelFormat::I4,
        _ => TexelFormat::I8,
    };
    let (s, sfrac) = axis_s.coordinate(s);
    let (t, tfrac) = axis_t.coordinate(t);
    let (s0, s1) = axis_s.wrap(s);
    let (t0, t1) = axis_t.wrap(t);
    if !BILINEAR {
        return neon::widen(decode_packed(tmem, tile, format, s0, t0));
    }
    let lower = sfrac + tfrac < 32;
    let (sa, ta, sb, tb, sc, tc) = if lower { (s0, t0, s1, t0, s0, t1) } else { (s1, t1, s0, t1, s1, t0) };
    let (fs, ft) = if lower { (sfrac, tfrac) } else { (32 - sfrac, 32 - tfrac) };
    let weights = Lanes::pair((32 - fs - ft) as u32 | (fs as u32) << 16, ft as u32);
    let [corner, along_s, along_t] = decode_packed_n(tmem, [tile; 3], format, [(sa, ta), (sb, tb), (sc, tc)]);
    neon::filter3(corner, along_s, along_t, weights)
}

/// What span_cycles_neon's combiner needs of the modes: both cycles' input lanes, whether the second
/// gives the first's color back, and which texels the cycles take. Made again only when what they come
/// from changes (the modes, the colors and constants the inputs pick, the noise's bits of the cycle
/// count): spans of a draw, and draws after it, mostly keep them.
#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
type CombinerLanes = ([crate::raster_neon::CycleLanes; 2], bool, [bool; 2]);

#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
fn combiner_lanes(rdp: &Rdp, cycles: u64, two_cycle: bool) -> CombinerLanes {
    type Key = (u64, [u32; 5], [i32; 2], u64, bool);
    thread_local! {
        static LAST: std::cell::Cell<Option<(Key, CombinerLanes)>> = const { std::cell::Cell::new(None) };
    }
    let colors = [rdp.prim, rdp.env, rdp.key[0], rdp.key[1], rdp.prim_lod_frac];
    let key = (rdp.combine, colors, [rdp.convert[4], rdp.convert[5]], cycles >> 3 & 7, two_cycle);
    LAST.with(|last| {
        if let Some((made_for, made)) = last.get() && made_for == key {
            return made;
        }
        let [first, second] = [0, 1].map(|cycle| PickedInputs::new(rdp, cycle, cycles));
        // The second of two cycles takes the texels the other way round.
        let second = if two_cycle { second.texels_swapped() } else { second };
        let made = ([first.lanes(), second.lanes()], two_cycle && second.passes_combined(),
            texels_used(rdp.combine, two_cycle));
        last.set(Some((key, made)));
        made
    })
}

/// Both texels of two-cycle mode at once, for two tiles of one texel format with the same axes: the
/// coordinates, wrap and filter weights worked out once (the tiles may still differ in TMEM address,
/// line, size and palette).
#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
type NeonPairSampler = fn(&[u8; 0x1000], &(Tile, TexelFormat, Axis, Axis), &Tile, i32, i32)
    -> (crate::raster_neon::Lanes, crate::raster_neon::Lanes);

#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
fn neon_pair_sampler(format: TexelFormat, bilinear: bool) -> NeonPairSampler {
    macro_rules! each {
        ($($format:pat => $n:literal),*) => {
            match (format, bilinear) {
                $(($format, true) => neon_sample_pair_with::<$n, true>,
                    ($format, false) => neon_sample_pair_with::<$n, false>,)*
            }
        };
    }
    each!(TexelFormat::Palette { ia: false } => 0, TexelFormat::Palette { ia: true } => 1, TexelFormat::Rgba32 => 2,
        TexelFormat::Rgba16 => 3, TexelFormat::Ia16 => 4, TexelFormat::Ia8 => 5, TexelFormat::Ia4 => 6,
        TexelFormat::I4 => 7, TexelFormat::I8 => 8)
}

#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
fn neon_sample_pair_with<const FORMAT: u8, const BILINEAR: bool>(tmem: &[u8; 0x1000],
    (tile, _, axis_s, axis_t): &(Tile, TexelFormat, Axis, Axis), other: &Tile, s: i32, t: i32)
    -> (crate::raster_neon::Lanes, crate::raster_neon::Lanes) {
    use crate::raster_neon::{self as neon, Lanes};
    let format = match FORMAT {
        0 => TexelFormat::Palette { ia: false },
        1 => TexelFormat::Palette { ia: true },
        2 => TexelFormat::Rgba32,
        3 => TexelFormat::Rgba16,
        4 => TexelFormat::Ia16,
        5 => TexelFormat::Ia8,
        6 => TexelFormat::Ia4,
        7 => TexelFormat::I4,
        _ => TexelFormat::I8,
    };
    let (s, sfrac) = axis_s.coordinate(s);
    let (t, tfrac) = axis_t.coordinate(t);
    let (s0, s1) = axis_s.wrap(s);
    let (t0, t1) = axis_t.wrap(t);
    if !BILINEAR {
        return (neon::widen(decode_packed(tmem, tile, format, s0, t0)),
            neon::widen(decode_packed(tmem, other, format, s0, t0)));
    }
    let lower = sfrac + tfrac < 32;
    let (sa, ta, sb, tb, sc, tc) = if lower { (s0, t0, s1, t0, s0, t1) } else { (s1, t1, s0, t1, s1, t0) };
    let (fs, ft) = if lower { (sfrac, tfrac) } else { (32 - sfrac, 32 - tfrac) };
    let weights = Lanes::pair((32 - fs - ft) as u32 | (fs as u32) << 16, ft as u32);
    // Both tiles' texels loaded together.
    let points = [(sa, ta), (sb, tb), (sc, tc), (sa, ta), (sb, tb), (sc, tc)];
    let [a0, b0, c0, a1, b1, c1] = decode_packed_n(tmem, [tile, tile, tile, other, other, other], format, points);
    (neon::filter3(a0, b0, c0, weights), neon::filter3(a1, b1, c1, weights))
}

/// A combiner input: a color fixed for the span, or one of a pixel's (the combined color, texel 0,
/// texel 1, shade), whole or its alpha in all four.
#[derive(Clone, Copy, PartialEq)]
enum Input {
    Fixed(Color),
    Pixel(u8),
    PixelAlpha(u8),
}

/// An alpha input: fixed for the span, or a pixel's color's alpha.
#[derive(Clone, Copy, PartialEq)]
enum AlphaInput {
    Fixed(i32),
    Pixel(u8),
}

/// combine_selected's inputs of a cycle, picked once for a span.
struct PickedInputs {
    rgb: [Input; 4],
    alpha: [AlphaInput; 4],
}

impl PickedInputs {
    /// As combine_selected picks them, for this cycle (at these cycles, for the noise).
    fn new(rdp: &crate::rdp::Rdp, cycle: usize, cycles: u64) -> PickedInputs {
        let m = rdp.combine;
        let bits = |shift: u32, width: u32| (m >> shift & ((1 << width) - 1)) as usize;
        let (sub_a, mul, sub_b, add) = if cycle == 0 {
            (bits(52, 4), bits(47, 5), bits(28, 4), bits(15, 3))
        } else {
            (bits(37, 4), bits(32, 5), bits(24, 4), bits(6, 3))
        };
        let (alpha_a, alpha_mul, alpha_b, alpha_add) = if cycle == 0 {
            (bits(44, 3), bits(41, 3), bits(12, 3), bits(9, 3))
        } else {
            (bits(21, 3), bits(18, 3), bits(3, 3), bits(0, 3))
        };
        let (prim, env) = (rgba32(rdp.prim), rgba32(rdp.env));
        let lod_frac = 255;
        let (one, zero) = ([256; 4], [0; 4]);
        let pick = |selector: usize| match selector {
            0 => Input::Pixel(0),
            1 => Input::Pixel(1),
            2 => Input::Pixel(2),
            3 => Input::Fixed(prim),
            4 => Input::Pixel(3),
            5 => Input::Fixed(env),
            _ => Input::Fixed(zero),
        };
        let a = match sub_a {
            6 => Input::Fixed(one),
            7 => Input::Fixed([((cycles as i32 >> 3 & 7) << 6) | 0x20; 4]),
            selector => pick(selector),
        };
        let b = match sub_b {
            6 => Input::Fixed(key_center(rdp.key)),
            7 => Input::Fixed([rdp.convert[4]; 4]),
            selector => pick(selector),
        };
        let c = match mul {
            0..=5 => pick(mul),
            6 => Input::Fixed(key_scale(rdp.key)),
            7 => Input::PixelAlpha(0),
            8 => Input::PixelAlpha(1),
            9 => Input::PixelAlpha(2),
            10 => Input::Fixed([prim[3]; 4]),
            11 => Input::PixelAlpha(3),
            12 => Input::Fixed([env[3]; 4]),
            13 => Input::Fixed([lod_frac; 4]),
            14 => Input::Fixed([rdp.prim_lod_frac as i32; 4]),
            15 => Input::Fixed([rdp.convert[5]; 4]),
            _ => Input::Fixed(zero),
        };
        let d = if add == 6 { Input::Fixed(one) } else { pick(add) };
        let alpha_of = |input: Input| match input {
            Input::Fixed(color) => AlphaInput::Fixed(color[3]),
            Input::Pixel(n) | Input::PixelAlpha(n) => AlphaInput::Pixel(n),
        };
        let alpha = |which: usize| if which == 6 { AlphaInput::Fixed(256) } else { alpha_of(pick(which)) };
        let alpha_mul = match alpha_mul {
            0 => AlphaInput::Fixed(lod_frac),
            6 => AlphaInput::Fixed(rdp.prim_lod_frac as i32),
            7 => AlphaInput::Fixed(0),
            selector => alpha_of(pick(selector)),
        };
        PickedInputs { rgb: [a, b, c, d], alpha: [alpha(alpha_a), alpha(alpha_b), alpha_mul, alpha(alpha_add)] }
    }

    /// The same inputs with texel 0 and texel 1 the other way round, as the second of two cycles takes
    /// them.
    #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
    fn texels_swapped(self) -> PickedInputs {
        let swap = |n: u8| if n == 1 || n == 2 { 3 - n } else { n };
        PickedInputs {
            rgb: self.rgb.map(|input| match input {
                Input::Pixel(n) => Input::Pixel(swap(n)),
                Input::PixelAlpha(n) => Input::PixelAlpha(swap(n)),
                fixed => fixed,
            }),
            alpha: self.alpha.map(|input| match input {
                AlphaInput::Pixel(n) => AlphaInput::Pixel(swap(n)),
                fixed => fixed,
            }),
        }
    }

    /// Whether the cycle gives the combined color (0-255) back: (A - B) * C is nothing in color and
    /// alpha (A is B, or C is zero) and D is the combined color, which clamp9 leaves as it is.
    #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
    fn passes_combined(&self) -> bool {
        let [a, b, c, d] = self.rgb;
        let [alpha_a, alpha_b, alpha_c, alpha_d] = self.alpha;
        (a == b || matches!(c, Input::Fixed([0, 0, 0, _]))) && d == Input::Pixel(0)
            && (alpha_a == alpha_b || alpha_c == AlphaInput::Fixed(0)) && alpha_d == AlphaInput::Pixel(0)
    }

    /// Whether every lane (the color's three, the alpha) is a product `(a * c + 128) >> 8` plus d that
    /// stays within 0-255: b nothing, a and c 0-255, and the largest a * c and d summing to at most 255.
    /// Then (A - B) * C + D needs no 32 bits and clamp9 changes nothing (raster_neon's combine_cycle).
    #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
    fn is_product(&self) -> bool {
        // The values an input can take: a pixel's colors are 0-255.
        let color = |input: Input, lane: usize| match input {
            Input::Fixed(color) => (color[lane], color[lane]),
            Input::Pixel(_) | Input::PixelAlpha(_) => (0, 255),
        };
        let alpha = |input: AlphaInput| match input {
            AlphaInput::Fixed(value) => (value, value),
            AlphaInput::Pixel(_) => (0, 255),
        };
        (0..4).all(|lane| {
            let [a, b, c, d] = if lane < 3 { self.rgb.map(|input| color(input, lane)) } else { self.alpha.map(alpha) };
            let within = |(low, high): (i32, i32)| low >= 0 && high <= 255;
            b == (0, 0) && within(a) && within(c) && within(d) && ((a.1 * c.1 + 0x80) >> 8) + d.1 <= 255
        })
    }

    /// The inputs as NEON lanes: the color's in lanes 0-2, the alpha's in lane 3; and whether the cycle
    /// is a product.
    #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
    fn lanes(&self) -> crate::raster_neon::CycleLanes {
        let color = |input: Input, lane: usize| match input {
            Input::Fixed(color) => Err(color[lane] as i16),
            Input::Pixel(n) => Ok((n, lane as u8)),
            Input::PixelAlpha(n) => Ok((n, 3)),
        };
        let alpha = |input: AlphaInput| match input {
            AlphaInput::Fixed(value) => Err(value as i16),
            AlphaInput::Pixel(n) => Ok((n, 3)),
        };
        crate::raster_neon::CycleLanes::new(std::array::from_fn(|k| {
            [color(self.rgb[k], 0), color(self.rgb[k], 1), color(self.rgb[k], 2), alpha(self.alpha[k])]
        }), self.is_product())
    }

    /// (A - B) * C + D for the color and the alpha, as combine_selected works it.
    #[inline(always)]
    fn combine(&self, pixel: &[Color; 4]) -> Color {
        let get = |input: Input| match input {
            Input::Fixed(color) => color,
            Input::Pixel(n) => pixel[usize::from(n)],
            Input::PixelAlpha(n) => [pixel[usize::from(n)][3]; 4],
        };
        let alpha = |input: AlphaInput| match input {
            AlphaInput::Fixed(v) => v,
            AlphaInput::Pixel(n) => pixel[usize::from(n)][3],
        };
        let (a, b, c, d) = (get(self.rgb[0]), get(self.rgb[1]), get(self.rgb[2]), get(self.rgb[3]));
        let (aa, ab, ac, ad) = (alpha(self.alpha[0]), alpha(self.alpha[1]), alpha(self.alpha[2]), alpha(self.alpha[3]));
        [
            clamp9(((a[0] - b[0]) * c[0] + (d[0] << 8) + 0x80) >> 8),
            clamp9(((a[1] - b[1]) * c[1] + (d[1] << 8) + 0x80) >> 8),
            clamp9(((a[2] - b[2]) * c[2] + (d[2] << 8) + 0x80) >> 8),
            clamp9(((aa - ab) * ac + (ad << 8) + 0x80) >> 8),
        ]
    }
}

/// combine_specialized's formulas, the primitive and environment colors
/// given.
#[inline(always)]
fn combine_plan(plan: CombinePlan, prim: Color, env: Color, combined: Color, texel0: Color, texel1: Color,
    shade: Color) -> Color {
    let rgb = usize::from(plan.rgb);
    let alpha = usize::from(plan.alpha);
    let d = usize::from(plan.rgb_source);
    let ad = usize::from(plan.alpha_source);
    let source = |selector: usize| -> Color {
        match selector {
            0 => combined,
            1 => texel0,
            2 => texel1,
            3 => prim,
            4 => shade,
            5 => env,
            6 => [256; 4],
            _ => [0; 4],
        }
    };
    let mut output = match rgb {
        0 => source(d).map(clamp9),
        1 | 2 => {
            let texture = if rgb == 1 { texel0 } else { texel1 };
            std::array::from_fn(|n| clamp9((texture[n] * shade[n] + 0x80) >> 8))
        }
        5 | 6 => {
            let texture = if rgb == 5 { texel0 } else { texel1 };
            std::array::from_fn(|n| clamp9((texture[n] * prim[n] + 0x80) >> 8))
        }
        _ => {
            let texture = if rgb == 3 { texel0 } else { texel1 };
            std::array::from_fn(|n| clamp9(((prim[n] - shade[n]) * texture[n] + (shade[n] << 8) + 0x80) >> 8))
        }
    };
    output[3] = match alpha {
        0 => clamp9(source(ad)[3]),
        1 | 2 => {
            let texture = if alpha == 1 { texel0 } else { texel1 };
            clamp9((texture[3] * shade[3] + 0x80) >> 8)
        }
        5 | 6 => {
            let texture = if alpha == 5 { texel0 } else { texel1 };
            clamp9((texture[3] * prim[3] + 0x80) >> 8)
        }
        _ => {
            let texture = if alpha == 3 { texel0 } else { texel1 };
            clamp9(((prim[3] - shade[3]) * texture[3] + (shade[3] << 8) + 0x80) >> 8)
        }
    };
    output
}

impl Raster<'_> {

    /// Original generic selector pipeline, retained as the conformance reference.
    fn combine_reference(&self, cycle: usize, combined: Color, texel0: Color, texel1: Color, shade: Color) -> Color {
        let rdp = &self.rdp;
        let m = rdp.combine;
        let bits = |shift: u32, width: u32| (m >> shift & ((1 << width) - 1)) as usize;
        let (sub_a, mul, sub_b, add) = if cycle == 0 {
            (bits(52, 4), bits(47, 5), bits(28, 4), bits(15, 3))
        } else {
            (bits(37, 4), bits(32, 5), bits(24, 4), bits(6, 3))
        };
        let (alpha_a, alpha_mul, alpha_b, alpha_add) = if cycle == 0 {
            (bits(44, 3), bits(41, 3), bits(12, 3), bits(9, 3))
        } else {
            (bits(21, 3), bits(18, 3), bits(3, 3), bits(0, 3))
        };
        let (prim, env) = (rgba32(rdp.prim), rgba32(rdp.env));
        // Without mipmaps (one level, LOD off) the RDP's LOD fraction comes
        // out at its maximum.
        let lod_frac = 255;
        let prim_lod = rdp.prim_lod_frac as i32;
        let noise = ((self.cycles as i32 >> 3 & 7) << 6) | 0x20;
        let mut out = [0; 4];
        for c in 0..3 {
            let a = match sub_a {
                0 => combined[c],
                1 => texel0[c],
                2 => texel1[c],
                3 => prim[c],
                4 => shade[c],
                5 => env[c],
                6 => 256,
                7 => noise,
                _ => 0,
            };
            let b = match sub_b {
                0 => combined[c],
                1 => texel0[c],
                2 => texel1[c],
                3 => prim[c],
                4 => shade[c],
                5 => env[c],
                6 => key_center(rdp.key)[c],
                7 => rdp.convert[4],
                _ => 0,
            };
            let mul = match mul {
                0 => combined[c],
                1 => texel0[c],
                2 => texel1[c],
                3 => prim[c],
                4 => shade[c],
                5 => env[c],
                6 => key_scale(rdp.key)[c],
                7 => combined[3],
                8 => texel0[3],
                9 => texel1[3],
                10 => prim[3],
                11 => shade[3],
                12 => env[3],
                13 => lod_frac,
                14 => prim_lod,
                15 => rdp.convert[5],
                _ => 0,
            };
            let d = match add {
                0 => combined[c],
                1 => texel0[c],
                2 => texel1[c],
                3 => prim[c],
                4 => shade[c],
                5 => env[c],
                6 => 256,
                _ => 0,
            };
            out[c] = clamp9(((a - b) * mul + (d << 8) + 0x80) >> 8);
        }
        let alpha_input = |which: usize| match which {
            0 => combined[3],
            1 => texel0[3],
            2 => texel1[3],
            3 => prim[3],
            4 => shade[3],
            5 => env[3],
            6 => 256,
            _ => 0,
        };
        let alpha_mul = match alpha_mul {
            0 => lod_frac,
            1 => texel0[3],
            2 => texel1[3],
            3 => prim[3],
            4 => shade[3],
            5 => env[3],
            6 => prim_lod,
            _ => 0,
        };
        let alpha = (alpha_input(alpha_a) - alpha_input(alpha_b)) * alpha_mul + (alpha_input(alpha_add) << 8);
        out[3] = clamp9((alpha + 0x80) >> 8);
        out
    }

    /// The blender: P * A + M * B, with P and M among the pixel's color, the
    /// memory color, the blend and fog colors, A among the pixel's, the
    /// fog's and the shade's alpha. The second cycle (or the only one)
    /// blends only where force_blend is set (every pixel here is fully
    /// covered); otherwise it passes P through.
    fn blend(&self, two_cycle: bool, combined: Color, shade_alpha: i32, memory: Color) -> Color {
        let rdp = &self.rdp;
        let other = rdp.other_modes;
        let (blend_color, fog) = (rgba32(rdp.blend), rgba32(rdp.fog));
        let force = other >> 14 & 1 != 0;
        let cycle = |n: u32, pixel: Color, always: bool| -> Color {
            let p_sel = (other >> (30 - 2 * n) & 3) as usize;
            let a_sel = (other >> (26 - 2 * n) & 3) as usize;
            let m_sel = (other >> (22 - 2 * n) & 3) as usize;
            let b_sel = (other >> (18 - 2 * n) & 3) as usize;
            let pick = |sel: usize| match sel {
                0 => pixel,
                1 => memory,
                2 => blend_color,
                _ => fog,
            };
            let (p, m) = (pick(p_sel), pick(m_sel));
            if !always && !force {
                return [p[0], p[1], p[2], combined[3]];
            }
            let a = match a_sel {
                0 => combined[3],
                1 => fog[3],
                2 => shade_alpha,
                _ => 0,
            } & 0xff;
            let b = match b_sel {
                0 => !a & 0xff,
                1 => memory[3],
                2 => 0xff,
                _ => 0,
            };
            let (a5, b5) = (a >> 3, (b >> 3) + 1);
            let mix = |c: usize| ((p[c] * a5 + m[c] * b5) >> 5).clamp(0, 255);
            [mix(0), mix(1), mix(2), combined[3]]
        };
        if two_cycle {
            let first = cycle(0, combined, true);
            cycle(1, first, false)
        } else {
            cycle(0, combined, false)
        }
    }

    /// The color image's pixel, as a color (its alpha the pixel's coverage
    /// as the blender sees memory alpha).
    fn read_pixel(&self, base: usize, i: usize, size: u32) -> Color {
        match size {
            3 => {
                let at = (base + i * 4) & 0x7f_fffc;
                let p = self.rdram.get(at..at + 4).map_or(0, |b| u32::from_be_bytes(b.try_into().unwrap()));
                [(p >> 24) as i32, (p >> 16 & 0xff) as i32, (p >> 8 & 0xff) as i32, (p & 0xe0) as i32]
            }
            _ => {
                let at = (base + i * 2) & 0x7f_fffe;
                let p = self.rdram.get(at..at + 2).map_or(0, |b| u32::from(u16::from_be_bytes([b[0], b[1]])));
                [expand5(p >> 11 & 31), expand5(p >> 6 & 31), expand5(p >> 1 & 31), if p & 1 != 0 { 0xe0 } else { 0 }]
            }
        }
    }

    /// Writes a color to the color image, with full coverage in its alpha
    /// bits.
    fn write_pixel(&mut self, base: usize, i: usize, size: u32, color: Color) {
        let [r, g, b, _] = color.map(|c| c.clamp(0, 255) as u32);
        match size {
            3 => self.store32(base + i * 4, r << 24 | g << 16 | b << 8 | 0xe0),
            2 => self.store16(base + i * 2, ((r >> 3) << 11 | (g >> 3) << 6 | (b >> 3) << 1 | 1) as u16),
            _ => {}
        }
    }

    /// A texel of a tile at texture coordinates (s10.5, from the divide or
    /// straight), bilinearly filtered (three of the four neighbors, as the
    /// RDP does) or point sampled.
    fn sample(&mut self, tile_index: usize, s: i32, t: i32) -> Color {
        if !self.rdp.accelerated {
            return self.sample_reference(tile_index, s, t);
        }
        let tile = self.rdp.tiles[tile_index];
        // The format test is shared by every contributing texel. Size2
        // is direct RGBA5551 even with TLUT enabled.
        if self.rdp.rgba16_sampling && tile.format == 0 && tile.size == 2 {
            self.sample_three::<true>(&tile, s, t)
        } else {
            self.sample_three::<false>(&tile, s, t)
        }
    }

    fn sample_three<const RGBA16: bool>(&mut self, tile: &Tile, s: i32, t: i32) -> Color {
        let (s, sfrac, t, tfrac) = self.texture_coordinates(tile, s, t);
        let (s0, s1) = self.wrap(tile, 0, s);
        let (t0, t1) = self.wrap(tile, 1, t);
        if self.rdp.other_modes >> 45 & 1 == 0 {
            if RGBA16 && self.rdp.profiling {
                self.rdp.rgba16_samples += 1;
            }
            return self.sample_texel::<RGBA16>(tile, s0, t0);
        }
        // Only the three texels on the selected triangle contribute.
        // Texel reads are pure; avoid decoding the unused fourth corner.
        let (base, along_s, along_t, fs, ft) = if sfrac + tfrac < 32 {
            (self.sample_texel::<RGBA16>(tile, s0, t0), self.sample_texel::<RGBA16>(tile, s1, t0),
                self.sample_texel::<RGBA16>(tile, s0, t1), sfrac, tfrac)
        } else {
            (self.sample_texel::<RGBA16>(tile, s1, t1), self.sample_texel::<RGBA16>(tile, s0, t1),
                self.sample_texel::<RGBA16>(tile, s1, t0), 32 - sfrac, 32 - tfrac)
        };
        if self.rdp.profiling {
            self.rdp.three_texel_samples += 1;
            if RGBA16 { self.rdp.rgba16_samples += 1; }
        }
        std::array::from_fn(|c| {
            base[c] + ((fs * (along_s[c] - base[c]) + ft * (along_t[c] - base[c]) + 0x10) >> 5)
        })
    }

    #[inline(always)]
    fn sample_texel<const RGBA16: bool>(&self, tile: &Tile, s: i32, t: i32) -> Color {
        if RGBA16 {
            let (s, t) = (s as u32, t as u32);
            let at = tile.tmem.wrapping_add(t.wrapping_mul(tile.line)).wrapping_add(s.wrapping_mul(2));
            let at = (if t & 1 != 0 { at ^ 4 } else { at }) as usize & 0xfff;
            rgba16(u32::from(u16::from_be_bytes([self.rdp.tmem[at], self.rdp.tmem[(at + 1) & 0xfff]])))
        } else {
            self.texel(tile, s, t)
        }
    }

    /// Original four-corner sampling, retained for exact comparisons.
    fn sample_reference(&self, tile_index: usize, s: i32, t: i32) -> Color {
        let tile = self.rdp.tiles[tile_index];
        let (s, sfrac, t, tfrac) = self.texture_coordinates(&tile, s, t);
        let bilinear = self.rdp.other_modes >> 45 & 1 != 0;
        let (s0, s1) = self.wrap(&tile, 0, s);
        let (t0, t1) = self.wrap(&tile, 1, t);
        let texel00 = self.texel(&tile, s0, t0);
        if !bilinear {
            return texel00;
        }
        let texel10 = self.texel(&tile, s1, t0);
        let texel01 = self.texel(&tile, s0, t1);
        let texel11 = self.texel(&tile, s1, t1);
        std::array::from_fn(|c| {
            if sfrac + tfrac < 32 {
                texel00[c] + ((sfrac * (texel10[c] - texel00[c]) + tfrac * (texel01[c] - texel00[c]) + 0x10) >> 5)
            } else {
                let (is, it) = (32 - sfrac, 32 - tfrac);
                texel11[c] + ((is * (texel01[c] - texel11[c]) + it * (texel10[c] - texel11[c]) + 0x10) >> 5)
            }
        })
    }

    /// A coordinate shifted by the tile's shift, made relative to its upper
    /// left corner and clamped to its extent: the texel and its 5-bit
    /// fraction, for s and t.
    fn texture_coordinates(&self, tile: &Tile, s: i32, t: i32) -> (i32, i32, i32, i32) {
        let axis = |coord: i32, n: usize| -> (i32, i32) {
            // Flags of a perspective divide out of range clamp it.
            let overflow = coord >> 17 & 3;
            let mut c = i32::from(coord as i16);
            let shift = tile.shift[n];
            c = if shift < 11 { c >> shift } else { i32::from((c << (16 - shift)) as i16) };
            let max = (c >> 3) >= tile.hi[n] as i32 || overflow == 2;
            c -= (tile.lo[n] << 3) as i32;
            let clamp = tile.clamp[n] || tile.mask[n] == 0;
            if !clamp {
                return (c >> 5, c & 0x1f);
            }
            if max {
                (((tile.hi[n] >> 2).wrapping_sub(tile.lo[n] >> 2) & 0x3ff) as i32, 0)
            } else if c & 0x10000 != 0 || c < 0 || overflow == 1 {
                (0, 0)
            } else {
                (c >> 5, c & 0x1f)
            }
        };
        let (s, sfrac) = axis(s, 0);
        let (t, tfrac) = axis(t, 1);
        (s, sfrac, t, tfrac)
    }

    /// A texel coordinate and the next one, masked (wrapped) and mirrored
    /// as the tile says.
    fn wrap(&self, tile: &Tile, n: usize, c: i32) -> (i32, i32) {
        let mask = tile.mask[n];
        if mask == 0 {
            return (c, c + 1);
        }
        let bits = ((1 << mask.min(10)) - 1) as i32;
        let one = |c: i32| {
            let mut c = c;
            if tile.mirror[n] && (c >> mask.min(10)) & 1 != 0 {
                c = !c;
            }
            c & bits
        };
        (one(c), one(c + 1))
    }

    /// The TMEM address of a texel's first byte (or nibble's byte), odd
    /// lines with their 32-bit halves swapped.
    fn texel_address(tile: &Tile, s: i32, t: i32) -> usize {
        let (s, t) = (s as u32, t as u32);
        let offset = match tile.size {
            0 => s >> 1,
            1 => s,
            _ => s.wrapping_mul(2),
        };
        let addr = tile.tmem.wrapping_add(t.wrapping_mul(tile.line)).wrapping_add(offset);
        let addr = if t & 1 != 0 { addr ^ 4 } else { addr };
        addr as usize & if tile.size == 3 { 0x7ff } else { 0xfff }
    }

    /// A texel's raw index (for palettes and copy mode): 4 or 8 bits, with
    /// the tile's palette above a 4-bit one.
    fn texel_index(&self, tile: &Tile, s: i32, t: i32) -> u32 {
        let byte = u32::from(self.rdp.tmem[Self::texel_address(tile, s, t)]);
        match tile.size {
            0 => {
                let nibble = if s & 1 == 0 { byte >> 4 } else { byte & 15 };
                tile.palette << 4 | nibble
            }
            _ => byte,
        }
    }

    fn texel16(&self, tile: &Tile, s: i32, t: i32) -> u16 {
        let at = Self::texel_address(tile, s, t);
        u16::from_be_bytes([self.rdp.tmem[at], self.rdp.tmem[(at + 1) & 0xfff]])
    }

    /// A palette entry in high TMEM (each is there four times over).
    fn palette_entry(&self, index: u32) -> u16 {
        let at = 0x800 + (index as usize & 0xff) * 8;
        u16::from_be_bytes([self.rdp.tmem[at], self.rdp.tmem[at + 1]])
    }

    /// A texel as a color, by the tile's format and size (or through the
    /// palette when TLUT is on).
    fn texel(&self, tile: &Tile, s: i32, t: i32) -> Color {
        let other = self.rdp.other_modes;
        if other >> 47 & 1 != 0 && tile.size < 2 {
            let entry = u32::from(self.palette_entry(self.texel_index(tile, s, t)));
            return if other >> 46 & 1 != 0 {
                let (i, a) = ((entry >> 8) as i32, (entry & 0xff) as i32);
                [i, i, i, a]
            } else {
                rgba16(entry)
            };
        }
        match (tile.format, tile.size) {
            (0, 3) => {
                let at = Self::texel_address(tile, s, t);
                let tmem = &self.rdp.tmem;
                let (r, g) = (tmem[at], tmem[(at + 1) & 0x7ff]);
                let (b, a) = (tmem[0x800 | at], tmem[0x800 | ((at + 1) & 0x7ff)]);
                [r, g, b, a].map(i32::from)
            }
            (0, _) => rgba16(u32::from(self.texel16(tile, s, t))),
            (3, 2) => {
                let v = u32::from(self.texel16(tile, s, t));
                let (i, a) = ((v >> 8) as i32, (v & 0xff) as i32);
                [i, i, i, a]
            }
            (3, 1) => {
                let v = self.texel_index(tile, s, t);
                let (i, a) = ((v >> 4 & 15) as i32 * 17, (v & 15) as i32 * 17);
                [i, i, i, a]
            }
            (3, 0) => {
                let v = self.texel_index(tile, s, t) & 15;
                let i = (v >> 1) as i32;
                let i = i << 5 | i << 2 | i >> 1;
                [i, i, i, if v & 1 != 0 { 255 } else { 0 }]
            }
            (_, 0) => {
                let i = (self.texel_index(tile, s, t) & 15) as i32 * 17;
                [i; 4]
            }
            (_, 1) => {
                let i = self.texel_index(tile, s, t) as i32;
                [i; 4]
            }
            _ => rgba16(u32::from(self.texel16(tile, s, t))),
        }
    }

    /// A span of pixels, x0 to x1 and y0 to y1 (exclusive), clipped to the
    /// scissor box and the color image.
    fn clip(&self, x0: u32, y0: u32, x1: u32, y1: u32) -> [u32; 4] {
        let rdp = &self.rdp;
        let [sx0, sy0, sx1, sy1] = rdp.scissor.map(|v| v >> 2);
        let width = rdp.color_image.width;
        [x0.max(sx0), y0.max(sy0), x1.min(sx1).min(width), y1.min(sy1)]
    }

    fn store32(&mut self, at: usize, value: u32) {
        if let Some(word) = self.rdram.get_mut(at & 0x7f_fffc..(at & 0x7f_fffc) + 4) {
            word.copy_from_slice(&value.to_be_bytes());
        }
    }

    fn store16(&mut self, at: usize, value: u16) {
        if let Some(half) = self.rdram.get_mut(at & 0x7f_fffe..(at & 0x7f_fffe) + 2) {
            half.copy_from_slice(&value.to_be_bytes());
        }
    }

    fn store8(&mut self, at: usize, value: u8) {
        if let Some(byte) = self.rdram.get_mut(at & 0x7f_ffff) {
            *byte = value;
        }
    }
}

/// combine_plan's color channels in one cycle, where the combined input is
/// nothing and texel1 is texel0.
#[inline(always)]
fn combine_rgb(plan: CombinePlan, prim: Color, env: Color, texel: [i32; 3], shade: [i32; 3]) -> [i32; 3] {
    match plan.rgb {
        0 => std::array::from_fn(|n| clamp9(match plan.rgb_source {
            1 | 2 => texel[n],
            3 => prim[n],
            4 => shade[n],
            5 => env[n],
            6 => 256,
            _ => 0,
        })),
        1 | 2 => std::array::from_fn(|n| clamp9((texel[n] * shade[n] + 0x80) >> 8)),
        5 | 6 => std::array::from_fn(|n| clamp9((texel[n] * prim[n] + 0x80) >> 8)),
        _ => std::array::from_fn(|n| clamp9(((prim[n] - shade[n]) * texel[n] + (shade[n] << 8) + 0x80) >> 8)),
    }
}

/// A color register (RGBA8888) as a color.
fn rgba32(c: u32) -> Color {
    [(c >> 24) as i32, (c >> 16 & 0xff) as i32, (c >> 8 & 0xff) as i32, (c & 0xff) as i32]
}

/// The chroma key's centre and scale of each color channel, from Set Key
/// R (key[0]: centre in bits 8-15, scale in 0-7) and Set Key GB (key[1]:
/// green's centre and scale in bits 24-31 and 16-23, blue's in 8-15 and
/// 0-7). The combiner takes them as inputs too (B 6 and C 6).
fn key_center([r, gb]: [u32; 2]) -> Color {
    [(r >> 8 & 0xff) as i32, (gb >> 24) as i32, (gb >> 8 & 0xff) as i32, 0]
}

fn key_scale([r, gb]: [u32; 2]) -> Color {
    [(r & 0xff) as i32, (gb >> 16 & 0xff) as i32, (gb & 0xff) as i32, 0]
}

/// An RGBA5551 texel or palette entry as a color.
fn rgba16(v: u32) -> Color {
    [expand5(v >> 11 & 31), expand5(v >> 6 & 31), expand5(v >> 1 & 31), if v & 1 != 0 { 255 } else { 0 }]
}

/// The depth slope rounded up to a power of two (angrylion,
/// rasterizer.c).
fn normalize_dz(sum: u32) -> u32 {
    if sum & 0xc000 != 0 {
        return 0x8000;
    }
    if sum & 0xffff == 0 {
        return 1;
    }
    if sum == 1 {
        return 3;
    }
    let mut count = 0x2000;
    while count > 0 {
        if sum & count != 0 {
            return count << 1;
        }
        count >>= 1;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp9_table_is_the_branches() {
        for v in -0x2_0000..0x2_0000 {
            assert_eq!(clamp9(v), clamp9_branches(v), "{v}");
        }
    }

    /// A triangle (from Donkey Kong 64) whose right edge goes left of the
    /// scissor box: the lines it covers there draw nothing, rather than a
    /// span to the end of memory.
    #[test]
    fn spans_left_of_the_screen() {
        let mut rom = vec![0u8; 0x1000];
        rom[..4].copy_from_slice(&[0x80, 0x37, 0x12, 0x40]);
        let mut n64 = crate::N64::new(&rom).unwrap();
        n64.rdp.scissor = [80, 80, 2476, 1836];
        n64.rdp.color_image = crate::rdp::Image { format: 0, size: 2, width: 640, addr: 0 };
        n64.rdp.other_modes = 3 << 52;
        n64.rdp.fill_color = 0xffff_ffff;
        n64.raster().triangle(&[0xc880_060d_0540_053c, 0x002a_c000_fffe_a117, 0xffc2_c000_0000_a55a, 0xffc2_c000_0067_ffcc]);
        let lit = |y: usize, x: usize| n64.rdram[(y * 640 + x) * 2] != 0;
        assert!(lit(336, 20) && lit(336, 42) && !lit(336, 43));
        assert!((353..400).all(|y| (0..640).all(|x| !lit(y, x))));
    }

    /// Triangles against the reference rasterizer: edges with gentle and steep slopes (the edges'
    /// 32-bit sums and the 64-bit ones), every attribute, one and two cycles.
    #[test]
    fn triangles_match_the_reference() {
        let mut seed = 0x7a1a_9e1e_5eed_0001;
        // One machine and its reference throughout (the drawing piles up in both), compared every few
        // triangles.
        let mut fast = sampler_fixture();
        fast.set_rdp_acceleration(true);
        fast.set_rdp_profiling(true);
        let mut reference = fast.clone();
        reference.set_rdp_acceleration(false);
        for case in 0..240 {
            let r = sampler_random(&mut seed);
            let size = 2 + (r & 1) as u32;
            let other_modes = sampler_random(&mut seed) & 0x000f_ffff_ffff_ffff & !(3 << 52) | (r >> 1 & 1) << 52;
            let (combine, cycles) = (one_cycle_combiner(&mut seed), sampler_random(&mut seed) >> 20);
            for n64 in [&mut fast, &mut reference] {
                n64.rdp.color_image = crate::rdp::Image { format: 0, size, width: 320, addr: 0x10_0000 };
                n64.rdp.z_image = 0x20_0000;
                n64.rdp.scissor = [0, 0, 320 << 2, 240 << 2];
                n64.rdp.other_modes = other_modes;
                n64.rdp.set_combine(combine);
                n64.cycles = cycles;
            }
            let command = 8 + (r >> 2 & 7);
            let yh = (r >> 5 & 0x3ff) % 900;
            let ym = yh + (r >> 15 & 0x7f);
            let yl = ym + (r >> 22 & 0x7f);
            let steep = |v: u64| if v & 7 == 0 { (v >> 3) as u32 & 0x3fff_ffff } else { (v >> 3) as u32 & 0x7_ffff };
            let slope = |v: u64| if v >> 40 & 1 != 0 { steep(v).wrapping_neg() & 0x3fff_ffff } else { steep(v) };
            let x = |v: u64| ((v >> 20 & 0x1ff) as u32 + 8) << 16 | (v & 0xffff) as u32;
            let edge = |seed: &mut u64| {
                let v = sampler_random(seed);
                u64::from(x(v)) << 32 | u64::from(slope(sampler_random(seed)))
            };
            let mut words = vec![command << 56 | (r >> 29 & 1) << 55 | (r >> 30 & 7) << 48 | yl << 32 | ym << 16 | yh];
            words.extend([edge(&mut seed), edge(&mut seed), edge(&mut seed)]);
            let coefficients = [8, 8, 2].iter().zip([4, 2, 1]).map(|(n, bit)| if command & bit != 0 { *n } else { 0 });
            for _ in 0..coefficients.sum::<u32>() {
                words.push(sampler_random(&mut seed));
            }
            fast.raster().triangle(&words);
            reference.raster().triangle(&words);
            if case % 8 == 7 {
                assert!(fast.rdram == reference.rdram, "triangles to case {case}");
            }
        }
        let drawn = fast.rdp_stats().pixels;
        assert!(drawn > 100_000, "only {drawn} pixels drawn");
    }

    /// Drawing threads' lines: each draws only its own, and together they
    /// draw what one thread does.
    #[test]
    fn lines_split_drawing() {
        let machine = || {
            let mut rom = vec![0u8; 0x1000];
            rom[..4].copy_from_slice(&[0x80, 0x37, 0x12, 0x40]);
            let mut n64 = crate::N64::new(&rom).unwrap();
            n64.rdp.scissor = [80, 80, 2476, 1836];
            n64.rdp.color_image = crate::rdp::Image { format: 0, size: 2, width: 640, addr: 0 };
            n64.rdp.other_modes = 3 << 52;
            n64.rdp.fill_color = 0xffff_ffff;
            n64
        };
        // A triangle (as above) and a fill rectangle from (40, 30) to (200, 100).
        let draws: [&[u64]; 2] = [
            &[0xc880_060d_0540_053c, 0x002a_c000_fffe_a117, 0xffc2_c000_0000_a55a, 0xffc2_c000_0067_ffcc],
            &[0x36 << 56 | 800 << 44 | 400 << 32 | 160 << 12 | 120],
        ];
        let mut whole = machine();
        for words in draws {
            whole.raster().draw(words);
        }
        for every in [2, 3] {
            let mut split = machine();
            for first in 0..every {
                let mut alone = machine();
                for words in draws {
                    for n64 in [&mut split, &mut alone] {
                        let mut raster = n64.raster();
                        raster.lines = Lines { first, every };
                        raster.draw(words);
                    }
                }
                let lit = |y: usize| alone.rdram[y * 1280..(y + 1) * 1280].iter().any(|&b| b != 0);
                assert!((0..480).all(|y| !lit(y) || y as u32 % every == first), "lines of {first} in {every}");
            }
            assert!(split.rdram[..480 * 1280] == whole.rdram[..480 * 1280], "{every} threads draw it all");
        }
    }

    #[test]
    fn depth_format() {
        // The cleared maximum, and values round-tripping at their precision.
        assert_eq!(z_decompress(0xfffc >> 2), 0x3ffff);
        for z in [0, 0x1234, 0x20000, 0x3c123, 0x3f7ff, 0x3ffff] {
            let back = z_decompress(z_compress(z));
            assert!(back <= z && z - back < 64, "{z:x} -> {back:x}");
        }
    }

    #[test]
    fn perspective_by_one() {
        // w of (almost) 1.0 leaves coordinates as they are.
        let (s, t) = perspective(32 << 5, 7 << 5, 0x7fff);
        assert_eq!((s >> 5, t >> 5), (32, 7));
    }

    fn sampler_fixture() -> N64 {
        let mut rom = vec![0; 0x1000];
        rom[..4].copy_from_slice(&0x8037_1240u32.to_be_bytes());
        let mut n64 = N64::new(&rom).unwrap();
        let mut seed = 0x3d64_7e1e_0000_ffff;
        for byte in &mut n64.rdp.tmem {
            *byte = sampler_random(&mut seed) as u8;
        }
        n64
    }

    fn sampler_random(seed: &mut u64) -> u64 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        *seed
    }

    #[test]
    fn three_texel_filter_matches_all_fractions_formats_and_palettes() {
        use savestate::State;
        let mut n64 = sampler_fixture();
        n64.set_rdp_profiling(true);
        let (mut before, mut after) = (Vec::new(), Vec::new());
        for format in 0..8 {
            for size in 0..4 {
                for tlut in 0..3 {
                    let config = format * 12 + size * 3 + tlut;
                    let index = config as usize & 7;
                    n64.rdp.tiles[index] = Tile {
                        format, size, line: 8 * (1 + config % 31), tmem: 0xff8,
                        palette: config & 15, mask: [5, 4], mirror: [true, true],
                        hi: [0xfff; 2], ..Tile::default()
                    };
                    n64.rdp.other_modes = 1 << 45 | u64::from(tlut != 0) << 47 | u64::from(tlut == 2) << 46;
                    before.clear();
                    n64.rdp.save(&mut before);
                    let sbase = [0, 31, 32, -1, -17][config as usize % 5];
                    let tbase = [0, 1, 3, 15, -3][config as usize / 5 % 5];
                    for sf in 0..32 {
                        for tf in 0..32 {
                            let (s, t) = ((sbase << 5) + sf, (tbase << 5) + tf);
                            let expected = n64.raster().sample_reference(index, s, t);
                            assert_eq!(n64.raster().sample(index, s, t), expected,
                                "format {format}, size {size}, TLUT {tlut}, fractions {sf}/{tf}");
                        }
                    }
                    after.clear();
                    n64.rdp.save(&mut after);
                    assert_eq!(before, after, "sampler changed emulated RDP state");
                }
            }
        }
        assert_eq!(n64.rdp_stats().three_texel_samples, 98_304);
    }

    #[test]
    fn three_texel_filter_matches_shift_clamp_wrap_and_point_boundaries() {
        let mut n64 = sampler_fixture();
        n64.set_rdp_profiling(true);
        let mut seed = 0x5a1e_c1a0_ffff_ffff;
        let coords = [-1, -32768, 32767, 0, 1, 31, 32, 0x1ffff, 0x20000, 0x40000, 0x60000, 0x7ffff];
        let mut filtered = 0;
        for case in 0..20_000 {
            let index = sampler_random(&mut seed) as usize & 7;
            let tile = &mut n64.rdp.tiles[index];
            tile.format = sampler_random(&mut seed) as u32 & 7;
            tile.size = sampler_random(&mut seed) as u32 & 3;
            tile.line = (sampler_random(&mut seed) as u32 & 511) * 8;
            tile.tmem = (sampler_random(&mut seed) as u32 & 511) * 8;
            tile.palette = sampler_random(&mut seed) as u32 & 15;
            for axis in 0..2 {
                tile.clamp[axis] = sampler_random(&mut seed) & 1 != 0;
                tile.mirror[axis] = sampler_random(&mut seed) & 1 != 0;
                tile.mask[axis] = sampler_random(&mut seed) as u32 & 15;
                tile.shift[axis] = sampler_random(&mut seed) as u32 & 15;
                tile.lo[axis] = sampler_random(&mut seed) as u32 & 0xfff;
                tile.hi[axis] = sampler_random(&mut seed) as u32 & 0xfff;
            }
            let bilinear = sampler_random(&mut seed) & 1 != 0;
            n64.rdp.other_modes = u64::from(bilinear) << 45 | (sampler_random(&mut seed) & 3) << 46;
            let (s, t) = if case < coords.len() * coords.len() {
                (coords[case % coords.len()], coords[case / coords.len()])
            } else {
                ((sampler_random(&mut seed) & 0x7ffff) as i32, (sampler_random(&mut seed) & 0x7ffff) as i32)
            };
            assert_eq!(n64.raster().sample(index, s, t), n64.raster().sample_reference(index, s, t),
                "case {case}, tile {index}, coords {s:x}/{t:x}");
            filtered += u64::from(bilinear);
        }
        assert_eq!(n64.rdp_stats().three_texel_samples, filtered);
        n64.set_rdp_profiling(false);
        n64.rdp.other_modes |= 1 << 45;
        n64.raster().sample(0, 0, 0);
        assert_eq!(n64.rdp_stats().three_texel_samples, 0);
    }

    #[test]
    fn rdp_preferences_and_counters_survive_machine_restore() {
        let mut fast = sampler_fixture();
        fast.rdp.other_modes = 1 << 45;
        fast.set_rdp_profiling(true);
        let initial = fast.save_state();
        let mut reference = fast.clone();
        reference.set_rdp_acceleration(false);
        assert_eq!(fast.raster().sample(0, 17, 23), reference.raster().sample(0, 17, 23));
        assert_eq!(fast.rdp_stats().three_texel_samples, 1);
        assert_eq!(reference.rdp_stats().three_texel_samples, 0);
        assert!(fast.save_state() == reference.save_state());
        fast.set_rdp_acceleration(false);
        fast.load_state(&initial).unwrap();
        assert!(!fast.rdp.accelerated);
        assert_eq!(fast.rdp_stats().three_texel_samples, 0);
        fast.raster().sample(0, 17, 23);
        assert_eq!(fast.rdp_stats().three_texel_samples, 0);
        fast.set_rdp_acceleration(true);
        fast.load_state(&initial).unwrap();
        reference.load_state(&initial).unwrap();
        assert!(fast.rdp.accelerated);
        assert!(!reference.rdp.accelerated);
        assert_eq!(fast.raster().sample(0, 17, 23), reference.raster().sample(0, 17, 23));
        assert_eq!(fast.rdp_stats().three_texel_samples, 1);
        assert!(fast.save_state() == reference.save_state());
        let state = fast.save_state();
        assert!(fast.load_state(&state[..state.len() - 1]).is_err());
        assert_eq!(fast.rdp_stats().three_texel_samples, 1);
        assert!(fast.save_state() == state);
        fast.set_rdp_profiling(false);
        fast.raster().sample(0, 17, 23);
        assert_eq!(fast.rdp_stats().three_texel_samples, 0);
    }
    #[test]
    fn rgba16_sampler_matches_every_fraction_wrap_and_tlut_mode() {
        let mut n64 = sampler_fixture();
        n64.set_rdp_profiling(true);
        for config in 0..24u32 {
            n64.rdp.tiles[0] = Tile {
                format: 0, size: 2, line: [0, 1, 8, 0xff8][config as usize & 3],
                tmem: [0, 1, 0xff8, 0xfff][(config as usize / 4) & 3],
                mask: [5, 4], mirror: [config & 1 != 0, config & 2 != 0],
                hi: [0xfff; 2], ..Tile::default()
            };
            n64.rdp.other_modes = 1 << 45 | u64::from(config % 3 != 0) << 47 | u64::from(config % 3 == 2) << 46;
            for sf in 0..32 {
                for tf in 0..32 {
                    let (s, t) = (([0, 31, -17][config as usize % 3] << 5) + sf,
                        ([0, 15, -3][config as usize / 3 % 3] << 5) + tf);
                    let expected = n64.raster().sample_reference(0, s, t);
                    assert_eq!(n64.raster().sample(0, s, t), expected, "RGBA16 cfg{config} frac{sf}/{tf}");
                    n64.set_rdp_rgba16_sampling(false);
                    assert_eq!(n64.raster().sample(0, s, t), expected);
                    n64.set_rdp_rgba16_sampling(true);
                }
            }
        }
        assert_eq!(n64.rdp_stats().rgba16_samples, 24_576);
    }

    #[test]
    fn rgba16_sampler_matches_random_geometry_and_restore_preferences() {
        let mut n64 = sampler_fixture();
        n64.set_rdp_profiling(true);
        let mut seed = 0x1644_ffff_0123_4567;
        for _ in 0..30_000 {
            let tile = &mut n64.rdp.tiles[0];
            tile.format = 0; tile.size = 2;
            tile.line = sampler_random(&mut seed) as u32;
            tile.tmem = sampler_random(&mut seed) as u32;
            for axis in 0..2 {
                tile.clamp[axis] = sampler_random(&mut seed) & 1 != 0;
                tile.mirror[axis] = sampler_random(&mut seed) & 1 != 0;
                tile.mask[axis] = sampler_random(&mut seed) as u32 & 15;
                tile.shift[axis] = sampler_random(&mut seed) as u32 & 15;
                tile.lo[axis] = sampler_random(&mut seed) as u32 & 0xfff;
                tile.hi[axis] = sampler_random(&mut seed) as u32 & 0xfff;
            }
            n64.rdp.other_modes = (sampler_random(&mut seed) & 7) << 45;
            let (s, t) = (sampler_random(&mut seed) as i32, sampler_random(&mut seed) as i32);
            assert_eq!(n64.raster().sample(0, s, t), n64.raster().sample_reference(0, s, t));
        }
        let state = n64.save_state();
        n64.set_rdp_rgba16_sampling(false);
        n64.load_state(&state).unwrap();
        assert!(!n64.rdp.rgba16_sampling);
        assert_eq!(n64.rdp_stats().rgba16_samples, 0);
        n64.set_rdp_rgba16_sampling(true);
        n64.load_state(&state).unwrap();
        assert!(n64.rdp.rgba16_sampling);
        let before = n64.save_state();
        n64.raster().sample(0, 0, 0);
        assert_eq!(n64.save_state(), before);
        n64.rdp.other_modes &= !(1 << 45);
        n64.set_rdp_profiling(true);
        n64.raster().sample(0, 0, 0);
        assert_eq!(n64.rdp_stats().rgba16_samples, 1);
        assert_eq!(n64.rdp_stats().three_texel_samples, 0);
        n64.set_rdp_acceleration(false);
        n64.set_rdp_profiling(true);
        n64.raster().sample(0, 0, 0);
        assert_eq!(n64.rdp_stats().rgba16_samples, 0);
        n64.set_rdp_acceleration(true);
        n64.set_rdp_profiling(false);
        n64.raster().sample(0, 0, 0);
        assert_eq!(n64.rdp_stats().rgba16_samples, 0);
    }
    fn memory_blend_modes(selectors: u64, two_cycle: bool, force: bool) -> u64 {
        selectors << 16 | u64::from(two_cycle) << 52 | u64::from(force) << 14
    }

    #[test]
    fn unused_color_read_matches_every_selector_cycle_and_force_combination() {
        let mut n64 = sampler_fixture();
        let mut seed = 0xb1e0_d00d_c010_a1fa;
        let mut skipped = 0;
        for two_cycle in [false, true] {
            for force in [false, true] {
                for selectors in 0..=u16::MAX {
                    let other = memory_blend_modes(u64::from(selectors), two_cycle, force);
                    n64.rdp.other_modes = other;
                    let needed = blender_needs_memory(other, two_cycle);
                    // Independent structural truth table; A never selects
                    // memory, and inactive cycle1 fields must be ignored.
                    let active = if two_cycle { 2 } else { 1 };
                    let expected = (0..active).any(|n| {
                        let [p, m, b] = [30, 22, 18].map(|shift| other >> (shift - n * 2) & 3);
                        let blending = force || two_cycle && n == 0;
                        [p == 1, blending && m == 1, blending && b == 1].contains(&true)
                    });
                    assert_eq!(needed, expected, "selectors {selectors:04x}, two {two_cycle}, force {force}");
                    skipped += u64::from(!needed);
                    for case in 0..4 {
                        n64.rdp.blend = sampler_random(&mut seed) as u32;
                        n64.rdp.fog = sampler_random(&mut seed) as u32;
                        let combined = match case {
                            0 => [0; 4],
                            1 => [255; 4],
                            _ => std::array::from_fn(|_| sampler_random(&mut seed) as u8 as i32),
                        };
                        let memory = match case {
                            0 => [255, 0, 255, 224],
                            1 => [0, 255, 0, 0],
                            _ => {
                                let mut color = std::array::from_fn(|_| sampler_random(&mut seed) as u8 as i32);
                                color[3] &= 0xe0;
                                color
                            }
                        };
                        let shade_alpha = sampler_random(&mut seed) as u8 as i32;
                        let expected = n64.raster().blend(two_cycle, combined, shade_alpha, memory);
                        let substitute = if needed { memory } else { [0; 4] };
                        assert_eq!(n64.raster().blend(two_cycle, combined, shade_alpha, substitute), expected,
                            "selectors {selectors:04x}, two {two_cycle}, force {force}, case {case}");
                    }
                }
            }
        }
        assert_eq!(skipped, 109_200);
    }

    /// A combine mode whose second cycle is a formula the plan covers (or,
    /// one time in four, random selectors), its first cycle random.
    fn one_cycle_combiner(seed: &mut u64) -> u64 {
        let r = sampler_random(seed);
        let mut mode = sampler_random(seed) & 0x00ff_ffff_ffff_ffff;
        if r & 3 != 0 {
            let texture = 1 + (r >> 2 & 1);
            let (a, b, c, d) = match r >> 3 & 3 {
                0 => (r >> 5 & 15, r >> 5 & 15, 6, r >> 9 & 7),
                1 => (texture, 8 + (r >> 12 & 7), 4, 7),
                2 => (4, 8 + (r >> 12 & 7), texture, 7),
                _ => (3, 4, texture, 4),
            };
            let (aa, ab, ac, ad) = match r >> 15 & 3 {
                0 => (r >> 17 & 7, r >> 17 & 7, r >> 20 & 7, r >> 23 & 7),
                1 => (texture, 7, 4, 7),
                2 => (4, 7, texture, 7),
                _ => (3, 4, texture, 4),
            };
            mode &= !(15 << 37 | 15 << 24 | 31 << 32 | 7 << 6 | 7 << 21 | 7 << 3 | 7 << 18 | 7);
            mode |= a << 37 | b << 24 | c << 32 | d << 6 | aa << 21 | ab << 3 | ac << 18 | ad;
        }
        mode
    }

    #[test]
    fn one_cycle_spans_match_the_pixel_pipeline() {
        let formats = [(0, 2), (0, 2), (0, 3), (3, 1), (3, 2), (3, 0), (2, 0), (2, 1), (4, 0), (4, 1), (1, 2)];
        let mut seed = 0x5ba7_1c0d_e000_0001;
        let mut drawn = 0;
        for case in 0..400 {
            let mut fast = sampler_fixture();
            for byte in &mut fast.rdram[..0x10000] {
                *byte = sampler_random(&mut seed) as u8;
            }
            let r = sampler_random(&mut seed);
            let cycle = if case % 8 == 7 { r & 3 } else { 0 };
            fast.rdp.other_modes = sampler_random(&mut seed) & 0x00ff_ffff_ffff_ffff & !(3 << 52) | cycle << 52;
            if r >> 2 & 1 != 0 {
                // Mostly no alpha compare, coverage or forced blending.
                fast.rdp.other_modes &= !(3 | 3 << 12 | 1 << 14);
            }
            fast.rdp.set_combine(one_cycle_combiner(&mut seed));
            for tile in &mut fast.rdp.tiles {
                let (t, odd) = (sampler_random(&mut seed), sampler_random(&mut seed));
                let (format, size) = formats[t as usize % formats.len()];
                *tile = Tile {
                    format, size,
                    // Now and then an odd TMEM address or line, which commands never give.
                    line: (t >> 8 & 63) as u32 * 8 + u32::from(odd & 7 == 0),
                    tmem: (t >> 14 & 0x1ff) as u32 * 8 + u32::from(odd >> 3 & 7 == 0),
                    palette: (t >> 23 & 15) as u32,
                    clamp: [t >> 27 & 1 != 0, t >> 28 & 1 != 0],
                    mirror: [t >> 29 & 1 != 0, t >> 30 & 1 != 0],
                    mask: [(t >> 31 & 15) as u32 % 11, (t >> 35 & 15) as u32 % 11],
                    shift: [(t >> 39 & 15) as u32, (t >> 43 & 15) as u32],
                    lo: [(t >> 47 & 0x3ff) as u32, (t >> 50 & 0x3ff) as u32],
                    hi: [(t >> 53 & 0x7ff) as u32, (t >> 40 & 0x7ff) as u32],
                };
            }
            let size = if r >> 3 & 7 == 0 { (r >> 6 & 3) as u32 } else { 2 + (r >> 6 & 1) as u32 };
            let width = 8 + (r >> 8 & 0xff) as u32;
            fast.rdp.color_image = crate::rdp::Image { format: 0, size, width, addr: (r >> 16 & 0x3ffe) as u32 };
            fast.rdp.z_image = (r >> 30 & 0x7ffe) as u32;
            fast.rdp.prim_z = (r >> 45 & 0x7fff) as u32;
            fast.rdp.blend = sampler_random(&mut seed) as u32;
            fast.rdp.fog = sampler_random(&mut seed) as u32;
            fast.rdp.prim = sampler_random(&mut seed) as u32;
            fast.rdp.env = sampler_random(&mut seed) as u32;
            fast.cycles = sampler_random(&mut seed) >> 20;
            fast.set_rdp_acceleration(true);
            fast.set_rdp_profiling(true);
            let mut reference = fast.clone();
            reference.set_rdp_span_acceleration(false);
            let mut attrs = Attrs::default();
            let mut dx = Attrs::default();
            for c in 0..8 {
                let v = sampler_random(&mut seed);
                attrs.v[c] = v as i32 >> (v >> 40 & 7);
                dx.v[c] = (v >> 32) as i32 >> (8 + (v >> 60 & 7));
            }
            if r >> 50 & 1 != 0 {
                attrs.v[W] = 0x7fff << 16 | attrs.v[W] & 0xffff;
            }
            let prim = Primitive { tile: (r >> 51 & 7) as usize, texture: r >> 54 & 3 != 0, persp: r >> 56 & 1 != 0,
                dz: 1 << (r >> 57 & 15) };
            let y = (r >> 61) as u32;
            let x0 = (sampler_random(&mut seed) % u64::from(width)) as u32;
            let x1 = (x0 + 1 + (sampler_random(&mut seed) % 40) as u32).min(width);
            fast.raster().draw_span(y, x0, x1, attrs, &dx, &prim);
            reference.raster().draw_span(y, x0, x1, attrs, &dx, &prim);
            assert!(fast.save_state() == reference.save_state(), "span case {case}");
            let (f, r) = (fast.rdp_stats(), reference.rdp_stats());
            assert_eq!((f.three_texel_samples, f.rgba16_samples, f.skipped_color_reads),
                (r.three_texel_samples, r.rgba16_samples, r.skipped_color_reads), "span case {case} counters");
            assert_eq!(r.fast_span_pixels, 0);
            drawn += f.fast_span_pixels;
        }
        assert!(drawn > 2000, "only {drawn} pixels took the specialized span");
    }

    /// span_cycles and span_copy: one- and two-cycle spans with any combiner in either cycle
    /// (planned or not), either palette, any blender, depth and coverage modes, and copy-mode
    /// spans, against pixel() and copy_pixel().
    #[test]
    fn two_cycle_spans_match_the_pixel_pipeline() {
        let formats = [(0, 2), (0, 3), (3, 1), (3, 2), (3, 0), (2, 0), (2, 1), (4, 0), (4, 1), (1, 2), (0, 0)];
        let mut seed = 0x2c1c_1e55_9a7e_0001;
        let mut drawn = 0;
        for case in 0..600 {
            let mut fast = sampler_fixture();
            for byte in &mut fast.rdram[..0x10000] {
                *byte = sampler_random(&mut seed) as u8;
            }
            let r = sampler_random(&mut seed);
            let cycle = [0, 1, 1, COPY as u64][case % 4];
            fast.rdp.other_modes = sampler_random(&mut seed) & 0x00ff_ffff_ffff_ffff & !(3 << 52) | cycle << 52;
            // Either cycle planned or not: random, or the formulas games draw with.
            let combine = if r & 1 != 0 { one_cycle_combiner(&mut seed) } else { sampler_random(&mut seed) };
            let first = one_cycle_combiner(&mut seed);
            let mask0 = 15 << 52 | 31 << 47 | 7 << 44 | 7 << 41 | 15 << 28 | 7 << 15 | 7 << 12 | 7 << 9;
            let combine = if r >> 1 & 1 != 0 { combine & !mask0 | first >> 15 << 15 & mask0 } else { combine };
            fast.rdp.set_combine(combine & 0x00ff_ffff_ffff_ffff);
            for tile in &mut fast.rdp.tiles {
                let (t, odd) = (sampler_random(&mut seed), sampler_random(&mut seed));
                let (format, size) = formats[t as usize % formats.len()];
                *tile = Tile {
                    format, size,
                    // Now and then an odd TMEM address or line, which commands never give.
                    line: (t >> 8 & 63) as u32 * 8 + u32::from(odd & 7 == 0),
                    tmem: (t >> 14 & 0x1ff) as u32 * 8 + u32::from(odd >> 3 & 7 == 0),
                    palette: (t >> 23 & 15) as u32,
                    clamp: [t >> 27 & 1 != 0, t >> 28 & 1 != 0],
                    mirror: [t >> 29 & 1 != 0, t >> 30 & 1 != 0],
                    mask: [(t >> 31 & 15) as u32 % 11, (t >> 35 & 15) as u32 % 11],
                    shift: [(t >> 39 & 15) as u32, (t >> 43 & 15) as u32],
                    lo: [(t >> 47 & 0x3ff) as u32, (t >> 50 & 0x3ff) as u32],
                    hi: [(t >> 53 & 0x7ff) as u32, (t >> 40 & 0x7ff) as u32],
                };
            }
            // Often texel 1's tile as texel 0's but for its place in TMEM and palette (both texels
            // through one sampler).
            if r >> 58 & 1 != 0 {
                let (first, t) = ((r >> 51 & 7) as usize, sampler_random(&mut seed));
                let tile = fast.rdp.tiles[first];
                fast.rdp.tiles[(first + 1) & 7] = Tile {
                    line: (t >> 8 & 63) as u32 * 8, tmem: (t >> 14 & 0x1ff) as u32 * 8, palette: (t >> 23 & 15) as u32,
                    size: if t & 1 != 0 && tile.size < 2 { tile.size ^ 1 } else { tile.size }, ..tile
                };
            }
            let size = if r >> 3 & 7 == 0 { (r >> 6 & 3) as u32 } else { 2 + (r >> 6 & 1) as u32 };
            let width = 8 + (r >> 8 & 0xff) as u32;
            fast.rdp.color_image = crate::rdp::Image { format: 0, size, width, addr: (r >> 16 & 0x3ffe) as u32 };
            fast.rdp.z_image = (r >> 30 & 0x7ffe) as u32;
            fast.rdp.prim_z = (r >> 45 & 0x7fff) as u32;
            fast.rdp.blend = sampler_random(&mut seed) as u32;
            fast.rdp.fog = sampler_random(&mut seed) as u32;
            fast.rdp.prim = sampler_random(&mut seed) as u32;
            fast.rdp.env = sampler_random(&mut seed) as u32;
            fast.rdp.key = [sampler_random(&mut seed) as u32, sampler_random(&mut seed) as u32];
            fast.rdp.convert = std::array::from_fn(|_| (sampler_random(&mut seed) & 0x1ff) as i32 - 0x100);
            fast.rdp.prim_lod_frac = (sampler_random(&mut seed) & 0xff) as u32;
            fast.cycles = sampler_random(&mut seed) >> 20;
            fast.set_rdp_acceleration(true);
            fast.set_rdp_profiling(true);
            let mut reference = fast.clone();
            reference.set_rdp_span_acceleration(false);
            let mut attrs = Attrs::default();
            let mut dx = Attrs::default();
            for c in 0..8 {
                let v = sampler_random(&mut seed);
                attrs.v[c] = v as i32 >> (v >> 40 & 7);
                dx.v[c] = (v >> 32) as i32 >> (8 + (v >> 60 & 7));
            }
            let prim = Primitive { tile: (r >> 51 & 7) as usize, texture: r >> 54 & 3 != 0, persp: r >> 56 & 1 != 0,
                dz: 1 << (r >> 57 & 15) };
            let y = (r >> 61) as u32;
            let x0 = (sampler_random(&mut seed) % u64::from(width)) as u32;
            let x1 = (x0 + 1 + (sampler_random(&mut seed) % 40) as u32).min(width);
            fast.raster().draw_span(y, x0, x1, attrs, &dx, &prim);
            reference.raster().draw_span(y, x0, x1, attrs, &dx, &prim);
            assert!(fast.save_state() == reference.save_state(), "span case {case}");
            let (f, r) = (fast.rdp_stats(), reference.rdp_stats());
            assert_eq!((f.three_texel_samples, f.rgba16_samples, f.skipped_color_reads),
                (r.three_texel_samples, r.rgba16_samples, r.skipped_color_reads), "span case {case} counters");
            drawn += f.fast_span_pixels;
        }
        assert!(drawn > 4000, "only {drawn} pixels took the specialized spans");
    }

    /// span_opaque's case only (perspective, bilinear RGBA16, IA16, IA8, I4,
    /// I8 or through the palette, a 16-bit image the blender only writes),
    /// in every depth mode, with and without the force blend that gives the
    /// combined color back, against pixel().
    #[test]
    fn opaque_spans_match_the_pixel_pipeline() {
        let mut seed = 0x0a9a_9e5a_7ba5_1ce5;
        let mut drawn = 0;
        for case in 0..900 {
            let mut fast = sampler_fixture();
            for byte in &mut fast.rdram[..0x10000] {
                *byte = sampler_random(&mut seed) as u8;
            }
            let r = sampler_random(&mut seed);
            // One cycle, bilinear; no alpha compare, primitive depth,
            // coverage times alpha, force blend or memory color.
            fast.rdp.other_modes = sampler_random(&mut seed) & 0x000f_ffff_ffff_ffff
                & !(1 | 1 << 2 | 1 << 12 | 1 << 14 | 3 << 30) | 1 << 45;
            if r >> 4 & 1 != 0 {
                // a 0, m the combined color, b 1.
                fast.rdp.other_modes = fast.rdp.other_modes & !(3 << 26 | 3 << 22 | 3 << 18) | 1 << 14 | 3 << 26 | 2 << 18;
            } else if r >> 13 & 3 == 0 {
                // The combined color over memory by its alpha (alpha from
                // coverage sometimes).
                fast.rdp.other_modes = fast.rdp.other_modes & !(3 << 26 | 3 << 22 | 3 << 18) | 1 << 14 | 1 << 22;
            } else if r >> 5 & 1 != 0 {
                // Other forced blends without memory, which span_opaque leaves.
                let (a, m, b) = (r >> 6 & 3, [0, 2, 3][(r >> 8) as usize % 3], [0, 2, 3][(r >> 10) as usize % 3]);
                fast.rdp.other_modes = fast.rdp.other_modes & !(3 << 26 | 3 << 22 | 3 << 18) | 1 << 14 | a << 26 | m << 22 | b << 18;
            }
            fast.rdp.set_combine(one_cycle_combiner(&mut seed));
            for tile in &mut fast.rdp.tiles {
                let (t, odd) = (sampler_random(&mut seed), sampler_random(&mut seed));
                let (format, size) = [(0, 2), (3, 2), (3, 1), (0, 2), (4, 0), (4, 1), (2, 0)][(t >> 2) as usize % 7];
                *tile = Tile {
                    format, size,
                    // Now and then an odd TMEM address or line, which commands never give.
                    line: (t >> 8 & 63) as u32 * 8 + u32::from(odd & 7 == 0),
                    tmem: (t >> 14 & 0x1ff) as u32 * 8 + u32::from(odd >> 3 & 7 == 0),
                    palette: (t >> 23 & 15) as u32,
                    clamp: [t >> 27 & 1 != 0, t >> 28 & 1 != 0],
                    mirror: [t >> 29 & 1 != 0, t >> 30 & 1 != 0],
                    mask: [(t >> 31 & 15) as u32 % 11, (t >> 35 & 15) as u32 % 11],
                    shift: [(t >> 39 & 15) as u32, (t >> 43 & 15) as u32],
                    lo: [(t >> 47 & 0x3ff) as u32, (t >> 50 & 0x3ff) as u32],
                    hi: [(t >> 53 & 0x7ff) as u32, (t >> 40 & 0x7ff) as u32],
                };
            }
            let width = 8 + (r >> 8 & 0xff) as u32;
            fast.rdp.color_image = crate::rdp::Image { format: 0, size: 2, width, addr: (r >> 16 & 0x3ffe) as u32 };
            fast.rdp.z_image = (r >> 30 & 0x7ffe) as u32;
            fast.rdp.blend = sampler_random(&mut seed) as u32;
            fast.rdp.fog = sampler_random(&mut seed) as u32;
            fast.rdp.prim = sampler_random(&mut seed) as u32;
            fast.rdp.env = sampler_random(&mut seed) as u32;
            fast.cycles = sampler_random(&mut seed) >> 20;
            fast.set_rdp_acceleration(true);
            fast.set_rdp_profiling(true);
            let mut reference = fast.clone();
            reference.set_rdp_span_acceleration(false);
            let mut attrs = Attrs::default();
            let mut dx = Attrs::default();
            for c in 0..8 {
                let v = sampler_random(&mut seed);
                attrs.v[c] = v as i32 >> (v >> 40 & 7);
                dx.v[c] = (v >> 32) as i32 >> (8 + (v >> 60 & 7));
            }
            if r >> 50 & 1 != 0 {
                attrs.v[W] = 0x7fff << 16 | attrs.v[W] & 0xffff;
            }
            let prim = Primitive { tile: (r >> 51 & 7) as usize, texture: r >> 12 & 3 != 0, persp: true,
                dz: 1 << (r >> 57 & 15) };
            let y = (r >> 61) as u32;
            let x0 = (sampler_random(&mut seed) % u64::from(width)) as u32;
            let x1 = (x0 + 1 + (sampler_random(&mut seed) % 40) as u32).min(width);
            fast.raster().draw_span(y, x0, x1, attrs, &dx, &prim);
            reference.raster().draw_span(y, x0, x1, attrs, &dx, &prim);
            assert!(fast.save_state() == reference.save_state(), "opaque span case {case}");
            let (f, r) = (fast.rdp_stats(), reference.rdp_stats());
            assert_eq!((f.three_texel_samples, f.rgba16_samples, f.skipped_color_reads),
                (r.three_texel_samples, r.rgba16_samples, r.skipped_color_reads), "opaque span case {case} counters");
            drawn += f.fast_span_pixels;
        }
        assert!(drawn > 4000, "only {drawn} pixels took the specialized span");
    }

    /// Fill rectangles' rows four bytes at a time against the pixel loop: every image size, odd and
    /// even first pixels and addresses, rows that run past RDRAM's end.
    #[test]
    fn fill_rows_match_the_pixel_loop() {
        let mut seed = 0xf111_7e57_0000_0001;
        // One machine and its reference throughout, compared every few rectangles.
        let mut fast = sampler_fixture();
        let mut reference = fast.clone();
        reference.set_rdp_acceleration(false);
        for case in 0..400 {
            let r = sampler_random(&mut seed);
            let size = (r & 3) as u32;
            let width = 1 + (r >> 2 & 0x1ff) as u32;
            let addr = if case % 5 == 0 { 0x7f_ff00 + (r >> 12 & 0xff) as u32 } else { (r >> 12 & 0xffff) as u32 };
            let x0 = (sampler_random(&mut seed) % u64::from(width)) as u32;
            let x1 = (x0 + (sampler_random(&mut seed) % 70) as u32).min(width + 3);
            let (y0, y1) = ((r >> 30 & 3) as u32, (r >> 30 & 3) as u32 + 1 + (r >> 33 & 3) as u32);
            for n64 in [&mut fast, &mut reference] {
                n64.rdp.color_image = crate::rdp::Image { format: 0, size, width, addr };
                n64.rdp.fill_color = (r >> 32) as u32;
                n64.raster().fill_span_rect(x0, y0, x1, y1);
            }
            if case % 16 == 15 {
                assert!(fast.rdram == reference.rdram, "fills to case {case}");
            }
        }
    }

    fn memory_shade_combiner() -> u64 {
        4 << 15 | 4 << 9 | 4 << 6 | 4
    }

    #[test]
    fn unused_color_read_pixels_match_full_machine_at_depth_alpha_and_address_edges() {
        let mut fast = sampler_fixture();
        let mut reference = fast.clone();
        let mut seed = 0x7fff_ffff_b1e0_d001;
        let bases = [0, 1, 0x7f_fffc, 0x7f_fffe, 0x7f_ffff, 0x80_0000, 0xff_fffc, 0xffff_ff00];
        let alphas = [0, 1, 31, 32, 127, 224, 254, 255];
        let depths = [0, 1, 0x1234, 0x20000, 0x3fffe, 0x3ffff];
        let mut total_skipped = 0;
        for case in 0..128 {
            let size = (case / 8 % 4) as u32;
            let x = (case / 8 % 4) as u32;
            let y = (case / 32 % 2) as u32;
            let i = (y * 8 + x) as usize;
            let base = bases[case % bases.len()];
            let selectors = if case % 4 == 0 { 0 } else { sampler_random(&mut seed) & 0xffff };
            let two_cycle = case & 1 != 0;
            let force = case & 2 != 0;
            let zmode = (case / 4 % 4) as u64;
            fast.rdp.other_modes = memory_blend_modes(selectors, two_cycle, force)
                | zmode << 10 | u64::from(case & 4 != 0) << 4 | u64::from(case & 8 != 0) << 5
                | u64::from(case & 16 != 0) << 2 | (case / 16 % 4) as u64
                | u64::from(case & 32 != 0) << 12 | u64::from(case & 64 != 0) << 13
                | u64::from(case % 7 == 0) << 45;
            fast.rdp.set_combine(memory_shade_combiner());
            fast.rdp.color_image = crate::rdp::Image { format: 0, size, width: 8, addr: base };
            fast.rdp.z_image = if case % 5 == 0 { base } else { 0x400 };
            fast.rdp.prim_z = depths[case % depths.len()] >> 3;
            fast.rdp.blend = sampler_random(&mut seed) as u32;
            fast.rdp.fog = sampler_random(&mut seed) as u32;
            fast.cycles = case as u64 * 17;
            reference.rdp = fast.rdp.clone();
            reference.cycles = fast.cycles;
            fast.set_rdp_acceleration(true);
            reference.set_rdp_acceleration(true);
            reference.set_rdp_memory_acceleration(false);
            fast.set_rdp_profiling(true);
            reference.set_rdp_profiling(true);
            let color_at = (base as usize + i * if size == 3 { 4 } else { 2 }) & 0x7f_fffc;
            let z_at = (fast.rdp.z_image as usize + i * 2) & 0x7f_fffe;
            let color = (sampler_random(&mut seed) as u32).to_be_bytes();
            let z = (z_compress(depths[(case + 1) % depths.len()]) << 2 | (case & 3) as u16).to_be_bytes();
            for machine in [&mut fast, &mut reference] {
                machine.rdram[color_at..color_at + 4].copy_from_slice(&color);
                machine.rdram[z_at..z_at + 2].copy_from_slice(&z);
            }
            let mut attrs = Attrs::default();
            for c in 0..3 { attrs.v[c] = (sampler_random(&mut seed) as u8 as i32) << 16; }
            attrs.v[3] = alphas[case % alphas.len()] << 16;
            attrs.v[Z] = (depths[(case + 2) % depths.len()] << 13) as i32;
            attrs.v[S] = ((case as i32 & 31) << 5 | 17) << 16;
            attrs.v[T] = ((case as i32 & 15) << 5 | 23) << 16;
            attrs.v[W] = 0x7fff << 16;
            let prim = Primitive { tile: case & 7, texture: case % 7 == 0, persp: case % 3 == 0,
                dz: 1 << (case % 16) };
            fast.raster().draw_span(y, x, x + 1, attrs, &Attrs::default(), &prim);
            reference.raster().draw_span(y, x, x + 1, attrs, &Attrs::default(), &prim);
            total_skipped += fast.rdp_stats().skipped_color_reads;
            assert_eq!(reference.rdp_stats().skipped_color_reads, 0);
            assert!(fast.save_state() == reference.save_state(), "pixel edge case {case}");
        }
        assert!(total_skipped > 0);
    }

    #[test]
    fn unused_color_read_counts_real_paths_and_preserves_restore_preferences() {
        let mut fast = sampler_fixture();
        fast.rdp.color_image = crate::rdp::Image { format: 0, size: 3, width: 1, addr: 0 };
        fast.rdp.set_combine(memory_shade_combiner());
        fast.rdp.z_image = 0x100;
        let attrs = Attrs { v: [128 << 16, 64 << 16, 192 << 16, 255 << 16, 0, 0, 0, 0x10000 << 13] };
        let prim = Primitive { tile: 0, texture: false, persp: false, dz: 1 };
        fast.set_rdp_profiling(true);
        fast.raster().draw_span(0, 0, 1, attrs, &Attrs::default(), &prim);
        assert_eq!(fast.rdp_stats().skipped_color_reads, 1);
        // P always needs memory; M/B need it only while actually blending.
        for mode in [1 << 30, 1 << 22 | 1 << 14, 1 << 18 | 1 << 14,
            1 << 52 | 1 << 22, 1 << 52 | 1 << 28] {
            fast.rdp.other_modes = mode;
            fast.raster().draw_span(0, 0, 1, attrs, &Attrs::default(), &prim);
            assert_eq!(fast.rdp_stats().skipped_color_reads, 1);
        }
        fast.rdp.other_modes = 1 << 22 | 1 << 18;
        fast.raster().draw_span(0, 0, 1, attrs, &Attrs::default(), &prim);
        assert_eq!(fast.rdp_stats().skipped_color_reads, 2);
        // Rejection happens before any skipped read is counted.
        fast.rdp.other_modes = 1 << 4;
        fast.rdram[0x100..0x102].fill(0);
        fast.raster().draw_span(0, 0, 1, attrs, &Attrs::default(), &prim);
        assert_eq!(fast.rdp_stats().skipped_color_reads, 2);
        let mut transparent = attrs;
        transparent.v[3] = 0;
        fast.rdp.other_modes = 1;
        fast.rdp.blend = 255;
        fast.raster().draw_span(0, 0, 1, transparent, &Attrs::default(), &prim);
        assert_eq!(fast.rdp_stats().skipped_color_reads, 2);
        fast.rdp.other_modes = 1 << 12;
        transparent.v[3] = 31 << 16;
        fast.raster().draw_span(0, 0, 1, transparent, &Attrs::default(), &prim);
        assert_eq!(fast.rdp_stats().skipped_color_reads, 2);
        let state = fast.save_state();
        assert!(fast.load_state(&state[..state.len() - 1]).is_err());
        assert_eq!(fast.rdp_stats().skipped_color_reads, 2);
        assert!(fast.save_state() == state);
        fast.set_rdp_acceleration(false);
        fast.load_state(&state).unwrap();
        assert!(!fast.rdp.accelerated);
        assert!(fast.rdp.profiling);
        assert_eq!(fast.rdp_stats().skipped_color_reads, 0);
        fast.rdp.other_modes = 0;
        fast.raster().draw_span(0, 0, 1, attrs, &Attrs::default(), &prim);
        assert_eq!(fast.rdp_stats().skipped_color_reads, 0);
        fast.set_rdp_acceleration(true);
        fast.raster().draw_span(0, 0, 1, attrs, &Attrs::default(), &prim);
        assert_eq!(fast.rdp_stats().skipped_color_reads, 1);
        fast.set_rdp_profiling(false);
        fast.raster().draw_span(0, 0, 1, attrs, &Attrs::default(), &prim);
        assert_eq!(fast.rdp_stats().skipped_color_reads, 0);
    }

    /// Same binary and pixel pipeline; no textures, so this isolates
    /// framebuffer-read elision from the separate three-texel sampler.
    #[test]
    #[ignore]
    fn unused_color_read_instruction_cost() {
        let mode = std::env::var("N64_RDP_MEMORY_MODE").unwrap_or_else(|_| "fast".into());
        let case = std::env::var("N64_RDP_MEMORY_CASE").unwrap_or_else(|_| "pass".into());
        let count: u32 = std::env::var("N64_RDP_MEMORY_COUNT").unwrap_or_else(|_| "200000".into()).parse().unwrap();
        let mut n64 = sampler_fixture();
        n64.set_rdp_memory_acceleration(mode == "fast");
        n64.rdp.color_image = crate::rdp::Image { format: 0, size: 2, width: 64, addr: 0 };
        n64.rdp.set_combine(memory_shade_combiner());
        n64.rdp.other_modes = match case.as_str() {
            "pass" => 0,
            "forced" => 3 << 26 | 2 << 18 | 1 << 14,
            "required" => 1 << 30,
            _ => panic!("unknown case"),
        };
        let prim = Primitive { tile: 0, texture: false, persp: false, dz: 1 };
        let mut attrs = Attrs::default();
        let dx = Attrs { v: [31 << 16, 17 << 16, 47 << 16, 0, 0, 0, 0, 0] };
        for first in (0..count).step_by(64) {
            attrs.v[0] = ((first as i32 * 31) & 255) << 16;
            attrs.v[1] = ((first as i32 * 17) & 255) << 16;
            attrs.v[2] = ((first as i32 * 47) & 255) << 16;
            attrs.v[3] = 255 << 16;
            n64.raster().draw_span(first >> 6 & 63, 0, (count - first).min(64),
                std::hint::black_box(attrs), &dx, &prim);
            std::hint::black_box(&n64);
        }
        let hash = n64.save_state().into_iter().fold(0xcbf29ce484222325u64,
            |h, b| (h ^ u64::from(b)).wrapping_mul(0x100000001b3));
        eprintln!("memory {case} {mode}: {count}, state hash {hash:016x}");
    }

    #[test]
    fn rdp_memory_preference_is_independent_and_preserved_by_restore() {
        let mut n64 = sampler_fixture();
        n64.rdp.color_image = crate::rdp::Image { format: 0, size: 3, width: 1, addr: 0 };
        n64.rdp.set_combine(memory_shade_combiner());
        let attrs = Attrs { v: [128 << 16, 64 << 16, 192 << 16, 255 << 16, 0, 0, 0, 0] };
        let prim = Primitive { tile: 0, texture: false, persp: false, dz: 1 };
        for master in [false, true] {
            for component in [false, true] {
                n64.set_rdp_acceleration(master);
                n64.set_rdp_memory_acceleration(component);
                n64.set_rdp_profiling(true);
                assert_eq!(n64.rdp.accelerated, master);
                assert!(n64.rdp.rgba16_sampling);
                n64.raster().draw_span(0, 0, 1, attrs, &Attrs::default(), &prim);
                assert_eq!(n64.rdp_stats().skipped_color_reads, u64::from(master && component));
                let state = n64.save_state();
                n64.set_rdp_memory_acceleration(!component);
                assert!(n64.save_state() == state);
                n64.load_state(&state).unwrap();
                assert_eq!(n64.rdp.accelerated, master);
                assert_eq!(n64.rdp.lazy_color_reads, !component);
                assert!(n64.rdp.profiling);
                assert_eq!(n64.rdp_stats().skipped_color_reads, 0);
                assert!(n64.save_state() == state);
            }
        }
    }

}

#[cfg(test)]
mod combiner_tests {
    use super::*;

    fn random(seed: &mut u64) -> u64 {
        *seed ^= *seed << 13; *seed ^= *seed >> 7; *seed ^= *seed << 17; *seed
    }

    fn machine() -> N64 {
        let mut rom = vec![0u8; 0x1000];
        rom[..4].copy_from_slice(&[0x80, 0x37, 0x12, 0x40]);
        N64::new(&rom).unwrap()
    }

    fn inputs(n64: &mut N64, seed: &mut u64) -> [Color; 4] {
        n64.cycles = random(seed);
        n64.rdp.prim = random(seed) as u32;
        n64.rdp.env = random(seed) as u32;
        n64.rdp.prim_lod_frac = random(seed) as u32 & 255;
        n64.rdp.key = [random(seed) as u32, random(seed) as u32];
        n64.rdp.convert = std::array::from_fn(|_| (random(seed) & 511) as i32 - 256);
        std::array::from_fn(|_| std::array::from_fn(|_| {
            let value = random(seed);
            if value & 3 == 0 {
                [-1024, -513, -512, -257, -256, -129, -128, -1, 0, 1, 127, 128, 255, 256, 383, 384, 511, 512, 1023][(value >> 2) as usize % 19]
            } else { ((value >> 2) & 1023) as i32 - 512 }
        }))
    }

    fn mode(mut original: u64, cycle: usize, rgb: [u64; 4], alpha: [u64; 4]) -> u64 {
        let fields = if cycle == 0 {
            [(52,4),(28,4),(47,5),(15,3),(44,3),(12,3),(41,3),(9,3)]
        } else { [(37,4),(24,4),(32,5),(6,3),(21,3),(3,3),(18,3),(0,3)] };
        for ((shift, width), value) in fields.into_iter().zip(rgb.into_iter().chain(alpha)) {
            let mask = ((1u64 << width) - 1) << shift;
            original = original & !mask | (value << shift & mask);
        }
        original
    }

    fn same(n64: &mut N64, cycle: usize, values: [Color; 4], require_fast: bool) {
        let [combined, t0, t1, shade] = values;
        let reference = n64.raster().combine_reference(cycle, combined, t0, t1, shade);
        let plan = n64.rdp.combine_program[cycle];
        let specialized = plan.map(|plan| n64.raster().combine_specialized(plan, combined, t0, t1, shade));
        if require_fast { assert!(specialized.is_some(), "expected specialization {:016x}/{cycle}", n64.rdp.combine); }
        if let Some(actual) = specialized {
            assert_eq!(actual, reference, "specialization {:016x}/{cycle}: {values:?}", n64.rdp.combine);
        }
        assert_eq!(n64.raster().combine(cycle, combined, t0, t1, shade), reference,
            "dispatch {:016x}/{cycle}: {values:?}", n64.rdp.combine);
    }

    #[test]
    fn recognized_selector_shapes_match_random9bit_and_out_of_range_inputs() {
        let mut n64 = machine();
        let mut seed = 0x9b17_cafe_c011_1280;
        for cycle in 0..2 {
            for n in 0..100_000 {
                let variant = random(&mut seed);
                let texture = 1 + (variant >> 4 & 1);
                let zero = 8 + (variant >> 8 & 7);
                let rgb = match n % 9 {
                    0 => [variant & 15, variant >> 12 & 15, 16 + (variant >> 16 & 15), variant >> 20 & 7],
                    1 => {let a=variant%6; [a,a,variant>>16&31,variant>>20&7]},
                    2 => [zero,8+(variant>>12&7),variant>>16&31,variant>>20&7],
                    3 => [texture,zero,4,7],
                    4 => [4,zero,texture,7],
                    5 => [texture,zero,3,7],
                    6 => [3,zero,texture,7],
                    _ => [3,4,texture,4],
                };
                let alpha = match n % 7 {
                    0 => [variant>>3&7,variant>>9&7,7,variant>>15&7],
                    1 => {let a=variant>>4&7; [a,a,variant>>12&7,variant>>18&7]},
                    2 => [texture,7,4,7],
                    3 => [4,7,texture,7],
                    4 => [texture,7,3,7],
                    5 => [3,7,texture,7],
                    _ => [3,4,texture,4],
                };
                n64.rdp.set_combine(mode(random(&mut seed), cycle, rgb, alpha));
                let values=inputs(&mut n64,&mut seed);
                same(&mut n64,cycle,values,true);
            }
        }
    }

    #[test]
    fn every_mode_bit_mutation_keeps_the_exact_reference_or_falls_back() {
        let mut n64 = machine();
        let mut seed = 0x6410_0000_ffff_ffff;
        let modes = [0x00ff_ffff_fffc_f87c, 0x0030_b261_4466_4924, 0x00ff_ffff_fffc_f279,
            0x0012_7e24_ffff_f9fc, 0x00ff_ffff_fffe_7b3d, 0x0012_1824_ff33_ffff,
            0x00ff_ffff_fffe_793c, 0x0014_7e28_44fe_7b3d];
        for original in modes {
            for cycle in 0..2 {
                for bit in 0..65 {
                    n64.rdp.set_combine(if bit==64 {original} else {original ^ (1<<bit)});
                    for _ in 0..128 {
                        let values=inputs(&mut n64,&mut seed);
                        same(&mut n64,cycle,values,false);
                    }
                }
            }
        }
    }

    #[test]
    fn random_all64_mode_bits_and_both_cycles_match_reference() {
        let mut n64 = machine();
        let mut seed = 0xf011_bacc_9b17_0001;
        let mut hits=0;
        for n in 0..100_000 {
            n64.rdp.set_combine(random(&mut seed));
            let values=inputs(&mut n64,&mut seed);
            if n64.rdp.combine_program[n&1].is_some(){hits+=1;}
            same(&mut n64,n&1,values,false);
        }
        assert!(hits>1000,"random recognized cases {hits}");
    }

    #[test]
    fn multiply_rounding_and_direct9bit_clamp_are_preserved() {
        let mut n64=machine();
        for cycle in 0..2 {
            n64.rdp.set_combine(mode(0,cycle,[1,15,4,7],[1,7,4,7]));
            let value=n64.raster().combine(cycle,[0;4],[255;4],[0;4],[255;4]);
            assert_eq!(value,[254;4]);
            for input in -2048..2048 {
                n64.rdp.set_combine(mode(0,cycle,[15,15,31,0],[7,7,7,0]));
                assert_eq!(n64.raster().combine(cycle,[input;4],[0;4],[0;4],[0;4]),[clamp9(input);4]);
            }
        }
    }

    #[test]
    fn reference_preference_is_host_only_and_survives_restore() {
        let mut n64=machine();
        let bytes=n64.save_state();
        n64.set_rdp_combiner_acceleration(false);
        assert_eq!(n64.save_state(),bytes);
        n64.load_state(&bytes).unwrap();
        assert!(!n64.rdp.fast_combiner);
    }

    #[test]
    fn set_combine_command_rebuilds_both_cycle_programs() {
        let mut n64=machine();
        assert_eq!(n64.rdp.combine_program, std::array::from_fn(|c| classify_combiner(0,c)));
        for mode in [0x127e24fffff9fc,0x147e2844fe7b3d,0x30b26144664924,0] {
            let command=(0x3cu64<<56)|mode;
            n64.rdram[..8].copy_from_slice(&command.to_be_bytes());
            n64.write_dp(0,0);
            n64.write_dp(4,8);
            assert_eq!(n64.rdp.combine,mode);
            assert_eq!(n64.rdp.combine_program,std::array::from_fn(|cycle|classify_combiner(mode,cycle)));
            let values=[[97,16,255,128],[255,17,128,255],[0,11,97,0],[255,128,0,255]];
            for cycle in 0..2 {same(&mut n64,cycle,values,false);}
        }
    }

    #[test]
    fn successful_restore_rebuilds_programs_from_saved_mode_and_failed_restore_preserves_them() {
        let mut n64=machine();
        for saved_mode in [0x127e24fffff9fc,0x147e2844fe7b3d,0x30b26144664924] {
            n64.rdp.set_combine(saved_mode);
            let saved=n64.save_state();
            let expected=n64.rdp.combine_program;
            // A stale derived cache must neither be saved nor survive restore.
            n64.rdp.combine_program=if expected==[None;2] {
                std::array::from_fn(|cycle|classify_combiner(0,cycle))
            } else {[None;2]};
            assert_ne!(n64.rdp.combine_program,expected);
            assert_eq!(n64.save_state(),saved);
            n64.rdp.set_combine(if saved_mode==0x147e2844fe7b3d {0x127e24fffff9fc}else{0x147e2844fe7b3d});
            n64.load_state(&saved).unwrap();
            assert_eq!(n64.rdp.combine,saved_mode);
            assert_eq!(n64.rdp.combine_program,expected);
            n64.rdp.set_combine(if saved_mode==0x147e2844fe7b3d {0x127e24fffff9fc}else{0x147e2844fe7b3d});
            n64.set_rdp_combiner_acceleration(false);
            let before=n64.save_state();
            let before_program=n64.rdp.combine_program;
            assert_ne!(n64.rdp.combine,saved_mode);
            let truncated=saved[..saved.len()-1].to_vec();
            let mut trailing=saved.clone();
            trailing.push(0x64);
            for invalid in [truncated,trailing] {
                assert!(n64.load_state(&invalid).is_err());
                assert_eq!(n64.rdp.combine_program,before_program);
                assert_eq!(n64.save_state(),before);
                assert!(!n64.rdp.fast_combiner);
            }
            let values=[[256,-1,383,384],[255,17,128,255],[0,11,97,0],[255,128,0,255]];
            for cycle in 0..2 {same(&mut n64,cycle,values,false);}
        }
    }

    #[test]
    fn cloned_programs_and_preferences_change_independently() {
        let mut original=machine();
        original.rdp.set_combine(0x127e24fffff9fc);
        let program=original.rdp.combine_program;
        let mut copy=original.clone();
        copy.rdp.set_combine(0x147e2844fe7b3d);
        copy.set_rdp_combiner_acceleration(false);
        assert_eq!(original.rdp.combine,0x127e24fffff9fc);
        assert_eq!(original.rdp.combine_program,program);
        assert!(original.rdp.fast_combiner);
        assert_ne!(copy.rdp.combine_program,program);
        assert!(!copy.rdp.fast_combiner);
    }

    #[test]
    #[ignore]
    fn combiner_instruction_cost() {
        let mut n64=machine();
        let count:u32=std::env::var("N64_COMBINE_COUNT").unwrap_or_else(|_|"200000".into()).parse().unwrap();
        let mode=std::env::var("N64_COMBINE_MODE").unwrap_or_else(|_|"127e24fffff9fc".into());
        let fast=std::env::var("N64_COMBINE_FAST").unwrap_or_else(|_|"1".into())!="0";
        n64.rdp.fast_combiner=fast;
        n64.rdp.set_combine(u64::from_str_radix(&mode,16).unwrap());
        n64.rdp.prim=0x6417_83ff;
        n64.rdp.env=0x7f4a_00ac;
        let mut combined=[0;4];
        let mut texture=[23,71,253,127];
        let shade=[255,128,17,237];
        for n in 0..count {
            texture[n as usize&3]=(n.wrapping_mul(31337)>>3&255) as i32;
            let state=std::hint::black_box(&mut n64);
            combined=std::hint::black_box(state.raster().combine(1,combined,texture,[0;4],shade));
        }
        let hash=combined.into_iter().fold(0xcbf29ce484222325u64,|h,c|(h^(c as u64)).wrapping_mul(0x100000001b3));
        eprintln!("combine {mode} fast{fast}: {count}, hash{hash:016x}, result{combined:?}");
    }
}
