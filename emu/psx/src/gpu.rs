//! The GPU: 1 MB of VRAM as 1024 x 512 halfwords (RGB555 and a mask bit),
//! drawn into by commands on GP0 (polygons, lines, rectangles, transfers,
//! drawing settings) and set up for display by commands on GP1. Drawing is
//! done in software as each command completes.

use savestate::state;

pub const VRAM_WIDTH: usize = 1024;
pub const VRAM_HEIGHT: usize = 512;

/// The most words a command gathers: a polyline's vertices, cut off there.
const FIFO: usize = 256;

/// The dither added to 8-bit colours before they are cut to 5 bits.
const DITHER: [[i32; 4]; 4] = [[-4, 0, -3, 1], [2, -2, 3, -1], [-3, 1, -4, 0], [3, -1, 2, -2]];

/// What is added without dithering.
const NO_DITHER: [[i32; 4]; 4] = [[0; 4]; 4];

/// A vertex of a primitive, after the drawing offset: its position,
/// colour and texture coordinates.
#[derive(Clone, Copy, Default)]
struct Vertex {
    x: i32,
    y: i32,
    r: i32,
    g: i32,
    b: i32,
    u: i32,
    v: i32,
}

/// How a primitive is drawn: texture (its page and palette), shading,
/// blending.
#[derive(Clone, Copy, Default)]
struct Style {
    textured: bool,
    raw: bool,
    gouraud: bool,
    semi: bool,
    /// Texture page: base x and y, colour depth (0 4-bit, 1 8-bit, 2 15-bit),
    /// semi-transparency mode; the palette's position.
    page_x: i32,
    page_y: i32,
    depth: u32,
    blend: u32,
    clut_x: i32,
    clut_y: i32,
    dither: bool,
}

/// A triangle's rows: the drawing bounds, its edges (with the bias of
/// each, and the step of its function along x), and its values (colour,
/// texture coordinates) at the bounds' corner with their steps along x and
/// y.
struct Rows {
    x0: i32,
    x1: i32,
    y0: i32,
    y1: i32,
    edges: [(Vertex, Vertex); 3],
    bias: [i32; 3],
    step_x: [i32; 3],
    values: [[i64; 3]; 5],
}

/// A rectangle: its corner (with its colour), size, texture corner, flips.
struct Rect {
    origin: Vertex,
    width: i32,
    height: i32,
    u0: u32,
    v0: u32,
    flip: (bool, bool),
}

/// What all the pixels of a primitive share: the texture window (as masks
/// to apply), page, blending, dithering, the mask bit settings.
struct Shader {
    u_and: u32,
    u_or: u32,
    v_and: u32,
    v_or: u32,
    page_x: u32,
    page_y: u32,
    blend: u32,
    dither: &'static [[i32; 4]; 4],
    check_mask: bool,
    set_mask: u16,
}

impl Shader {
    /// Whether the texture window changes any texel's coordinates.
    fn windowed(&self) -> bool {
        self.u_and != 0xff || self.v_and != 0xff
    }
}

/// A transfer in progress between the CPU and a rectangle of VRAM.
#[derive(Clone, Copy, Default)]
struct Transfer {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    /// The halfword of the rectangle next.
    i: u32,
}

state!(Transfer { x, y, w, h, i });

#[derive(Clone)]
pub struct Gpu {
    vram: Box<[u16; VRAM_WIDTH * VRAM_HEIGHT]>,
    /// GP0(E1): texture page, dither, drawing to the display, texture
    /// disable, rectangle flips; GP0(E2) texture window; GP0(E3-E5) the
    /// drawing area and offset; GP0(E6) mask bit settings.
    draw_mode: u32,
    window: u32,
    area: [i32; 4],
    offset: (i32, i32),
    set_mask: bool,
    check_mask: bool,
    /// GP1: display start, horizontal and vertical ranges, mode, display
    /// off, DMA direction, the interrupt flag.
    display_x: u32,
    display_y: u32,
    h_range: u32,
    v_range: u32,
    mode: u32,
    display_off: bool,
    dma_direction: u32,
    /// GP1(09h): GP0(E1h) bit 11 may turn textures off.
    texture_disable: bool,
    irq: bool,
    /// The command words gathered so far, and how many it takes.
    fifo: [u32; FIFO],
    fifo_len: usize,
    needed: usize,
    polyline: bool,
    load: Option<Transfer>,
    store: Option<Transfer>,
    /// What GPUREAD gives when no transfer runs.
    read_latch: u32,
    pub(crate) odd: bool,
    /// The palette as the GPU loaded it for 4- and 8-bit textures, and what
    /// it loaded (the palette's position and whether 8-bit; FFFF_FFFF
    /// none): drawing over the palette in VRAM does not change it.
    clut: [u16; 256],
    clut_loaded: u32,
    /// False for the copy that only follows the commands, for what the CPU
    /// reads, while another draws them (see `video`); that copy notes the
    /// VRAM they draw in (its corners, inclusive).
    pub(crate) draws: bool,
    pub(crate) touched: Vec<[u32; 4]>,
}

state!(Gpu { vram, draw_mode, window, area, offset, set_mask, check_mask, display_x, display_y, h_range, v_range,
    mode, display_off, dma_direction, texture_disable, irq, fifo, fifo_len, needed, polyline, load, store, read_latch, odd,
    clut, clut_loaded });

impl Gpu {
    pub fn new() -> Gpu {
        Gpu {
            vram: vec![0; VRAM_WIDTH * VRAM_HEIGHT].into_boxed_slice().try_into().unwrap(),
            draw_mode: 0,
            window: 0,
            area: [0, 0, 0, 0],
            offset: (0, 0),
            set_mask: false,
            check_mask: false,
            display_x: 0,
            display_y: 0,
            h_range: 0xc6_0260,
            v_range: 0x04_2c10,
            mode: 0,
            display_off: true,
            dma_direction: 0,
            texture_disable: false,
            irq: false,
            fifo: [0; FIFO],
            fifo_len: 0,
            needed: 0,
            polyline: false,
            load: None,
            store: None,
            read_latch: 0,
            odd: false,
            clut: [0; 256],
            clut_loaded: u32::MAX,
            draws: true,
            touched: Vec::new(),
        }
    }

