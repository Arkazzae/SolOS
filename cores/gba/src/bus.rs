//! The memory map, what each access costs, and the I/O registers.
//!
//! | Address    | What                                   | Bus, waits         |
//! |------------|----------------------------------------|--------------------|
//! | 0000_0000  | BIOS, 16 KB                            | 32-bit             |
//! | 0200_0000  | work RAM on the board, 256 KB          | 16-bit, 2 waits    |
//! | 0300_0000  | work RAM in the chip, 32 KB            | 32-bit             |
//! | 0400_0000  | I/O registers                          | 32-bit             |
//! | 0500_0000  | palettes, 1 KB                         | 16-bit             |
//! | 0600_0000  | VRAM, 96 KB                            | 16-bit             |
//! | 0700_0000  | OAM (sprites), 1 KB                    | 32-bit             |
//! | 0800_0000  | cartridge ROM, three times over        | 16-bit, WAITCNT    |
//! | 0E00_0000  | cartridge save memory (SRAM, Flash)    | 8-bit, WAITCNT     |
//!
//! Each region repeats through its 16 MB. Reading where nothing answers
//! gives the bus's last value: the opcode the CPU fetched last.

use crate::Gba;
use savestate::state;

/// The cycles one access takes in each 16 MB region (and past them all),
/// by width and whether it follows on from the last.
#[derive(Clone)]
pub struct Waits {
    n16: [u8; 17],
    s16: [u8; 17],
    n32: [u8; 17],
    s32: [u8; 17],
}

impl Waits {
    /// The costs WAITCNT sets for the cartridge, and the internal memory
    /// control for the board's work RAM.
    pub fn new(waitcnt: u16, ewram_waits: u8) -> Waits {
        let mut n16 = [1u8; 17];
        let mut s16 = [1u8; 17];
        n16[2] = 1 + ewram_waits;
        s16[2] = 1 + ewram_waits;
        let first = [4, 3, 2, 8];
        let w = usize::from(waitcnt);
        let rom = [
            (first[w >> 2 & 3], [2, 1][w >> 4 & 1]),
            (first[w >> 5 & 3], [4, 1][w >> 7 & 1]),
            (first[w >> 8 & 3], [8, 1][w >> 10 & 1]),
        ];
        for (i, (n, s)) in rom.into_iter().enumerate() {
            n16[8 + 2 * i] = 1 + n;
            n16[9 + 2 * i] = 1 + n;
            s16[8 + 2 * i] = 1 + s;
            s16[9 + 2 * i] = 1 + s;
        }
        let sram = 1 + first[w & 3];
        for region in [0xe, 0xf] {
            n16[region] = sram;
            s16[region] = sram;
        }
        let (mut n32, mut s32) = (n16, s16);
        // 16-bit buses take two accesses for a word, the second sequential.
        for region in [2, 5, 6, 8, 9, 0xa, 0xb, 0xc, 0xd] {
            n32[region] = n16[region] + s16[region];
            s32[region] = 2 * s16[region];
        }
        Waits { n16, s16, n32, s32 }
    }

    fn region(addr: u32) -> usize {
        (addr >> 24).min(16) as usize
    }

    pub fn half(&self, addr: u32, seq: bool) -> u64 {
        let r = Waits::region(addr);
        u64::from(if seq { self.s16[r] } else { self.n16[r] })
    }

    pub fn word(&self, addr: u32, seq: bool) -> u64 {
        let r = Waits::region(addr);
        u64::from(if seq { self.s32[r] } else { self.n32[r] })
    }
}

/// The cartridge's prefetch buffer (WAITCNT bit 14): while the CPU is busy
/// elsewhere, the cartridge goes on reading the halfwords after the last
/// opcode, up to eight, which the CPU then fetches in a cycle.
#[derive(Clone, Default)]
pub struct Prefetch {
    active: bool,
    /// The halfword the buffer starts at, how many it holds, the cycles
    /// spent on the one after them, and when the cartridge last read for
    /// the CPU.
    next: u32,
    count: u32,
    progress: u64,
    since: u64,
}

state!(Prefetch { active, next, count, progress, since });

