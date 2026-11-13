//! The physical memory map (what the CPU reaches after address
//! translation), RDRAM and its registers, the RDRAM interface and the
//! MIPS interface.
//!
//! | Address    | What                                                  |
//! |------------|-------------------------------------------------------|
//! | 0000_0000  | RDRAM, 8 MB; nothing answers above it                 |
//! | 03F0_0000  | RDRAM modules' registers (03F8_0000: to all of them)  |
//! | 0400_0000  | RSP DMEM, 4 KB, and IMEM, 4 KB, repeated to 0403_FFFF |
//! | 0404_0000  | RSP registers, and its PC at 0408_0000                |
//! | 0410_0000  | RDP command registers                                 |
//! | 0430_0000  | MIPS interface (MI)                                   |
//! | 0440_0000  | video interface (VI)                                  |
//! | 0450_0000  | audio interface (AI)                                  |
//! | 0460_0000  | peripheral interface (PI)                             |
//! | 0470_0000  | RDRAM interface (RI)                                  |
//! | 0480_0000  | serial interface (SI)                                 |
//! | 0500_0000  | the PI bus: SRAM or FlashRAM at 0800_0000, ROM from   |
//! |            | 1000_0000                                             |
//! | 1FC0_0000  | PIF: its boot ROM (locked after boot) and 64 B of RAM |
//!
//! Only RDRAM takes accesses of every size. The rest of the map, the RCP's
//! memories and registers and the PI and SI buses, sees one 32-bit word
//! whatever the size (n64brew, Memory map: physical memory map accesses). A
//! narrower read takes its part of the word. A narrower store sends the
//! register's low word shifted into the store's lane, and the device takes
//! all of it: the zeros below the lane and the register's bytes above it
//! (SB of 1234_5678 to offset 1 writes 5678_0000). A 64-bit store sends its
//! upper word only. 64-bit reads, which hang the console, are two 32-bit
//! reads here.

use crate::N64;

pub const RDRAM_SIZE: usize = 0x80_0000;
pub const RDRAM_MODULES: usize = 4;
pub const RDRAM_REGS: usize = 10;
/// The IS-Viewer's buffer, at 13FF_0000 on the PI bus.
pub const ISVIEWER_SIZE: usize = 0x1_0000;
const ISVIEWER: u32 = 0x13ff_0000;

// RDRAM registers, by number.
const RDRAM_DEVICE_TYPE: usize = 0;
const RDRAM_DEVICE_ID: usize = 1;
const RDRAM_DELAY: usize = 2;
const RDRAM_MODE: usize = 3;
const RDRAM_MIN_INTERVAL: usize = 7;
const RDRAM_MANUFACTURER: usize = 9;

/// The bits of an RDRAM module's Mode register that read inverted (n64brew,
/// RDRAM: X2 and the current control value; the CE bit too, as mupen64plus
/// has it).
const MODE_INVERTED: u32 = 0xc0c0_c0c0;
const MODE_CE: u32 = 1 << 31;

impl N64 {
    /// RDRAM's modules and the RDRAM interface as they are at power on,
    /// before the boot code sets them up: as mupen64plus has them
    /// (src/device/rdram/rdram.c), 2 MB modules of 9-bit bytes.
    pub(crate) fn power_on_rdram(&mut self) {
        for regs in &mut self.rdram_regs {
            regs[RDRAM_DEVICE_TYPE] = 0xb519_0010;
            regs[RDRAM_DEVICE_ID] = 0;
            regs[RDRAM_DELAY] = 0x230b_0223;
            regs[RDRAM_MODE] = 0xc4c0_c0c0;
            regs[RDRAM_MIN_INTERVAL] = 0x0040_c0e0;
            regs[RDRAM_MANUFACTURER] = 0x0000_0500;
        }
        self.rdram_unreadable = false;
    }

    pub(crate) fn read_u8(&mut self, addr: u32) -> u8 {
        if (addr as usize) < RDRAM_SIZE && !self.rdram_unreadable {
            self.touch_rdram(addr as usize, 1);
            return self.rdram[addr as usize];
        }
        (self.read_io(narrow_word(addr)) >> (24 - 8 * (addr & 3))) as u8
    }

