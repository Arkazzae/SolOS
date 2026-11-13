//! The cartridge's save memory, of which a game has one kind or none: an
//! EEPROM of 4 or 16 kbit, which the PIF reaches as the fifth channel of
//! the joybus; or, on the PI bus at 0800_0000 (domain 2), 32 KB of SRAM
//! kept by a battery (three banks of it on one game) or 128 KB of
//! FlashRAM. Nothing on the cartridge tells which it has, so the game is
//! looked up (games.rs). The memory is kept as the chip holds it, byte for
//! byte, as flashcarts keep their save files.
//!
//! The FlashRAM works by commands written to its command register at
//! 0801_0000 (n64brew, Flash): erase a sector of 16 KB or the whole chip
//! (back to FF), load a page of 128 bytes into its buffer and program it
//! (which can only clear bits), and choose what reading it gives, the
//! array itself, its status or its silicon ID. Erasing and programming
//! finish at once. It is an MX29L1100, the kind that takes the address of
//! a read in 16-bit words, which every version of libultra handles (later
//! ones tell it by its ID, earlier ones only knew it).

use savestate::state;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    None,
    Eeprom4k,
    Eeprom16k,
    Sram,
    /// Three banks of 32 KB, chosen by bits 18-19 of the address
    /// (Dezaemon 3D's).
    SramBanked,
    Flash,
}

impl Kind {
    pub fn size(self) -> usize {
        match self {
            Kind::None => 0,
            Kind::Eeprom4k => 0x200,
            Kind::Eeprom16k => 0x800,
            Kind::Sram => 0x8000,
            Kind::SramBanked => 0x1_8000,
            Kind::Flash => 0x2_0000,
        }
    }
}

// What reading the FlashRAM gives, as its last command left it.
const READ_ARRAY: u8 = 0;
const STATUS: u8 = 1;
const SILICON_ID: u8 = 2;
const LOAD_PAGE: u8 = 3;

// Its status register's bits for an erase and a program that went well.
const ERASE_OK: u8 = 1 << 3;
const PROGRAM_OK: u8 = 1 << 2;

/// The MX29L1100's silicon ID: the FlashRAM type, then Macronix and the
/// device.
const FLASH_ID: [u8; 8] = [0x11, 0x11, 0x80, 0x01, 0x00, 0xc2, 0x00, 0x1e];

#[derive(Clone)]
pub struct Save {
    pub kind: Kind,
    pub memory: Vec<u8>,
    /// The FlashRAM's mode and status, the page it is loading, and what
    /// the erase it is set up for would erase: a start and a length.
    mode: u8,
    status: u8,
    page: [u8; 128],
    erase: (u32, u32),
}

state!(Save { memory, mode, status, page, erase });

impl Save {
    /// Save memory as it comes, blank: FF in an erased FlashRAM, 0 in the
    /// others (as other emulators start them).
    pub fn new(kind: Kind) -> Save {
        let blank = if kind == Kind::Flash { 0xff } else { 0 };
        Save { kind, memory: vec![blank; kind.size()], mode: READ_ARRAY, status: 0, page: [0xff; 128], erase: (0, 0) }
    }

    /// Answers a joybus command on the cartridge's channel, if the
    /// cartridge has an EEPROM: what it is, a read of a block of 8 bytes,
    /// or a write of one. Blocks past the end wrap (a 4 kbit EEPROM ignores
    /// the top two bits of the block).
    pub fn eeprom(&mut self, command: &[u8]) -> Option<Vec<u8>> {
        let id = match self.kind {
            Kind::Eeprom4k => 0x80,
            Kind::Eeprom16k => 0xc0,
            _ => return None,
        };
        let len = self.memory.len();
        let block = |n: &u8| usize::from(*n) * 8 % len;
        match command {
            // Its kind, and a status with no write going on.
            [0x00 | 0xff, ..] => Some(vec![0x00, id, 0x00]),
            [0x04, n, ..] => {
                let at = block(n);
                Some(self.memory[at..at + 8].to_vec())
            }
            [0x05, n, data @ ..] if data.len() >= 8 => {
                let at = block(n);
                self.memory[at..at + 8].copy_from_slice(&data[..8]);
                // Not busy: the write has started.
                Some(vec![0x00])
            }
            _ => None,
        }
    }

