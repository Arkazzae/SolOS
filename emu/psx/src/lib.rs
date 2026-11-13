//! A PlayStation (NTSC), emulated instruction by instruction: the R3000A
//! runs, and the rest of the console (the GPU's lines, the timers, DMA)
//! catches up with it at events.
//!
//! The core does no I/O: a host gives it a disc (or a PS-X EXE alone) and
//! the pads, and takes back the picture, the sound and the TTY output. The
//! BIOS is the core's own: it starts the program on the disc (boot.rs) and
//! answers what the program calls of the kernel (bios.rs). A BIOS ROM
//! given instead (Sony's, or OpenBIOS) runs as on the console.

mod bios;
mod boot;
mod bus;
mod cdrom;
mod cpu;
mod dma;
mod files;
mod gpu;
mod gte;
// Compiled for the host's ARM, 32-bit or 64-bit; elsewhere its code is unused.
#[cfg_attr(not(any(target_arch = "arm", target_arch = "aarch64")), allow(dead_code))]
mod jit;
mod mdec;
mod pad;
mod sio;
mod tap;
mod spu;
mod timers;
mod video;

pub use cdrom::{Disc, Track};
pub use gpu::{VRAM_HEIGHT, VRAM_WIDTH};
pub use sio::buttons;

use std::sync::{Arc, Mutex};

// Interrupts, as bits of I_STAT and I_MASK.
const VBLANK: u32 = 1 << 0;
const CDROM: u32 = 1 << 2;
const DMA: u32 = 1 << 3;
const PAD: u32 = 1 << 7;
const SPU: u32 = 1 << 9;

/// CPU cycles a scanline (3413 GPU clocks of 53.69 MHz at 33.87 MHz), as
/// sevenths, and lines a frame (NTSC); the vertical blank starts at 240.
const LINE_SEVENTHS: u64 = 3413 * 7 * 7 / 11;
const LINES: u32 = 263;
const RAM: usize = 0x20_0000;
const VBLANK_LINE: u32 = 240;

#[derive(Debug, PartialEq)]
pub enum Error {
    /// Not a PS-X EXE, or one that does not fit in RAM.
    NotExe,
    /// A BIOS that is not 512 KB.
    BadBios,
    /// A memory card that is not 128 KB.
    BadCard,
    /// A save state that is damaged, or from another version of the core.
    BadState,
    /// A save state from another game.
    OtherGame,
    /// A disc with no program to start: no ISO 9660 file system, or no
    /// file where SYSTEM.CNF's BOOT line (or PSX.EXE without one) says.
    NotBootable,
}

impl From<savestate::BadState> for Error {
    fn from(_: savestate::BadState) -> Error {
        Error::BadState
    }
}

/// A save state starts with these, the format's version and the hash of
/// the game it is of.
const STATE_MAGIC: &[u8] = b"SOLPSX";
const STATE_VERSION: u8 = 5;

/// FNV-1a.
fn hash(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3))
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Error::NotExe => write!(f, "not a PS-X EXE"),
            Error::BadBios => write!(f, "a BIOS is 512 KB"),
            Error::BadCard => write!(f, "a memory card is 128 KB"),
            Error::BadState => write!(f, "the save state is damaged, or from another version"),
            Error::OtherGame => write!(f, "the save state is from another game"),
            Error::NotBootable => write!(f, "the disc has no program to start"),
        }
    }
}

impl std::error::Error for Error {}

/// The CPU's registers, for tests and debugging.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Registers {
    pub r: [u32; 32],
    pub pc: u32,
    pub sr: u32,
}

