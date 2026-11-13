//! A Game Boy (DMG) or Game Boy Color, emulated by machine cycle: every CPU
//! memory access is one machine cycle (four clocks), during which the PPU
//! draws four dots and the timer counts four clocks, so the timing games
//! rely on comes out right. In the Color's double speed a machine cycle is
//! two clocks.
//!
//! The core does no I/O: a host gives it a cartridge image and the buttons
//! held each frame, and takes back the picture (as shades, or colours on the
//! Color), the sound, the cartridge's saves and, for Quick Resume, a save
//! state. It starts as the boot ROM leaves the console (the boot ROM itself
//! is Nintendo's, and not needed).

mod apu;
mod cart;
mod cpu;
mod ppu;
mod rtc;

use savestate::state;

pub use apu::SAMPLE_HZ;
pub use cart::Error;
pub use ppu::{HEIGHT, WIDTH};

// Buttons, as bits of what run_frame takes.
pub const RIGHT: u8 = 0x01;
pub const LEFT: u8 = 0x02;
pub const UP: u8 = 0x04;
pub const DOWN: u8 = 0x08;
pub const A: u8 = 0x10;
pub const B: u8 = 0x20;
pub const SELECT: u8 = 0x40;
pub const START: u8 = 0x80;

// Interrupts, as bits of IF and IE (the PPU requests its own two).
const TIMER: u8 = 0x04;
const SERIAL: u8 = 0x08;
const JOYPAD: u8 = 0x10;

/// Machine cycles in a frame: 154 lines of 456 dots, four dots a cycle.
const FRAME_CYCLES: u64 = 154 * 456 / 4;

/// The console the core is: the original Game Boy, or the Game Boy Color
/// running a game made for it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Model {
    Dmg,
    Cgb,
}

impl Model {
    /// The console a cartridge is for: the Color for games made for it
    /// (whether or not they also run on the original), the original for
    /// the rest.
    pub fn detect(rom: &[u8]) -> Model {
        if rom.get(0x143).is_some_and(|flags| flags & 0x80 != 0) { Model::Cgb } else { Model::Dmg }
    }
}

/// The CPU's registers, for tests and debugging.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Registers {
    pub a: u8,
    pub f: u8,
    pub b: u8,
    pub c: u8,
    pub d: u8,
    pub e: u8,
    pub h: u8,
    pub l: u8,
    pub sp: u16,
    pub pc: u16,
}

#[derive(Clone)]
pub struct Gb {
    /// Running a Color game on the Color.
    cgb: bool,
    cpu: cpu::Cpu,
    cart: cart::Cart,
    ppu: ppu::Ppu,
    apu: apu::Apu,
    /// Work RAM: 8 KB, or on the Color 32 KB, of which the bank at D000 is
    /// chosen (SVBK).
    wram: Vec<u8>,
    wram_bank: usize,
    hram: [u8; 0x7f],
    /// Interrupts requested (IF) and enabled (IE).
    requested: u8,
    enabled: u8,

    /// The timer: a 16-bit counter whose top byte is DIV; TIMA counts the
    /// falls of one of its bits. An overflow leaves TIMA at 0 for a cycle
    /// before TMA is loaded and the interrupt requested.
    counter: u16,
    tima: u8,
    tma: u8,
    tac: u8,
    overflow: bool,
    reloading: bool,

    /// The link port: what was sent (for tests), and the bits left of the
    /// byte being sent.
    sb: u8,
    sc: u8,
    serial_bits: u8,
    serial_out: Vec<u8>,

    /// Buttons held, and P1's selection of which to read.
    buttons: u8,
    p1: u8,

    /// OAM DMA: the transfer under way (its source page and next byte),
    /// and one about to start (its page, and the cycles until it does).
    dma: Option<(u8, u8)>,
    dma_start: Option<(u8, u8)>,

    /// The Color's double speed, and a switch to or from it waiting for
    /// STOP (KEY1).
    double_speed: bool,
    speed_switch: bool,
    /// The Color's VRAM DMA: source and destination, the 16-byte blocks
    /// left, and whether it copies one each horizontal blank (rather than
    /// all at once).
    hdma_source: u16,
    hdma_dest: u16,
    hdma_blocks: u8,
    hdma_hblank: bool,
    /// The Color's infrared port (it sees no light), and four registers
    /// with no known use that keep what is written.
    infrared: u8,
    undocumented: [u8; 4],

    cycles: u64,
    /// Time, in the Game Boy's clocks (4 MiHz), for the cartridge's clock.
    clocks: u64,
}

