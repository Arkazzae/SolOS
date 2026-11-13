//! The RDP (the RCP's rasterizer): its command interface and its state.
//! The CPU or the RSP points DPC_START and DPC_END at a list of commands,
//! in RDRAM or (through the XBUS) in DMEM; the RDP fetches and runs them as
//! soon as DPC_END is written, a command at a time (one that the end cuts
//! is kept until the rest arrives). SYNC_FULL raises the DP interrupt,
//! which is how libultra learns a frame's drawing is done.
//!
//! Drawing happens at once (on the drawing threads), but the RDP takes its
//! time as the chip would (`Timing`): DPC_STATUS shows it busy, and the DP
//! interrupt comes, once it would have done the work it was given.
//!
//! Commands are 64-bit words (n64brew, "Reality Display Processor/
//! Commands"); this file holds the ones that set state and load textures,
//! raster.rs draws.

use crate::N64;
use savestate::state;

// DPC_STATUS, as read: commands from DMEM, frozen, flushing, the clock
// running and the pipeline busy (from the DPC_END write that starts a run,
// or work given, until a SYNC_FULL is done), commands being worked (until
// the RDP is through with what it was given), the buffer ready, and the
// start and end waiting for the transfer in progress.
const XBUS: u32 = 1 << 0;
const FREEZE: u32 = 1 << 1;
const FLUSH: u32 = 1 << 2;
const START_GCLK: u32 = 1 << 3;
const PIPE_BUSY: u32 = 1 << 5;
const CMD_BUSY: u32 = 1 << 6;
const CBUF_READY: u32 = 1 << 7;
const END_PENDING: u32 = 1 << 9;
const START_PENDING: u32 = 1 << 10;

/// A tile descriptor: how a texture lies in TMEM, and how coordinates
/// reach it (Set Tile, Set Tile Size).
#[derive(Clone, Copy, Default)]
pub struct Tile {
    pub format: u32,
    pub size: u32,
    /// A line's length and the start, in bytes of TMEM.
    pub line: u32,
    pub tmem: u32,
    pub palette: u32,
    /// For s and t: clamp, mirror, mask (bits of the wrap) and shift.
    pub clamp: [bool; 2],
    pub mirror: [bool; 2],
    pub mask: [u32; 2],
    pub shift: [u32; 2],
    /// The tile's extent, upper left and lower right, in 10.2.
    pub lo: [u32; 2],
    pub hi: [u32; 2],
}

state!(Tile { format, size, line, tmem, palette, clamp, mirror, mask, shift, lo, hi });

/// An image in RDRAM: the color or texture image.
#[derive(Clone, Copy, Default)]
pub struct Image {
    pub format: u32,
    pub size: u32,
    pub width: u32,
    pub addr: u32,
}

state!(Image { format, size, width, addr });

// What the RDP's work takes, in its clocks (62.5 MHz: two for three of the
// CPU's), roughly, as gopher64 counts it (`rdp_process_commands`): a
// primitive sets up in 16, then draws the pixels the scissor leaves it, one
// a clock in 1-cycle mode, one in two clocks in 2-cycle mode, 64 bits a
// clock in fill and copy modes; a texture load moves 64 bits a clock; each
// line of a Load Tile or a textured rectangle is a transfer from RDRAM of
// its own, 16 clocks more; a sync waits 50 for the pipeline to drain.
const SETUP_CLOCKS: u64 = 16;
const LINE_CLOCKS: u64 = 16;
const SYNC_CLOCKS: u64 = 50;

/// SYNC_FULLs whose interrupts can wait at once (more are merged into the
/// newest).
const SYNCS: usize = 4;

/// When the RDP is done with what it was given, kept in the state as the
/// record "rdp.timing". Drawing happens at once on our threads; what waits
/// for these times is DPC_STATUS's busy bits and the DP interrupt.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Timing {
    /// The CPU cycle at which the RDP is through with the commands it was
    /// given (CMD_BUSY until then); work given later starts when it is
    /// given or when this is reached, whichever is later.
    pub busy_until: u64,
    /// When the last SYNC_FULL is done and the RDP rests (START_GCLK and
    /// PIPE_BUSY clear); u64::MAX while none waits or work came after it.
    pub rest_at: u64,
    /// The SYNC_FULLs whose DP interrupts are still to come, oldest first,
    /// by when each is done (u64::MAX after the last).
    pub syncs: [u64; SYNCS],
    /// How many of them, the newest, the RSP sent (or graphics HLE, running
    /// a task): their interrupts also wait for the RSP to stop, so that the
    /// DP interrupt never comes before the SP one of the task that sent the
    /// list (Resident Evil 2's scheduler takes it as a task done twice).
    pub waiting: u32,
    /// When the RDP's event (`N64::rdp_event`) is next due; u64::MAX: none.
    pub event: u64,
}

state!(Timing { busy_until, rest_at, syncs, waiting, event });

impl Timing {
    /// An RDP with nothing to do, as at power on (and in states saved
    /// before the record).
    pub(crate) const IDLE: Timing =
        Timing { busy_until: 0, rest_at: u64::MAX, syncs: [u64::MAX; SYNCS], waiting: 0, event: u64::MAX };

    fn pending(&self) -> usize {
        self.syncs.iter().take_while(|&&at| at != u64::MAX).count()
    }

