//! The video chip of the Master System (315-5124/5246) and the Game Gear (315-5378): mode 4 only. Behaviour follows
//! Charles MacDonald's VDP and hardware notes, and is that of the SMS 2 and Game Gear chips; the original
//! chip's quirks (the name-table mirroring bit, zoomed sprites past the fourth) are not reproduced. The
//! TMS9918 modes (the SG-1000's) show the backdrop colour only.
//!
//! Time is kept by the machine: it calls `render_line` for each line and `start_of_line` for the line after it, at
//! the moments it picks (`FRAME_IRQ_AT` and the constants after it say where a line begins for the chip's counters and interrupts).

use savestate::state;

/// Where in a 228-cycle line the next line begins for the chip, as the H counter reads at the program's port
/// accesses (which `Hardware` stamps as their instruction starts): the frame interrupt is raised and the line's
/// horizontal scroll latched at H counter $F3, the line interrupt a pixel later, and the V counter steps and the
/// status flags show at $F4 (FluBBa's measurements, https://github.com/FluBBaOfWard/SmsVdpTest; his test ROM
/// agrees with these cycles: the V counter's step, a handler's flags and both interrupts' H counters). The H counter
/// reads $F3 at cycle 211, $F4 at 212 and 213 and $F5 at 214; the flags are measured at $F5, a cycle after the step
/// here, which a port access stamped as its instruction starts cannot tell apart.
pub(crate) const FRAME_IRQ_AT: u32 = 211;
pub(crate) const LINE_IRQ_AT: u32 = 212;
/// Where the V counter steps and the status flags show, together.
pub(crate) const STEP_AT: u32 = 212;
/// A port access happens about 12 cycles after its instruction starts, so the interrupt, which the Z80 takes at
/// instruction boundaries, reaches it that many cycles after it is raised (the same test: the H counter a
/// program latches after waiting for the interrupt).
pub(crate) const IRQ_LATENCY: u32 = 12;

/// `Vdp::collision_at` when the line has no collision.
const NO_COLLISION: u16 = u16::MAX;
/// A collision shows in the status as the pixel it happens at is reached: the pixel's x is 3 H counter pixels for
/// 2 cycles, and the flag follows this many cycles later (the cycle FluBBa's test 52 finds for a collision at x = 131).
const COLLISION_DELAY: u32 = 8;

pub const WIDTH: usize = 256;
pub const HEIGHT: usize = 192;
pub const GG_WIDTH: usize = 160;
pub const GG_HEIGHT: usize = 144;
/// Where the Game Gear's window sits in the 256x192 picture.
const GG_LEFT: usize = 48;
const GG_TOP: usize = 24;
const MAX_HEIGHT: usize = 240;

#[derive(Clone)]
pub(crate) struct Vdp {
    vram: Box<[u8; 0x4000]>,
    /// Colour RAM: 32 entries, each 6 bits (--BBGGRR) on the Master System and 12 bits (BBBBGGGGRRRR) on the
    /// Game Gear, where writes go in pairs through a latch.
    cram: [u16; 32],
    cram_latch: u8,
    regs: [u8; 16],
    addr: u16,
    code: u8,
    /// First byte of a command word, kept until the second one arrives.
    latch: u8,
    second: bool,
    read_buffer: u8,
    /// Bit 7 frame interrupt, bit 6 sprite overflow, bit 5 sprite collision.
    status: u8,
    /// The cycle of the current line at which its sprites first collide, which the collision flag waits for
    /// (`NO_COLLISION` when they do not).
    collision_at: u16,
    hint_pending: bool,
    /// The frame interrupt line, up a little before the flag shows in the status.
    vint_irq: bool,
    counter: u8,
    /// Vertical scroll as the frame took it: a write during the picture waits for the next frame.
    vscroll: u8,
    /// Horizontal scroll as the line took it when it began: a write during the line waits for the next one.
    hscroll: u8,
    /// The line being drawn, 0 being the first of the picture.
    line: u16,
    /// The sprites on that line (`find_sprites`), found as it began: derived, so not part of the saved state, and
    /// found again at the start of the next line.
    found: ([(usize, usize); 8], usize),
    gg: bool,
    pal: bool,
    frame: Vec<u16>,
}

state!(Vdp {
    vram,
    cram,
    cram_latch,
    regs,
    addr,
    code,
    latch,
    second,
    read_buffer,
    status,
    collision_at,
    hint_pending,
    vint_irq,
    counter,
    vscroll,
    hscroll,
    line
});

fn rgb565(r: u16, g: u16, b: u16, max: u16) -> u16 {
    // Scale each channel to its 5/6/5 bits, rounding to the nearest.
    let r5 = (r * 31 + max / 2) / max;
    let g6 = (g * 63 + max / 2) / max;
    let b5 = (b * 31 + max / 2) / max;
    (r5 << 11) | (g6 << 5) | b5
}

impl Vdp {
    pub fn new(gg: bool, pal: bool) -> Self {
        let mut regs = [0; 16];
        // What the BIOS leaves, which games that skip their own set-up expect.
        regs[0] = 0x36;
        regs[1] = 0xa0;
        regs[2..=6].fill(0xff);
        regs[10] = 0xff;
        let size = if gg {
            GG_WIDTH * GG_HEIGHT
        } else {
            WIDTH * MAX_HEIGHT
        };
        Self {
            vram: vec![0; 0x4000].into_boxed_slice().try_into().unwrap(),
            cram: [0; 32],
            cram_latch: 0,
            regs,
            addr: 0,
            code: 0,
            latch: 0,
            second: false,
            read_buffer: 0,
            status: 0,
            collision_at: NO_COLLISION,
            hint_pending: false,
            vint_irq: false,
            counter: 0xff,
            vscroll: 0,
            hscroll: 0,
            line: 0,
            found: ([(0, 0); 8], 0),
            gg,
            pal,
            frame: vec![0; size],
        }
    }

