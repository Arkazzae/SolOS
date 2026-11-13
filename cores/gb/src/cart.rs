//! The cartridge: its ROM, its RAM (battery-backed on some), and the memory
//! bank controller that pages them in. Controllers here: none (32 KB),
//! MBC1, MBC2, MBC3 (with its real-time clock) and MBC5, which cover nearly
//! all Game Boy games.

use std::fmt;

use savestate::{BadState, State, state, take};

use crate::rtc::Rtc;

#[derive(Debug, PartialEq)]
pub enum Error {
    /// Too short for a Game Boy ROM, or its size does not fit its header.
    NotRom,
    /// A memory bank controller this core does not have.
    Mbc(u8),
    /// A game for the Game Boy Color only.
    ColorOnly,
    /// A save state that is damaged, or from another version of the core.
    BadState,
    /// A save state from another game.
    OtherGame,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::NotRom => write!(f, "not a Game Boy ROM"),
            Error::Mbc(t) => write!(f, "cartridge type {t:#04x} is not supported"),
            Error::ColorOnly => write!(f, "a Game Boy Color game"),
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

#[derive(Clone, Copy, Debug, PartialEq)]
enum Mbc {
    None,
    /// The 5-bit and 2-bit bank registers, and the mode that says what the
    /// 2-bit one pages.
    Mbc1 { low: u8, high: u8, mode: bool },
    /// 16 ROM banks, and 512 half-bytes of RAM inside the controller.
    Mbc2 { bank: u8 },
    /// 128 ROM banks (256 on the MBC30), 4 RAM banks or a clock register
    /// at A000.
    Mbc3 { rom: u8, ram: u8 },
    /// 512 ROM banks (9 bits), 16 RAM banks.
    Mbc5 { rom: u16, ram: u8 },
}

#[derive(Clone)]
pub struct Cart {
    /// The ROM's hash, which save states carry.
    pub hash: u64,
    rom: Vec<u8>,
    ram: Vec<u8>,
    battery: bool,
    mbc: Mbc,
    rtc: Option<Rtc>,
    ram_enabled: bool,
    /// An MBC5 cartridge with a motor (types 1C to 1E): bit 3 of the RAM
    /// bank register runs it, the others page the RAM. Whether it runs is
    /// for the host to feel, not saved.
    rumble: bool,
    motor: bool,
}

impl Cart {
    pub fn parse(rom: &[u8]) -> Result<Cart, Error> {
        if rom.len() < 0x8000 || rom.len() % 0x4000 != 0 {
            return Err(Error::NotRom);
        }
        let kind = rom[0x147];
        let (mbc, battery) = match kind {
            0x00 => (Mbc::None, false),
            0x08 => (Mbc::None, false),
            0x09 => (Mbc::None, true),
            0x01 | 0x02 => (Mbc::Mbc1 { low: 1, high: 0, mode: false }, false),
            0x03 => (Mbc::Mbc1 { low: 1, high: 0, mode: false }, true),
            0x05 => (Mbc::Mbc2 { bank: 1 }, false),
            0x06 => (Mbc::Mbc2 { bank: 1 }, true),
            0x11 | 0x12 => (Mbc::Mbc3 { rom: 1, ram: 0 }, false),
            0x0f | 0x10 | 0x13 => (Mbc::Mbc3 { rom: 1, ram: 0 }, true),
            0x19 | 0x1a | 0x1c | 0x1d => (Mbc::Mbc5 { rom: 1, ram: 0 }, false),
            0x1b | 0x1e => (Mbc::Mbc5 { rom: 1, ram: 0 }, true),
            t => return Err(Error::Mbc(t)),
        };
        let ram_size = match (mbc, rom[0x149]) {
            (Mbc::Mbc2 { .. }, _) => 512,
            (_, 2) => 0x2000,
            (_, 3) => 0x8000,
            (_, 4) => 0x20000,
            (_, 5) => 0x10000,
            _ => 0,
        };
        Ok(Cart {
            hash: rom.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)),
            rom: rom.to_vec(),
            ram: vec![0; ram_size],
            battery,
            mbc,
            rtc: matches!(kind, 0x0f | 0x10).then(Rtc::default),
            ram_enabled: false,
            rumble: matches!(kind, 0x1c..=0x1e),
            motor: false,
        })
    }

    /// The cartridge's RAM as a whole, every bank one after the other and whatever the game has switched
    /// to; empty for a cartridge with none. The clock's registers are not in it.
    pub fn ram(&self) -> &[u8] {
        &self.ram
    }

    /// Whether the cartridge's motor runs.
    pub fn motor(&self) -> bool {
        self.motor
    }

    /// The ROM bank in 0000-3FFF (0, but for MBC1's large-ROM mode) and in
    /// 4000-7FFF.
    fn rom_banks(&self) -> (usize, usize) {
        match self.mbc {
            Mbc::None => (0, 1),
            Mbc::Mbc1 { low, high, mode } => {
                let low = if low == 0 { 1 } else { usize::from(low) };
                let high = usize::from(high) << 5;
                (if mode { high } else { 0 }, high | low)
            }
            Mbc::Mbc2 { bank } => (0, usize::from(bank.max(1))),
            Mbc::Mbc3 { rom, .. } => (0, usize::from(rom.max(1))),
            Mbc::Mbc5 { rom, .. } => (0, usize::from(rom)),
        }
    }

    fn ram_offset(&self, addr: u16) -> usize {
        let bank = match self.mbc {
            Mbc::Mbc1 { high, mode: true, .. } => usize::from(high),
            Mbc::Mbc3 { ram, .. } | Mbc::Mbc5 { ram, .. } => usize::from(ram),
            _ => 0,
        };
        (bank * 0x2000 + usize::from(addr & 0x1fff)) % self.ram.len().max(1)
    }

    /// A read from ROM (0000-7FFF).
    pub fn read_rom(&self, addr: u16) -> u8 {
        let (low, high) = self.rom_banks();
        let bank = if addr < 0x4000 { low } else { high };
        self.rom[(bank * 0x4000 + usize::from(addr & 0x3fff)) % self.rom.len()]
    }

    /// A read from cartridge RAM (A000-BFFF), or the MBC3's clock; 0xFF
    /// while they are off.
    pub fn read_ram(&self, addr: u16) -> u8 {
        if !self.ram_enabled {
            return 0xff;
        }
        match self.mbc {
            Mbc::Mbc3 { ram: register @ 0x08..=0x0c, .. } => self.rtc.map_or(0xff, |rtc| rtc.read(register)),
            _ if self.ram.is_empty() => 0xff,
            // Half-bytes, the top four bits unused; 512 of them repeat.
            Mbc::Mbc2 { .. } => self.ram[usize::from(addr & 0x1ff)] | 0xf0,
            _ => self.ram[self.ram_offset(addr)],
        }
    }

    /// A write to cartridge RAM or the clock, at the given time (in clocks).
    pub fn write_ram(&mut self, addr: u16, value: u8, now: u64) {
        if !self.ram_enabled {
            return;
        }
        match self.mbc {
            Mbc::Mbc3 { ram: register @ 0x08..=0x0c, .. } => {
                if let Some(rtc) = &mut self.rtc {
                    rtc.write(register, value, now);
                }
            }
            _ if self.ram.is_empty() => {}
            Mbc::Mbc2 { .. } => self.ram[usize::from(addr & 0x1ff)] = value & 0x0f,
            _ => {
                let i = self.ram_offset(addr);
                self.ram[i] = value;
            }
        }
    }

    /// A write to ROM space: the controller's registers.
    pub fn write_rom(&mut self, addr: u16, value: u8, now: u64) {
        match &mut self.mbc {
            Mbc::None => {}
            Mbc::Mbc1 { low, high, mode } => match addr {
                0x0000..=0x1fff => self.ram_enabled = value & 0x0f == 0x0a,
                0x2000..=0x3fff => *low = value & 0x1f,
                0x4000..=0x5fff => *high = value & 3,
                _ => *mode = value & 1 != 0,
            },
            // Bit 8 of the address picks the register.
            Mbc::Mbc2 { bank } => match addr {
                0x0000..=0x3fff if addr & 0x100 == 0 => self.ram_enabled = value & 0x0f == 0x0a,
                0x0000..=0x3fff => *bank = value & 0x0f,
                _ => {}
            },
            Mbc::Mbc3 { rom, ram } => match addr {
                0x0000..=0x1fff => self.ram_enabled = value & 0x0f == 0x0a,
                0x2000..=0x3fff => *rom = value,
                0x4000..=0x5fff => *ram = value,
                _ => {
                    if let Some(rtc) = &mut self.rtc {
                        rtc.write_latch(value, now);
                    }
                }
            },
            Mbc::Mbc5 { rom, ram } => match addr {
                0x0000..=0x1fff => self.ram_enabled = value & 0x0f == 0x0a,
                0x2000..=0x2fff => *rom = *rom & 0x100 | u16::from(value),
                0x3000..=0x3fff => *rom = *rom & 0xff | u16::from(value & 1) << 8,
                0x4000..=0x5fff if self.rumble => {
                    *ram = value & 0x07;
                    self.motor = value & 0x08 != 0;
                }
                0x4000..=0x5fff => *ram = value & 0x0f,
                _ => {}
            },
        }
    }

    /// What the battery keeps (game saves): the RAM, then the clock's
    /// registers and the time of the save, if the cartridge has them.
    pub fn save_data(&self, unix_time: u64) -> Option<Vec<u8>> {
        if !self.battery || (self.ram.is_empty() && self.rtc.is_none()) {
            return None;
        }
        let mut out = self.ram.clone();
        if let Some(rtc) = &self.rtc {
            rtc.save(unix_time, &mut out);
        }
        Some(out)
    }

    pub fn load_save_data(&mut self, data: &[u8], unix_time: u64) {
        let n = data.len().min(self.ram.len());
        self.ram[..n].copy_from_slice(&data[..n]);
        if let Some(rtc) = &mut self.rtc
            && data.len() >= n + 44
        {
            rtc.load(&data[n..], unix_time);
        }
    }

    /// Moves the clock on by time the game did not run.
    pub fn pass_time(&mut self, seconds: u64) {
        if let Some(rtc) = &mut self.rtc {
            rtc.pass(seconds);
        }
    }
}