/// The registers of the system: interrupts, waits, keypad, link port.
#[derive(Clone)]
pub struct System {
    pub ie: u16,
    pub requested: u16,
    pub ime: bool,
    pub waitcnt: u16,
    /// The internal memory control (at 0400_0800, repeated every 64 KB).
    pub memory_control: u32,
    /// Buttons held (set bits), and the keypad interrupt's condition.
    pub keys: u16,
    pub keycnt: u16,
    pub postflg: u8,
    /// The link port's registers, kept as written: no other console
    /// answers.
    pub serial: [u16; 8],
    pub rcnt: u16,
    pub joycnt: u16,
}

state!(System { ie, requested, ime, waitcnt, memory_control, keys, keycnt, postflg, serial, rcnt, joycnt });

impl System {
    pub fn new() -> System {
        System {
            ie: 0,
            requested: 0,
            ime: false,
            waitcnt: 0,
            memory_control: 0x0d00_0020,
            keys: 0,
            keycnt: 0,
            postflg: 1,
            serial: [0; 8],
            rcnt: 0x8000,
            joycnt: 0,
        }
    }
}

fn word(memory: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(memory[i..i + 4].try_into().unwrap())
}

fn half(memory: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([memory[i], memory[i + 1]])
}

fn set_word(memory: &mut [u8], i: usize, value: u32) {
    memory[i..i + 4].copy_from_slice(&value.to_le_bytes());
}

fn set_half(memory: &mut [u8], i: usize, value: u16) {
    memory[i..i + 2].copy_from_slice(&value.to_le_bytes());
}

/// Where a VRAM address lands: the 96 KB repeat every 128 KB, the last
/// 32 KB of each repeat showing the 32 KB before them.
pub fn vram_index(addr: u32) -> usize {
    let i = addr as usize & 0x1ffff;
    if i >= 0x18000 { i - 0x8000 } else { i }
}

impl Gba {
    /// Where the CPU's access to a VRAM address lands. In the bitmap modes
    /// the repeat of the 16 KB the bitmap takes from the sprites is not
    /// there: reads give 0 and writes go nowhere.
    fn vram_at(&self, addr: u32) -> Option<usize> {
        let i = addr as usize & 0x1ffff;
        if (0x18000..0x1c000).contains(&i) && self.ppu.background_vram() > 0x10000 {
            None
        } else {
            Some(vram_index(addr))
        }
    }

    /// Fetches an opcode: most come from the cartridge or the chip's
    /// work RAM; those from the BIOS are what reads of it give later.
    pub(crate) fn fetch32(&mut self, addr: u32) -> u32 {
        let seq = self.cpu.seq;
        self.cpu.seq = true;
        match addr >> 24 {
            0x08..=0x0d => {
                self.cycles += self.cartridge_fetch(addr, 2, seq);
                return u32::from(self.cart.read_rom(addr & !3)) | u32::from(self.cart.read_rom(addr | 2)) << 16;
            }
            _ => self.cycles += self.waits.word(addr, seq),
        }
        match addr >> 24 {
            0x03 => word(&self.iwram, addr as usize & 0x7ffc),
            0x00 if addr < 0x4000 => {
                self.bios.latch = word(&self.bios.rom, addr as usize & 0x3ffc);
                self.bios.latch
            }
            _ => self.load32(addr),
        }
    }

    pub(crate) fn fetch16(&mut self, addr: u32) -> u16 {
        let seq = self.cpu.seq;
        self.cpu.seq = true;
        match addr >> 24 {
            0x08..=0x0d => {
                self.cycles += self.cartridge_fetch(addr, 1, seq);
                return self.cart.read_rom(addr);
            }
            _ => self.cycles += self.waits.half(addr, seq),
        }
        match addr >> 24 {
            0x03 => half(&self.iwram, addr as usize & 0x7ffe),
            0x00 if addr < 0x4000 => {
                self.bios.latch = word(&self.bios.rom, addr as usize & 0x3ffc);
                (self.bios.latch >> ((addr & 2) * 8)) as u16
            }
            _ => self.load16(addr),
        }
    }