// C layout, the CPU and the cycle count first: compiled code reaches them
// at fixed offsets.
#[derive(Clone)]
#[repr(C)]
pub struct Psx {
    cpu: cpu::Cpu,
    cycles: u64,
    /// The cycle anything next happens (a device's event, or the
    /// scanline's end).
    next_event: u64,
    /// RAM's 4 KB pages that compiled code came from: writing to one drops
    /// that code.
    code_pages: [u8; RAM / 4096],
    jit: jit::Jit,
    gte: gte::Gte,
    /// 2 MB, as an array so that a masked index needs no bounds check.
    ram: Box<[u8; RAM]>,
    /// The BIOS ROM; empty when the BIOS is answered in HLE.
    bios: Vec<u8>,
    scratchpad: Vec<u8>,
    gpu: video::Video,
    dma: dma::Dma,
    timers: timers::Timers,
    spu: spu::Spu,
    cdrom: cdrom::Cdrom,
    mdec: mdec::Mdec,
    sio: sio::Sio,
    /// SIO1, the serial port nothing is plugged into: its mode, control,
    /// misc and baud registers kept as written.
    sio1: [u16; 4],
    /// The cycle a device (the CD-ROM controller, SIO0) next has something
    /// to do.
    due: u64,
    hle: bios::Hle,
    /// The interrupt controller: requested, and let through.
    irq_stat: u32,
    irq_mask: u32,
    /// Memory control registers, kept as written.
    memctrl: [u32; 9],
    ram_size: u32,
    cache_control: u32,
    /// The scanline, and the cycle (in sevenths) the next starts at.
    line: u32,
    next_line: u64,
    frames: u64,
    tty: Vec<u8>,
    /// The game's hash, which save states carry: of the disc's volume
    /// descriptor, or of the EXE.
    game: u64,
}

// What a save state holds: all but the BIOS, the disc, the memory cards
// (the host keeps them) and what is worked out again.
savestate::state!(Psx { cpu, gte, ram, scratchpad, gpu, dma, timers, spu, cdrom, mdec, sio, sio1, hle, irq_stat, irq_mask,
    memctrl, ram_size, cache_control, cycles, line, next_line, frames });

impl Psx {
    /// A console that starts its BIOS, as on power-on.
    pub fn new(bios: &[u8]) -> Result<Psx, Error> {
        if bios.len() != 0x8_0000 {
            return Err(Error::BadBios);
        }
        let mut psx = Psx::blank();
        psx.bios = bios.to_vec();
        Ok(psx)
    }

    /// A console with a PS-X EXE loaded, as the BIOS leaves it to run one:
    /// its code in RAM, the registers its header asks for, interrupts on.
    pub fn with_exe(exe: &[u8]) -> Result<Psx, Error> {
        let mut psx = Psx::blank();
        psx.game = hash(exe);
        psx.set_up_kernel(4, 16);
        psx.load_exe(exe, None)?;
        // Kernel mode, the GTE on, interrupts on (the controller's mask is
        // still all off).
        psx.cpu.sr = 0x4000_0401;
        Ok(psx)
    }

    /// A console with a disc in the drive, its program started as the BIOS
    /// starts it at power-on (with the stack SYSTEM.CNF asks for, in a
    /// critical section the program leaves when it is set up), with the
    /// core's own BIOS: no BIOS ROM is needed.
    pub fn with_disc(mut disc: impl Disc + 'static) -> Result<Psx, Error> {
        let boot = boot::find(&mut disc).ok_or(Error::NotBootable)?;
        let mut psx = Psx::blank();
        psx.set_up_kernel(boot.threads, boot.events);
        psx.load_exe(&boot.exe, Some(boot.stack))?;
        // One argument, no argv.
        psx.cpu.r[4] = 1;
        psx.cpu.sr = 0x4000_0000;
        psx.insert_disc(disc);
        // What the BIOS leaves set up: the display on (as its shell leaves
        // it), the CD-ROM controller's interrupts enabled, and DMA for the
        // MDEC and the CD-ROM with its interrupts.
        psx.io_write(0x1f80_1814, 0x0300_0000, 4);
        psx.store8(0x1f80_1800, 1);
        psx.store8(0x1f80_1802, 0x1f);
        psx.store8(0x1f80_1803, 0x1f);
        psx.store8(0x1f80_1800, 0);
        psx.store32(0x1f80_10f0, 0x9099);
        psx.store32(0x1f80_10f4, 0x8800_0000);
        Ok(psx)
    }

