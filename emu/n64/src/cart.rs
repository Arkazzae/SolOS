//! The cartridge: its ROM, put in the console's byte order whatever order
//! the file has it in, and what its header and boot code tell: where the
//! game starts, the region, and the CIC (the lockout chip on the
//! cartridge) it was made for, whose seed the boot code needs.
//!
//! ROM files come in three byte orders, told apart by the header's first
//! word (the PI bus timings, 0x80371240 on every cartridge), not by the
//! file's extension: big-endian as the console reads it (.z64), with the
//! bytes of each half-word swapped (.v64, from the Doctor V64), and with
//! each word reversed (.n64, little-endian). Some files called .n64 are in
//! fact big-endian, which the first word shows.

use std::fmt;
use std::sync::Arc;

use savestate::BadState;

#[derive(Debug, PartialEq)]
pub enum Error {
    /// Too short for an N64 ROM, or with a first word of none of the three
    /// byte orders.
    NotRom,
    /// A save state that is damaged, or from another version of the core.
    BadState,
    /// A save state from another game.
    OtherGame,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::NotRom => write!(f, "not a Nintendo 64 ROM"),
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

/// The lockout chip a game was made for, which decides the seed the PIF
/// hands the boot code and how the boot code finds the game's entry point.
/// Named by their NTSC chips; each PAL chip pairs with one (7101 with 6102,
/// 7103 with 6103 and so on), except 7102, which has 6101's boot code.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Cic {
    Cic6101,
    Cic6102,
    Cic7102,
    Cic6103,
    Cic6105,
    Cic6106,
}

impl Cic {
    /// Told by the boot code, by the sum of its words, as mupen64plus does
    /// (src/device/pif/cic.c); boot code it does not know is taken for
    /// 6102's, which most games have.
    fn detect(rom: &[u8]) -> Cic {
        let sum: u64 = rom[0x40..0x1000].chunks(4).map(|w| u64::from(u32::from_be_bytes(w.try_into().unwrap()))).sum();
        match sum {
            0x0000_00d0_027f_df31 | 0x0000_00cf_fb63_1223 => Cic::Cic6101,
            0x0000_00d6_497e_414b => Cic::Cic6103,
            0x0000_011a_49f6_0e96 => Cic::Cic6105,
            0x0000_00d6_d5be_5580 => Cic::Cic6106,
            _ => Cic::Cic6102,
        }
    }

    /// The seed of the boot code's checksums, which the PIF gets from the
    /// CIC and IPL2 leaves in s6 (n64brew, PIF-NUS: the IPL2 and IPL3 seeds
    /// are the same byte on every known chip).
    pub fn seed(self) -> u8 {
        match self {
            Cic::Cic6101 | Cic::Cic6102 | Cic::Cic7102 => 0x3f,
            Cic::Cic6103 => 0x78,
            Cic::Cic6105 => 0x91,
            Cic::Cic6106 => 0x85,
        }
    }
}

/// The television standard of the console a game is for, as the boot code
/// tells the game in s4.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Region {
    Pal = 0,
    Ntsc = 1,
    Mpal = 2,
}

#[derive(Clone)]
pub struct Cart {
    /// The ROM, big-endian. Shared, so that copying the machine (as loading
    /// a save state does) does not copy up to 64 MB.
    pub rom: Arc<[u8]>,
    /// The ROM's hash, which save states carry.
    pub hash: u64,
    pub cic: Cic,
    pub region: Region,
}

