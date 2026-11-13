//! A Nintendo 64. The CPU (a NEC VR4300, 64-bit MIPS III, at 93.75 MHz)
//! is interpreted instruction by instruction; the rest of the console
//! moves at events, as in the GBA core: the video interface at the line
//! that interrupts and at the end of each field, the timer when Count
//! reaches Compare, each DMA when it is done, and the RSP (the RCP's signal
//! processor), a burst of its instructions at a time. The RDP (the RCP's
//! rasterizer) draws its commands as soon as it is given them, but shows
//! itself busy until, and interrupts when, it would have done them (an event
//! of its own, kept with its timing).
//!
//! The core does no I/O: a host gives it a cartridge image and the buttons
//! held each frame, and takes back the picture, the sound, the cartridge's
//! saves and, for Quick Resume, a save state. It starts as the PIF's boot code (IPL1
//! and IPL2, Nintendo's) leaves the console, about to run the cartridge's
//! own boot code (IPL3), so no boot ROM is needed.

mod ai;
mod boot;
mod bus;
mod cart;
mod cop0;
mod cpu;
mod decode;
mod fpu;
mod games;
mod audio_hle;
mod gfx_hle;
mod jit;
mod pi;
mod raster;
mod raster_neon;
mod rdp;
mod rdp_thread;
mod rsp;
mod save;
mod si;
mod vi;
mod video_hle;
mod vu;

use savestate::state;

pub use ai::SAMPLE_HZ;
pub use cart::{Cic, Error, Region};
pub use audio_hle::AudioReport as AudioHleReport;
pub use gfx_hle::Report as HleReport;
pub use video_hle::VideoReport as VideoHleReport;
pub use jit::Stats as JitStats;

/// The CPU's clock.
pub const CPU_HZ: u64 = 93_750_000;

// Buttons, as bits of what run_frame takes: the controller's own order, as
// it answers the PIF.
pub const A: u16 = 1 << 15;
pub const B: u16 = 1 << 14;
pub const Z: u16 = 1 << 13;
pub const START: u16 = 1 << 12;
pub const UP: u16 = 1 << 11;
pub const DOWN: u16 = 1 << 10;
pub const LEFT: u16 = 1 << 9;
pub const RIGHT: u16 = 1 << 8;
pub const L: u16 = 1 << 5;
pub const R: u16 = 1 << 4;
pub const C_UP: u16 = 1 << 3;
pub const C_DOWN: u16 = 1 << 2;
pub const C_LEFT: u16 = 1 << 1;
pub const C_RIGHT: u16 = 1 << 0;

// Interrupts of the MIPS interface (MI_INTR and MI_MASK), one for each
// part of the RCP.
const SP_INTR: u32 = 1 << 0;
const SI_INTR: u32 = 1 << 1;
const AI_INTR: u32 = 1 << 2;
const VI_INTR: u32 = 1 << 3;
const PI_INTR: u32 = 1 << 4;
const DP_INTR: u32 = 1 << 5;

// The events, by number: the video interface's next line of note, the
// timer (Count reaching Compare), the end of a DMA of the PI and the SI,
// the RSP's next burst of instructions, and the sound buffer playing out.
const VI: usize = 0;
const TIMER: usize = 1;
const PI_DMA: usize = 2;
const SI_DMA: usize = 3;
const RSP: usize = 4;
const AUDIO: usize = 5;
const EVENTS: usize = 6;

/// The CPU's registers, for tests and debugging.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Registers {
    pub gpr: [u64; 32],
    pub hi: u64,
    pub lo: u64,
    /// The address of the next instruction to run.
    pub pc: u64,
    pub status: u32,
    pub cause: u32,
    pub epc: u64,
}

/// Optional host profiling counters, excluded from save states.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RspStats {
    pub native_vector_instructions: u64,
    pub fast_vector_memory_instructions: u64,
    /// Instructions run as compiled code.
    pub compiled_instructions: u64,
}

