//! The 2C02 PPU, dot by dot: 341 dots a line, 262 lines a frame (line 0
//! the first visible one, vblank from line 241, line 261 the pre-render
//! line). Background tiles are fetched eight dots ahead into shift
//! registers, sprites are picked for the next line and their patterns
//! fetched at the dots the chip fetches them, so mappers that watch the
//! PPU's address bus see what they would on a real console.

use crate::cart::{Cart, Mirroring};
use savestate::{BadState, State, state};

pub const WIDTH: usize = 256;

// A pixel of sprite_line: from sprite 0, and behind the background.
const SPRITE_ZERO: u8 = 0x80;
const BEHIND: u8 = 0x40;

pub const HEIGHT: usize = 240;

/// A pattern byte's bits spread one to a byte, the leftmost pixel first.
const SPREAD: [u64; 256] = {
    let mut table = [0; 256];
    let mut i = 0;
    while i < 256 {
        let mut bit = 0;
        while bit < 8 {
            table[i] |= ((i as u64) >> (7 - bit) & 1) << (bit * 8);
            bit += 1;
        }
        i += 1;
    }
    table
};

/// A sprite on the line being drawn.
#[derive(Clone, Copy, Default)]
struct Sprite {
    x: u8,
    attr: u8,
    /// Its row's pattern, already flipped if the sprite is.
    lo: u8,
    hi: u8,
    /// Sprite 0, for the hit flag.
    zero: bool,
}

state!(Sprite { x, attr, lo, hi, zero });

#[derive(Clone)]
pub struct Ppu {
    pub dot: u16,
    pub scanline: u16,
    /// Frames completed since power-on, and dots (wrapping).
    pub frames: u64,
    clock: u32,
    pub picture: Picture,
    /// The NMI line to the CPU: the PPU wants an NMI.
    pub nmi: bool,
    /// $2002 was read the dot before vblank: it does not start this frame.
    suppress_vblank: bool,
    /// Rendering was on when the pre-render line decided on the short frame.
    skip_dot: bool,

    /// $2000, $2001.
    ctrl: u8,
    mask: u8,
    /// $2002's flags: vblank (bit 7), sprite 0 hit (6), overflow (5).
    status: u8,
    oam_addr: u8,
    oam: [u8; 256],
    /// The scroll and address registers (v, t, fine x) and the write
    /// toggle shared by $2005 and $2006.
    v: u16,
    t: u16,
    x: u8,
    w: bool,
    /// What $2007 last read, returned by the next read.
    buffer: u8,
    /// The last value written to or read from a register: reads of
    /// write-only registers return it. Its bits fade to 0 unless refreshed
    /// within about 600 ms (36 frames); when each was last is kept.
    latch: u8,
    latch_frames: [u64; 8],
    /// Nametables: 2 KB, or 4 KB for four-screen cartridges.
    vram: [u8; 0x1000],
    palette: [u8; 32],
    /// The picture's pixel for each palette index, from update_colors.
    colors: [u16; 32],
    /// The palette entries of the last two pixels drawn: greyscale and
    /// emphasis act on the chip's video output two dots after a pixel is
    /// made, so a change of them reaches these too.
    recent: [u8; 2],

    /// The background fetches for the next tile, and the shift registers
    /// the pixels come out of (attributes spread to a bit per pixel).
    tile: u8,
    attribute: u8,
    pattern_lo: u8,
    pattern_hi: u8,
    /// The background's shift registers, kept as the pixels they make: a
    /// ring of 16, the next tile's 8 after the current one's, `bg_head` the
    /// pixel at fine x 0.
    bg: [u8; 16],
    bg_head: u8,

    /// The sprites found for the next line, then those of this one, and the
    /// dot of this line at which the search finds a ninth (or u16::MAX).
    found: [u8; 8],
    found_count: usize,
    overflow_dot: u16,
    sprites: [Sprite; 8],
    sprite_count: usize,
    /// Their pixels by x: SPRITE_ZERO, BEHIND, and the palette index.
    sprite_line: [u8; WIDTH],
    /// How many dots the PPU may run behind the CPU before one of them
    /// changes the CPU's interrupt lines (or ends the frame).
    pub until_event: u32,
    /// A line shows only the first eight sprites on it, as on the console
    /// (a setting, not the machine's state: see Nes::set_sprite_limit).
    pub sprite_limit: bool,
}

state!(Ppu {
    dot, scanline, frames, clock, picture, nmi, suppress_vblank, skip_dot, ctrl, mask, status, oam_addr, oam, v, t, x, w,
    buffer, latch, latch_frames, vram, palette, colors, recent, tile, attribute, pattern_lo, pattern_hi, bg, bg_head,
    found, found_count, overflow_dot, sprites, sprite_count, sprite_line, until_event,
});