    /// VRAM, the colour RAM as little-endian words, and the registers (`Sms::video_memory`).
    pub fn memory(&self) -> Vec<u8> {
        let mut out = self.vram.to_vec();
        out.extend(self.cram.iter().flat_map(|c| c.to_le_bytes()));
        out.extend_from_slice(&self.regs);
        out
    }

    pub fn lines(&self) -> u16 {
        if self.pal { 313 } else { 262 }
    }

    /// Height of the picture: 192 lines, or 224 and 240 when mode 4 asks for them (the Game Gear has 192 only).
    pub fn height(&self) -> usize {
        let (r0, r1) = (self.regs[0], self.regs[1]);
        if self.gg || r0 & 0x04 == 0 || r0 & 0x02 == 0 {
            return HEIGHT;
        }
        // The V counter has no 240-line sequence for 60 Hz (nor does the documentation say what the chip does), so
        // those bits mean the 192-line picture there.
        match (r1 & 0x10 != 0, r1 & 0x08 != 0) {
            (true, false) => 224,
            (false, true) if self.pal => 240,
            _ => HEIGHT,
        }
    }

    pub fn dimensions(&self) -> (usize, usize) {
        if self.gg {
            (GG_WIDTH, GG_HEIGHT)
        } else {
            (WIDTH, self.height())
        }
    }

    pub fn frame(&self) -> &[u16] {
        let (w, h) = self.dimensions();
        &self.frame[..w * h]
    }

    pub fn validate(&self) -> bool {
        self.addr < 0x4000
            && self.code < 4
            && self.status & 0x1f == 0
            && self.line < self.lines()
            && (self.collision_at == NO_COLLISION || u32::from(self.collision_at) < 228)
            && self
                .cram
                .iter()
                .all(|&c| c < if self.gg { 0x1000 } else { 0x40 })
    }

    /// The INT line: the frame interrupt flag and the line interrupt flag, each behind its enable bit.
    pub fn irq(&self) -> bool {
        (self.vint_irq && self.regs[1] & 0x20 != 0)
            || (self.hint_pending && self.regs[0] & 0x10 != 0)
    }

    pub fn set_line(&mut self, line: u16) {
        // A collision on the line before that nobody read has shown by now.
        if self.collision_at != NO_COLLISION {
            self.status |= 0x20;
        }
        self.line = line;
        if line == 0 {
            self.vscroll = self.regs[9];
        }
        let height = self.height();
        self.collision_at = NO_COLLISION;
        self.found = ([(0, 0); 8], 0);
        if usize::from(line) < height && self.regs[1] & 0x40 != 0 && self.regs[0] & 0x04 != 0 {
            let (found, count, _) = self.find_sprites(usize::from(line), height);
            self.found = (found, count);
            // Two sprites are the least that can collide, and most lines have fewer.
            if count >= 2
                && let Some(x) = self.first_collision()
            {
                self.collision_at = (x as u32 * 2 / 3 + COLLISION_DELAY) as u16;
            }
        }
    }

    /// What reading the V counter returns `cycle` cycles into the current line: the next line's number from
    /// the V step time on.
    pub fn v_counter(&self, cycle: u32) -> u8 {
        let next = if self.line + 1 == self.lines() {
            0
        } else {
            self.line + 1
        };
        let line = i32::from(if cycle >= STEP_AT { next } else { self.line });
        let v = match (self.pal, self.height()) {
            (false, 192) if line > 0xda => line - 6,
            (false, 224) if line > 0xea => line - 6,
            (true, 192) if line > 0xf2 => line - 57,
            (true, 224) if line > 258 => line - 57,
            (true, 224) if line > 255 => line - 256,
            (true, 240) if line > 266 => line - 57,
            (true, 240) if line > 255 => line - 256,
            _ => line,
        };
        v as u8
    }

    /// The H counter at `cycle` Z80 cycles into a line of 228: 342 pixels, counted two at a time, which skip
    /// from $93 to $E9 in the blanking. Three counts come in every four cycles and the fourth cycle repeats the
    /// third's count ($00 $01 $02 $02 $03 $04 $05 $05 ...), as the values latched on hardware in FluBBa's VDP test
    /// go ($90 $91 $92 $92 $93 $E9 $EA $EA ... $FD $FE $FF $FF).
    pub fn h_counter(cycle: u32) -> u8 {
        let cycle = cycle.min(227);
        let h = 3 * (cycle / 4) + (cycle % 4).min(2);
        (if h >= 0x94 { h + 0x55 } else { h }) as u8
    }

    // ---- ports ----

    pub fn read_data(&mut self) -> u8 {
        self.second = false;
        let value = self.read_buffer;
        self.read_buffer = self.vram[usize::from(self.addr)];
        self.addr = (self.addr + 1) & 0x3fff;
        value
    }

    /// Reads the status register `cycle` cycles into the current line, which clears it.
    pub fn read_status(&mut self, cycle: u32) -> u8 {
        if u32::from(self.collision_at) <= cycle {
            self.status |= 0x20;
            self.collision_at = NO_COLLISION;
        }
        let value = self.status | 0x1f;
        self.status = 0;
        self.vint_irq = false;
        self.hint_pending = false;
        self.second = false;
        value
    }