    /// Whether the save memory answers on the PI bus (at 0800_0000 to
    /// 0FFF_FFFF).
    pub fn on_pi_bus(&self) -> bool {
        matches!(self.kind, Kind::Sram | Kind::SramBanked | Kind::Flash)
    }

    /// Where an address of the PI bus is in the SRAM.
    fn sram_offset(&self, addr: u32) -> usize {
        let bank = if self.kind == Kind::SramBanked { (addr >> 18 & 3) as usize } else { 0 };
        (bank * 0x8000 + (addr & 0x7fff) as usize) % self.memory.len()
    }

    /// A DMA from the save memory into the buffer, from an address of the
    /// PI bus.
    pub fn read_dma(&mut self, addr: u32, out: &mut [u8]) {
        if self.kind != Kind::Flash {
            for (i, byte) in out.iter_mut().enumerate() {
                *byte = self.memory[self.sram_offset(addr + i as u32)];
            }
            return;
        }
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = match self.mode {
                // The address is of 16-bit words.
                READ_ARRAY => self.memory[((addr & 0xffff) as usize * 2 + i) % self.memory.len()],
                // 00 and the status, over and over.
                STATUS => [0, self.status][i & 1],
                SILICON_ID => FLASH_ID[i & 7],
                _ => self.page[(addr as usize + i) & 0x7f],
            };
        }
    }

    /// A DMA from the buffer into the save memory: into the SRAM, or into
    /// the FlashRAM's page while it is loading one.
    pub fn write_dma(&mut self, addr: u32, data: &[u8]) {
        if self.kind != Kind::Flash {
            for (i, &byte) in data.iter().enumerate() {
                let at = self.sram_offset(addr + i as u32);
                self.memory[at] = byte;
            }
        } else if self.mode == LOAD_PAGE {
            for (i, &byte) in data.iter().enumerate() {
                self.page[(addr as usize + i) & 0x7f] = byte;
            }
        }
    }

    /// A 32-bit read by the CPU, as libultra reads the FlashRAM's status.
    pub fn read_word(&mut self, addr: u32) -> u32 {
        let mut word = [0; 4];
        self.read_dma(addr, &mut word);
        u32::from_be_bytes(word)
    }

    /// A 32-bit write by the CPU: to the SRAM, or a command to the
    /// FlashRAM (or 0 to its status, which clears it).
    pub fn write_word(&mut self, addr: u32, value: u32) {
        if self.kind != Kind::Flash {
            self.write_dma(addr, &value.to_be_bytes());
        } else if addr & 0x1_ffff == 0x1_0000 {
            self.flash_command(value);
        } else if self.mode == STATUS {
            self.status = 0;
        } else {
            self.write_dma(addr, &value.to_be_bytes());
        }
    }

    /// A command to the FlashRAM; those that erase or program leave it
    /// showing its status, for the game to see they are done.
    fn flash_command(&mut self, command: u32) {
        let page = command & 0x3ff;
        match command >> 24 {
            // Set up an erase of the whole chip, or of the sector a page
            // is in.
            0x3c => self.erase = (0, 0x2_0000),
            0x4b => self.erase = ((page & 0x380) * 128, 0x4000),
            0x78 => {
                let (start, length) = self.erase;
                if length != 0 {
                    self.memory[start as usize..(start + length) as usize].fill(0xff);
                    self.status |= ERASE_OK;
                }
                self.erase = (0, 0);
                self.mode = STATUS;
            }
            0xa5 => {
                let at = page as usize * 128;
                for (byte, &new) in self.memory[at..at + 128].iter_mut().zip(&self.page) {
                    *byte &= new;
                }
                self.page = [0xff; 128];
                self.status |= PROGRAM_OK;
                self.mode = STATUS;
            }
            0xb4 => self.mode = LOAD_PAGE,
            0xd2 => self.mode = STATUS,
            0xe1 => self.mode = SILICON_ID,
            0xf0 => self.mode = READ_ARRAY,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eeprom() {
        let mut save = Save::new(Kind::Eeprom4k);
        assert_eq!(save.eeprom(&[0x00]), Some(vec![0x00, 0x80, 0x00]));
        let mut write = vec![0x05, 3];
        write.extend(1..=8);
        assert_eq!(save.eeprom(&write), Some(vec![0x00]));
        assert_eq!(save.eeprom(&[0x04, 3]), Some((1..=8).collect()));
        // 4 kbit: block 67 is block 3.
        assert_eq!(save.eeprom(&[0x04, 67]), Some((1..=8).collect()));
        assert_eq!(&save.memory[24..32], &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(Save::new(Kind::Eeprom16k).eeprom(&[0xff]), Some(vec![0x00, 0xc0, 0x00]));
        assert_eq!(Save::new(Kind::Sram).eeprom(&[0x00]), None);
    }

    #[test]
    fn sram() {
        let mut save = Save::new(Kind::Sram);
        save.write_dma(0x0800_7ffe, &[1, 2, 3, 4]);
        assert_eq!(&save.memory[0x7ffe..], &[1, 2]);
        assert_eq!(&save.memory[..2], &[3, 4]);
        save.write_word(0x0800_0100, 0x1234_5678);
        assert_eq!(save.read_word(0x0800_0100), 0x1234_5678);
        let mut banked = Save::new(Kind::SramBanked);
        banked.write_dma(0x0808_0010, &[9]);
        assert_eq!(banked.memory[0x1_0010], 9);
    }

    /// As libultra goes about it: the ID, erasing a sector, loading a page
    /// and programming it, reading it back, each step's status.
    #[test]
    fn flash() {
        let mut save = Save::new(Kind::Flash);
        let command = 0x0801_0000;
        save.write_word(command, 0xe100_0000);
        let mut id = [0; 8];
        save.read_dma(0x0800_0000, &mut id);
        assert_eq!(id, FLASH_ID);

        save.memory[0x4000..0x8000].fill(0);
        save.write_word(command, 0x4b00_0080 + 5);
        save.write_word(command, 0x7800_0000);
        assert_eq!(save.read_word(0x0800_0000) & 0xff, u32::from(ERASE_OK));
        save.write_word(command, 0xd200_0000);
        save.write_word(0x0800_0000, 0);
        assert_eq!(save.read_word(0x0800_0000), 0);
        assert!(save.memory[0x4000..0x8000].iter().all(|&b| b == 0xff));

        save.write_word(command, 0xb400_0000);
        let data: Vec<u8> = (0..128).collect();
        save.write_dma(0x0800_0000, &data);
        save.write_word(command, 0xa500_0000 + 0x81);
        assert_eq!(save.read_word(0x0800_0000) & 0xff, u32::from(PROGRAM_OK));
        save.write_word(command, 0xf000_0000);
        let mut page = [0; 128];
        // Page 0x81 by its address in words.
        save.read_dma(0x0800_0000 + 0x81 * 64, &mut page);
        assert_eq!(&page[..], &data[..]);
        assert_eq!(&save.memory[0x4080..0x4100], &data[..]);

        // Programming only clears bits.
        save.write_word(command, 0xb400_0000);
        save.write_dma(0x0800_0000, &[0xf0; 128]);
        save.write_word(command, 0xa500_0081);
        assert_eq!(save.memory[0x4080 + 0x3f], 0x30);
    }
}
