//! Cartridge: the ROM behind the Sega or the Codemasters mapper, and the cartridge RAM the Sega mapper can put at
//! $8000. The header at $7FF0 (or $3FF0, $1FF0) names the system and the region when a cartridge has one.
use std::sync::Arc;

use savestate::state;

use crate::Error;

const PAGE: usize = 0x4000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mapper {
    /// Registers at $FFFC-$FFFF; the first 1 KiB is always page 0.
    Sega,
    /// Registers at $0000, $4000 and $8000, one 16 KiB bank each, no fixed area.
    Codemasters,
}

/// What the 16-byte "TMR SEGA" header says, when there is one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Header {
    /// 3 SMS Japan, 4 SMS export, 5 GG Japan, 6 GG export, 7 GG international.
    pub region_code: u8,
}

#[derive(Clone)]
pub(crate) struct Cart {
    rom: Arc<[u8]>,
    /// Pages in the (padded) ROM, minus one: the mask a bank number is cut with.
    page_mask: usize,
    pub hash: u64,
    /// The image's CRC-32, as game lists name it.
    pub crc: u32,
    pub mapper: Mapper,
    pub header: Option<Header>,
    /// Cartridge RAM: two 16 KiB banks.
    pub ram: Vec<u8>,
    /// Whether a game has switched the RAM on, which is when it is worth saving.
    pub ram_used: bool,
    /// $FFFC: bit 3 puts cartridge RAM at $8000, bit 2 picks its bank.
    control: u8,
    banks: [u8; 3],
}

state!(Cart {
    ram,
    ram_used,
    control,
    banks
});

fn fnv(data: &[u8]) -> u64 {
    data.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
    })
}

impl Cart {
    pub fn parse(input: &[u8]) -> Result<Self, Error> {
        // Copiers put 512 bytes in front of the ROM.
        let data = if input.len() > 512 && input.len() % PAGE == 512 {
            &input[512..]
        } else {
            input
        };
        // A program can be a few dozen bytes (the Game Gear's Fade Test is 50): the rest of its first page reads as
        // an unprogrammed ROM, $FF.
        if data.is_empty() || data.len() > 0x40_0000 {
            return Err(Error::NotRom);
        }
        let header = [0x7ff0usize, 0x3ff0, 0x1ff0]
            .into_iter()
            .find(|&at| data.len() >= at + 16 && &data[at..at + 8] == b"TMR SEGA")
            .map(|at| Header {
                region_code: data[at + 15] >> 4,
            });
        let word = |at: usize| u32::from(u16::from_le_bytes([data[at], data[at + 1]]));
        let mapper =
            if data.len() >= 0x8000 && word(0x7fe6) != 0 && word(0x7fe6) + word(0x7fe8) == 0x1_0000
            {
                Mapper::Codemasters
            } else {
                Mapper::Sega
            };
        let pages = data.len().div_ceil(PAGE).next_power_of_two();
        let mut rom = data.to_vec();
        rom.resize(pages * PAGE, 0xff);
        Ok(Self {
            hash: fnv(data),
            crc: crc32fast::hash(data),
            rom: rom.into(),
            page_mask: pages - 1,
            mapper,
            header,
            ram: vec![0; 2 * PAGE],
            ram_used: false,
            control: 0,
            banks: [0, 1, 2],
        })
    }

    pub fn reset(&mut self) {
        self.control = 0;
        self.banks = [0, 1, 2];
    }

    fn rom_at(&self, bank: u8, addr: usize) -> u8 {
        self.rom[((usize::from(bank) & self.page_mask) << 14) | (addr & (PAGE - 1))]
    }

    pub fn read(&self, addr: u16) -> u8 {
        let a = usize::from(addr);
        match a >> 14 {
            0 if self.mapper == Mapper::Sega && a < 0x400 => self.rom[a],
            0 => self.rom_at(self.banks[0], a),
            1 => self.rom_at(self.banks[1], a),
            _ if self.mapper == Mapper::Sega && self.control & 8 != 0 => {
                self.ram[(usize::from(self.control >> 2) & 1) << 14 | (a & (PAGE - 1))]
            }
            _ => self.rom_at(self.banks[2], a),
        }
    }

    /// A write to $0000-$BFFF: the Codemasters bank registers, or cartridge RAM where the Sega mapper has put it.
    pub fn write(&mut self, addr: u16, value: u8) {
        match self.mapper {
            Mapper::Codemasters => match addr {
                0x0000 => self.banks[0] = value,
                0x4000 => self.banks[1] = value,
                0x8000 => self.banks[2] = value,
                _ => {}
            },
            Mapper::Sega => {
                if addr >= 0x8000 && self.control & 8 != 0 {
                    let at = (usize::from(self.control >> 2) & 1) << 14
                        | (usize::from(addr) & (PAGE - 1));
                    self.ram[at] = value;
                }
            }
        }
    }