impl Ppu {
    pub fn new() -> Ppu {
        Ppu {
            dot: 0,
            scanline: 0,
            frames: 0,
            clock: 0,
            picture: Picture { pixels: Box::new([0; WIDTH * HEIGHT]), drawn: 0 },
            nmi: false,
            suppress_vblank: false,
            skip_dot: false,
            ctrl: 0,
            mask: 0,
            status: 0,
            oam_addr: 0,
            oam: [0; 256],
            v: 0,
            t: 0,
            x: 0,
            w: false,
            buffer: 0,
            latch: 0,
            latch_frames: [0; 8],
            vram: [0; 0x1000],
            palette: [0; 32],
            colors: [0; 32],
            recent: [0; 2],
            tile: 0,
            attribute: 0,
            pattern_lo: 0,
            pattern_hi: 0,
            bg: [0; 16],
            bg_head: 0,
            found: [0; 8],
            found_count: 0,
            overflow_dot: u16::MAX,
            sprites: [Sprite::default(); 8],
            sprite_count: 0,
            sprite_line: [0; WIDTH],
            until_event: 0,
            sprite_limit: true,
        }
    }

    /// The reset button: the registers it clears.
    pub fn reset(&mut self) {
        self.ctrl = 0;
        self.mask = 0;
        self.w = false;
        self.buffer = 0;
        self.nmi = false;
        self.until_event = 0;
    }

    /// PPUCTRL and PPUMASK, for debugging.
    pub fn registers(&self) -> (u8, u8) {
        (self.ctrl, self.mask)
    }

    fn rendering(&self) -> bool {
        self.mask & 0x18 != 0
    }

    /// Runs some dots. A visible line's tiles go a tile (eight dots) at a
    /// time where nothing but drawing and fetching happens in them, and the
    /// lines of the vertical blank, where nothing happens but at its start,
    /// at once.
    pub fn run(&mut self, cart: &mut Cart, dots: u32) {
        let mut left = dots;
        while left > 0 {
            let (line, dot) = (self.scanline, self.dot);
            if line < 240 || line == 261 {
                if left >= 8 && line < 240 && dot & 7 == 1 && (9..=241).contains(&dot) && dot != 65 && self.rendering()
                    && !(dot..dot + 8).contains(&self.overflow_dot)
                {
                    self.render_tile(cart, dot);
                    left -= 8;
                    continue;
                }
                if left >= 8 && (265..=313).contains(&dot) && dot & 7 == 1 && self.rendering() {
                    self.fetch_sprite_slot(cart, line, dot);
                    left -= 8;
                    continue;
                }
            } else if line <= 260 && !(line == 241 && dot <= 1) {
                // The vertical blank.
                let n = left.min(341 - u32::from(dot));
                self.dot += n as u16;
                self.clock = self.clock.wrapping_add(n);
                left -= n;
                if self.dot == 341 {
                    self.dot = 0;
                    self.scanline += 1;
                }
                continue;
            }
            self.tick(cart);
            left -= 1;
        }
        self.schedule(cart);
    }

    /// Dots `dot` to `dot` + 7 of a visible line, rendering, as render() runs
    /// them: the shifters move and take the fetched tile, eight pixels come
    /// out (none at the left edge, which is not in these dots), and the next
    /// tile is fetched, each fetch at its own dot.
    fn render_tile(&mut self, cart: &mut Cart, dot: u16) {
        let (show_bg, show_sprites) = (self.mask & 0x08 != 0, self.mask & 0x10 != 0);
        self.load_shifters_ahead();
        let mut pixels = [0u16; 8];
        for (k, pixel) in pixels.iter_mut().enumerate() {
            let x = usize::from(dot - 1) + k;
            let head = self.bg_head.wrapping_add(1 + k as u8);
            let bg = if show_bg { self.bg[usize::from(head.wrapping_add(self.x)) & 15] } else { 0 };
            let mut color = bg;
            let sprite = if show_sprites { self.sprite_line[x] } else { 0 };
            if sprite != 0 {
                if sprite & SPRITE_ZERO != 0 && bg != 0 && x != 255 {
                    self.status |= 0x40;
                }
                if bg == 0 || sprite & BEHIND == 0 {
                    color = sprite & 0x1f;
                }
            }
            *pixel = self.colors[usize::from(color)];
            if k >= 6 {
                self.recent[k - 6] = self.entry(color);
            }
        }
        self.bg_head = self.bg_head.wrapping_add(8);
        let drawn = self.picture.drawn;
        if let Some(out) = self.picture.pixels.get_mut(drawn..drawn + 8) {
            out.copy_from_slice(&pixels);
            self.picture.drawn = drawn + 8;
        }
        // The fetches, each at its dot (the picture does not depend on them
        // within the tile).
        let clock = self.clock;
        let v = self.v;
        cart.watch(0x2000 | v & 0x0fff, clock);
        self.tile = self.vram[nametable(0x2000 | v & 0x0fff, cart.mirroring)];
        let at_addr = 0x23c0 | v & 0x0c00 | v >> 4 & 0x38 | v >> 2 & 0x07;
        cart.watch(at_addr, clock.wrapping_add(2));
        self.attribute = self.vram[nametable(at_addr, cart.mirroring)] >> (v >> 4 & 4 | v & 2) & 3;
        let pattern = self.pattern_addr();
        cart.watch(pattern, clock.wrapping_add(4));
        self.pattern_lo = cart.chr_read(pattern);
        cart.watch(pattern + 8, clock.wrapping_add(6));
        self.pattern_hi = cart.chr_read(pattern + 8);
        self.increment_x();
        self.clock = clock.wrapping_add(8);
        self.dot += 8;
    }

