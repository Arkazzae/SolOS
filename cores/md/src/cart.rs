//! Cartridge decoding, linear ROM / Sega bank mapper and parallel save RAM.
use std::sync::Arc;

use savestate::state;

use crate::{Error, Region};

#[derive(Clone)]
pub(crate) struct Cart {
    rom: Arc<[u8]>,
    pub hash: u64,
    pub title: String,
    pub region: Region,
    pub ram: Vec<u8>,
    ram_start: u32,
    ram_end: u32,
    ram_stride: u32,
    enabled: bool,
    protected: bool,
    banks: [u8; 8],
}

// The ROM and its metadata stay attached to the cartridge on restore.
state!(Cart {
    ram,
    enabled,
    protected,
    banks
});

impl Cart {
    pub fn parse(input: &[u8]) -> Result<Self, Error> {
        if input.len() < 0x200 || input.len() > 0x1000000 + 512 {
            return Err(Error::NotRom);
        }
        let mut rom = input.to_vec();
        // SMD: a 512-byte copier header followed by 16 KiB blocks, the
        // odd bytes first and the even bytes in the second half.
        if input.len() % 0x4000 == 512 && input[8..10] == [0xaa, 0xbb] {
            rom = vec![0; input.len() - 512];
            for (src, dst) in input[512..]
                .chunks_exact(0x4000)
                .zip(rom.chunks_exact_mut(0x4000))
            {
                for i in 0..0x2000 {
                    dst[i * 2] = src[i + 0x2000];
                    dst[i * 2 + 1] = src[i];
                }
            }
        } else if &rom[0x100..0x104] == b"ESAG" {
            for word in rom.chunks_exact_mut(2) {
                word.swap(0, 1);
            }
        }
        if rom.len() < 0x200 || &rom[0x100..0x104] != b"SEGA" || !rom.len().is_multiple_of(2) {
            return Err(Error::NotRom);
        }
        if rom[0x100..0x110].windows(3).any(|s| s == b"32X") {
            return Err(Error::Unsupported("32X cartridges"));
        }
        if rom[0x100..0x110].windows(4).any(|s| s == b"PICO") {
            return Err(Error::Unsupported("Sega Pico cartridges"));
        }
        let long = |at: usize| u32::from_be_bytes(rom[at..at + 4].try_into().unwrap());
        let pc = long(4);
        if pc & 1 != 0 || pc < 8 || pc as usize >= rom.len() {
            return Err(Error::NotRom);
        }
        let title = String::from_utf8_lossy(&rom[0x150..0x180])
            .trim()
            .to_owned();
        let codes = &rom[0x1f0..0x200];
        // Prefer the USA mode on multi-region cartridges. A hex region
        // mask uses J=1, U=4, E=8, alongside the usual letter codes.
        let mask = codes
            .iter()
            .take_while(|&&b| !b.is_ascii_whitespace() && b != 0)
            .filter_map(|&b| (b as char).to_digit(16))
            .fold(0, |a, b| a | b);
        let region = if codes.contains(&b'U') {
            Region::Usa
        } else if codes.contains(&b'E') {
            Region::Europe
        } else if codes.contains(&b'J') {
            Region::Japan
        } else if mask & 4 != 0 {
            Region::Usa
        } else if mask & 8 != 0 {
            Region::Europe
        } else {
            Region::Japan
        };
        let (ram_start, ram_end, ram_stride, ram_size) = if &rom[0x1b0..0x1b2] == b"RA" {
            let (start, end) = (long(0x1b4), long(0x1b8));
            if start == end {
                return Err(Error::Unsupported("serial EEPROM cartridges"));
            }
            if start > end || end > 0x3fffff {
                return Err(Error::NotRom);
            }
            let stride = if start & 1 == end & 1 { 2 } else { 1 };
            let size = (end - start) / stride + 1;
            if size > 0x20000 {
                return Err(Error::Unsupported("save RAM larger than 128 KiB"));
            }
            (start, end, stride, size as usize)
        } else {
            (0, 0, 1, 0)
        };
        let hash = rom.iter().fold(0xcbf29ce484222325u64, |h, &b| {
            (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
        });
        let enabled = ram_size != 0 && ram_start as usize >= rom.len();
        Ok(Self {
            rom: rom.into(),
            hash,
            title,
            region,
            ram: vec![0xff; ram_size],
            ram_start,
            ram_end,
            ram_stride,
            enabled,
            protected: false,
            banks: [0, 1, 2, 3, 4, 5, 6, 7],
        })
    }

    fn ram_index(&self, addr: u32) -> Option<usize> {
        (self.enabled
            && !self.ram.is_empty()
            && (self.ram_start..=self.ram_end).contains(&addr)
            && (addr - self.ram_start).is_multiple_of(self.ram_stride))
        .then(|| ((addr - self.ram_start) / self.ram_stride) as usize)
    }

    pub fn read(&self, addr: u32) -> u8 {
        if let Some(i) = self.ram_index(addr) {
            return self.ram[i];
        }
        let slot = (addr >> 19) as usize;
        let at = usize::from(self.banks[slot & 7]) * 0x80000 + (addr as usize & 0x7ffff);
        self.rom.get(at).copied().unwrap_or(0xff)
    }

    pub fn write(&mut self, addr: u32, value: u8) {
        if !self.protected
            && let Some(i) = self.ram_index(addr)
        {
            self.ram[i] = value;
        }
    }

    pub fn mapper(&mut self, addr: u32, value: u8) {
        if addr == 0xa130f1 {
            self.enabled = value & 1 != 0;
            self.protected = value & 2 != 0;
        } else if (0xa130f3..=0xa130ff).contains(&addr) && addr & 1 != 0 {
            self.banks[((addr - 0xa130f1) / 2) as usize] = value & 31;
        }
    }
}