    pub(crate) fn read_u16(&mut self, addr: u32) -> u16 {
        if (addr as usize) < RDRAM_SIZE && !self.rdram_unreadable {
            self.touch_rdram(addr as usize, 2);
            let i = addr as usize;
            return u16::from_be_bytes([self.rdram[i], self.rdram[i + 1]]);
        }
        (self.read_io(narrow_word(addr)) >> (16 - 8 * (addr & 2))) as u16
    }

    pub(crate) fn read_u32(&mut self, addr: u32) -> u32 {
        if (addr as usize) < RDRAM_SIZE && !self.rdram_unreadable {
            self.touch_rdram(addr as usize, 4);
            let i = addr as usize;
            return u32::from_be_bytes(self.rdram[i..i + 4].try_into().unwrap());
        }
        self.read_io(addr)
    }

    pub(crate) fn read_u64(&mut self, addr: u32) -> u64 {
        if (addr as usize) < RDRAM_SIZE && !self.rdram_unreadable {
            self.touch_rdram(addr as usize, 8);
            let i = addr as usize;
            return u64::from_be_bytes(self.rdram[i..i + 8].try_into().unwrap());
        }
        u64::from(self.read_io(addr)) << 32 | u64::from(self.read_io(addr + 4))
    }

    /// SB, given the register's low word: RDRAM takes its low byte, the
    /// rest of the map the word shifted into the byte's lane.
    pub(crate) fn write_u8(&mut self, addr: u32, value: u32) {
        if (addr as usize) < RDRAM_SIZE && self.mi_mode & MI_REPEAT == 0 {
            self.wrote_rdram(addr as usize, 1);
            self.rdram[addr as usize] = value as u8;
            return;
        }
        let word = value << (24 - 8 * (addr & 3));
        self.write_word(addr, word, u64::from(word) * 0x1_0000_0001);
    }

    /// SH, as SB with the register's low half.
    pub(crate) fn write_u16(&mut self, addr: u32, value: u32) {
        if (addr as usize) < RDRAM_SIZE && self.mi_mode & MI_REPEAT == 0 {
            self.wrote_rdram(addr as usize, 2);
            let i = addr as usize;
            self.rdram[i..i + 2].copy_from_slice(&(value as u16).to_be_bytes());
            return;
        }
        let word = value << (16 - 8 * (addr & 2));
        self.write_word(addr, word, u64::from(word) * 0x1_0000_0001);
    }

    pub(crate) fn write_u32(&mut self, addr: u32, value: u32) {
        if (addr as usize) < RDRAM_SIZE && self.mi_mode & MI_REPEAT == 0 {
            self.wrote_rdram(addr as usize, 4);
            let i = addr as usize;
            self.rdram[i..i + 4].copy_from_slice(&value.to_be_bytes());
            return;
        }
        self.write_word(addr, value, u64::from(value) * 0x1_0000_0001);
    }

    pub(crate) fn write_u64(&mut self, addr: u32, value: u64) {
        if (addr as usize) < RDRAM_SIZE && self.mi_mode & MI_REPEAT == 0 {
            self.wrote_rdram(addr as usize, 8);
            let i = addr as usize;
            self.rdram[i..i + 8].copy_from_slice(&value.to_be_bytes());
            return;
        }
        self.write_word(addr, (value >> 32) as u32, value);
    }

    /// A store anywhere but RDRAM in its usual mode: RDRAM in repeat mode
    /// takes the doubleword the CPU sent (a narrower store's word twice),
    /// the rest of the map the word.
    fn write_word(&mut self, addr: u32, word: u32, doubleword: u64) {
        if addr < 0x03f0_0000 {
            self.write_rdram_repeat(addr, doubleword);
        } else {
            self.write_io(narrow_word(addr), word);
        }
    }

    /// A 32-bit read of anything but readable RDRAM.
    fn read_io(&mut self, addr: u32) -> u32 {
        match addr >> 20 {
            0x000..=0x03e => self.read_rdram_slow(addr),
            0x03f => self.read_rdram_register(addr),
            0x040 if addr < 0x0404_0000 => self.rsp.read_memory(addr),
            0x040 => self.read_sp_register(addr),
            0x041 => self.read_dp(addr),
            0x043 => self.read_mi(addr),
            0x044 => self.read_vi(addr),
            0x045 => self.read_ai(addr),
            0x046 => self.read_pi(addr),
            0x047 => self.ri[(addr as usize >> 2) & 7],
            0x048 => self.read_si(addr),
            0x050..=0x1fb => self.read_pi_bus(addr),
            0x1fc => self.read_pif(addr),
            _ => 0,
        }
    }