    /// Dots `dot` to `dot` + 7 of the sprite fetches, for the slots after
    /// the first (dots 265-320), as render() runs them: OAMADDR held at 0,
    /// the pre-render line copying the vertical scroll, the slot's two
    /// pattern bytes at their dots, and at dot 320 the next line's sprites.
    fn fetch_sprite_slot(&mut self, cart: &mut Cart, line: u16, dot: u16) {
        let visible = line < 240;
        self.oam_addr = 0;
        if !visible && dot + 7 >= 280 && dot <= 304 {
            self.v = self.v & !0x7be0 | self.t & 0x7be0;
        }
        let slot = usize::from(dot - 257) / 8;
        let clock = self.clock;
        self.clock = clock.wrapping_add(4);
        self.fetch_sprite(slot, line, cart, false);
        self.clock = clock.wrapping_add(6);
        self.fetch_sprite(slot, line, cart, true);
        self.clock = clock.wrapping_add(8);
        self.dot += 8;
        if dot == 313 {
            self.sprite_count = if visible { self.found_count } else { 0 };
            self.compose_sprites(cart, line);
        }
    }

    /// load_shifters as the first dot of a tile does it: after the shift.
    fn load_shifters_ahead(&mut self) {
        self.bg_head = self.bg_head.wrapping_add(1);
        self.load_shifters();
        self.bg_head = self.bg_head.wrapping_sub(1);
    }

    /// Works out until_event from where the PPU is: the dots to vblank's
    /// start and end (the NMI), to the end of the frame, and for a mapper
    /// counting A12's rises, to the next fetch that may raise it.
    pub fn schedule(&mut self, cart: &Cart) {
        let (line, dot) = (u32::from(self.scanline), u32::from(self.dot));
        // An event at a dot happens as that dot runs: after this many.
        let until = |event_line: u32, event_dot: u32| (event_line * 341 + event_dot + 1).checked_sub(line * 341 + dot);
        let mut next = [until(241, 1), until(261, 1), until(261, 340)].into_iter().flatten().min().unwrap_or(u32::MAX);
        if cart.watches_a12() && self.rendering() {
            next = next.min(self.until_a12(line, dot));
        }
        self.until_event = next;
    }

    /// Dots to the next fetch that may raise A12. With the background's
    /// tiles at $1000, every tile's; else only sprites' (from $1000, or 8x16
    /// by their tile), fetched two to a slot at dots 261-319 of the lines
    /// that render, with the background's (at $0000) before them. The
    /// slots' sprites are known from the search at dot 65 of the line (OAM
    /// does not change behind the CPU's back); before it, the window's start.
    fn until_a12(&self, line: u32, dot: u32) -> u32 {
        if self.ctrl & 0x10 != 0 {
            return 1;
        }
        let tall = self.ctrl & 0x20 != 0;
        if !tall && self.ctrl & 0x08 == 0 {
            return u32::MAX;
        }
        let renders = line < 240 || line == 261;
        if renders && dot <= 320 {
            if line < 240 && dot <= 65 {
                return 257 - dot + 1;
            }
            // A slot's fetches with A12 set, after ones with it clear.
            let high = |slot: usize| {
                if !tall {
                    true
                } else if slot < self.found_count {
                    self.oam[usize::from(self.found[slot]) * 4 + 1] & 1 != 0
                } else {
                    // Empty slots fetch tile $FF.
                    true
                }
            };
            for slot in 0..8 {
                let rise = 261 + 8 * slot as u32;
                if rise >= dot && high(slot) && (slot == 0 || !high(slot - 1)) {
                    return rise - dot + 1;
                }
            }
        }
        // The window of the next line that renders; after line 261's, the
        // end of the frame comes first.
        let next = match line {
            0..239 => line + 1,
            239..261 => 261,
            _ => return u32::MAX,
        };
        next * 341 + 257 + 1 - (line * 341 + dot)
    }