/// Optional rasterizer counters, excluded from save states.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RdpStats {
    pub three_texel_samples: u64,
    pub rgba16_samples: u64,
    pub skipped_color_reads: u64,
    /// Pixels the spans covered (before depth and alpha tests), and spans.
    pub pixels: u64,
    pub spans: u64,
    /// Pixels drawn by the specialized one-cycle span.
    pub fast_span_pixels: u64,
}

#[derive(Clone)]
pub struct N64 {
    /// The RDP's drawing thread. First: a copy waits for it before RDRAM is
    /// copied, and it is joined before RDRAM is freed.
    rdp_thread: rdp_thread::RdpThread,
    cpu: cpu::Cpu,
    /// Host code and its cache are derived from RAM, never part of a save.
    jit: jit::Jit,
    cart: cart::Cart,
    /// RDRAM, 8 MB (as with the Expansion Pak), big-endian, and the
    /// registers of its four 2 MB modules and of the RDRAM interface.
    rdram: Vec<u8>,
    rdram_regs: [[u32; bus::RDRAM_REGS]; bus::RDRAM_MODULES],
    /// A module's current control is set so that it cannot answer, which
    /// the boot code's calibration looks for; reads then take a slow path.
    rdram_unreadable: bool,
    ri: [u32; 8],
    /// The MIPS interface: its mode, and the interrupts requested and let
    /// through to the CPU.
    mi_mode: u32,
    mi_intr: u32,
    mi_mask: u32,
    rsp: rsp::Rsp,
    /// The RSP's compiled code, like `jit`.
    rsp_jit: jit::RspJit,
    /// High-level emulation of graphics tasks (host preferences).
    hle: gfx_hle::Hle,
    audio: audio_hle::AudioHle,
    video: video_hle::VideoHle,
    rdp: rdp::Rdp,
    vi: vi::Vi,
    ai: ai::Ai,
    pi: pi::Pi,
    si: si::Si,
    /// The cartridge's EEPROM, SRAM or FlashRAM, if it has one.
    save: save::Save,
    /// The IS-Viewer, a development cartridge's port for text: its buffer,
    /// and what the game has printed through it.
    isviewer: Vec<u8>,
    isviewer_out: Vec<u8>,
    /// Each port's buttons held and where its stick is (-128 to 127 on each
    /// axis, up and right positive), for the PIF to answer the controller
    /// with; and how many ports have a controller, from the first.
    buttons: [u16; 4],
    stick: [(i8, i8); 4],
    controllers: usize,
    /// A Rumble Pak in each controller (the host's setting), and whether
    /// each one's motor runs (the host's to feel, not saved).
    rumble_paks: bool,
    motors: [bool; 4],

    /// CPU cycles since power on.
    cycles: u64,
    /// When each event is next due, and the soonest of them.
    events: [u64; EVENTS],
    next_event: u64,
}

// A save state is STATE_MAGIC, the format's version (a byte), the hash of
// the cartridge it belongs to, the fields of the `state!` lists and, from
// version 4, named records (savestate's `records!`).
//
// The `state!` lists are version 3's layout and stay as they are, so that
// every state saved since loads (the console's Quick Resume, the benchmark
// states). To add machine state: put the field in its struct, and a line in
// the list below with a name never used before, the field, and its value at
// power on, which a state without the record (version 3, or from a build
// before the line) gives it. The record holds the field's `State`: a
// number, an array, or a struct with a `state!` list of its own. A build
// skips records it does not know; STATE_VERSION stays 4.
state!(N64 { cpu, rdram, rdram_regs, rdram_unreadable, ri, mi_mode, mi_intr, mi_mask, rsp, rdp, vi, ai, pi, si,
    save, isviewer, cycles, events, next_event });

savestate::records!(N64 {
    "cpu.cop2": cpu.cop2 = 0,
    "pi.io_word": pi.io_word = 0,
    "pi.io_until": pi.io_until = 0,
    "rdp.timing": rdp.timing = rdp::Timing::IDLE,
});

