//! The peripheral interface: its DMA between RDRAM and the PI bus (the
//! cartridge), the CPU's writes to that bus, and the bus timings of its two
//! domains. A DMA copies at once and stays busy for as long as the bus
//! takes, from the domain's timings: per 16 bits, the pulse width and
//! release (PWD + 1 and RLS + 1 RCP cycles), and per page of 2^(PGS + 2)
//! bytes the latency (LAT + 1). The RCP's clock is two thirds of the CPU's.

use crate::{N64, PI_DMA, PI_INTR};
use savestate::state;

// PI_STATUS, as read.
const DMA_BUSY: u32 = 1 << 0;
const IO_BUSY: u32 = 1 << 1;
const INTERRUPT: u32 = 1 << 3;

#[derive(Clone, Default)]
pub struct Pi {
    dram_addr: u32,
    cart_addr: u32,
    /// The lengths as they read back after a DMA.
    rd_len: u32,
    wr_len: u32,
    busy: bool,
    /// The two domains' latency, pulse width, page size and release.
    pub domains: [[u32; 4]; 2],
    /// The word the CPU last wrote to the PI bus, and the cycle the PI is
    /// done writing it (`pi_io_write`); records of the save state.
    pub(crate) io_word: u32,
    pub(crate) io_until: u64,
}

state!(Pi { dram_addr, cart_addr, rd_len, wr_len, busy, domains });

impl N64 {
    pub(crate) fn read_pi(&mut self, addr: u32) -> u32 {
        let pi = &self.pi;
        match addr & 0x3c {
            0x00 => pi.dram_addr,
            0x04 => pi.cart_addr,
            0x08 => pi.rd_len,
            0x0c => pi.wr_len,
            0x10 => {
                u32::from(pi.busy) * DMA_BUSY | u32::from(self.cycles < pi.io_until) * IO_BUSY
                    | if self.mi_intr & PI_INTR != 0 { INTERRUPT } else { 0 }
            }
            reg @ 0x14..=0x30 => {
                let reg = (reg - 0x14) as usize / 4;
                pi.domains[reg / 4][reg % 4]
            }
            _ => 0,
        }
    }

    pub(crate) fn write_pi(&mut self, addr: u32, value: u32) {
        match addr & 0x3c {
            0x00 => self.pi.dram_addr = value & 0xff_fffe,
            0x04 => self.pi.cart_addr = value & !1,
            0x08 => self.pi_dma(value & 0xff_ffff, false),
            0x0c => self.pi_dma(value & 0xff_ffff, true),
            0x10 => {
                // Bit 0 resets the DMA, bit 1 acknowledges the interrupt.
                if value & 1 != 0 {
                    self.pi.busy = false;
                    self.schedule(PI_DMA, u64::MAX);
                }
                if value & 2 != 0 {
                    self.acknowledge(PI_INTR);
                }
            }
            reg @ 0x14..=0x30 => {
                let reg = (reg - 0x14) as usize / 4;
                let mask = [0xff, 0xff, 0xf, 0x3][reg % 4];
                self.pi.domains[reg / 4][reg % 4] = value & mask;
            }
            _ => {}
        }
    }

    /// A DMA of length + 1 bytes between the PI bus and RDRAM. The bus moves
    /// 16 bits at a time, so the PI reads an even number of bytes, and the
    /// cartridge address moves on by as many. Going out it writes them all,
    /// and only the SRAM and the FlashRAM take them; coming in, RDRAM is
    /// written as `dma_blocks` says.
    fn pi_dma(&mut self, length: u32, to_rdram: bool) {
        if self.pi.busy {
            return;
        }
        let (dram, cart) = (self.pi.dram_addr, self.pi.cart_addr);
        let save = (0x0800_0000..0x1000_0000).contains(&cart) && self.save.on_pi_bus();
        let even = (length + 2) & !1;
        if to_rdram {
            self.pi_dma_in(dram, cart, length + 1, save);
        } else {
            let (d, n) = (dram as usize, even as usize);
            self.touch_rdram(d, n);
            if save {
                // Past the end of RDRAM nothing answers.
                let in_rdram = d.min(self.rdram.len())..(d + n).min(self.rdram.len());
                self.save.write_dma(cart, &self.rdram[in_rdram]);
            }
            self.pi.dram_addr = (dram + even + 7) & 0xff_fff8;
            self.pi.rd_len = 0x7f;
        }
        self.pi.cart_addr = cart + even;
        self.pi.busy = true;
        let [lat, pwd, pgs, rls] = self.pi.domains[domain(cart)];
        let page = 1 << (pgs + 2);
        let rcp = u64::from(even / 2) * u64::from(pwd + 1 + rls + 1) + u64::from(even.div_ceil(page) * (lat + 1));
        let at = self.cycles + rcp * 3 / 2;
        self.schedule(PI_DMA, at);
    }

