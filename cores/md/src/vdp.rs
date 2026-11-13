//! Mode 5 VDP. Ports and DMA are word-addressed by the 68000; display is
//! composed one scanline at a time, so raster changes take effect next line.
//! Register layout follows Sega's Genesis Technical Overview, VDP chapters.
use savestate::state;

pub const WIDTH: usize = 320;
pub const HEIGHT: usize = 240;

#[derive(Clone)]
pub(crate) struct Vdp {
    pub regs: [u8; 24],
    vram: Vec<u8>,
    cram: [u16; 64],
    vsram: [u16; 40],
    address: u16,
    code: u8,
    first_word: Option<u16>,
    dma_source: u32,
    dma_left: u32,
    fill_pending: bool,
    line: u16,
    line_cycles: u16,
    pal: bool,
    h_counter: u8,
    h_pending: bool,
    v_pending: bool,
    collision: bool,
    overflow: bool,
    odd: bool,
    picture: Vec<u16>,
    /// RGB565 of every CRAM entry at the three brightnesses (shadow, normal, highlight), rebuilt by
    /// `render_line` when CRAM has changed since. Derived, so not part of the saved state.
    lut: [u16; 192],
    lut_cram: [u16; 64],
}

state!(Vdp {
    regs,
    vram,
    cram,
    vsram,
    address,
    code,
    first_word,
    dma_source,
    dma_left,
    fill_pending,
    line,
    line_cycles,
    pal,
    h_counter,
    h_pending,
    v_pending,
    collision,
    overflow,
    odd
});

impl Vdp {
    pub fn new() -> Self {
        Self {
            regs: [0; 24],
            vram: vec![0; 65536],
            cram: [0; 64],
            vsram: [0; 40],
            address: 0,
            code: 0,
            first_word: None,
            dma_source: 0,
            dma_left: 0,
            fill_pending: false,
            line: 0,
            line_cycles: 0,
            pal: false,
            h_counter: 0,
            h_pending: false,
            v_pending: false,
            collision: false,
            overflow: false,
            odd: false,
            picture: vec![0; WIDTH * HEIGHT],
            lut: [0; 192],
            // No CRAM value has these bits, so the first line builds the table.
            lut_cram: [0xffff; 64],
        }
    }

    pub fn dimensions(&self) -> (usize, usize) {
        let width = if self.regs[12] & 1 != 0 { 320 } else { 256 };
        let height = if self.pal && self.regs[1] & 8 != 0 {
            240
        } else {
            224
        };
        (width, height)
    }

    pub fn frame(&self) -> &[u16] {
        let (width, height) = self.dimensions();
        &self.picture[..width * height]
    }

    #[inline]
    fn interlace2(&self) -> bool {
        self.regs[12] & 6 == 6
    }

    pub fn validate(&self) -> bool {
        self.code < 64
            && self.dma_left <= 65536
            && self.dma_source <= 0x00ff_fffe
            && self.dma_source & 1 == 0
            && self.line < 313
            && self.line_cycles <= 488
            && self.first_word.is_none_or(|word| word & 0xc000 != 0x8000)
    }

    /// Begins a scanline. The caller supplies the television region; the
    /// cartridge cannot change the crystal frequency using VDP registers.
    pub fn start_line(&mut self, line: u16, pal: bool) {
        self.line = line;
        self.line_cycles = 0;
        self.pal = pal;
        let height = self.dimensions().1 as u16;
        if line == 0 {
            self.odd = !self.odd;
            self.h_counter = self.regs[10];
            self.collision = false;
            self.overflow = false;
        }
        if line <= height {
            if self.h_counter == 0 {
                self.h_counter = self.regs[10];
                self.h_pending = true;
            } else {
                self.h_counter = self.h_counter.wrapping_sub(1);
            }
        } else {
            self.h_counter = self.regs[10];
        }
        if line == height {
            self.v_pending = true;
        }
    }

    /// Approximate horizontal beam position within this scanline, in 68000
    /// cycles. FIFO stalls and exact dot timing are not modelled yet.
    pub fn set_line_cycles(&mut self, cycles: u16) {
        self.line_cycles = cycles.min(488);
    }

    pub fn irq_level(&self) -> u8 {
        if self.v_pending && self.regs[1] & 0x20 != 0 {
            6
        } else if self.h_pending && self.regs[0] & 0x10 != 0 {
            4
        } else {
            0
        }
    }

    pub fn ack_irq(&mut self, level: u8) {
        match level {
            6 => self.v_pending = false,
            4 => self.h_pending = false,
            _ => {}
        }
    }

    pub fn read_status(&mut self) -> u16 {
        let status = 0x3400
            | 0x0200
            | (u16::from(self.v_pending) << 7)
            | (u16::from(self.overflow) << 6)
            | (u16::from(self.collision) << 5)
            | (u16::from(self.odd && self.regs[12] & 2 != 0) << 4)
            | (u16::from(self.line >= self.dimensions().1 as u16 || self.regs[1] & 0x40 == 0) << 3)
            | (u16::from(self.line_cycles >= 400) << 2)
            | (u16::from(self.dma_left != 0 || self.fill_pending) << 1)
            | u16::from(self.pal);
        self.first_word = None;
        self.collision = false;
        self.overflow = false;
        // Reading status does not acknowledge the CPU's interrupt request.
        status
    }

    pub fn hv_counter(&self) -> u16 {
        let mut v = self.line;
        if !self.pal && v > 0xea {
            v += 250;
        } else if self.pal
            && v > if self.dimensions().1 == 224 {
                0x102
            } else {
                0x10a
            }
        {
            v += 199;
        }
        if self.regs[12] & 2 != 0 {
            if self.interlace2() {
                v <<= 1;
            }
            // The external counter substitutes bit 8 for bit 0 in both
            // interlaced modes; the field parity is read in status bit 4.
            v = (v & !1) | ((v >> 8) & 1);
        }
        let h40 = self.dimensions().0 == 320;
        let ticks = if h40 { 210 } else { 171 };
        let mut h = u32::from(self.line_cycles) * ticks / 488;
        if h40 && h > 0xb6 {
            h += 0xe4 - 0xb7;
        } else if !h40 && h > 0x93 {
            h += 0xe9 - 0x94;
        }
        (v << 8) | h as u16 & 0xff
    }