state!(Gb { cpu, cart, ppu, apu, wram, wram_bank, hram, requested, enabled, counter, tima, tma, tac, overflow, reloading,
    sb, sc, serial_bits, buttons, p1, dma, dma_start, double_speed, speed_switch, hdma_source, hdma_dest, hdma_blocks,
    hdma_hblank, infrared, undocumented, cycles, clocks });

/// A save state starts with these, the format's version and the hash of
/// the cartridge it belongs to.
const STATE_MAGIC: &[u8] = b"SOLGB";
const STATE_VERSION: u8 = 5;

impl Gb {
    /// Inserts a cartridge into a console and powers on, as the boot ROM
    /// leaves things. The original refuses games for the Color only.
    pub fn new(rom: &[u8], model: Model) -> Result<Gb, Error> {
        let cgb = model == Model::Cgb;
        let cart = cart::Cart::parse(rom)?;
        if !cgb && rom[0x143] == 0xc0 {
            return Err(Error::ColorOnly);
        }
        let mut gb = Gb {
            cgb,
            cpu: cpu::Cpu::after_boot(cgb),
            cart,
            ppu: ppu::Ppu::new(cgb),
            apu: apu::Apu::new(cgb),
            wram: vec![0; if cgb { 0x8000 } else { 0x2000 }],
            wram_bank: 1,
            hram: [0; 0x7f],
            requested: 0x01,
            enabled: 0,
            counter: 0xabc8,
            tima: 0,
            tma: 0,
            tac: 0xf8,
            overflow: false,
            reloading: false,
            sb: 0,
            sc: 0x7e,
            serial_bits: 0,
            serial_out: Vec::new(),
            buttons: 0,
            p1: 0xcf,
            dma: None,
            dma_start: None,
            double_speed: false,
            speed_switch: false,
            hdma_source: 0,
            hdma_dest: 0,
            hdma_blocks: 0,
            hdma_hblank: false,
            infrared: 0,
            undocumented: [0; 4],
            cycles: 0,
            clocks: 0,
        };
        gb.apu.after_boot();
        Ok(gb)
    }

    /// Runs until the PPU finishes a frame (1/59.73 s) with the buttons
    /// held; while the LCD is off, for that long.
    pub fn run_frame(&mut self, buttons: u8) {
        let before = self.p1_lines();
        self.buttons = buttons;
        // A selected line going low requests the joypad interrupt.
        if self.p1_lines() & !before != 0 {
            self.requested |= JOYPAD;
        }
        self.apu.samples.clear();
        let frame = self.ppu.frames;
        let (end, latest) = (self.clocks + 4 * FRAME_CYCLES, self.clocks + 8 * FRAME_CYCLES);
        while self.ppu.frames == frame {
            self.step();
            if self.clocks >= end && (!self.ppu.on() || self.clocks >= latest) {
                break;
            }
        }
    }

    /// The last frame, 160 x 144: shades, 0 (lightest) to 3 (darkest), or
    /// on the Color RGB555 colours (red in the low five bits).
    pub fn frame(&self) -> &[u16; WIDTH * HEIGHT] {
        &self.ppu.picture.pixels
    }

    /// Whether frame() holds colours rather than shades.
    /// Whether a rumble cartridge's motor runs (an MBC5 with one).
    pub fn rumbling(&self) -> bool {
        self.cart.motor()
    }

    pub fn color(&self) -> bool {
        self.cgb
    }

    /// The sound of the last frame: stereo, left first, SAMPLE_HZ pairs a
    /// second.
    pub fn audio(&self) -> &[i16] {
        &self.apu.samples
    }

    /// The whole machine as bytes (tens of KB, with the cartridge's RAM),
    /// for Quick Resume.
    pub fn save_state(&self) -> Vec<u8> {
        use savestate::State;
        let mut out = Vec::with_capacity(48 * 1024);
        out.extend_from_slice(STATE_MAGIC);
        out.push(STATE_VERSION);
        self.cart.hash.save(&mut out);
        self.save(&mut out);
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
        if !input.is_empty() {
            return Err(Error::BadState);
        }
        machine.apu.samples.clear();
        *self = machine;
        Ok(())
    }

    /// The bytes the game has sent out of the link port.
    pub fn serial(&self) -> &[u8] {
        &self.serial_out
    }

    pub fn registers(&self) -> Registers {
        self.cpu.registers()
    }

    /// Whether LD B,B has run: the Mooneye tests' signal that they are done.
    pub fn breakpoint(&self) -> bool {
        self.cpu.breakpoint
    }