    pub fn vram(&self) -> &[u16] {
        &self.vram[..]
    }

    /// The line the display is on: GPUSTAT bit 31 alternates with them.
    pub fn next_line(&mut self, line: u32) {
        self.odd = line & 1 != 0;
    }

    /// Whether a VRAM-to-CPU transfer runs.
    pub fn storing(&self) -> bool {
        self.store.is_some()
    }

    pub fn status(&self, _line: u32) -> u32 {
        let mut s = self.draw_mode & 0x7ff;
        s |= u32::from(self.set_mask) << 11 | u32::from(self.check_mask) << 12;
        s |= 1 << 13;
        s |= (self.draw_mode >> 11 & 1) << 15;
        s |= (self.mode >> 6 & 1) << 16 | (self.mode & 3) << 17 | (self.mode >> 2 & 1) << 19;
        s |= (self.mode >> 3 & 1) << 20 | (self.mode >> 4 & 1) << 21 | (self.mode >> 5 & 1) << 22;
        s |= u32::from(self.display_off) << 23 | u32::from(self.irq) << 24;
        // Ready for commands and DMA; ready to send VRAM while a store runs.
        let sending = self.store.is_some();
        s |= 1 << 26 | u32::from(sending) << 27 | 1 << 28;
        s |= match self.dma_direction {
            1 | 2 => 1 << 25,
            3 => u32::from(sending) << 25,
            _ => 0,
        };
        s |= self.dma_direction << 29;
        // In 480-line interlace the bit is the field; else it follows lines.
        s |= u32::from(self.odd) << 31;
        s
    }

    /// GPUREAD: the next pixels of a VRAM store, or what GP1(10) asked.
    pub fn read(&mut self) -> u32 {
        let Some(mut t) = self.store else {
            return self.read_latch;
        };
        let mut word = 0;
        for half in 0..2 {
            if t.i < t.w * t.h {
                let (x, y) = (t.x + t.i % t.w, t.y + t.i / t.w);
                word |= u32::from(self.pixel(x, y)) << (16 * half);
                t.i += 1;
            }
        }
        self.store = if t.i < t.w * t.h { Some(t) } else { None };
        word
    }

    fn pixel(&self, x: u32, y: u32) -> u16 {
        self.vram[(y as usize & 511) * VRAM_WIDTH + (x as usize & 1023)]
    }

    fn set_pixel(&mut self, x: u32, y: u32, value: u16) {
        self.vram[(y as usize & 511) * VRAM_WIDTH + (x as usize & 1023)] = value;
    }

    pub fn gp1(&mut self, value: u32) {
        match value >> 24 {
            0x00 => {
                let mut fresh = Gpu { draws: self.draws, ..Gpu::new() };
                std::mem::swap(&mut fresh.vram, &mut self.vram);
                *self = fresh;
            }
            0x01 => {
                self.fifo_len = 0;
                self.needed = 0;
                self.polyline = false;
                self.load = None;
            }
            0x02 => self.irq = false,
            0x03 => self.display_off = value & 1 != 0,
            0x04 => self.dma_direction = value & 3,
            0x05 => {
                self.display_x = value & 0x3fe;
                self.display_y = value >> 10 & 0x1ff;
            }
            0x06 => self.h_range = value & 0xff_ffff,
            0x07 => self.v_range = value & 0xf_ffff,
            0x08 => self.mode = value & 0xff,
            0x09 => self.texture_disable = value & 1 != 0,
            0x10..=0x1f => {
                self.read_latch = match value & 7 {
                    2 => self.window,
                    3 => (self.area[0] | self.area[1] << 10) as u32,
                    4 => (self.area[2] | self.area[3] << 10) as u32,
                    5 => (self.offset.0 as u32 & 0x7ff) | (self.offset.1 as u32 & 0x7ff) << 11,
                    7 => 2,
                    _ => self.read_latch,
                };
            }
            _ => {}
        }
    }

    pub fn gp0(&mut self, word: u32) {
        // Pixels of a CPU-to-VRAM transfer.
        if let Some(mut t) = self.load {
            if !self.draws {
                self.touch(t.x, t.y, t.x + t.w - 1, t.y + t.h - 1);
            }
            for half in 0..2 {
                if t.i < t.w * t.h {
                    let (x, y) = (t.x + t.i % t.w, t.y + t.i / t.w);
                    if self.draws {
                        self.masked_write(x, y, (word >> (16 * half)) as u16);
                    }
                    t.i += 1;
                }
            }
            self.load = if t.i < t.w * t.h { Some(t) } else { None };
            return;
        }
        if self.fifo_len == 0 {
            self.needed = command_length(word >> 24);
            self.polyline = matches!(word >> 24, 0x48..=0x4f | 0x58..=0x5f);
        }
        self.fifo[self.fifo_len] = word;
        self.fifo_len += 1;
        // A polyline ends at a word with the 5xxx5xxx pattern, after two
        // vertices at least.
        if self.polyline {
            let end = self.fifo_len > 3 && word & 0xf000_f000 == 0x5000_5000;
            if end || self.fifo_len == FIFO {
                let (words, len) = (self.fifo, self.fifo_len);
                self.fifo_len = 0;
                self.polyline = false;
                if self.draws {
                    self.draw_polyline(&words[..len]);
                } else {
                    self.touch_area();
                }
            }
            return;
        }
        if self.fifo_len >= self.needed {
            // No command but a polyline takes more than 12 words.
            let mut words = [0; 12];
            let len = self.fifo_len;
            words[..len].copy_from_slice(&self.fifo[..len]);
            self.fifo_len = 0;
            self.execute(&words[..len]);
        }
    }