    pub fn write_control(&mut self, value: u16) {
        if let Some(first) = self.first_word.take() {
            self.address = first & 0x3fff | (value & 3) << 14;
            self.code = ((first >> 14) as u8 & 3) | ((value >> 2) as u8 & 0x3c);
            if self.code & 0x20 != 0 && self.regs[1] & 0x10 != 0 {
                self.begin_dma();
            }
        } else if value & 0xc000 == 0x8000 {
            let reg = ((value >> 8) & 0x1f) as usize;
            if reg < self.regs.len() {
                self.regs[reg] = value as u8;
            }
            self.code = 0;
        } else {
            // The first half already changes the low address and code bits.
            self.address = self.address & 0xc000 | value & 0x3fff;
            self.code = self.code & 0x3c | (value >> 14) as u8;
            self.first_word = Some(value);
        }
    }

    fn increment(&mut self) {
        self.address = self.address.wrapping_add(u16::from(self.regs[15]));
    }

    pub fn read_data(&mut self) -> u16 {
        self.first_word = None;
        let address = usize::from(self.address);
        let value = match self.code & 0xf {
            0 => self.word(address & !1),
            8 => self.cram[(address & 0x7e) >> 1],
            4 if address & 0x7f < 80 => self.vsram[(address & 0x7e) >> 1],
            _ => 0,
        };
        self.increment();
        value
    }

    pub fn write_data(&mut self, value: u16) {
        self.first_word = None;
        if self.fill_pending {
            self.fill_pending = false;
            // The triggering word is written first (low on the selected
            // lane, high on the other). Fill then advances before each
            // repeated high byte; the initial word is additional to length.
            let address = usize::from(self.address);
            self.vram[address] = value as u8;
            self.vram[address ^ 1] = (value >> 8) as u8;
            while self.dma_left != 0 {
                self.increment();
                self.vram[usize::from(self.address ^ 1)] = (value >> 8) as u8;
                self.dma_left -= 1;
            }
            self.finish_dma();
        } else {
            self.write_memory(value);
        }
    }

    fn write_memory(&mut self, value: u16) {
        let address = usize::from(self.address);
        match self.code & 0xf {
            1 => {
                self.vram[address] = (value >> 8) as u8;
                self.vram[address ^ 1] = value as u8;
            }
            3 => self.cram[(address & 0x7e) >> 1] = value & 0x0eee,
            5 if address & 0x7f < 80 => self.vsram[(address & 0x7e) >> 1] = value & 0x07ff,
            _ => {}
        }
        self.increment();
    }

    fn begin_dma(&mut self) {
        self.dma_left = u32::from(self.regs[19]) | u32::from(self.regs[20]) << 8;
        if self.dma_left == 0 {
            self.dma_left = 65536;
        }
        match self.regs[23] >> 6 {
            0 | 1 => {
                self.dma_source = (u32::from(self.regs[21])
                    | u32::from(self.regs[22]) << 8
                    | u32::from(self.regs[23] & 0x7f) << 16)
                    << 1;
            }
            2 => self.fill_pending = true,
            3 => {
                let mut source = u16::from(self.regs[21]) | u16::from(self.regs[22]) << 8;
                while self.dma_left != 0 {
                    self.vram[usize::from(self.address)] = self.vram[usize::from(source)];
                    source = source.wrapping_add(1);
                    self.increment();
                    self.dma_left -= 1;
                }
                self.regs[21] = source as u8;
                self.regs[22] = (source >> 8) as u8;
                self.finish_dma();
            }
            _ => unreachable!(),
        }
    }

    /// A memory-to-VDP transfer waiting for the machine's 68000 bus. The
    /// source advances inside its original 128 KB bank, as on the VDP.
    pub fn dma_request(&self) -> Option<(u32, u32)> {
        if self.dma_left != 0 && self.regs[23] & 0x80 == 0 {
            Some((self.dma_source, self.dma_left.min(65536)))
        } else {
            None
        }
    }

    /// Estimated 68000 bus hold in master clocks. VRAM consumes two byte
    /// slots per word; CRAM/VSRAM transfer a word per slot. The rate is held
    /// at its starting scanline's value, so a DMA crossing blanking remains
    /// an approximation until transfers themselves are scheduled per line.
    pub fn dma_clocks(&self, words: u32) -> u32 {
        let h40 = self.dimensions().0 == 320;
        let blank = self.line >= self.dimensions().1 as u16 || self.regs[1] & 0x40 == 0;
        let slots_per_line = match (h40, blank) {
            (false, false) => 16,
            (true, false) => 18,
            (false, true) => 167,
            (true, true) => 205,
        };
        let slots = words.min(65536) * if self.code & 0xf == 1 { 2 } else { 1 };
        (slots * 3420).div_ceil(slots_per_line)
    }

    pub fn dma_word(&mut self, value: u16) {
        if self.dma_request().is_none() {
            return;
        }
        self.write_memory(value);
        self.dma_source = self.dma_source & 0xfe0000 | self.dma_source.wrapping_add(2) & 0x1ffff;
        let source = self.dma_source >> 1;
        self.regs[21] = source as u8;
        self.regs[22] = (source >> 8) as u8;
        self.dma_left -= 1;
        self.regs[19] = self.dma_left as u8;
        self.regs[20] = (self.dma_left >> 8) as u8;
        if self.dma_left == 0 {
            self.finish_dma();
        }
    }

    fn finish_dma(&mut self) {
        self.regs[19] = 0;
        self.regs[20] = 0;
        self.code &= !0x20;
    }