    /// What the cartridge's battery keeps, if it has one: the game's saves,
    /// and the MBC3's clock with the time of the save (as BGB and VBA-M save
    /// it), for the host to keep between sessions.
    pub fn save_data(&self, unix_time: u64) -> Option<Vec<u8>> {
        self.cart.save_data(unix_time)
    }

    /// Puts back what the battery kept, before running; the clock moves on
    /// by the time since it was saved.
    pub fn load_save_data(&mut self, data: &[u8], unix_time: u64) {
        self.cart.load_save_data(data, unix_time);
    }

    /// Moves the cartridge's clock on by time the game did not run (while
    /// it was suspended, or after a save state was taken).
    pub fn pass_time(&mut self, seconds: u64) {
        self.cart.pass_time(seconds);
    }

    /// What the CPU would read at addr, without the side effects of a read.
    pub fn peek(&self, addr: u16) -> u8 {
        match addr {
            0x0000..=0x7fff => self.cart.read_rom(addr),
            0xa000..=0xbfff => self.cart.read_ram(addr),
            0xc000..=0xfdff => self.wram[self.wram_index(addr)],
            0xff80..=0xfffe => self.hram[usize::from(addr - 0xff80)],
            _ => 0xff,
        }
    }

    /// Work RAM as a whole, bank after bank: the original's 8 KB, the Color's eight banks of 4 KB, bank 0
    /// first, whatever bank the game has switched to at D000.
    pub fn work_ram(&self) -> &[u8] {
        &self.wram
    }

    /// The cartridge's RAM as a whole, every bank one after the other, whether the game has enabled it or
    /// not (peek reads 0xFF while it is off, as the CPU would); empty for a cartridge with none.
    pub fn cart_ram(&self) -> &[u8] {
        self.cart.ram()
    }

    /// Where a work RAM address (C000-FDFF, with its echo) lands.
    fn wram_index(&self, addr: u16) -> usize {
        let bank = if addr & 0x1000 != 0 { self.wram_bank } else { 0 };
        bank * 0x1000 + usize::from(addr & 0xfff)
    }

    /// The four lines P1 reads, set where a selected button is held: the
    /// directions with bit 4 clear, A, B, Select and Start with bit 5.
    fn p1_lines(&self) -> u8 {
        let mut lines = 0;
        if self.p1 & 0x10 == 0 {
            lines |= self.buttons & 0x0f;
        }
        if self.p1 & 0x20 == 0 {
            lines |= self.buttons >> 4;
        }
        lines
    }

    /// One machine cycle of everything but the CPU.
    fn tick(&mut self) {
        self.cycles += 1;
        self.clocks += if self.double_speed { 2 } else { 4 };
        self.tick_timer();
        self.requested |= self.ppu.tick();
        self.apu.tick(self.counter, self.double_speed);
        // The link port's clock (8192 Hz, or on the Color 262144 Hz) is a
        // bit of the counter: a bit goes out as it falls.
        if self.serial_bits > 0 {
            let period = if self.cgb && self.sc & 2 != 0 { 0x0f } else { 0x1ff };
            if self.counter & period == 0 {
                self.shift_serial();
            }
        }
        if self.dma.is_some() || self.dma_start.is_some() {
            self.tick_dma();
        }
    }

    /// STOP: on the Color, with a switch asked for, changes the speed and
    /// starts the timer's counter over.
    fn stop(&mut self) {
        if self.speed_switch {
            self.speed_switch = false;
            self.double_speed = !self.double_speed;
            self.ppu.dots_per_cycle = if self.double_speed { 2 } else { 4 };
            self.reset_divider();
        }
    }

    /// Clearing the timer's counter can make the bit TIMA counts fall, and
    /// the link port's clock.
    fn reset_divider(&mut self) {
        let before = self.timer_bit();
        let serial_clock = self.counter & 0x100 != 0;
        self.counter = 0;
        if before {
            self.increment_tima();
        }
        if serial_clock {
            self.shift_serial();
        }
    }

    /// Copies a 16-byte block of the Color's VRAM DMA, during which the CPU
    /// waits: 32 clocks.
    fn hdma_block(&mut self) {
        for i in 0..16 {
            let value = self.dma_read(self.hdma_source.wrapping_add(i));
            let dest = self.ppu.vram_bank * 0x2000 + usize::from(self.hdma_dest.wrapping_add(i) & 0x1fff);
            self.ppu.vram[dest] = value;
        }
        self.hdma_source = self.hdma_source.wrapping_add(16);
        self.hdma_dest = self.hdma_dest.wrapping_add(16) & 0x1ff0;
        self.hdma_blocks -= 1;
        if self.hdma_blocks == 0 {
            self.hdma_hblank = false;
        }
        for _ in 0..if self.double_speed { 16 } else { 8 } {
            self.tick();
        }
    }

