//! Sega Master System and Game Gear: a native Rust core with no host I/O. A host supplies the cartridge bytes and
//! the pads, then takes RGB565 frames, interleaved stereo PCM and portable Quick Resume states.
//!
//! One Z80 at 3.58 MHz, 8 KiB of RAM, the VDP with its PSG and the cartridge. No BIOS is needed: a cartridge starts
//! the way the BIOS leaves it, and a BIOS image with a game in it (the Master System II's Sonic) runs in the BIOS
//! slot with the cartridge slot empty. The Game Gear is the same machine with a Start button, 12-bit colours, a
//! stereo PSG and a 160x144 window onto the picture; in its Master System mode, which some of its cartridges
//! select, it shows a Master System's whole picture in that console's colours.
mod cart;
mod sound;
mod vdp;

use savestate::{BadState, State, state};
use z80::Z80;

pub use sound::SAMPLE_HZ;
pub use vdp::{GG_HEIGHT, GG_WIDTH, HEIGHT, WIDTH};

pub const UP: u16 = 1;
pub const DOWN: u16 = 2;
pub const LEFT: u16 = 4;
pub const RIGHT: u16 = 8;
/// The left fire button (TL).
pub const BUTTON_1: u16 = 16;
/// The right fire button (TR).
pub const BUTTON_2: u16 = 32;
/// The Game Gear's Start button, and the Master System's Pause button (an NMI).
pub const START: u16 = 64;

/// Z80 cycles in a scanline, and the point at which the VDP is done drawing one (256 of the line's 342 pixels).
const LINE_CYCLES: u32 = 228;
const ACTIVE_END: u32 = 172;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum System {
    MasterSystem,
    GameGear,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Region {
    Japan,
    Usa,
    Europe,
}

impl Region {
    pub fn pal(self) -> bool {
        self == Self::Europe
    }
}

/// Whether the machine runs 313 lines at 50 Hz: only a European Master System does. The region still tells a
/// Game Gear's port $00 whether to say Japanese.
fn is_pal(system: System, region: Region) -> bool {
    system == System::MasterSystem && region.pal()
}

/// The Game Gear cartridges that put the console in its Master System mode (a pin of the cartridge does), by their
/// CRC-32: the Game Gear set of No-Intro's DAT (libretro-database 77f15a8) has these, and only these, as `.sms`
/// files. They are Master System games in a Game Gear cartridge (Castle of Illusion, Chase H.Q., OutRun Europa,
/// Olympic Gold ...): 6-bit colours one byte each, the whole 256x192 picture, Start as Pause.
const MASTER_SYSTEM_MODE: [u32; 25] = [
    0x59840fd6, 0xcc521975, 0xc8381def, 0x8821a7c9, 0xaa140c9c, 0xc888222b, 0x76c5bdfb, 0xa59b4a09,
    0x9942b69b, 0x1d93246e, 0xa2f9c7af, 0x3382d73f, 0x01eab89d, 0xf037ec00, 0xe5f789b9, 0x311d2863,
    0x56201996, 0x9c76fb3a, 0x01a2d595, 0x9fa727a0, 0xfb481971, 0x10dbbef4, 0x7bb81e3d, 0xda8e95a9,
    0x5e7b18c8,
];

/// BIOS images with a game in them, by their CRC-32, which run in a Master System's BIOS slot with no cartridge:
/// the Master System II's with Sonic The Hedgehog (Europe). A frontend names others by the `[BIOS]` tag in their
/// file's name (`Sms::bios`).
const BIOS_GAMES: [u32; 1] = [0x81c3476b];

#[derive(Debug, PartialEq)]
pub enum Error {
    NotRom,
    BadState,
    OtherGame,
    SaveSize,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotRom => {
                f.write_str("not a supported Master System or Game Gear cartridge image")
            }
            Self::BadState => f.write_str("damaged or incompatible Master System save state"),
            Self::OtherGame => f.write_str("save state belongs to another cartridge"),
            Self::SaveSize => f.write_str("save RAM size does not match the cartridge"),
        }
    }
}
impl std::error::Error for Error {}
impl From<BadState> for Error {
    fn from(_: BadState) -> Self {
        Self::BadState
    }
}

