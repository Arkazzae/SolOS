//! The picture: 240 x 160 in RGB555, drawn a line at a time as the line's
//! horizontal blank begins, from the registers as they are then. Changes
//! during a line (rare in games) are not seen until the next.
//!
//! Up to four backgrounds (tiled, affine, or one bitmap, by mode) and 128
//! sprites are put in order of priority, through two windows and the
//! sprites' window, and the top two blended or brightened as BLDCNT asks.

use crate::Gba;
use crate::bus::set_byte;
use savestate::state;

pub const WIDTH: usize = 240;
pub const HEIGHT: usize = 160;

/// A line is 1232 cycles: the horizontal blank flag comes up after 1006,
/// and there are 228 lines, the last 68 the vertical blank.
const HDRAW: u64 = 1006;
const HBLANK: u64 = 226;
const LINES: u16 = 228;

/// A pixel no layer drew.
const CLEAR: u16 = 0x8000;

// The layers, as BLDCNT and the windows number them.
const OBJ: usize = 4;
const BACKDROP: usize = 5;

/// Sprites' sizes in pixels, by shape and size.
const SPRITE_SIZES: [[(i32, i32); 4]; 3] = [
    [(8, 8), (16, 16), (32, 32), (64, 64)],
    [(16, 8), (32, 8), (32, 16), (64, 32)],
    [(8, 16), (8, 32), (16, 32), (32, 64)],
];

/// A pixel of the sprite layer: its colour (or CLEAR), its priority, and
/// whether it is semi-transparent.
#[derive(Clone, Copy)]
struct ObjPixel {
    color: u16,
    priority: u8,
    semi: bool,
}

#[derive(Clone)]
pub struct Ppu {
    pub vram: Vec<u8>,
    pub palette: Vec<u8>,
    pub oam: Vec<u8>,
    dispcnt: u16,
    greenswap: u16,
    dispstat: u16,
    /// VCOUNT: the line being drawn.
    pub line: u16,
    bgcnt: [u16; 4],
    hofs: [u16; 4],
    vofs: [u16; 4],
    /// BG2 and BG3's affine matrices (PA, PB, PC, PD), their reference
    /// points as written, and as they have moved down the screen.
    matrix: [[u16; 4]; 2],
    reference: [[u32; 2]; 2],
    internal: [[i32; 2]; 2],
    winh: [u16; 2],
    winv: [u16; 2],
    winin: u16,
    winout: u16,
    mosaic: u16,
    bldcnt: u16,
    bldalpha: u16,
    bldy: u16,
    hblank: bool,
    pub frames: u64,
    pub picture: Vec<u16>,
    /// The sprites that show, by OAM index, with their first line and
    /// height; listed again after OAM changes.
    shown: Vec<(u8, u8, u8)>,
    pub oam_changed: bool,
    /// The palettes as colours, kept as they are written: not part of the
    /// state.
    colors: [u16; 512],
}

// The picture is not part of the state (the next frame draws it again),
// nor the list of sprites, made again from OAM.
state!(Ppu { vram, palette, oam, dispcnt, greenswap, dispstat, line, bgcnt, hofs, vofs, matrix, reference, internal,
    winh, winv, winin, winout, mosaic, bldcnt, bldalpha, bldy, hblank, frames });

/// A 28-bit reference point register, as a signed number (20.8).
fn signed28(value: u32) -> i32 {
    ((value << 4) as i32) >> 4
}

impl Ppu {
    pub fn new() -> Ppu {
        Ppu {
            vram: vec![0; 0x18000],
            palette: vec![0; 0x400],
            oam: vec![0; 0x400],
            dispcnt: 0x80,
            greenswap: 0,
            dispstat: 0,
            line: 0,
            bgcnt: [0; 4],
            hofs: [0; 4],
            vofs: [0; 4],
            matrix: [[0x100, 0, 0, 0x100]; 2],
            reference: [[0; 2]; 2],
            internal: [[0; 2]; 2],
            winh: [0; 2],
            winv: [0; 2],
            winin: 0,
            winout: 0,
            mosaic: 0,
            bldcnt: 0,
            bldalpha: 0,
            bldy: 0,
            hblank: false,
            frames: 0,
            picture: vec![0x7fff; WIDTH * HEIGHT],
            shown: Vec::with_capacity(128),
            oam_changed: true,
            colors: [0; 512],
        }
    }

