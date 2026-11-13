//! The PPU: 154 lines of 456 dots, lines 144-153 the vertical blank. A
//! visible line searches OAM (mode 2, 80 dots), draws (mode 3, 172 dots and
//! more for fine scrolling, the window and sprites), then rests (mode 0).
//! The line's pixels are worked out as mode 3 starts, from the registers as
//! they are then; the PPU otherwise moves from one point of the line where
//! something changes to the next.
//!
//! On the Game Boy Color, with a Color game, VRAM has a second bank of
//! tiles and the background's attributes (palette, bank, flips, priority),
//! and the picture is in colours from eight background and eight sprite
//! palettes of four RGB555 colours.

use savestate::{BadState, State, state, take};

pub const WIDTH: usize = 160;
pub const HEIGHT: usize = 144;

// Interrupt request bits, as IF has them.
const VBLANK: u8 = 0x01;
const STAT: u8 = 0x02;

// What of VRAM and OAM is out of the CPU's reach.
const OAM_READ: u8 = 0x01;
const OAM_WRITE: u8 = 0x02;
const VRAM_READ: u8 = 0x04;
const VRAM_WRITE: u8 = 0x08;

/// The points of a line where something changes, in order.
#[derive(Clone, Copy, PartialEq, Debug)]
#[repr(u8)]
enum Event {
    /// Dot 0: OAM search (mode 2), or the vertical blank (mode 1).
    LineStart,
    /// Dot 76, a cycle before drawing: VRAM can no longer be read, and OAM
    /// can be written again for that cycle.
    VramBlock,
    /// Dot 80: drawing (mode 3).
    Draw,
    /// The end of drawing: the horizontal blank (mode 0).
    HBlank,
    /// Dot 452: LY shows the next line, and the comparison with LYC is off
    /// until it starts; OAM can no longer be read, for the next line's
    /// search.
    NextLy,
    /// Dot 4 of line 153: LY shows 0 for the rest of the frame.
    Ly153,
    /// Dot 456.
    LineEnd,
}

/// The picture: shades 0-3, or RGB555 colours on the Color.
#[derive(Clone)]
pub struct Picture {
    pub pixels: [u16; WIDTH * HEIGHT],
    color: bool,
}

#[derive(Clone)]
pub struct Ppu {
    /// Running a Color game.
    cgb: bool,
    /// Two banks of 8 KB (the second on the Color), and the one the CPU
    /// sees (VBK).
    pub vram: [u8; 0x4000],
    pub vram_bank: usize,
    pub oam: [u8; 0xa0],
    pub picture: Picture,
    /// Frames finished, counted as each vertical blank starts.
    pub frames: u64,
    /// The last value written to DMA, which reads back.
    pub dma: u8,

    lcdc: u8,
    /// STAT's interrupt selects (bits 3-6).
    stat: u8,
    scy: u8,
    scx: u8,
    ly: u8,
    lyc: u8,
    bgp: u8,
    obp: [u8; 2],
    wy: u8,
    wx: u8,

    /// The line being drawn (LY runs ahead of it at the end of a line),
    /// the dot in it, and the next point where something changes.
    line: u8,
    dot: u16,
    next: Event,
    next_dot: u16,
    /// Where drawing ends on this line.
    hblank_dot: u16,
    mode: u8,
    /// LY=LYC, as the PPU last compared them; it holds while the LCD is off.
    coincidence: bool,
    /// OAM_READ, OAM_WRITE, VRAM_READ and VRAM_WRITE.
    blocked: u8,
    /// LY has equalled WY this frame, so the window shows from WX on; the
    /// window's own line counter only moves on lines it is drawn.
    window_hit: bool,
    window_line: u8,
    /// The STAT interrupt line: an interrupt is its rising edge.
    stat_line: bool,
    /// The first frame after the LCD is turned on is not shown.
    blank_frame: bool,
    /// Dots a machine cycle: 4, or 2 in the Color's double speed.
    pub dots_per_cycle: u16,
    /// A horizontal blank of a visible line started, for HDMA.
    pub hblank_started: bool,