    /// One dot.
    #[inline]
    fn tick(&mut self, cart: &mut Cart) {
        let (line, dot) = (self.scanline, self.dot);
        let visible = line < 240;
        let prerender = line == 261;
        if self.rendering() && (visible || prerender) {
            self.render(cart, line, dot, visible);
        } else if visible && (1..=256).contains(&dot) {
            // Rendering off: the backdrop colour, or the palette entry v
            // points at, if it points into the palette.
            let i = if self.v & 0x3f00 == 0x3f00 { palette_index(self.v) } else { 0 };
            let entry = self.palette[i];
            self.output(entry, self.color(entry));
        }
        if dot == 1 {
            if line == 241 {
                if !self.suppress_vblank {
                    self.status |= 0x80;
                    self.update_nmi();
                }
                self.suppress_vblank = false;
            } else if prerender {
                self.status &= !0xe0;
                self.update_nmi();
            }
        } else if dot == 338 && prerender {
            // Whether this frame is short is settled here, two dots before
            // the dot it skips (as blargg's even_odd_timing measures).
            self.skip_dot = self.rendering();
        }

        self.clock = self.clock.wrapping_add(1);
        self.dot += 1;
        if self.dot == 341 {
            self.dot = 0;
            self.scanline += 1;
            if self.scanline == 262 {
                self.scanline = 0;
                self.frames += 1;
                self.picture.drawn = 0;
                // With rendering on, every other frame is a dot shorter: the
                // first, idle dot of line 0 goes.
                if self.frames.is_multiple_of(2) && self.skip_dot {
                    self.dot = 1;
                }
            }
        }
    }

    /// A dot of a visible or the pre-render line, with rendering on, as the
    /// chip's timing has it. Dots 1-256 draw a pixel and fetch the tile 16
    /// pixels ahead (name, attribute, pattern low and high, two dots each);
    /// 257-320 fetch the next line's sprites; 321-336 the next line's first
    /// two tiles. The shifters move a pixel a dot from dot 2 to 257 and from
    /// 322 to 337, and take the next fetched tile every eight.
    fn render(&mut self, cart: &mut Cart, line: u16, dot: u16, visible: bool) {
        match dot {
            1..=256 => {
                if dot >= 2 {
                    self.shift();
                    if dot & 7 == 1 {
                        self.load_shifters();
                    }
                }
                if visible {
                    self.pixel(dot - 1);
                    if dot == 65 {
                        self.evaluate_sprites(line);
                    }
                    if dot == self.overflow_dot {
                        self.status |= 0x20;
                    }
                }
                self.fetch_tile(cart, dot);
                if dot == 256 {
                    self.increment_y();
                }
            }
            257..=320 => {
                if dot == 257 {
                    self.shift();
                    self.load_shifters();
                    self.v = self.v & !0x041f | self.t & 0x041f;
                    if visible && dot == self.overflow_dot {
                        self.status |= 0x20;
                    }
                } else if !visible && (280..=304).contains(&dot) {
                    self.v = self.v & !0x7be0 | self.t & 0x7be0;
                }
                // Eight dots a sprite slot; OAMADDR is held at 0.
                self.oam_addr = 0;
                let slot = usize::from(dot - 257) / 8;
                match (dot - 257) & 7 {
                    4 => self.fetch_sprite(slot, line, cart, false),
                    6 => self.fetch_sprite(slot, line, cart, true),
                    _ => {}
                }
                if dot == 320 {
                    self.sprite_count = if visible { self.found_count } else { 0 };
                    self.compose_sprites(cart, line);
                }
            }
            321..=337 => {
                if dot >= 322 {
                    self.shift();
                    if dot & 7 == 1 {
                        self.load_shifters();
                    }
                }
                if dot <= 336 {
                    self.fetch_tile(cart, dot);
                }
            }
            // The two unused nametable fetches at the end of the line.
            338 | 340 => self.tile = self.vram_read(0x2000 | self.v & 0x0fff, cart),
            _ => {}
        }
    }

    /// The background fetch of a dot: two dots each for a tile's name,
    /// attribute and pattern bytes, then a move of v to the next tile.
    fn fetch_tile(&mut self, cart: &mut Cart, dot: u16) {
        match dot & 7 {
            1 => self.tile = self.vram_read(0x2000 | self.v & 0x0fff, cart),
            3 => {
                let v = self.v;
                let at = self.vram_read(0x23c0 | v & 0x0c00 | v >> 4 & 0x38 | v >> 2 & 0x07, cart);
                self.attribute = at >> (v >> 4 & 4 | v & 2) & 3;
            }
            5 => self.pattern_lo = self.fetch(cart, self.pattern_addr()),
            7 => self.pattern_hi = self.fetch(cart, self.pattern_addr() + 8),
            0 => self.increment_x(),
            _ => {}
        }
    }

    /// The next line's sprites, laid out by x: the first opaque sprite at
    /// each pixel, so drawing a pixel takes one look.
    fn compose_sprites(&mut self, cart: &Cart, line: u16) {
        self.sprite_line = [0; WIDTH];
        if !self.sprite_limit && self.sprite_count == 8 {
            self.compose_past_eight(cart, line);
        }
        for s in self.sprites[..self.sprite_count].iter().rev() {
            draw_sprite(&mut self.sprite_line, s);
        }
    }