/// What a cartridge's header and mapper say it was made for: the Game Gear when the header's region code is 5
/// to 7, else the Master System; a Japanese header means Japan, a Codemasters cartridge for the Master System
/// Europe (those games want a PAL console; no Game Gear is PAL) and anything else the USA. A cartridge without a
/// header is a USA Master System game.
pub fn detect(rom: &[u8]) -> (System, Region) {
    cart::Cart::parse(rom).map_or((System::MasterSystem, Region::Usa), |cart| {
        detect_cart(&cart)
    })
}

fn detect_cart(cart: &cart::Cart) -> (System, Region) {
    let code = cart.header.map_or(0, |h| h.region_code);
    let system = if (5..=7).contains(&code) {
        System::GameGear
    } else {
        System::MasterSystem
    };
    let region = match code {
        3 | 5 => Region::Japan,
        _ if cart.mapper == cart::Mapper::Codemasters && system == System::MasterSystem => {
            Region::Europe
        }
        _ => Region::Usa,
    };
    (system, region)
}

/// Port $3E as the program finds it: the BIOS slot, RAM and the I/O chip on for a BIOS (the slot it starts from),
/// and as the BIOS leaves it for a cartridge (cartridge slot, RAM and I/O on) otherwise.
fn mem_ctl_at_start(bios: bool) -> u8 {
    if bios { 0xe3 } else { 0xab }
}

#[derive(Clone)]
struct Hardware {
    cart: cart::Cart,
    ram: Box<[u8; 0x2000]>,
    vdp: vdp::Vdp,
    sound: sound::Sound,
    /// Port $3E, the memory control: which of the slots and RAM are switched on.
    mem_ctl: u8,
    /// Port $3F, the I/O control: direction and level of the pads' TR and TH pins.
    io_ctl: u8,
    /// The Game Gear's link-cable ports $01-$05, read back as written.
    link: [u8; 5],
    /// The H counter as the TH pins last latched it.
    h_latch: u8,
    /// Z80 cycles into the current line. Between frames it is what the last instruction ran over.
    cycle: u32,
    /// Cycles that have passed since the PSG was last run; see `sound_now`. Zero between lines.
    sound_debt: u32,
    /// Whether the VDP's interrupt line was up at the last instruction, and the cycles that still have to pass
    /// before the Z80 sees it (`vdp::IRQ_LATENCY`).
    irq_up: bool,
    irq_wait: u32,
    system: System,
    region: Region,
    /// Whether the image is a BIOS in the console's BIOS slot (one with a game in it, such as the Master System
    /// II's Sonic), with the cartridge slot empty, rather than a cartridge.
    bios: bool,
    /// A Game Gear in its Master System mode: the video chip is a Master System's (its colours, its whole
    /// picture) and Start is the Pause button's NMI; the ports, the I/O and the 60 Hz stay the Game Gear's.
    sms_mode: bool,
    pads: [u16; 2],
    /// Bytes the program wrote to the SDSC debug console (port $FD), for test programs.
    console: Vec<u8>,
}

state!(Hardware {
    cart,
    ram,
    vdp,
    sound,
    mem_ctl,
    io_ctl,
    link,
    h_latch,
    cycle,
    irq_up,
    irq_wait
});

impl Hardware {
    fn new(cart: cart::Cart, system: System, region: Region, bios: bool, sms_mode: bool) -> Self {
        Self {
            cart,
            ram: vec![0; 0x2000].into_boxed_slice().try_into().unwrap(),
            vdp: vdp::Vdp::new(
                system == System::GameGear && !sms_mode,
                is_pal(system, region),
            ),
            sound: sound::Sound::new(is_pal(system, region)),
            mem_ctl: mem_ctl_at_start(bios),
            io_ctl: 0xff,
            link: [0x7f, 0xff, 0x00, 0xff, 0x00],
            h_latch: 0,
            cycle: 0,
            sound_debt: 0,
            irq_up: false,
            irq_wait: 0,
            system,
            region,
            bios,
            sms_mode,
            pads: [0; 2],
            console: Vec::new(),
        }
    }