    pub fn write_data(&mut self, value: u8) {
        self.second = false;
        self.read_buffer = value;
        if self.code == 3 {
            if self.gg {
                if self.addr & 1 == 0 {
                    self.cram_latch = value;
                } else {
                    let at = usize::from(self.addr >> 1) & 31;
                    self.cram[at] = (u16::from(value & 0x0f) << 8) | u16::from(self.cram_latch);
                }
            } else {
                self.cram[usize::from(self.addr) & 31] = u16::from(value & 0x3f);
            }
        } else {
            self.vram[usize::from(self.addr)] = value;
        }
        self.addr = (self.addr + 1) & 0x3fff;
    }

    pub fn write_control(&mut self, value: u8) {
        if !self.second {
            self.latch = value;
            self.addr = (self.addr & 0x3f00) | u16::from(value);
            self.second = true;
            return;
        }
        self.second = false;
        self.code = value >> 6;
        self.addr = (u16::from(value & 0x3f) << 8) | u16::from(self.latch);
        match self.code {
            0 => {
                self.read_buffer = self.vram[usize::from(self.addr)];
                self.addr = (self.addr + 1) & 0x3fff;
            }
            2 => {
                // Registers 11 to 15 do not exist.
                let reg = usize::from(value & 15);
                if reg <= 10 {
                    self.regs[reg] = self.latch;
                }
            }
            _ => {}
        }
    }

    // ---- timing ----

    /// The first stage of `line` beginning (the machine runs it near the end of the line before, or of the last
    /// line of the frame for line 0): the line's horizontal scroll is latched and the frame interrupt is raised as
    /// line 193 (the first after the picture) begins.
    pub fn start_of_line(&mut self, line: u16) {
        self.hscroll = self.regs[8];
        if line == self.height() as u16 + 1 {
            self.vint_irq = true;
        }
    }

    /// A little later, the line counter counts the lines of the picture and the one after it, and raises the line
    /// interrupt when it runs out.
    pub fn line_counter(&mut self, line: u16) {
        if line <= self.height() as u16 {
            if self.counter == 0 {
                self.counter = self.regs[10];
                self.hint_pending = true;
            } else {
                self.counter -= 1;
            }
        } else {
            self.counter = self.regs[10];
        }
    }

    /// The second stage, a few cycles later: the status flags of the line that is beginning. The frame flag, and
    /// the overflow flag, which shows to a program that has just seen the V counter step to the line (FluBBa's
    /// VDP test 58 and 59), not only after the line is drawn.
    pub fn line_flags(&mut self, line: u16) {
        let active = self.height() as u16;
        if line == active + 1 {
            self.status |= 0x80;
        }
        if usize::from(line) < usize::from(active)
            && self.regs[1] & 0x40 != 0
            && self.regs[0] & 0x04 != 0
            && self.find_sprites(usize::from(line), usize::from(active)).2
        {
            self.status |= 0x40;
        }
    }

    // ---- drawing ----

    fn color(&self, index: usize) -> u16 {
        let c = self.cram[index & 31];
        if self.gg {
            rgb565(c & 15, (c >> 4) & 15, (c >> 8) & 15, 15)
        } else {
            rgb565(c & 3, (c >> 2) & 3, (c >> 4) & 3, 3)
        }
    }

    /// Draws the current line into the picture.
    pub fn render_line(&mut self) {
        let y = usize::from(self.line);
        let height = self.height();
        if y >= height {
            return;
        }
        let backdrop = 16 + (self.regs[7] & 15);
        let mut line = [backdrop; WIDTH];
        if self.regs[1] & 0x40 != 0 && self.regs[0] & 0x04 != 0 {
            let mut priority = [false; WIDTH];
            self.background(y, height, backdrop, &mut line, &mut priority);
            self.sprites(&mut line, &priority);
            if self.regs[0] & 0x20 != 0 {
                line[..8].fill(backdrop);
            }
        }
        let palette: [u16; 32] = std::array::from_fn(|i| self.color(i));
        if self.gg {
            if (GG_TOP..GG_TOP + GG_HEIGHT).contains(&y) {
                let row = &mut self.frame[(y - GG_TOP) * GG_WIDTH..][..GG_WIDTH];
                for (px, &c) in row.iter_mut().zip(&line[GG_LEFT..GG_LEFT + GG_WIDTH]) {
                    *px = palette[usize::from(c)];
                }
            }
        } else {
            let row = &mut self.frame[y * WIDTH..][..WIDTH];
            for (px, &c) in row.iter_mut().zip(&line) {
                *px = palette[usize::from(c)];
            }
        }
    }

    fn background(
        &self,
        y: usize,
        height: usize,
        backdrop: u8,
        line: &mut [u8; WIDTH],
        priority: &mut [bool; WIDTH],
    ) {
        let r0 = self.regs[0];
        let base = if height == HEIGHT {
            usize::from(self.regs[2] & 0x0e) << 10
        } else {
            (usize::from(self.regs[2] & 0x0c) << 10) | 0x700
        };
        let hscroll = if y < 16 && r0 & 0x40 != 0 {
            0
        } else {
            usize::from(self.hscroll)
        };
        let (fine, start) = (hscroll & 7, (32 - (hscroll >> 3)) & 31);
        // The pixels the fine scroll pushes off the left edge show the backdrop, not the previous column.
        line[..fine].fill(backdrop);
        for c in 0..32 {
            let vscroll = if r0 & 0x80 != 0 && c >= 24 {
                0
            } else {
                usize::from(self.vscroll)
            };
            let ypos = if height == HEIGHT {
                (y + vscroll) % 224
            } else {
                (y + vscroll) & 255
            };
            let (row, fine_y) = (ypos >> 3, ypos & 7);
            let at = base + (row * 32 + ((start + c) & 31)) * 2;
            let entry = usize::from(self.vram[at]) | usize::from(self.vram[at + 1]) << 8;
            let (tile, flip_h, flip_v) = (entry & 0x1ff, entry & 0x200 != 0, entry & 0x400 != 0);
            let palette = if entry & 0x800 != 0 { 16 } else { 0 };
            let front = entry & 0x1000 != 0;
            let pattern = tile * 32 + (if flip_v { 7 - fine_y } else { fine_y }) * 4;
            let planes = &self.vram[pattern..pattern + 4];
            for i in 0..8 {
                let x = fine + c * 8 + i;
                if x >= WIDTH {
                    break;
                }
                let bit = if flip_h { i } else { 7 - i };
                let color = ((planes[0] >> bit) & 1)
                    | (((planes[1] >> bit) & 1) << 1)
                    | (((planes[2] >> bit) & 1) << 2)
                    | (((planes[3] >> bit) & 1) << 3);
                line[x] = color | palette;
                priority[x] = front && color != 0;
            }
        }
    }