    /// A DMA of `length` bytes from the PI bus into RDRAM: the bytes the PI
    /// reads (the ROM's own when it holds them all), written in its blocks.
    fn pi_dma_in(&mut self, dram: u32, cart: u32, length: u32, save: bool) {
        let n = ((length + 1) & !1) as usize;
        let mut reach = dram;
        let (end, wr_len) = dma_blocks(dram, length, |at, _, count| reach = at + count as u32);
        self.wrote_rdram(dram as usize, (reach - dram) as usize);
        let rom = cart.wrapping_sub(0x1000_0000) as usize;
        let in_rom = !save && cart >= 0x1000_0000 && rom + n <= self.cart.rom.len();
        let mut data = Vec::new();
        if save {
            data = vec![0; n];
            self.save.read_dma(cart, &mut data);
        } else if !in_rom {
            data = (0..n as u32).map(|i| self.read_pi_byte(cart + i)).collect();
        }
        let source = if in_rom { &self.cart.rom[rom..rom + n] } else { &data[..] };
        let rdram = &mut self.rdram;
        dma_blocks(dram, length, |at, from, count| {
            // Past the end of RDRAM nothing answers.
            let (at, end) = (at as usize, (at as usize + count).min(rdram.len()));
            if at < end {
                rdram[at..end].copy_from_slice(&source[from..from + end - at]);
            }
        });
        self.pi.dram_addr = end & 0xff_fff8;
        self.pi.wr_len = wr_len;
    }

    pub(crate) fn pi_dma_done(&mut self) {
        self.pi.busy = false;
        self.request(PI_INTR);
    }

    /// The CPU writing a word to the PI bus (n64brew, Memory map: PI external
    /// bus). The PI holds the word and lets the CPU go on while it writes it,
    /// as two halves after the domain's latency; until then PI_STATUS shows
    /// IO busy and further writes are lost (false: the PI did not take it).
    /// The PI does not know what answers an address, so a write to ROM is
    /// made too, and dropped by the cartridge.
    pub(crate) fn pi_io_write(&mut self, addr: u32, value: u32) -> bool {
        if self.cycles < self.pi.io_until {
            return false;
        }
        let [lat, pwd, _, rls] = self.pi.domains[domain(addr)];
        let rcp = u64::from(lat + 1 + 2 * (pwd + 1 + rls + 1));
        self.pi.io_word = value;
        self.pi.io_until = self.cycles + rcp * 3 / 2;
        true
    }

    /// A CPU read of the PI bus while a write is on it: whatever the address,
    /// the PI answers with the word it is writing, and the write is over.
    /// n64-systemtest's cart-writing tests find this on the console (A Bug's
    /// Life depends on it).
    pub(crate) fn pi_io_read(&mut self) -> Option<u32> {
        if self.cycles < self.pi.io_until {
            self.pi.io_until = 0;
            return Some(self.pi.io_word);
        }
        None
    }

    /// A byte of the PI bus, as a DMA reads it.
    fn read_pi_byte(&mut self, addr: u32) -> u8 {
        let word = if addr >= 0x1000_0000 && addr < 0x1fc0_0000 {
            self.cart.read_u32((addr & !3) - 0x1000_0000)
        } else {
            (addr & 0xffff & !3) * 0x0001_0001
        };
        (word >> (24 - 8 * (addr & 3))) as u8
    }
}