    /// GP0 words in a row: a whole command among them is taken as it is,
    /// not gathered a word at a time (the state left the same).
    pub fn gp0_all(&mut self, words: &[u32]) {
        let mut at = 0;
        while let Some(&word) = words.get(at) {
            if self.load.is_none() && self.fifo_len == 0 && !matches!(word >> 24, 0x48..=0x4f | 0x58..=0x5f) {
                let n = command_length(word >> 24);
                if let Some(command) = words.get(at..at + n) {
                    (self.needed, self.polyline) = (n, false);
                    self.fifo[..n].copy_from_slice(command);
                    self.execute(command);
                    at += n;
                    continue;
                }
            }
            self.gp0(word);
            at += 1;
        }
    }

    /// A write that respects the mask bit settings.
    fn masked_write(&mut self, x: u32, y: u32, value: u16) {
        let i = (y as usize & 511) * VRAM_WIDTH + (x as usize & 1023);
        if self.check_mask && self.vram[i] & 0x8000 != 0 {
            return;
        }
        self.vram[i] = value | u16::from(self.set_mask) << 15;
    }

    fn execute(&mut self, w: &[u32]) {
        let command = w[0] >> 24;
        match command {
            0x02 | 0x40..=0x47 | 0x50..=0x57 | 0x60..=0x9f if !self.draws => self.touch_command(w),
            0x02 => self.fill(w),
            // Clear cache.
            0x01 => self.clut_loaded = u32::MAX,
            0x1f => self.irq = true,
            0x20..=0x3f => {
                self.take_page(w);
                if self.draws {
                    self.polygon(w);
                } else {
                    self.touch_area();
                }
            }
            0x40..=0x47 | 0x50..=0x57 => {
                let gouraud = command & 0x10 != 0;
                let v0 = self.vertex(w[1], w[0]);
                let v1 = if gouraud { self.vertex(w[3], w[2]) } else { self.vertex(w[2], w[0]) };
                self.line(v0, v1, gouraud, command & 2 != 0);
            }
            0x60..=0x7f => self.rectangle(w),
            0x80..=0x9f => self.copy(w),
            0xa0..=0xbf => {
                let (x, y, wd, h) = transfer_rect(w[1], w[2]);
                self.load = Some(Transfer { x, y, w: wd, h, i: 0 });
            }
            0xc0..=0xdf => {
                let (x, y, wd, h) = transfer_rect(w[1], w[2]);
                self.store = Some(Transfer { x, y, w: wd, h, i: 0 });
            }
            0xe1 => self.draw_mode = w[0] & self.draw_mode_mask(),
            0xe2 => self.window = w[0] & 0xf_ffff,
            0xe3 => self.area[0..2].copy_from_slice(&[(w[0] & 0x3ff) as i32, (w[0] >> 10 & 0x1ff) as i32]),
            0xe4 => self.area[2..4].copy_from_slice(&[(w[0] & 0x3ff) as i32, (w[0] >> 10 & 0x1ff) as i32]),
            0xe5 => self.offset = (sign11(w[0]), sign11(w[0] >> 11)),
            0xe6 => {
                self.set_mask = w[0] & 1 != 0;
                self.check_mask = w[0] & 2 != 0;
            }
            _ => {}
        }
    }

    /// The VRAM a command draws in, noted by the copy that does not draw:
    /// a fill's or copy's rectangle, else the drawing area.
    fn touch_command(&mut self, w: &[u32]) {
        match w[0] >> 24 {
            0x02 => {
                let (x, y, width, height) = fill_rect(w);
                if width > 0 && height > 0 {
                    self.touch(x, y, x + width - 1, y + height - 1);
                }
            }
            0x80..=0x9f => {
                let (_, (x, y), (width, height)) = copy_rects(w);
                self.touch(x, y, x + width - 1, y + height - 1);
            }
            _ => self.touch_area(),
        }
    }

    fn touch_area(&mut self) {
        let [x0, y0, x1, y1] = self.area;
        if x0 <= x1 && y0 <= y1 {
            self.touch(x0 as u32, y0 as u32, x1 as u32, y1 as u32);
        }
    }

    /// Adds a rectangle to the VRAM touched: all of it for one that wraps
    /// past the edge.
    fn touch(&mut self, x0: u32, y0: u32, x1: u32, y1: u32) {
        let r = if x1 < 1024 && y1 < 512 { [x0, y0, x1, y1] } else { [0, 0, 1023, 511] };
        add_rect(&mut self.touched, r);
    }

    /// A textured polygon's page (in its second vertex's texture word)
    /// becomes the drawing mode's, as on the chip.
    fn take_page(&mut self, w: &[u32]) {
        let command = w[0] >> 24;
        if command & 4 != 0 {
            let page = w[if command & 0x10 != 0 { 5 } else { 4 }] >> 16;
            let mask = 0x9ff & self.draw_mode_mask();
            self.draw_mode = self.draw_mode & !mask | page & mask;
        }
    }

    /// The bits of GP0(E1h) kept: bit 11 only when GP1(09h) allows it.
    fn draw_mode_mask(&self) -> u32 {
        if self.texture_disable { 0x3fff } else { 0x37ff }
    }

    /// A vertex word (and the colour word of its command or its own), with
    /// the drawing offset.
    fn vertex(&self, xy: u32, color: u32) -> Vertex {
        Vertex {
            x: sign11(xy) + self.offset.0,
            y: sign11(xy >> 16) + self.offset.1,
            r: (color & 0xff) as i32,
            g: (color >> 8 & 0xff) as i32,
            b: (color >> 16 & 0xff) as i32,
            u: 0,
            v: 0,
        }
    }

    /// GP0(02): a rectangle filled with a colour, in 16-pixel steps across,
    /// regardless of the drawing area and mask.
    fn fill(&mut self, w: &[u32]) {
        let color = rgb15(w[0]);
        let (x0, y0, width, height) = fill_rect(w);
        for y in y0..y0 + height {
            for x in x0..x0 + width {
                self.set_pixel(x, y, color);
            }
        }
    }