    /// The sprites a line shows: the first eight of the table, each with the row of it that falls on the line, and
    /// whether a ninth was found (the overflow).
    fn find_sprites(&self, y: usize, height: usize) -> ([(usize, usize); 8], usize, bool) {
        let r1 = self.regs[1];
        let (tall, zoom) = (r1 & 2 != 0, r1 & 1 != 0);
        let size = if tall { 16 } else { 8 };
        let span = if zoom { size * 2 } else { size };
        let table = usize::from(self.regs[5] & 0x7e) << 7;
        let mut found = [(0usize, 0usize); 8];
        let mut count = 0;
        for i in 0..64 {
            let byte = self.vram[table + i];
            if byte == 0xd0 && height == HEIGHT {
                break;
            }
            // The Y coordinate is one less than the first line; values near the top of the byte are above the
            // picture.
            let mut top = i32::from(byte) + 1;
            if top > 240 {
                top -= 256;
            }
            let row = y as i32 - top;
            if (0..span).contains(&row) {
                if count == 8 {
                    return (found, count, true);
                }
                found[count] = (i, row as usize);
                count += 1;
            }
        }
        (found, count, false)
    }

    /// Calls `visit(x, color)` for each opaque pixel of the sprites found on the current line, the lowest-numbered
    /// sprite's first, those past the edges left out.
    fn sprite_pixels(&self, mut visit: impl FnMut(usize, u8)) {
        let r1 = self.regs[1];
        let (tall, zoom) = (r1 & 2 != 0, r1 & 1 != 0);
        let table = usize::from(self.regs[5] & 0x7e) << 7;
        let shift = if self.regs[0] & 8 != 0 { 8 } else { 0 };
        let (found, count) = self.found;
        for &(i, row) in &found[..count] {
            let x0 = i32::from(self.vram[table + 0x80 + i * 2]) - shift;
            let mut tile = usize::from(self.vram[table + 0x81 + i * 2]);
            let row = if zoom { row / 2 } else { row };
            if tall {
                tile = (tile & 0xfe) + (row >> 3);
            }
            if self.regs[6] & 4 != 0 {
                tile |= 0x100;
            }
            let pattern = tile * 32 + (row & 7) * 4;
            let planes = &self.vram[pattern..pattern + 4];
            for px in 0..8 {
                let bit = 7 - px;
                let color = ((planes[0] >> bit) & 1)
                    | (((planes[1] >> bit) & 1) << 1)
                    | (((planes[2] >> bit) & 1) << 2)
                    | (((planes[3] >> bit) & 1) << 3);
                if color == 0 {
                    continue;
                }
                for z in 0..if zoom { 2 } else { 1 } {
                    let x = x0 + (px * if zoom { 2 } else { 1 } + z);
                    if (0..WIDTH as i32).contains(&x) {
                        visit(x as usize, color);
                    }
                }
            }
        }
    }

    fn sprites(&self, line: &mut [u8; WIDTH], priority: &[bool; WIDTH]) {
        let mut drawn = [false; WIDTH];
        self.sprite_pixels(|x, color| {
            if !drawn[x] {
                drawn[x] = true;
                if !priority[x] {
                    line[x] = 16 + color;
                }
            }
        });
    }