/// How the PI writes a DMA of `length` bytes from its bus into RDRAM at
/// `dram` (n64brew, PI: DMA transfers, from dumps of the console's DMAs at
/// every RDRAM offset, length and distance to a page's end; n64-systemtest's
/// cart_memory tests). Each part it writes goes to `write` as its RDRAM
/// address, its start among the bytes read from the bus, and its length;
/// returned are the RDRAM address after the DMA and PI_WR_LEN after it.
///
/// The PI works in blocks of up to 128 bytes through a buffer, which it
/// fills from the bus a half at a time and then writes out; a block also
/// ends with its RDRAM page (2 KB). RDRAM is written in doublewords and the
/// address goes on to the next doubleword after each block, so an odd last
/// block writes a byte more. The first block is meant to bring the address
/// to a doubleword when it is not on one: it writes bytes, so an odd first
/// block writes what it should unless it fills its buffer, which starts at
/// the address's offset in its doubleword and is that much shorter. But the
/// PI counts the block's end from the buffer's start, so the first block
/// writes that many bytes fewer and leaves them in RDRAM as they were (a
/// short one writes nothing). The block after one that ended less than a
/// doubleword before a page's end is shorter by the first's offset as well.
fn dma_blocks(dram: u32, length: u32, mut write: impl FnMut(u32, usize, usize)) -> (u32, u32) {
    // On a doubleword and even, as libultra's DMAs are: all of it, in order.
    if dram & 7 == 0 && length & 1 == 0 {
        write(dram, 0, length as usize);
        return ((dram + length + 7) & !7, 0x7f);
    }
    let (mut dram, mut from, mut left) = (dram, 0, length);
    let (mut most, mut first, mut wr_len) = (128, true, 0x7f);
    while left > 0 {
        let offset = dram & 7;
        let row = 0x800 - (dram & 0x7ff);
        let block = (most - offset).min(row).min(left);
        let count = block.saturating_sub(offset);
        let count = if first && block + offset < 127 { count } else { (count + 1) & !1 };
        if count > 0 {
            write(dram, from, count as usize);
        }
        dram = (dram + count + 7) & !7;
        let read = (block + 1) & !1;
        from += read as usize;
        left = left.saturating_sub(read);
        // PI_WR_LEN reads 0x7F, less the offset after a block of 8 bytes or
        // fewer.
        wr_len = if block <= 8 { 0x7f - offset } else { 0x7f };
        most = if row < 8 { 128 - offset } else { 128 };
        first = false;
    }
    (dram, wr_len)
}