    /// What a DMA reads: the cartridge, VRAM or work RAM.
    fn dma_read(&self, addr: u16) -> u8 {
        match addr {
            0x0000..=0x7fff => self.cart.read_rom(addr),
            0x8000..=0x9fff => self.ppu.vram[self.ppu.vram_bank * 0x2000 + usize::from(addr & 0x1fff)],
            0xa000..=0xbfff => self.cart.read_ram(addr),
            _ => self.wram[self.wram_index(addr & 0xdfff)],
        }
    }

    fn shift_serial(&mut self) {
        if self.serial_bits == 0 {
            return;
        }
        // No other Game Boy answers: 1s come in.
        self.sb = self.sb << 1 | 1;
        self.serial_bits -= 1;
        if self.serial_bits == 0 {
            self.sc &= 0x7f;
            self.requested |= SERIAL;
        }
    }

    /// The bit of the counter TIMA counts the falls of, if TIMA runs.
    fn timer_mask(&self) -> u16 {
        if self.tac & 4 == 0 { 0 } else { [1 << 9, 1 << 3, 1 << 5, 1 << 7][usize::from(self.tac & 3)] }
    }

    fn timer_bit(&self) -> bool {
        self.counter & self.timer_mask() != 0
    }

    fn tick_timer(&mut self) {
        if self.reloading || self.overflow {
            self.reloading = false;
            if self.overflow {
                self.overflow = false;
                self.tima = self.tma;
                self.requested |= TIMER;
                self.reloading = true;
            }
        }
        let (before, mask) = (self.counter, self.timer_mask());
        self.counter = before.wrapping_add(4);
        if before & mask != 0 && self.counter & mask == 0 {
            self.increment_tima();
        }
    }

    fn increment_tima(&mut self) {
        let (tima, overflow) = self.tima.overflowing_add(1);
        self.tima = tima;
        self.overflow |= overflow;
    }

    /// OAM DMA starts the second cycle after the write (a transfer under
    /// way goes on until then), and copies a byte a cycle; OAM is out of the
    /// CPU's reach from the first byte to the cycle after the last.
    #[inline(never)]
    fn tick_dma(&mut self) {
        if let Some((page, wait)) = self.dma_start {
            if wait > 0 {
                self.dma_start = Some((page, wait - 1));
            } else {
                self.dma_start = None;
                self.dma = Some((page, 0));
            }
        }
        let Some((page, i)) = self.dma else {
            return;
        };
        if i == 160 {
            self.dma = None;
            return;
        }
        let value = self.dma_read(u16::from(page) << 8 | u16::from(i));
        self.ppu.oam[usize::from(i)] = value;
        self.dma = Some((page, i + 1));
    }

    fn read(&mut self, addr: u16) -> u8 {
        self.tick();
        self.bus_read(addr)
    }

    fn write(&mut self, addr: u16, value: u8) {
        self.tick();
        self.bus_write(addr, value);
    }

    fn bus_read(&mut self, addr: u16) -> u8 {
        match addr {
            0x0000..=0x7fff => self.cart.read_rom(addr),
            0x8000..=0x9fff => self.ppu.read_vram(addr),
            0xa000..=0xbfff => self.cart.read_ram(addr),
            0xc000..=0xfdff => self.wram[self.wram_index(addr)],
            0xfe00..=0xfe9f if self.dma.is_some() => 0xff,
            0xfe00..=0xfe9f => self.ppu.read_oam(addr),
            0xfea0..=0xfeff => 0,
            // Held buttons read as 0.
            0xff00 => 0xc0 | self.p1 | !self.p1_lines() & 0x0f,
            0xff01 => self.sb,
            0xff02 => self.sc | 0x7e,
            0xff04 => (self.counter >> 8) as u8,
            0xff05 => self.tima,
            0xff06 => self.tma,
            0xff07 => self.tac | 0xf8,
            0xff0f => self.requested | 0xe0,
            0xff10..=0xff3f => self.apu.read(addr),
            0xff40..=0xff4b => self.ppu.read_register(addr),
            0xff80..=0xfffe => self.hram[usize::from(addr - 0xff80)],
            0xffff => self.enabled,
            _ if !self.cgb => 0xff,
            0xff4d => 0x7e | u8::from(self.double_speed) << 7 | u8::from(self.speed_switch),
            0xff4f | 0xff68..=0xff6c => self.ppu.read_register(addr),
            // VRAM DMA: the blocks left less one, with bit 7 set when it is
            // not running.
            0xff55 if self.hdma_blocks == 0 => 0xff,
            0xff55 => u8::from(!self.hdma_hblank) << 7 | (self.hdma_blocks - 1),
            0xff56 => 0x3e | self.infrared,
            0xff70 => 0xf8 | self.wram_bank as u8,
            0xff72 | 0xff73 | 0xff74 => self.undocumented[usize::from(addr - 0xff72)],
            0xff75 => 0x8f | self.undocumented[3],
            0xff76 => self.apu.pcm(0),
            0xff77 => self.apu.pcm(1),
            _ => 0xff,
        }
    }