    /// Runs the PSG up to the present, which it only needs when something reads or changes it and at the end of
    /// every line.
    fn sound_now(&mut self) -> &mut sound::Sound {
        if self.sound_debt != 0 {
            self.sound.advance(self.sound_debt);
            self.sound_debt = 0;
        }
        &mut self.sound
    }

    /// Whether the image answers: a cartridge in the cartridge slot (always on in a Game Gear) or a BIOS in the
    /// BIOS slot. The other slots (card, expansion, and the cartridge slot when the image is the BIOS) are empty.
    fn cart_enabled(&self) -> bool {
        if self.bios {
            self.mem_ctl & 0x08 == 0
        } else {
            self.system == System::GameGear || self.mem_ctl & 0x40 == 0
        }
    }
    fn ram_enabled(&self) -> bool {
        self.mem_ctl & 0x10 == 0
    }
    fn japanese(&self) -> bool {
        self.region == Region::Japan
    }

    /// Ports $DC and $C0: pad 1's directions and fire buttons, pad 2's up and down; a pressed button reads 0.
    fn port_a_b(&self) -> u8 {
        let [a, b] = self.pads;
        let mut value = (!((a & 0x3f) | ((b & 3) << 6))) as u8;
        // A pin set as an output reads back its level, except on a Japanese console.
        if !self.japanese() && self.io_ctl & 0x01 == 0 {
            value = (value & !0x20) | ((self.io_ctl & 0x10) << 1);
        }
        value
    }

    /// Ports $DD and $C1: pad 2's left, right and fire buttons, the reset button, and the TH pins.
    fn port_b_misc(&self) -> u8 {
        let b = self.pads[1];
        let mut value = ((!(b >> 2)) as u8 & 0x0f) | 0x30;
        let japan = self.japanese();
        if !japan && self.io_ctl & 0x04 == 0 {
            value = (value & !0x08) | (((self.io_ctl >> 6) & 1) << 3);
        }
        // A TH pin used as an input is pulled high. Used as an output, an export console reads its level back
        // and a Japanese one has the pin unconnected, which is how games tell them apart.
        let th = |direction: u8, level: u8| -> u8 {
            if self.io_ctl & direction != 0 {
                1
            } else if japan {
                0
            } else {
                (self.io_ctl >> level) & 1
            }
        };
        value | (th(0x02, 5) << 6) | (th(0x08, 7) << 7)
    }

    fn gg_port(&self, port: u8) -> u8 {
        match port {
            0 => {
                let start = if self.pads[0] & START != 0 { 0 } else { 0x80 };
                start | if self.japanese() { 0 } else { 0x40 }
            }
            1..=5 => self.link[usize::from(port) - 1],
            _ => 0xff,
        }
    }

    fn io_read(&self, port: u8) -> u8 {
        match self.system {
            System::MasterSystem => {
                // Bit 2 of the memory control switches the I/O chip off.
                if self.mem_ctl & 0x04 != 0 {
                    return 0xff;
                }
            }
            System::GameGear => {
                if !matches!(port, 0xc0 | 0xc1 | 0xdc | 0xdd) {
                    return 0xff;
                }
            }
        }
        if port & 1 == 0 {
            self.port_a_b()
        } else {
            self.port_b_misc()
        }
    }
}

impl z80::Bus for Hardware {
    fn read(&mut self, addr: u16) -> u8 {
        if addr < 0xc000 {
            if self.cart_enabled() {
                self.cart.read(addr)
            } else {
                0xff
            }
        } else if self.ram_enabled() {
            self.ram[usize::from(addr & 0x1fff)]
        } else {
            0xff
        }
    }

    fn write(&mut self, addr: u16, value: u8) {
        if addr < 0xc000 {
            if self.cart_enabled() {
                self.cart.write(addr, value);
            }
            return;
        }
        if self.ram_enabled() {
            self.ram[usize::from(addr & 0x1fff)] = value;
        }
        // The mapper's registers sit in the RAM mirror, which keeps what was written as well.
        if addr >= 0xfffc {
            self.cart.mapper_write(addr, value);
        }
    }