const STATE_MAGIC: &[u8] = b"SOLN64";
const STATE_VERSION: u8 = 4;
/// States before the records, which load with every record's default.
const STATE_VERSION_3: u8 = 3;

impl N64 {
    /// The cycles to the next event, as many as a word holds: what blocks
    /// linked to are charged against (context word LEFT), from where the
    /// cycles were last counted.
    pub(crate) fn cycles_left(&self) -> u32 {
        self.next_event.saturating_sub(self.cycles).min(u64::from(u32::MAX)) as u32
    }
    /// Inserts a cartridge (in any of the three byte orders) and powers on,
    /// as the PIF's boot code leaves things for the cartridge's.
    pub fn new(rom: &[u8]) -> Result<N64, Error> {
        let cart = cart::Cart::parse(rom)?;
        let mut n64 = N64 {
            rdp_thread: rdp_thread::RdpThread::new(true),
            cpu: cpu::Cpu::new(),
            jit: jit::Jit::default(),
            rdram: vec![0; bus::RDRAM_SIZE],
            rdram_regs: [[0; bus::RDRAM_REGS]; bus::RDRAM_MODULES],
            rdram_unreadable: false,
            ri: [0; 8],
            mi_mode: 0,
            mi_intr: 0,
            mi_mask: 0,
            rsp: rsp::Rsp::new(),
            rsp_jit: jit::RspJit::new(true),
            hle: gfx_hle::Hle::default(),
            audio: audio_hle::AudioHle::default(),
            video: video_hle::VideoHle::default(),
            rdp: rdp::Rdp::new(),
            vi: vi::Vi::new(),
            ai: ai::Ai::default(),
            pi: pi::Pi::default(),
            si: si::Si::new(),
            save: save::Save::new(games::save_kind(&cart.rom)),
            isviewer: vec![0; bus::ISVIEWER_SIZE],
            isviewer_out: Vec::new(),
            buttons: [0; 4],
            stick: [(0, 0); 4],
            controllers: 1,
            rumble_paks: false,
            motors: [false; 4],
            cycles: 0,
            events: [u64::MAX; EVENTS],
            next_event: u64::MAX,
            cart,
        };
        n64.power_on();
        Ok(n64)
    }

    /// Runs until the video interface finishes a field (1/60 s on NTSC,
    /// 1/50 on PAL), with the buttons held.
    pub fn run_frame(&mut self, buttons: u16) {
        self.buttons[0] = buttons;
        self.ai.samples.clear();
        let field = self.vi.fields;
        while self.vi.fields == field {
            self.run_cpu_until_event();
            self.run_events();
        }
        let now = self.cycles;
        self.mix_audio(now);
        // The drawing thread can go on while the host shows the frame;
        // its latest picture is this frame's.
        self.rdp_thread.flush();
        if let Some((width, height)) = self.rdp_thread.latest_picture(&mut self.vi.picture) {
            (self.vi.picture_width, self.vi.picture_height) = (width, height);
        }
    }

    /// Waits for the RDP's drawing thread to take each field's picture, so
    /// that frame() is the picture of the field just run (for comparisons);
    /// otherwise it can be a field or two behind.
    pub fn set_rdp_exact_pictures(&mut self, exact: bool) {
        self.rdp_thread.exact = exact;
    }

    /// Moves the controller's stick, from -128 to 127 on each axis (up and
    /// right positive; a real stick reaches about 80).
    pub fn set_stick(&mut self, x: i8, y: i8) {
        self.stick[0] = (x, y);
    }

    /// Plugs in controllers in the first `n` ports (1 to 4); the others
    /// answer as empty, as the console's do with nothing in them.
    pub fn set_controllers(&mut self, n: usize) {
        self.controllers = n.clamp(1, 4);
    }

    /// Puts a Rumble Pak in each controller, or takes them out.
    pub fn set_rumble_paks(&mut self, on: bool) {
        self.rumble_paks = on;
        if !on {
            self.motors = [false; 4];
        }
    }