    /// When the oldest SYNC_FULL's interrupt is due, if it does not wait
    /// for the RSP.
    fn next_interrupt(&self) -> u64 {
        if self.pending() > self.waiting as usize { self.syncs[0] } else { u64::MAX }
    }
}

#[derive(Clone)]
pub struct Rdp {
    /// Host preferences/counters, excluded from the emulated state.
    pub(crate) accelerated: bool,
    pub(crate) rgba16_sampling: bool,
    pub(crate) lazy_color_reads: bool,
    pub(crate) fast_combiner: bool,
    pub(crate) fast_spans: bool,
    pub(crate) combine_program: [Option<crate::raster::CombinePlan>; 2],
    pub(crate) profiling: bool,
    /// The samples pixel() would take (span_cycles counts those it skips
    /// too, so that its counts match): not a measure of the work done.
    pub(crate) three_texel_samples: u64,
    pub(crate) rgba16_samples: u64,
    pub(crate) skipped_color_reads: u64,
    pub(crate) pixels: u64,
    pub(crate) spans: u64,
    pub(crate) fast_span_pixels: u64,
    pub(crate) start: u32,
    pub(crate) end: u32,
    pub(crate) current: u32,
    pub(crate) status: u32,
    /// A command cut by the end of the list, waiting for its rest (the
    /// longest, a triangle with everything, is 22 words).
    pending: [u64; 22],
    pending_len: usize,
    /// Saved as a record, not in the list below.
    pub(crate) timing: Timing,

    pub other_modes: u64,
    pub combine: u64,
    pub fill_color: u32,
    pub fog: u32,
    pub blend: u32,
    pub prim: u32,
    pub env: u32,
    pub prim_lod_frac: u32,
    pub prim_z: u32,
    pub prim_dz: u32,
    /// The chroma key (Set Key R/GB: center and scale of each channel)
    /// and the YUV conversion's constants (Set Convert).
    pub key: [u32; 2],
    pub convert: [i32; 6],
    /// The scissor box (10.2) and its interlace mode.
    pub scissor: [u32; 4],
    pub scissor_field: u32,
    pub texture_image: Image,
    pub color_image: Image,
    pub z_image: u32,
    pub tiles: [Tile; 8],
    pub tmem: Vec<u8>,
}

state!(Rdp { start, end, current, status, pending, pending_len, other_modes, combine, fill_color, fog, blend, prim, env,
    prim_lod_frac, prim_z, prim_dz, key, convert, scissor, scissor_field, texture_image, color_image, z_image, tiles,
    tmem });

impl Rdp {
    pub fn new() -> Rdp {
        Rdp {
            accelerated: true,
            rgba16_sampling: true,
            lazy_color_reads: true,
            fast_combiner: true,
            fast_spans: true,
            combine_program: std::array::from_fn(|cycle| crate::raster::classify_combiner(0, cycle)),
            profiling: false,
            three_texel_samples: 0,
            rgba16_samples: 0,
            skipped_color_reads: 0,
            pixels: 0,
            spans: 0,
            fast_span_pixels: 0,
            start: 0,
            end: 0,
            current: 0,
            status: CBUF_READY,
            pending: [0; 22],
            pending_len: 0,
            timing: Timing::IDLE,
            other_modes: 0,
            combine: 0,
            fill_color: 0,
            fog: 0,
            blend: 0,
            prim: 0,
            env: 0,
            prim_lod_frac: 0,
            prim_z: 0,
            prim_dz: 0,
            key: [0; 2],
            convert: [0; 6],
            scissor: [0; 4],
            scissor_field: 0,
            texture_image: Image::default(),
            color_image: Image::default(),
            z_image: 0,
            tiles: [Tile::default(); 8],
            tmem: vec![0; 0x1000],
        }
    }

    pub(crate) fn set_combine(&mut self, mode: u64) {
        self.combine = mode;
        self.rebuild_combiner();
    }

    pub(crate) fn rebuild_combiner(&mut self) {
        self.combine_program = std::array::from_fn(|cycle| crate::raster::classify_combiner(self.combine, cycle));
    }

