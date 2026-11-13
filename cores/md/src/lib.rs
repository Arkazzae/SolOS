//! Sega Mega Drive / Genesis: a native Rust core with no host I/O.
//! A host supplies cartridge bytes and the two pads, then takes RGB565
//! frames, interleaved stereo PCM and portable Quick Resume states.
mod cart;
mod cpu;
mod io;
mod sound;
#[cfg(test)]
mod sst;
#[cfg(test)]
mod tests;
mod vdp;

use savestate::{BadState, State, state};

pub use sound::SAMPLE_HZ;
pub use vdp::{HEIGHT, WIDTH};

pub const UP: u16 = 1;
pub const DOWN: u16 = 2;
pub const LEFT: u16 = 4;
pub const RIGHT: u16 = 8;
pub const B: u16 = 16;
pub const C: u16 = 32;
pub const A: u16 = 64;
pub const START: u16 = 128;
pub const Z: u16 = 256;
pub const Y: u16 = 512;
pub const X: u16 = 1024;
pub const MODE: u16 = 2048;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Region {
    Japan,
    Usa,
    Europe,
}

impl Region {
    fn version(self) -> u8 {
        match self {
            Self::Japan => 0,
            Self::Usa => 0x80,
            Self::Europe => 0xc0,
        }
    }
    pub fn pal(self) -> bool {
        self == Self::Europe
    }
}