    fn bus_write(&mut self, addr: u16, value: u8) {
        match addr {
            0x0000..=0x7fff => self.cart.write_rom(addr, value, self.clocks),
            0x8000..=0x9fff => self.ppu.write_vram(addr, value),
            0xa000..=0xbfff => self.cart.write_ram(addr, value, self.clocks),
            0xc000..=0xfdff => {
                let i = self.wram_index(addr);
                self.wram[i] = value;
            }
            0xfe00..=0xfe9f if self.dma.is_some() => {}
            0xfe00..=0xfe9f => self.ppu.write_oam(addr, value),
            0xff00 => self.p1 = value & 0x30,
            0xff01 => self.sb = value,
            0xff02 => {
                self.sc = value;
                // Sending on its own clock.
                if value & 0x81 == 0x81 {
                    self.serial_out.push(self.sb);
                    self.serial_bits = 8;
                }
            }
            0xff04 => self.reset_divider(),
            0xff05 => {
                // Ignored in the cycle TMA is loaded; cancels a pending load.
                if !self.reloading {
                    self.tima = value;
                    self.overflow = false;
                }
            }
            0xff06 => {
                self.tma = value;
                if self.reloading {
                    self.tima = value;
                }
            }
            0xff07 => {
                // Changing the bit or turning TIMA off can count a fall too.
                let before = self.timer_bit();
                self.tac = value & 7;
                if before && !self.timer_bit() {
                    self.increment_tima();
                }
            }
            0xff0f => self.requested = value & 0x1f,
            0xff10..=0xff3f => self.apu.write(addr, value),
            0xff46 => {
                self.dma_start = Some((value, 1));
                self.ppu.dma = value;
            }
            0xff40..=0xff4b => self.requested |= self.ppu.write_register(addr, value),
            0xff80..=0xfffe => self.hram[usize::from(addr - 0xff80)] = value,
            0xffff => self.enabled = value,
            _ if !self.cgb => {}
            0xff4d => self.speed_switch = value & 1 != 0,
            0xff4f | 0xff68..=0xff6c => self.requested |= self.ppu.write_register(addr, value),
            0xff51 => self.hdma_source = self.hdma_source & 0x00ff | u16::from(value) << 8,
            0xff52 => self.hdma_source = self.hdma_source & 0xff00 | u16::from(value & 0xf0),
            0xff53 => self.hdma_dest = self.hdma_dest & 0x00ff | u16::from(value & 0x1f) << 8,
            0xff54 => self.hdma_dest = self.hdma_dest & 0xff00 | u16::from(value & 0xf0),
            0xff55 => {
                // The length is set by every write; one with bit 7 clear
                // stops a DMA copying in horizontal blanks, or else copies
                // it all at once.
                self.hdma_blocks = (value & 0x7f) + 1;
                if self.hdma_hblank && value & 0x80 == 0 {
                    self.hdma_hblank = false;
                } else if value & 0x80 == 0 {
                    while self.hdma_blocks > 0 {
                        self.hdma_block();
                    }
                } else {
                    self.hdma_hblank = true;
                    // Started in a horizontal blank, or with the LCD off, it
                    // copies its first block at once.
                    if !self.ppu.on() || self.ppu.mode() == 0 {
                        self.hdma_block();
                    }
                }
            }
            0xff56 => self.infrared = value & 0xc1,
            0xff70 => self.wram_bank = usize::from(value & 7).max(1),
            0xff72 | 0xff73 | 0xff74 => self.undocumented[usize::from(addr - 0xff72)] = value,
            0xff75 => self.undocumented[3] = value & 0x70,
            _ => {}
        }
    }
}