    /// A write of a 32-bit word to anything but RDRAM, which takes it whole.
    fn write_io(&mut self, addr: u32, value: u32) {
        match addr >> 20 {
            0x03f => self.write_rdram_register(addr, value),
            0x040 if addr < 0x0404_0000 => self.rsp.write_memory(addr, value, !0),
            0x040 => self.write_sp_register(addr, value),
            0x041 => self.write_dp(addr, value),
            0x043 => self.write_mi(addr, value),
            0x044 => self.write_vi(addr, value),
            0x045 => self.write_ai(addr, value),
            0x046 => self.write_pi(addr, value),
            0x047 => self.ri[(addr as usize >> 2) & 7] = value,
            0x048 => self.write_si(addr, value),
            0x050..=0x1fb => self.write_pi_bus(addr, value),
            0x1fc => self.write_pif(addr, value),
            _ => {}
        }
    }

    /// What is at a physical address, read without side effects.
    pub(crate) fn peek_byte(&self, addr: u32) -> u8 {
        self.rdp_thread.drain();
        let word = match addr >> 20 {
            0x000..=0x007 => return self.rdram[addr as usize],
            0x040 if addr < 0x0404_0000 => self.rsp.read_memory(addr & !3),
            0x044 => self.read_vi(addr & !3),
            0x100..=0x1fb => self.cart.read_u32(addr & !3 & 0x0fff_ffff),
            0x1fc => self.si.read_pif(addr & !3),
            _ => 0,
        };
        (word >> (24 - 8 * (addr & 3))) as u8
    }

    /// RDRAM below 8 MB, read while a module's current control is too low
    /// for it to answer: that module reads 0, and so does everything above
    /// 8 MB, where there are no modules.
    fn read_rdram_slow(&mut self, addr: u32) -> u32 {
        if addr as usize >= RDRAM_SIZE {
            return 0;
        }
        // In memory space, Adr[28:20] is the address's megabyte (n64brew,
        // RDRAM Interface), which picks the module.
        match self.rdram_module(addr >> 20 & 0x3f) {
            Some(module) if !unreadable(self.rdram_regs[module][RDRAM_MODE]) => {
                let i = addr as usize;
                self.touch_rdram(i, 4);
                u32::from_be_bytes(self.rdram[i..i + 4].try_into().unwrap())
            }
            _ => 0,
        }
    }

    /// The module whose DeviceId matches a megabyte of the RDRAM bus: the
    /// first in the chain, which is how modules that share an ID (before
    /// the boot code gives them their own) answer one at a time. Bit 20
    /// does not count, as these are 2 MB modules.
    fn rdram_module(&self, megabyte: u32) -> Option<usize> {
        self.rdram_regs.iter().position(|regs| (id_field(regs[RDRAM_DEVICE_ID]) ^ megabyte) & !1 == 0)
    }

    fn read_rdram_register(&mut self, addr: u32) -> u32 {
        // In register space, Adr[28:20] comes from address bits 10-18.
        let reg = (addr as usize & 0x3ff) >> 2;
        if addr & 0x8_0000 != 0 || reg >= RDRAM_REGS {
            return 0;
        }
        match self.rdram_module(addr >> 10 & 0x1ff) {
            Some(module) if reg == RDRAM_MODE => self.rdram_regs[module][reg] ^ MODE_INVERTED,
            Some(module) => self.rdram_regs[module][reg],
            None => 0,
        }
    }

    /// A write to one module's register, or to all of theirs (at 03F8_0000
    /// and up).
    fn write_rdram_register(&mut self, addr: u32, value: u32) {
        let reg = (addr as usize & 0x3ff) >> 2;
        if reg >= RDRAM_REGS {
            return;
        }
        if addr & 0x8_0000 != 0 {
            self.rdram_regs.iter_mut().for_each(|regs| regs[reg] = value);
        } else if let Some(module) = self.rdram_module(addr >> 10 & 0x1ff) {
            self.rdram_regs[module][reg] = value;
        }
        if reg == RDRAM_MODE {
            self.rdram_unreadable = self.rdram_regs.iter().any(|regs| unreadable(regs[RDRAM_MODE]));
        }
        self.mi_mode &= !MI_REPEAT;
    }