#[derive(Debug, PartialEq)]
pub enum Error {
    NotRom,
    Unsupported(&'static str),
    BadState,
    OtherGame,
    SaveSize,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotRom => f.write_str("not a supported Mega Drive cartridge image"),
            Self::Unsupported(kind) => write!(f, "not supported: {kind}"),
            Self::BadState => f.write_str("damaged or incompatible Mega Drive save state"),
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

#[derive(Clone)]
struct Hardware {
    cart: cart::Cart,
    ram: Box<[u8; 65536]>,
    zram: Box<[u8; 8192]>,
    vdp: vdp::Vdp,
    sound: sound::Sound,
    /// Master clocks that have passed since the sound chips were last run; see `sound_now`. Always zero
    /// between lines, so it is not part of the saved state.
    sound_debt: u32,
    io: io::Io,
    zbank: u32,
    z_busreq: bool,
    z_reset: bool,
    z_reset_pending: bool,
    dma_wait: u32,
    region: Region,
}

state!(Hardware {
    cart,
    ram,
    zram,
    vdp,
    sound,
    io,
    zbank,
    z_busreq,
    z_reset,
    z_reset_pending,
    dma_wait
});

impl Hardware {
    fn new(cart: cart::Cart) -> Self {
        let region = cart.region;
        Self {
            cart,
            region,
            ram: vec![0; 65536].into_boxed_slice().try_into().unwrap(),
            zram: vec![0; 8192].into_boxed_slice().try_into().unwrap(),
            vdp: vdp::Vdp::new(),
            sound: sound::Sound::new(),
            sound_debt: 0,
            io: io::Io::new(),
            zbank: 0,
            z_busreq: false,
            z_reset: true,
            z_reset_pending: true,
            dma_wait: 0,
        }
    }

    /// Runs the sound chips up to the present. `advance_devices` only adds the clocks that pass to
    /// `sound_debt`, and they go through the chips here, when something reads or changes them, and at the
    /// end of every line: the same samples and timer flags as advancing after every instruction (which
    /// was a quarter of an idle frame), because a chip never sees anything between two of those moments.
    fn sound_now(&mut self) -> &mut sound::Sound {
        if self.sound_debt != 0 {
            self.sound.advance(self.sound_debt, self.region.pal());
            self.sound_debt = 0;
        }
        &mut self.sound
    }

    fn zread(&mut self, addr: u16) -> u8 {
        match addr {
            0..=0x3fff => self.zram[usize::from(addr & 0x1fff)],
            0x4000..=0x5fff => self.sound_now().read_fm(),
            0x7f00..=0x7fff => self.read8(0xc00000 | u32::from(addr & 0x1f)),
            0x8000..=0xffff => {
                let mapped = self.zbank | u32::from(addr & 0x7fff);
                // A bank window onto itself must not recurse forever.
                if (0xa00000..=0xa0ffff).contains(&mapped) {
                    0xff
                } else {
                    self.read8(mapped)
                }
            }
            _ => 0xff,
        }
    }

    fn zwrite(&mut self, addr: u16, value: u8) {
        match addr {
            0..=0x3fff => self.zram[usize::from(addr & 0x1fff)] = value,
            0x4000..=0x5fff => self.sound_now().write_fm((addr & 3) as u8, value),
            0x6000..=0x60ff => {
                self.zbank = ((self.zbank >> 1) | (u32::from(value & 1) << 23)) & 0xff8000
            }
            0x7f00..=0x7fff => self.write8(0xc00000 | u32::from(addr & 0x1f), value),
            0x8000..=0xffff => {
                let mapped = self.zbank | u32::from(addr & 0x7fff);
                if !(0xa00000..=0xa0ffff).contains(&mapped) {
                    self.write8(mapped, value);
                }
            }
            _ => {}
        }
    }

    fn drain_dma(&mut self) {
        if let Some((source, words)) = self.vdp.dma_request() {
            self.dma_wait = self
                .dma_wait
                .saturating_add(self.vdp.dma_clocks(words))
                .min(28_016_640);
            // Length 0 means 65536 words on the VDP, not an unbounded job.
            for i in 0..words.min(65536) {
                let addr = (source & 0xfe0000) | ((source + i * 2) & 0x1ffff);
                let word = if !(0x400000..0xe00000).contains(&addr) {
                    self.read16(addr)
                } else {
                    0xffff
                };
                self.vdp.dma_word(word);
            }
        }
    }
}

use cpu::Bus as _;
impl cpu::Bus for Hardware {
    fn read8(&mut self, addr: u32) -> u8 {
        let addr = addr & 0xffffff;
        match addr {
            0..=0x3fffff => self.cart.read(addr),
            0xa00000..=0xa0ffff if self.z_busreq || self.z_reset => self.zread(addr as u16),
            0xa10000..=0xa1001f => self
                .io
                .read(((addr & 0x1f) >> 1) as usize, self.region.version()),
            0xa11100 => u8::from(!self.z_busreq && !self.z_reset),
            0xa11101 | 0xa11201 => 0,
            0xa11200 => u8::from(!self.z_reset),
            0xc00000..=0xdfffff if addr & 0xe700e0 == 0xc00000 => {
                let word = match addr & 0x1c {
                    0 => self.vdp.read_data(),
                    4 => self.vdp.read_status(),
                    8 | 12 => self.vdp.hv_counter(),
                    _ => 0xffff,
                };
                if addr & 1 == 0 {
                    (word >> 8) as u8
                } else {
                    word as u8
                }
            }
            0xe00000..=0xffffff => self.ram[addr as usize & 0xffff],
            _ => 0xff,
        }
    }

    fn read16(&mut self, addr: u32) -> u16 {
        let addr = addr & 0xfffffe;
        if addr & 0xe700e0 == 0xc00000 {
            return match addr & 0x1c {
                0 => self.vdp.read_data(),
                4 => self.vdp.read_status(),
                8 | 12 => self.vdp.hv_counter(),
                _ => 0xffff,
            };
        }
        u16::from_be_bytes([self.read8(addr), self.read8(addr + 1)])
    }

    fn write8(&mut self, addr: u32, value: u8) {
        let addr = addr & 0xffffff;
        match addr {
            0..=0x3fffff => self.cart.write(addr, value),
            0xa00000..=0xa0ffff if self.z_busreq || self.z_reset => self.zwrite(addr as u16, value),
            0xa10000..=0xa1001f => self.io.write(((addr & 0x1f) >> 1) as usize, value),
            0xa11100 => self.z_busreq = value & 1 != 0,
            0xa11200 => {
                let reset = value & 1 == 0;
                if reset && !self.z_reset {
                    self.z_reset_pending = true;
                    self.sound_now().reset_fm();
                }
                self.z_reset = reset;
            }
            0xa130f1..=0xa130ff => self.cart.mapper(addr, value),
            // TMSS accepts the game's SEGA write; no proprietary BIOS.
            0xa14000..=0xa14003 => {}
            0xc00000..=0xdfffff if addr & 0xe700e0 == 0xc00000 => match addr & 0x1c {
                0 => self.vdp.write_data(u16::from(value) * 0x101),
                4 => {
                    self.vdp.write_control(u16::from(value) * 0x101);
                    self.drain_dma();
                }
                0x10 | 0x14 if addr & 1 != 0 => self.sound_now().write_psg(value),
                _ => {}
            },
            0xe00000..=0xffffff => self.ram[addr as usize & 0xffff] = value,
            _ => {}
        }
    }

    fn write16(&mut self, addr: u32, value: u16) {
        let addr = addr & 0xfffffe;
        if addr & 0xe700e0 == 0xc00000 {
            match addr & 0x1c {
                0 => self.vdp.write_data(value),
                4 => {
                    self.vdp.write_control(value);
                    self.drain_dma();
                }
                0x10 | 0x14 => self.sound_now().write_psg(value as u8),
                _ => {}
            }
        } else {
            let [high, low] = value.to_be_bytes();
            self.write8(addr, high);
            self.write8(addr + 1, low);
        }
    }

    fn reset_devices(&mut self) {
        self.sound_now().reset();
        self.z_reset = true;
        self.z_reset_pending = true;
    }
}

struct SoundBus<'a>(&'a mut Hardware);
impl z80::Bus for SoundBus<'_> {
    fn read(&mut self, addr: u16) -> u8 {
        self.0.zread(addr)
    }
    fn write(&mut self, addr: u16, value: u8) {
        self.0.zwrite(addr, value);
    }
}

#[derive(Clone)]
pub struct Md {
    cpu: cpu::Cpu,
    z80: z80::Z80,
    hw: Hardware,
    master_debt: u32,
    z_credit: i32,
    frames: u64,
}
state!(Md {
    cpu,
    z80,
    hw,
    master_debt,
    z_credit,
    frames
});

