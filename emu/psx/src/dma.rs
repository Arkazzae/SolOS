//! DMA: seven channels (0-1 MDEC, 2 GPU, 3 CD-ROM, 4 SPU, 5 PIO, 6 the
//! ordering table clear), each an address (MADR), a size (BCR) and a
//! control word (CHCR), started when CHCR's start bit is written with the
//! channel enabled in DPCR. Transfers are done at once; DICR raises the DMA
//! interrupt when one ends.

use crate::Psx;
use savestate::state;

#[derive(Clone, Copy, Default)]
pub struct Channel {
    madr: u32,
    bcr: u32,
    chcr: u32,
}

state!(Channel { madr, bcr, chcr });

#[derive(Clone)]
pub struct Dma {
    channels: [Channel; 7],
    dpcr: u32,
    dicr: u32,
}

impl Default for Dma {
    fn default() -> Dma {
        Dma { channels: [Channel::default(); 7], dpcr: 0x0765_4321, dicr: 0 }
    }
}

state!(Dma { channels, dpcr, dicr });

impl Psx {
    pub(crate) fn dma_read(&self, addr: u32) -> u32 {
        let dma = &self.dma;
        match addr {
            0x1f80_10f0 => dma.dpcr,
            0x1f80_10f4 => dma.dicr,
            _ => {
                let ch = &dma.channels[((addr >> 4) & 7) as usize % 7];
                match addr & 0xf {
                    0x0 => ch.madr,
                    0x4 => ch.bcr,
                    _ => ch.chcr,
                }
            }
        }
    }

    pub(crate) fn dma_write(&mut self, addr: u32, value: u32, width: u32) {
        // Narrow writes merge into the register.
        let mask = match width & 7 {
            1 => 0xff << ((width >> 4 & 3) * 8),
            2 => 0xffff << ((width >> 4 & 3) * 8),
            _ => 0xffff_ffff,
        };
        let merge = |old: u32| old & !mask | value & mask;
        match addr {
            0x1f80_10f0 => self.dma.dpcr = merge(self.dma.dpcr),
            0x1f80_10f4 => {
                // Bits 24-30 are flags, cleared by writing 1s.
                let old = self.dma.dicr;
                let new = merge(old);
                self.dma.dicr = new & 0x00ff_803f | old & !new & 0x7f00_0000;
                self.update_dicr();
            }
            _ => {
                let n = ((addr >> 4) & 7) as usize;
                if n == 7 {
                    return;
                }
                let ch = &mut self.dma.channels[n];
                match addr & 0xf {
                    0x0 => ch.madr = merge(ch.madr) & 0x00ff_ffff,
                    0x4 => ch.bcr = merge(ch.bcr),
                    0x8 => {
                        ch.chcr = merge(ch.chcr);
                        // Channel 6 only counts down; its other bits are fixed.
                        if n == 6 {
                            ch.chcr = ch.chcr & 0x5100_0000 | 0x0000_0002;
                        }
                        self.maybe_start(n);
                    }
                    _ => {}
                }
            }
        }
    }

    /// The master flag (bit 31) is up while an enabled channel's flag is,
    /// with the master enable (bit 23) on, or force (bit 15).
    fn update_dicr(&mut self) {
        let d = self.dma.dicr;
        let up = d & 1 << 15 != 0 || d & 1 << 23 != 0 && d >> 24 & d >> 16 & 0x7f != 0;
        if up && d & 1 << 31 == 0 {
            self.request(crate::DMA);
        }
        self.dma.dicr = d & !(1 << 31) | u32::from(up) << 31;
    }

    fn maybe_start(&mut self, n: usize) {
        let ch = self.dma.channels[n];
        let enabled = self.dma.dpcr >> (4 * n + 3) & 1 != 0;
        // Mode 0 waits for the trigger bit too.
        let sync = ch.chcr >> 9 & 3;
        let go = ch.chcr & 1 << 24 != 0 && (sync != 0 || ch.chcr & 1 << 28 != 0);
        if !(enabled && go) {
            return;
        }
        match (n, sync) {
            (6, _) => self.dma_otc(),
            (_, 2) => {
                // A list that loops on itself never ends.
                if !self.dma_linked_list(n) {
                    return;
                }
            }
            _ => {
                // Only block transfers use BCR. In linked-list mode it is normally zero.
                let count = block_words(&ch);
                // The MDEC's output goes as it is decoded: what there is, and the
                // channel stays on for the rest.
                if n == 1 && (self.mdec.output.len() as u64) < count {
                    let have = self.mdec.output.len() as u64;
                    if have > 0 && !self.mdec.busy() {
                        self.dma_block(1, have);
                        let block_size = match ch.bcr & 0xffff { 0 => 0x10000, n => u64::from(n) };
                        let blocks = (count - have).div_ceil(block_size) as u32;
                        self.dma.channels[1].bcr = ch.bcr & 0xffff | blocks << 16;
                    }
                    return;
                }
                self.dma_block(n, count);
            }
        }
        let ch = &mut self.dma.channels[n];
        ch.chcr &= !(1 << 24 | 1 << 28);
        // The transfer is over: the channel's flag, if enabled.
        if self.dma.dicr & 1 << (16 + n) != 0 {
            self.dma.dicr |= 1 << (24 + n);
        }
        self.update_dicr();
    }