    /// A store to RDRAM in the MIPS interface's repeat mode, which ends the
    /// mode, or a store above 8 MB, which nothing takes. The store goes on
    /// for RepeatCount + 1 bytes (n64brew, MIPS Interface) as n64-systemtest's
    /// MI Repeat tests find it on the console: counted from the start of the
    /// doubleword the address is in, but written from the address on (a byte
    /// at 5 with a count of 4 writes nothing), each byte the one in its place
    /// of the doubleword the CPU sent, wrapping within the 2 KB RDRAM page
    /// the address is in.
    fn write_rdram_repeat(&mut self, addr: u32, doubleword: u64) {
        if self.mi_mode & MI_REPEAT == 0 {
            return;
        }
        self.mi_mode &= !MI_REPEAT;
        let bytes = doubleword.to_be_bytes();
        let end = (addr & !7) + (self.mi_mode & 0x7f) + 1;
        let page = addr & !0x7ff;
        for at in addr..end {
            let i = (page | at & 0x7ff) as usize;
            if i < RDRAM_SIZE {
                self.wrote_rdram(i, 1);
                self.rdram[i] = bytes[at as usize & 7];
            }
        }
    }

    fn read_mi(&mut self, addr: u32) -> u32 {
        match addr & 0xf {
            0x0 => self.mi_mode & 0x3ff,
            0x4 => 0x0202_0102,
            0x8 => self.mi_intr,
            _ => self.mi_mask,
        }
    }

    fn write_mi(&mut self, addr: u32, value: u32) {
        match addr & 0xf {
            0x0 => {
                // Each feature has a bit to clear it and one to set it.
                let mut mode = self.mi_mode & !0x7f | value & 0x7f;
                let flags = [(MI_REPEAT, 1 << 7, 1 << 8), (MI_EBUS, 1 << 9, 1 << 10), (MI_UPPER, 1 << 12, 1 << 13)];
                for (bit, clear, set) in flags {
                    if value & clear != 0 {
                        mode &= !bit;
                    }
                    if value & set != 0 {
                        mode |= bit;
                    }
                }
                self.mi_mode = mode;
                if value & 1 << 11 != 0 {
                    self.acknowledge(crate::DP_INTR);
                }
            }
            0xc => {
                for i in 0..6 {
                    if value & 1 << (2 * i) != 0 {
                        self.mi_mask &= !(1 << i);
                    }
                    if value & 2 << (2 * i) != 0 {
                        self.mi_mask |= 1 << i;
                    }
                }
                self.update_interrupt_line();
            }
            _ => {}
        }
    }

    /// The PI bus outside the PI's registers: the cartridge's ROM, the
    /// IS-Viewer's buffer, the SRAM or FlashRAM, and nothing else, which
    /// reads as the low half of the address twice, as the bus does where
    /// nothing answers (n64brew, Peripheral Interface). While the PI is
    /// writing a word the CPU sent, every read gets that word (but the
    /// IS-Viewer's; see `write_pi_bus`).
    fn read_pi_bus(&mut self, addr: u32) -> u32 {
        if (ISVIEWER..ISVIEWER + ISVIEWER_SIZE as u32).contains(&addr) {
            let i = (addr - ISVIEWER) as usize;
            return u32::from_be_bytes(std::array::from_fn(|k| self.isviewer.get(i + k).copied().unwrap_or(0)));
        }
        if let Some(word) = self.pi_io_read() {
            return word;
        }
        if addr >= 0x1000_0000 {
            return self.cart.read_u32(addr - 0x1000_0000);
        }
        if addr >= 0x0800_0000 && self.save.on_pi_bus() {
            return self.save.read_word(addr);
        }
        (addr & 0xffff) * 0x0001_0001
    }