    /// Without the limit: the sprites on the next line past the eight the
    /// chip found, under them, their patterns read where the cartridge does
    /// not see it. Not when one of the eight is empty all through: then the
    /// game fills the line's slots on purpose, to hide what comes after.
    fn compose_past_eight(&mut self, cart: &Cart, line: u16) {
        let height = if self.ctrl & 0x20 != 0 { 16 } else { 8 };
        let past = (usize::from(self.found[7]) + 1..64)
            .filter(|&n| line.wrapping_sub(u16::from(self.oam[n * 4])) < height)
            .fold(0u64, |past, n| past | 1 << n);
        if past == 0 || self.found.iter().any(|&n| self.empty_sprite(cart, self.oam[usize::from(n) * 4 + 1])) {
            return;
        }
        for n in (0..64).rev().filter(|n| past >> n & 1 != 0) {
            let [y, tile, attr, x] = [0, 1, 2, 3].map(|i| self.oam[n * 4 + i]);
            let addr = self.sprite_addr(line, y, tile, attr);
            let flip = |bits: u8| if attr & 0x40 != 0 { bits.reverse_bits() } else { bits };
            let (lo, hi) = (flip(cart.chr_read(addr)), flip(cart.chr_read(addr + 8)));
            draw_sprite(&mut self.sprite_line, &Sprite { x, attr, lo, hi, zero: false });
        }
    }

    /// Whether a sprite's tile (both of them, for 8x16 sprites) has no
    /// opaque pixel.
    fn empty_sprite(&self, cart: &Cart, tile: u8) -> bool {
        let (start, len) = if self.ctrl & 0x20 != 0 {
            (u16::from(tile & 1) << 12 | u16::from(tile & 0xfe) << 4, 32)
        } else {
            (u16::from(self.ctrl & 0x08) << 9 | u16::from(tile) << 4, 16)
        };
        (start..start + len).all(|addr| cart.chr_read(addr) == 0)
    }

    /// A pattern fetch, which the cartridge sees on the address bus.
    fn fetch(&self, cart: &mut Cart, addr: u16) -> u8 {
        cart.watch(addr, self.clock);
        cart.chr_read(addr)
    }

    fn pattern_addr(&self) -> u16 {
        u16::from(self.ctrl & 0x10) << 8 | u16::from(self.tile) << 4 | self.v >> 12 & 7
    }

    fn shift(&mut self) {
        self.bg_head = self.bg_head.wrapping_add(1);
    }

    /// The fetched tile into the low half of the shift registers: pixels 8
    /// to 15 of the ring.
    fn load_shifters(&mut self) {
        // Eight 2-bit pixels a byte each, and the attribute on those not 0.
        let p = SPREAD[usize::from(self.pattern_lo)] | SPREAD[usize::from(self.pattern_hi)] << 1;
        let opaque = (p | p >> 1) & 0x0101_0101_0101_0101;
        let bytes = (p | opaque * u64::from(self.attribute << 2)).to_le_bytes();
        let start = usize::from(self.bg_head.wrapping_add(8));
        for (i, &byte) in bytes.iter().enumerate() {
            self.bg[(start + i) & 15] = byte;
        }
    }

    /// Coarse x, wrapping into the next nametable across.
    fn increment_x(&mut self) {
        if self.v & 0x001f == 31 {
            self.v = self.v & !0x001f ^ 0x0400;
        } else {
            self.v += 1;
        }
    }

    /// Fine y, then coarse y, wrapping into the next nametable down after
    /// row 29 (rows 30 and 31 are attributes, and wrap within the table).
    fn increment_y(&mut self) {
        if self.v & 0x7000 != 0x7000 {
            self.v += 0x1000;
            return;
        }
        self.v &= !0x7000;
        let y = match self.v >> 5 & 31 {
            29 => {
                self.v ^= 0x0800;
                0
            }
            31 => 0,
            y => y + 1,
        };
        self.v = self.v & !0x03e0 | y << 5;
    }

    /// Picks the sprites for the next line, as the chip does from dot 65:
    /// the first eight in range, then, looking for a ninth, the chip's bug
    /// that reads the wrong byte of each entry, which makes the overflow
    /// flag unreliable in the same way. The flag goes up at the dot the
    /// chip gets there: two dots an entry, eight for one it copies.
    fn evaluate_sprites(&mut self, line: u16) {
        let height = if self.ctrl & 0x20 != 0 { 16 } else { 8 };
        let in_range = |y: u8| line.wrapping_sub(u16::from(y)) < height;
        self.found_count = 0;
        self.overflow_dot = u16::MAX;
        let mut dot = 65;
        let mut n = 0;
        while n < 64 && self.found_count < 8 {
            if in_range(self.oam[n * 4]) {
                self.found[self.found_count] = n as u8;
                self.found_count += 1;
                dot += 8;
            } else {
                dot += 2;
            }
            n += 1;
        }
        let mut m = 0;
        while n < 64 && self.found_count == 8 {
            if in_range(self.oam[n * 4 + m]) {
                self.overflow_dot = dot;
                break;
            }
            dot += 2;
            n += 1;
            m = (m + 1) & 3;
        }
    }