    /// What fetching an opcode of one or two halfwords from the cartridge
    /// costs. With the prefetch buffer on, the time since the cartridge
    /// last read for the CPU has filled the buffer: an opcode in it takes
    /// a cycle, the one being read the rest of its time, any other a whole
    /// access, after which the buffer reads on from there.
    fn cartridge_fetch(&mut self, addr: u32, halfwords: u32, seq: bool) -> u64 {
        // Every 128 KB the cartridge's address counter starts over.
        let seq = seq && addr & 0x1_fffe != 0;
        let prefetching = self.system.waitcnt & 0x4000 != 0;
        let p = &mut self.prefetch;
        if !prefetching || !(p.active && addr == p.next) {
            let access = if halfwords == 2 { self.waits.word(addr, seq) } else { self.waits.half(addr, seq) };
            if prefetching {
                p.active = true;
                p.count = 0;
                p.progress = 0;
                p.next = addr + 2 * halfwords;
                p.since = self.cycles + access;
            }
            return access;
        }
        // The halfwords read meanwhile, a step each, up to a full buffer
        // (counted, not divided: the console's CPU has no division).
        let step = self.waits.half(addr, true) as u32;
        let mut progress = p.progress as u32 + (self.cycles - p.since).min(255) as u32;
        while p.count < 8 && progress >= step {
            progress -= step;
            p.count += 1;
        }
        if p.count == 8 {
            progress = 0;
        }
        let mut wait = 0;
        for _ in 0..halfwords {
            if p.count > 0 {
                p.count -= 1;
            } else {
                wait += step - progress;
                progress = 0;
            }
        }
        let cost = u64::from(wait.max(1));
        p.progress = u64::from(progress);
        p.next = addr + 2 * halfwords;
        p.since = self.cycles + cost;
        cost
    }

    /// A word, from the word-aligned address below.
    pub(crate) fn read32(&mut self, addr: u32, seq: bool) -> u32 {
        self.cycles += self.waits.word(addr, seq);
        self.load32(addr)
    }

    /// LDR from an address that is not word-aligned: the aligned word,
    /// rotated so the addressed byte comes first.
    pub(crate) fn read32_rotated(&mut self, addr: u32, seq: bool) -> u32 {
        self.read32(addr, seq).rotate_right((addr & 3) * 8)
    }

    pub(crate) fn read16(&mut self, addr: u32, seq: bool) -> u16 {
        self.cycles += self.waits.half(addr, seq);
        self.load16(addr)
    }

    /// LDRH from an odd address: the halfword, rotated by a byte.
    pub(crate) fn read16_rotated(&mut self, addr: u32, seq: bool) -> u32 {
        u32::from(self.read16(addr, seq)).rotate_right((addr & 1) * 8)
    }

    /// LDRSH; from an odd address it loads the byte, as LDRSB.
    pub(crate) fn read16_signed(&mut self, addr: u32, seq: bool) -> u32 {
        if addr & 1 != 0 { self.read8(addr, seq) as i8 as u32 } else { self.read16(addr, seq) as i16 as u32 }
    }

    pub(crate) fn read8(&mut self, addr: u32, seq: bool) -> u8 {
        self.cycles += self.waits.half(addr, seq);
        self.load8(addr)
    }

    pub(crate) fn write32(&mut self, addr: u32, value: u32, seq: bool) {
        self.cycles += self.waits.word(addr, seq);
        self.store32(addr, value);
    }

    pub(crate) fn write16(&mut self, addr: u32, value: u16, seq: bool) {
        self.cycles += self.waits.half(addr, seq);
        self.store16(addr, value);
    }

    pub(crate) fn write8(&mut self, addr: u32, value: u8, seq: bool) {
        self.cycles += self.waits.half(addr, seq);
        self.store8(addr, value);
    }

    /// What reads where nothing answers give: the last opcode fetched, or
    /// in Thumb state the last two halfwords, in an order that depends on
    /// where the code runs.
    pub(crate) fn open_bus(&self) -> u32 {
        let [current, next] = self.cpu.pipe;
        if !self.cpu.thumb() {
            return next;
        }
        let pc = self.cpu.r[15];
        let aligned = pc & 2 == 0;
        match pc >> 24 {
            0x00 | 0x07 if aligned => next | u32::from(self.peek16(pc.wrapping_add(2))) << 16,
            0x03 if aligned => next | current << 16,
            0x00 | 0x03 | 0x07 => current | next << 16,
            _ => next | next << 16,
        }
    }

    /// A halfword of BIOS or OAM, without the effects of a read.
    fn peek16(&self, addr: u32) -> u16 {
        match addr >> 24 {
            0x00 => half(&self.bios.rom, addr as usize & 0x3ffe),
            _ => half(&self.ppu.oam, addr as usize & 0x3fe),
        }
    }