    /// GP0(80): a rectangle of VRAM copied, pixel by pixel in order.
    fn copy(&mut self, w: &[u32]) {
        let ((sx, sy), (dx, dy), (width, height)) = copy_rects(w);
        for y in 0..height {
            for x in 0..width {
                let value = self.pixel(sx + x, sy + y);
                self.masked_write(dx + x, dy + y, value);
            }
        }
    }

    /// The style a polygon's or rectangle's command gives, with the texture
    /// page and palette of its words when textured.
    fn style(&self, command: u32, clut: u32, page: u32) -> Style {
        let textured = command & 4 != 0 && self.draw_mode & 0x800 == 0;
        Style {
            textured,
            raw: command & 1 != 0,
            gouraud: command & 0x10 != 0,
            semi: command & 2 != 0,
            page_x: ((page & 0xf) * 64) as i32,
            page_y: ((page >> 4 & 1) * 256) as i32,
            depth: (page >> 7 & 3).min(2),
            blend: page >> 5 & 3,
            clut_x: ((clut & 0x3f) * 16) as i32,
            clut_y: (clut >> 6 & 0x1ff) as i32,
            dither: self.draw_mode & 0x200 != 0,
        }
    }

    fn polygon(&mut self, w: &[u32]) {
        let command = w[0] >> 24;
        let (quad, textured, gouraud) = (command & 8 != 0, command & 4 != 0, command & 0x10 != 0);
        let n = if quad { 4 } else { 3 };
        let mut vertices = [Vertex::default(); 4];
        let (mut clut, mut page) = (0, self.draw_mode);
        let mut i = 1;
        for (k, vertex) in vertices.iter_mut().enumerate().take(n) {
            let color = if gouraud && k > 0 {
                i += 1;
                w[i - 1]
            } else {
                w[0]
            };
            *vertex = self.vertex(w[i], color);
            i += 1;
            if textured {
                let uv = w[i];
                vertex.u = (uv & 0xff) as i32;
                vertex.v = (uv >> 8 & 0xff) as i32;
                match k {
                    0 => clut = uv >> 16,
                    1 => page = uv >> 16,
                    _ => {}
                }
                i += 1;
            }
        }
        let style = self.style(command, clut, page);
        self.load_clut(&style, clut);
        self.triangle([vertices[0], vertices[1], vertices[2]], &style);
        if quad {
            self.triangle([vertices[1], vertices[2], vertices[3]], &style);
        }
    }

    /// A triangle: the pixels whose centres are inside (the top and left
    /// edges in, the bottom and right out), with colour and texture
    /// coordinates interpolated across it as the GPU does: in 12-bit fixed
    /// point from its leftmost vertex, by gradients worked out from its
    /// vertices in order down the screen, wrapping at 256.
    fn triangle(&mut self, v: [Vertex; 3], style: &Style) {
        // Too large a triangle is not drawn.
        let (min_x, max_x) = (v.iter().map(|p| p.x).min().unwrap(), v.iter().map(|p| p.x).max().unwrap());
        let (min_y, max_y) = (v.iter().map(|p| p.y).min().unwrap(), v.iter().map(|p| p.y).max().unwrap());
        if max_x - min_x >= 1024 || max_y - min_y >= 512 {
            return;
        }
        // The edges, turned so that inside is positive for all three.
        let mut e = v;
        let area = edge(&e[0], &e[1], e[2].x, e[2].y);
        if area == 0 {
            return;
        }
        if area < 0 {
            e.swap(1, 2);
        }
        let (x0, x1) = (min_x.max(self.area[0]), max_x.min(self.area[2]));
        let (y0, y1) = (min_y.max(self.area[1]), max_y.min(self.area[3]));
        if x0 > x1 || y0 > y1 {
            return;
        }
        let edges = [(e[1], e[2]), (e[2], e[0]), (e[0], e[1])];
        // A pixel on an edge is in if the edge is a top or a left one.
        let bias = edges.map(|(a, b)| if (a.y == b.y && b.x > a.x) || b.y < a.y { 0 } else { -1 });
        let step_x = edges.map(|(a, b)| a.y - b.y);

        // The gradients, from the vertices sorted by y (ties kept in order).
        let mut s = v;
        if s[2].y < s[1].y {
            s.swap(1, 2);
        }
        if s[1].y < s[0].y {
            s.swap(0, 1);
        }
        if s[2].y < s[1].y {
            s.swap(1, 2);
        }
        let core = if s[1].x <= s[0].x {
            if s[2].x <= s[1].x { 2 } else { 1 }
        } else if s[2].x < s[0].x {
            2
        } else {
            0
        };
        let (a, b, c) = (s[0], s[1], s[2]);
        let denom = (b.x - a.x) * (c.y - b.y) - (c.x - b.x) * (b.y - a.y);
        let core = s[core];
        // A value's start at x0 of the first line, and its steps along x
        // and y. With values under 256 and the triangle under 1024 x 512,
        // the steps' numerators (and so their divisions) fit in 32 bits.
        let gradient = |f: fn(&Vertex) -> i32| {
            let (fa, fb, fc) = (f(&a), f(&b), f(&c));
            let dx = i64::from(((fb - fa) * (c.y - b.y) - (fc - fb) * (b.y - a.y)) * 4096 / denom);
            let dy = i64::from(((b.x - a.x) * (fc - fb) - (c.x - b.x) * (fb - fa)) * 4096 / denom);
            let start = (i64::from(f(&core)) << 12) + 0x800 + dx * i64::from(x0 - core.x) + dy * i64::from(y0 - core.y);
            [start, dx, dy]
        };
        let zero = [0; 3];
        let (r, g, bl) = if style.gouraud {
            (gradient(|p| p.r), gradient(|p| p.g), gradient(|p| p.b))
        } else {
            let flat = |c: i32| [i64::from(c) << 12, 0, 0];
            (flat(v[0].r), flat(v[0].g), flat(v[0].b))
        };
        let (u, tv) = if style.textured { (gradient(|p| p.u), gradient(|p| p.v)) } else { (zero, zero) };
        let dither = style.gouraud || style.textured && !style.raw;
        let rows = Rows { x0, x1, y0, y1, edges, bias, step_x, values: [r, g, bl, u, tv] };
        let shader = self.shader(style, dither);
        // A raw texture ignores the colour, so its shading does not matter.
        let gouraud = style.gouraud && !(style.textured && style.raw);
        let window = shader.windowed();
        macro_rules! draw {
            ($($kind:tt)*) => { self.triangle_rows::<$($kind)*>(&rows, &shader) };
        }
        macro_rules! rows {
            (flat: $(($s:literal, $g:literal)),*; textured: $(($tr:literal, $ts:literal, $tg:literal)),*) => {
                match (style.textured, style.textured && style.raw, style.semi, gouraud, style.depth, window) {
                    $((false, _, $s, $g, _, _) => draw!(false, false, $s, $g, 0, false),)*
                    $((true, $tr, $ts, $tg, 0, false) => draw!(true, $tr, $ts, $tg, 0, false),
                    (true, $tr, $ts, $tg, 1, false) => draw!(true, $tr, $ts, $tg, 1, false),
                    (true, $tr, $ts, $tg, _, false) => draw!(true, $tr, $ts, $tg, 2, false),
                    (true, $tr, $ts, $tg, 0, true) => draw!(true, $tr, $ts, $tg, 0, true),
                    (true, $tr, $ts, $tg, 1, true) => draw!(true, $tr, $ts, $tg, 1, true),
                    (true, $tr, $ts, $tg, _, true) => draw!(true, $tr, $ts, $tg, 2, true),)*
                    _ => unreachable!(),
                }
            };
        }
        rows!(flat: (false, false), (false, true), (true, false), (true, true);
            textured: (false, false, false), (false, false, true), (false, true, false), (false, true, true),
            (true, false, false), (true, true, false));
    }