    #[inline]
    fn word(&self, address: usize) -> u16 {
        u16::from(self.vram[address & 0xffff]) << 8 | u16::from(self.vram[(address + 1) & 0xffff])
    }

    #[inline]
    fn tile_pixel(&self, attr: u16, x: usize, y: usize) -> u8 {
        let tall = self.interlace2();
        let row_mask = if tall { 15 } else { 7 };
        let x = if attr & 0x0800 != 0 {
            7 - (x & 7)
        } else {
            x & 7
        };
        let y = if attr & 0x1000 != 0 {
            row_mask - (y & row_mask)
        } else {
            y & row_mask
        };
        let shift = if tall { 6 } else { 5 };
        let byte = self.vram[((usize::from(attr & 0x7ff) << shift) + y * 4 + x / 2) & 0xffff];
        let index = if x & 1 == 0 { byte >> 4 } else { byte & 15 };
        if index == 0 {
            0
        } else {
            index | ((attr >> 9) as u8 & 0x30)
        }
    }

    #[cfg(test)]
    fn plane_pixel(&self, plane: usize, x: usize, y: usize) -> (u8, bool) {
        let tall = self.interlace2();
        let cell_height = if tall { 16 } else { 8 };
        let h40 = self.dimensions().0 == 320;
        let window_x = usize::from(self.regs[17] & 31) * 16;
        let window_y = usize::from(self.regs[18] & 31) * cell_height;
        let in_x = if self.regs[17] & 0x80 != 0 {
            x >= window_x
        } else {
            x < window_x
        };
        let in_y = if self.regs[18] & 0x80 != 0 {
            y >= window_y
        } else {
            y < window_y
        };
        if plane == 0 && (in_x || in_y) {
            let base = usize::from(self.regs[3] & if h40 { 0x3c } else { 0x3e }) << 10;
            let columns = if h40 { 64 } else { 32 };
            let attr = self.word(base + ((y / cell_height) * columns + x / 8) * 2);
            return (self.tile_pixel(attr, x, y), attr & 0x8000 != 0);
        }
        let cells = [32, 64, 32, 128];
        let columns = cells[usize::from(self.regs[16] & 3)];
        let rows = cells[usize::from((self.regs[16] >> 4) & 3)];
        let field_y = if tall { y / 2 } else { y };
        let hscroll_line = match self.regs[11] & 3 {
            0 => 0,
            1 => field_y & 7,
            2 => field_y & !7,
            _ => field_y,
        };
        let scroll_base = usize::from(self.regs[13] & 0x3f) << 10;
        let hscroll = usize::from(self.word(scroll_base + hscroll_line * 4 + plane * 2) & 0x3ff);
        let vs_index = if self.regs[11] & 4 != 0 {
            (x / 16) * 2 + plane
        } else {
            plane
        };
        let vscroll = usize::from(
            self.vsram.get(vs_index).copied().unwrap_or(0) & if tall { 0x7ff } else { 0x3ff },
        );
        let sx = x.wrapping_sub(hscroll) & (columns * 8 - 1);
        let sy = (y + vscroll) & (rows * cell_height - 1);
        let base = if plane == 0 {
            usize::from(self.regs[2] & 0x38) << 10
        } else {
            usize::from(self.regs[4] & 7) << 13
        };
        let attr = self.word(base + ((sy / cell_height) * columns + sx / 8) * 2);
        (self.tile_pixel(attr, sx, sy), attr & 0x8000 != 0)
    }

    /// Writes the pixels of one pattern row, starting at pixel `first` of the tile: the planes' colour
    /// (palette bits and index, 0 for transparent) and the tile's priority bit.
    #[inline]
    fn tile_run(&self, attr: u16, first: usize, y: usize, tall: bool, colors: &mut [u8], high: &mut [bool]) {
        let row_mask = if tall { 15 } else { 7 };
        let y = if attr & 0x1000 != 0 {
            row_mask - (y & row_mask)
        } else {
            y & row_mask
        };
        let base = (usize::from(attr & 0x7ff) << if tall { 6 } else { 5 }) + y * 4;
        let row = [
            self.vram[base & 0xffff],
            self.vram[(base + 1) & 0xffff],
            self.vram[(base + 2) & 0xffff],
            self.vram[(base + 3) & 0xffff],
        ];
        let palette = (attr >> 9) as u8 & 0x30;
        let flip = attr & 0x0800 != 0;
        let priority = attr & 0x8000 != 0;
        for (i, (color, high)) in colors.iter_mut().zip(high.iter_mut()).enumerate() {
            let x = if flip { 7 - (first + i) } else { first + i };
            let byte = row[x >> 1];
            let index = if x & 1 == 0 { byte >> 4 } else { byte & 15 };
            *color = if index == 0 { 0 } else { index | palette };
            *high = priority;
        }
    }