    /// The BIOS can only be read by code running in it; from anywhere else
    /// a read gives the last opcode fetched from it.
    fn bios_word(&self, addr: u32) -> u32 {
        if addr >= 0x4000 {
            self.open_bus()
        } else if self.cpu.r[15] < 0x4000 {
            word(&self.bios.rom, addr as usize & 0x3ffc)
        } else {
            self.bios.latch
        }
    }

    pub(crate) fn load32(&mut self, addr: u32) -> u32 {
        match addr >> 24 {
            0x00 => self.bios_word(addr),
            0x02 => word(&self.ewram, addr as usize & 0x3fffc),
            0x03 => word(&self.iwram, addr as usize & 0x7ffc),
            0x04 => u32::from(self.io_read16(addr & !3)) | u32::from(self.io_read16((addr & !3) | 2)) << 16,
            0x05 => word(&self.ppu.palette, addr as usize & 0x3fc),
            0x06 => self.vram_at(addr).map_or(0, |i| word(&self.ppu.vram, i & !3)),
            0x07 => word(&self.ppu.oam, addr as usize & 0x3fc),
            0x08..=0x0d => {
                let addr = addr & !3;
                u32::from(self.rom16(addr)) | u32::from(self.rom16(addr | 2)) << 16
            }
            // An 8-bit bus: the byte comes on all four lanes.
            0x0e | 0x0f => u32::from(self.cart.read_backup(addr)) * 0x0101_0101,
            _ => self.open_bus(),
        }
    }

    pub(crate) fn load16(&mut self, addr: u32) -> u16 {
        match addr >> 24 {
            0x00 => (self.bios_word(addr) >> ((addr & 2) * 8)) as u16,
            0x02 => half(&self.ewram, addr as usize & 0x3fffe),
            0x03 => half(&self.iwram, addr as usize & 0x7ffe),
            0x04 => self.io_read16(addr & !1),
            0x05 => half(&self.ppu.palette, addr as usize & 0x3fe),
            0x06 => self.vram_at(addr).map_or(0, |i| half(&self.ppu.vram, i & !1)),
            0x07 => half(&self.ppu.oam, addr as usize & 0x3fe),
            0x08..=0x0d => self.rom16(addr & !1),
            0x0e | 0x0f => u16::from(self.cart.read_backup(addr)) * 0x0101,
            _ => (self.open_bus() >> ((addr & 2) * 8)) as u16,
        }
    }

    pub(crate) fn load8(&mut self, addr: u32) -> u8 {
        match addr >> 24 {
            0x00 => (self.bios_word(addr) >> ((addr & 3) * 8)) as u8,
            0x02 => self.ewram[addr as usize & 0x3ffff],
            0x03 => self.iwram[addr as usize & 0x7fff],
            0x04 => (self.io_read16(addr & !1) >> ((addr & 1) * 8)) as u8,
            0x05 => self.ppu.palette[addr as usize & 0x3ff],
            0x06 => self.vram_at(addr).map_or(0, |i| self.ppu.vram[i]),
            0x07 => self.ppu.oam[addr as usize & 0x3ff],
            0x08..=0x0d => (self.rom16(addr & !1) >> ((addr & 1) * 8)) as u8,
            0x0e | 0x0f => self.cart.read_backup(addr),
            _ => (self.open_bus() >> ((addr & 3) * 8)) as u8,
        }
    }

    /// A halfword of the cartridge: ROM, or the EEPROM where it answers. A
    /// read of data stops the prefetch buffer.
    fn rom16(&mut self, addr: u32) -> u16 {
        self.prefetch.active = false;
        if self.cart.eeprom_at(addr) {
            return self.cart.read_eeprom();
        }
        self.cart.read_gpio(addr).unwrap_or_else(|| self.cart.read_rom(addr))
    }

    /// A write ends a loop's idling, unless it is to work RAM and leaves it
    /// as it was, or is to IWRAM, which the loop judges by the stack (see
    /// IdleLoop).
    fn touch(&mut self, addr: u32) {
        if !matches!(addr >> 24, 0x02 | 0x03) {
            self.idle_loop.touched = true;
        }
    }

