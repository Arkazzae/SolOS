//! The cartridge: an iNES or NES 2.0 image, its PRG (program) and CHR
//! (pattern) memory, and the mapper that pages them into the CPU's and the
//! PPU's address spaces. Mappers here: NROM (0), MMC1 (1), UxROM (2),
//! CNROM (3), MMC3 (4) and AxROM (7), most of the library.

use std::fmt;

use savestate::{BadState, State, state, take};

#[derive(Debug, PartialEq)]
pub enum Error {
    /// Not an iNES image.
    NotInes,
    /// Shorter than its header says.
    Truncated,
    /// A mapper this core does not have.
    Mapper(u16),
    /// A save state that is damaged, or from another version of the core.
    BadState,
    /// A save state from another game.
    OtherGame,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::NotInes => write!(f, "not an iNES image"),
            Error::Truncated => write!(f, "the image is shorter than its header says"),
            Error::Mapper(m) => write!(f, "mapper {m} is not supported"),
            Error::BadState => write!(f, "the save state is damaged, or from another version"),
            Error::OtherGame => write!(f, "the save state is from another game"),
        }
    }
}

impl std::error::Error for Error {}

impl From<BadState> for Error {
    fn from(_: BadState) -> Error {
        Error::BadState
    }
}

/// How the PPU's four nametables map onto its 2 KB of video RAM.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mirroring {
    Horizontal,
    Vertical,
    /// All four are the first, or the second, kilobyte.
    SingleLow,
    SingleHigh,
    /// The cartridge brings 2 KB more, for four separate nametables.
    FourScreen,
}

/// MMC1's registers, written a bit at a time through a shift register.
#[derive(Clone, Copy, Debug, Default)]
struct Mmc1 {
    shift: u8,
    count: u8,
    control: u8,
    chr0: u8,
    chr1: u8,
    prg: u8,
    /// MMC1 takes only the first of writes on consecutive CPU cycles (the
    /// two writes of a read-modify-write instruction).
    last_write: u64,
}

/// MMC3's registers and its scanline counter, which counts rises of the
/// PPU's address line A12 (once a line, as sprites are fetched from the
/// pattern table at $1000 and the background from $0000).
#[derive(Clone, Copy, Debug, Default)]
struct Mmc3 {
    select: u8,
    banks: [u8; 8],
    ram_protect: u8,
    irq_latch: u8,
    irq_counter: u8,
    irq_reload: bool,
    irq_enabled: bool,
    irq: bool,
    /// A12 as last seen, and the PPU dot it went low.
    a12: bool,
    a12_low_since: u32,
}

state!(Mmc1 { shift, count, control, chr0, chr1, prg, last_write });
state!(Mmc3 { select, banks, ram_protect, irq_latch, irq_counter, irq_reload, irq_enabled, irq, a12, a12_low_since });

#[derive(Clone, Copy, Debug)]
enum Mapper {
    /// No paging; 16 KB of PRG appear twice.
    Nrom,
    Mmc1(Mmc1),
    /// A switchable 16 KB PRG bank, then the last one.
    Uxrom(u8),
    /// A switchable 8 KB CHR bank.
    Cnrom(u8),
    Mmc3(Mmc3),
    /// A switchable 32 KB PRG bank, and one-screen mirroring.
    Axrom(u8),
}

#[derive(Clone)]
pub struct Cart {
    /// FNV-1a of the PRG and CHR ROM, which save states carry.
    pub hash: u64,
    prg: Vec<u8>,
    chr: Vec<u8>,
    /// No CHR ROM: 8 KB of CHR RAM instead, which the game writes.
    chr_ram: bool,
    /// Work RAM at $6000-$7FFF, battery-backed on some cartridges.
    prg_ram: Vec<u8>,
    battery: bool,
    pub mirroring: Mirroring,
    /// Four-screen boards ignore the mapper's mirroring.
    four_screen: bool,
    mapper: Mapper,
    /// Where in PRG each 8 KB window at $8000-$FFFF starts, and in CHR each
    /// 1 KB window of $0000-$1FFF, worked out when the mapper changes them.
    prg_map: [usize; 4],
    chr_map: [usize; 8],
    /// The mapper's IRQ line, kept here as the CPU looks at it every cycle.
    irq: bool,
}