    /// A write that reached $FFFC-$FFFF (the machine also stores it in work RAM).
    pub fn mapper_write(&mut self, addr: u16, value: u8) {
        if self.mapper != Mapper::Sega {
            return;
        }
        match addr {
            0xfffc => {
                self.control = value;
                self.ram_used |= value & 8 != 0;
            }
            0xfffd => self.banks[0] = value,
            0xfffe => self.banks[1] = value,
            0xffff => self.banks[2] = value,
            _ => {}
        }
    }

    pub fn validate(&self) -> bool {
        self.ram.len() == 2 * PAGE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rom(pages: usize) -> Vec<u8> {
        let mut rom = vec![0; pages * PAGE];
        for (i, page) in rom.chunks_mut(PAGE).enumerate() {
            page.fill(i as u8);
        }
        rom
    }

    #[test]
    fn sega_mapper_pages_and_fixed_first_kilobyte() {
        let mut data = rom(8);
        data[0x10] = 0xaa;
        let mut c = Cart::parse(&data).unwrap();
        assert_eq!(
            (c.read(0x0010), c.read(0x4000), c.read(0x8000)),
            (0xaa, 1, 2)
        );
        c.mapper_write(0xfffd, 5);
        c.mapper_write(0xfffe, 6);
        c.mapper_write(0xffff, 7);
        // Page 0 stays under the first 1 KiB, the rest of slot 0 follows $FFFD.
        assert_eq!(
            (
                c.read(0x0010),
                c.read(0x0400),
                c.read(0x4000),
                c.read(0x8000)
            ),
            (0xaa, 5, 6, 7)
        );
        // A bank number past the end wraps by the ROM's page count.
        c.mapper_write(0xffff, 0x0b);
        assert_eq!(c.read(0x8000), 3);
    }

    #[test]
    fn sega_cartridge_ram_replaces_slot_two() {
        let mut c = Cart::parse(&rom(4)).unwrap();
        assert!(!c.ram_used);
        c.write(0x8000, 0x55);
        assert_eq!(c.read(0x8000), 2, "RAM is off until $FFFC bit 3");
        c.mapper_write(0xfffc, 0x08);
        c.write(0x8001, 0x42);
        assert_eq!((c.read(0x8001), c.ram_used), (0x42, true));
        c.mapper_write(0xfffc, 0x0c);
        assert_eq!(c.read(0x8001), 0, "the other RAM bank");
        c.mapper_write(0xfffc, 0x08);
        assert_eq!(c.read(0x8001), 0x42);
        c.mapper_write(0xfffc, 0x00);
        assert_eq!(c.read(0x8001), 2);
    }

    #[test]
    fn codemasters_mapper_detected_from_the_checksum_pair() {
        let mut data = rom(4);
        data[0x7fe6..0x7fea].copy_from_slice(&[0x34, 0x12, 0xcc, 0xed]);
        let mut c = Cart::parse(&data).unwrap();
        assert_eq!(c.mapper, Mapper::Codemasters);
        // No fixed kilobyte, and the registers sit in the ROM area.
        assert_eq!((c.read(0), c.read(0x4000), c.read(0x8000)), (0, 1, 2));
        c.write(0x8000, 3);
        c.write(0x4000, 2);
        assert_eq!((c.read(0x4000), c.read(0x8000)), (2, 3));
        c.mapper_write(0xfffe, 0);
        assert_eq!(c.read(0x4000), 2, "the Sega registers do nothing");
    }

    #[test]
    fn a_program_of_exactly_a_copier_headers_length_is_not_cut_to_nothing() {
        let c = Cart::parse(&[0x76; 512]).unwrap();
        assert_eq!(c.read(0), 0x76);
    }

    #[test]
    fn a_tiny_program_is_a_page_with_the_rest_unprogrammed() {
        let c = Cart::parse(&[0xc3, 0x00, 0x00]).unwrap();
        // A one-page ROM repeats in the other slots.
        assert_eq!(
            (
                c.read(0),
                c.read(2),
                c.read(3),
                c.read(0x3fff),
                c.read(0x4000)
            ),
            (0xc3, 0x00, 0xff, 0xff, 0xc3)
        );
        assert_eq!(c.header, None);
    }

    #[test]
    fn header_copier_prefix_and_bad_roms() {
        let mut data = vec![0; 2 * PAGE];
        data[0x7ff0..0x7ff8].copy_from_slice(b"TMR SEGA");
        data[0x7fff] = 0x6c;
        assert_eq!(
            Cart::parse(&data).unwrap().header,
            Some(Header { region_code: 6 })
        );
        let mut prefixed = vec![0xee; 512];
        prefixed.append(&mut data);
        let c = Cart::parse(&prefixed).unwrap();
        assert_eq!(c.header, Some(Header { region_code: 6 }));
        assert!(Cart::parse(&[]).is_err());
        assert!(Cart::parse(&vec![0; 0x40_0001]).is_err());
    }
}