    /// Whether the Rumble Pak in a port's controller runs its motor.
    pub fn motor(&self, port: usize) -> bool {
        self.motors.get(port).copied().unwrap_or(false)
    }

    /// Another port's controller (1 to 3): its buttons held and its stick,
    /// as run_frame and set_stick the first's.
    pub fn set_controller(&mut self, port: usize, buttons: u16, (x, y): (i8, i8)) {
        if let Some(b) = self.buttons.get_mut(port) {
            *b = buttons;
            self.stick[port] = (x, y);
        }
    }

    /// Selects the native ARM CPU compiler (32- or 64-bit). The interpreter
    /// is the default while native coverage/performance is developed;
    /// other hosts use it either way. Clears compiled code and counters.
    /// Debugging: compares every compiled block's source on each entry, as
    /// before, and stops if one changed though nothing wrote its page.
    pub fn set_jit_source_checks(&mut self, on: bool) {
        self.jit.checked_sources = on;
    }

    pub fn set_jit(&mut self, enabled: bool) {
        self.jit.enabled = enabled && cfg!(any(target_arch = "arm", target_arch = "aarch64"));
        self.jit.flush();
        self.jit.stats = JitStats::default();
        self.jit.interpreted_ops.clear();
    }

    /// Native blocks compiled/executed, for profiling and conformance checks.
    pub fn jit_stats(&self) -> JitStats {
        self.jit.stats
    }

    /// Interpreted opcodes breakdown, for profiling dynarec bottlenecks.
    pub fn interpreted_opcodes(&self) -> Vec<(crate::decode::Op, u64)> {
        let mut ops: Vec<_> = self.jit.interpreted_ops.iter().map(|(&op, &count)| (op, count)).collect();
        ops.sort_by_key(|&(_, count)| std::cmp::Reverse(count));
        ops
    }

    /// Interpreted MFC0 destination registers (RD).
    pub fn interpreted_mfc0(&self) -> Vec<(usize, u64)> {
        let mut list: Vec<_> = self.jit.interpreted_mfc0_rd.iter().enumerate().filter(|&(_, &c)| c > 0).map(|(r, &c)| (r, c)).collect();
        list.sort_by_key(|&(_, c)| std::cmp::Reverse(c));
        list
    }

    /// The compiled CPU blocks: host address of the code, RDRAM address of
    /// the guest code, guest instructions (empty without the dynarec).
    pub fn jit_code_map(&self) -> Vec<(usize, u32, usize)> {
        #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
        return self.jit.code_map();
        #[cfg(not(any(target_arch = "arm", target_arch = "aarch64")))]
        Vec::new()
    }

    /// Collects native CPU profiling counters. Disabled during ordinary
    /// play so 64-bit counter updates do not cost ARM work on every block.
    pub fn set_jit_profiling(&mut self, enabled: bool) {
        self.jit.profiling = enabled;
        self.jit.stats = JitStats::default();
    }

    /// Chooses RSP memory fast paths and vector arithmetic supported by
    /// the host. Byte loops and scalar arithmetic remain the reference.
    pub fn set_rsp_acceleration(&mut self, enabled: bool) {
        self.rsp.fast_memory = enabled;
        self.set_rsp_vector_acceleration(enabled);
        self.set_rsp_jit(enabled);
    }

    /// Independently chooses the RSP's compiled code (on ARM; elsewhere
    /// the interpreter runs either way), keeping the other paths.
    pub fn set_rsp_jit(&mut self, enabled: bool) {
        self.rsp_jit = jit::RspJit::new(enabled);
    }

    /// Independently chooses vector arithmetic supported by the host,
    /// keeping memory acceleration unchanged for conformance/profiling.
    pub fn set_rsp_vector_acceleration(&mut self, enabled: bool) {
        #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
        {
            self.rsp.vu.accelerated = enabled && vu::neon_available();
        }
        #[cfg(not(any(target_arch = "arm", target_arch = "aarch64")))]
        let _ = enabled;
    }

    /// Independently chooses memory fast paths for profiling their cost.
    pub fn set_rsp_memory_acceleration(&mut self, enabled: bool) {
        self.rsp.fast_memory = enabled;
    }

