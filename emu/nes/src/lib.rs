//! A Nintendo Entertainment System (NTSC), emulated cycle by cycle: every
//! CPU bus access is one CPU cycle, during which the PPU draws three dots,
//! so the timing games rely on (sprite 0 hits, scroll changes mid-frame,
//! NMI timing) comes out right.
//!
//! The PPU runs behind the CPU and catches up, dot by dot as ever, when
//! the CPU is about to see or change something of it: its registers, the
//! cartridge's banks, OAM, and the dots at which it changes the CPU's
//! interrupt lines. What the CPU sees is the same as if the two went step
//! by step, at a fraction of the cost.
//!
//! The core does no I/O: a host gives it a cartridge image and the pads'
//! state each frame, and takes back the picture (as palette indices), the
//! sound and, for Quick Resume, a save state.

mod apu;
mod cart;
mod cpu;
pub mod palette;
mod ppu;

use savestate::state;

pub use apu::SAMPLE_HZ;
pub use cart::Error;
pub use ppu::{HEIGHT, WIDTH};

// A pad's buttons, as bits of its state.
pub const A: u8 = 0x01;
pub const B: u8 = 0x02;
pub const SELECT: u8 = 0x04;
pub const START: u8 = 0x08;
pub const UP: u8 = 0x10;
pub const DOWN: u8 = 0x20;
pub const LEFT: u8 = 0x40;
pub const RIGHT: u8 = 0x80;

#[derive(Clone)]
pub struct Nes {
    cpu: cpu::Cpu,
    ram: [u8; 0x800],
    ppu: ppu::Ppu,
    apu: apu::Apu,
    cart: cart::Cart,
    /// The pads' buttons, and the bits still to shift out of each.
    pads: [u8; 2],
    shift: [u8; 2],
    strobe: bool,
    /// The last value on the data bus: what a read from nothing returns.
    bus: u8,
    /// The PPU's NMI line as the CPU last saw it: an NMI is its rising edge.
    nmi_line: bool,
    /// A sprite DMA asked for, which starts at the CPU's next read.
    oam_dma: Option<u8>,
    /// CPU cycles since power-on.
    cycles: u64,
    /// The dots the PPU is behind the CPU.
    ppu_owed: u32,
    /// A Four Score on the pads' ports, while plugged in. Saved after the
    /// rest of a state, so states from before it still load.
    four: FourScore,
}

/// The NES Four Score, players 3 and 4 behind players 1 and 2 (nesdev,
/// Four Score): each port sends its pad's eight bits, then the second pad's
/// on it, then a signature that says a Four Score is there ($10 on $4016,
/// $20 on $4017, the lowest bit first), then 1s.
#[derive(Clone, Default)]
struct FourScore {
    plugged: bool,
    /// Players 3 and 4's buttons.
    pads: [u8; 2],
    /// Each port's 24 bits still to shift out, 1s after them.
    shift: [u32; 2],
}

state!(FourScore { plugged, pads, shift });

impl FourScore {
    /// What a port shifts out from a latch: its first pad, its second, the
    /// signature, then 1s.
    fn latch(&mut self, pads: [u8; 2]) {
        for (i, signature) in [0x10u32, 0x20].into_iter().enumerate() {
            self.shift[i] = u32::from(pads[i]) | u32::from(self.pads[i]) << 8 | signature << 16 | 0xff00_0000;
        }
    }
}

state!(Nes { cpu, ram, ppu, apu, cart, pads, shift, strobe, bus, nmi_line, oam_dma, cycles, ppu_owed });

/// A save state starts with these, the format's version and the hash of
/// the cartridge it belongs to.
const STATE_MAGIC: &[u8] = b"SOLNES";
const STATE_VERSION: u8 = 4;

impl Nes {
    /// Inserts a cartridge (an iNES or NES 2.0 image) and powers on.
    pub fn new(rom: &[u8]) -> Result<Nes, Error> {
        let mut nes = Nes {
            cpu: cpu::Cpu::default(),
            ram: [0; 0x800],
            ppu: ppu::Ppu::new(),
            apu: apu::Apu::new(),
            cart: cart::Cart::parse(rom)?,
            pads: [0; 2],
            shift: [0; 2],
            strobe: false,
            bus: 0,
            nmi_line: false,
            oam_dma: None,
            cycles: 0,
            ppu_owed: 0,
            four: FourScore::default(),
        };
        nes.reset_cpu();
        Ok(nes)
    }