/// The domain whose timings the PI uses at an address of its bus: 2 for the
/// 64DD's registers and the SRAM, 1 (here 0) for the cartridge's ROM and the
/// rest (n64brew, PI: Domains).
fn domain(addr: u32) -> usize {
    usize::from(matches!(addr >> 24, 0x05 | 0x08..=0x0f))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A console whose 4 KB cartridge counts bytes (from 0x40, after the
    /// header's first word), with the usual domain 1 timings.
    fn n64() -> N64 {
        let mut rom: Vec<u8> = (0..0x1000).map(|i| i as u8).collect();
        rom[..4].copy_from_slice(&[0x80, 0x37, 0x12, 0x40]);
        N64::new(&rom).unwrap()
    }

    /// n64-systemtest's cart-writing tests: a write to the cartridge is held
    /// by the PI, which shows IO busy and loses further writes; the next
    /// read, at any address, gets it and ends it; it is gone after the
    /// bus's time for it (here 111 RCP cycles, 166 of the CPU's).
    #[test]
    fn writes_to_the_cartridge() {
        let mut n64 = n64();
        let status = |n64: &mut N64| n64.read_u32(0x0460_0010) & IO_BUSY != 0;
        n64.write_u32(0x1000_0000, 0xbadc_0ffe);
        n64.write_u32(0x1000_0040, 0x000d_ecaf);
        assert!(status(&mut n64));
        assert_eq!(n64.read_u32(0x1000_0040), 0xbadc_0ffe);
        assert!(!status(&mut n64));
        assert_eq!(n64.read_u32(0x1000_0040), 0x4041_4243);
        n64.write_u8(0x1000_0001, 0x1234_56ba);
        assert_eq!(n64.read_u8(0x1000_0040), 0x56);
        n64.write_u64(0x1000_0000, 0x9876_5432_1af1_231a);
        n64.cycles += 165;
        assert_eq!(n64.read_u16(0x1000_0040), 0x9876);
        n64.write_u16(0x1000_0000, 0xbadc);
        n64.cycles += 166;
        assert!(!status(&mut n64));
        assert_eq!(n64.read_u32(0x1000_0040), 0x4041_4243);
        // The PIF is on the SI bus.
        n64.write_u32(0x1fc0_0000, 0xbadc_0ffe);
        assert_eq!(n64.read_u32(0x1000_0040), 0x4041_4243);
    }

    /// What a DMA from the cartridge's counting bytes at `cart` leaves in
    /// RDRAM filled with AA, from 16 bytes before `dram`, and the registers.
    fn dma(dram: u32, cart: u32, length: u32) -> (Vec<u8>, u32, u32) {
        let mut n64 = n64();
        n64.rdram[0x1000..0x2000].fill(0xaa);
        n64.write_u32(0x0460_0000, dram);
        n64.write_u32(0x0460_0004, 0x1000_0000 + cart);
        n64.write_u32(0x0460_000c, length - 1);
        let d = dram as usize - 16;
        let bytes = n64.rdram[d..d + length as usize + 48].to_vec();
        (bytes, n64.read_u32(0x0460_0000) - dram, n64.read_u32(0x0460_0004) - 0x1000_0000 - cart)
    }

    /// The bytes expected from `dma`: AA, the counting bytes from `cart` at
    /// each offset in `parts` (from the DMA's address), AA after them.
    fn expect(cart: u32, length: u32, parts: &[(u32, u32, u32)]) -> Vec<u8> {
        let mut bytes = vec![0xaa; length as usize + 48];
        for &(at, from, n) in parts {
            for i in 0..n {
                bytes[(16 + at + i) as usize] = (cart + from + i) as u8;
            }
        }
        bytes
    }

    /// n64-systemtest's cart_memory tests and n64brew's examples: odd lengths
    /// in the first block exact unless it fills the buffer, a byte more in
    /// later blocks; a misaligned first block writes its offset fewer bytes;
    /// the block after one that ended close to a page's end is short.
    #[test]
    fn dma_blocks_from_the_cartridge() {
        // On a doubleword: 1, 125 exact; 127 rounded up; 129, 133 a byte
        // more. Addresses after: RDRAM's on to a doubleword, the cartridge's
        // on by the halves read.
        for (length, written) in [(1, 1), (7, 7), (125, 125), (127, 128), (129, 130), (133, 134)] {
            let (bytes, dram, cart) = dma(0x1010, 0x40, length);
            assert_eq!(bytes, expect(0x40, length, &[(0, 0, written)]), "length {length}");
            assert_eq!((dram, cart), ((written + 7) & !7, (length + 1) & !1));
        }
        // Six bytes into a doubleword: the first block writes six fewer;
        // when it fills its buffer (122 bytes) six are left as they were.
        for (length, parts) in [
            (1, &[][..]),
            (8, &[(0, 0, 2)][..]),
            (119, &[(0, 0, 113)][..]),
            (121, &[(0, 0, 116)][..]),
            (284, &[(0, 0, 116), (122, 122, 162)][..]),
        ] {
            let (bytes, ..) = dma(0x1016, 0x40, length);
            assert_eq!(bytes, expect(0x40, length, parts), "length {length}");
        }
        // Two bytes before a page's end, six into a doubleword: nothing in
        // the first block, then a block of 122 and a hole of six.
        let (bytes, dram, _) = dma(0x17fe, 0x40, 224);
        assert_eq!(bytes, expect(0x40, 224, &[(2, 2, 122), (130, 124, 100)]));
        assert_eq!(dram, 0x18e8 - 0x17fe);
        // Six before it, two into a doubleword.
        let (bytes, ..) = dma(0x17fa, 0x40, 133);
        assert_eq!(bytes, expect(0x40, 133, &[(0, 0, 4), (6, 6, 126), (134, 132, 2)]));
    }
}