    /// One plane's colours and priority bits across the line, in segments that share a name table cell, a
    /// pattern row and a vertical scroll value (at most 8 pixels, so at most one cell each).
    fn plane_line(&self, plane: usize, y: usize, width: usize, colors: &mut [u8; WIDTH], high: &mut [bool; WIDTH]) {
        let tall = self.interlace2();
        let cell_height = if tall { 16 } else { 8 };
        let h40 = width == 320;
        let window_x = usize::from(self.regs[17] & 31) * 16;
        let window_y = usize::from(self.regs[18] & 31) * cell_height;
        let window_right = self.regs[17] & 0x80 != 0;
        let in_x = |x: usize| if window_right { x >= window_x } else { x < window_x };
        let in_y = if self.regs[18] & 0x80 != 0 {
            y >= window_y
        } else {
            y < window_y
        };
        let window_base = usize::from(self.regs[3] & if h40 { 0x3c } else { 0x3e }) << 10;
        let window_columns = if h40 { 64 } else { 32 };
        let cells = [32, 64, 32, 128];
        let columns = cells[usize::from(self.regs[16] & 3)];
        let rows = cells[usize::from((self.regs[16] >> 4) & 3)];
        let field_y = if tall { y / 2 } else { y };
        let hscroll_line = match self.regs[11] & 3 {
            0 => 0,
            1 => field_y & 7,
            2 => field_y & !7,
            _ => field_y,
        };
        let scroll_base = usize::from(self.regs[13] & 0x3f) << 10;
        let hscroll = usize::from(self.word(scroll_base + hscroll_line * 4 + plane * 2) & 0x3ff);
        let column_vscroll = self.regs[11] & 4 != 0;
        let vscroll_mask = if tall { 0x7ff } else { 0x3ff };
        let name_base = if plane == 0 {
            usize::from(self.regs[2] & 0x38) << 10
        } else {
            usize::from(self.regs[4] & 7) << 13
        };
        let mut x = 0;
        while x < width {
            if plane == 0 && (in_y || in_x(x)) {
                // The window shows its own, unscrolled name table.
                let end = ((x | 7) + 1).min(width);
                let attr = self.word(window_base + ((y / cell_height) * window_columns + x / 8) * 2);
                self.tile_run(attr, x & 7, y, tall, &mut colors[x..end], &mut high[x..end]);
                x = end;
                continue;
            }
            let vs_index = if column_vscroll { (x / 16) * 2 + plane } else { plane };
            let vscroll = usize::from(self.vsram.get(vs_index).copied().unwrap_or(0) & vscroll_mask);
            let sx = x.wrapping_sub(hscroll) & (columns * 8 - 1);
            let sy = (y + vscroll) & (rows * cell_height - 1);
            let attr = self.word(name_base + ((sy / cell_height) * columns + sx / 8) * 2);
            let mut end = (x + 8 - (sx & 7)).min(width);
            if column_vscroll {
                end = end.min((x | 15) + 1);
            }
            if plane == 0 && window_right && x < window_x {
                end = end.min(window_x);
            }
            self.tile_run(attr, sx & 7, sy, tall, &mut colors[x..end], &mut high[x..end]);
            x = end;
        }
    }

    /// Render the current visible line, including linked sprites. Sprite
    /// priority is resolved within the sprite layer before plane priority.
    pub fn render_line(&mut self) {
        let (width, height) = self.dimensions();
        let y = usize::from(self.line);
        if y >= height {
            return;
        }
        // Keep a 224/240-line bob-deinterlaced output. In mode 2 the
        // renderer nevertheless addresses the full 448/480-line scene,
        // including odd scroll and sprite positions, for this field.
        let tall = self.interlace2();
        let raster_y = if tall {
            y * 2 + usize::from(self.odd)
        } else {
            y
        };
        let cell_height = if tall { 16 } else { 8 };
        let background = self.regs[7] & 0x3f;
        let mut colors = [background; WIDTH];
        let mut ranks = [0u8; WIDTH];
        let mut brightness = [1u8; WIDTH];
        let shadow = self.regs[12] & 8 != 0;
        if self.regs[1] & 0x44 == 0x44 {
            let mut plane_colors = [[0u8; WIDTH]; 2];
            let mut plane_high = [[false; WIDTH]; 2];
            for plane in 0..2 {
                self.plane_line(plane, raster_y, width, &mut plane_colors[plane], &mut plane_high[plane]);
            }
            for x in 0..width {
                let mut any_high = false;
                for plane in [1, 0] {
                    let (color, high) = (plane_colors[plane][x], plane_high[plane][x]);
                    // Priority contributes to S/H even when this plane's
                    // texel is transparent and the other plane is visible.
                    any_high |= high;
                    let rank = if high { 4 } else { 1 } + u8::from(plane == 0);
                    if color & 15 != 0 && rank > ranks[x] {
                        colors[x] = color;
                        ranks[x] = rank;
                    }
                }
                brightness[x] = u8::from(!shadow || any_high);
            }
            let h40 = width == 320;
            let base = usize::from(self.regs[5] & if h40 { 0x7e } else { 0x7f }) << 9;
            let limit = if h40 { 80 } else { 64 };
            let line_limit = if h40 { 20 } else { 16 };
            let mut sprite_colors = [0u8; WIDTH];
            let mut sprite_high = [false; WIDTH];
            let mut link = 0usize;
            let mut on_line = 0;
            let mut fetched_pixels = 0;
            let mut had_nonzero_x = false;
            for _ in 0..limit {
                if link >= limit {
                    break;
                }
                let entry = base + link * 8;
                let sy = i32::from(self.word(entry) & if tall { 0x3ff } else { 0x1ff })
                    - if tall { 256 } else { 128 };
                let size_link = self.word(entry + 2);
                let attr = self.word(entry + 4);
                let raw_x = self.word(entry + 6) & 0x1ff;
                let sx = i32::from(raw_x) - 128;
                let sh = usize::from((size_link >> 8) & 3) + 1;
                let sw = usize::from((size_link >> 10) & 3) + 1;
                let row = raster_y as i32 - sy;
                if row >= 0 && row < (sh * cell_height) as i32 {
                    if on_line >= line_limit || fetched_pixels >= width {
                        self.overflow = true;
                        break;
                    }
                    on_line += 1;
                    if raw_x == 0 && had_nonzero_x {
                        break;
                    }
                    had_nonzero_x |= raw_x != 0;
                    let py = if attr & 0x1000 != 0 {
                        sh * cell_height - 1 - row as usize
                    } else {
                        row as usize
                    };
                    for dx in 0..sw * 8 {
                        if fetched_pixels >= width {
                            break;
                        }
                        fetched_pixels += 1;
                        let x = sx + dx as i32;
                        if x < 0 || x >= width as i32 {
                            continue;
                        }
                        let px = if attr & 0x0800 != 0 {
                            sw * 8 - 1 - dx
                        } else {
                            dx
                        };
                        let tile = usize::from(attr & 0x7ff) + (px / 8) * sh + py / cell_height;
                        let tile_attr = (attr & 0xe000) | (tile as u16 & 0x7ff);
                        let color = self.tile_pixel(tile_attr, px, py);
                        if color & 15 != 0 {
                            let x = x as usize;
                            if sprite_colors[x] != 0 {
                                self.collision = true;
                            } else {
                                sprite_colors[x] = color;
                                sprite_high[x] = attr & 0x8000 != 0;
                            }
                        }
                    }
                }
                link = usize::from(size_link & 0x7f);
                if link == 0 {
                    break;
                }
            }
            for x in 0..width {
                let color = sprite_colors[x];
                let rank = if sprite_high[x] { 6 } else { 3 };
                if color != 0 && rank > ranks[x] {
                    if shadow && color == 0x3e {
                        brightness[x] = (brightness[x] + 1).min(2);
                    } else if shadow && color == 0x3f {
                        brightness[x] = 0;
                    } else {
                        colors[x] = color;
                        // Color 14 in any sprite palette is always normal.
                        brightness[x] =
                            u8::from(brightness[x] != 0 || sprite_high[x] || color & 15 == 14);
                    }
                }
            }
        }
        if self.regs[0] & 0x20 != 0 {
            for x in 0..8.min(width) {
                colors[x] = background;
                brightness[x] = 1;
            }
        }
        if self.lut_cram != self.cram {
            for brightness in 0..3u8 {
                for (i, &color) in self.cram.iter().enumerate() {
                    self.lut[usize::from(brightness) * 64 + i] = rgb565(color, brightness);
                }
            }
            self.lut_cram = self.cram;
        }
        let line = &mut self.picture[y * width..(y + 1) * width];
        for x in 0..width {
            line[x] = self.lut[usize::from(brightness[x]) * 64 + usize::from(colors[x] & 63)];
        }
    }