    fn input(&mut self, port: u16) -> u8 {
        let port = port as u8;
        match port {
            0x00..=0x3f => {
                if self.system == System::GameGear && port <= 6 {
                    self.gg_port(port)
                } else {
                    0xff
                }
            }
            0x40..=0x7f => {
                if port & 1 == 0 {
                    self.vdp.v_counter(self.cycle)
                } else {
                    self.h_latch
                }
            }
            0x80..=0xbf => {
                if port & 1 == 0 {
                    self.vdp.read_data()
                } else {
                    self.vdp.read_status(self.cycle)
                }
            }
            _ => self.io_read(port),
        }
    }

    fn output(&mut self, port: u16, value: u8) {
        let port = port as u8;
        match port {
            0x00..=0x3f => {
                if self.system == System::GameGear && port <= 6 {
                    match port {
                        1..=5 => self.link[usize::from(port) - 1] = value,
                        6 => self.sound_now().set_stereo(value),
                        _ => {}
                    }
                } else if port & 1 == 0 {
                    self.mem_ctl = value;
                } else {
                    // A TH pin going high latches the H counter, which is how a light gun reads its position (going low
                    // does not: FluBBa's VDP test 23). An output pin is at its level bit, one set to input is pulled up.
                    let th = |ctl: u8| {
                        [
                            ctl & 0x02 != 0 || ctl & 0x20 != 0,
                            ctl & 0x08 != 0 || ctl & 0x80 != 0,
                        ]
                    };
                    let (before, after) = (th(self.io_ctl), th(value));
                    if (0..2).any(|i| after[i] && !before[i]) {
                        self.h_latch = vdp::Vdp::h_counter(self.cycle);
                    }
                    self.io_ctl = value;
                }
            }
            0x40..=0x7f => self.sound_now().write(value),
            0x80..=0xbf => {
                if port & 1 == 0 {
                    self.vdp.write_data(value);
                } else {
                    self.vdp.write_control(value);
                }
            }
            0xfd => {
                if self.console.len() < 1 << 20 {
                    self.console.push(value);
                }
            }
            _ => {}
        }
    }
}

#[derive(Clone)]
pub struct Sms {
    z80: Z80,
    hw: Hardware,
    /// Whether the Pause button was held on the last frame, so that only pressing it is an NMI.
    pause_held: bool,
    frames: u64,
}

state!(Sms {
    z80,
    hw,
    pause_held,
    frames
});

impl Sms {
    /// A cartridge in the given console, in the region its header names (see `detect`).
    pub fn new(rom: &[u8], system: System) -> Result<Self, Error> {
        let cart = cart::Cart::parse(rom)?;
        let region = detect_cart(&cart).1;
        Ok(Self::cartridge(cart, system, region))
    }

    /// A cartridge in the given console and region. The image's CRC-32 says the rest: a known BIOS image with a
    /// game in it goes in a Master System's BIOS slot (`bios`), and a Game Gear cartridge of the Master System
    /// mode puts the Game Gear in that mode (`master_system_mode`).
    pub fn with_region(rom: &[u8], system: System, region: Region) -> Result<Self, Error> {
        Ok(Self::cartridge(cart::Cart::parse(rom)?, system, region))
    }

    fn cartridge(cart: cart::Cart, system: System, region: Region) -> Self {
        let bios = system == System::MasterSystem && BIOS_GAMES.contains(&cart.crc);
        let sms_mode = system == System::GameGear && MASTER_SYSTEM_MODE.contains(&cart.crc);
        Self::from_cart(cart, system, region, bios, sms_mode)
    }

    /// A Master System with this BIOS image in its BIOS slot and no cartridge, which runs the game built into it.
    pub fn bios(image: &[u8], region: Region) -> Result<Self, Error> {
        Ok(Self::from_cart(
            cart::Cart::parse(image)?,
            System::MasterSystem,
            region,
            true,
            false,
        ))
    }

    /// A Game Gear in its Master System mode with this cartridge: a Master System game on a Game Gear, as the
    /// Master Gear adapter runs one, and the Game Gear cartridges made that way.
    pub fn master_system_mode(rom: &[u8], region: Region) -> Result<Self, Error> {
        Ok(Self::from_cart(
            cart::Cart::parse(rom)?,
            System::GameGear,
            region,
            false,
            true,
        ))
    }