    /// Collects RSP fast-path counts without adding counter updates
    /// to ordinary play. Enabling/disabling starts a new measurement.
    pub fn set_rsp_profiling(&mut self, enabled: bool) {
        self.rsp.profiling = enabled;
        self.rsp.fast_memory_count = 0;
        self.rsp_jit.instructions = 0;
        #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
        {
            self.rsp.vu.profiling = enabled;
            self.rsp.vu.native_count = 0;
        }
    }

    /// The RSP's cycles since power on (none pass while it is halted), to
    /// see how busy a scene keeps it.
    pub fn rsp_cycles(&self) -> u64 {
        self.rsp.clock
    }

    /// Runs graphics tasks of the microcodes it knows (F3D) here, at once,
    /// instead of on the RSP: much faster, close to but not exactly what the
    /// microcode draws. The RSP runs the rest.
    pub fn set_gfx_hle(&mut self, enabled: bool) {
        self.hle.enabled = enabled;
    }

    /// Runs graphics tasks on the RSP and compares each one's RDP commands
    /// with what the high-level emulation makes of it (gfx_hle_report).
    pub fn set_gfx_hle_compare(&mut self, enabled: bool) {
        self.hle.compare = enabled;
    }

    pub fn gfx_hle_report(&mut self) -> HleReport {
        self.finish_comparison();
        self.hle.report.clone()
    }

    /// Runs Resident Evil 2's video tasks on the RSP and compares each with
    /// what the HLE makes of it (video_hle_report). With the graphics HLE
    /// on, the HLE runs them otherwise.
    pub fn set_video_hle_compare(&mut self, enabled: bool) {
        self.video.compare = enabled;
    }

    pub fn video_hle_report(&self) -> VideoHleReport {
        self.video.report.clone()
    }

    /// Runs the RSP's sound tasks whose microcode this knows (ABI1's: Super
    /// Mario 64's, GoldenEye's; EAD's in Mario Kart 64 and Ocarina of Time;
    /// n_aspMain's: Paper Mario's, Donkey Kong 64's) on the CPU instead of
    /// the RSP; others still run on the RSP.
    pub fn set_audio_hle(&mut self, enabled: bool) {
        self.audio.enabled = enabled;
    }

    /// Runs audio tasks on the RSP and compares what each wrote to RDRAM
    /// with what the high-level emulation makes of it (audio_hle_report).
    pub fn set_audio_hle_compare(&mut self, enabled: bool) {
        self.audio.compare = enabled;
    }

    pub fn audio_hle_report(&self) -> AudioHleReport {
        self.audio.report.clone()
    }

    /// The RSP's cycles by the kind of task it ran: none known, graphics,
    /// sound, video, JPEG.
    pub fn rsp_task_cycles(&self) -> [u64; 5] {
        self.rsp.task_cycles
    }

    /// The RSP JIT's blocks whose words were compared again with IMEM,
    /// and blocks compiled.
    pub fn rsp_jit_counts(&self) -> (u64, u64) {
        (self.rsp_jit.comparisons, self.rsp_jit.blocks_compiled)
    }

    pub fn rsp_stats(&self) -> RspStats {
        #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
        let native_vector_instructions = self.rsp.vu.native_count;
        #[cfg(not(any(target_arch = "arm", target_arch = "aarch64")))]
        let native_vector_instructions = 0;
        RspStats {
            native_vector_instructions,
            fast_vector_memory_instructions: self.rsp.fast_memory_count,
            compiled_instructions: self.rsp_jit.instructions,
        }
    }

    /// Chooses optimized drawing with the original rasterizer available
    /// as a conformance reference. This does not change the RDP's state.
    pub fn set_rdp_acceleration(&mut self, enabled: bool) {
        self.rdp_thread.stop();
        self.rdp.accelerated = enabled;
    }