    /// Puts a PS-X EXE's code in RAM, clears its BSS and sets the registers
    /// its header asks for, as the BIOS's Exec does: the stack given, or
    /// the header's.
    fn load_exe(&mut self, exe: &[u8], stack: Option<u32>) -> Result<(), Error> {
        if exe.len() < 0x800 || &exe[..8] != b"PS-X EXE" {
            return Err(Error::NotExe);
        }
        let word = |i: usize| u32::from_le_bytes(exe[i..i + 4].try_into().unwrap());
        let (pc, gp, addr, size) = (word(0x10), word(0x14), word(0x18), word(0x1c));
        let (bss, bss_size) = (word(0x28), word(0x2c));
        let (stack, stack_size) = stack.map_or((word(0x30), word(0x34)), |s| (s, 0));
        let start = (addr & 0x1f_ffff) as usize;
        let size = (size as usize).min(exe.len() - 0x800);
        if start + size > RAM {
            return Err(Error::NotExe);
        }
        self.ram[start..start + size].copy_from_slice(&exe[0x800..0x800 + size]);
        let bss = (bss & 0x1f_fffc) as usize;
        self.ram[bss..(bss + bss_size as usize).min(RAM)].fill(0);
        let cpu = &mut self.cpu;
        cpu.pc = pc;
        cpu.next_pc = pc.wrapping_add(4);
        cpu.r[28] = gp;
        cpu.r[29] = if stack != 0 { stack.wrapping_add(stack_size) } else { 0x801f_fff0 };
        cpu.r[30] = cpu.r[29];
        Ok(())
    }

    /// Puts a disc in the drive (closing it), for the BIOS or the program
    /// to find.
    pub fn insert_disc(&mut self, mut disc: impl Disc + 'static) {
        let mut pvd = [0; 2352];
        disc.read(16, &mut pvd);
        self.game = hash(&pvd);
        let disc: Arc<Mutex<dyn Disc>> = Arc::new(Mutex::new(disc));
        self.cdrom = cdrom::Cdrom::new(Some(disc));
        self.schedule();
    }

    /// Whether blocks of the program are compiled to the host's code (on
    /// 32-bit or 64-bit ARM; elsewhere there is only the interpreter).
    pub fn set_jit(&mut self, on: bool) {
        self.jit.enabled = on && cfg!(any(target_arch = "arm", target_arch = "aarch64"));
        self.jit.flush();
    }

    /// Whether the GPU draws on a thread of its own (the default where
    /// threads start), the machine running the same either way.
    pub fn set_gpu_thread(&mut self, on: bool) {
        self.gpu.set_thread(on);
    }

    /// Puts a memory card in a slot (0 or 1): its 128 KB as the host kept
    /// them, or a freshly formatted one; None takes the card out. Slot 0
    /// starts with a formatted card.
    pub fn insert_memory_card(&mut self, slot: usize, data: Option<&[u8]>) -> Result<(), Error> {
        let card = match data {
            Some(d) => {
                let d: Box<[u8; sio::CARD]> = d.to_vec().into_boxed_slice().try_into().map_err(|_| Error::BadCard)?;
                sio::Card::new(d)
            }
            None => sio::Card::formatted(),
        };
        self.sio.insert(slot & 1, Some(card));
        Ok(())
    }

    pub fn remove_memory_card(&mut self, slot: usize) {
        self.sio.insert(slot & 1, None);
    }

    /// A memory card's 128 KB, for the host to keep.
    pub fn memory_card(&self, slot: usize) -> Option<&[u8]> {
        self.sio.cards[slot & 1].as_ref().map(|c| &c.data[..])
    }

    /// Whether a memory card was written since this was last asked: when
    /// the host should keep it.
    pub fn memory_card_written(&mut self) -> bool {
        std::mem::take(&mut self.sio.card_written)
    }

    /// The buttons held on the pad in a port (0 or 1; only port 0 has a
    /// pad), as bits of `buttons`.
    pub fn set_pad(&mut self, port: usize, held: u16) {
        self.sio.pads[port & 1] = held;
    }