    /// A triangle's rows: the span of each inside all three edges, its
    /// values stepped a pixel at a time. Made for each kind of shading (and
    /// texture depth, and whether a texture window applies), so that a
    /// pixel is drawn without asking what kind.
    fn triangle_rows<
        const TEX: bool,
        const RAW: bool,
        const SEMI: bool,
        const GOURAUD: bool,
        const DEPTH: u32,
        const WINDOW: bool,
    >(
        &mut self,
        t: &Rows,
        shader: &Shader,
    ) {
        for y in t.y0..=t.y1 {
            let (mut xs, mut xe) = (t.x0, t.x1);
            // A triangle is under 1024 x 512, so its edge functions fit in
            // 32 bits (and divide in one instruction).
            for i in 0..3 {
                let (a, b) = t.edges[i];
                let w = edge(&a, &b, t.x0, y) + t.bias[i];
                let step = t.step_x[i];
                if step > 0 {
                    xs = xs.max(t.x0 + (w.min(0).unsigned_abs()).div_ceil(step as u32) as i32);
                } else if step < 0 {
                    if w < 0 {
                        xe = t.x0 - 1;
                    } else {
                        xe = xe.min(t.x0 + (w as u32 / step.unsigned_abs()) as i32);
                    }
                } else if w < 0 {
                    xe = t.x0 - 1;
                }
            }
            if xs > xe {
                continue;
            }
            // The values at the span's start, then a step a pixel. Only bits
            // 12-19 of a value count, so they are kept to 32 bits (which
            // need only the low 32 bits of the terms).
            let (dx, dy) = ((xs - t.x0) as u32, (y - t.y0) as u32);
            let start = t.values.map(|w| {
                (w[0] as u32).wrapping_add((w[1] as u32).wrapping_mul(dx)).wrapping_add((w[2] as u32).wrapping_mul(dy))
            });
            let steps = t.values.map(|w| w[1] as u32);
            let mut rgb = [start[0], start[1], start[2]];
            let mut uv = [start[3], start[4]];
            let byte = |v: u32| (v >> 12 & 0xff) as i32;
            for x in xs..xe + 1 {
                let (r, g, b, u, v) = (byte(rgb[0]), byte(rgb[1]), byte(rgb[2]), byte(uv[0]), byte(uv[1]));
                self.put::<TEX, RAW, SEMI, DEPTH, WINDOW>(x, y, r, g, b, u, v, shader);
                if GOURAUD {
                    for (v, s) in rgb.iter_mut().zip(&steps[..3]) {
                        *v = v.wrapping_add(*s);
                    }
                }
                if TEX {
                    for (v, s) in uv.iter_mut().zip(&steps[3..]) {
                        *v = v.wrapping_add(*s);
                    }
                }
            }
        }
    }

    /// What a primitive's pixels share, worked out once.
    fn shader(&self, style: &Style, dither: bool) -> Shader {
        let (mask_x, mask_y) = ((self.window & 0x1f) * 8, (self.window >> 5 & 0x1f) * 8);
        let (off_x, off_y) = ((self.window >> 10 & 0x1f) * 8, (self.window >> 15 & 0x1f) * 8);
        Shader {
            u_and: 0xff & !mask_x,
            u_or: off_x & mask_x,
            v_and: 0xff & !mask_y,
            v_or: off_y & mask_y,
            page_x: style.page_x as u32,
            page_y: style.page_y as u32,
            blend: style.blend,
            dither: if dither && style.dither { &DITHER } else { &NO_DITHER },
            check_mask: self.check_mask,
            set_mask: u16::from(self.set_mask) << 15,
        }
    }