    /// The plain per-pixel renderer, the specification `render_line` is checked against by
    /// `render_line_matches_the_per_pixel_reference`.
    #[cfg(test)]
    fn render_line_reference(&mut self) {
        let (width, height) = self.dimensions();
        let y = usize::from(self.line);
        if y >= height {
            return;
        }
        // Keep a 224/240-line bob-deinterlaced output. In mode 2 the
        // renderer nevertheless addresses the full 448/480-line scene,
        // including odd scroll and sprite positions, for this field.
        let tall = self.interlace2();
        let raster_y = if tall {
            y * 2 + usize::from(self.odd)
        } else {
            y
        };
        let cell_height = if tall { 16 } else { 8 };
        let background = self.regs[7] & 0x3f;
        let mut colors = [background; WIDTH];
        let mut ranks = [0u8; WIDTH];
        let mut brightness = [1u8; WIDTH];
        let shadow = self.regs[12] & 8 != 0;
        if self.regs[1] & 0x44 == 0x44 {
            for x in 0..width {
                let mut plane_high = false;
                for plane in [1, 0] {
                    let (color, high) = self.plane_pixel(plane, x, raster_y);
                    // Priority contributes to S/H even when this plane's
                    // texel is transparent and the other plane is visible.
                    plane_high |= high;
                    let rank = if high { 4 } else { 1 } + u8::from(plane == 0);
                    if color & 15 != 0 && rank > ranks[x] {
                        colors[x] = color;
                        ranks[x] = rank;
                    }
                }
                brightness[x] = u8::from(!shadow || plane_high);
            }
            let h40 = width == 320;
            let base = usize::from(self.regs[5] & if h40 { 0x7e } else { 0x7f }) << 9;
            let limit = if h40 { 80 } else { 64 };
            let line_limit = if h40 { 20 } else { 16 };
            let mut sprite_colors = [0u8; WIDTH];
            let mut sprite_high = [false; WIDTH];
            let mut link = 0usize;
            let mut on_line = 0;
            let mut fetched_pixels = 0;
            let mut had_nonzero_x = false;
            for _ in 0..limit {
                if link >= limit {
                    break;
                }
                let entry = base + link * 8;
                let sy = i32::from(self.word(entry) & if tall { 0x3ff } else { 0x1ff })
                    - if tall { 256 } else { 128 };
                let size_link = self.word(entry + 2);
                let attr = self.word(entry + 4);
                let raw_x = self.word(entry + 6) & 0x1ff;
                let sx = i32::from(raw_x) - 128;
                let sh = usize::from((size_link >> 8) & 3) + 1;
                let sw = usize::from((size_link >> 10) & 3) + 1;
                let row = raster_y as i32 - sy;
                if row >= 0 && row < (sh * cell_height) as i32 {
                    if on_line >= line_limit || fetched_pixels >= width {
                        self.overflow = true;
                        break;
                    }
                    on_line += 1;
                    if raw_x == 0 && had_nonzero_x {
                        break;
                    }
                    had_nonzero_x |= raw_x != 0;
                    let py = if attr & 0x1000 != 0 {
                        sh * cell_height - 1 - row as usize
                    } else {
                        row as usize
                    };
                    for dx in 0..sw * 8 {
                        if fetched_pixels >= width {
                            break;
                        }
                        fetched_pixels += 1;
                        let x = sx + dx as i32;
                        if x < 0 || x >= width as i32 {
                            continue;
                        }
                        let px = if attr & 0x0800 != 0 {
                            sw * 8 - 1 - dx
                        } else {
                            dx
                        };
                        let tile = usize::from(attr & 0x7ff) + (px / 8) * sh + py / cell_height;
                        let tile_attr = (attr & 0xe000) | (tile as u16 & 0x7ff);
                        let color = self.tile_pixel(tile_attr, px, py);
                        if color & 15 != 0 {
                            let x = x as usize;
                            if sprite_colors[x] != 0 {
                                self.collision = true;
                            } else {
                                sprite_colors[x] = color;
                                sprite_high[x] = attr & 0x8000 != 0;
                            }
                        }
                    }
                }
                link = usize::from(size_link & 0x7f);
                if link == 0 {
                    break;
                }
            }
            for x in 0..width {
                let color = sprite_colors[x];
                let rank = if sprite_high[x] { 6 } else { 3 };
                if color != 0 && rank > ranks[x] {
                    if shadow && color == 0x3e {
                        brightness[x] = (brightness[x] + 1).min(2);
                    } else if shadow && color == 0x3f {
                        brightness[x] = 0;
                    } else {
                        colors[x] = color;
                        // Color 14 in any sprite palette is always normal.
                        brightness[x] =
                            u8::from(brightness[x] != 0 || sprite_high[x] || color & 15 == 14);
                    }
                }
            }
        }
        for x in 0..width {
            if self.regs[0] & 0x20 != 0 && x < 8 {
                colors[x] = background;
                brightness[x] = 1;
            }
            self.picture[y * width + x] =
                rgb565(self.cram[usize::from(colors[x] & 63)], brightness[x]);
        }
    }
}

