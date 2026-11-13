//! The memory map, as the CPU sees it (KUSEG, KSEG0 and KSEG1 all reach
//! the same physical addresses):
//!
//! | Physical     | What                                              |
//! |--------------|---------------------------------------------------|
//! | 0000_0000    | RAM, 2 MB, mirrored four times                   |
//! | 1F00_0000    | expansion 1 (nothing: reads all 1s)               |
//! | 1F80_0000    | scratchpad, 1 KB (the data cache used as RAM)     |
//! | 1F80_1000    | I/O ports                                         |
//! | 1F80_2000    | expansion 2 (debug ports)                         |
//! | 1FC0_0000    | BIOS ROM, 512 KB                                  |
//! | FFFE_0130    | cache control (KSEG2)                             |
//!
//! With SR bit 16 set the cache is isolated: stores go to it, not to
//! memory (the BIOS clears the instruction cache that way).

use crate::Psx;

impl Psx {
    /// A word of RAM, the address masked to it.
    pub(crate) fn ram_word(&self, physical: u32) -> u32 {
        let i = physical as usize & 0x1f_fffc;
        u32::from_le_bytes(self.ram[i..i + 4].try_into().unwrap())
    }

    pub(crate) fn load32(&mut self, addr: u32) -> u32 {
        if addr >= 0xfffe_0000 {
            return if addr == 0xfffe_0130 { self.cache_control } else { 0 };
        }
        let physical = addr & 0x1fff_ffff;
        match physical {
            0x0000_0000..=0x007f_ffff => {
                self.cycles += 4;
                self.ram_word(physical)
            }
            0x1f80_0000..=0x1f80_03ff => word(&self.scratchpad, physical as usize & 0x3fc),
            _ => {
                self.cycles += load_cycles(physical, 2);
                match physical {
                    0x1f80_1000..=0x1f80_2fff => self.io_read(physical & !3, 4),
                    0x1fc0_0000..=0x1fc7_ffff => self.bios_word(physical),
                    _ => self.open_read(physical),
                }
            }
        }
    }

    pub(crate) fn load16(&mut self, addr: u32) -> u16 {
        let physical = addr & 0x1fff_ffff;
        self.cycles += load_cycles(physical, 1);
        match physical {
            0x0000_0000..=0x007f_ffff => {
                let i = physical as usize & 0x1f_fffe;
                u16::from_le_bytes([self.ram[i], self.ram[i + 1]])
            }
            0x1f80_0000..=0x1f80_03ff => {
                let i = physical as usize & 0x3fe;
                u16::from_le_bytes([self.scratchpad[i], self.scratchpad[i + 1]])
            }
            0x1f80_1000..=0x1f80_2fff => (self.io_read(physical & !3, 2) >> ((physical & 2) * 8)) as u16,
            0x1fc0_0000..=0x1fc7_ffff => (self.bios_word(physical) >> ((physical & 2) * 8)) as u16,
            _ => self.open_read(physical) as u16,
        }
    }

    pub(crate) fn load8(&mut self, addr: u32) -> u8 {
        let physical = addr & 0x1fff_ffff;
        self.cycles += load_cycles(physical, 0);
        match physical {
            0x0000_0000..=0x007f_ffff => self.ram[physical as usize & 0x1f_ffff],
            0x1f80_0000..=0x1f80_03ff => self.scratchpad[physical as usize & 0x3ff],
            0x1f80_1800..=0x1f80_1803 => self.cdrom_read(physical & 3),
            0x1f80_1000..=0x1f80_2fff => (self.io_read(physical & !3, 1) >> ((physical & 3) * 8)) as u8,
            0x1fc0_0000..=0x1fc7_ffff => (self.bios_word(physical) >> ((physical & 3) * 8)) as u8,
            _ => self.open_read(physical) as u8,
        }
    }

    /// A write to RAM where compiled code came from drops that code.
    fn code_written(&mut self, physical: u32) {
        if self.code_pages[(physical as usize & 0x1f_ffff) >> 12] != 0 {
            self.invalidate_code(physical);
        }
    }

    /// A word of the BIOS ROM (none when the BIOS is answered in HLE).
    fn bios_word(&self, physical: u32) -> u32 {
        if self.bios.is_empty() { 0 } else { word(&self.bios, physical as usize & 0x7_fffc) }
    }

    /// What nothing reads as.
    fn open_read(&self, physical: u32) -> u32 {
        match physical {
            0x1f00_0000..=0x1f7f_ffff => 0xffff_ffff,
            _ => 0,
        }
    }