    /// The Color's palettes, background and sprites: 8 x 4 colours, two
    /// bytes each; the index the CPU reaches them at, with bit 7 to count
    /// it up on writes (BCPS, OCPS); and the sprites' priority (OPRI: by OAM
    /// order, or by x as on the DMG).
    palettes: [[u8; 64]; 2],
    palette_index: [u8; 2],
    opri: u8,
}

state!(Ppu { vram, vram_bank, oam, picture, frames, dma, lcdc, stat, scy, scx, ly, lyc, bgp, obp, wy, wx, line, dot, next,
    next_dot, hblank_dot, mode, coincidence, blocked, window_hit, window_line, stat_line, blank_frame, dots_per_cycle,
    hblank_started, palettes, palette_index, opri });

impl State for Event {
    fn save(&self, out: &mut Vec<u8>) {
        out.push(*self as u8);
    }

    fn load(&mut self, input: &mut &[u8]) -> Result<(), BadState> {
        const EVENTS: [Event; 7] =
            [Event::LineStart, Event::VramBlock, Event::Draw, Event::HBlank, Event::NextLy, Event::Ly153, Event::LineEnd];
        *self = *EVENTS.get(usize::from(take(input, 1)?[0])).ok_or(BadState)?;
        Ok(())
    }
}

/// Shades four pixels a byte, colours as they are.
impl State for Picture {
    fn save(&self, out: &mut Vec<u8>) {
        if self.color {
            self.pixels.save(out);
        } else {
            out.extend(self.pixels.chunks(4).map(|p| (p[0] | p[1] << 2 | p[2] << 4 | p[3] << 6) as u8));
        }
    }

    fn load(&mut self, input: &mut &[u8]) -> Result<(), BadState> {
        if self.color {
            return self.pixels.load(input);
        }
        for (pixels, byte) in self.pixels.chunks_mut(4).zip(take(input, WIDTH * HEIGHT / 4)?) {
            for (i, pixel) in pixels.iter_mut().enumerate() {
                *pixel = u16::from(byte >> (i * 2) & 3);
            }
        }
        Ok(())
    }
}

impl Ppu {
    pub fn new(cgb: bool) -> Ppu {
        Ppu {
            cgb,
            vram: [0; 0x4000],
            vram_bank: 0,
            oam: [0; 0xa0],
            picture: Picture { pixels: [Self::blank(cgb); WIDTH * HEIGHT], color: cgb },
            frames: 0,
            dma: 0xff,
            lcdc: 0x91,
            stat: 0,
            scy: 0,
            scx: 0,
            ly: 0,
            lyc: 0,
            bgp: 0xfc,
            obp: [0xff, 0xff],
            wy: 0,
            wx: 0,
            line: 0,
            dot: 0,
            next: Event::VramBlock,
            next_dot: 76,
            hblank_dot: 252,
            mode: 2,
            coincidence: true,
            blocked: OAM_READ | OAM_WRITE,
            window_hit: false,
            window_line: 0,
            stat_line: false,
            blank_frame: false,
            dots_per_cycle: 4,
            hblank_started: false,
            // The boot ROM leaves the background palettes white.
            palettes: [[0xff; 64], [0; 64]],
            palette_index: [0; 2],
            opri: 0,
        }
    }

    /// What a blank screen shows: shade 0, or white.
    fn blank(cgb: bool) -> u16 {
        if cgb { 0x7fff } else { 0 }
    }

    /// The line being drawn.
    pub fn line(&self) -> u8 {
        self.line
    }

    pub fn mode(&self) -> u8 {
        self.mode
    }

    pub fn on(&self) -> bool {
        self.lcdc & 0x80 != 0
    }

    /// One machine cycle: four dots, or two in double speed. Returns the
    /// interrupts it requests.
    #[inline]
    pub fn tick(&mut self) -> u8 {
        if !self.on() {
            return 0;
        }
        self.dot += self.dots_per_cycle;
        if self.dot < self.next_dot { 0 } else { self.events() }
    }