/// The controller's registers, after its kind, which must be this
/// cartridge's.
impl State for Mbc {
    fn save(&self, out: &mut Vec<u8>) {
        match *self {
            Mbc::None => out.push(0),
            Mbc::Mbc1 { low, high, mode } => {
                out.push(1);
                (low, high).save(out);
                mode.save(out);
            }
            Mbc::Mbc2 { bank } => {
                out.push(2);
                bank.save(out);
            }
            Mbc::Mbc3 { rom, ram } => {
                out.push(3);
                (rom, ram).save(out);
            }
            Mbc::Mbc5 { rom, ram } => {
                out.push(5);
                (rom, ram).save(out);
            }
        }
    }

    fn load(&mut self, input: &mut &[u8]) -> Result<(), BadState> {
        match (self, take(input, 1)?[0]) {
            (Mbc::None, 0) => Ok(()),
            (Mbc::Mbc1 { low, high, mode }, 1) => {
                low.load(input)?;
                high.load(input)?;
                mode.load(input)
            }
            (Mbc::Mbc2 { bank }, 2) => bank.load(input),
            (Mbc::Mbc3 { rom, ram }, 3) => {
                rom.load(input)?;
                ram.load(input)
            }
            (Mbc::Mbc5 { rom, ram }, 5) => {
                rom.load(input)?;
                ram.load(input)
            }
            _ => Err(BadState),
        }
    }
}

state!(Cart { ram, mbc, rtc, ram_enabled });

#[cfg(test)]
mod tests {
    use super::*;

    /// A rumble cartridge: bit 3 of the RAM bank register runs its motor,
    /// the bits under it page the RAM; an MBC5 without a motor pages with
    /// all four.
    #[test]
    fn a_rumble_cartridge() {
        let mut rom = vec![0u8; 0x8000];
        (rom[0x147], rom[0x149]) = (0x1e, 3);
        let mut cart = Cart::parse(&rom).unwrap();
        cart.write_rom(0x4000, 0x0b, 0);
        assert!(cart.motor());
        assert_eq!(cart.mbc, Mbc::Mbc5 { rom: 1, ram: 3 });
        cart.write_rom(0x4000, 0x03, 0);
        assert!(!cart.motor());
        rom[0x147] = 0x1b;
        let mut cart = Cart::parse(&rom).unwrap();
        cart.write_rom(0x4000, 0x0b, 0);
        assert!(!cart.motor());
        assert_eq!(cart.mbc, Mbc::Mbc5 { rom: 1, ram: 0x0b });
    }
}