    fn isolated(&self) -> bool {
        self.cpu.sr & 1 << 16 != 0
    }

    pub(crate) fn store32(&mut self, addr: u32, value: u32) {
        if addr >= 0xfffe_0000 {
            if addr == 0xfffe_0130 {
                self.cache_control = value;
            }
            return;
        }
        if self.isolated() {
            return;
        }
        let physical = addr & 0x1fff_ffff;
        match physical {
            0x0000_0000..=0x007f_ffff => {
                self.code_written(physical);
                set_word(&mut self.ram[..], physical as usize & 0x1f_fffc, value)
            }
            0x1f80_0000..=0x1f80_03ff => set_word(&mut self.scratchpad, physical as usize & 0x3fc, value),
            0x1f80_1000..=0x1f80_2fff => self.io_write(physical & !3, value, 4),
            _ => {}
        }
    }

    pub(crate) fn store16(&mut self, addr: u32, value: u16) {
        if self.isolated() {
            return;
        }
        let physical = addr & 0x1fff_ffff;
        match physical {
            0x0000_0000..=0x007f_ffff => {
                self.code_written(physical);
                let i = physical as usize & 0x1f_fffe;
                self.ram[i..i + 2].copy_from_slice(&value.to_le_bytes());
            }
            0x1f80_0000..=0x1f80_03ff => {
                let i = physical as usize & 0x3fe;
                self.scratchpad[i..i + 2].copy_from_slice(&value.to_le_bytes());
            }
            // The halfword on its lane of the word.
            0x1f80_1000..=0x1f80_2fff => {
                self.io_write(physical & !3, u32::from(value) << ((physical & 2) * 8), 2 | (physical & 2) << 4)
            }
            _ => {}
        }
    }

    pub(crate) fn store8(&mut self, addr: u32, value: u8) {
        if self.isolated() {
            return;
        }
        let physical = addr & 0x1fff_ffff;
        match physical {
            0x0000_0000..=0x007f_ffff => {
                self.code_written(physical);
                self.ram[physical as usize & 0x1f_ffff] = value;
            }
            0x1f80_0000..=0x1f80_03ff => self.scratchpad[physical as usize & 0x3ff] = value,
            0x1f80_1000..=0x1f80_2fff => {
                self.io_write(physical & !3, u32::from(value) << ((physical & 3) * 8), 1 | (physical & 3) << 4)
            }
            _ => {}
        }
    }

    /// A read of an I/O port's word (the narrower reads take their part).
    fn io_read(&mut self, addr: u32, _width: u32) -> u32 {
        match addr {
            0x1f80_1000..=0x1f80_1020 => self.memctrl[((addr - 0x1f80_1000) / 4) as usize],
            0x1f80_1060 => self.ram_size,
            // SIO0: its halfword registers two to a word.
            0x1f80_1040 => self.sio.read(0),
            0x1f80_1044 => self.sio.read(4),
            0x1f80_1048 => self.sio.read(8) | self.sio.read(0xa) << 16,
            0x1f80_104c => self.sio.read(0xe) << 16,
            // SIO1: nothing received, always ready to send.
            0x1f80_1050 => 0xff,
            0x1f80_1054 => 0x0000_0005,
            0x1f80_1058 => u32::from(self.sio1[0]) | u32::from(self.sio1[1]) << 16,
            0x1f80_105c => u32::from(self.sio1[2]) | u32::from(self.sio1[3]) << 16,
            0x1f80_1070 => self.irq_stat,
            0x1f80_1074 => self.irq_mask,
            0x1f80_1080..=0x1f80_10ff => self.dma_read(addr),
            0x1f80_1100..=0x1f80_112f => {
                let cycles = self.cycles;
                self.timers.read(addr, cycles)
            }
            0x1f80_1800 => (0..4).fold(0, |w, port| w | u32::from(self.cdrom_read(port)) << (8 * port)),
            0x1f80_1820 => self.mdec.read(),
            0x1f80_1824 => self.mdec.status(),
            0x1f80_1810 => self.gpu.read(),
            0x1f80_1814 => self.gpu.status(self.line),
            0x1f80_1c00..=0x1f80_1fff => {
                let offset = addr - 0x1f80_1c00;
                u32::from(self.spu_read(offset)) | u32::from(self.spu_read(offset + 2)) << 16
            }
            _ => 0,
        }
    }

