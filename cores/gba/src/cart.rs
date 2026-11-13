//! The cartridge: its ROM and the memory that keeps the game's saves,
//! which is one of SRAM (32 KB), Flash (64 or 128 KB) or EEPROM (512 bytes
//! or 8 KB). Which one a game has is not in its header; Nintendo's
//! libraries leave its name in the ROM ("SRAM_V113", "FLASH1M_V102"...),
//! and that is how it is found, as other emulators do.

use std::fmt;

use savestate::{BadState, State, state};

use crate::rtc::Rtc;

#[derive(Debug, PartialEq)]
pub enum Error {
    /// Too short for a GBA ROM, or longer than a cartridge can be (32 MB).
    NotRom,
    /// A save state that is damaged, or from another version of the core.
    BadState,
    /// A save state from another game.
    OtherGame,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::NotRom => write!(f, "not a Game Boy Advance ROM"),
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

/// Flash chips answer their ID command with a maker and a device code:
/// Panasonic's 64 KB chip and Sanyo's 128 KB one, as other emulators do.
const FLASH64_ID: [u8; 2] = [0x32, 0x1b];
const FLASH128_ID: [u8; 2] = [0x62, 0x13];

#[derive(Clone, Debug, PartialEq)]
enum Kind {
    None,
    Sram,
    Flash,
    Eeprom,
}

/// Where a Flash chip's command sequence has got to.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Flash {
    Ready,
    /// AA written to 5555, then 55 to 2AAA: the chip waits for a command.
    Unlock1,
    Unlock2,
    /// The next write programs a byte, or (128 KB) picks the bank.
    Program,
    Bank,
}

impl State for Flash {
    fn save(&self, out: &mut Vec<u8>) {
        out.push(*self as u8);
    }

    fn load(&mut self, input: &mut &[u8]) -> Result<(), BadState> {
        let mut n = 0u8;
        n.load(input)?;
        *self = [Flash::Ready, Flash::Unlock1, Flash::Unlock2, Flash::Program, Flash::Bank]
            .get(usize::from(n))
            .copied()
            .ok_or(BadState)?;
        Ok(())
    }
}

#[derive(Clone)]
pub struct Cart {
    rom: Vec<u8>,
    kind: Kind,
    /// The save memory: SRAM, Flash or EEPROM, by kind.
    pub memory: Vec<u8>,
    /// The ROM's hash, which save states carry.
    pub hash: u64,

    flash: Flash,
    flash_id: bool,
    /// An erase command (80) has been given, and the chip waits for which.
    flash_erase: bool,
    flash_bank: usize,

    /// The EEPROM's address width, 6 bits (512 bytes) or 14 (8 KB), known
    /// from how many bits the game sends the first time.
    eeprom_wide: Option<bool>,
    /// The bits of the request coming in, and how many.
    eeprom_in: u128,
    eeprom_count: u32,
    /// What a read sends back, and the bits of it left.
    eeprom_out: u64,
    eeprom_left: u32,

    /// The real-time clock, for the cartridges with one.
    pub rtc: Option<Rtc>,
    /// A cartridge with a motor (Drill Dozer, WarioWare: Twisted!) on pin 3
    /// of the port, and the port's data and direction as the game last set
    /// them: the motor runs while pin 3 is an output and high. Whether it
    /// runs is for the host to feel, not saved.
    rumble: bool,
    gpio: (u16, u16),
}

state!(Cart { memory, flash, flash_id, flash_erase, flash_bank, eeprom_wide, eeprom_in, eeprom_count, eeprom_out,
    eeprom_left, rtc });

/// The save memory a game's library names in its ROM, and its size.
fn detect(rom: &[u8]) -> (Kind, usize) {
    let names: [(&[u8], Kind, usize); 6] = [
        (b"EEPROM_V", Kind::Eeprom, 0x2000),
        (b"SRAM_V", Kind::Sram, 0x8000),
        (b"SRAM_F_V", Kind::Sram, 0x8000),
        (b"FLASH_V", Kind::Flash, 0x10000),
        (b"FLASH512_V", Kind::Flash, 0x10000),
        (b"FLASH1M_V", Kind::Flash, 0x20000),
    ];
    // The names sit at word-aligned addresses.
    for i in (0..rom.len().saturating_sub(12)).step_by(4) {
        if !matches!(rom[i], b'E' | b'S' | b'F') {
            continue;
        }
        for (name, kind, size) in &names {
            if rom[i..].starts_with(name) {
                return (kind.clone(), *size);
            }
        }
    }
    (Kind::None, 0)
}