impl Cart {
    pub fn parse(file: &[u8]) -> Result<Cart, Error> {
        if file.len() < 0x1000 {
            return Err(Error::NotRom);
        }
        let mut rom = file.to_vec();
        match u32::from_be_bytes(file[..4].try_into().unwrap()) {
            0x8037_1240 => {}
            0x3780_4012 => rom.chunks_exact_mut(2).for_each(|half| half.swap(0, 1)),
            0x4012_3780 => rom.chunks_exact_mut(4).for_each(|word| word.reverse()),
            _ => return Err(Error::NotRom),
        }
        let hash = rom.chunks(8).fold(0xcbf2_9ce4_8422_2325u64, |h, chunk| {
            let mut bytes = [0; 8];
            bytes[..chunk.len()].copy_from_slice(chunk);
            (h ^ u64::from_le_bytes(bytes)).wrapping_mul(0x0100_0000_01b3)
        });
        let mut cic = Cic::detect(&rom);
        // The country code: Europe's and Australia's games are PAL, Brazil's
        // PAL-M; 7102 is the PAL chip that has 6101's boot code.
        let region = match rom[0x3e] {
            b'D' | b'F' | b'I' | b'P' | b'S' | b'U' | b'X' | b'Y' | b'L' => Region::Pal,
            b'B' => Region::Mpal,
            _ => Region::Ntsc,
        };
        if cic == Cic::Cic6101 && region == Region::Pal {
            cic = Cic::Cic7102;
        }
        Ok(Cart { rom: rom.into(), hash, cic, region })
    }

    /// The address the game's code is loaded to and started at: the
    /// header's, but 1 MB lower with CIC 6103's boot code and 2 MB lower
    /// with 6106's, which move it (Paper Mario's header says 8012_5C00, its
    /// code starts at 8002_5C00; N64FlashcartMenu, rom_info.c,
    /// fix_boot_address).
    pub fn entry(&self) -> u32 {
        let header = u32::from_be_bytes(self.rom[8..12].try_into().unwrap());
        match self.cic {
            Cic::Cic6103 => header.wrapping_sub(0x10_0000),
            Cic::Cic6106 => header.wrapping_sub(0x20_0000),
            _ => header,
        }
    }

    /// A 32-bit read of the ROM, at a byte offset (which the PI keeps even);
    /// past its end the bus reads back the low half of the address, twice.
    pub fn read_u32(&self, offset: u32) -> u32 {
        let i = offset as usize;
        match self.rom.get(i..i + 4) {
            Some(word) => u32::from_be_bytes(word.try_into().unwrap()),
            None if i < self.rom.len() => {
                let mut bytes = [0; 4];
                bytes[..self.rom.len() - i].copy_from_slice(&self.rom[i..]);
                u32::from_be_bytes(bytes)
            }
            None => (offset & 0xffff) * 0x0001_0001,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ROM of 4 KB with the header's first word, an entry point and a
    /// country code.
    fn rom() -> Vec<u8> {
        let mut rom = vec![0; 0x1000];
        rom[..4].copy_from_slice(&[0x80, 0x37, 0x12, 0x40]);
        rom[8..12].copy_from_slice(&[0x80, 0x24, 0x60, 0x00]);
        rom[0x3e] = b'E';
        rom
    }

    #[test]
    fn byte_orders() {
        let z64 = rom();
        let v64: Vec<u8> = z64.chunks(2).flat_map(|h| [h[1], h[0]]).collect();
        let n64: Vec<u8> = z64.chunks(4).flat_map(|w| [w[3], w[2], w[1], w[0]]).collect();
        for file in [&z64, &v64, &n64] {
            let cart = Cart::parse(file).unwrap();
            assert_eq!(&cart.rom[..], &z64[..]);
            assert_eq!(cart.entry(), 0x8024_6000);
            assert_eq!(cart.region, Region::Ntsc);
            assert_eq!(cart.cic, Cic::Cic6102);
        }
        assert_eq!(Cart::parse(&vec![0; 0x1000]).err(), Some(Error::NotRom));
        assert_eq!(Cart::parse(&z64[..0x800]).err(), Some(Error::NotRom));
    }

    #[test]
    fn reads_past_the_end() {
        let cart = Cart::parse(&rom()).unwrap();
        assert_eq!(cart.read_u32(0), 0x8037_1240);
        assert_eq!(cart.read_u32(0x0123_4568), 0x4568_4568);
    }
}
