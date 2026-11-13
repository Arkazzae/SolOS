//! A Game Boy Advance. The CPU (an ARM7TDMI at 16.78 MHz) runs instruction
//! by instruction, each costing the cycles its memory accesses take; the
//! rest of the console is not stepped with it but moves at events: the PPU
//! at the start and end of each line's horizontal blank, the timers when
//! they overflow, and the sound's mixer 32768 times a second. Between
//! events the CPU runs on its own, which is what makes the console cheap
//! enough to emulate on the Pi.
//!
//! The core does no I/O: a host gives it a cartridge image and the buttons
//! held each frame, and takes back the picture (RGB555), the sound, the
//! cartridge's saves and, for Quick Resume, a save state. It starts as the
//! BIOS leaves the console, with a BIOS of its own (bios.rs): Nintendo's
//! is not needed.

mod apu;
mod arm;
mod bios;
mod bus;
mod cart;
mod cpu;
mod dma;
mod ppu;
mod rtc;
mod thumb;
mod timer;

use savestate::state;

pub use apu::SAMPLE_HZ;
pub use cart::Error;
pub use ppu::{HEIGHT, WIDTH};

// Buttons, as bits of what run_frame takes (KEYINPUT's order).
pub const A: u16 = 1 << 0;
pub const B: u16 = 1 << 1;
pub const SELECT: u16 = 1 << 2;
pub const START: u16 = 1 << 3;
pub const RIGHT: u16 = 1 << 4;
pub const LEFT: u16 = 1 << 5;
pub const UP: u16 = 1 << 6;
pub const DOWN: u16 = 1 << 7;
pub const R: u16 = 1 << 8;
pub const L: u16 = 1 << 9;

// Interrupts, as bits of IE and IF.
const VBLANK_IRQ: u16 = 1 << 0;
const HBLANK_IRQ: u16 = 1 << 1;
const VCOUNT_IRQ: u16 = 1 << 2;
const TIMER_IRQ: u16 = 1 << 3;
const DMA_IRQ: u16 = 1 << 8;
const KEYPAD_IRQ: u16 = 1 << 12;

// The events, by number: the PPU, the four timers' overflows, the mixer.
const PPU: usize = 0;
const TIMER0: usize = 1;
const AUDIO: usize = 5;
const EVENTS: usize = 6;

/// The CPU's registers, for tests and debugging: r0-r15 as the current mode
/// sees them, r15 being the address of the next instruction to run.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Registers {
    pub r: [u32; 16],
    pub cpsr: u32,
}

#[derive(Clone)]
pub struct Gba {
    cpu: cpu::Cpu,
    bios: bios::Bios,
    cart: cart::Cart,
    ppu: ppu::Ppu,
    apu: apu::Apu,
    dma: [dma::Channel; 4],
    /// The last value DMA moved, which it reads where nothing answers.
    dma_latch: u32,
    timers: [timer::Timer; 4],
    system: bus::System,
    prefetch: bus::Prefetch,
    ewram: Vec<u8>,
    iwram: Vec<u8>,
    /// What WAITCNT makes each access cost, whether an interrupt is
    /// requested, enabled and let through by IME, and the loop the CPU may
    /// be idling in: none of them part of the state.
    waits: bus::Waits,
    irq_line: bool,
    idle_loop: cpu::IdleLoop,
    cycles: u64,
    /// When each event is next due, and the soonest of them.
    events: [u64; EVENTS],
    next_event: u64,
}

state!(Gba { cpu, bios, cart, ppu, apu, dma, dma_latch, timers, system, prefetch, ewram, iwram, cycles, events,
    next_event });

/// A save state starts with these, the format's version and the hash of
/// the cartridge it belongs to.
const STATE_MAGIC: &[u8] = b"SOLGBA";
const STATE_VERSION: u8 = 1;

impl Gba {
    /// Inserts a cartridge and powers on, as the BIOS leaves things.
    pub fn new(rom: &[u8]) -> Result<Gba, Error> {
        let mut gba = Gba {
            cpu: cpu::Cpu::after_boot(),
            bios: bios::Bios::new(),
            cart: cart::Cart::parse(rom)?,
            ppu: ppu::Ppu::new(),
            apu: apu::Apu::new(),
            dma: [dma::Channel::default(); 4],
            dma_latch: 0,
            timers: [timer::Timer::default(); 4],
            system: bus::System::new(),
            prefetch: bus::Prefetch::default(),
            ewram: vec![0; 0x40000],
            iwram: vec![0; 0x8000],
            waits: bus::Waits::new(0, 2),
            irq_line: false,
            idle_loop: cpu::IdleLoop::default(),
            cycles: 0,
            events: [u64::MAX; EVENTS],
            next_event: u64::MAX,
        };
        gba.apu.set_bias_level(0x200);
        gba.update_waits();
        gba.schedule(PPU, 1006);
        gba.schedule(AUDIO, apu::MIX_CYCLES);
        gba.jump(0x0800_0000);
        gba.cpu.flushed = false;
        Ok(gba)
    }