    /// A write to the PI bus, which the PI makes in the background and loses
    /// while it is still making the last one (`pi_io_write`). Below the ROM
    /// the SRAM or the FlashRAM takes it; the ROM drops it. The IS-Viewer,
    /// a port for text of development cartridges and emulators, takes its
    /// writes at once and leaves the PI alone, so that what a program prints
    /// does not change what it finds on the bus (n64-systemtest prints each
    /// test's name just before it reads the cartridge): its buffer from
    /// 13FF_0020, printed when the length is written to 13FF_0014.
    fn write_pi_bus(&mut self, addr: u32, value: u32) {
        if !(ISVIEWER..ISVIEWER + ISVIEWER_SIZE as u32).contains(&addr) {
            if self.pi_io_write(addr, value) && (0x0800_0000..0x1000_0000).contains(&addr) && self.save.on_pi_bus() {
                self.save.write_word(addr, value);
            }
            return;
        }
        let i = (addr - ISVIEWER) as usize;
        let n = 4.min(ISVIEWER_SIZE - i);
        self.isviewer[i..i + n].copy_from_slice(&value.to_be_bytes()[..n]);
        if i == 0x14 {
            let end = (0x20 + value as usize).min(ISVIEWER_SIZE);
            let text = self.isviewer[0x20..end].to_vec();
            self.isviewer_out.extend_from_slice(&text);
        }
    }
}

/// The address of the word a narrower access takes its lane from, or puts
/// it in: the word the address is in, but on the PI bus the word from the
/// address's 16-bit half on. That bus is 16 bits wide and the PI moves a
/// word as two halves from the address the CPU sends, which the CPU does
/// not align, while the CPU takes the lane as if the word were aligned
/// (n64brew, Memory map: a 16-bit read at 1000_0002 returns the half at
/// 1000_0004). So the last two bytes of a word, and its second half, are
/// read from the word after it.
fn narrow_word(addr: u32) -> u32 {
    if (0x0500_0000..0x1fc0_0000).contains(&addr) { addr & !1 } else { addr & !3 }
}

// MI_MODE's bits: the repeat mode, the EBus mode, and the upper mode (for
// RDRAM registers).
pub(crate) const MI_REPEAT: u32 = 1 << 7;
const MI_EBUS: u32 = 1 << 8;
const MI_UPPER: u32 = 1 << 9;

/// A DeviceId register's IdField[35:20] (n64brew, RDRAM: IdField[25:20] in
/// bits 31-26, IdField[26] in bit 23, IdField[34:27] in bits 15-8 and
/// IdField[35] in bit 7), as a number of megabytes.
fn id_field(device_id: u32) -> u32 {
    (device_id >> 26 & 0x3f) | (device_id >> 23 & 1) << 6 | (device_id >> 8 & 0xff) << 7 | (device_id >> 7 & 1) << 15
}

/// Whether a module's Mode register (as written) sets its current control
/// so that it cannot drive the bus: as read back, in automatic mode with a
/// value of 0. This is what lets the boot code's calibration find, and
/// count, the modules.
fn unreadable(mode: u32) -> bool {
    let read = mode ^ MODE_INVERTED;
    let cc = (read >> 6 & 1) | (read >> 14 & 1) << 1 | (read >> 22 & 1) << 2 | (read >> 7 & 1) << 3
        | (read >> 15 & 1) << 4 | (read >> 23 & 1) << 5;
    read & MODE_CE != 0 && cc == 0
}

#[cfg(test)]
mod tests {
    use crate::N64;

    /// A console with a 4 KB cartridge whose ROM counts bytes from 0x40.
    fn n64() -> N64 {
        let mut rom: Vec<u8> = (0..0x1000).map(|i| i as u8).collect();
        rom[..4].copy_from_slice(&[0x80, 0x37, 0x12, 0x40]);
        N64::new(&rom).unwrap()
    }