    pub(crate) fn store32(&mut self, addr: u32, value: u32) {
        self.touch(addr);
        match addr >> 24 {
            0x02 => {
                let i = addr as usize & 0x3fffc;
                self.idle_loop.touched |= word(&self.ewram, i) != value;
                set_word(&mut self.ewram, i, value);
            }
            0x03 => {
                let i = addr as usize & 0x7ffc;
                if word(&self.iwram, i) != value {
                    self.idle_loop.iwram_end = self.idle_loop.iwram_end.max(i as u32 + 4);
                }
                set_word(&mut self.iwram, i, value);
            }
            0x04 => {
                let addr = addr & !3;
                for i in 0..4 {
                    self.io_write8(addr + i, (value >> (8 * i)) as u8);
                }
            }
            0x05 => {
                let i = addr as usize & 0x3fc;
                set_word(&mut self.ppu.palette, i, value);
                self.ppu.palette_written(i, i + 4);
            }
            0x06 => {
                if let Some(i) = self.vram_at(addr) {
                    set_word(&mut self.ppu.vram, i & !3, value);
                }
            }
            0x07 => {
                set_word(&mut self.ppu.oam, addr as usize & 0x3fc, value);
                self.ppu.oam_changed = true;
            }
            0x08..=0x0d => {
                self.rom_write16(addr & !3, value as u16);
                self.rom_write16((addr & !3) | 2, (value >> 16) as u16);
            }
            // The byte on the lane of the address.
            0x0e | 0x0f => self.cart.write_backup(addr, (value >> ((addr & 3) * 8)) as u8),
            _ => {}
        }
    }

    pub(crate) fn store16(&mut self, addr: u32, value: u16) {
        self.touch(addr);
        match addr >> 24 {
            0x02 => {
                let i = addr as usize & 0x3fffe;
                self.idle_loop.touched |= half(&self.ewram, i) != value;
                set_half(&mut self.ewram, i, value);
            }
            0x03 => {
                let i = addr as usize & 0x7ffe;
                if half(&self.iwram, i) != value {
                    self.idle_loop.iwram_end = self.idle_loop.iwram_end.max(i as u32 + 2);
                }
                set_half(&mut self.iwram, i, value);
            }
            0x04 => {
                self.io_write8(addr & !1, value as u8);
                self.io_write8(addr | 1, (value >> 8) as u8);
            }
            0x05 => {
                let i = addr as usize & 0x3fe;
                set_half(&mut self.ppu.palette, i, value);
                self.ppu.palette_written(i, i + 2);
            }
            0x06 => {
                if let Some(i) = self.vram_at(addr) {
                    set_half(&mut self.ppu.vram, i & !1, value);
                }
            }
            0x07 => {
                set_half(&mut self.ppu.oam, addr as usize & 0x3fe, value);
                self.ppu.oam_changed = true;
            }
            0x08..=0x0d => self.rom_write16(addr & !1, value),
            0x0e | 0x0f => self.cart.write_backup(addr, (value >> ((addr & 1) * 8)) as u8),
            _ => {}
        }
    }

    /// Byte writes to palettes and to the backgrounds' VRAM write the byte
    /// to both halves of its halfword; to sprites' VRAM and OAM they do
    /// nothing.
    pub(crate) fn store8(&mut self, addr: u32, value: u8) {
        self.touch(addr);
        match addr >> 24 {
            0x02 => {
                let i = addr as usize & 0x3ffff;
                self.idle_loop.touched |= self.ewram[i] != value;
                self.ewram[i] = value;
            }
            0x03 => {
                let i = addr as usize & 0x7fff;
                if self.iwram[i] != value {
                    self.idle_loop.iwram_end = self.idle_loop.iwram_end.max(i as u32 + 1);
                }
                self.iwram[i] = value;
            }
            0x04 => self.io_write8(addr, value),
            0x05 => {
                let i = addr as usize & 0x3fe;
                set_half(&mut self.ppu.palette, i, u16::from(value) * 0x0101);
                self.ppu.palette_written(i, i + 2);
            }
            0x06 => {
                let i = vram_index(addr);
                if i < self.ppu.background_vram() && self.vram_at(addr).is_some() {
                    set_half(&mut self.ppu.vram, i & !1, u16::from(value) * 0x0101);
                }
            }
            0x07 => {}
            0x08..=0x0d => self.rom_write16(addr & !1, u16::from(value)),
            0x0e | 0x0f => self.cart.write_backup(addr, value),
            _ => {}
        }
    }