impl Cart {
    pub fn parse(rom: &[u8]) -> Result<Cart, Error> {
        if rom.len() < 0xc0 || rom.len() > 0x200_0000 {
            return Err(Error::NotRom);
        }
        let (kind, size) = detect(rom);
        let hash = rom.chunks(8).fold(0xcbf2_9ce4_8422_2325u64, |h, chunk| {
            let mut bytes = [0; 8];
            bytes[..chunk.len()].copy_from_slice(chunk);
            (h ^ u64::from_le_bytes(bytes)).wrapping_mul(0x0100_0000_01b3)
        });
        Ok(Cart {
            rom: rom.to_vec(),
            kind,
            memory: vec![0xff; size],
            hash,
            flash: Flash::Ready,
            flash_id: false,
            flash_erase: false,
            flash_bank: 0,
            eeprom_wide: None,
            eeprom_in: 0,
            eeprom_count: 0,
            eeprom_out: 0,
            eeprom_left: 0,
            // Nintendo's library for the clock names itself in the ROM too.
            rtc: rom.windows(8).any(|w| w == b"SIIRTC_V").then(Rtc::new),
            // By the game code at 0xAC, any region: Drill Dozer, WarioWare: Twisted!.
            rumble: matches!(rom.get(0xac..0xaf), Some(b"V49" | b"RZW")),
            gpio: (0, 0),
        })
    }

    /// A halfword of ROM; past its end the bus holds the low bits of the
    /// address (in halfwords), which the cartridge was last sent.
    pub fn read_rom(&self, addr: u32) -> u16 {
        let i = addr as usize & 0x1ff_fffe;
        match self.rom.get(i..i + 2) {
            Some(bytes) => u16::from_le_bytes([bytes[0], bytes[1]]),
            None => (addr >> 1) as u16,
        }
    }

    /// Reads at 0E00_0000: SRAM, or Flash (its ID in ID mode).
    pub fn read_backup(&self, addr: u32) -> u8 {
        match self.kind {
            Kind::Sram => self.memory[addr as usize & 0x7fff],
            Kind::Flash if self.flash_id && addr & 0xfffe == 0 => {
                let id = if self.memory.len() > 0x10000 { FLASH128_ID } else { FLASH64_ID };
                id[addr as usize & 1]
            }
            Kind::Flash => self.memory[self.flash_bank * 0x10000 + (addr as usize & 0xffff)],
            _ => 0xff,
        }
    }

    pub fn write_backup(&mut self, addr: u32, value: u8) {
        match self.kind {
            Kind::Sram => self.memory[addr as usize & 0x7fff] = value,
            Kind::Flash => self.write_flash(addr as usize & 0xffff, value),
            _ => {}
        }
    }

    /// Flash takes commands as writes of AA to 5555, 55 to 2AAA and the
    /// command to 5555: ID mode on (90) and off (F0), erase (80, then the
    /// sequence again and 10 for the chip or 30 at a 4 KB sector), program
    /// a byte (A0) and, on 128 KB chips, pick the 64 KB bank (B0).
    fn write_flash(&mut self, addr: usize, value: u8) {
        self.flash = match (self.flash, addr, value) {
            (Flash::Program, _, _) => {
                self.memory[self.flash_bank * 0x10000 + addr] = value;
                Flash::Ready
            }
            (Flash::Bank, 0, _) => {
                self.flash_bank = usize::from(value & 1) * usize::from(self.memory.len() > 0x10000);
                Flash::Ready
            }
            (Flash::Ready, 0x5555, 0xaa) => Flash::Unlock1,
            (Flash::Unlock1, 0x2aaa, 0x55) => Flash::Unlock2,
            (Flash::Unlock2, _, _) if self.flash_erase => {
                self.flash_erase = false;
                if addr == 0x5555 && value == 0x10 {
                    self.memory.fill(0xff);
                } else if value == 0x30 {
                    let start = self.flash_bank * 0x10000 + (addr & 0xf000);
                    self.memory[start..start + 0x1000].fill(0xff);
                }
                Flash::Ready
            }
            (Flash::Unlock2, 0x5555, command) => match command {
                0x90 => {
                    self.flash_id = true;
                    Flash::Ready
                }
                0xf0 => {
                    self.flash_id = false;
                    Flash::Ready
                }
                0x80 => {
                    self.flash_erase = true;
                    Flash::Ready
                }
                0xa0 => Flash::Program,
                0xb0 if self.memory.len() > 0x10000 => Flash::Bank,
                _ => Flash::Ready,
            },
            // F0 alone also leaves ID mode.
            (_, _, 0xf0) => {
                self.flash_id = false;
                Flash::Ready
            }
            _ => Flash::Ready,
        };
    }