    fn mode(&self) -> u16 {
        self.dispcnt & 7
    }

    /// How much of VRAM the backgrounds have (the rest is the sprites'):
    /// 64 KB, or 80 KB in the bitmap modes.
    pub fn background_vram(&self) -> usize {
        if self.mode() >= 3 { 0x14000 } else { 0x10000 }
    }

    pub fn read_register(&self, reg: u32) -> Option<u16> {
        Some(match reg {
            0x00 => self.dispcnt,
            0x02 => self.greenswap,
            0x04 => self.dispstat,
            0x06 => self.line,
            0x08 | 0x0a => self.bgcnt[(reg as usize - 8) / 2] & 0xdfff,
            0x0c | 0x0e => self.bgcnt[(reg as usize - 8) / 2],
            0x48 => self.winin,
            0x4a => self.winout,
            0x50 => self.bldcnt,
            0x52 => self.bldalpha,
            // Registers that are there but cannot be read.
            0x4e | 0x56..=0x5e => 0,
            _ => return None,
        })
    }

    pub fn write_register(&mut self, reg: u32, value: u8) {
        match reg {
            0x00 | 0x01 => {
                set_byte(&mut self.dispcnt, reg, value);
                self.dispcnt &= 0xfff7;
            }
            0x02 | 0x03 => set_byte(&mut self.greenswap, reg, value),
            0x04 => self.dispstat = self.dispstat & 0xff07 | u16::from(value) & 0x38,
            0x05 => self.dispstat = self.dispstat & 0x00ff | u16::from(value) << 8,
            0x08..=0x0f => set_byte(&mut self.bgcnt[(reg as usize - 8) / 2], reg, value),
            0x10..=0x1f => {
                let i = (reg as usize - 0x10) / 4;
                let offset = if reg & 2 == 0 { &mut self.hofs[i] } else { &mut self.vofs[i] };
                set_byte(offset, reg, value);
                *offset &= 0x1ff;
            }
            0x20..=0x3f => {
                let (bg, at) = ((reg as usize - 0x20) / 16, reg & 0xf);
                if at < 8 {
                    set_byte(&mut self.matrix[bg][at as usize / 2], reg, value);
                } else {
                    // Writing a reference point moves the drawing there too.
                    let axis = (at as usize - 8) / 4;
                    let shift = (at & 3) * 8;
                    let r = &mut self.reference[bg][axis];
                    *r = (*r & !(0xff << shift) | u32::from(value) << shift) & 0x0fff_ffff;
                    self.internal[bg][axis] = signed28(*r);
                }
            }
            0x40..=0x43 => set_byte(&mut self.winh[(reg as usize - 0x40) / 2], reg, value),
            0x44..=0x47 => set_byte(&mut self.winv[(reg as usize - 0x44) / 2], reg, value),
            0x48 | 0x49 => {
                set_byte(&mut self.winin, reg, value);
                self.winin &= 0x3f3f;
            }
            0x4a | 0x4b => {
                set_byte(&mut self.winout, reg, value);
                self.winout &= 0x3f3f;
            }
            0x4c | 0x4d => set_byte(&mut self.mosaic, reg, value),
            0x50 | 0x51 => {
                set_byte(&mut self.bldcnt, reg, value);
                self.bldcnt &= 0x3fff;
            }
            0x52 | 0x53 => {
                set_byte(&mut self.bldalpha, reg, value);
                self.bldalpha &= 0x1f1f;
            }
            0x54 | 0x55 => set_byte(&mut self.bldy, reg, value),
            _ => {}
        }
    }

    fn vram16(&self, i: usize) -> u16 {
        u16::from_le_bytes([self.vram[i], self.vram[i + 1]])
    }