    /// The other modes' cycle type: 1-cycle, 2-cycle, copy or fill.
    /// A command that only sets state: applied, and true; false for the
    /// others (draws, loads, syncs). The drawing thread's copy of the state
    /// follows the machine's through this alone.
    pub(crate) fn set_state(&mut self, w: u64) -> bool {
        match w >> 56 & 0x3f {
            0x2a => self.key[1] = w as u32,
            0x2b => self.key[0] = w as u32,
            0x2c => {
                // Set Convert: six 9-bit signed constants.
                for (i, k) in self.convert.iter_mut().enumerate() {
                    let value = (w >> (45 - 9 * i) & 0x1ff) as i32;
                    *k = (value << 23) >> 23;
                }
            }
            0x2d => {
                self.scissor = [field(w, 44), field(w, 32), field(w, 12), field(w, 0)];
                self.scissor_field = (w >> 24 & 3) as u32;
            }
            0x2e => {
                self.prim_z = (w >> 16 & 0xffff) as u32;
                self.prim_dz = (w & 0xffff) as u32;
            }
            0x2f => self.other_modes = w & 0x00ff_ffff_ffff_ffff,
            0x32 => {
                let tile = &mut self.tiles[(w >> 24 & 7) as usize];
                (tile.lo, tile.hi) = ([field(w, 44), field(w, 32)], [field(w, 12), field(w, 0)]);
            }
            0x35 => {
                let tile = &mut self.tiles[(w >> 24 & 7) as usize];
                tile.format = (w >> 53 & 7) as u32;
                tile.size = (w >> 51 & 3) as u32;
                tile.line = (w >> 41 & 0x1ff) as u32 * 8;
                tile.tmem = (w >> 32 & 0x1ff) as u32 * 8;
                tile.palette = (w >> 20 & 15) as u32;
                for (axis, shift) in [(1usize, 10u32), (0, 0)] {
                    tile.clamp[axis] = w >> (shift + 9) & 1 != 0;
                    tile.mirror[axis] = w >> (shift + 8) & 1 != 0;
                    tile.mask[axis] = (w >> (shift + 4) & 15) as u32;
                    tile.shift[axis] = (w >> shift & 15) as u32;
                }
            }
            0x37 => self.fill_color = w as u32,
            0x38 => self.fog = w as u32,
            0x39 => self.blend = w as u32,
            0x3a => {
                self.prim = w as u32;
                self.prim_lod_frac = (w >> 32 & 0xff) as u32;
            }
            0x3b => self.env = w as u32,
            0x3c => self.set_combine(w & 0x00ff_ffff_ffff_ffff),
            0x3d => self.texture_image = image(w),
            0x3e => self.z_image = w as u32 & 0xff_ffff,
            0x3f => self.color_image = image(w),
            _ => return false,
        }
        true
    }

    pub fn cycle_type(&self) -> u32 {
        (self.other_modes >> 52 & 3) as u32
    }

    /// The clocks a command takes the RDP in the modes set before it; 0 for
    /// those that only set state (and Load TLUT, as gopher64 counts it).
    /// Not inlined, as the timing's other functions: inlined into the
    /// command and event loops they cost Paper Mario's machine thread 6% on
    /// the Zero 2 W, for the code around them, not their own work.
    #[inline(never)]
    fn clocks(&self, words: &[u64]) -> u64 {
        let w = words[0];
        match w >> 56 & 0x3f {
            0x08..=0x0f => SETUP_CLOCKS + self.pixel_clocks(self.triangle_pixels(words)),
            0x24 | 0x25 => {
                let (pixels, lines) = self.rectangle(w);
                SETUP_CLOCKS + self.pixel_clocks(pixels) + lines * LINE_CLOCKS
            }
            0x36 => SETUP_CLOCKS + self.pixel_clocks(self.rectangle(w).0),
            0x26..=0x29 => SYNC_CLOCKS,
            op @ (0x33 | 0x34) => {
                let (sl, tl, sh, th) = (field(w, 44), field(w, 32), field(w, 12), field(w, 0));
                // Load Block's texels are a run from SL to SH (its TH is
                // DXT), fetched at once; Load Tile's a rectangle in 10.2,
                // fetched line by line.
                let span = |lo: u32, hi: u32| if hi >= lo { u64::from((hi - lo) >> 2) + 1 } else { 0 };
                let (texels, lines) = if op == 0x33 {
                    (if sh >= sl { u64::from(sh - sl) + 1 } else { 0 }, 1)
                } else {
                    (span(sl, sh) * span(tl, th), span(tl, th))
                };
                let bits = texels * (4 << self.texture_image.size);
                SETUP_CLOCKS + bits.div_ceil(64) + lines * LINE_CLOCKS
            }
            _ => 0,
        }
    }

    /// The clocks so many pixels take in the cycle type set.
    fn pixel_clocks(&self, pixels: u64) -> u64 {
        match self.cycle_type() {
            0 => pixels,
            1 => pixels * 2,
            _ => (pixels * (4 << self.color_image.size)).div_ceil(64),
        }
    }

    /// About how many pixels a triangle covers inside the scissor: its
    /// width at the middle vertex times the lines it spans, halved.
    fn triangle_pixels(&self, words: &[u64]) -> u64 {
        // YL, YM and YH are s11.2; XL and XH s15.16, and DxHDy the major
        // edge's slope a line.
        let y = |shift: u32| ((words[0] >> shift) as i32) << 18 >> 18;
        let (yl, ym, yh) = (y(32), y(16), y(0));
        let (xl, xh, dxhdy) = ((words[1] >> 32) as i32, (words[2] >> 32) as i32, words[2] as i32);
        let major = i64::from(xh) + i64::from(dxhdy) * i64::from(ym - yh) / 4;
        let [left, top, right, bottom] = self.scissor.map(|v| (v >> 2) as i32);
        let width = ((major - i64::from(xl)).abs() >> 16).min(i64::from((right - left).max(0))) as u64;
        width * self.scissor_lines((yh >> 2).max(top), (yl >> 2).min(bottom)) / 2
    }

    /// A rectangle's (fill or textured) pixels and lines inside the
    /// scissor; fill and copy modes draw its last column and line too.
    fn rectangle(&self, w: u64) -> (u64, u64) {
        let (mut xl, mut yl) = (field(w, 44) as i32, field(w, 32) as i32);
        let (xh, yh) = (field(w, 12) as i32, field(w, 0) as i32);
        if self.cycle_type() >= 2 {
            (xl, yl) = (xl + 4, yl + 4);
        }
        let [left, top, right, bottom] = self.scissor.map(|v| (v >> 2) as i32);
        let lines = self.scissor_lines((yh >> 2).max(top), (yl >> 2).min(bottom));
        let (from, to) = ((xh >> 2).max(left), (xl >> 2).min(right));
        (if to > from { (to - from) as u64 * lines } else { 0 }, lines)
    }