    /// Clears an ordering table: each entry points to the one before it,
    /// the last (lowest) ends the list.
    fn dma_otc(&mut self) {
        let ch = self.dma.channels[6];
        let mut addr = ch.madr & 0x1f_fffc;
        let count = if ch.bcr & 0xffff == 0 { 0x10000 } else { ch.bcr & 0xffff };
        for i in 0..count {
            let value = if i == count - 1 { 0x00ff_ffff } else { addr.wrapping_sub(4) & 0x1f_ffff };
            self.store_ram(addr, value);
            addr = addr.wrapping_sub(4) & 0x1f_fffc;
        }
        self.cycles += u64::from(count);
    }

    /// Words in blocks (mode 1: BCR's block size times count; mode 0: its
    /// word count), to the device or from it.
    fn dma_block(&mut self, n: usize, count: u64) {
        let ch = self.dma.channels[n];
        let step = if ch.chcr & 2 != 0 { 4u32.wrapping_neg() } else { 4 };
        let to_device = ch.chcr & 1 != 0;
        let mut addr = ch.madr & 0x1f_fffc;
        if n == 4 {
            self.spu_sync();
        }
        let mut spu_irq = false;
        for _ in 0..count {
            if to_device {
                let v = self.load_ram(addr);
                match n {
                    0 => self.mdec.write(v),
                    2 => self.gpu.gp0(v),
                    4 => spu_irq |= self.spu.dma_write(v),
                    _ => {}
                }
            } else {
                let v = match n {
                    1 => self.mdec.read(),
                    2 => self.gpu.read(),
                    3 => self.cdrom.read_data_word(),
                    4 => {
                        let (v, irq) = self.spu.dma_read();
                        spu_irq |= irq;
                        v
                    }
                    _ => 0,
                };
                self.store_ram(addr, v);
            }
            addr = addr.wrapping_add(step) & 0x1f_fffc;
        }
        if spu_irq {
            self.request(crate::SPU);
        }
        if n == 0 {
            self.mdec_output_ready();
        }
        self.dma.channels[n].madr = addr;
        self.cycles += count;
        if n == 2 {
            self.gpu.flush();
        }
    }

    /// The GPU's command lists: each node a header (the next node's address
    /// and the words that follow), until an address with bit 23 set. False
    /// for a list that does not end (taken as one past a million nodes),
    /// which keeps the channel busy.
    fn dma_linked_list(&mut self, n: usize) -> bool {
        let mut addr = self.dma.channels[n].madr & 0x1f_fffc;
        let mut words = 0u64;
        let mut ended = false;
        for _ in 0..0x10_0000 {
            let header = self.load_ram(addr);
            let (first, count) = (addr as usize + 4, (header >> 24) as usize);
            if n == 2 && first + 4 * count <= self.ram.len() {
                // The node's words in a row, the usual case.
                self.gpu.gp0_words(&self.ram[first..first + 4 * count]);
            } else if n == 2 {
                for i in 0..count as u32 {
                    let v = self.load_ram(addr.wrapping_add(4 * (i + 1)) & 0x1f_fffc);
                    self.gpu.gp0(v);
                }
            }
            words += u64::from(header >> 24) + 1;
            if header & 0x80_0000 != 0 {
                ended = true;
                break;
            }
            addr = header & 0x1f_fffc;
        }
        self.cycles += words;
        self.gpu.flush();
        if ended {
            self.dma.channels[n].madr = 0x00ff_ffff;
        } else {
            self.dma.channels[n].madr = addr;
        }
        ended
    }

    /// A transfer to the MDEC's output that was waiting runs if there is
    /// now enough decoded.
    pub(crate) fn mdec_output_ready(&mut self) {
        if self.dma.channels[1].chcr & 1 << 24 != 0 {
            self.maybe_start(1);
        }
    }

    fn load_ram(&self, addr: u32) -> u32 {
        let i = addr as usize & 0x1f_fffc;
        u32::from_le_bytes(self.ram[i..i + 4].try_into().unwrap())
    }

    fn store_ram(&mut self, addr: u32, value: u32) {
        let i = addr as usize & 0x1f_fffc;
        if self.code_pages[i >> 12] != 0 {
            self.invalidate_code(addr);
        }
        self.ram[i..i + 4].copy_from_slice(&value.to_le_bytes());
    }
}

/// The words a block transfer moves: BCR's count, or block size times
/// blocks (a count of 0 meaning 10000h).
fn block_words(ch: &Channel) -> u64 {
    let or_most = |v: u32| if v == 0 { 0x10000 } else { u64::from(v) };
    if ch.chcr >> 9 & 3 == 0 {
        or_most(ch.bcr & 0xffff)
    } else {
        or_most(ch.bcr & 0xffff) * or_most(ch.bcr >> 16)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_counts_include_the_full_zero_encoded_range() {
        assert_eq!(block_words(&Channel { bcr: 0, chcr: 0, ..Channel::default() }), 0x10000);
        assert_eq!(block_words(&Channel { bcr: 0, chcr: 1 << 9, ..Channel::default() }), 1u64 << 32);
        assert_eq!(block_words(&Channel { bcr: 0x0002_0010, chcr: 1 << 9, ..Channel::default() }), 32);
    }

    #[test]
    fn a_gpu_linked_list_does_not_use_the_block_count() {
        let mut psx = Psx::blank();
        psx.set_gpu_thread(false);
        psx.store_ram(0, 0x00ff_ffff);
        psx.dma_write(0x1f80_10f0, 1 << 11, 4);
        psx.dma_write(0x1f80_10a8, 0x0100_0401, 4);
        assert_eq!(psx.dma_read(0x1f80_10a0), 0x00ff_ffff);
        assert_eq!(psx.dma_read(0x1f80_10a8) & (1 << 24), 0);
        assert_eq!(psx.cycles, 1);
    }
}