    /// n64-systemtest's spmem and pifram tests: the RCP and the SI bus take
    /// the whole word a narrower store sends, and only the upper word of a
    /// 64-bit one; narrower reads take their lane.
    #[test]
    fn stores_outside_rdram_write_the_word_on_the_bus() {
        let mut n64 = n64();
        for i in 0..4 {
            n64.write_u32(0x0400_0000 + 4 * i, 0xdead_beef);
        }
        for (k, word) in [0x7800_0000, 0x5678_0000, 0x3456_7800, 0x1234_5678].into_iter().enumerate() {
            n64.write_u8(0x0400_0000 + 5 * k as u32, 0x1234_5678);
            assert_eq!(n64.read_u32(0x0400_0000 + 4 * k as u32), word);
        }
        n64.write_u16(0x0400_0006, 0x1234_5678);
        assert_eq!(n64.read_u32(0x0400_0004), 0x1234_5678);
        n64.write_u64(0x0400_0008, 0xabcd_ef98_7654_3210);
        assert_eq!((n64.read_u32(0x0400_0008), n64.read_u32(0x0400_000c)), (0xabcd_ef98, 0x1234_5678));
        assert_eq!((n64.read_u8(0x0400_0009), n64.read_u16(0x0400_000a)), (0xcd, 0xef98));
        n64.write_u16(0x1fc0_07c0, 0x4241_31e2);
        assert_eq!(n64.read_u32(0x1fc0_07c0), 0x31e2_0000);
        n64.write_u8(0x1fc0_07c3, 0x1234_5678);
        assert_eq!(n64.read_u32(0x1fc0_07c0), 0x1234_5678);
        // RDRAM takes the store's own bytes.
        n64.write_u32(0x1000, 0xffff_ffff);
        n64.write_u8(0x1001, 0x1234_5678);
        n64.write_u16(0x1002, 0x1234_5678);
        assert_eq!(n64.read_u32(0x1000), 0xff78_5678);
    }

    /// n64-systemtest's cart tests: a narrower read of the PI bus gets the
    /// lane of the word the PI reads from the address's half on.
    #[test]
    fn narrow_reads_of_the_pi_bus() {
        let mut n64 = n64();
        let halves: Vec<u16> = (0..4).map(|i| n64.read_u16(0x1000_0040 + 2 * i)).collect();
        assert_eq!(halves, [0x4041, 0x4445, 0x4445, 0x4849]);
        let bytes: Vec<u8> = (0..8).map(|i| n64.read_u8(0x1000_0040 + i)).collect();
        assert_eq!(bytes, [0x40, 0x41, 0x44, 0x45, 0x44, 0x45, 0x48, 0x49]);
        assert_eq!(n64.read_u32(0x1000_0040), 0x4041_4243);
    }

    /// n64-systemtest's MI Repeat tests: the store goes on to RepeatCount + 1
    /// bytes from the start of its doubleword, writing each byte from its
    /// place in the doubleword sent, within the address's 2 KB page.
    #[test]
    fn repeat_mode() {
        let mut n64 = n64();
        let repeat = |n64: &mut N64, count: u32| n64.write_u32(0x0430_0000, 0x100 | (count - 1));
        let fill = |n64: &mut N64| (0..0x1000).for_each(|i| n64.write_u32(0x10_0000 + 4 * i, !0));
        fill(&mut n64);
        repeat(&mut n64, 7);
        n64.write_u8(0x10_0001, 0x1234_5678);
        assert_eq!(n64.read_u64(0x10_0000), 0xff78_0000_5678_00ff);
        assert_eq!(n64.read_u8(0x10_0001), 0x78);
        // A byte at 4 with a count of 4 writes nothing, and the mode is over.
        repeat(&mut n64, 4);
        n64.write_u8(0x10_0014, 0x1234_5678);
        assert_eq!(n64.read_u64(0x10_0010), !0);
        n64.write_u8(0x10_0014, 0x1234_5678);
        assert_eq!(n64.read_u64(0x10_0010), 0xffff_ffff_78ff_ffff);
        repeat(&mut n64, 13);
        n64.write_u64(0x10_0020, 0x1234_5678_9abc_def1);
        assert_eq!((n64.read_u64(0x10_0020), n64.read_u64(0x10_0028)), (0x1234_5678_9abc_def1, 0x1234_5678_9aff_ffff));
        fill(&mut n64);
        repeat(&mut n64, 128);
        n64.write_u64(0x10_07f8, 0);
        assert_eq!(n64.read_u64(0x10_07f8), 0);
        assert!((0..15).all(|i| n64.read_u64(0x10_0000 + 8 * i) == 0));
        assert_eq!((n64.read_u64(0x10_0078), n64.read_u64(0x10_0800)), (!0, !0));
    }
}