    /// A write to an I/O port: `width` is 1, 2 or 4, with the byte lane of
    /// a narrower one in its bits 4-5; the value is on its lane.
    pub(crate) fn io_write(&mut self, addr: u32, value: u32, width: u32) {
        match addr {
            0x1f80_1000..=0x1f80_1020 => self.memctrl[((addr - 0x1f80_1000) / 4) as usize] = value,
            0x1f80_1060 => self.ram_size = value,
            // Writing 0 bits acknowledges interrupts.
            0x1f80_1070 => {
                self.irq_stat &= value | !lane_mask(width);
                self.update_irq_line();
            }
            0x1f80_1074 => {
                self.irq_mask = value & 0x7ff;
                self.update_irq_line();
            }
            0x1f80_1080..=0x1f80_10ff => self.dma_write(addr, value, width),
            0x1f80_1100..=0x1f80_112f => {
                let cycles = self.cycles;
                self.timers.write(addr, value, cycles);
                self.timers_irq();
            }
            0x1f80_1040 => self.sio_write(0, value & 0xff),
            0x1f80_1048 => {
                let mask = lane_mask(width);
                if mask & 0xffff != 0 {
                    self.sio_write(8, value & 0xffff);
                }
                if mask >> 16 != 0 {
                    self.sio_write(0xa, value >> 16);
                }
            }
            0x1f80_104c => {
                if lane_mask(width) >> 16 != 0 {
                    self.sio_write(0xe, value >> 16);
                }
            }
            0x1f80_1058 | 0x1f80_105c => {
                let mask = lane_mask(width);
                let i = (addr as usize - 0x1f80_1058) / 2;
                if mask & 0xffff != 0 {
                    self.sio1[i] = value as u16;
                }
                if mask >> 16 != 0 {
                    self.sio1[i + 1] = (value >> 16) as u16;
                }
            }
            // The CD-ROM's byte ports, each byte a write of its own.
            0x1f80_1800 => {
                let mask = lane_mask(width);
                for port in 0..4 {
                    if mask >> (8 * port) & 0xff != 0 {
                        self.cdrom_write(port, (value >> (8 * port)) as u8);
                    }
                }
            }
            0x1f80_1820 => {
                self.mdec.write(value);
                self.mdec_output_ready();
            }
            0x1f80_1824 => self.mdec.control(value),
            0x1f80_1810 => self.gpu.gp0(value),
            0x1f80_1814 => self.gpu.gp1(value),
            // Halfword registers: a byte written goes to its halfword.
            0x1f80_1c00..=0x1f80_1fff => {
                let offset = addr - 0x1f80_1c00;
                match (width & 7, width >> 4 & 2) {
                    (4, _) => {
                        self.spu_write(offset, value as u16);
                        self.spu_write(offset + 2, (value >> 16) as u16);
                    }
                    (_, lane) => self.spu_write(offset + lane, (value >> (lane * 8)) as u16),
                }
            }
            // Expansion 2's debug UART: a TTY some programs print to.
            0x1f80_2020 if width & 0x30 == 0x30 => self.tty.push((value >> 24) as u8),
            _ => {}
        }
    }
}

/// The cycles a load waits for the bus, past the instruction's own: by
/// where it reads and how wide (0 a byte, 1 a halfword, 2 a word), as
/// cpu/access-time measures them. The 8-bit buses (the BIOS, the expansion
/// ports, the CD-ROM) take a byte at a time.
fn load_cycles(physical: u32, size: usize) -> u64 {
    match physical {
        0x0000_0000..=0x007f_ffff => 4,
        0x1f80_0000..=0x1f80_03ff => 0,
        0x1f80_1800..=0x1f80_180f => [7, 13, 25][size],
        0x1f80_1c00..=0x1f80_1fff => [17, 17, 38][size],
        0x1f80_1000..=0x1f80_1fff => 2,
        0x1f80_2000..=0x1f80_3fff => [10, 25, 55][size],
        0x1fa0_0000..=0x1fbf_ffff => [6, 5, 9][size],
        0x1fc0_0000..=0x1fc7_ffff => [6, 12, 24][size],
        0x1f00_0000..=0x1f7f_ffff => [6, 13, 25][size],
        _ => 0,
    }
}

/// The bits of the word a write of `width` (with its lane) covers.
fn lane_mask(width: u32) -> u32 {
    let lane = width >> 4 & 3;
    match width & 7 {
        1 => 0xff << (lane * 8),
        2 => 0xffff << (lane * 8),
        _ => 0xffff_ffff,
    }
}

fn word(memory: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(memory[i..i + 4].try_into().unwrap())
}

fn set_word(memory: &mut [u8], i: usize, value: u32) {
    memory[i..i + 4].copy_from_slice(&value.to_le_bytes());
}