    /// One pixel of a primitive: the texel (if textured) modulated by the
    /// colour (unless raw), dithered, blended (if semi-transparent), and
    /// written where the mask lets it. A pixel kept (its mask bit set and
    /// checked, or its texel transparent) is written back as it was, so
    /// that the pixel is drawn without a branch.
    #[allow(clippy::too_many_arguments)]
    #[inline(always)]
    fn put<const TEX: bool, const RAW: bool, const SEMI: bool, const DEPTH: u32, const WINDOW: bool>(
        &mut self,
        x: i32,
        y: i32,
        r: i32,
        g: i32,
        b: i32,
        u: i32,
        v: i32,
        shader: &Shader,
    ) {
        let i = (y as usize & 511) << 10 | (x as usize & 1023);
        let behind = self.vram[i];
        let mut keep = shader.check_mask & (behind & 0x8000 != 0);
        let (mut cr, mut cg, mut cb, semi, mask);
        if TEX {
            let texel = self.texel::<DEPTH, WINDOW>(u, v, shader);
            keep |= texel == 0;
            semi = SEMI && texel & 0x8000 != 0;
            mask = texel & 0x8000;
            let (tr, tg, tb) = (i32::from(texel & 31) << 3, i32::from(texel >> 5 & 31) << 3, i32::from(texel >> 10 & 31) << 3);
            if RAW {
                (cr, cg, cb) = (tr, tg, tb);
            } else {
                (cr, cg, cb) = ((tr * r) >> 7, (tg * g) >> 7, (tb * b) >> 7);
            }
        } else {
            (cr, cg, cb) = (r, g, b);
            semi = SEMI;
            mask = 0;
        }
        let d = shader.dither[y as usize & 3][x as usize & 3];
        (cr, cg, cb) = (cr + d, cg + d, cb + d);
        // Saturated to 8 bits (one instruction), cut to five.
        let five = |c: i32| c.clamp(0, 255) >> 3;
        let mut color = [five(cr), five(cg), five(cb)];
        if semi {
            let back = [i32::from(behind & 31), i32::from(behind >> 5 & 31), i32::from(behind >> 10 & 31)];
            for (c, b) in color.iter_mut().zip(back) {
                *c = match shader.blend {
                    0 => (b + *c) >> 1,
                    1 => (b + *c).min(31),
                    2 => (b - *c).max(0),
                    _ => (b + (*c >> 2)).min(31),
                };
            }
        }
        let value = color[0] as u16 | (color[1] as u16) << 5 | (color[2] as u16) << 10 | mask | shader.set_mask;
        self.vram[i] = if keep { behind } else { value };
    }

    /// Loads the palette a 4- or 8-bit texture uses, unless it is the one
    /// loaded already.
    fn load_clut(&mut self, style: &Style, clut: u32) {
        if !style.textured || style.depth == 2 {
            return;
        }
        let key = clut & 0xffff | style.depth << 16;
        if key == self.clut_loaded {
            return;
        }
        self.clut_loaded = key;
        let entries = if style.depth == 0 { 16 } else { 256 };
        for i in 0..entries {
            self.clut[i] = self.pixel(style.clut_x as u32 + i as u32, style.clut_y as u32);
        }
    }

    /// A texel of the primitive's page (its depth 0 for 4-bit, 1 for 8,
    /// 2 for 15): 4- and 8-bit ones through the palette, in the texture
    /// window if one applies.
    #[inline(always)]
    fn texel<const DEPTH: u32, const WINDOW: bool>(&self, u: i32, v: i32, shader: &Shader) -> u16 {
        let (u, v) = (u as u32 & 0xff, v as u32 & 0xff);
        let (u, v) = if WINDOW { (u & shader.u_and | shader.u_or, v & shader.v_and | shader.v_or) } else { (u, v) };
        let (px, py) = (shader.page_x, shader.page_y + v);
        match DEPTH {
            0 => {
                // A page's rows are 0-255 or 256-511 and its x 0-960, so a
                // 4-bit texel's halfword needs no wrapping.
                let word = self.vram[((py << 10) + px + u / 4) as usize & (VRAM_WIDTH * VRAM_HEIGHT - 1)];
                self.clut[usize::from(word >> ((u & 3) * 4) & 15)]
            }
            1 => {
                let word = self.pixel(px + u / 2, py);
                self.clut[usize::from(word >> ((u & 1) * 8) & 0xff)]
            }
            _ => self.pixel(px + u, py),
        }
    }

    fn rectangle(&mut self, w: &[u32]) {
        let command = w[0] >> 24;
        let textured = command & 4 != 0;
        let origin = self.vertex(w[1], w[0]);
        let (u0, v0, clut) = if textured { (w[2] & 0xff, w[2] >> 8 & 0xff, w[2] >> 16) } else { (0, 0, 0) };
        let (width, height) = match command >> 3 & 3 {
            0 => {
                let size = w[if textured { 3 } else { 2 }];
                ((size & 0x3ff) as i32, (size >> 16 & 0x1ff) as i32)
            }
            1 => (1, 1),
            2 => (8, 8),
            _ => (16, 16),
        };
        let mut style = self.style(command, clut, self.draw_mode);
        self.load_clut(&style, clut);
        style.gouraud = false;
        let flip = (self.draw_mode & 1 << 12 != 0, self.draw_mode & 1 << 13 != 0);
        let shader = self.shader(&style, false);
        let rect = Rect { origin, width, height, u0, v0, flip };
        let window = shader.windowed();
        macro_rules! draw {
            ($($kind:tt)*) => { self.rectangle_rows::<$($kind)*>(&rect, &shader) };
        }
        macro_rules! rows {
            (flat: $($s:literal),*; textured: $(($tr:literal, $ts:literal)),*) => {
                match (style.textured, style.raw, style.semi, style.depth, window) {
                    $((false, _, $s, _, _) => draw!(false, false, $s, 0, false),)*
                    $((true, $tr, $ts, 0, false) => draw!(true, $tr, $ts, 0, false),
                    (true, $tr, $ts, 1, false) => draw!(true, $tr, $ts, 1, false),
                    (true, $tr, $ts, _, false) => draw!(true, $tr, $ts, 2, false),
                    (true, $tr, $ts, 0, true) => draw!(true, $tr, $ts, 0, true),
                    (true, $tr, $ts, 1, true) => draw!(true, $tr, $ts, 1, true),
                    (true, $tr, $ts, _, true) => draw!(true, $tr, $ts, 2, true),)*
                }
            };
        }
        rows!(flat: false, true; textured: (false, false), (false, true), (true, false), (true, true));
    }