    #[inline(never)]
    fn events(&mut self) -> u8 {
        let mut requested = 0;
        while self.dot >= self.next_dot {
            requested |= self.event();
        }
        requested
    }

    /// Does what happens at the next point of the line, and finds the one
    /// after.
    fn event(&mut self) -> u8 {
        let mut requested = 0;
        let mut momentary = false;
        let (next, next_dot) = match self.next {
            Event::LineStart => {
                self.coincidence = self.line == self.lyc;
                if self.line < 144 {
                    self.mode = 2;
                    self.blocked = OAM_READ | OAM_WRITE;
                    self.window_hit |= self.line == self.wy;
                    (Event::VramBlock, 76)
                } else {
                    self.mode = 1;
                    if self.line == 144 {
                        self.frames += 1;
                        self.blank_frame = false;
                        requested |= VBLANK;
                        // The mode 2 interrupt goes off here too.
                        momentary = self.stat & 0x20 != 0;
                    }
                    if self.line == 153 { (Event::Ly153, 4) } else { (Event::NextLy, 452) }
                }
            }
            Event::VramBlock => {
                self.blocked = OAM_READ | VRAM_READ;
                (Event::Draw, 80)
            }
            Event::Draw => {
                self.mode = 3;
                self.blocked = OAM_READ | OAM_WRITE | VRAM_READ | VRAM_WRITE;
                self.hblank_dot = self.draw_line();
                (Event::HBlank, self.hblank_dot)
            }
            Event::HBlank => {
                self.mode = 0;
                self.blocked = 0;
                self.hblank_started = true;
                (Event::NextLy, 452)
            }
            Event::Ly153 => {
                self.ly = 0;
                self.coincidence = self.lyc == 0;
                (Event::NextLy, 452)
            }
            Event::NextLy => {
                if self.line != 153 {
                    self.ly = self.line + 1;
                    self.coincidence = false;
                }
                if self.line < 143 || self.line == 153 {
                    self.blocked = OAM_READ;
                }
                (Event::LineEnd, 456)
            }
            Event::LineEnd => {
                self.dot -= 456;
                self.line = if self.line == 153 { 0 } else { self.line + 1 };
                if self.line == 0 {
                    self.window_hit = false;
                    self.window_line = 0;
                }
                (Event::LineStart, 0)
            }
        };
        self.next = next;
        self.next_dot = next_dot;
        requested | self.update_stat(momentary)
    }

    /// The STAT interrupt is requested when any of its selected conditions
    /// starts to hold while none did; `momentary` is the mode 2 condition
    /// holding for an instant.
    fn update_stat(&mut self, momentary: bool) -> u8 {
        let line = (self.mode == 0 && self.stat & 0x08 != 0)
            || (self.mode == 1 && self.stat & 0x10 != 0)
            || (self.mode == 2 && self.stat & 0x20 != 0)
            || (self.coincidence && self.stat & 0x40 != 0);
        let rising = (line || momentary) && !self.stat_line;
        self.stat_line = line;
        if rising { STAT } else { 0 }
    }