    fn color(&self, index: usize) -> u16 {
        self.colors[index & 511]
    }

    /// Takes the palette bytes from `start` to `end` into the colours.
    pub fn palette_written(&mut self, start: usize, end: usize) {
        for i in start / 2..end.div_ceil(2) {
            self.colors[i] = u16::from_le_bytes([self.palette[2 * i], self.palette[2 * i + 1]]) & 0x7fff;
        }
    }

    fn oam16(&self, i: usize) -> u16 {
        u16::from_le_bytes([self.oam[i], self.oam[i + 1]])
    }

    /// The mosaic's block size, horizontally and vertically, for the
    /// backgrounds (0) or the sprites (1).
    fn mosaic_size(&self, sprites: bool) -> (usize, usize) {
        let m = self.mosaic >> if sprites { 8 } else { 0 };
        (usize::from(m & 15) + 1, usize::from(m >> 4 & 15) + 1)
    }

    /// The horizontal mosaic: each block of pixels shows its first.
    fn mosaic_line(&self, out: &mut [u16; WIDTH]) {
        let (size, _) = self.mosaic_size(false);
        if size > 1 {
            for x in 0..WIDTH {
                out[x] = out[x - x % size];
            }
        }
    }

    /// The tiles of a line of a tiled background (32 x 32-tile screens of
    /// map entries: tile, flips, palette; tiles of 16 or 256 colours), left
    /// to right: where on the line, how many pixels, from which of the tile
    /// row's, the row's eight palette numbers a byte each (0 clear, the
    /// leftmost lowest; all 0 for a clear row), and the palette's first.
    fn text_tiles(&self, bg: usize, y: usize, mut tile: impl FnMut(usize, usize, usize, u64, usize)) {
        let cnt = usize::from(self.bgcnt[bg]);
        let (chars, screens) = ((cnt >> 2 & 3) * 0x4000, (cnt >> 8 & 31) * 0x800);
        let colors256 = cnt & 0x80 != 0;
        let (width, height) = [(256, 256), (512, 256), (256, 512), (512, 512)][cnt >> 14];
        let mosaic = cnt & 0x40 != 0;
        let mosaic_y = if mosaic { self.mosaic_size(false).1 } else { 1 };
        let py = (y - y % mosaic_y + usize::from(self.vofs[bg])) & (height - 1);
        let (ty, fine_y) = (py / 8, py % 8);
        // The row of the map, in the screen (or the first of two across)
        // the line is in.
        let row = screens + ty / 32 * (width / 256) * 0x800 + ty % 32 * 64;
        let mut px = usize::from(self.hofs[bg]) & (width - 1);
        let mut x = 0;
        while x < WIDTH {
            let tx = px / 8;
            let entry = usize::from(self.vram16(row + tx / 32 * 0x800 + tx % 32 * 2));
            let line = if entry & 0x800 != 0 { 7 - fine_y } else { fine_y };
            let (first, n) = (px % 8, (8 - px % 8).min(WIDTH - x));
            // The tile's row: eight bytes, or eight nibbles in a word. Past
            // the backgrounds' VRAM it is clear.
            let (pixels, palette) = if colors256 {
                let addr = chars + (entry & 0x3ff) * 64 + line * 8;
                let bytes = self.vram.get(addr..addr + 8).filter(|_| addr < 0x10000);
                (bytes.map_or(0, |b| u64::from_le_bytes(b.try_into().unwrap())), 0)
            } else {
                let addr = chars + (entry & 0x3ff) * 32 + line * 4;
                let bytes = self.vram.get(addr..addr + 4).filter(|_| addr < 0x10000);
                let word = bytes.map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()));
                // Spread to a byte a pixel, as the 256-colour row is.
                let mut spread = u64::from(word);
                spread = (spread | spread << 16) & 0x0000_ffff_0000_ffff;
                spread = (spread | spread << 8) & 0x00ff_00ff_00ff_00ff;
                spread = (spread | spread << 4) & 0x0f0f_0f0f_0f0f_0f0f;
                (spread, (entry >> 12) * 16)
            };
            let pixels = if entry & 0x400 != 0 { pixels.swap_bytes() } else { pixels };
            tile(x, n, first, pixels, palette);
            x += n;
            px = (px + n) & (width - 1);
        }
    }

    /// A line of a tiled background, as colours (CLEAR where clear).
    fn text_line(&self, bg: usize, y: usize, out: &mut [u16; WIDTH]) {
        self.text_tiles(bg, y, |x, n, first, pixels, palette| {
            if pixels == 0 {
                out[x..x + n].fill(CLEAR);
                return;
            }
            // A byte array, not shifts: the console's CPU has 32-bit ones.
            let bytes = pixels.to_le_bytes();
            for (pixel, &index) in out[x..x + n].iter_mut().zip(&bytes[first..first + n]) {
                *pixel = if index == 0 { CLEAR } else { self.color(palette + usize::from(index)) };
            }
        });
        if self.bgcnt[bg] & 0x40 != 0 {
            self.mosaic_line(out);
        }
    }

    /// A line of a tiled background with no mosaic, painted straight onto
    /// the canvas where the windows let it; clear tile rows are passed over
    /// whole.
    fn paint_text(&self, bg: usize, y: usize, masks: &[u8; WIDTH], windows: bool, canvas: &mut Canvas) {
        let bit = 1u8 << bg;
        self.text_tiles(bg, y, |x, n, first, pixels, palette| {
            if pixels == 0 {
                return;
            }
            let bytes = pixels.to_le_bytes();
            for (i, &index) in bytes[first..first + n].iter().enumerate() {
                if index != 0 && (!windows || masks[x + i] & bit != 0) {
                    canvas.paint(x + i, self.color(palette + usize::from(index)), bg as u8);
                }
            }
        });
    }

    /// Where each pixel of the line falls in an affine background (or the
    /// bitmap): the reference point stepped by PA and PC across the line.
    fn affine_points(&self, bg: usize) -> impl Iterator<Item = (i32, i32)> + use<> {
        let [pa, _, pc, _] = self.matrix[bg - 2].map(|m| i32::from(m as i16));
        let [x0, y0] = self.internal[bg - 2];
        (0..WIDTH as i32).map(move |x| ((x0 + pa * x) >> 8, (y0 + pc * x) >> 8))
    }

    /// A line of an affine background: a square map of one-byte tile
    /// numbers, 256-colour tiles, wrapping round or clear outside.
    fn affine_line(&self, bg: usize, out: &mut [u16; WIDTH]) {
        let cnt = usize::from(self.bgcnt[bg]);
        let (chars, screens) = ((cnt >> 2 & 3) * 0x4000, (cnt >> 8 & 31) * 0x800);
        let size = 128 << (cnt >> 14);
        let wrap = cnt & 0x2000 != 0;
        for (pixel, (x, y)) in out.iter_mut().zip(self.affine_points(bg)) {
            let (x, y) = if wrap {
                (x as usize & (size - 1), y as usize & (size - 1))
            } else if (x as u32) < size as u32 && (y as u32) < size as u32 {
                (x as usize, y as usize)
            } else {
                *pixel = CLEAR;
                continue;
            };
            let tile = usize::from(self.vram[screens + y / 8 * (size / 8) + x / 8]);
            let addr = chars + tile * 64 + y % 8 * 8 + x % 8;
            let index = if addr < 0x10000 { usize::from(self.vram[addr]) } else { 0 };
            *pixel = if index == 0 { CLEAR } else { self.color(index) };
        }
        if cnt & 0x40 != 0 {
            self.mosaic_line(out);
        }
    }

    /// A line of BG2 in the bitmap modes: 240 x 160 colours (mode 3), or
    /// two frames of 240 x 160 palette numbers (4) or 160 x 128 colours (5).
    fn bitmap_line(&self, out: &mut [u16; WIDTH]) {
        let mode = self.mode();
        let frame = if self.dispcnt & 0x10 != 0 && mode != 3 { 0xa000 } else { 0 };
        let (width, height) = if mode == 5 { (160, 128) } else { (240, 160) };
        for (pixel, (x, y)) in out.iter_mut().zip(self.affine_points(2)) {
            if x as u32 >= width || y as u32 >= height {
                *pixel = CLEAR;
                continue;
            }
            let i = (y as u32 * width + x as u32) as usize;
            *pixel = match mode {
                4 => match self.vram[frame + i] {
                    0 => CLEAR,
                    index => self.color(usize::from(index)),
                },
                _ => self.vram16(frame + 2 * i) & 0x7fff,
            };
        }
        if self.bgcnt[2] & 0x40 != 0 {
            self.mosaic_line(out);
        }
    }

    /// The sprites on a line, in OAM order: a later one shows over an
    /// earlier one only with a higher priority. Sprites in window mode draw
    /// the sprites' window instead. Drawing a line has a budget of cycles,
    /// which sprites past it do not get.
    fn sprite_line(&self, y: usize, out: &mut [ObjPixel; WIDTH], window: &mut [bool; WIDTH]) {
        let one_dimensional = self.dispcnt & 0x40 != 0;
        let bitmap = self.mode() >= 3;
        let mut budget: i32 = if self.dispcnt & 0x20 != 0 { 954 } else { 1210 };
        let (mosaic_x, mosaic_y) = self.mosaic_size(true);
        for &(i, top, height) in &self.shown {
            let mut row = (y as i32 - i32::from(top)) & 0xff;
            if row >= i32::from(height) {
                continue;
            }
            let i = usize::from(i);
            let [a0, a1, a2] = [0, 2, 4].map(|o| i32::from(self.oam16(i * 8 + o)));
            let (affine, mode) = (a0 & 0x100 != 0, a0 >> 10 & 3);
            let (w, h) = SPRITE_SIZES[(a0 >> 14) as usize][(a1 >> 14) as usize];
            let double = affine && a0 & 0x200 != 0;
            let (bw, bh) = if double { (2 * w, 2 * h) } else { (w, h) };
            budget -= if affine { 10 + 2 * bw } else { bw };
            if budget < 0 {
                break;
            }
            let tile = a2 & 0x3ff;
            if bitmap && tile < 512 {
                continue;
            }
            let mosaic = a0 & 0x1000 != 0;
            if mosaic {
                row = (row - (y % mosaic_y) as i32).max(0);
            }
            let x0 = if a1 & 0x100 != 0 { (a1 & 0x1ff) - 512 } else { a1 & 0x1ff };
            let colors256 = a0 & 0x2000 != 0;
            let (priority, palette) = ((a2 >> 10 & 3) as u8, (a2 >> 12) as usize * 16);
            let matrix = if affine {
                let group = (a1 >> 9 & 31) as usize * 32;
                [6, 14, 22, 30].map(|o| i32::from(self.oam16(group + o) as i16))
            } else {
                [0; 4]
            };
            // Tiles: in 1D mapping one after another, in 2D rows of 32.
            let unit = if colors256 { 2 } else { 1 };
            let stride = if one_dimensional { (w / 8 * unit) as usize } else { 32 };
            let base = (if colors256 && !one_dimensional { tile & !1 } else { tile }) as usize;
            let (hflip, vflip) = (!affine && a1 & 0x1000 != 0, !affine && a1 & 0x2000 != 0);
            for sx in x0.max(0)..(x0 + bw).min(WIDTH as i32) {
                let x = sx as usize;
                // Over a sprite as high or higher there is nothing to draw.
                if mode != 2 && out[x].color != CLEAR && out[x].priority <= priority {
                    continue;
                }
                let mut col = sx - x0;
                if mosaic {
                    col = (col - sx % mosaic_x as i32).max(0);
                }
                let (px, py) = if affine {
                    let (cx, cy) = (col - bw / 2, row - bh / 2);
                    let px = ((matrix[0] * cx + matrix[1] * cy) >> 8) + w / 2;
                    let py = ((matrix[2] * cx + matrix[3] * cy) >> 8) + h / 2;
                    if !(0..w).contains(&px) || !(0..h).contains(&py) {
                        continue;
                    }
                    (px as usize, py as usize)
                } else {
                    let px = if hflip { w - 1 - col } else { col };
                    let py = if vflip { h - 1 - row } else { row };
                    (px as usize, py as usize)
                };
                let n = (base + py / 8 * stride + px / 8 * unit as usize) & 0x3ff;
                let (fx, fy) = (px % 8, py % 8);
                let index = if colors256 {
                    usize::from(self.vram[0x10000 + n * 32 + fy * 8 + fx])
                } else {
                    match usize::from(self.vram[0x10000 + n * 32 + fy * 4 + fx / 2] >> ((fx & 1) * 4) & 15) {
                        0 => 0,
                        i => palette + i,
                    }
                };
                if index == 0 {
                    continue;
                }
                if mode == 2 {
                    window[x] = true;
                } else {
                    out[x] = ObjPixel { color: self.color(256 + index), priority, semi: mode == 1 };
                }
            }
        }
    }

    /// Lists the sprites that show (not disabled), with their first line and
    /// height, after OAM has changed.
    fn list_sprites(&mut self) {
        self.oam_changed = false;
        self.shown.clear();
        for i in 0..128u8 {
            let [a0, a1] = [0, 2].map(|o| self.oam16(usize::from(i) * 8 + o));
            let (affine, shape, mode) = (a0 & 0x100 != 0, a0 >> 14, a0 >> 10 & 3);
            if !affine && a0 & 0x200 != 0 || shape == 3 || mode == 3 {
                continue;
            }
            let h = SPRITE_SIZES[usize::from(shape)][usize::from(a1 >> 14)].1 as u8;
            let double = affine && a0 & 0x200 != 0;
            self.shown.push((i, a0 as u8, if double { 2 * h } else { h }));
        }
    }

    /// Whether a point is in a window on one axis: from the first
    /// coordinate up to the second, or round past the edge when the first
    /// is the greater.
    fn in_window(range: u16, at: usize, edge: usize) -> bool {
        let (start, end) = (usize::from(range >> 8), usize::from(range & 0xff).min(edge));
        if start <= end { (start..end).contains(&at) } else { at >= start || at < end }
    }

    /// What each pixel's window lets through: window 0 over window 1 over
    /// the sprites' window over the rest.
    fn window_masks(&self, y: usize, windows: u16, obj_window: &[bool; WIDTH]) -> [u8; WIDTH] {
        let mut masks = [0x3fu8; WIDTH];
        if windows == 0 {
            return masks;
        }
        masks.fill((self.winout & 0x3f) as u8);
        if windows & 4 != 0 {
            let inside = (self.winout >> 8 & 0x3f) as u8;
            for (mask, _) in masks.iter_mut().zip(obj_window).filter(|(_, on)| **on) {
                *mask = inside;
            }
        }
        for w in [1, 0] {
            if windows & 1 << w != 0 && Ppu::in_window(self.winv[w], y, 228) {
                let inside = (self.winin >> (8 * w) & 0x3f) as u8;
                // The window's span across: one run, or two round the edge.
                let range = self.winh[w];
                let (start, end) = (usize::from(range >> 8).min(WIDTH), usize::from(range & 0xff).min(WIDTH));
                if usize::from(range >> 8) <= usize::from(range & 0xff).min(WIDTH) {
                    masks[start..end].fill(inside);
                } else {
                    masks[start..].fill(inside);
                    masks[..end].fill(inside);
                }
            }
        }
        masks
    }

    /// Draws the line VCOUNT is on into the picture: the layers are painted
    /// over one another from the lowest priority up (a priority's
    /// backgrounds by number from the highest, then its sprites, which go
    /// over backgrounds of their priority), where the windows let them,
    /// keeping each pixel's top layer and the one under it for blending.
    fn render_line(&mut self) {
        let y = usize::from(self.line);
        if self.dispcnt & 0x80 != 0 {
            self.picture[y * WIDTH..(y + 1) * WIDTH].fill(0x7fff);
            return;
        }
        let mode = self.mode();
        let backgrounds: u16 = match mode {
            0 => 0b1111,
            1 => 0b0111,
            2 => 0b1100,
            3..=5 => 0b0100,
            _ => 0,
        } & self.dispcnt >> 8;
        let mut sprites = [ObjPixel { color: CLEAR, priority: 4, semi: false }; WIDTH];
        let mut obj_window = [false; WIDTH];
        if self.dispcnt & 0x1000 != 0 {
            if self.oam_changed {
                self.list_sprites();
            }
            self.sprite_line(y, &mut sprites, &mut obj_window);
        }
        let windows = self.dispcnt >> 13;
        let masks = self.window_masks(y, windows, &obj_window);
        // The priorities sprites on the line have, and whether any is
        // semi-transparent.
        let (mut sprite_priorities, mut semi) = (0u8, false);
        for sprite in sprites.iter().filter(|s| s.color != CLEAR) {
            sprite_priorities |= 1 << sprite.priority;
            semi |= sprite.semi;
        }
        let backdrop = self.color(0);
        let mut canvas = Canvas {
            top: [backdrop; WIDTH],
            top_id: [BACKDROP as u8; WIDTH],
            under: [backdrop; WIDTH],
            under_id: [BACKDROP as u8; WIDTH],
        };
        let mut layer = [CLEAR; WIDTH];
        for priority in (0..4u8).rev() {
            for bg in (0..4).rev() {
                if backgrounds & 1 << bg == 0 || self.bgcnt[bg] & 3 != u16::from(priority) {
                    continue;
                }
                let text = mode == 0 || mode == 1 && bg < 2;
                if text && self.bgcnt[bg] & 0x40 == 0 {
                    self.paint_text(bg, y, &masks, windows != 0, &mut canvas);
                    continue;
                }
                if text {
                    self.text_line(bg, y, &mut layer);
                } else if mode <= 2 {
                    self.affine_line(bg, &mut layer);
                } else {
                    self.bitmap_line(&mut layer);
                }
                for (x, (&color, &mask)) in layer.iter().zip(&masks).enumerate() {
                    if color != CLEAR && mask & 1 << bg != 0 {
                        canvas.paint(x, color, bg as u8);
                    }
                }
            }
            if sprite_priorities & 1 << priority != 0 {
                for (x, (sprite, &mask)) in sprites.iter().zip(&masks).enumerate() {
                    if sprite.color != CLEAR && sprite.priority == priority && mask & 0x10 != 0 {
                        canvas.paint(x, sprite.color, OBJ as u8);
                    }
                }
            }
        }
        let out = &mut self.picture[y * WIDTH..(y + 1) * WIDTH];
        out.copy_from_slice(&canvas.top);
        // An effect with no first target, or a fade by nothing, changes no
        // pixel; else it changes those a first target tops (in a window
        // that lets effects through), and semi-transparent sprites blend.
        let effect = self.bldcnt >> 6 & 3;
        if (effect == 0 || self.bldcnt & 0x3f == 0 || effect >= 2 && self.bldy & 0x1f == 0) && !semi {
            return;
        }
        let (eva, evb, evy) = (
            u32::from(self.bldalpha & 0x1f).min(16),
            u32::from(self.bldalpha >> 8 & 0x1f).min(16),
            u32::from(self.bldy & 0x1f).min(16),
        );
        for x in 0..WIDTH {
            let (a, first) = (canvas.top[x], usize::from(canvas.top_id[x]));
            let first_target = self.bldcnt & 1 << first != 0;
            let semi = semi && first == OBJ && sprites[x].semi;
            let enabled = masks[x];
            if !semi && (enabled & 0x20 == 0 || !first_target) {
                continue;
            }
            let (b, second) = (canvas.under[x], usize::from(canvas.under_id[x]));
            let second_target = self.bldcnt >> 8 & 1 << second != 0;
            out[x] = if semi && second_target {
                blend(a, b, eva, evb)
            } else if enabled & 0x20 == 0 || !first_target {
                a
            } else {
                match effect {
                    1 if second_target => blend(a, b, eva, evb),
                    2 => fade(a, 31, evy),
                    3 => fade(a, 0, evy),
                    _ => a,
                }
            };
        }
    }
}