impl Cart {
    pub fn parse(rom: &[u8]) -> Result<Cart, Error> {
        if rom.len() < 16 || &rom[..4] != b"NES\x1a" {
            return Err(Error::NotInes);
        }
        let (flags6, flags7) = (rom[6], rom[7]);
        let nes2 = flags7 & 0x0c == 0x08;
        let mut mapper = u16::from(flags6 >> 4);
        let (mut prg_units, mut chr_units) = (usize::from(rom[4]), usize::from(rom[5]));
        if nes2 {
            mapper |= u16::from(flags7 & 0xf0) | (u16::from(rom[8] & 0x0f) << 8);
            prg_units |= usize::from(rom[9] & 0x0f) << 8;
            chr_units |= usize::from(rom[9] >> 4) << 8;
        } else if rom[12..16] == [0; 4] {
            // Old dumps carry junk ("DiskDude!") from byte 7 on; only trust
            // the high mapper nibble when the header's tail is clean.
            mapper |= u16::from(flags7 & 0xf0);
        }
        let start = 16 + if flags6 & 0x04 != 0 { 512 } else { 0 };
        let (prg_len, chr_len) = (prg_units * 0x4000, chr_units * 0x2000);
        if rom.len() < start + prg_len + chr_len || prg_len == 0 {
            return Err(Error::Truncated);
        }
        let mapper = match mapper {
            0 => Mapper::Nrom,
            // Powers on with the last PRG bank fixed at $C000.
            1 => Mapper::Mmc1(Mmc1 { control: 0x0c, ..Mmc1::default() }),
            2 => Mapper::Uxrom(0),
            3 => Mapper::Cnrom(0),
            // Its work RAM on, and writable, as most games expect.
            4 => Mapper::Mmc3(Mmc3 { ram_protect: 0x80, ..Mmc3::default() }),
            7 => Mapper::Axrom(0),
            m => return Err(Error::Mapper(m)),
        };
        let four_screen = flags6 & 0x08 != 0;
        let mirroring = if four_screen {
            Mirroring::FourScreen
        } else if flags6 & 0x01 != 0 {
            Mirroring::Vertical
        } else {
            Mirroring::Horizontal
        };
        let chr = if chr_len > 0 { rom[start + prg_len..start + prg_len + chr_len].to_vec() } else { vec![0; 0x2000] };
        let hash = rom[start..start + prg_len + chr_len]
            .iter()
            .fold(0xcbf2_9ce4_8422_2325u64, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3));
        let mut cart = Cart {
            hash,
            prg: rom[start..start + prg_len].to_vec(),
            chr,
            chr_ram: chr_len == 0,
            prg_ram: vec![0; 0x2000],
            battery: flags6 & 0x02 != 0,
            mirroring,
            four_screen,
            mapper,
            prg_map: [0; 4],
            chr_map: [0; 8],
            irq: false,
        };
        cart.map();
        Ok(cart)
    }

    /// Works out where the windows point, from the mapper's registers.
    fn map(&mut self) {
        for window in 0..4 {
            self.prg_map[window] = self.prg_bank(window) * 0x2000 % self.prg.len();
        }
        for window in 0..8 {
            self.chr_map[window] = self.chr_bank(window) * 0x400 % self.chr.len();
        }
    }

    /// The 8 KB PRG bank in one of the four windows at $8000, $A000,
    /// $C000 and $E000.
    fn prg_bank(&self, window: usize) -> usize {
        let banks = self.prg.len() / 0x2000;
        match self.mapper {
            Mapper::Nrom => window,
            Mapper::Mmc1(m) => {
                // 512 KB boards (SUROM) take the top PRG line from CHR bank 0.
                let outer = if self.prg.len() > 0x40000 { usize::from(m.chr0 & 0x10) } else { 0 };
                let bank = outer | usize::from(m.prg & 0x0f);
                let bank16 = match (m.control >> 2 & 3, window / 2) {
                    (0 | 1, half) => bank & !1 | half,
                    (2, 0) => outer,
                    (2, _) => bank,
                    (_, 0) => bank,
                    (_, _) => outer | 0x0f,
                };
                bank16 * 2 + window % 2
            }
            Mapper::Uxrom(bank) => match window {
                0 | 1 => usize::from(bank) * 2 + window,
                _ => banks - 4 + window,
            },
            Mapper::Cnrom(_) => window,
            Mapper::Mmc3(m) => {
                let r6 = usize::from(m.banks[6]);
                let r7 = usize::from(m.banks[7]);
                match (m.select & 0x40 != 0, window) {
                    (false, 0) | (true, 2) => r6,
                    (_, 1) => r7,
                    (_, 3) => banks - 1,
                    _ => banks - 2,
                }
            }
            Mapper::Axrom(bank) => usize::from(bank & 7) * 4 + window,
        }
    }

    /// The 1 KB CHR bank in one of the eight windows of $0000-$1FFF.
    fn chr_bank(&self, window: usize) -> usize {
        match self.mapper {
            Mapper::Mmc1(m) if m.control & 0x10 != 0 => {
                let bank = if window < 4 { m.chr0 } else { m.chr1 };
                usize::from(bank) * 4 + window % 4
            }
            Mapper::Mmc1(m) => usize::from(m.chr0 & !1) * 4 + window,
            Mapper::Cnrom(bank) => usize::from(bank) * 8 + window,
            Mapper::Mmc3(m) => {
                // Two 2 KB banks and four 1 KB ones, the halves swapped by bit 7.
                let w = if m.select & 0x80 != 0 { window ^ 4 } else { window };
                match w {
                    0..=3 => usize::from(m.banks[w / 2] & !1) + w % 2,
                    _ => usize::from(m.banks[w - 2]),
                }
            }
            _ => window,
        }
    }

    /// A CPU read from $4020-$FFFF; None where nothing answers (open bus).
    pub fn cpu_read(&self, addr: u16) -> Option<u8> {
        match addr {
            0x6000..=0x7fff if self.prg_ram_enabled() => Some(self.prg_ram[usize::from(addr - 0x6000)]),
            0x8000.. => Some(self.prg[self.prg_map[usize::from(addr >> 13 & 3)] + usize::from(addr & 0x1fff)]),
            _ => None,
        }
    }

    fn prg_ram_enabled(&self) -> bool {
        match self.mapper {
            Mapper::Mmc1(m) => m.prg & 0x10 == 0,
            _ => true,
        }
    }

    /// A CPU write to $4020-$FFFF, on CPU cycle `cycle`.
    pub fn cpu_write(&mut self, addr: u16, value: u8, cycle: u64) {
        if let 0x6000..=0x7fff = addr {
            let writable = match self.mapper {
                Mapper::Mmc1(m) => m.prg & 0x10 == 0,
                Mapper::Mmc3(m) => m.ram_protect & 0xc0 == 0x80,
                _ => true,
            };
            if writable {
                self.prg_ram[usize::from(addr - 0x6000)] = value;
            }
            return;
        }
        if addr < 0x8000 {
            return;
        }
        match &mut self.mapper {
            Mapper::Nrom => {}
            Mapper::Mmc1(m) => {
                let consecutive = cycle == m.last_write + 1;
                m.last_write = cycle;
                if consecutive {
                    return;
                }
                if value & 0x80 != 0 {
                    m.shift = 0;
                    m.count = 0;
                    m.control |= 0x0c;
                } else {
                    m.shift |= (value & 1) << m.count;
                    m.count += 1;
                    if m.count == 5 {
                        match addr >> 13 & 3 {
                            0 => m.control = m.shift,
                            1 => m.chr0 = m.shift,
                            2 => m.chr1 = m.shift,
                            _ => m.prg = m.shift,
                        }
                        m.shift = 0;
                        m.count = 0;
                    }
                }
                if !self.four_screen {
                    self.mirroring = match m.control & 3 {
                        0 => Mirroring::SingleLow,
                        1 => Mirroring::SingleHigh,
                        2 => Mirroring::Vertical,
                        _ => Mirroring::Horizontal,
                    };
                }
            }
            Mapper::Uxrom(bank) | Mapper::Cnrom(bank) => *bank = value,
            Mapper::Mmc3(m) => match (addr >> 13 & 3, addr & 1) {
                (0, 0) => m.select = value,
                (0, _) => m.banks[usize::from(m.select & 7)] = value,
                (1, 0) => {
                    if !self.four_screen {
                        self.mirroring = if value & 1 != 0 { Mirroring::Horizontal } else { Mirroring::Vertical };
                    }
                }
                (1, _) => m.ram_protect = value,
                (2, 0) => m.irq_latch = value,
                (2, _) => {
                    m.irq_counter = 0;
                    m.irq_reload = true;
                }
                (_, 0) => {
                    m.irq_enabled = false;
                    m.irq = false;
                    self.irq = false;
                }
                (_, _) => m.irq_enabled = true,
            },
            Mapper::Axrom(bank) => {
                *bank = value;
                self.mirroring = if value & 0x10 != 0 { Mirroring::SingleHigh } else { Mirroring::SingleLow };
            }
        }
        self.map();
    }

    /// A PPU read from the pattern tables, $0000-$1FFF.
    pub fn chr_read(&self, addr: u16) -> u8 {
        self.chr[self.chr_map[usize::from(addr >> 10 & 7)] + usize::from(addr & 0x3ff)]
    }

    pub fn chr_write(&mut self, addr: u16, value: u8) {
        if self.chr_ram {
            self.chr[self.chr_map[usize::from(addr >> 10 & 7)] + usize::from(addr & 0x3ff)] = value;
        }
    }

    /// The PPU put addr on its address bus at PPU dot `clock`. MMC3 clocks
    /// its counter when A12 rises after staying low for a while (it filters
    /// the short drops between sprite fetches).
    pub fn watch(&mut self, addr: u16, clock: u32) {
        let Mapper::Mmc3(m) = &mut self.mapper else {
            return;
        };
        let a12 = addr & 0x1000 != 0;
        if a12 && !m.a12 && clock.wrapping_sub(m.a12_low_since) >= 10 {
            if m.irq_counter == 0 || m.irq_reload {
                m.irq_counter = m.irq_latch;
                m.irq_reload = false;
            } else {
                m.irq_counter -= 1;
            }
            if m.irq_counter == 0 && m.irq_enabled {
                m.irq = true;
                self.irq = true;
            }
        }
        if !a12 && m.a12 {
            m.a12_low_since = clock;
        }
        m.a12 = a12;
    }

    /// Work RAM kept by a battery (game saves), if the board has one.
    pub fn battery_ram(&self) -> Option<&[u8]> {
        self.battery.then_some(&self.prg_ram[..])
    }

    pub fn load_battery_ram(&mut self, data: &[u8]) {
        let n = data.len().min(self.prg_ram.len());
        self.prg_ram[..n].copy_from_slice(&data[..n]);
    }

    /// Whether the mapper counts A12's rises (MMC3), so the PPU must not
    /// run ahead of the CPU over them.
    pub fn watches_a12(&self) -> bool {
        matches!(self.mapper, Mapper::Mmc3(_))
    }

    /// The cartridge's IRQ line.
    pub fn irq(&self) -> bool {
        self.irq
    }
}