    /// The line's pixels: background, window, then up to ten sprites.
    /// Returns the dot where drawing them ends: fine scrolling, the window
    /// and each sprite hold the pixel fetcher up.
    fn draw_line(&mut self) -> u16 {
        let ly = self.line;
        let mut end = 252 + u16::from(self.scx & 7);
        // Each pixel's background colour (0-3), and on the Color its tile's
        // attributes. On the DMG, LCDC bit 0 blanks the background and the
        // window; on the Color it only lets sprites over them.
        let background = self.cgb || self.lcdc & 0x01 != 0;
        let mut colors = [0u8; WIDTH];
        let mut attrs = [0u8; WIDTH];
        if background {
            let map = if self.lcdc & 0x08 != 0 { 0x1c00 } else { 0x1800 };
            self.map_row(map, self.scx, ly.wrapping_add(self.scy), &mut colors, &mut attrs);
            if self.lcdc & 0x20 != 0 && self.window_hit && self.wx <= 166 {
                let map = if self.lcdc & 0x40 != 0 { 0x1c00 } else { 0x1800 };
                let window_x = usize::from(self.wx).saturating_sub(7);
                let skip = 7u8.saturating_sub(self.wx);
                self.map_row(map, skip, self.window_line, &mut colors[window_x..], &mut attrs[window_x..]);
                self.window_line += 1;
                end += 6;
            }
        }

        // Each pixel's sprite colour (0 for none) and attributes: the first
        // sprite in priority order with a colour there takes it.
        let mut sprite_colors = [0u8; WIDTH];
        let mut sprite_attrs = [0u8; WIDTH];
        if self.lcdc & 0x02 != 0 {
            let height = if self.lcdc & 0x04 != 0 { 16 } else { 8 };
            // The first ten sprites on the line, by OAM order.
            let mut found = [(0u8, 0usize); 10];
            let mut count = 0;
            for i in 0..40 {
                let top = i16::from(self.oam[i * 4]) - 16;
                if count < 10 && (top..top + height).contains(&i16::from(ly)) {
                    found[count] = (self.oam[i * 4 + 1], i);
                    count += 1;
                }
            }
            let mut by_x = found;
            let by_x = &mut by_x[..count];
            by_x.sort_unstable();
            // Each sprite on screen holds the fetcher up 6 dots, and the
            // first over a background tile also until that tile's fetch is
            // done: longer the further left in the tile it starts. (Mode 0
            // shows in STAT 3 dots sooner than that adds up to.)
            let mut last_tile = None;
            let mut held = 0;
            for &(sx, _) in by_x.iter().filter(|&&(sx, _)| sx < 168) {
                let pixel = u16::from(sx) + u16::from(self.scx & 7);
                held += 6;
                if last_tile != Some(pixel / 8) {
                    last_tile = Some(pixel / 8);
                    held += 5u16.saturating_sub(pixel % 8);
                }
            }
            end += held.saturating_sub(3);
            // Priority: OAM order on the Color, the lowest x (then OAM order)
            // on the DMG.
            let order: &[(u8, usize)] = if self.cgb && self.opri & 1 == 0 { &found[..count] } else { by_x };
            for &(sx, i) in order {
                let attr = self.oam[i * 4 + 3];
                let mut row = (i16::from(ly) - (i16::from(self.oam[i * 4]) - 16)) as u8;
                if attr & 0x40 != 0 {
                    row = height as u8 - 1 - row;
                }
                let mut tile = self.oam[i * 4 + 2];
                if height == 16 {
                    tile = tile & 0xfe | row >> 3;
                }
                let bank = if self.cgb { usize::from(attr >> 3 & 1) * 0x2000 } else { 0 };
                let (lo, hi) = self.tile_row(bank + usize::from(tile) * 16, row & 7);
                for px in 0..8 {
                    let x = i16::from(sx) - 8 + px;
                    if !(0..WIDTH as i16).contains(&x) || sprite_colors[x as usize] != 0 {
                        continue;
                    }
                    let bit = if attr & 0x20 != 0 { px } else { 7 - px };
                    let color = (hi >> bit & 1) << 1 | lo >> bit & 1;
                    if color != 0 {
                        sprite_colors[x as usize] = color;
                        sprite_attrs[x as usize] = attr;
                    }
                }
            }
        }

        // A sprite goes behind the background's colours 1-3 if it asks to
        // (or, on the Color, the background tile does, unless LCDC bit 0 is
        // clear).
        let mut line = [0u16; WIDTH];
        for x in 0..WIDTH {
            let (color, attr) = (colors[x], attrs[x]);
            let (sprite, sprite_attr) = (sprite_colors[x], sprite_attrs[x]);
            line[x] = if self.cgb {
                let behind = color != 0 && self.lcdc & 0x01 != 0 && (attr | sprite_attr) & 0x80 != 0;
                if sprite != 0 && !behind {
                    self.color(1, sprite_attr & 7, sprite)
                } else {
                    self.color(0, attr & 7, color)
                }
            } else if sprite != 0 && !(sprite_attr & 0x80 != 0 && color != 0) {
                u16::from(self.obp[usize::from(sprite_attr >> 4 & 1)] >> (sprite * 2) & 3)
            } else if background {
                u16::from(self.bgp >> (color * 2) & 3)
            } else {
                0
            };
        }
        if !self.blank_frame {
            let start = usize::from(ly) * WIDTH;
            self.picture.pixels[start..start + WIDTH].copy_from_slice(&line);
        }
        end
    }