fn rgb565(color: u16, brightness: u8) -> u16 {
    let component = |shift: u32, bits: u32| {
        let v = (u32::from(color) >> shift) & 7;
        let v = match brightness {
            0 => v,
            2 => v + 7,
            _ => v * 2,
        };
        (v * ((1 << bits) - 1) / 14) as u16
    };
    component(1, 5) << 11 | component(5, 6) << 5 | component(9, 5)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(v: &mut Vdp, address: u16, code: u8) {
        v.write_control(address & 0x3fff | u16::from(code & 3) << 14);
        v.write_control((address >> 14) | u16::from(code & 0x3c) << 2);
    }

    #[test]
    fn ports_latch_increment_wrap_and_color_mask() {
        let mut v = Vdp::new();
        v.write_control(0x8f02);
        command(&mut v, 0xfffe, 1);
        v.write_data(0x1234);
        v.write_data(0x5678);
        command(&mut v, 0xfffe, 0);
        assert_eq!(v.read_data(), 0x1234);
        assert_eq!(v.read_data(), 0x5678);
        command(&mut v, 0x7e, 3);
        v.write_data(0xffff);
        v.write_data(0x0246);
        command(&mut v, 0x7e, 8);
        assert_eq!(v.read_data(), 0x0eee);
        assert_eq!(v.read_data(), 0x0246);
        v.write_control(0x4000);
        v.read_status(); // cancels half a command, permitting register write
        v.write_control(0x8f04);
        assert_eq!(v.regs[15], 4);
    }

    #[test]
    fn dma_from_cpu_wraps_source_within_bank() {
        let mut v = Vdp::new();
        v.regs[1] = 0x10;
        v.regs[15] = 2;
        v.regs[19] = 2;
        v.regs[21] = 0xff;
        v.regs[22] = 0xff;
        v.regs[23] = 1;
        command(&mut v, 0x100, 0x21);
        assert_eq!(v.dma_request(), Some((0x3fffe, 2)));
        v.dma_word(0xabcd);
        assert_eq!(v.dma_request(), Some((0x20000, 1)));
        v.dma_word(0x2468);
        assert!(v.dma_request().is_none());
        command(&mut v, 0x100, 0);
        assert_eq!(v.read_data(), 0xabcd);
        assert_eq!(v.read_data(), 0x2468);
    }

    #[test]
    fn cpu_dma_stalls_depend_on_display_and_destination() {
        let mut v = Vdp::new();
        v.regs[1] = 0x50;
        v.regs[19] = 8;
        v.start_line(0, false);
        command(&mut v, 0, 0x21);
        assert_eq!(v.dma_clocks(8), 3420); // 16 VRAM bytes in H32
        assert_eq!(v.dma_clocks(u32::MAX), 28_016_640);
        v.start_line(224, false);
        assert_eq!(v.dma_clocks(8), (16u32 * 3420).div_ceil(167));
        v.regs[12] = 1;
        assert_eq!(v.dma_clocks(8), (16u32 * 3420).div_ceil(205));
        v.start_line(0, false);
        assert_eq!(v.dma_clocks(8), (16u32 * 3420).div_ceil(18));
        command(&mut v, 0, 0x23); // color RAM: one slot per word
        assert_eq!(v.dma_clocks(8), (8u32 * 3420).div_ceil(18));
        v.regs[1] &= !0x40;
        assert_eq!(v.dma_clocks(8), (8u32 * 3420).div_ceil(205));
    }

    #[test]
    fn fill_and_copy_are_byte_transfers() {
        let mut v = Vdp::new();
        v.regs[1] = 0x10;
        v.regs[15] = 1;
        v.regs[19] = 3;
        v.regs[23] = 0x80;
        command(&mut v, 0x100, 0x21);
        assert!(v.fill_pending);
        v.write_data(0xaaaa);
        assert!(!v.fill_pending);
        assert_eq!(&v.vram[0x100..0x104], &[0xaa; 4]);
        // Copy uses individual bytes; an overlapping destination sees writes.
        v.vram[0x200] = 0x42;
        v.regs[19] = 3;
        v.regs[21] = 0;
        v.regs[22] = 2;
        v.regs[23] = 0xc0;
        command(&mut v, 0x201, 0x21);
        assert_eq!(&v.vram[0x200..0x204], &[0x42; 4]);
    }

    #[test]
    fn pending_interrupts_survive_status_read() {
        let mut v = Vdp::new();
        v.regs[0] = 0x10;
        v.regs[1] = 0x60;
        v.start_line(0, false);
        assert_eq!(v.irq_level(), 4);
        v.read_status();
        assert_eq!(v.irq_level(), 4);
        v.ack_irq(4);
        assert_eq!(v.irq_level(), 0);
        v.start_line(224, false);
        assert_eq!(v.irq_level(), 6);
        assert_ne!(v.read_status() & 0x88, 0);
        v.ack_irq(6);
        assert_eq!(v.irq_level(), 4);
    }

    fn visible() -> Vdp {
        let mut v = Vdp::new();
        v.regs[1] = 0x44;
        v.regs[2] = 0x30; // A at C000
        v.regs[4] = 7; // B at E000
        v.regs[5] = 0x50; // sprites at A000
        v.regs[13] = 0x3c; // hscroll at F000
        v.cram[1] = 0x000e;
        v.cram[2] = 0x00e0;
        v.cram[3] = 0x0e00;
        for p in &mut v.vram[32..64] {
            *p = 0x11;
        }
        for p in &mut v.vram[64..96] {
            *p = 0x22;
        }
        for p in &mut v.vram[96..128] {
            *p = 0x33;
        }
        v
    }

    #[test]
    fn plane_and_sprite_priorities_and_collision() {
        let mut v = visible();
        v.vram[0xc001] = 1;
        v.vram[0xe001] = 2;
        // A low priority is above B low.
        v.start_line(0, false);
        v.render_line();
        assert_eq!(v.frame()[0], 0xf800);
        // B high priority is above A low.
        v.vram[0xe000] = 0x80;
        v.render_line();
        assert_eq!(v.frame()[0], 0x07e0);
        // Two linked overlapping sprites, first is high, second low.
        v.vram[0xa001] = 128;
        v.vram[0xa003] = 1;
        v.vram[0xa004] = 0x80;
        v.vram[0xa005] = 3;
        v.vram[0xa007] = 128;
        v.vram[0xa009] = 128;
        v.vram[0xa00d] = 1;
        v.vram[0xa00f] = 128;
        v.render_line();
        assert_eq!(v.frame()[0], 0x001f);
        assert_ne!(v.read_status() & 0x20, 0);
        assert_eq!(v.read_status() & 0x20, 0);
    }

    #[test]
    fn scroll_flip_and_window_use_their_own_coordinates() {
        let mut v = visible();
        v.vram[0xc001] = 1;
        v.vram[0xc003] = 2;
        v.vram[0xf001] = 8; // shift A 8 pixels right; x8 displays tile1
        v.start_line(0, false);
        v.render_line();
        assert_eq!(v.frame()[8], 0xf800);
        assert_eq!(v.frame()[16], 0x07e0);
        v.regs[3] = 0x28; // window at A000
        v.regs[17] = 1; // left 16 pixels
        v.vram[0xa001] = 3;
        v.render_line();
        assert_eq!(v.frame()[0], 0x001f);
        assert_eq!(v.frame()[16], 0x07e0);
        // Direct tile decoder covers flips and transparent palette entry 0.
        v.vram[128] = 0x12;
        assert_eq!(v.tile_pixel(4, 0, 0), 1);
        assert_eq!(v.tile_pixel(4, 1, 0), 2);
        assert_eq!(v.tile_pixel(4 | 0x0800, 6, 0), 2);
        assert_eq!(v.tile_pixel(4 | 0x6000, 2, 0), 0);
    }

    #[test]
    fn pal_has_240_lines_and_contiguous_active_width() {
        let mut v = Vdp::new();
        v.regs[1] = 8;
        v.regs[7] = 1;
        v.cram[1] = 0x0eee;
        v.start_line(239, true);
        assert_eq!(v.dimensions(), (256, 240));
        v.render_line();
        assert_eq!(v.frame().len(), 256 * 240);
        assert_eq!(v.frame()[239 * 256], 0xffff);
        v.start_line(239, false);
        assert_eq!(v.dimensions(), (256, 224));
    }

    fn sprite(v: &mut Vdp, attr: u16) {
        v.vram[0xa001] = 128;
        v.vram[0xa004..0xa006].copy_from_slice(&attr.to_be_bytes());
        v.vram[0xa007] = 128;
    }

    #[test]
    fn transparent_plane_priority_controls_shadow_under_planes_and_sprites() {
        let mut v = visible();
        v.regs[12] = 8;
        v.vram[0xe001] = 2; // low B, green
        v.vram[0xc000] = 0x80; // high A, but transparent pattern zero
        v.start_line(0, false);
        v.render_line();
        assert_eq!(v.frame()[0], 0x07e0);
        // Even a low sprite is normal when a transparent high-priority
        // plane is behind it. Priority controls lighting separately.
        sprite(&mut v, 3);
        v.render_line();
        assert_eq!(v.frame()[0], 0x001f);
        v.vram[0xc000] = 0;
        v.render_line();
        assert_eq!(v.frame()[0], rgb565(0x0e00, 0));
    }

    #[test]
    fn shadow_highlight_operators_respect_plane_priority_and_sprite_order() {
        let mut v = visible();
        v.regs[12] = 8;
        v.vram[0xc001] = 1;
        v.vram[4 * 32..5 * 32].fill(0xee);
        v.vram[5 * 32..6 * 32].fill(0xff);
        sprite(&mut v, 0x6004); // palette 3, colour 14
        v.start_line(0, false);
        v.render_line();
        assert_eq!(v.frame()[0], 0xf800); // shadow + highlight = normal
        v.vram[0xc000] = 0x80;
        v.render_line();
        assert_eq!(v.frame()[0], 0xf800); // low operator hidden by high A
        sprite(&mut v, 0xe004); // high operator above high A
        v.render_line();
        assert_eq!(v.frame()[0], rgb565(0x000e, 2));
        sprite(&mut v, 0xe005); // shadow operator forces shadow
        v.render_line();
        assert_eq!(v.frame()[0], rgb565(0x000e, 0));
        v.regs[12] = 0; // operators are ordinary palette colours without S/H
        v.cram[63] = 0x00e0;
        v.render_line();
        assert_eq!(v.frame()[0], 0x07e0);
    }

    #[test]
    fn interlace2_uses_sixteen_row_patterns_and_alternates_fields() {
        let mut v = visible();
        v.regs[12] = 6;
        v.vram[..64].fill(0);
        v.vram[0xc001] = 1;
        for row in 0..16 {
            v.vram[64 + row * 4..64 + row * 4 + 4].fill(if row & 1 == 0 { 0x11 } else { 0x22 });
        }
        v.start_line(0, false);
        let first = if v.odd { 0x07e0 } else { 0xf800 };
        v.render_line();
        assert_eq!(v.dimensions(), (256, 224));
        assert_eq!(v.frame()[0], first);
        v.start_line(0, false);
        v.render_line();
        assert_ne!(v.frame()[0], first);
        // The low scroll bit is meaningful: odd scroll swaps even/odd
        // rows, rather than being discarded in a field-sized renderer.
        v.vsram[0] = 1;
        v.render_line();
        assert_eq!(v.frame()[0], first);
        assert_eq!(v.tile_pixel(0x1001, 0, 0), 2); // flip all 16 rows
        assert_eq!(v.tile_pixel(0x0401, 0, 0), 1); // index bit 10 wraps VRAM
    }

    #[test]
    fn interlace2_sprite_odd_y_and_ten_bit_position_are_preserved() {
        let mut v = visible();
        v.regs[12] = 6;
        v.vram[..64].fill(0);
        v.vram[64..128].fill(0x11);
        sprite(&mut v, 1);
        // A one-physical-line offset makes the sprite start in only the
        // odd field. Dropping bit 0 would move it into both fields.
        v.vram[0xa000..0xa002].copy_from_slice(&257u16.to_be_bytes());
        v.start_line(0, false);
        v.render_line();
        assert_eq!(v.frame()[0], if v.odd { 0xf800 } else { 0 });
        v.start_line(0, false);
        v.render_line();
        assert_eq!(v.frame()[0], if v.odd { 0xf800 } else { 0 });
        // Positions beyond $1ff belong to the bottom half of a full
        // interlaced picture, rather than wrapping to its top half.
        v.vram[0xa000..0xa002].copy_from_slice(&554u16.to_be_bytes());
        v.start_line(149, false);
        v.render_line();
        assert_eq!(v.frame()[149 * 256], 0xf800);
        v.start_line(21, false);
        v.render_line();
        assert_eq!(v.frame()[21 * 256], 0);
    }

    #[test]
    fn interlace_counter_replaces_bit_zero_with_bit_eight() {
        let mut v = Vdp::new();
        v.start_line(235, false); // first NTSC discontinuity: raw $1e5
        assert_eq!(v.hv_counter() >> 8, 0xe5);
        v.regs[12] = 2;
        assert_eq!(v.hv_counter() >> 8, 0xe5);
        v.regs[12] = 6;
        v.start_line(128, false); // raw $100 after mode2 doubling
        assert_eq!(v.hv_counter() >> 8, 1);
        v.start_line(127, false);
        assert_eq!(v.hv_counter() >> 8, 0xfe);
        v.start_line(235, false); // doubled raw $1e5 -> $3ca -> $cb
        assert_eq!(v.hv_counter() >> 8, 0xcb);
    }
    /// Deterministic xorshift for the differential test below.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn byte(&mut self) -> u8 {
            (self.next() >> 24) as u8
        }
    }

    fn same_line(fast: &Vdp, slow: &Vdp, round: usize) {
        let (width, _) = fast.dimensions();
        let y = usize::from(fast.line);
        assert_eq!(
            fast.picture[y * width..(y + 1) * width],
            slow.picture[y * width..(y + 1) * width],
            "round {round}, line {y}, regs {:02x?}",
            fast.regs
        );
        assert_eq!(fast.collision, slow.collision, "collision, round {round}");
        assert_eq!(fast.overflow, slow.overflow, "overflow, round {round}");
    }

    /// The segment renderer must give, bit for bit, what the plain per-pixel one gives, whatever the
    /// registers: scroll modes, the window on either side, 32/64/128-cell planes, interlace, shadow/highlight,
    /// left column blanking, PAL height, sparse and dense patterns and random sprite tables.
    #[test]
    fn render_line_matches_the_per_pixel_reference() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for round in 0..6000 {
            let mut v = Vdp::new();
            let sparse = round % 2 == 0;
            for b in v.vram.iter_mut() {
                *b = rng.byte() & if sparse { rng.byte() & rng.byte() } else { 0xff };
            }
            for r in v.regs.iter_mut() {
                *r = rng.byte();
            }
            v.regs[0] &= 0x24;
            v.regs[1] = 0x44 | (rng.byte() & 0x08);
            if round % 17 == 0 {
                v.regs[1] &= !0x40; // display off: only the background
            }
            v.regs[12] &= 0x0f;
            for c in v.cram.iter_mut() {
                *c = u16::from_le_bytes([rng.byte(), rng.byte()]) & 0x0eee;
            }
            for c in v.vsram.iter_mut() {
                *c = u16::from_le_bytes([rng.byte(), rng.byte()]) & 0x07ff;
            }
            v.pal = rng.byte() & 1 == 1;
            v.odd = rng.byte() & 1 == 1;
            let height = v.dimensions().1 as u64;
            v.line = (rng.next() % height) as u16;

            let mut fast = v.clone();
            fast.render_line();
            let mut slow = v;
            slow.render_line_reference();
            same_line(&fast, &slow, round);

            // The colour table of `fast` is now cached: change CRAM and a line, and it must follow.
            let k = usize::from(rng.byte() & 63);
            let color = u16::from_le_bytes([rng.byte(), rng.byte()]) & 0x0eee;
            fast.cram[k] = color;
            slow.cram[k] = color;
            let height = fast.dimensions().1 as u64;
            fast.line = (rng.next() % height) as u16;
            slow.line = fast.line;
            fast.render_line();
            slow.render_line_reference();
            same_line(&fast, &slow, round);
        }
    }
}