    /// One pattern byte of a sprite slot for the next line; empty slots
    /// fetch tile $FF's, as the chip does.
    fn fetch_sprite(&mut self, slot: usize, line: u16, cart: &mut Cart, high: bool) {
        let (y, tile, attr, x, zero) = if slot < self.found_count {
            let i = usize::from(self.found[slot]) * 4;
            (self.oam[i], self.oam[i + 1], self.oam[i + 2], self.oam[i + 3], i == 0)
        } else {
            (0xff, 0xff, 0xff, 0xff, false)
        };
        let addr = self.sprite_addr(line, y, tile, attr);
        let mut bits = self.fetch(cart, addr + if high { 8 } else { 0 });
        if attr & 0x40 != 0 {
            bits = bits.reverse_bits();
        }
        if slot < 8 {
            let s = &mut self.sprites[slot];
            if high {
                s.hi = bits;
            } else {
                *s = Sprite { x, attr, lo: bits, hi: 0, zero };
            }
        }
    }

    /// Where the pattern of a sprite's row on the next line is (its low
    /// byte; the high one is 8 on).
    fn sprite_addr(&self, line: u16, y: u8, tile: u8, attr: u8) -> u16 {
        let tall = self.ctrl & 0x20 != 0;
        let mut row = line.wrapping_sub(u16::from(y)) & if tall { 15 } else { 7 };
        if attr & 0x80 != 0 {
            row = if tall { 15 } else { 7 } - row;
        }
        if tall {
            u16::from(tile & 1) << 12 | u16::from(tile & 0xfe) << 4 | (row & 8) << 1 | row & 7
        } else {
            u16::from(self.ctrl & 0x08) << 9 | u16::from(tile) << 4 | row
        }
    }

    /// The pixel at x of the current line: background and sprites, by
    /// priority, with sprite 0 hit.
    fn pixel(&mut self, x: u16) {
        let left = x < 8;
        let mut bg = 0;
        if self.mask & 0x08 != 0 && !(left && self.mask & 0x02 == 0) {
            bg = self.bg[usize::from(self.bg_head.wrapping_add(self.x)) & 15];
        }
        let mut color = bg;
        if self.mask & 0x10 != 0 && !(left && self.mask & 0x04 == 0) {
            // The first opaque sprite decides, even behind the background.
            let sprite = self.sprite_line[usize::from(x)];
            if sprite != 0 {
                if sprite & SPRITE_ZERO != 0 && bg != 0 && x != 255 {
                    self.status |= 0x40;
                }
                if bg == 0 || sprite & BEHIND == 0 {
                    color = sprite & 0x1f;
                }
            }
        }
        self.output(self.entry(color), self.colors[usize::from(color)]);
    }

    /// The next pixel of the picture, from a palette entry: the pixels of a
    /// frame come one a dot.
    fn output(&mut self, entry: u8, pixel: u16) {
        self.recent = [self.recent[1], entry];
        let i = self.picture.drawn;
        if let Some(p) = self.picture.pixels.get_mut(i) {
            *p = pixel;
            self.picture.drawn = i + 1;
        }
    }

    /// What a palette entry looks like in the picture, with $2001's
    /// greyscale and colour emphasis.
    fn color(&self, entry: u8) -> u16 {
        let grey = if self.mask & 0x01 != 0 { 0x30 } else { 0x3f };
        u16::from(entry & grey) | u16::from(self.mask >> 5) << 6
    }

    /// The palette entry of a background or sprite pixel's index. Indexes
    /// whose low two bits are 0 are transparent: the backdrop colour.
    fn entry(&self, index: u8) -> u8 {
        self.palette[if index & 3 == 0 { 0 } else { usize::from(index) }]
    }

    /// Greyscale and emphasis changed: they reach the pixels of this line
    /// from two before the last dot run, which may be drawn already.
    fn recolor_recent(&mut self) {
        if self.scanline >= 240 || self.dot == 0 {
            return;
        }
        let first = usize::from(self.scanline) * WIDTH + usize::from(self.dot - 1).saturating_sub(2);
        let (drawn, recent) = (self.picture.drawn, self.recent);
        for (back, &entry) in recent.iter().rev().enumerate() {
            match drawn.checked_sub(back + 1) {
                Some(i) if i >= first => self.picture.pixels[i] = self.color(entry),
                _ => break,
            }
        }
    }

    /// Works out `colors` again, when the palette or $2001 change.
    fn update_colors(&mut self) {
        for i in 0..32 {
            self.colors[usize::from(i)] = self.color(self.entry(i));
        }
    }