/// Each pixel's top layer and the one under it, as layers are painted over
/// one another: their colours, and which layers they are (as BLDCNT
/// numbers them).
struct Canvas {
    top: [u16; WIDTH],
    top_id: [u8; WIDTH],
    under: [u16; WIDTH],
    under_id: [u8; WIDTH],
}

impl Canvas {
    #[inline(always)]
    fn paint(&mut self, x: usize, color: u16, id: u8) {
        self.under[x] = self.top[x];
        self.under_id[x] = self.top_id[x];
        self.top[x] = color;
        self.top_id[x] = id;
    }
}

/// Alpha blending: each channel a weighted sum, in sixteenths.
fn blend(a: u16, b: u16, eva: u32, evb: u32) -> u16 {
    let channel = |shift: u16| {
        let (x, y) = (u32::from(a >> shift & 31), u32::from(b >> shift & 31));
        ((x * eva + y * evb) >> 4).min(31) as u16
    };
    channel(0) | channel(5) << 5 | channel(10) << 10
}

/// Brightening towards white (31) or darkening towards black (0).
fn fade(a: u16, to: u32, evy: u32) -> u16 {
    let channel = |shift: u16| {
        let x = u32::from(a >> shift & 31);
        if to > x { (x + (((to - x) * evy) >> 4)) as u16 } else { (x - ((x * evy) >> 4)) as u16 }
    };
    channel(0) | channel(5) << 5 | channel(10) << 10
}