    /// Whether a line shows only the first eight sprites on it, as on the
    /// console (the default), or all of them: then games with more on a
    /// line, which take turns showing them, stop flickering. A line whose
    /// first eight include an empty one keeps the limit, as a game fills
    /// the slots like that on purpose, to hide the sprites after them. The
    /// CPU sees the console's behaviour either way: sprite 0, the overflow
    /// flag, the PPU's bus as the mapper watches it.
    pub fn set_sprite_limit(&mut self, limit: bool) {
        self.sync_ppu();
        self.ppu.sprite_limit = limit;
    }

    /// Presses the reset button.
    pub fn reset(&mut self) {
        self.ppu.reset();
        self.apu.reset();
        self.reset_cpu();
    }

    /// Runs until the PPU finishes a frame, with the pads held as given.
    pub fn run_frame(&mut self, pads: [u8; 2]) {
        self.run_frame_players([pads[0], pads[1], 0, 0]);
    }

    /// Plugs a Four Score into the pads' ports, or takes it out: players 3
    /// and 4 behind players 1 and 2 (FourScore).
    pub fn set_four_score(&mut self, plugged: bool) {
        self.four.plugged = plugged;
    }

    /// As run_frame, with four players: 3 and 4 play through the Four Score
    /// while it is plugged in.
    pub fn run_frame_players(&mut self, pads: [u8; 4]) {
        self.pads = [pads[0], pads[1]];
        self.four.pads = [pads[2], pads[3]];
        self.apu.samples.clear();
        let frame = self.ppu.frames;
        while self.ppu.frames == frame {
            self.step();
        }
        self.sync_ppu();
    }

    /// The last frame: each pixel a palette index (bits 0-5) with the colour
    /// emphasis bits of PPUMASK (6-8); palette::rgb gives the colours.
    pub fn frame(&self) -> &[u16; WIDTH * HEIGHT] {
        &self.ppu.picture.pixels
    }

    /// The sound of the last frame: mono, SAMPLE_HZ samples a second (about
    /// 800 a frame).
    pub fn audio(&self) -> &[i16] {
        &self.apu.samples
    }

    /// The whole machine as bytes (a few tens of KB), for Quick Resume.
    pub fn save_state(&self) -> Vec<u8> {
        use savestate::State;
        let mut out = Vec::with_capacity(16 * 1024);
        out.extend_from_slice(STATE_MAGIC);
        out.push(STATE_VERSION);
        self.cart.hash.save(&mut out);
        self.save(&mut out);
        // The Four Score last: a state from before it has none.
        self.four.save(&mut out);
        out
    }

    /// Puts the machine back as a save state of this game has it. A state
    /// that does not load leaves the machine as it was.
    pub fn load_state(&mut self, data: &[u8]) -> Result<(), Error> {
        use savestate::{State, take};
        let mut input = data;
        if take(&mut input, STATE_MAGIC.len())? != STATE_MAGIC || take(&mut input, 1)?[0] != STATE_VERSION {
            return Err(Error::BadState);
        }
        let mut hash = 0u64;
        hash.load(&mut input)?;
        if hash != self.cart.hash {
            return Err(Error::OtherGame);
        }
        let mut machine = self.clone();
        machine.load(&mut input)?;
        machine.four = FourScore { plugged: self.four.plugged, pads: self.four.pads, shift: [0; 2] };
        if !input.is_empty() {
            machine.four.load(&mut input)?;
        }
        if !input.is_empty() {
            return Err(Error::BadState);
        }
        machine.apu.samples.clear();
        *self = machine;
        Ok(())
    }

    /// The cartridge's battery-backed work RAM, if it has one: the game's
    /// saves, for the host to keep between sessions.
    pub fn battery_ram(&self) -> Option<&[u8]> {
        self.cart.battery_ram()
    }

    /// Puts back battery-backed RAM the host kept, before running.
    pub fn load_battery_ram(&mut self, data: &[u8]) {
        self.cart.load_battery_ram(data);
    }

    /// The CPU's registers and the PPU's state, for debugging.
    pub fn debug(&mut self) -> String {
        self.sync_ppu();
        let c = &self.cpu;
        let (ctrl, mask) = self.ppu.registers();
        format!(
            "PC:{:04X} A:{:02X} X:{:02X} Y:{:02X} P:{:02X} SP:{:02X}{} PPUCTRL:{ctrl:02X} PPUMASK:{mask:02X} line {} cycle {}",
            c.pc, c.a, c.x, c.y, c.p, c.s, if c.jammed { " jammed" } else { "" }, self.ppu.scanline, self.cycles
        )
    }

    /// What the CPU would read at addr, without the side effects a read
    /// can have; for hosts and tests.
    pub fn peek(&self, addr: u16) -> u8 {
        match addr {
            0x0000..=0x1fff => self.ram[usize::from(addr & 0x7ff)],
            0x4020.. => self.cart.cpu_read(addr).unwrap_or(self.bus),
            _ => self.bus,
        }
    }