    fn rectangle_rows<const TEX: bool, const RAW: bool, const SEMI: bool, const DEPTH: u32, const WINDOW: bool>(
        &mut self,
        r: &Rect,
        shader: &Shader,
    ) {
        let (flip_x, flip_y) = r.flip;
        let o = r.origin;
        // The part inside the drawing area.
        let (dx0, dx1) = ((self.area[0] - o.x).max(0), (self.area[2] - o.x).min(r.width - 1));
        let (dy0, dy1) = ((self.area[1] - o.y).max(0), (self.area[3] - o.y).min(r.height - 1));
        for dy in dy0..=dy1 {
            let y = o.y + dy;
            let v = if flip_y { r.v0 as i32 - dy } else { r.v0 as i32 + dy };
            for dx in dx0..dx1 + 1 {
                // Flipped, the texture starts a texel on (from an odd u).
                let u = if flip_x { (r.u0 | 1) as i32 - dx } else { r.u0 as i32 + dx };
                self.put::<TEX, RAW, SEMI, DEPTH, WINDOW>(o.x + dx, y, o.r, o.g, o.b, u, v, shader);
            }
        }
    }

    /// A line from one vertex to the other, both ends drawn, its colour
    /// stepped along it when shaded. Positions step in 32.32 fixed point
    /// from the middle of the first pixel (lines going left start from
    /// their other end), by divisions rounded away from zero.
    fn line(&mut self, mut a: Vertex, mut b: Vertex, gouraud: bool, semi: bool) {
        let (dx, dy) = (b.x - a.x, b.y - a.y);
        if dx.abs() >= 1024 || dy.abs() >= 512 {
            return;
        }
        let steps = dx.abs().max(dy.abs());
        if a.x >= b.x && steps > 0 {
            std::mem::swap(&mut a, &mut b);
        }
        let divide = |delta: i32| {
            if steps == 0 {
                return 0;
            }
            let delta = i64::from(delta) << 32;
            let k = i64::from(steps);
            let rounded = if delta < 0 { delta - (k - 1) } else if delta > 0 { delta + (k - 1) } else { 0 };
            rounded / k
        };
        let (step_x, step_y) = (divide(b.x - a.x), divide(b.y - a.y));
        let mut x = (i64::from(a.x) << 32 | 1 << 31) - 1024;
        let mut y = (i64::from(a.y) << 32 | 1 << 31) - if step_y < 0 { 1024 } else { 0 };
        let color_step = |from: i32, to: i32| if steps == 0 { 0 } else { ((to - from) << 12) / steps };
        let (step_r, step_g, step_b) = (color_step(a.r, b.r), color_step(a.g, b.g), color_step(a.b, b.b));
        let (mut r, mut g, mut bl) = (a.r << 12 | 0x800, a.g << 12 | 0x800, a.b << 12 | 0x800);
        let style = Style { semi, blend: self.draw_mode >> 5 & 3, dither: self.draw_mode & 0x200 != 0, ..Style::default() };
        // Lines are dithered shaded or not.
        let shader = self.shader(&style, true);
        for _ in 0..=steps {
            // Coordinates wrap at 2048, as signed 11-bit numbers.
            let (px, py) = (sign11((x >> 32) as u32), sign11((y >> 32) as u32));
            if px >= self.area[0] && px <= self.area[2] && py >= self.area[1] && py <= self.area[3] {
                let (cr, cg, cb) = if gouraud { (r >> 12 & 0xff, g >> 12 & 0xff, bl >> 12 & 0xff) } else { (a.r, a.g, a.b) };
                if semi {
                    self.put::<false, false, true, 0, false>(px, py, cr, cg, cb, 0, 0, &shader);
                } else {
                    self.put::<false, false, false, 0, false>(px, py, cr, cg, cb, 0, 0, &shader);
                }
            }
            x += step_x;
            y += step_y;
            r += step_r;
            g += step_g;
            bl += step_b;
        }
    }

    fn draw_polyline(&mut self, w: &[u32]) {
        let gouraud = w[0] >> 24 & 0x10 != 0;
        let semi = w[0] >> 24 & 2 != 0;
        let mut vertices = Vec::new();
        let mut i = 1;
        let mut color = w[0];
        while i < w.len() {
            if gouraud && i > 1 {
                if w[i] & 0xf000_f000 == 0x5000_5000 {
                    break;
                }
                color = w[i];
                i += 1;
            }
            if i >= w.len() || (i > 1 && w[i] & 0xf000_f000 == 0x5000_5000) {
                break;
            }
            vertices.push(self.vertex(w[i], color));
            i += 1;
        }
        for pair in vertices.windows(2) {
            self.line(pair[0], pair[1], gouraud, semi);
        }
    }

    /// What is shown, as GP1 set it up.
    pub fn display(&self) -> Display {
        Display { x: self.display_x, y: self.display_y, mode: self.mode, off: self.display_off }
    }

    pub fn picture(&self) -> (usize, usize, Vec<u8>) {
        self.display().picture(&self.vram[..])
    }
}

/// What GP1 sets up to be shown: where the display area starts in VRAM,
/// the video mode, whether the display is off.
#[derive(Clone, Copy, PartialEq)]
pub struct Display {
    x: u32,
    y: u32,
    mode: u32,
    off: bool,
}

impl Display {
    /// As wide as the horizontal resolution and as high as the vertical.
    fn size(&self) -> (usize, usize) {
        let width = match (self.mode >> 6 & 1, self.mode & 3) {
            (1, _) => 368,
            (_, 0) => 256,
            (_, 1) => 320,
            (_, 2) => 512,
            _ => 640,
        };
        (width, if self.mode & 0x24 == 0x24 { 480 } else { 240 })
    }

    /// The VRAM shown, as its corners (inclusive; all of VRAM for an area
    /// that wraps), or none while the display is off.
    pub fn area(&self) -> Option<[u32; 4]> {
        if self.off {
            return None;
        }
        let (width, height) = self.size();
        let halfwords = if self.mode & 0x10 != 0 { width * 3 / 2 } else { width } as u32;
        let (x1, y1) = (self.x + halfwords - 1, self.y + height as u32 - 1);
        Some(if x1 < 1024 && y1 < 512 { [self.x, self.y, x1, y1] } else { [0, 0, 1023, 511] })
    }