    /// Chooses direct RGBA16 decoding while RDP acceleration is enabled.
    pub fn set_rdp_rgba16_sampling(&mut self, enabled: bool) {
        self.rdp_thread.stop();
        self.rdp.rgba16_sampling = enabled;
    }

    /// Chooses cached combiner formulas while RDP acceleration is enabled.
    pub fn set_rdp_combiner_acceleration(&mut self, enabled: bool) {
        self.rdp_thread.stop();
        self.rdp.fast_combiner = enabled;
    }

    /// Chooses the specialized one-cycle span while RDP acceleration is
    /// enabled; pixel() draws the rest either way.
    pub fn set_rdp_span_acceleration(&mut self, enabled: bool) {
        self.rdp_thread.stop();
        self.rdp.fast_spans = enabled;
    }

    /// Independently skips framebuffer reads unused by the blender.
    /// The master RDP acceleration preference still gates this path.
    pub fn set_rdp_memory_acceleration(&mut self, enabled: bool) {
        self.rdp_thread.stop();
        self.rdp.lazy_color_reads = enabled;
    }

    pub fn set_rdp_profiling(&mut self, enabled: bool) {
        self.rdp_thread.stop();
        self.rdp.profiling = enabled;
        self.rdp.three_texel_samples = 0;
        self.rdp.rgba16_samples = 0;
        self.rdp.skipped_color_reads = 0;
        self.rdp.pixels = 0;
        self.rdp.spans = 0;
        self.rdp.fast_span_pixels = 0;
    }

    pub fn rdp_stats(&self) -> RdpStats {
        let mut rdp = self.rdp.clone();
        self.rdp_thread.add_counters(&mut rdp);
        RdpStats {
            three_texel_samples: rdp.three_texel_samples,
            rgba16_samples: rdp.rgba16_samples,
            skipped_color_reads: rdp.skipped_color_reads,
            pixels: rdp.pixels,
            spans: rdp.spans,
            fast_span_pixels: rdp.fast_span_pixels,
        }
    }

    /// Chooses drawing on a thread of its own (the default) or on the
    /// machine's; pictures and states are the same either way.
    pub fn set_rdp_thread(&mut self, enabled: bool) {
        self.rdp_thread.stop();
        self.rdp_thread.enabled = enabled;
    }

    /// How many threads draw (each every n-th line): by default all the
    /// cores but the machine's, at most three.
    pub fn set_rdp_drawers(&mut self, drawers: usize) {
        self.rdp_thread.stop();
        self.rdp_thread.drawers = drawers.max(1);
    }

    /// Runs one instruction of the CPU (and the events due before it), for
    /// tests and debugging.
    pub fn step(&mut self) {
        if self.cycles >= self.next_event {
            self.run_events();
        }
        self.step_cpu();
    }

    /// The last field the video interface showed, as XRGB8888 pixels (red
    /// in bits 16-23), its width and height: the part of the frame buffer
    /// the video interface takes, unscaled. Empty while video is off.
    pub fn frame(&self) -> (&[u32], usize, usize) {
        (&self.vi.picture, self.vi.picture_width, self.vi.picture_height)
    }

    /// The sound of the last frame: stereo, left first, SAMPLE_HZ pairs a
    /// second.
    pub fn audio(&self) -> &[i16] {
        &self.ai.samples
    }

    /// What the cartridge's save memory holds (EEPROM, SRAM or FlashRAM),
    /// if it has some, for the host to keep between sessions: as the chip
    /// holds it, byte for byte, as flashcarts and most emulators keep their
    /// save files.
    pub fn save_data(&self) -> Option<Vec<u8>> {
        (self.save.kind != save::Kind::None).then(|| self.save.memory.clone())
    }

    /// Puts back what the save memory held, before running.
    pub fn load_save_data(&mut self, data: &[u8]) {
        let n = data.len().min(self.save.memory.len());
        self.save.memory[..n].copy_from_slice(&data[..n]);
    }

    /// The whole machine as bytes (about 8.2 MB, most of it RDRAM), for
    /// Quick Resume.
    pub fn save_state(&self) -> Vec<u8> {
        let mut out = self.save_state_as(STATE_VERSION);
        self.save_records(&mut out);
        out
    }