    /// Fills pixels from a 256 x 256 tile map, from column x of row y on:
    /// each pixel's colour (0-3) and, on the Color, its tile's attributes
    /// (palette, VRAM bank, flips, priority), kept in the second bank.
    fn map_row(&self, map: usize, mut x: u8, y: u8, colors: &mut [u8], attrs: &mut [u8]) {
        let mut i = 0;
        while i < colors.len() {
            let offset = map + usize::from(y / 8) * 32 + usize::from(x / 8);
            let index = self.vram[offset];
            let attr = if self.cgb { self.vram[0x2000 + offset] } else { 0 };
            // Tiles from 8000 counting up, or from 9000 counting both ways.
            let tile = if self.lcdc & 0x10 != 0 { usize::from(index) } else { (256 + i32::from(index as i8)) as usize };
            let row = if attr & 0x40 != 0 { 7 - (y & 7) } else { y & 7 };
            let (lo, hi) = self.tile_row(usize::from(attr >> 3 & 1) * 0x2000 + tile * 16, row);
            let pixels: [u8; 8] = std::array::from_fn(|px| {
                let bit = if attr & 0x20 != 0 { px } else { 7 - px };
                (hi >> bit & 1) << 1 | lo >> bit & 1
            });
            // The tile's pixels from x on, to its end or the line's.
            let first = usize::from(x & 7);
            let n = (8 - first).min(colors.len() - i);
            colors[i..i + n].copy_from_slice(&pixels[first..first + n]);
            if self.cgb {
                attrs[i..i + n].fill(attr);
            }
            i += n;
            x = x.wrapping_add(n as u8);
        }
    }

    fn tile_row(&self, tile: usize, row: u8) -> (u8, u8) {
        let i = tile + usize::from(row) * 2;
        (self.vram[i], self.vram[i + 1])
    }

    /// A colour of the Color's palettes: background (0) or sprites (1).
    fn color(&self, kind: usize, palette: u8, color: u8) -> u16 {
        let i = usize::from(palette) * 8 + usize::from(color) * 2;
        u16::from_le_bytes([self.palettes[kind][i], self.palettes[kind][i + 1]]) & 0x7fff
    }

    /// VRAM is out of the CPU's reach while the PPU draws, OAM while it
    /// searches it and draws.
    pub fn read_vram(&self, addr: u16) -> u8 {
        if self.blocked & VRAM_READ != 0 { 0xff } else { self.vram[self.vram_bank * 0x2000 + usize::from(addr & 0x1fff)] }
    }

    pub fn write_vram(&mut self, addr: u16, value: u8) {
        if self.blocked & VRAM_WRITE == 0 {
            self.vram[self.vram_bank * 0x2000 + usize::from(addr & 0x1fff)] = value;
        }
    }

    pub fn read_oam(&self, addr: u16) -> u8 {
        if self.blocked & OAM_READ != 0 { 0xff } else { self.oam[usize::from(addr & 0xff)] }
    }

    pub fn write_oam(&mut self, addr: u16, value: u8) {
        if self.blocked & OAM_WRITE == 0 {
            self.oam[usize::from(addr & 0xff)] = value;
        }
    }