    /// Runs until the start of the next vertical blank (1/59.73 s), with
    /// the buttons held.
    pub fn run_frame(&mut self, buttons: u16) {
        self.system.keys = buttons & 0x3ff;
        self.check_keypad();
        // The keys may have changed: a loop that waited looks again.
        self.idle_loop.waiting = false;
        self.apu.samples.clear();
        let frame = self.ppu.frames;
        while self.ppu.frames == frame {
            while self.cycles < self.next_event {
                if self.cpu.halted {
                    if self.system.ie & self.system.requested == 0 {
                        self.cycles = self.next_event;
                        break;
                    }
                    self.cpu.halted = false;
                }
                self.step();
            }
            self.run_events();
            // An idle loop waits on to the next event if these changed
            // nothing it sees.
            let idle = &mut self.idle_loop;
            if idle.waiting {
                if idle.across_events && !idle.dma_ran && !(self.irq_line && self.cpu.cpsr & cpu::I == 0) {
                    self.cycles = self.cycles.max(self.next_event);
                } else {
                    idle.waiting = false;
                }
                idle.dma_ran = false;
            }
        }
    }

    /// Whether a rumble cartridge's motor runs.
    pub fn rumbling(&self) -> bool {
        self.cart.motor()
    }

    /// The last frame, 240 x 160 RGB555 colours (red in the low five bits).
    pub fn frame(&self) -> &[u16; WIDTH * HEIGHT] {
        self.ppu.picture.as_slice().try_into().unwrap()
    }

    /// The sound of the last frame: stereo, left first, SAMPLE_HZ pairs a
    /// second.
    pub fn audio(&self) -> &[i16] {
        &self.apu.samples
    }

    /// The whole machine as bytes (about 400 KB, with the cartridge's save
    /// memory), for Quick Resume.
    pub fn save_state(&self) -> Vec<u8> {
        use savestate::State;
        let mut out = Vec::with_capacity(512 * 1024);
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
        machine.ppu.oam_changed = true;
        machine.ppu.palette_written(0, 0x400);
        machine.update_waits();
        machine.update_irq();
        *self = machine;
        Ok(())
    }

    pub fn registers(&self) -> Registers {
        let mut r = self.cpu.r;
        r[15] = r[15].wrapping_sub(if self.cpu.thumb() { 4 } else { 8 });
        Registers { r, cpsr: self.cpu.cpsr }
    }

    /// What the cartridge's save memory holds (SRAM, Flash or EEPROM), if
    /// it has some, for the host to keep between sessions.
    pub fn save_data(&self) -> Option<Vec<u8>> {
        self.cart.save_data()
    }

    /// Puts back what the save memory held, before running.
    pub fn load_save_data(&mut self, data: &[u8]) {
        self.cart.load_save_data(data);
    }

    /// Sets the cartridge's clock, if it has one (Pokémon Ruby, Sapphire
    /// and Emerald), to a date and time as seconds since 1970, in the
    /// player's time zone. From then on it moves with the emulated time.
    pub fn set_clock(&mut self, seconds: u64) {
        let now = self.cycles;
        if let Some(rtc) = &mut self.cart.rtc {
            rtc.set(seconds, now);
        }
    }

    /// Moves the cartridge's clock on by time the game did not run (while
    /// it was suspended, or after a save state was taken).
    pub fn pass_time(&mut self, seconds: u64) {
        if let Some(rtc) = &mut self.cart.rtc {
            rtc.pass_time(seconds);
        }
    }

    /// What the CPU would read at an address, without the side effects of
    /// a read (I/O registers read as 0).
    pub fn peek(&self, addr: u32) -> u8 {
        match addr >> 24 {
            0x02 => self.ewram[addr as usize & 0x3ffff],
            0x03 => self.iwram[addr as usize & 0x7fff],
            0x05 => self.ppu.palette[addr as usize & 0x3ff],
            0x06 => self.ppu.vram[bus::vram_index(addr)],
            0x07 => self.ppu.oam[addr as usize & 0x3ff],
            0x08..=0x0d => (self.cart.read_rom(addr & !1) >> ((addr & 1) * 8)) as u8,
            0x0e | 0x0f => self.cart.read_backup(addr),
            _ => 0,
        }
    }

    /// Sets when an event is next due.
    fn schedule(&mut self, event: usize, at: u64) {
        self.events[event] = at;
        self.next_event = self.events.iter().copied().min().unwrap_or(u64::MAX);
    }

    /// Handles the events that are due, soonest first.
    fn run_events(&mut self) {
        while self.next_event <= self.cycles {
            let event = (0..EVENTS).min_by_key(|&e| self.events[e]).unwrap();
            let at = self.events[event];
            self.schedule(event, u64::MAX);
            match event {
                PPU => self.ppu_event(at),
                AUDIO => self.audio_event(at),
                _ => self.timer_overflow(event - TIMER0, at),
            }
        }
    }

    fn request(&mut self, irq: u16) {
        self.system.requested |= irq;
        self.update_irq();
    }

    fn update_irq(&mut self) {
        self.irq_line = self.system.ime && self.system.ie & self.system.requested != 0;
    }

    /// The keypad's interrupt, when the buttons KEYCNT picks are held (all
    /// of them, or any).
    fn check_keypad(&mut self) {
        let keycnt = self.system.keycnt;
        if keycnt & 0x4000 == 0 {
            return;
        }
        let (picked, held) = (keycnt & 0x3ff, self.system.keys & keycnt & 0x3ff);
        if if keycnt & 0x8000 != 0 { held == picked } else { held != 0 } {
            self.request(KEYPAD_IRQ);
        }
    }
}