    /// A state as version 3 saved it, without the records, for the test
    /// that such states still load.
    #[doc(hidden)]
    pub fn save_state_version_3(&self) -> Vec<u8> {
        self.save_state_as(STATE_VERSION_3)
    }

    fn save_state_as(&self, version: u8) -> Vec<u8> {
        self.rdp_thread.drain();
        use savestate::State;
        let mut out = Vec::with_capacity(bus::RDRAM_SIZE + 64 * 1024);
        out.extend_from_slice(STATE_MAGIC);
        out.push(version);
        self.cart.hash.save(&mut out);
        self.save(&mut out);
        out
    }

    /// Puts the machine back as a save state of this game has it. A state
    /// that does not load leaves the machine as it was.
    pub fn load_state(&mut self, data: &[u8]) -> Result<(), Error> {
        use savestate::{State, take};
        let mut input = data;
        if take(&mut input, STATE_MAGIC.len())? != STATE_MAGIC {
            return Err(Error::BadState);
        }
        let version = take(&mut input, 1)?[0];
        if version != STATE_VERSION && version != STATE_VERSION_3 {
            return Err(Error::BadState);
        }
        let mut hash = 0u64;
        hash.load(&mut input)?;
        if hash != self.cart.hash {
            return Err(Error::OtherGame);
        }
        let mut machine = self.clone();
        machine.load(&mut input)?;
        if version == STATE_VERSION_3 {
            machine.reset_records();
        } else {
            machine.load_records(&mut input)?;
        }
        if !input.is_empty() {
            return Err(Error::BadState);
        }
        machine.ai.samples.clear();
        machine.rdp.rebuild_combiner();
        machine.rdp.three_texel_samples = 0;
        machine.rdp.rgba16_samples = 0;
        machine.rdp.skipped_color_reads = 0;
        machine.rdp.pixels = 0;
        machine.rdp.spans = 0;
        machine.rdp.fast_span_pixels = 0;
        machine.rsp.fast_memory_count = 0;
        #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
        {
            machine.rsp.vu.native_count = 0;
        }
        *self = machine;
        Ok(())
    }

    pub fn registers(&self) -> Registers {
        self.cpu.registers()
    }

    /// The lockout chip the cartridge was made for, as its boot code shows.
    pub fn cic(&self) -> Cic {
        self.cart.cic
    }

    /// The television standard the cartridge is for (its header's country): PAL runs 50 fields a second,
    /// the others 60.
    pub fn region(&self) -> Region {
        self.cart.region
    }

    /// Where the game's code starts: where the header says, but for the
    /// boot code of CIC 6103 and 6106, which move it 1 or 2 MB lower.
    pub fn entry(&self) -> u32 {
        self.cart.entry()
    }

    /// What the game has printed through the IS-Viewer (as test ROMs and
    /// homebrew do).
    pub fn isviewer(&self) -> &[u8] {
        &self.isviewer_out
    }

    /// The byte at a physical address of RDRAM, SP memory, the video
    /// interface's registers, PIF RAM or the cartridge ROM, without the side
    /// effects of a read (other registers read as 0).
    pub fn peek(&self, addr: u32) -> u8 {
        self.rdp_thread.drain();
        self.peek_byte(addr)
    }

    /// A byte of RDRAM at a physical address (None past its 8 MB), read without the side effects of a read
    /// and without waiting for the RDP's drawing threads unless a draw not yet drawn may reach it: what
    /// reads a few hundred bytes after every frame (achievements) must not make the machine wait for each
    /// frame's drawing, which `peek` does. The CPU's reads wait the same way (`touch_rdram`).
    pub fn peek_rdram(&self, addr: u32) -> Option<u8> {
        let i = addr as usize;
        if i >= bus::RDRAM_SIZE {
            return None;
        }
        if self.rdp_thread.pending(i, 1) {
            self.rdp_thread.drain();
        }
        Some(self.rdram[i])
    }