impl Md {
    pub fn new(rom: &[u8]) -> Result<Self, Error> {
        let hw = Hardware::new(cart::Cart::parse(rom)?);
        let mut md = Self {
            cpu: cpu::Cpu::new(),
            z80: z80::Z80::new(),
            hw,
            master_debt: 0,
            z_credit: 0,
            frames: 0,
        };
        md.cpu.reset(&mut md.hw);
        Ok(md)
    }

    pub fn reset(&mut self) {
        self.cpu.reset(&mut self.hw);
        self.z80.reset();
        self.hw.sound_now().reset();
        self.hw.vdp = vdp::Vdp::new();
        self.hw.io = io::Io::new();
        self.hw.z_reset = true;
        self.hw.z_reset_pending = false;
        self.hw.z_busreq = false;
        self.hw.zbank = 0;
        self.hw.dma_wait = 0;
        self.master_debt = 0;
        self.z_credit = 0;
    }

    pub fn region(&self) -> Region {
        self.hw.region
    }
    pub fn title(&self) -> &str {
        &self.hw.cart.title
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

    fn advance_devices(&mut self, master: u32) {
        if self.hw.z_reset_pending {
            self.z80.reset();
            self.hw.z_reset_pending = false;
        }
        if self.hw.z_reset || self.hw.z_busreq {
            self.z_credit = 0;
        } else {
            self.z_credit += master as i32;
            while self.z_credit > 0 {
                let clocks = self.z80.step(&mut SoundBus(&mut self.hw)).clamp(1, 4096);
                self.z_credit -= clocks as i32 * 15;
            }
        }
        self.hw.sound_debt = self.hw.sound_debt.saturating_add(master);
        self.hw.io.advance(master);
    }

    pub fn run_frame(&mut self, pads: [u16; 2]) {
        self.hw.io.pads = pads;
        self.hw.sound.begin_frame();
        let pal = self.hw.region.pal();
        let lines = if pal { 313 } else { 262 };
        for line in 0..lines {
            self.hw.vdp.start_line(line, pal);
            self.hw.vdp.render_line();
            self.z80.irq = line as usize == self.hw.vdp.dimensions().1;
            let mut elapsed = self.master_debt;
            while elapsed < 3420 {
                self.hw.vdp.set_line_cycles((elapsed / 7) as u16);
                if self.hw.dma_wait != 0 {
                    let master = self.hw.dma_wait.min(3420 - elapsed);
                    self.hw.dma_wait -= master;
                    elapsed += master;
                    self.advance_devices(master);
                    continue;
                }
                let level = self.hw.vdp.irq_level();
                let mut cycles = if level != 0 {
                    self.cpu.interrupt(&mut self.hw, level)
                } else {
                    0
                };
                if cycles != 0 {
                    self.hw.vdp.ack_irq(level);
                } else {
                    cycles = self.cpu.step(&mut self.hw);
                }
                let master = cycles.clamp(1, 4096) * 7;
                elapsed += master;
                self.advance_devices(master);
            }
            self.master_debt = elapsed - 3420;
            self.hw.sound_now();
        }
        self.frames = self.frames.wrapping_add(1);
    }

    pub fn save_data(&self) -> Option<&[u8]> {
        (!self.hw.cart.ram.is_empty()).then_some(self.hw.cart.ram.as_slice())
    }
    pub fn load_save_data(&mut self, data: &[u8]) -> Result<(), Error> {
        if data.len() != self.hw.cart.ram.len() {
            return Err(Error::SaveSize);
        }
        self.hw.cart.ram.copy_from_slice(data);
        Ok(())
    }

    pub fn save_state(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(160 * 1024);
        out.extend_from_slice(b"SOLMD\x01");
        self.hw.cart.hash.save(&mut out);
        self.save(&mut out);
        out
    }
    /// Loading is transactional: corrupt or foreign states leave us intact.
    pub fn load_state(&mut self, data: &[u8]) -> Result<(), Error> {
        let mut input = data;
        if savestate::take(&mut input, 6)? != b"SOLMD\x01" {
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
            || candidate.master_debt > 28672
            || !(-65536..=65536).contains(&candidate.z_credit)
            || candidate.hw.zbank & !0xff8000 != 0
            || candidate.hw.dma_wait > 28_016_640
            || !candidate.hw.vdp.validate()
            || !candidate.hw.sound.validate(candidate.hw.region.pal())
            || !candidate.z80.validate()
        {
            return Err(Error::BadState);
        }
        candidate.hw.sound.begin_frame();
        *self = candidate;
        Ok(())
    }

    pub fn debug(&self) -> String {
        format!(
            "PC:{:06X} SR:{:04X} SP:{:08X} frame:{} {}x{} {:?}",
            self.cpu.pc,
            self.cpu.sr,
            self.cpu.a[7],
            self.frames,
            self.dimensions().0,
            self.dimensions().1,
            self.region()
        )
    }
}