impl State for Mirroring {
    fn save(&self, out: &mut Vec<u8>) {
        out.push(*self as u8);
    }

    fn load(&mut self, input: &mut &[u8]) -> Result<(), BadState> {
        *self = match take(input, 1)?[0] {
            0 => Mirroring::Horizontal,
            1 => Mirroring::Vertical,
            2 => Mirroring::SingleLow,
            3 => Mirroring::SingleHigh,
            4 => Mirroring::FourScreen,
            _ => return Err(BadState),
        };
        Ok(())
    }
}

/// The mapper's registers, after its number, which must be this board's.
impl State for Mapper {
    fn save(&self, out: &mut Vec<u8>) {
        match self {
            Mapper::Nrom => out.push(0),
            Mapper::Mmc1(m) => {
                out.push(1);
                m.save(out);
            }
            Mapper::Uxrom(bank) => {
                out.push(2);
                bank.save(out);
            }
            Mapper::Cnrom(bank) => {
                out.push(3);
                bank.save(out);
            }
            Mapper::Mmc3(m) => {
                out.push(4);
                m.save(out);
            }
            Mapper::Axrom(bank) => {
                out.push(7);
                bank.save(out);
            }
        }
    }

    fn load(&mut self, input: &mut &[u8]) -> Result<(), BadState> {
        match (self, take(input, 1)?[0]) {
            (Mapper::Nrom, 0) => Ok(()),
            (Mapper::Mmc1(m), 1) => m.load(input),
            (Mapper::Mmc3(m), 4) => m.load(input),
            (Mapper::Uxrom(bank), 2) | (Mapper::Cnrom(bank), 3) | (Mapper::Axrom(bank), 7) => bank.load(input),
            _ => Err(BadState),
        }
    }
}

/// What the cartridge holds that changes: its RAM and the mapper's state.
impl State for Cart {
    fn save(&self, out: &mut Vec<u8>) {
        self.prg_ram.save(out);
        if self.chr_ram {
            self.chr.save(out);
        }
        self.mirroring.save(out);
        self.mapper.save(out);
    }

    fn load(&mut self, input: &mut &[u8]) -> Result<(), BadState> {
        self.prg_ram.load(input)?;
        if self.chr_ram {
            self.chr.load(input)?;
        }
        self.mirroring.load(input)?;
        self.mapper.load(input)?;
        self.map();
        self.irq = matches!(self.mapper, Mapper::Mmc3(m) if m.irq);
        Ok(())
    }
}