    /// The lines from `top` to before `bottom` that are drawn: in the
    /// scissor's interlaced mode only the odd or the even ones.
    fn scissor_lines(&self, top: i32, bottom: i32) -> u64 {
        if bottom <= top {
            return 0;
        }
        if self.scissor_field & 2 == 0 {
            return (bottom - top) as u64;
        }
        let odd = (self.scissor_field & 1) as i32;
        let below = |y: i32| (y - odd + 1) >> 1;
        (below(bottom) - below(top)) as u64
    }
}

/// How many 64-bit words a command takes, by its first word.
fn length(word: u64) -> usize {
    match word >> 56 & 0x3f {
        // Triangles: the edges, then shade, texture and depth if set.
        command @ 0x08..=0x0f => {
            4 + if command & 4 != 0 { 8 } else { 0 } + if command & 2 != 0 { 8 } else { 0 }
                + if command & 1 != 0 { 2 } else { 0 }
        }
        0x24 | 0x25 => 2,
        _ => 1,
    }
}

/// A 10.2 coordinate field of a command.
fn field(word: u64, shift: u32) -> u32 {
    (word >> shift & 0xfff) as u32
}

impl N64 {
    pub(crate) fn read_dp(&mut self, addr: u32) -> u32 {
        let rdp = &self.rdp;
        match (addr >> 2) & 7 {
            0 => rdp.start,
            1 => rdp.end,
            2 => rdp.current,
            3 => rdp.status,
            _ => 0,
        }
    }

    /// Whether the CPU has the RDP frozen (DPC_STATUS).
    pub(crate) fn rdp_frozen(&self) -> bool {
        self.rdp.status & FREEZE != 0
    }

    pub(crate) fn write_dp(&mut self, addr: u32, value: u32) {
        match (addr >> 2) & 7 {
            0 => {
                // The start waits for the next end; until then another
                // start is ignored (n64-systemtest's start-valid).
                if self.rdp.status & START_PENDING == 0 {
                    self.rdp.start = value & 0xff_fff8;
                    self.rdp.status |= START_PENDING;
                }
            }
            1 => {
                self.rdp.end = value & 0xff_fff8;
                if self.rdp.status & START_PENDING != 0 {
                    self.rdp.current = self.rdp.start;
                    self.rdp.status &= !START_PENDING;
                }
                self.run_rdp();
            }
            3 => {
                let mut flag = |clear: u32, set: u32, bit: u32| {
                    if value & clear != 0 && value & set == 0 {
                        self.rdp.status &= !bit;
                    }
                    if value & set != 0 && value & clear == 0 {
                        self.rdp.status |= bit;
                    }
                };
                flag(1 << 0, 1 << 1, XBUS);
                flag(1 << 2, 1 << 3, FREEZE);
                flag(1 << 4, 1 << 5, FLUSH);
                if self.rdp.status & FREEZE == 0 {
                    self.run_rdp();
                }
            }
            _ => {}
        }
    }

    /// Fetches the commands from DPC_CURRENT to DPC_END, from RDRAM or DMEM,
    /// and runs each that is whole.
    fn run_rdp(&mut self) {
        if self.rdp.status & (FREEZE | END_PENDING) != 0 {
            return;
        }
        // Started, even on an empty list, the RDP's clock runs and its
        // pipeline counts as busy until a SYNC_FULL is done.
        self.rdp.status |= START_GCLK | PIPE_BUSY;
        self.rdp.timing.rest_at = u64::MAX;
        // What graphics HLE made while the RDP was frozen comes first.
        for words in std::mem::take(&mut self.hle.held) {
            self.rdp_command(&words);
        }
        self.run_rdp_commands();
        if self.rdp_thread.serial {
            self.rdp_thread.sync();
        } else {
            self.rdp_thread.flush_some();
        }
    }

    fn run_rdp_commands(&mut self) {
        while self.rdp.current < self.rdp.end {
            let at = self.rdp.current as usize;
            let word = if self.rdp.status & XBUS != 0 {
                let i = at & 0xff8;
                u64::from_be_bytes(self.rsp.memory[i..i + 8].try_into().unwrap())
            } else if at + 8 <= self.rdram.len() {
                self.touch_rdram(at, 8);
                u64::from_be_bytes(self.rdram[at..at + 8].try_into().unwrap())
            } else {
                0
            };
            self.rdp.current += 8;
            let rdp = &mut self.rdp;
            rdp.pending[rdp.pending_len] = word;
            rdp.pending_len += 1;
            if rdp.pending_len == length(rdp.pending[0]) {
                let (command, len) = (rdp.pending, rdp.pending_len);
                rdp.pending_len = 0;
                self.rdp_command(&command[..len]);
            }
        }
    }