    /// A port's sticks: right x, right y, left x, left y, from -1 to 1 (right
    /// and down positive), for a game that puts its DualShock in analog mode.
    pub fn set_sticks(&mut self, port: usize, sticks: [(f32, f32); 2]) {
        let to = |v: f32| ((v.clamp(-1.0, 1.0) + 1.0) * 127.5).round() as u8;
        let [(rx, ry), (lx, ly)] = sticks;
        self.sio.dual_shocks[port & 1].sticks = [to(rx), to(ry), to(lx), to(ly)];
    }

    /// Whether a port's DualShock is in analog mode, which a game asks for:
    /// its sticks are then the game's, not the d-pad.
    pub fn analog(&self, port: usize) -> bool {
        self.sio.dual_shocks[port & 1].analog()
    }

    /// A port's DualShock's motors: the small one runs, and the large one's
    /// strength (0 to 255).
    pub fn motors(&self, port: usize) -> (bool, u8) {
        let pad = &self.sio.dual_shocks[port & 1];
        (pad.small, pad.large)
    }

    /// Plugs a pad into the second port, or takes it out (a pad is always in
    /// the first).
    pub fn set_second_pad(&mut self, plugged: bool) {
        self.sio.second_pad = plugged;
    }

    /// Plugs a Multitap into a port (0 or 1), or takes it out (None), with
    /// pads in as many of its slots (1 to 4, from slot A): the players are
    /// then its slots, and the other port is empty.
    pub fn set_multitap(&mut self, port: Option<usize>, pads: usize) {
        self.sio.tap.port = port.map(|p| p & 1);
        self.sio.tap.plugged = std::array::from_fn(|slot| slot + 1 < pads);
        if port.is_some() {
            self.sio.second_pad = false;
        }
    }

    /// A player's (0 to 3) DualShock and buttons: the ports' pads, or with a
    /// Multitap its slots; none for a player who has no pad there.
    fn player(&mut self, player: usize) -> Option<(&mut pad::Pad, &mut u16)> {
        let sio = &mut self.sio;
        match (sio.tap.port, player) {
            (Some(port), 0) | (None, port @ (0 | 1)) => Some((&mut sio.dual_shocks[port], &mut sio.pads[port])),
            (Some(_), slot @ 1..=3) => Some((&mut sio.tap.pads[slot - 1], &mut sio.tap.buttons[slot - 1])),
            _ => None,
        }
    }

    /// The buttons a player (0 to 3) holds, as bits of `buttons`.
    pub fn set_player(&mut self, player: usize, held: u16) {
        if let Some((_, buttons)) = self.player(player) {
            *buttons = held;
        }
    }

    /// A player's sticks, as set_sticks takes a port's.
    pub fn set_player_sticks(&mut self, player: usize, sticks: [(f32, f32); 2]) {
        let to = |v: f32| ((v.clamp(-1.0, 1.0) + 1.0) * 127.5).round() as u8;
        let [(rx, ry), (lx, ly)] = sticks;
        if let Some((pad, _)) = self.player(player) {
            pad.sticks = [to(rx), to(ry), to(lx), to(ly)];
        }
    }

    /// Whether a player's DualShock is in analog mode.
    pub fn player_analog(&mut self, player: usize) -> bool {
        self.player(player).is_some_and(|(pad, _)| pad.analog())
    }

    /// A player's DualShock's motors, as motors gives a port's.
    pub fn player_motors(&mut self, player: usize) -> (bool, u8) {
        self.player(player).map_or((false, 0), |(pad, _)| (pad.small, pad.large))
    }

    fn blank() -> Psx {
        Psx {
            cpu: cpu::Cpu::new(),
            code_pages: [0; RAM / 4096],
            jit: jit::Jit::default(),
            gte: gte::Gte::default(),
            ram: vec![0; RAM].into_boxed_slice().try_into().unwrap(),
            bios: Vec::new(),
            scratchpad: vec![0; 0x400],
            gpu: video::Video::new(),
            dma: dma::Dma::default(),
            timers: timers::Timers::default(),
            spu: spu::Spu::default(),
            cdrom: cdrom::Cdrom::new(None),
            mdec: mdec::Mdec::default(),
            sio: sio::Sio::new(),
            sio1: [0; 4],
            due: u64::MAX,
            next_event: LINE_SEVENTHS.div_ceil(7),
            hle: bios::Hle::default(),
            irq_stat: 0,
            irq_mask: 0,
            memctrl: [0; 9],
            ram_size: 0x0000_0b88,
            cache_control: 0,
            cycles: 0,
            line: 0,
            next_line: LINE_SEVENTHS,
            frames: 0,
            tty: Vec::new(),
            game: 0,
        }
    }