    /// The halfwords a row of the picture takes.
    fn row_halfwords(&self) -> usize {
        let (width, _) = self.size();
        if self.mode & 0x10 != 0 { width * 3 / 2 } else { width }
    }

    /// The display area's halfwords, row after row (wrapping round VRAM as
    /// the display does): what the picture is made of.
    pub fn rows(&self, vram: &[u16]) -> Vec<u16> {
        let (_, height) = self.size();
        let (n, start) = (self.row_halfwords(), self.x as usize);
        let mut out = Vec::with_capacity(n * height);
        if self.off {
            return out;
        }
        for y in 0..height {
            let row = &vram[((self.y as usize + y) & 511) * VRAM_WIDTH..][..VRAM_WIDTH];
            if start + n <= VRAM_WIDTH {
                out.extend_from_slice(&row[start..start + n]);
            } else {
                out.extend((0..n).map(|i| row[(start + i) & 1023]));
            }
        }
        out
    }

    /// The picture as RGB888, from the display area's rows: in 15-bit
    /// colour or 24 (three bytes a pixel in the halfwords of a row).
    pub fn picture_of(&self, rows: &[u16]) -> (usize, usize, Vec<u8>) {
        let (width, height) = self.size();
        let mut out = vec![0u8; width * height * 3];
        if self.off {
            return (width, height, out);
        }
        for (row, line) in rows.chunks_exact(self.row_halfwords()).zip(out.chunks_exact_mut(width * 3)) {
            if self.mode & 0x10 != 0 {
                for (i, byte) in line.iter_mut().enumerate() {
                    *byte = (row[i / 2] >> ((i & 1) * 8)) as u8;
                }
            } else {
                let c = |v: u16| ((v & 31) << 3 | (v & 31) >> 2) as u8;
                for (&p, pixel) in row.iter().zip(line.chunks_exact_mut(3)) {
                    pixel.copy_from_slice(&[c(p), c(p >> 5), c(p >> 10)]);
                }
            }
        }
        (width, height, out)
    }

    pub fn picture(&self, vram: &[u16]) -> (usize, usize, Vec<u8>) {
        self.picture_of(&self.rows(vram))
    }
}

/// The words a GP0 command takes, its first included (a polyline's are
/// counted until its end word).
fn command_length(command: u32) -> usize {
    match command {
        0x02 => 3,
        0x20..=0x3f => {
            let n = if command & 8 != 0 { 4 } else { 3 };
            let textured = command & 4 != 0;
            let gouraud = command & 0x10 != 0;
            1 + n + if textured { n } else { 0 } + if gouraud { n - 1 } else { 0 }
        }
        0x40..=0x5f => {
            if command & 0x10 != 0 {
                4
            } else {
                3
            }
        }
        0x60..=0x7f => 2 + usize::from(command & 4 != 0) + usize::from(command >> 3 & 3 == 0),
        0x80..=0x9f => 4,
        0xa0..=0xdf => 3,
        _ => 1,
    }
}

/// Adds a rectangle (corners inclusive) to a short list of them: into one
/// it fits in, or over one that fits in it, else at the end; past eight
/// it goes into the last one.
pub fn add_rect(list: &mut Vec<[u32; 4]>, r: [u32; 4]) {
    let inside = |a: &[u32; 4], b: &[u32; 4]| a[0] >= b[0] && a[1] >= b[1] && a[2] <= b[2] && a[3] <= b[3];
    if let Some(t) = list.iter_mut().rev().find(|t| inside(&r, t) || inside(t, &r)) {
        if inside(t, &r) {
            *t = r;
        }
        return;
    }
    if list.len() < 8 {
        list.push(r);
    } else if let Some(t) = list.last_mut() {
        *t = [t[0].min(r[0]), t[1].min(r[1]), t[2].max(r[2]), t[3].max(r[3])];
    }
}

/// GP0(02)'s rectangle: its corner and size, in 16-pixel steps across.
fn fill_rect(w: &[u32]) -> (u32, u32, u32, u32) {
    (w[1] & 0x3f0, w[1] >> 16 & 0x1ff, (w[2] & 0x3ff).div_ceil(16) * 16, w[2] >> 16 & 0x1ff)
}

/// GP0(80)'s source and destination corners, and size (0 meaning the
/// most).
fn copy_rects(w: &[u32]) -> ((u32, u32), (u32, u32), (u32, u32)) {
    let width = ((w[3] & 0x3ff).wrapping_sub(1) & 0x3ff) + 1;
    let height = ((w[3] >> 16 & 0x1ff).wrapping_sub(1) & 0x1ff) + 1;
    ((w[1] & 0x3ff, w[1] >> 16 & 0x1ff), (w[2] & 0x3ff, w[2] >> 16 & 0x1ff), (width, height))
}

/// A transfer's rectangle: its corner, and its size (0 meaning the most).
fn transfer_rect(xy: u32, size: u32) -> (u32, u32, u32, u32) {
    let (x, y) = (xy & 0x3ff, xy >> 16 & 0x1ff);
    let w = ((size & 0x3ff).wrapping_sub(1) & 0x3ff) + 1;
    let h = ((size >> 16 & 0x1ff).wrapping_sub(1) & 0x1ff) + 1;
    (x, y, w, h)
}

fn sign11(v: u32) -> i32 {
    ((v << 21) as i32) >> 21
}

fn rgb15(color: u32) -> u16 {
    ((color >> 3 & 31) | (color >> 11 & 31) << 5 | (color >> 19 & 31) << 10) as u16
}

/// Twice the signed area of a, b, p: positive with p to the left.
fn edge(a: &Vertex, b: &Vertex, x: i32, y: i32) -> i32 {
    (b.x - a.x) * (y - a.y) - (b.y - a.y) * (x - a.x)
}