    pub(crate) fn rdp_command(&mut self, words: &[u64]) {
        if self.hle.capturing {
            self.hle.captured.push(words.to_vec());
            if self.hle.capture_only {
                return;
            }
        }
        let clocks = self.rdp.clocks(words);
        if clocks != 0 {
            self.rdp_works(clocks);
        }
        let w = words[0];
        match w >> 56 & 0x3f {
            0x08..=0x0f | 0x24 | 0x25 | 0x36 => {
                if self.rdp_thread.enabled {
                    let rdram = self.rdram.as_mut_ptr();
                    self.rdp_thread.draw(&self.rdp, rdram, words, self.cycles);
                } else if self.rdp_thread.check_marks {
                    self.draw_checking_marks(words);
                } else {
                    self.raster().draw(words);
                }
            }
            // SYNC_FULL: once everything before it is done, the RDP rests
            // and interrupts.
            0x29 => self.full_sync(),
            op @ (0x30 | 0x33 | 0x34) => {
                // Loads read RDRAM that drawing may still be writing.
                let (from, len) = self.load_source(w);
                self.touch_rdram(from, len);
                let written = match op {
                    0x30 => self.load_tlut(w),
                    0x33 => self.load_block(w),
                    _ => self.load_tile(w),
                };
                self.rdp_thread.loaded(written);
                // TLUT, Block and Tile loads set the tile's size too, as
                // SET_TILE_SIZE (with the same fields) would.
                self.rdp_thread.state(w & !(0x3f << 56) | 0x32 << 56);
            }
            _ => {
                if self.rdp.set_state(w) {
                    self.rdp_thread.state(w);
                }
            }
        }
    }

    /// The RDP is given work of so many of its clocks: it starts once it is
    /// through with what it had, and shows itself busy until it is through.
    #[inline(never)]
    fn rdp_works(&mut self, clocks: u64) {
        let timing = &mut self.rdp.timing;
        timing.busy_until = timing.busy_until.max(self.cycles) + clocks * 3 / 2;
        timing.rest_at = u64::MAX;
        self.rdp.status |= START_GCLK | PIPE_BUSY | CMD_BUSY;
        // An event already due earlier looks again then.
        if timing.event > timing.busy_until {
            let at = timing.busy_until;
            self.schedule_rdp(at);
        }
    }

    /// A SYNC_FULL (its own clocks counted): the RDP rests when it is done,
    /// and its interrupt comes then; one the RSP sent (`waits`) comes no
    /// sooner than the RSP stops (`rdp_rsp_stopped`). Interrupts come in
    /// order, so one that follows a waiting one waits too.
    #[inline(never)]
    pub(crate) fn rdp_full_sync(&mut self, waits: bool) {
        let timing = &mut self.rdp.timing;
        let done = timing.busy_until;
        timing.rest_at = done;
        let n = timing.pending();
        let waits = waits || timing.waiting != 0;
        if n < SYNCS {
            timing.syncs[n] = done;
            timing.waiting += u32::from(waits);
        } else {
            // Merged into the newest, which then waits if either did.
            timing.syncs[SYNCS - 1] = done;
            if waits && timing.waiting == 0 {
                timing.waiting = 1;
            }
        }
        let next = timing.next_interrupt().min(done);
        if next < timing.event {
            self.schedule_rdp(next);
        }
    }

    /// The RSP stopped: the interrupts that waited for it come when their
    /// SYNC_FULLs are done, at once if they are (after the SP interrupt,
    /// which BREAK and graphics HLE raise before this).
    #[inline(never)]
    pub(crate) fn rdp_rsp_stopped(&mut self) {
        if self.rdp.timing.waiting == 0 {
            return;
        }
        self.rdp.timing.waiting = 0;
        self.rdp_interrupts();
        let next = self.rdp.timing.next_interrupt();
        if next < self.rdp.timing.event {
            self.schedule_rdp(next);
        }
    }

    /// The DP interrupt, if a SYNC_FULL that does not wait for the RSP is
    /// done by now.
    fn rdp_interrupts(&mut self) {
        let now = self.cycles;
        let timing = &mut self.rdp.timing;
        let free = timing.pending().saturating_sub(timing.waiting as usize);
        let due = timing.syncs[..free].iter().take_while(|&&at| at <= now).count();
        if due != 0 {
            timing.syncs.copy_within(due.., 0);
            timing.syncs[SYNCS - due..].fill(u64::MAX);
            self.request(crate::DP_INTR);
        }
    }

    /// The RDP's event: it is through with its commands (CMD_BUSY clears),
    /// a SYNC_FULL is done (it rests: START_GCLK and PIPE_BUSY clear), or
    /// an interrupt is due.
    #[inline(never)]
    pub(crate) fn rdp_event(&mut self) {
        let now = self.cycles;
        let timing = &mut self.rdp.timing;
        if timing.busy_until <= now {
            self.rdp.status &= !CMD_BUSY;
        }
        if timing.rest_at <= now {
            timing.rest_at = u64::MAX;
            self.rdp.status &= !(START_GCLK | PIPE_BUSY);
        }
        self.rdp_interrupts();
        let timing = self.rdp.timing;
        let busy = if self.rdp.status & CMD_BUSY != 0 { timing.busy_until } else { u64::MAX };
        let next = busy.min(timing.rest_at).min(timing.next_interrupt());
        if next != u64::MAX {
            self.schedule_rdp(next);
        }
    }