    /// Runs until the vertical blank starts; the sound of the frame is
    /// then in `audio`.
    pub fn run_frame(&mut self) {
        self.spu.output.clear();
        let frame = self.frames;
        if self.jit.enabled {
            while self.frames == frame {
                self.jit_step();
            }
        } else {
            while self.frames == frame {
                self.step_instruction();
            }
        }
        self.spu_sync();
        self.gpu.flush();
    }

    /// The whole machine as bytes, to be put back with `load_state`.
    pub fn save_state(&mut self) -> Vec<u8> {
        use savestate::State;
        self.gpu.sync();
        let mut out = Vec::with_capacity(4 * 1024 * 1024);
        out.extend_from_slice(STATE_MAGIC);
        out.push(STATE_VERSION);
        self.game.save(&mut out);
        self.save(&mut out);
        // After the rest, so a state from before them loads too.
        self.sio.dual_shocks.save(&mut out);
        self.sio.tap.save(&mut out);
        out
    }

    /// Puts the machine back as a save state of this game has it (the disc
    /// in the drive stays). A state that does not load leaves the machine
    /// as it was.
    pub fn load_state(&mut self, data: &[u8]) -> Result<(), Error> {
        use savestate::{State, take};
        let mut input = data;
        if take(&mut input, STATE_MAGIC.len())? != STATE_MAGIC || take(&mut input, 1)?[0] != STATE_VERSION {
            return Err(Error::BadState);
        }
        let mut game = 0u64;
        game.load(&mut input)?;
        if game != self.game {
            return Err(Error::OtherGame);
        }
        self.gpu.sync();
        let mut machine = self.clone();
        machine.load(&mut input)?;
        // The pads' modes, which states from before them do not have: digital.
        machine.sio.dual_shocks = Default::default();
        if !input.is_empty() {
            machine.sio.dual_shocks.load(&mut input)?;
        }
        // The Multitap's slots, the same.
        machine.sio.tap = self.sio.tap.emptied();
        if !input.is_empty() {
            machine.sio.tap.load(&mut input)?;
        }
        if !input.is_empty() {
            return Err(Error::BadState);
        }
        machine.spu.output.clear();
        machine.jit.flush();
        machine.code_pages = [0; RAM / 4096];
        machine.update_irq_line();
        machine.schedule();
        *self = machine;
        Ok(())
    }

    /// The sound made in the last frame: stereo pairs of 16-bit samples at
    /// 44100 Hz.
    pub fn audio(&self) -> &[i16] {
        &self.spu.output
    }

    /// Runs one instruction (or exception, or BIOS call answered), and the
    /// scanline's end if it comes: for debuggers.
    pub fn step_instruction(&mut self) {
        self.step();
        if self.cycles >= self.next_event {
            self.events();
        }
    }

    /// Scanlines that end and devices that have something due.
    fn events(&mut self) {
        while self.cycles * 7 >= self.next_line {
            self.next_scanline();
        }
        if self.cycles >= self.due {
            if self.cycles >= self.cdrom.due() {
                self.cdrom_update();
            }
            self.sio_update();
        }
        self.schedule();
    }

    /// Finds when the devices next have something to do.
    pub(crate) fn schedule(&mut self) {
        self.due = self.cdrom.due().min(self.sio.due());
        self.next_event = self.due.min(self.next_line.div_ceil(7));
    }

    /// A scanline ends: the timers that count them see its blank, and at
    /// line 240 the vertical blank starts.
    fn next_scanline(&mut self) {
        self.next_line += LINE_SEVENTHS;
        self.line = (self.line + 1) % LINES;
        self.timers.hblank(self.cycles);
        self.gpu.next_line(self.line);
        if self.line == VBLANK_LINE {
            self.frames += 1;
            self.gpu.blank();
            self.request(VBLANK);
        }
        self.timers_irq();
    }