    /// The leftmost pixel of the current line where two sprites overlap in opaque pixels.
    fn first_collision(&self) -> Option<usize> {
        let mut drawn = [false; WIDTH];
        let mut first: Option<usize> = None;
        self.sprite_pixels(|x, _| {
            if drawn[x] {
                first = Some(first.map_or(x, |f| f.min(x)));
            }
            drawn[x] = true;
        });
        first
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A line beginning, both stages.
    fn begin(v: &mut Vdp, line: u16) {
        v.start_of_line(line);
        v.line_counter(line);
        v.line_flags(line);
    }
    fn control(v: &mut Vdp, low: u8, high: u8) {
        v.write_control(low);
        v.write_control(high);
    }
    fn reg(v: &mut Vdp, n: u8, value: u8) {
        control(v, value, 0x80 | n);
    }
    fn set_addr(v: &mut Vdp, code: u8, addr: u16) {
        control(v, addr as u8, (code << 6) | (addr >> 8) as u8);
    }
    fn poke(v: &mut Vdp, addr: u16, data: &[u8]) {
        set_addr(v, 1, addr);
        for &b in data {
            v.write_data(b);
        }
    }
    fn cram(v: &mut Vdp, at: u8, colors: &[u8]) {
        set_addr(v, 3, u16::from(at));
        for &c in colors {
            v.write_data(c);
        }
    }
    /// A mode 4 display with the tables where the tests expect them.
    fn display() -> Vdp {
        let mut v = Vdp::new(false, false);
        reg(&mut v, 0, 0x04);
        reg(&mut v, 1, 0x40);
        reg(&mut v, 2, 0xff); // name table $3800
        reg(&mut v, 5, 0xff); // sprites $3F00
        reg(&mut v, 6, 0xfb); // sprite patterns from $0000
        reg(&mut v, 7, 0x00);
        reg(&mut v, 8, 0);
        reg(&mut v, 9, 0);
        v
    }
    fn tile_solid(color: u8) -> [u8; 32] {
        let mut t = [0; 32];
        for row in 0..8 {
            for plane in 0..4 {
                t[row * 4 + plane] = if color >> plane & 1 != 0 { 0xff } else { 0 };
            }
        }
        t
    }
    fn draw(v: &mut Vdp, y: u16) {
        v.set_line(y);
        // As the machine does at the start of every line.
        v.hscroll = v.regs[8];
        v.render_line();
    }
    fn px(v: &Vdp, x: usize, y: usize) -> u16 {
        v.frame()[y * WIDTH + x]
    }

    #[test]
    fn data_port_read_buffer_address_wrap_and_write_loads_the_buffer() {
        let mut v = Vdp::new(false, false);
        poke(&mut v, 0x3ffe, &[1, 2, 3]);
        // The third byte wrapped to $0000.
        assert_eq!(v.vram[0], 3);
        set_addr(&mut v, 0, 0x3ffe);
        // Setting up a read fetches the first byte already.
        assert_eq!((v.read_data(), v.read_data(), v.read_data()), (1, 2, 3));
        // A write loads the buffer too.
        set_addr(&mut v, 1, 0x100);
        v.write_data(0x77);
        assert_eq!(v.read_data(), 0x77);
    }

    #[test]
    fn command_word_flag_is_cleared_by_data_and_status_accesses() {
        let mut v = Vdp::new(false, false);
        v.write_control(0x34);
        v.read_status(227);
        // The half-finished word is forgotten: this starts a new one.
        control(&mut v, 0x00, 0x41);
        assert_eq!((v.addr, v.code), (0x100, 1));
        v.write_control(0x99);
        v.write_data(0);
        control(&mut v, 0x10, 0x40);
        assert_eq!(v.addr, 0x10);
    }

    #[test]
    fn register_writes_ignore_registers_above_ten() {
        let mut v = Vdp::new(false, false);
        reg(&mut v, 10, 0x12);
        reg(&mut v, 11, 0x34);
        reg(&mut v, 15, 0x56);
        assert_eq!(v.regs[10], 0x12);
        assert_eq!(v.regs[11..], [0; 5]);
    }

    #[test]
    fn color_ram_on_both_machines() {
        let mut v = Vdp::new(false, false);
        cram(&mut v, 3, &[0b00_11_10_01]);
        // Only six bits are kept: --BBGGRR.
        assert_eq!(v.cram[3], 0b11_10_01);
        assert_eq!(v.color(3), (10 << 11) | (42 << 5) | 31);
        let mut g = Vdp::new(true, false);
        // Even addresses fill the latch, odd ones write the word.
        cram(&mut g, 0x00, &[0xff]);
        assert_eq!(g.cram[0], 0);
        cram(&mut g, 0x21, &[0x0f]);
        assert_eq!(g.cram[0x10], 0x0fff);
        assert_eq!(g.cram[0], 0);
        assert_eq!(g.color(0x10), 0xffff);
    }

    #[test]
    fn background_tile_flips_palette_and_priority() {
        let mut v = display();
        let mut tile = [0u8; 32];
        // Row 0 has only its leftmost pixel set, colour 1; other rows are empty.
        tile[0] = 0x80;
        poke(&mut v, 32, &tile);
        // Name table entries 0-3 at $3800: tile 1 plain, flipped horizontally, flipped vertically, palette 2.
        poke(&mut v, 0x3800, &[1, 0x00, 1, 0x02, 1, 0x04, 1, 0x08]);
        cram(&mut v, 1, &[0x03]);
        cram(&mut v, 17, &[0x30]);
        draw(&mut v, 0);
        let red = rgb565(3, 0, 0, 3);
        let blue = rgb565(0, 0, 3, 3);
        assert_eq!((px(&v, 0, 0), px(&v, 1, 0)), (red, 0));
        assert_eq!((px(&v, 8, 0), px(&v, 15, 0)), (0, red), "horizontal flip");
        assert_eq!(
            px(&v, 16, 0),
            0,
            "vertical flip puts the pattern's first row at the bottom"
        );
        assert_eq!(px(&v, 24, 0), blue, "palette 2");
        draw(&mut v, 7);
        assert_eq!(px(&v, 16, 7), red);
        assert_eq!(px(&v, 0, 7), 0);
    }

    #[test]
    fn horizontal_scroll_fine_pixels_show_the_backdrop_and_row_lock() {
        let mut v = display();
        poke(&mut v, 32, &tile_solid(1));
        for row in 0..4 {
            poke(&mut v, 0x3800 + row * 64, &[1, 0, 1, 0]);
        }
        cram(&mut v, 1, &[0x03]);
        reg(&mut v, 8, 3);
        draw(&mut v, 0);
        let red = rgb565(3, 0, 0, 3);
        assert_eq!(
            [px(&v, 2, 0), px(&v, 3, 0), px(&v, 18, 0), px(&v, 19, 0)],
            [0, red, red, 0]
        );
        // Rows 0 and 1 stay put when bit 6 of register 0 is set.
        reg(&mut v, 0, 0x44);
        draw(&mut v, 0);
        assert_eq!(px(&v, 0, 0), red);
        draw(&mut v, 16);
        assert_eq!(px(&v, 0, 16), 0);
        // Scroll 8 starts at column 31, which is empty, then column 0.
        reg(&mut v, 0, 0x04);
        reg(&mut v, 8, 8);
        draw(&mut v, 20);
        assert_eq!((px(&v, 0, 20), px(&v, 8, 20)), (0, red));
    }

    #[test]
    fn vertical_scroll_wraps_at_224_and_waits_for_the_next_frame() {
        let mut v = display();
        poke(&mut v, 32, &tile_solid(1));
        // Row 27 of column 0 is the last row; row 0 is empty.
        poke(&mut v, 0x3800 + 27 * 64, &[1, 0]);
        cram(&mut v, 1, &[0x03]);
        reg(&mut v, 9, 220);
        // Written during the frame: not taken until line 0 again.
        draw(&mut v, 5);
        assert_eq!(px(&v, 0, 5), 0);
        v.set_line(0);
        assert_eq!(v.vscroll, 220);
        draw(&mut v, 4);
        // y 4 + 220 = 224 wraps to row 0, line 0: empty; y 3 + 220 = 223 is the last line of row 27.
        assert_eq!(px(&v, 0, 4), 0);
        draw(&mut v, 3);
        assert_eq!(px(&v, 0, 3), rgb565(3, 0, 0, 3));
        // With bit 7 of register 0 set, the right-hand eight columns do not scroll.
        poke(&mut v, 0x3800 + 24 * 2, &[1, 0]);
        reg(&mut v, 0, 0x84);
        v.set_line(0);
        draw(&mut v, 0);
        assert_eq!(px(&v, 24 * 8, 0), rgb565(3, 0, 0, 3));
    }

    #[test]
    fn first_column_mask_and_blanking_show_the_backdrop() {
        let mut v = display();
        poke(&mut v, 32, &tile_solid(1));
        poke(&mut v, 0x3800, &[1, 0, 1, 0]);
        cram(&mut v, 1, &[0x03]);
        cram(&mut v, 16, &[0x0c]);
        reg(&mut v, 7, 0);
        reg(&mut v, 0, 0x24);
        draw(&mut v, 0);
        let green = rgb565(0, 3, 0, 3);
        assert_eq!(
            (px(&v, 0, 0), px(&v, 7, 0), px(&v, 8, 0)),
            (green, green, rgb565(3, 0, 0, 3))
        );
        reg(&mut v, 1, 0x00);
        draw(&mut v, 0);
        assert_eq!(px(&v, 8, 0), green, "a blanked display is all backdrop");
    }

    fn sprite(v: &mut Vdp, index: usize, y: u8, x: u8, tile: u8) {
        poke(v, 0x3f00 + index as u16, &[y]);
        poke(v, 0x3f80 + 2 * index as u16, &[x, tile]);
    }

    #[test]
    fn sprites_position_palette_order_and_the_end_marker() {
        let mut v = display();
        poke(&mut v, 32, &tile_solid(2));
        poke(&mut v, 64, &tile_solid(3));
        cram(&mut v, 18, &[0x0c]);
        cram(&mut v, 19, &[0x30]);
        // Y 9 puts the top on line 10; sprite 0 over sprite 1 where they overlap.
        sprite(&mut v, 0, 9, 20, 1);
        sprite(&mut v, 1, 9, 24, 2);
        poke(&mut v, 0x3f02, &[0xd0]);
        draw(&mut v, 9);
        assert_eq!(px(&v, 20, 9), 0);
        draw(&mut v, 10);
        let green = rgb565(0, 3, 0, 3);
        let blue = rgb565(0, 0, 3, 3);
        assert_eq!(
            (
                px(&v, 20, 10),
                px(&v, 24, 10),
                px(&v, 28, 10),
                px(&v, 32, 10)
            ),
            (green, green, blue, 0)
        );
        assert_ne!(
            v.read_status(227) & 0x20,
            0,
            "overlapping opaque pixels collide"
        );
        draw(&mut v, 17);
        assert_eq!(px(&v, 20, 17), green);
        draw(&mut v, 18);
        assert_eq!(px(&v, 20, 18), 0, "eight lines tall");
    }

    #[test]
    fn sprite_size_zoom_shift_clipping_and_the_end_marker() {
        let mut v = display();
        poke(&mut v, 32, &tile_solid(2));
        cram(&mut v, 18, &[0x0c]);
        let green = rgb565(0, 3, 0, 3);
        // The end marker hides this sprite and everything after it.
        sprite(&mut v, 0, 0xd0, 40, 1);
        sprite(&mut v, 1, 0, 50, 1);
        draw(&mut v, 1);
        assert_eq!(px(&v, 40, 1), 0);
        assert_eq!(px(&v, 50, 1), 0);
        // Move the marker away: sprite 0 is the 8x8 one at line 1 once its Y is 0.
        poke(&mut v, 0x3f00, &[0]);
        poke(&mut v, 0x3f01, &[0xd0]);
        draw(&mut v, 1);
        assert_eq!(px(&v, 40, 1), green);
        // Zoomed sprites are 16 wide and 16 tall.
        reg(&mut v, 1, 0x41);
        draw(&mut v, 16);
        assert_eq!((px(&v, 40, 16), px(&v, 55, 16)), (green, green));
        draw(&mut v, 17);
        assert_eq!(px(&v, 40, 17), 0);
        // Tall sprites use the next tile for the lower half.
        reg(&mut v, 1, 0x42);
        poke(&mut v, 0x3f80, &[40, 0]);
        draw(&mut v, 1);
        assert_eq!(px(&v, 40, 1), 0, "tile 0 is empty");
        draw(&mut v, 10);
        assert_eq!(px(&v, 40, 10), green, "the lower half is tile 1");
        // Early clock: shifted left by eight.
        reg(&mut v, 1, 0x40);
        poke(&mut v, 0x3f81, &[1]);
        reg(&mut v, 0, 0x0c);
        draw(&mut v, 1);
        assert_eq!((px(&v, 32, 1), px(&v, 40, 1)), (green, 0));
        // A sprite at the left edge or past the right edge is clipped, not wrapped.
        poke(&mut v, 0x3f80, &[252, 1]);
        reg(&mut v, 0, 0x04);
        draw(&mut v, 1);
        assert_eq!(
            (px(&v, 252, 1), px(&v, 255, 1), px(&v, 0, 1)),
            (green, green, 0)
        );
    }

    #[test]
    fn sprite_overflow_flag_and_the_eight_per_line_limit() {
        let mut v = display();
        poke(&mut v, 32, &tile_solid(2));
        cram(&mut v, 18, &[0x0c]);
        for i in 0..9 {
            sprite(&mut v, i, 9, (i * 10) as u8, 1);
        }
        poke(&mut v, 0x3f09, &[0xd0]);
        draw(&mut v, 10);
        assert_eq!(v.status & 0x40, 0, "drawing the line does not flag it");
        begin(&mut v, 9);
        assert_eq!(v.status & 0x40, 0, "nine sprites are on line 10, not 9");
        begin(&mut v, 10);
        assert_ne!(v.status & 0x40, 0);
        let green = rgb565(0, 3, 0, 3);
        assert_eq!(px(&v, 70, 10), green);
        assert_eq!(px(&v, 80, 10), 0, "the ninth sprite is not drawn");
        assert_eq!(v.read_status(227) & 0xe0, 0x40);
        assert_eq!(v.status, 0);
    }

    /// FluBBa's test 52: two sprites overlapping from x = 131 set the flag between the cycles 94 and 95 of the
    /// line, where the H counter reads $47 for both: the flag follows the pixel, not the end of the line.
    #[test]
    fn the_collision_flag_shows_as_the_pixel_is_reached() {
        let mut v = display();
        poke(&mut v, 32, &tile_solid(2));
        cram(&mut v, 18, &[0x0c]);
        sprite(&mut v, 0, 9, 131, 1);
        sprite(&mut v, 1, 9, 131, 1);
        poke(&mut v, 0x3f02, &[0xd0]);
        v.set_line(10);
        assert_eq!(v.read_status(94) & 0x20, 0);
        assert_ne!(v.read_status(95) & 0x20, 0);
        // A collision nobody read shows by the next line.
        v.set_line(10);
        v.set_line(11);
        assert_ne!(v.read_status(0) & 0x20, 0);
        // And none on a line without one.
        v.read_status(227);
        v.set_line(40);
        assert_eq!(v.read_status(227) & 0x20, 0);
    }

    #[test]
    fn background_priority_hides_sprites_over_opaque_pixels_only() {
        let mut v = display();
        poke(&mut v, 32, &tile_solid(1));
        poke(&mut v, 64, &tile_solid(2));
        // Priority tile 1 at column 0, plain tile 0 (empty) with priority at column 1.
        poke(&mut v, 0x3800, &[1, 0x10, 0, 0x10]);
        cram(&mut v, 1, &[0x03]);
        cram(&mut v, 18, &[0x0c]);
        sprite(&mut v, 0, 0, 4, 2);
        sprite(&mut v, 1, 0, 8, 2);
        draw(&mut v, 1);
        let red = rgb565(3, 0, 0, 3);
        let green = rgb565(0, 3, 0, 3);
        assert_eq!(px(&v, 5, 1), red, "behind an opaque priority pixel");
        assert_eq!(px(&v, 10, 1), green, "in front of a transparent one");
    }

    #[test]
    fn counters_follow_the_documented_v_counter_jumps() {
        let mut v = Vdp::new(false, false);
        let at = |v: &mut Vdp, line| {
            v.set_line(line);
            v.v_counter(0)
        };
        assert_eq!(
            [at(&mut v, 0xda), at(&mut v, 0xdb), at(&mut v, 261)],
            [0xda, 0xd5, 0xff]
        );
        reg(&mut v, 0, 0x06);
        reg(&mut v, 1, 0x10);
        assert_eq!(v.height(), 224);
        assert_eq!(
            [at(&mut v, 0xea), at(&mut v, 0xeb), at(&mut v, 261)],
            [0xea, 0xe5, 0xff]
        );
        let mut p = Vdp::new(false, true);
        let at = |v: &mut Vdp, line| {
            v.set_line(line);
            v.v_counter(0)
        };
        assert_eq!(
            [at(&mut p, 0xf2), at(&mut p, 0xf3), at(&mut p, 312)],
            [0xf2, 0xba, 0xff]
        );
        assert_eq!(Vdp::h_counter(0), 0);
        assert_eq!(Vdp::h_counter(113), 0x55);
        assert!(Vdp::h_counter(226) >= 0xe9);
    }

    #[test]
    fn a_240_line_picture_is_for_50_hz_only() {
        for (pal, height) in [(false, 192), (true, 240)] {
            let mut v = Vdp::new(false, pal);
            reg(&mut v, 0, 0x06);
            reg(&mut v, 1, 0x08);
            assert_eq!(v.height(), height, "pal {pal}");
        }
    }

    #[test]
    fn v_counter_steps_to_the_next_line_where_the_line_starts() {
        let mut v = Vdp::new(false, false);
        v.set_line(100);
        assert_eq!(
            [
                v.v_counter(0),
                v.v_counter(STEP_AT - 1),
                v.v_counter(STEP_AT),
                v.v_counter(227)
            ],
            [100, 100, 101, 101]
        );
        // The last line of the frame steps to line 0, through the jump after the picture.
        v.set_line(261);
        assert_eq!([v.v_counter(0), v.v_counter(STEP_AT)], [0xff, 0]);
        // The H counter reads $F4 where the line starts, between the line interrupt's $F3 and the flag's $F5.
        assert_eq!([FRAME_IRQ_AT, STEP_AT].map(Vdp::h_counter), [0xf3, 0xf4]);
    }

    /// The values FluBBa's VDP test latches from a real Master System at each cycle of a line (his HCounterValues):
    /// the start, the jump over the blanking and the end.
    #[test]
    fn h_counter_goes_three_counts_in_four_cycles_as_latched_on_hardware() {
        let at = |from: u32, n: u32| (from..from + n).map(Vdp::h_counter).collect::<Vec<_>>();
        assert_eq!(
            at(0, 12),
            [
                0x00, 0x01, 0x02, 0x02, 0x03, 0x04, 0x05, 0x05, 0x06, 0x07, 0x08, 0x08
            ]
        );
        assert_eq!(at(192, 8), [0x90, 0x91, 0x92, 0x92, 0x93, 0xe9, 0xea, 0xea]);
        assert_eq!(at(224, 4), [0xfd, 0xfe, 0xff, 0xff]);
        // Past the line it stays put; a count is never skipped or taken twice in three.
        assert_eq!(Vdp::h_counter(500), 0xff);
        let all = at(0, 228);
        assert_eq!(all.iter().filter(|&&h| h == 0x93).count(), 1);
        assert!(
            all.windows(2)
                .all(|w| w[1] == w[0] || w[1] == w[0] + 1 || (w[0] == 0x93 && w[1] == 0xe9))
        );
    }

    #[test]
    fn frame_interrupt_sets_as_the_line_after_the_picture_begins_and_clears_on_status_read() {
        let mut v = display();
        reg(&mut v, 1, 0x60);
        for line in 0..=192 {
            begin(&mut v, line);
        }
        assert!(
            !v.irq(),
            "line 192 is the last of the blanking before the flag"
        );
        begin(&mut v, 193);
        assert!(v.irq());
        assert_eq!(v.read_status(227) & 0x80, 0x80);
        assert!(!v.irq());
        assert_eq!(v.read_status(227) & 0x80, 0);
        // The flag is held back by the enable bit and shows when it is set.
        begin(&mut v, 193);
        reg(&mut v, 1, 0x40);
        assert!(!v.irq());
        reg(&mut v, 1, 0x60);
        assert!(v.irq());
    }

    #[test]
    fn line_interrupt_counts_register_ten_and_reloads_in_the_blanking() {
        let mut v = display();
        reg(&mut v, 0, 0x14);
        reg(&mut v, 10, 3);
        // The blanking of the frame before loads the counter.
        for line in 193..262 {
            begin(&mut v, line);
        }
        let mut fired = vec![];
        for line in 0..=192 {
            begin(&mut v, line);
            if v.irq() {
                fired.push(line);
                v.read_status(227);
            }
        }
        // The interrupt comes as lines 3, 7, 11 ... begin: a handler reads those V counters (FluBBa's VDP test
        // sets register 10 to $61 and expects a handler to read $61).
        assert_eq!(fired[..3], [3, 7, 11]);
        assert_eq!(fired.len(), 193 / 4);
        // Register 10 = 0 interrupts on every line, the first being line 0.
        reg(&mut v, 10, 0);
        for line in 193..262 {
            begin(&mut v, line);
        }
        let mut count = 0;
        for line in 0..=192 {
            begin(&mut v, line);
            if v.irq() {
                count += 1;
                v.read_status(227);
            }
        }
        assert_eq!(count, 193);
        // Without the enable bit the flag stays quiet.
        reg(&mut v, 0, 0x04);
        begin(&mut v, 5);
        assert!(!v.irq());
    }

    #[test]
    fn horizontal_scroll_is_taken_as_the_line_begins() {
        let mut v = display();
        poke(&mut v, 32, &tile_solid(1));
        // Tile 1 in the first column of every row.
        for row in 0..4 {
            poke(&mut v, 0x3800 + row * 64, &[1, 0]);
        }
        cram(&mut v, 1, &[0x03]);
        let red = rgb565(3, 0, 0, 3);
        reg(&mut v, 8, 8);
        begin(&mut v, 1);
        // Written after the line began: it waits for the next line.
        reg(&mut v, 8, 0);
        v.set_line(1);
        v.render_line();
        assert_eq!(
            (px(&v, 0, 1), px(&v, 8, 1)),
            (0, red),
            "the first column went 8 pixels right"
        );
        begin(&mut v, 2);
        v.set_line(2);
        v.render_line();
        assert_eq!((px(&v, 0, 2), px(&v, 8, 2)), (red, 0));
    }

    #[test]
    fn game_gear_window_is_the_middle_of_the_picture() {
        let mut v = Vdp::new(true, false);
        reg(&mut v, 0, 0x04);
        reg(&mut v, 1, 0x40);
        reg(&mut v, 2, 0xff);
        reg(&mut v, 7, 1);
        cram(&mut v, 0x00, &[0xff, 0x0f]);
        draw(&mut v, 23);
        draw(&mut v, 24);
        assert_eq!(v.dimensions(), (160, 144));
        assert_eq!(v.frame()[0], 0xffff);
        assert_eq!(v.frame().len(), 160 * 144);
        // Always 192 lines, whatever mode bits say.
        reg(&mut v, 0, 0x06);
        reg(&mut v, 1, 0x50);
        assert_eq!(v.height(), 192);
    }
}