    /// NMI is asserted while vblank is on and enabled.
    fn update_nmi(&mut self) {
        self.nmi = self.status & 0x80 != 0 && self.ctrl & 0x80 != 0;
    }

    /// The I/O latch, its stale bits faded.
    fn open_bus(&mut self) -> u8 {
        for (bit, &frame) in self.latch_frames.iter().enumerate() {
            if self.frames - frame > 36 {
                self.latch &= !(1 << bit);
            }
        }
        self.latch
    }

    /// Puts a value's bits under mask into the latch, refreshing them.
    fn refresh(&mut self, value: u8, mask: u8) {
        self.latch = self.latch & !mask | value & mask;
        for bit in 0..8 {
            if mask & 1 << bit != 0 {
                self.latch_frames[bit] = self.frames;
            }
        }
    }

    /// A CPU read of $2000-$2007 (mirrored up to $3FFF). What a register
    /// does not drive comes from the latch.
    pub fn read_register(&mut self, addr: u16, cart: &mut Cart) -> u8 {
        let open = self.open_bus();
        match addr & 7 {
            2 => {
                // Read the dot before vblank starts, the flag reads clear and
                // is not set this frame, nor is there an NMI. (Read in its
                // first dots, it reads set and is cleared in time to keep
                // the CPU from seeing the NMI.)
                if self.scanline == 241 && self.dot == 1 {
                    self.suppress_vblank = true;
                }
                let value = self.status & 0xe0 | open & 0x1f;
                self.refresh(value, 0xe0);
                self.status &= !0x80;
                self.update_nmi();
                self.w = false;
                value
            }
            4 => {
                let value = self.oam[usize::from(self.oam_addr)];
                self.refresh(value, 0xff);
                value
            }
            7 => {
                let addr = self.v & 0x3fff;
                let value = if addr >= 0x3f00 {
                    // Palette reads are immediate, six bits; the buffer
                    // gets the nametable byte underneath.
                    let value = open & 0xc0 | self.palette[palette_index(addr)];
                    self.refresh(value, 0x3f);
                    self.buffer = self.vram_read(addr - 0x1000, cart);
                    value
                } else {
                    let value = self.buffer;
                    self.refresh(value, 0xff);
                    self.buffer = self.vram_read(addr, cart);
                    value
                };
                self.increment(cart);
                value
            }
            _ => open,
        }
    }

    /// A CPU write of $2000-$2007 (mirrored up to $3FFF).
    pub fn write_register(&mut self, addr: u16, value: u8, cart: &mut Cart) {
        self.refresh(value, 0xff);
        match addr & 7 {
            0 => {
                self.ctrl = value;
                self.t = self.t & !0x0c00 | u16::from(value & 3) << 10;
                self.update_nmi();
            }
            1 => {
                let changed = (self.mask ^ value) & 0xe1 != 0;
                self.mask = value;
                self.update_colors();
                if changed {
                    self.recolor_recent();
                }
            }
            3 => self.oam_addr = value,
            4 => self.write_oam(value),
            5 => {
                if self.w {
                    self.t = self.t & 0x8c1f | u16::from(value & 7) << 12 | u16::from(value & 0xf8) << 2;
                } else {
                    self.t = self.t & !0x001f | u16::from(value >> 3);
                    self.x = value & 7;
                }
                self.w = !self.w;
            }
            6 => {
                if self.w {
                    self.t = self.t & 0xff00 | u16::from(value);
                    self.v = self.t;
                    cart.watch(self.v, self.clock);
                } else {
                    self.t = self.t & 0x00ff | u16::from(value & 0x3f) << 8;
                }
                self.w = !self.w;
            }
            7 => {
                self.vram_write(self.v & 0x3fff, value, cart);
                self.increment(cart);
            }
            _ => {}
        }
    }

    /// One byte into OAM, as $2004 and sprite DMA write it. Bits 2-4 of a
    /// sprite's attributes do not exist.
    pub fn write_oam(&mut self, value: u8) {
        let value = if self.oam_addr & 3 == 2 { value & 0xe3 } else { value };
        self.oam[usize::from(self.oam_addr)] = value;
        self.oam_addr = self.oam_addr.wrapping_add(1);
    }

    /// $2007 moves v by 1, or by 32 (a row of the nametable); the new
    /// address goes out on the bus.
    fn increment(&mut self, cart: &mut Cart) {
        self.v = self.v.wrapping_add(if self.ctrl & 0x04 != 0 { 32 } else { 1 }) & 0x7fff;
        cart.watch(self.v, self.clock);
    }

    fn vram_read(&self, addr: u16, cart: &mut Cart) -> u8 {
        cart.watch(addr, self.clock);
        match addr {
            0x0000..=0x1fff => cart.chr_read(addr),
            0x2000..=0x3eff => self.vram[nametable(addr, cart.mirroring)],
            _ => self.palette[palette_index(addr)],
        }
    }