    fn from_cart(
        cart: cart::Cart,
        system: System,
        region: Region,
        bios: bool,
        sms_mode: bool,
    ) -> Self {
        let hw = Hardware::new(cart, system, region, bios, sms_mode);
        let mut sms = Self {
            z80: Z80::new(),
            hw,
            pause_held: false,
            frames: 0,
        };
        sms.power_on();
        sms
    }

    /// The state a cartridge finds when the BIOS is done.
    fn power_on(&mut self) {
        self.z80 = Z80::new();
        self.z80.sp = 0xdff0;
        self.pause_held = false;
    }

    pub fn reset(&mut self) {
        let hw = &mut self.hw;
        hw.cart.reset();
        hw.ram.fill(0);
        let pal = is_pal(hw.system, hw.region);
        hw.vdp = vdp::Vdp::new(hw.system == System::GameGear && !hw.sms_mode, pal);
        hw.sound = sound::Sound::new(pal);
        hw.mem_ctl = mem_ctl_at_start(hw.bios);
        hw.io_ctl = 0xff;
        hw.link = [0x7f, 0xff, 0x00, 0xff, 0x00];
        hw.h_latch = 0;
        hw.cycle = 0;
        hw.sound_debt = 0;
        hw.irq_up = false;
        hw.irq_wait = 0;
        self.power_on();
    }