    /// Whether the EEPROM answers at a cartridge address: anywhere in the
    /// top 16 MB, or with a 32 MB ROM only its last 256 bytes.
    pub fn eeprom_at(&self, addr: u32) -> bool {
        self.kind == Kind::Eeprom
            && addr >> 24 == 0x0d
            && (self.rom.len() <= 0x100_0000 || addr & 0x00ff_ff00 == 0x00ff_ff00)
    }

    /// A DMA of this many halfwords is about to reach the EEPROM: a read
    /// request is 9 bits (6-bit address) or 17 (14-bit), a write 73 or 81.
    pub fn eeprom_transfer(&mut self, halfwords: u32) {
        if self.eeprom_wide.is_none() {
            match halfwords {
                9 | 73 => self.eeprom_wide = Some(false),
                17 | 81 => self.eeprom_wide = Some(true),
                _ => {}
            }
        }
    }

    /// The EEPROM sends a bit a read: after a read request, 4 that mean
    /// nothing and the 64 of the data, highest first; otherwise 1, ready.
    pub fn read_eeprom(&mut self) -> u16 {
        if self.eeprom_left == 0 {
            return 1;
        }
        self.eeprom_left -= 1;
        if self.eeprom_left >= 64 {
            return 0;
        }
        (self.eeprom_out >> self.eeprom_left) as u16 & 1
    }

    /// The EEPROM takes a bit a write: 11 and an address to read, or 10,
    /// an address and 64 bits to write; each request ends with a 0.
    pub fn write_eeprom(&mut self, value: u16) {
        self.eeprom_in = self.eeprom_in << 1 | u128::from(value & 1);
        self.eeprom_count += 1;
        if self.eeprom_count < 2 {
            return;
        }
        let width = if self.eeprom_wide.unwrap_or(true) { 14 } else { 6 };
        let request = (self.eeprom_in >> (self.eeprom_count - 2)) & 3;
        match (self.eeprom_count, request) {
            (2, 0 | 1) => self.eeprom_count = 0,
            (n, 3) if n == 3 + width => {
                let block = (self.eeprom_in >> 1) as usize & ((1 << width) - 1);
                self.eeprom_out = self.eeprom_block(block);
                self.eeprom_left = 68;
                self.eeprom_count = 0;
            }
            (n, 2) if n == 3 + width + 64 => {
                let block = (self.eeprom_in >> 65) as usize & ((1 << width) - 1);
                let data = (self.eeprom_in >> 1) as u64;
                let i = block * 8 % self.eeprom_size();
                self.memory[i..i + 8].copy_from_slice(&data.to_be_bytes());
                self.eeprom_count = 0;
            }
            _ => {}
        }
    }

    /// A block of EEPROM, its first byte highest, as it is sent.
    fn eeprom_block(&self, block: usize) -> u64 {
        let i = block * 8 % self.eeprom_size();
        u64::from_be_bytes(self.memory[i..i + 8].try_into().unwrap())
    }

    /// The EEPROM's size: 8 KB is kept for either, so that save states
    /// have one size.
    fn eeprom_size(&self) -> usize {
        if self.eeprom_wide == Some(false) { 0x200 } else { 0x2000 }
    }

    /// Whether the cartridge's motor runs.
    pub fn motor(&self) -> bool {
        self.rumble && self.gpio.0 & self.gpio.1 & 8 != 0
    }