    /// A CPU cycle with no bus access of its own.
    fn tick(&mut self) {
        self.cycle_start();
        self.cycle_end();
    }

    /// The first part of a CPU cycle: two of its three PPU dots come before
    /// the bus access, with the most common alignment of the two clocks.
    #[inline(always)]
    fn cycle_start(&mut self) {
        self.cycles += 1;
        self.apu.tick();
        self.ppu_owed += 2;
    }

    /// The rest: the third dot, then the CPU samples its interrupt lines, as
    /// it does at the end of every cycle. It acts on what it saw a cycle
    /// before an instruction's last. The PPU catches up first if it changes
    /// the lines in the dots it is behind.
    #[inline(always)]
    fn cycle_end(&mut self) {
        self.ppu_owed += 1;
        if self.ppu_owed >= self.ppu.until_event {
            self.sync_ppu();
        }
        self.cpu.prev_nmi = self.cpu.nmi;
        if self.ppu.nmi && !self.nmi_line {
            self.cpu.nmi = true;
        }
        self.nmi_line = self.ppu.nmi;
        self.cpu.prev_irq = self.cpu.irq;
        self.cpu.irq = self.irq_line() && self.cpu.p & cpu::I == 0;
    }

    /// Runs the PPU up to the CPU.
    fn sync_ppu(&mut self) {
        if self.ppu_owed > 0 {
            self.ppu.run(&mut self.cart, self.ppu_owed);
            self.ppu_owed = 0;
        }
    }

    /// The IRQ line, which the APU and mappers pull.
    fn irq_line(&self) -> bool {
        self.cart.irq() || self.apu.irq()
    }

    /// The DMC fetching a sample byte: the CPU halts on a read cycle and
    /// repeats that read (with whatever it does to the registers read) for
    /// three cycles, then the DMC reads its byte.
    fn dmc_dma(&mut self, addr: u16, sample: u16) {
        for _ in 0..3 {
            self.cycle_start();
            self.bus_read(addr);
            self.cycle_end();
        }
        self.cycle_start();
        let byte = self.bus_read(sample);
        self.cycle_end();
        self.apu.dmc_fill(byte);
    }

    /// Sprite DMA: the CPU halts on a read and repeats it while 256 bytes
    /// from a page of its memory go to OAM, a read and a write cycle each,
    /// after a cycle to halt and one more to line up with the write cycles.
    fn sprite_dma(&mut self, addr: u16, page: u8) {
        self.cycle_start();
        self.bus_read(addr);
        self.cycle_end();
        if self.cycles % 2 == 1 {
            self.cycle_start();
            self.bus_read(addr);
            self.cycle_end();
        }
        for i in 0..=0xff {
            self.cycle_start();
            let value = self.bus_read(u16::from(page) << 8 | i);
            self.cycle_end();
            self.tick();
            self.sync_ppu();
            self.ppu.write_oam(value);
            self.ppu.schedule(&self.cart);
        }
    }

    fn read(&mut self, addr: u16) -> u8 {
        if let Some(page) = self.oam_dma {
            self.oam_dma = None;
            self.sprite_dma(addr, page);
        }
        if let Some(sample) = self.apu.dmc_wants() {
            self.dmc_dma(addr, sample);
        }
        self.cycle_start();
        let value = self.bus_read(addr);
        self.cycle_end();
        value
    }

    /// What a read of addr returns, and does, within a cycle.
    fn bus_read(&mut self, addr: u16) -> u8 {
        let value = match addr {
            0x0000..=0x1fff => self.ram[usize::from(addr & 0x7ff)],
            0x2000..=0x3fff => {
                self.sync_ppu();
                let value = self.ppu.read_register(addr, &mut self.cart);
                self.ppu.schedule(&self.cart);
                value
            }
            // Through a Four Score: 24 bits on each port, then 1s.
            0x4016 | 0x4017 if self.four.plugged => {
                let i = usize::from(addr & 1);
                if self.strobe {
                    self.four.latch(self.pads);
                }
                let bit = (self.four.shift[i] & 1) as u8;
                self.four.shift[i] = self.four.shift[i] >> 1 | 0x8000_0000;
                self.bus & 0xe0 | bit
            }
            0x4016 | 0x4017 => {
                // Pads answer on bit 0; the bus keeps the rest.
                let i = usize::from(addr & 1);
                if self.strobe {
                    self.shift[i] = self.pads[i];
                }
                let bit = self.shift[i] & 1;
                // Once all eight are out, a real pad keeps sending 1s.
                self.shift[i] = self.shift[i] >> 1 | 0x80;
                self.bus & 0xe0 | bit
            }
            // Bit 5 is not driven.
            0x4015 => self.apu.read_status() | self.bus & 0x20,
            0x4020.. => self.cart.cpu_read(addr).unwrap_or(self.bus),
            _ => self.bus,
        };
        self.bus = value;
        value
    }