    fn vram_write(&mut self, addr: u16, value: u8, cart: &mut Cart) {
        cart.watch(addr, self.clock);
        match addr {
            0x0000..=0x1fff => cart.chr_write(addr, value),
            0x2000..=0x3eff => self.vram[nametable(addr, cart.mirroring)] = value,
            _ => {
                self.palette[palette_index(addr)] = value & 0x3f;
                self.update_colors();
            }
        }
    }
}

/// The picture: each pixel a palette index (bits 0-5) with the colour
/// emphasis bits (6-8) of $2001.
#[derive(Clone)]
pub struct Picture {
    pub pixels: Box<[u16; WIDTH * HEIGHT]>,
    /// The pixels drawn so far of the frame being drawn.
    drawn: usize,
}

/// A state holds the part of the frame being drawn that is already there:
/// a few pixels when saved between frames, as a frame ends just into the
/// next one's first line.
impl State for Picture {
    fn save(&self, out: &mut Vec<u8>) {
        self.drawn.save(out);
        self.pixels[..self.drawn].iter().for_each(|p| p.save(out));
    }

    fn load(&mut self, input: &mut &[u8]) -> Result<(), BadState> {
        self.drawn.load(input)?;
        if self.drawn > WIDTH * HEIGHT {
            return Err(BadState);
        }
        self.pixels[..self.drawn].iter_mut().try_for_each(|p| p.load(input))
    }
}

/// A sprite's row into a line of sprite pixels, over what is there.
fn draw_sprite(line: &mut [u8; WIDTH], s: &Sprite) {
    let flags = if s.zero { SPRITE_ZERO } else { 0 } | if s.attr & 0x20 != 0 { BEHIND } else { 0 };
    for dx in 0..8 {
        let x = usize::from(s.x) + dx;
        if x >= WIDTH {
            break;
        }
        let p = (s.hi >> (7 - dx) & 1) << 1 | s.lo >> (7 - dx) & 1;
        if p != 0 {
            line[x] = flags | 0x10 | (s.attr & 3) << 2 | p;
        }
    }
}

/// Where a nametable address ($2000-$3EFF) lands in video RAM: the four
/// nametables of the address space share two kilobytes but on four-screen
/// cartridges.
fn nametable(addr: u16, mirroring: Mirroring) -> usize {
    let table = usize::from(addr >> 10 & 3);
    let page = match mirroring {
        Mirroring::Horizontal => table >> 1,
        Mirroring::Vertical => table & 1,
        Mirroring::SingleLow => 0,
        Mirroring::SingleHigh => 1,
        Mirroring::FourScreen => table,
    };
    page << 10 | usize::from(addr & 0x3ff)
}

/// The sprite palettes' first entries are the background ones'.
fn palette_index(addr: u16) -> usize {
    let i = usize::from(addr & 0x1f);
    if i & 0x13 == 0x10 { i & 0x0f } else { i }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cartridge whose tile 1 is solid (colour 1) and tile 0 empty.
    fn cart() -> Cart {
        let mut rom = vec![0; 16 + 0x4000 + 0x2000];
        rom[..6].copy_from_slice(b"NES\x1a\x01\x01");
        rom[16 + 0x4000 + 16..][..8].fill(0xff);
        Cart::parse(&rom).unwrap()
    }

    /// Ten sprites on line 50, after `empty` empty ones there: the x of
    /// those it draws, with sprites shown and the backdrop black.
    fn drawn(limit: bool, empty: usize) -> Vec<usize> {
        let mut cart = cart();
        let mut ppu = Ppu::new();
        ppu.sprite_limit = limit;
        ppu.mask = 0x14;
        ppu.palette[0] = 0x0f;
        ppu.palette[0x11] = 0x16;
        ppu.update_colors();
        for i in 0..empty + 10 {
            let tile = if i < empty { 0 } else { 1 };
            ppu.oam[i * 4..][..4].copy_from_slice(&[49, tile, 0, (i * 16) as u8]);
        }
        ppu.run(&mut cart, 341 * 60);
        let line = &ppu.picture.pixels[50 * WIDTH..][..WIDTH];
        (0..16).map(|i| i * 16).filter(|&x| line[x] == 0x16).collect()
    }

    #[test]
    fn eight_sprites_a_line() {
        assert_eq!(drawn(true, 0), (0..8).map(|i| i * 16).collect::<Vec<_>>());
    }

    #[test]
    fn all_sprites_a_line_without_the_limit() {
        assert_eq!(drawn(false, 0), (0..10).map(|i| i * 16).collect::<Vec<_>>());
    }

    #[test]
    fn empty_sprites_keep_the_limit() {
        // Eight empty sprites first: none of the ten after them shows.
        assert_eq!(drawn(false, 8), []);
        // Five: the first three show, as on the console.
        assert_eq!(drawn(false, 5), [80, 96, 112]);
    }
}