    pub fn system(&self) -> System {
        self.hw.system
    }
    pub fn region(&self) -> Region {
        self.hw.region
    }
    /// Whether it is a Game Gear in its Master System mode, whose picture is a Master System's.
    pub fn sms_mode(&self) -> bool {
        self.hw.sms_mode
    }
    /// Whether the image runs in the BIOS slot (`bios`).
    pub fn bios_slot(&self) -> bool {
        self.hw.bios
    }
    /// Whether it runs 313 lines at 50 Hz (a European Master System) rather than 262 at 60: what a frontend
    /// paces its frames by. The region alone does not say it, as a Game Gear is never PAL.
    pub fn pal(&self) -> bool {
        is_pal(self.hw.system, self.hw.region)
    }
    /// Video refresh rate from the Z80 clock and scanlines per frame.
    pub fn frame_hz(&self) -> f64 {
        let clock = if self.pal() {
            sound::PAL_CLOCK
        } else {
            sound::NTSC_CLOCK
        };
        f64::from(clock) / (f64::from(LINE_CYCLES) * f64::from(self.hw.vdp.lines()))
    }
    pub fn dimensions(&self) -> (usize, usize) {
        self.hw.vdp.dimensions()
    }
    /// The active picture, tightly packed, in RGB565 format.
    pub fn frame(&self) -> &[u16] {
        self.hw.vdp.frame()
    }
    /// Interleaved left/right signed 16-bit PCM at SAMPLE_HZ.
    pub fn audio(&self) -> &[i16] {
        self.hw.sound.audio()
    }
    /// The bytes a program has written to the SDSC debug console since the last call.
    pub fn take_console_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.hw.console)
    }

    fn run_cpu(&mut self, until: u32) {
        while self.hw.cycle < until {
            // The program's port accesses are stamped as their instruction starts and happen later in it, so
            // the interrupt reaches the Z80 that much (`vdp::IRQ_LATENCY`) after the VDP raises it.
            let up = self.hw.vdp.irq();
            if up && !self.hw.irq_up {
                self.hw.irq_wait = vdp::IRQ_LATENCY;
            }
            self.hw.irq_up = up;
            self.z80.irq = up && self.hw.irq_wait == 0;
            let cycles = self.z80.step(&mut self.hw);
            self.hw.cycle += cycles;
            self.hw.sound_debt += cycles;
            self.hw.irq_wait = self.hw.irq_wait.saturating_sub(cycles);
        }
    }

    pub fn run_frame(&mut self, pads: [u16; 2]) {
        self.hw.pads = pads;
        self.hw.sound.begin_frame();
        // The Pause button (and a Game Gear's Start in its Master System mode) is wired to the NMI pin: pressing it
        // is the interrupt, holding it is nothing.
        let pause = (self.hw.system == System::MasterSystem || self.hw.sms_mode)
            && (pads[0] | pads[1]) & START != 0;
        if pause && !self.pause_held {
            self.z80.nmi = true;
        }
        self.pause_held = pause;
        let lines = self.hw.vdp.lines();
        for line in 0..lines {
            self.hw.vdp.set_line(line);
            self.run_cpu(ACTIVE_END);
            self.hw.vdp.render_line();
            // The next line begins for the chip's counters and interrupts before this one is over.
            let next = if line + 1 == lines { 0 } else { line + 1 };
            // The chip's stages of the line beginning each have their cycle, in this order.
            self.run_cpu(vdp::FRAME_IRQ_AT);
            self.hw.vdp.start_of_line(next);
            self.run_cpu(vdp::LINE_IRQ_AT);
            self.hw.vdp.line_counter(next);
            self.run_cpu(vdp::STEP_AT);
            self.hw.vdp.line_flags(next);
            self.run_cpu(LINE_CYCLES);
            self.hw.cycle -= LINE_CYCLES;
            self.hw.sound_now();
        }
        self.hw.vdp.set_line(0);
        self.frames = self.frames.wrapping_add(1);
    }

    /// Cartridge RAM, once a game has switched it on.
    pub fn save_data(&self) -> Option<&[u8]> {
        self.hw.cart.ram_used.then_some(self.hw.cart.ram.as_slice())
    }
    pub fn load_save_data(&mut self, data: &[u8]) -> Result<(), Error> {
        if data.len() != self.hw.cart.ram.len() {
            return Err(Error::SaveSize);
        }
        self.hw.cart.ram.copy_from_slice(data);
        self.hw.cart.ram_used = true;
        Ok(())
    }

    /// The machine a state belongs to: the console (with its BIOS slot or Master System mode) and the region.
    fn kind(&self) -> [u8; 2] {
        let hw = &self.hw;
        [
            hw.system as u8 | u8::from(hw.bios) << 1 | u8::from(hw.sms_mode) << 2,
            hw.region as u8,
        ]
    }

    pub fn save_state(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(32 * 1024);
        out.extend_from_slice(b"SOLSM\x03");
        out.extend_from_slice(&self.kind());
        self.hw.cart.hash.save(&mut out);
        self.save(&mut out);
        out
    }

    /// Loading is transactional: corrupt or foreign states leave us intact.
    pub fn load_state(&mut self, data: &[u8]) -> Result<(), Error> {
        let mut input = data;
        if savestate::take(&mut input, 6)? != b"SOLSM\x03" {
            return Err(Error::BadState);
        }
        let kind = savestate::take(&mut input, 2)?;
        if kind != self.kind() {
            return Err(Error::BadState);
        }
        let mut hash = 0u64;
        hash.load(&mut input)?;
        if hash != self.hw.cart.hash {
            return Err(Error::OtherGame);
        }
        let mut candidate = self.clone();
        candidate.load(&mut input)?;
        if !input.is_empty()
            || candidate.hw.cycle >= 64
            || !candidate.z80.validate()
            || candidate.hw.irq_wait > vdp::IRQ_LATENCY
            || !candidate.hw.vdp.validate()
            || !candidate.hw.sound.validate()
            || !candidate.hw.cart.validate()
        {
            return Err(Error::BadState);
        }
        candidate.hw.sound.begin_frame();
        *self = candidate;
        Ok(())
    }

    /// The 8 KiB of work RAM, at $C000 (and again at $E000), for tools that read a program's results.
    pub fn ram(&self) -> &[u8] {
        &self.hw.ram[..]
    }

    /// The video chip's memories for tools: 16 KiB of VRAM, then the colour RAM as 32 little-endian words, then
    /// the 16 registers (11 to 15 always zero).
    pub fn video_memory(&self) -> Vec<u8> {
        self.hw.vdp.memory()
    }

    pub fn debug(&self) -> String {
        format!(
            "PC:{:04X} SP:{:04X} frame:{} {}x{} {:?} {:?}",
            self.z80.pc,
            self.z80.sp,
            self.frames,
            self.dimensions().0,
            self.dimensions().1,
            self.hw.system,
            self.hw.region
        )
    }
}