    fn write(&mut self, addr: u16, value: u8) {
        self.cycle_start();
        self.bus = value;
        match addr {
            0x0000..=0x1fff => self.ram[usize::from(addr & 0x7ff)] = value,
            0x2000..=0x3fff => {
                self.sync_ppu();
                self.ppu.write_register(addr, value, &mut self.cart);
                self.ppu.schedule(&self.cart);
            }
            0x4014 => self.oam_dma = Some(value),
            0x4000..=0x4013 | 0x4015 | 0x4017 => self.apu.write(addr, value),
            0x4016 => {
                self.strobe = value & 1 != 0;
                if self.strobe {
                    self.shift = self.pads;
                    self.four.latch(self.pads);
                }
            }
            0x4020..0x8000 => self.cart.cpu_write(addr, value, self.cycles),
            // Mapper registers: the PPU draws with the banks as they were.
            0x8000.. => {
                self.sync_ppu();
                self.cart.cpu_write(addr, value, self.cycles);
                self.ppu.schedule(&self.cart);
            }
            _ => {}
        }
        self.cycle_end();
    }
}

#[cfg(test)]
mod four_score_tests {
    use super::*;

    /// An NROM cartridge: 16 KB of a loop at $8000, no picture.
    fn machine() -> Nes {
        let mut rom = b"NES\x1a\x01\x01\0\0\0\0\0\0\0\0\0\0".to_vec();
        let mut prg = vec![0xea; 0x4000];
        prg[0..3].copy_from_slice(&[0x4c, 0x00, 0x80]);
        prg[0x3ffc..0x3ffe].copy_from_slice(&[0x00, 0x80]);
        rom.extend(prg);
        rom.extend(vec![0; 0x2000]);
        Nes::new(&rom).unwrap()
    }

    /// n bits from a port, the first read lowest.
    fn bits(nes: &mut Nes, port: u16, n: usize) -> u32 {
        (0..n).fold(0, |v, i| v | u32::from(nes.read(port) & 1) << i)
    }

    /// Each port: its pad, the pad behind it, the signature, then 1s.
    #[test]
    fn four_score_sends_two_pads_and_its_signature() {
        let mut nes = machine();
        nes.set_four_score(true);
        (nes.pads, nes.four.pads) = ([A | UP, B], [START | LEFT, SELECT | RIGHT]);
        nes.write(0x4016, 1);
        nes.write(0x4016, 0);
        assert_eq!(bits(&mut nes, 0x4016, 24), u32::from(A | UP) | u32::from(START | LEFT) << 8 | 0x10 << 16);
        assert_eq!(bits(&mut nes, 0x4017, 24), u32::from(B) | u32::from(SELECT | RIGHT) << 8 | 0x20 << 16);
        assert_eq!((bits(&mut nes, 0x4016, 8), bits(&mut nes, 0x4017, 8)), (0xff, 0xff));
        // With the strobe high, pad 1's A, read after read.
        nes.write(0x4016, 1);
        assert_eq!(bits(&mut nes, 0x4016, 4), 0xf);
    }

    /// Unplugged, a port is one pad: eight bits, then 1s.
    #[test]
    fn without_it_a_port_is_one_pad() {
        let mut nes = machine();
        (nes.pads, nes.four.pads) = ([A, B], [START, START]);
        nes.write(0x4016, 1);
        nes.write(0x4016, 0);
        assert_eq!(bits(&mut nes, 0x4016, 16), u32::from(A) | 0xff00);
    }

    /// A state saved before the Four Score (none at its end) still loads;
    /// a whole one brings it back as it was.
    #[test]
    fn states_from_before_it_still_load() {
        use savestate::State;
        let mut nes = machine();
        nes.set_four_score(true);
        nes.four.pads = [A, 0];
        nes.write(0x4016, 1);
        nes.write(0x4016, 0);
        let whole = nes.save_state();
        let mut tail = Vec::new();
        nes.four.save(&mut tail);
        let mut other = machine();
        assert_eq!(other.load_state(&whole[..whole.len() - tail.len()]), Ok(()));
        assert!(!other.four.plugged && other.four.shift == [0; 2]);
        assert_eq!(other.load_state(&whole), Ok(()));
        assert_eq!(other.save_state(), whole);
    }
}