    /// The bytes of RDRAM a load (TLUT, Block or Tile) reads: from where,
    /// and how many, as the loads below count them.
    fn load_source(&self, w: u64) -> (usize, usize) {
        let image = self.rdp.texture_image;
        let (sl, tl, sh, th) = (field(w, 44), field(w, 32), field(w, 12), field(w, 0));
        let texel_bits = 4 << image.size;
        let row_bytes = (image.width * texel_bits).div_ceil(8);
        let (from, len) = match w >> 56 & 0x3f {
            0x30 => {
                let count = ((sh >> 2) + 1).saturating_sub(sl >> 2);
                (image.addr.wrapping_add(((tl >> 2) * image.width + (sl >> 2)) * 2), count * 2)
            }
            0x33 => {
                let words = ((sh.saturating_sub(sl) + 1) * texel_bits).div_ceil(64);
                (image.addr.wrapping_add(tl * row_bytes + sl * texel_bits / 8), words * 8)
            }
            _ => {
                let (s0, t0, s1, t1) = (sl >> 2, tl >> 2, sh >> 2, th >> 2);
                let width_bytes = ((s1 + 1).saturating_sub(s0) * texel_bits).div_ceil(8);
                let rows = (t1 + 1).saturating_sub(t0);
                let from = image.addr.wrapping_add(t0 * row_bytes + s0 * texel_bits / 8);
                (from, rows.saturating_sub(1) * row_bytes + width_bytes)
            }
        };
        (from as usize, len as usize)
    }

    /// Draws on this thread, checking that only pages the draw marks change
    /// (`--check-rdp-marks`): what lets the drawing thread run ahead.
    fn draw_checking_marks(&mut self, words: &[u64]) {
        let reach = crate::rdp_thread::draw_reach(&self.rdp, words);
        let before = self.rdram.clone();
        self.raster().draw(words);
        let mut marks = crate::rdp_thread::RdpThread::new(false);
        for &(addr, len) in &reach {
            marks.mark(addr, len);
        }
        let piece = 1 << crate::rdp_thread::MARK;
        for (n, (a, b)) in before.chunks(piece).zip(self.rdram.chunks(piece)).enumerate() {
            assert!(a == b || marks.pending(n * piece, 1),
                "draw {:016x} changed piece {n:#x} outside {reach:x?}", words[0]);
        }
    }

    /// Load TLUT: palette entries into high TMEM, each 16-bit entry four
    /// times over (one copy for each of the four banks).
    fn load_tlut(&mut self, w: u64) -> std::ops::Range<usize> {
        let rdp = &mut self.rdp;
        let tile = (w >> 24 & 7) as usize;
        let (sl, tl, sh, th) = (field(w, 44), field(w, 32), field(w, 12), field(w, 0));
        rdp.tiles[tile].lo = [sl, tl];
        rdp.tiles[tile].hi = [sh, th];
        let image = rdp.texture_image;
        let count = ((sh >> 2) + 1).saturating_sub(sl >> 2);
        let base = image.addr + ((tl >> 2) * image.width + (sl >> 2)) * 2;
        let mut at = rdp.tiles[tile].tmem as usize;
        for i in 0..count {
            let addr = (base + i * 2) as usize & 0x7f_fffe;
            let entry = [self.rdram[addr], self.rdram[addr + 1]];
            for copy in 0..4 {
                let to = (at + copy * 2) & 0xffe;
                self.rdp.tmem[to..to + 2].copy_from_slice(&entry);
            }
            at += 8;
        }
        let first = self.rdp.tiles[tile].tmem as usize;
        written(first, count as usize * 8)
    }

    /// Load Block: a run of texels into TMEM, 64 bits at a time. dxt
    /// (1.11) is added up for each word, and the words of odd lines (where
    /// its integer part is odd) have their 32-bit halves swapped, as Load
    /// Tile lays lines out.
    fn load_block(&mut self, w: u64) -> std::ops::Range<usize> {
        let rdp = &mut self.rdp;
        let index = (w >> 24 & 7) as usize;
        let (sl, tl, sh, dxt) = (field(w, 44), field(w, 32), field(w, 12), field(w, 0));
        rdp.tiles[index].lo = [sl, tl];
        rdp.tiles[index].hi = [sh, dxt];
        let tile = rdp.tiles[index];
        let image = rdp.texture_image;
        // The texture image's size decides the size of the copy's texels.
        let texel_bits = 4 << image.size;
        let row_bytes = (image.width * texel_bits).div_ceil(8);
        let base = image.addr + tl * row_bytes + sl * texel_bits / 8;
        let words = ((sh.saturating_sub(sl) + 1) * texel_bits).div_ceil(64);
        let range = if tile.size == 3 { 0..0x1000 } else { written(tile.tmem as usize, words as usize * 8) };
        // Whole words, from RDRAM as it is into TMEM without wrapping.
        let (from, to) = (base as usize, tile.tmem as usize);
        if rdp.accelerated && tile.size != 3 && from + words as usize * 8 <= self.rdram.len().min(0x80_0000)
            && to + words as usize * 8 <= 0x1000
        {
            let mut t = 0u32;
            for i in 0..words as usize {
                let word = self.rdram[from + i * 8..from + i * 8 + 8].try_into().unwrap();
                store_word((&mut rdp.tmem[to + i * 8..to + i * 8 + 8]).try_into().unwrap(), word, t >> 11 & 1 != 0);
                t += dxt;
            }
            return range;
        }
        let mut t = 0u32;
        for i in 0..words {
            let odd = t >> 11 & 1 != 0;
            for k in 0..8 {
                let byte = self.rdram.get((base + i * 8 + k) as usize & 0x7f_ffff).copied().unwrap_or(0);
                store_texel_byte(&mut self.rdp.tmem, &tile, tile.tmem + i * 8, k, byte, odd);
            }
            t += dxt;
        }
        range
    }