    pub fn read_register(&self, addr: u16) -> u8 {
        match addr {
            0xff40 => self.lcdc,
            0xff41 => 0x80 | self.stat | u8::from(self.coincidence) << 2 | if self.on() { self.mode } else { 0 },
            0xff42 => self.scy,
            0xff43 => self.scx,
            0xff44 => self.ly,
            0xff45 => self.lyc,
            0xff46 => self.dma,
            0xff47 => self.bgp,
            0xff48 => self.obp[0],
            0xff49 => self.obp[1],
            0xff4a => self.wy,
            0xff4b => self.wx,
            _ if !self.cgb => 0xff,
            0xff4f => 0xfe | self.vram_bank as u8,
            0xff68 | 0xff6a => 0x40 | self.palette_index[usize::from(addr == 0xff6a)],
            // The palettes are out of the CPU's reach while the PPU draws.
            0xff69 | 0xff6b if self.blocked & VRAM_WRITE != 0 => 0xff,
            0xff69 | 0xff6b => {
                let kind = usize::from(addr == 0xff6b);
                self.palettes[kind][usize::from(self.palette_index[kind] & 0x3f)]
            }
            0xff6c => 0xfe | self.opri,
            _ => 0xff,
        }
    }

    /// The line LYC is compared with now, if the comparison is on: it is
    /// off from LY moving on to the next line starting.
    fn compared_line(&self) -> Option<u8> {
        match self.next {
            Event::LineEnd if self.line != 153 => None,
            _ => Some(self.ly),
        }
    }

    /// Returns the interrupts the write requests (a STAT condition that now
    /// holds).
    pub fn write_register(&mut self, addr: u16, value: u8) -> u8 {
        let mut requested = 0;
        match addr {
            0xff40 => {
                let was_on = self.on();
                self.lcdc = value;
                if was_on && !self.on() {
                    // Off: the PPU stops at line 0, in mode 0; LY=LYC stays as
                    // it was, and the screen is blank.
                    self.picture.pixels.fill(Self::blank(self.cgb));
                    self.line = 0;
                    self.ly = 0;
                    self.dot = 0;
                    self.mode = 0;
                    self.blocked = 0;
                    self.window_hit = false;
                    self.window_line = 0;
                } else if !was_on && self.on() {
                    // On: the first line has no OAM search, and goes from
                    // mode 0 straight to drawing.
                    self.next = Event::Draw;
                    self.next_dot = 80;
                    self.coincidence = self.lyc == 0;
                    self.blank_frame = true;
                }
            }
            0xff41 => {
                // On the DMG, a write to STAT selects every condition for a
                // moment.
                if self.on() && !self.cgb {
                    self.stat = 0x78;
                    requested = self.update_stat(false);
                }
                self.stat = value & 0x78;
            }
            0xff42 => self.scy = value,
            0xff43 => self.scx = value,
            0xff45 => {
                self.lyc = value;
                if self.on()
                    && let Some(line) = self.compared_line()
                {
                    self.coincidence = line == value;
                }
            }
            0xff47 => self.bgp = value,
            0xff48 => self.obp[0] = value,
            0xff49 => self.obp[1] = value,
            0xff4a => self.wy = value,
            0xff4b => self.wx = value,
            _ if !self.cgb => {}
            0xff4f => self.vram_bank = usize::from(value & 1),
            0xff68 | 0xff6a => self.palette_index[usize::from(addr == 0xff6a)] = value & 0xbf,
            0xff69 | 0xff6b => {
                let kind = usize::from(addr == 0xff6b);
                let index = self.palette_index[kind];
                // Writes while the PPU draws are lost, but still count the
                // index up.
                if self.blocked & VRAM_WRITE == 0 {
                    self.palettes[kind][usize::from(index & 0x3f)] = value;
                }
                if index & 0x80 != 0 {
                    self.palette_index[kind] = 0x80 | (index + 1) & 0x3f;
                }
            }
            0xff6c => self.opri = value & 1,
            _ => {}
        }
        if self.on() { requested | self.update_stat(false) } else { requested }
    }
}