impl Gba {
    /// The PPU's moments: the start of a line's horizontal blank, where it
    /// is drawn, and the end of the line.
    pub(crate) fn ppu_event(&mut self, at: u64) {
        let ppu = &mut self.ppu;
        if !ppu.hblank {
            ppu.hblank = true;
            ppu.dispstat |= 2;
            let line = ppu.line;
            if line < HEIGHT as u16 {
                ppu.render_line();
                for bg in 0..2 {
                    let [_, pb, _, pd] = ppu.matrix[bg].map(|m| i32::from(m as i16));
                    ppu.internal[bg][0] += pb;
                    ppu.internal[bg][1] += pd;
                }
            }
            if ppu.dispstat & 0x10 != 0 {
                self.request(crate::HBLANK_IRQ);
            }
            if line < HEIGHT as u16 {
                self.trigger_dma(crate::dma::HBLANK);
            }
            self.video_capture_dma(line);
            self.schedule(crate::PPU, at + HBLANK);
            return;
        }
        ppu.hblank = false;
        ppu.dispstat &= !2;
        ppu.line = (ppu.line + 1) % LINES;
        let line = ppu.line;
        let mut irq = 0;
        if line == HEIGHT as u16 {
            ppu.dispstat |= 1;
            ppu.frames += 1;
            // The affine backgrounds start each frame at their reference
            // points.
            for bg in 0..2 {
                ppu.internal[bg] = ppu.reference[bg].map(signed28);
            }
            if ppu.dispstat & 8 != 0 {
                irq |= crate::VBLANK_IRQ;
            }
        } else if line == LINES - 1 {
            ppu.dispstat &= !1;
        }
        if line == ppu.dispstat >> 8 {
            ppu.dispstat |= 4;
            if ppu.dispstat & 0x20 != 0 {
                irq |= crate::VCOUNT_IRQ;
            }
        } else {
            ppu.dispstat &= !4;
        }
        if irq != 0 {
            self.request(irq);
        }
        if line == HEIGHT as u16 {
            self.trigger_dma(crate::dma::VBLANK);
        }
        self.schedule(crate::PPU, at + HDRAW);
    }
}