    /// Before the machine touches RDRAM itself: waits for the RDP's
    /// drawing thread if a draw sent to it may still reach these bytes.
    #[inline]
    pub(crate) fn touch_rdram(&mut self, addr: usize, len: usize) {
        self.rdp_thread.touch(addr, len);
    }

    /// Before the machine writes RDRAM itself (touch_rdram is for reads):
    /// touch_rdram, and if code the CPU's JIT compiled may change, its
    /// blocks' sources are compared again.
    #[inline]
    pub(crate) fn wrote_rdram(&mut self, addr: usize, len: usize) {
        self.touch_rdram(addr, len);
        #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
        if self.jit.enabled {
            self.jit.wrote(&self.rdp_thread.code, &self.rdp_thread.code_chunks, addr, len);
        }
    }

    /// For native code that reads or writes RDRAM (the CPU's JIT): RDRAM's
    /// 1 KB pieces as the RDP's drawing thread marks them, by batch number,
    /// and the batches known to be drawn. Where pages[addr >> 10 & 8191] is
    /// above *known, the access must go through the interpreter's path
    /// (`touch_rdram`), which waits for the drawing. Both stay where they
    /// are while the machine does not move.
    #[cfg_attr(not(any(target_arch = "arm", target_arch = "aarch64")), allow(dead_code))]
    pub(crate) fn rdram_marks(&self) -> (*const u32, *const u32) {
        self.rdp_thread.marks()
    }

    /// Times the machine has waited for the RDP's drawing thread to draw,
    /// batches of commands sent to it, and sends that waited for room.
    pub fn rdp_thread_counts(&self) -> (u64, u64, u64) {
        (self.rdp_thread.waits, self.rdp_thread.batches.get(), self.rdp_thread.full.get())
    }

    /// Debugging: draws on the machine's thread, checking that each draw
    /// changes only the RDRAM pages the drawing thread would wait for.
    pub fn set_rdp_mark_check(&mut self, enabled: bool) {
        self.set_rdp_thread(!enabled && self.rdp_thread.enabled);
        self.rdp_thread.check_marks = enabled;
    }

    /// Sets when an event is next due.
    fn schedule(&mut self, event: usize, at: u64) {
        self.events[event] = at;
        self.next_event = self.soonest();
    }

    /// Sets when the RDP's event is next due: kept in its timing (a record
    /// of the state), as `events` is version 3's layout.
    fn schedule_rdp(&mut self, at: u64) {
        self.rdp.timing.event = at;
        self.next_event = self.soonest();
    }

    fn soonest(&self) -> u64 {
        self.events.iter().copied().fold(self.rdp.timing.event, u64::min)
    }

    /// Handles the events that are due, soonest first (the RDP's after the
    /// others due at the same cycle).
    fn run_events(&mut self) {
        while self.next_event <= self.cycles {
            let event = (0..EVENTS).min_by_key(|&e| self.events[e]).unwrap();
            let at = self.events[event];
            if self.rdp.timing.event < at {
                self.schedule_rdp(u64::MAX);
                self.rdp_event();
                continue;
            }
            self.schedule(event, u64::MAX);
            match event {
                VI => self.vi_event(at),
                TIMER => self.timer_event(at),
                PI_DMA => self.pi_dma_done(),
                SI_DMA => self.si_dma_done(),
                RSP => self.rsp_event(),
                _ => self.audio_event(at),
            }
        }
    }

    /// Requests interrupts of the MIPS interface, which reach the CPU as
    /// its interrupt line 2 where MI_MASK lets them.
    fn request(&mut self, intr: u32) {
        self.mi_intr |= intr;
        self.update_interrupt_line();
    }

    fn acknowledge(&mut self, intr: u32) {
        self.mi_intr &= !intr;
        self.update_interrupt_line();
    }

    fn update_interrupt_line(&mut self) {
        self.cpu.cop0.set_ip(cpu::IP2, self.mi_intr & self.mi_mask != 0);
    }
}