    /// Load Tile: a rectangle of texels into TMEM, line by line at the
    /// tile's line length, odd lines with their 32-bit halves swapped.
    fn load_tile(&mut self, w: u64) -> std::ops::Range<usize> {
        let rdp = &mut self.rdp;
        let index = (w >> 24 & 7) as usize;
        let (sl, tl, sh, th) = (field(w, 44), field(w, 32), field(w, 12), field(w, 0));
        rdp.tiles[index].lo = [sl, tl];
        rdp.tiles[index].hi = [sh, th];
        let tile = rdp.tiles[index];
        let image = rdp.texture_image;
        let texel_bits = 4 << image.size;
        let row_bytes = (image.width * texel_bits).div_ceil(8);
        let (s0, t0, s1, t1) = (sl >> 2, tl >> 2, sh >> 2, th >> 2);
        let width_bytes = ((s1 + 1).saturating_sub(s0) * texel_bits).div_ceil(8);
        let rows = (t1 + 1).saturating_sub(t0) as usize;
        let reach = rows.saturating_sub(1) * tile.line as usize + (width_bytes as usize).next_multiple_of(8);
        let range = if tile.size == 3 { 0..0x1000 } else { written(tile.tmem as usize, reach) };
        for (row, t) in (t0..=t1).enumerate() {
            let from = image.addr + t * row_bytes + s0 * texel_bits / 8;
            let line = tile.tmem + row as u32 * tile.line;
            // A whole line, from RDRAM as it is into TMEM without wrapping.
            let (at, to, len) = (from as usize, line as usize, width_bytes as usize);
            if rdp.accelerated && tile.size != 3 && at + len <= self.rdram.len().min(0x80_0000)
                && to + len.next_multiple_of(8) <= 0x1000
            {
                let odd = row & 1 != 0;
                let source = &self.rdram[at..at + len];
                let mut words = source.chunks_exact(8);
                for (k, word) in words.by_ref().enumerate() {
                    store_word((&mut rdp.tmem[to + k * 8..to + k * 8 + 8]).try_into().unwrap(), word.try_into().unwrap(), odd);
                }
                let done = len / 8 * 8;
                for (b, &byte) in words.remainder().iter().enumerate() {
                    rdp.tmem[(to + done + b) ^ if odd { 4 } else { 0 }] = byte;
                }
                continue;
            }
            for b in 0..width_bytes {
                let byte = self.rdram.get((from + b) as usize & 0x7f_ffff).copied().unwrap_or(0);
                store_texel_byte(&mut rdp.tmem, &tile, line, b, byte, row & 1 != 0);
            }
        }
        range
    }
}

/// The bytes of TMEM a load wrote from `first` on, all of it if they wrap.
fn written(first: usize, len: usize) -> std::ops::Range<usize> {
    if first + len <= 0x1000 { first..first + len } else { 0..0x1000 }
}

/// Stores a word of texels (not 32-bit ones) into TMEM: as it is, or on
/// odd lines with its 32-bit halves swapped, as `store_texel_byte` does.
fn store_word(tmem: &mut [u8; 8], word: [u8; 8], odd: bool) {
    *tmem = if odd { u64::from_ne_bytes(word).rotate_left(32).to_ne_bytes() } else { word };
}

/// Stores the byte at `offset` of a texture line into TMEM at `line`: as
/// it is, or for 32-bit texels, red and green into low TMEM and blue and
/// alpha into high TMEM, two bytes a texel in each. Odd lines have their
/// 32-bit halves swapped (the address XOR 4), which sampling undoes.
fn store_texel_byte(tmem: &mut [u8], tile: &Tile, line: u32, offset: u32, byte: u8, odd: bool) {
    let swap = if odd { 4 } else { 0 };
    if tile.size == 3 {
        let (texel, part) = (offset / 4, offset % 4);
        let at = ((line + texel * 2 + (part & 1)) ^ swap) & 0x7ff;
        tmem[at as usize | if part >= 2 { 0x800 } else { 0 }] = byte;
    } else {
        tmem[((line + offset) ^ swap) as usize & 0xfff] = byte;
    }
}

fn image(w: u64) -> Image {
    Image {
        format: (w >> 53 & 7) as u32,
        size: (w >> 51 & 3) as u32,
        width: (w >> 32 & 0x3ff) as u32 + 1,
        addr: w as u32 & 0xff_ffff,
    }
}