    /// The port at 0800_00C4-C9, where a cartridge with a clock has one,
    /// when the game has made it readable.
    pub fn read_gpio(&self, addr: u32) -> Option<u16> {
        if !(0xc4..0xca).contains(&(addr & 0x1ff_ffff)) {
            return None;
        }
        self.rtc.as_ref()?.read(addr)
    }

    pub fn write_gpio(&mut self, addr: u32, value: u16, now: u64) {
        if self.rumble {
            match addr & 0x1ff_ffff {
                0xc4 => self.gpio.0 = value,
                0xc6 => self.gpio.1 = value,
                _ => {}
            }
        }
        if let Some(rtc) = &mut self.rtc
            && (0xc4..0xca).contains(&(addr & 0x1ff_ffff))
        {
            rtc.write(addr, value, now);
        }
    }

    /// What the save memory holds, if the game has some, for the host to
    /// keep between sessions; as other emulators keep it, so .sav files
    /// carry over.
    pub fn save_data(&self) -> Option<Vec<u8>> {
        match self.kind {
            Kind::None => None,
            Kind::Eeprom => Some(self.memory[..self.eeprom_size()].to_vec()),
            _ => Some(self.memory.clone()),
        }
    }

    /// Puts back what the save memory held, before running. Its size
    /// tells an EEPROM's.
    pub fn load_save_data(&mut self, data: &[u8]) {
        if self.kind == Kind::Eeprom {
            self.eeprom_wide = Some(data.len() > 0x200);
        }
        let n = data.len().min(self.memory.len());
        self.memory[..n].copy_from_slice(&data[..n]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A rumble cartridge, known by its game code: pin 3 of the port, set
    /// as an output and high, runs its motor.
    #[test]
    fn a_rumble_cartridge() {
        let mut rom = vec![0u8; 0x200];
        rom[0xac..0xb0].copy_from_slice(b"V49E");
        let mut cart = Cart::parse(&rom).unwrap();
        cart.write_gpio(0x0800_00c4, 8, 0);
        assert!(!cart.motor());
        cart.write_gpio(0x0800_00c6, 8, 0);
        assert!(cart.motor());
        cart.write_gpio(0x0800_00c4, 0, 0);
        assert!(!cart.motor());
        rom[0xac..0xb0].copy_from_slice(b"AWRE");
        let mut cart = Cart::parse(&rom).unwrap();
        cart.write_gpio(0x0800_00c6, 8, 0);
        cart.write_gpio(0x0800_00c4, 8, 0);
        assert!(!cart.motor());
    }

    /// The EEPROM as games reach it, a bit per halfword of DMA: a request
    /// to write a block, then one to read it, which sends 4 bits that mean
    /// nothing and the 64 of the block. The first DMA's length tells a
    /// 512-byte EEPROM (6-bit addresses) from an 8 KB one.
    #[test]
    fn eeprom() {
        let mut rom = vec![0; 0x1000];
        rom[0x400..0x408].copy_from_slice(b"EEPROM_V");
        let mut cart = Cart::parse(&rom).unwrap();
        let bits = |value: u64, n: u32| (0..n).rev().map(move |i| (value >> i & 1) as u16);
        let data = 0x0123_4567_89ab_cdef;
        cart.eeprom_transfer(73);
        for bit in bits(0b10, 2).chain(bits(5, 6)).chain(bits(data, 64)).chain(bits(0, 1)) {
            cart.write_eeprom(bit);
        }
        assert_eq!(cart.read_eeprom(), 1, "ready after writing");
        cart.eeprom_transfer(9);
        for bit in bits(0b11, 2).chain(bits(5, 6)).chain(bits(0, 1)) {
            cart.write_eeprom(bit);
        }
        let read: Vec<u16> = (0..68).map(|_| cart.read_eeprom()).collect();
        assert_eq!(read[..4], [0; 4]);
        assert_eq!(read[4..].iter().fold(0, |v, &b| v << 1 | u64::from(b)), data);
        let saved = cart.save_data().unwrap();
        assert_eq!(saved.len(), 0x200);
        assert_eq!(saved[40..48], data.to_be_bytes());
    }
}