    pub(crate) fn request(&mut self, irq: u32) {
        self.irq_stat |= irq;
        self.update_irq_line();
    }

    /// CAUSE bit 10 is the interrupt controller's line: up while a
    /// requested interrupt is let through.
    pub(crate) fn update_irq_line(&mut self) {
        let up = self.irq_stat & self.irq_mask != 0;
        self.cpu.cause = self.cpu.cause & !0x400 | u32::from(up) << 10;
    }

    /// What the program printed through the BIOS.
    pub fn tty(&self) -> &[u8] {
        &self.tty
    }

    /// VRAM, 1024 x 512 halfwords (RGB555 and the mask bit).
    pub fn vram(&mut self) -> &[u16] {
        self.gpu.vram()
    }

    /// The picture the GPU showed at the last vertical blank (where it
    /// draws on its own thread; else as VRAM is): its size, and its pixels
    /// as RGB888.
    pub fn picture(&mut self) -> (usize, usize, &[u8]) {
        self.gpu.picture()
    }

    pub fn registers(&self) -> Registers {
        Registers { r: self.cpu.r, pc: self.cpu.pc, sr: self.cpu.sr }
    }

    /// A word of RAM, the scratchpad or the BIOS, read without touching
    /// any port: for debuggers and achievements.
    pub fn peek32(&self, addr: u32) -> u32 {
        let physical = addr & 0x1fff_ffff;
        let word = |memory: &[u8], i: usize| u32::from_le_bytes(memory[i..i + 4].try_into().unwrap());
        match physical {
            0x0000_0000..=0x007f_ffff => word(&self.ram[..], physical as usize & 0x1f_fffc),
            0x1f80_0000..=0x1f80_03ff => word(&self.scratchpad, physical as usize & 0x3fc),
            0x1fc0_0000..=0x1fc7_ffff if !self.bios.is_empty() => word(&self.bios, physical as usize & 0x7_fffc),
            _ => 0,
        }
    }

    /// A byte of RAM (2 MB, repeated through the first 8 MB of the bus, from any of its three segments) or
    /// of the scratchpad, read without touching any port; None elsewhere: for achievements, which read it
    /// after every frame.
    pub fn peek_byte(&self, addr: u32) -> Option<u8> {
        match addr & 0x1fff_ffff {
            physical @ 0x0000_0000..=0x007f_ffff => Some(self.ram[physical as usize & (RAM - 1)]),
            physical @ 0x1f80_0000..=0x1f80_03ff => Some(self.scratchpad[physical as usize & 0x3ff]),
            _ => None,
        }
    }

    /// The BIOS functions a program called that are not answered.
    pub fn unknown_calls(&self) -> &[(u32, u32)] {
        &self.hle.unknown
    }
}

#[cfg(test)]
mod peek_tests {
    use super::*;

    #[test]
    fn a_byte_of_ram_or_the_scratchpad() {
        let mut psx = Psx::blank();
        psx.store8(0x0001_0004, 0xab);
        psx.store8(0x1f80_0010, 0x5a);
        // The same byte through each segment, and RAM repeated every 2 MB through the first 8.
        for at in [0x0001_0004, 0x8001_0004, 0xa001_0004, 0x0021_0004, 0x0061_0004] {
            assert_eq!(psx.peek_byte(at), Some(0xab), "{at:x}");
        }
        assert_eq!((psx.peek_byte(0x1f80_0010), psx.peek_byte(0x9f80_0010)), (Some(0x5a), Some(0x5a)));
        assert_eq!(psx.peek_byte(0x0001_0005), Some(0));
        // Past the scratchpad, ports, the BIOS and what is past RAM's 8 MB: none.
        assert_eq!([0x1f80_0400, 0x1f80_1810, 0xbfc0_0000, 0x0080_0000].map(|at| psx.peek_byte(at)), [None; 4]);
    }
}