    /// Writes to the cartridge reach the EEPROM, or its clock's port.
    fn rom_write16(&mut self, addr: u32, value: u16) {
        self.prefetch.active = false;
        if self.cart.eeprom_at(addr) {
            self.cart.write_eeprom(value);
        } else {
            self.cart.write_gpio(addr, value, self.cycles);
        }
    }

    /// The I/O register at an even address; write-only and missing ones
    /// read as the bus.
    pub(crate) fn io_read16(&mut self, addr: u32) -> u16 {
        let reg = addr & 0x00ff_fffe;
        // Only the keys stay as they are over a frame's events.
        self.idle_loop.io_read |= reg != 0x130;
        let value = match reg {
            0x000..=0x05e => self.ppu.read_register(reg),
            0x060..=0x0ae => self.apu_read_register(reg),
            0x0b0..=0x0de => self.dma_read_register(reg),
            0x100..=0x10e => Some(self.timer_read_register(reg)),
            0x120..=0x12e => Some(self.system.serial[(reg as usize - 0x120) / 2]),
            0x130 => Some(!self.system.keys & 0x3ff),
            0x132 => Some(self.system.keycnt),
            0x134 => Some(self.system.rcnt),
            0x136 | 0x142 | 0x15a => Some(0),
            0x140 => Some(self.system.joycnt),
            0x150..=0x158 => Some(0),
            0x200 => Some(self.system.ie),
            0x202 => Some(self.system.requested),
            0x204 => Some(self.system.waitcnt),
            0x206 | 0x20a => Some(0),
            0x208 => Some(u16::from(self.system.ime)),
            0x300 => Some(u16::from(self.system.postflg)),
            0x302 => Some(0),
            _ if reg & 0xfffc == 0x800 => Some((self.system.memory_control >> ((reg & 2) * 8)) as u16),
            _ => None,
        };
        value.unwrap_or_else(|| (self.open_bus() >> ((addr & 2) * 8)) as u16)
    }

    pub(crate) fn io_write8(&mut self, addr: u32, value: u8) {
        let reg = addr & 0x00ff_ffff;
        match reg {
            0x000..=0x05f => self.ppu.write_register(reg, value),
            0x060..=0x0af => self.apu_write_register(reg, value),
            0x0b0..=0x0df => self.dma_write_register(reg, value),
            0x100..=0x10f => self.timer_write_register(reg, value),
            0x120..=0x12f => set_byte(&mut self.system.serial[(reg as usize - 0x120) / 2], reg, value),
            0x132 | 0x133 => {
                set_byte(&mut self.system.keycnt, reg, value);
                self.system.keycnt &= 0xc3ff;
                self.check_keypad();
            }
            0x134 | 0x135 => set_byte(&mut self.system.rcnt, reg, value),
            0x140 | 0x141 => set_byte(&mut self.system.joycnt, reg, value),
            0x200 | 0x201 => {
                set_byte(&mut self.system.ie, reg, value);
                self.system.ie &= 0x3fff;
                self.update_irq();
            }
            // Writing 1s acknowledges interrupts.
            0x202 | 0x203 => {
                self.system.requested &= !(u16::from(value) << ((reg & 1) * 8));
                self.update_irq();
            }
            0x204 | 0x205 => {
                set_byte(&mut self.system.waitcnt, reg, value);
                self.system.waitcnt &= 0x5fff;
                self.update_waits();
            }
            0x208 => {
                self.system.ime = value & 1 != 0;
                self.update_irq();
            }
            0x300 => self.system.postflg = value & 1,
            // HALTCNT: bit 7 asks for stop mode, which we treat as halt.
            0x301 => self.cpu.halted = true,
            _ if reg & 0xfffc == 0x800 => {
                let shift = (reg & 3) * 8;
                self.system.memory_control = self.system.memory_control & !(0xff << shift) | u32::from(value) << shift;
                self.update_waits();
            }
            _ => {}
        }
    }

    pub(crate) fn update_waits(&mut self) {
        let ewram = 15 - (self.system.memory_control >> 24 & 15) as u8;
        self.waits = Waits::new(self.system.waitcnt, ewram);
    }
}

/// Writes one byte of a 16-bit register: the low one at an even address.
pub fn set_byte(reg: &mut u16, addr: u32, value: u8) {
    if addr & 1 == 0 {
        *reg = *reg & 0xff00 | u16::from(value);
    } else {
        *reg = *reg & 0x00ff | u16::from(value) << 8;
    }
}