#[cfg(test)]
mod tests {
    /// DPC_STATUS through a run, as n64-systemtest's "Flags during a run"
    /// reads it: the clock and pipeline from the run's start until a
    /// SYNC_FULL is done, CMD_BUSY while the RDP works, and the interrupt
    /// when the SYNC_FULL is done.
    #[test]
    fn status_follows_the_work() {
        let mut rom = vec![0u8; 0x1000];
        rom[..4].copy_from_slice(&[0x80, 0x37, 0x12, 0x40]);
        let mut n64 = crate::N64::new(&rom).unwrap();
        let commands: [u64; 8] = [
            0x3f << 56 | 2 << 51 | 7 << 32 | 0x2000,
            0x2d << 56 | 32 << 12 | 32,
            0x2f << 56 | 3 << 52,
            0x37 << 56 | 0x003e_003e,
            0x36 << 56 | 28 << 44 | 28 << 32,
            0x27 << 56,
            0x29 << 56,
            0x29 << 56,
        ];
        for (i, w) in commands.iter().enumerate() {
            n64.rdram[0x1000 + 8 * i..0x1008 + 8 * i].copy_from_slice(&w.to_be_bytes());
        }
        let advance = |n64: &mut crate::N64, by: u64| {
            n64.cycles += by;
            n64.run_events();
        };
        n64.write_dp(0, 0x1000);
        n64.write_dp(4, 0x1000);
        assert_eq!(n64.rdp.status, 0xa8, "started, nothing to do");
        n64.write_dp(4, 0x1008);
        assert_eq!(n64.rdp.status, 0xa8, "a command that sets state takes no time");
        // The rectangle: 16 clocks to set up, 64 16-bit pixels at 64 bits a
        // clock; SYNC_PIPE 50: 82 clocks, 123 of the CPU's cycles.
        n64.write_dp(4, 0x1030);
        assert_eq!(n64.rdp.status, 0xe8);
        advance(&mut n64, 122);
        assert_eq!(n64.rdp.status, 0xe8);
        advance(&mut n64, 1);
        assert_eq!(n64.rdp.status, 0xa8);
        assert_eq!(n64.mi_intr & crate::DP_INTR, 0);
        n64.write_dp(4, 0x1038);
        advance(&mut n64, 74);
        assert_eq!((n64.rdp.status, n64.mi_intr & crate::DP_INTR), (0xe8, 0));
        advance(&mut n64, 1);
        assert_eq!((n64.rdp.status, n64.mi_intr & crate::DP_INTR), (0x80, crate::DP_INTR), "SYNC_FULL done");
        n64.write_dp(4, 0x1040);
        advance(&mut n64, 75);
        assert_eq!(n64.rdp.status, 0x80);
    }

    /// TMEM loads a word at a time give what loads a byte at a time give,
    /// and say which bytes they changed.
    #[test]
    fn loads_by_words_match_loads_by_bytes() {
        let mut rom = vec![0u8; 0x1000];
        rom[..4].copy_from_slice(&[0x80, 0x37, 0x12, 0x40]);
        let mut fast = crate::N64::new(&rom).unwrap();
        for (i, byte) in fast.rdram.iter_mut().enumerate() {
            *byte = ((i as u32).wrapping_mul(2_654_435_761) >> 24) as u8;
        }
        let mut slow = fast.clone();
        slow.rdp.accelerated = false;
        let mut seed = 0x1234_5678_9abc_def0u64;
        let mut random = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..4000 {
            let (r, q) = (random(), random());
            // Some images run off the end of RDRAM.
            let addr = if r & 15 == 0 { 0x7f_f000 + (q as u32 & 0xff8) } else { q as u32 & 0x7f_fff8 };
            let image = 0x3du64 << 56 | (r >> 4 & 3) << 51 | (r >> 6 & 0x7f) << 32 | u64::from(addr);
            let tile = 0x35u64 << 56 | (r >> 13 & 3) << 51 | (r >> 15 & 0x3f) << 41 | (r >> 21 & 0x1ff) << 32
                | (r >> 30 & 7) << 24;
            let (sl, tl, sh, th) = (q >> 32 & 0xfff, q >> 44 & 0xfff, r >> 33 & 0xfff, r >> 45 & 0xfff);
            let load = match r >> 57 & 3 {
                0 => 0x30u64 << 56 | sl << 44 | tl << 32 | (r >> 30 & 7) << 24 | (sl + (sh & 0xff)) << 12 | th,
                1 => 0x33u64 << 56 | sl << 44 | tl << 32 | (r >> 30 & 7) << 24 | (sl + (sh & 0x7ff)).min(0xfff) << 12
                    | (q & 0xfff),
                _ => 0x34u64 << 56 | sl << 44 | tl << 32 | (r >> 30 & 7) << 24 | (sl + (sh & 0xff)).min(0xfff) << 12
                    | (tl + (th & 0x7f)).min(0xfff),
            };
            for n64 in [&mut fast, &mut slow] {
                n64.rdp.set_state(image);
                n64.rdp.set_state(tile);
            }
            let before = fast.rdp.tmem.clone();
            let changed = match load >> 56 & 0x3f {
                0x30 => (fast.load_tlut(load), slow.load_tlut(load)).0,
                0x33 => (fast.load_block(load), slow.load_block(load)).0,
                _ => (fast.load_tile(load), slow.load_tile(load)).0,
            };
            assert!(fast.rdp.tmem == slow.rdp.tmem, "load {load:016x} after {image:016x} {tile:016x}");
            for (i, (a, b)) in before.iter().zip(&fast.rdp.tmem).enumerate() {
                assert!(a == b || changed.contains(&i), "load {load:016x} changed {i:#x} outside {changed:x?}");
            }
        }
    }
}
