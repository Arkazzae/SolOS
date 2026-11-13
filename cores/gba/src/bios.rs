//! The BIOS. Nintendo's is not ours to ship, so the core has its own: a
//! 16 KB ROM holding only the exception vectors and the interrupt
//! dispatcher (the same few opcodes as Nintendo's, at the same addresses,
//! since games and tests read them back), and the BIOS calls (SWI) done
//! in Rust when a game makes them. The calls give the same results as
//! Nintendo's, in roughly its time.

use crate::Gba;
use crate::cpu::Cpu;
use savestate::state;

/// The opcodes of the ROM, by address.
const ROM: [(usize, u32); 17] = [
    // Reset: as SoftReset.
    (0x00, 0xef00_0000), // swi 0
    // Undefined instructions are skipped; SWI never gets here.
    (0x04, 0xe1b0_f00e), // movs pc, lr
    (0x08, 0xe1b0_f00e), // movs pc, lr
    (0x0c, 0xe25e_f004), // subs pc, lr, #4
    (0x10, 0xe25e_f008), // subs pc, lr, #8
    (0x14, 0xeaff_fffe), // b .
    (0x18, 0xea00_0042), // b 0x128
    (0x1c, 0xe25e_f004), // subs pc, lr, #4
    // The interrupt dispatcher: calls the game's handler, whose address
    // the game leaves at 0300_7FFC (seen at 03FF_FFFC), in ARM state.
    (0x128, 0xe92d_500f), // stmfd sp!, {r0-r3, r12, lr}
    (0x12c, 0xe3a0_0301), // mov r0, #0x04000000
    (0x130, 0xe28f_e000), // add lr, pc, #0
    (0x134, 0xe510_f004), // ldr pc, [r0, #-4]
    (0x138, 0xe8bd_500f), // ldmfd sp!, {r0-r3, r12, lr}
    (0x13c, 0xe25e_f004), // subs pc, lr, #4
    // What reads of the BIOS give after an interrupt, after startup and
    // after a BIOS call: the opcodes Nintendo's has there.
    (0x144, 0xe55e_c002),
    (0xe4, 0xe129_f000),
    (0x190, 0xe3a0_2004),
];

const AFTER_STARTUP: u32 = 0xe129_f000;
const AFTER_SWI: u32 = 0xe3a0_2004;

#[derive(Clone)]
pub struct Bios {
    pub rom: Vec<u8>,
    /// The last opcode fetched from the BIOS, which reads of it from
    /// outside give.
    pub latch: u32,
    /// IntrWait is waiting: the call runs again after each interrupt,
    /// until one it waits for has come.
    waiting: bool,
    /// The BIOS's sine table: sin(i/256 of a turn), 1.14 fixed point.
    sine: Vec<i16>,
}

state!(Bios { latch, waiting });

impl Bios {
    pub fn new() -> Bios {
        let mut rom = vec![0; 0x4000];
        for (addr, op) in ROM {
            rom[addr..addr + 4].copy_from_slice(&op.to_le_bytes());
        }
        let sine =
            (0..256).map(|i| ((i as f64 * std::f64::consts::TAU / 256.0).sin() * 16384.0).round() as i16).collect();
        Bios { rom, latch: AFTER_STARTUP, waiting: false, sine }
    }

    fn sin(&self, angle: u16) -> i32 {
        i32::from(self.sine[usize::from(angle >> 8)])
    }

    fn cos(&self, angle: u16) -> i32 {
        i32::from(self.sine[usize::from((angle >> 8).wrapping_add(64) & 0xff)])
    }
}

/// The BIOS's arctangent of a 1.14 fixed-point number: a polynomial
/// whose result runs 0-0x2000 for 0-45 degrees.
fn arctan(i: i32) -> i32 {
    let a = -(i.wrapping_mul(i) >> 14);
    let mut b = (0xa9i32.wrapping_mul(a) >> 14) + 0x390;
    for c in [0x91c, 0xfb6, 0x16aa, 0x2081, 0x3651, 0xa2f9] {
        b = (b.wrapping_mul(a) >> 14) + c;
    }
    i.wrapping_mul(b) >> 16
}

/// The angle of a vector, 0-0xFFFF for a whole turn.
fn arctan2(x: i32, y: i32) -> u32 {
    let tan = |n: i32, d: i32| arctan(((i64::from(n) << 14) / i64::from(d)) as i32);
    let angle = match (x, y) {
        (x, 0) => {
            if x >= 0 {
                0
            } else {
                0x8000
            }
        }
        (0, y) => {
            if y >= 0 {
                0x4000
            } else {
                0xc000
            }
        }
        (x, y) if y >= 0 && x >= 0 && x >= y => tan(y, x),
        (x, y) if y >= 0 && x < 0 && -x >= y => tan(y, x) + 0x8000,
        (x, y) if y >= 0 => 0x4000 - tan(x, y),
        (x, y) if x <= 0 && -x > -y => tan(y, x) + 0x8000,
        (x, y) if x > 0 && x >= -y => tan(y, x) + 0x10000,
        (x, y) => 0xc000 - tan(x, y),
    };
    angle as u32 & 0xffff
}

impl Gba {
    /// A BIOS call: the function's number is in the SWI opcode.
    pub(crate) fn swi(&mut self, function: u32) {
        let [r0, r1, r2, r3] = [self.cpu.r[0], self.cpu.r[1], self.cpu.r[2], self.cpu.r[3]];
        // Into the BIOS and back out.
        self.idle(20);
        match function {
            0x00 => self.soft_reset(),
            0x01 => self.register_ram_reset(r0),
            0x02 | 0x03 => self.cpu.halted = true,
            0x04 => self.intr_wait(r0 != 0, r1 as u16),
            0x05 => self.intr_wait(true, 1),
            0x06 => self.divide(r0 as i32, r1 as i32),
            0x07 => self.divide(r1 as i32, r0 as i32),
            0x08 => {
                self.cpu.r[0] = r0.isqrt();
                self.idle(40);
            }
            0x09 => {
                self.cpu.r[0] = arctan(r0 as i32) as u32;
                self.idle(40);
            }
            0x0a => {
                self.cpu.r[0] = arctan2(r0 as i16 as i32, r1 as i16 as i32);
                self.idle(60);
            }
            0x0b => self.cpu_set(r0, r1, r2),
            0x0c => self.cpu_fast_set(r0, r1, r2),
            0x0d => self.cpu.r[0] = 0xbaae_187f,
            0x0e => self.bg_affine_set(r0, r1, r2),
            0x0f => self.obj_affine_set(r0, r1, r2, r3),
            0x10 => self.bit_unpack(r0, r1, r2),
            0x11 => self.lz77(r0, r1, false),
            0x12 => self.lz77(r0, r1, true),
            0x13 => self.huffman(r0, r1),
            0x14 => self.run_length(r0, r1, false),
            0x15 => self.run_length(r0, r1, true),
            0x16 => self.unfilter(r0, r1, 1, false),
            0x17 => self.unfilter(r0, r1, 1, true),
            0x18 => self.unfilter(r0, r1, 2, true),
            0x19 => self.apu.set_bias_level(if r0 != 0 { 0x200 } else { 0 }),
            0x1f => {
                // MidiKey2Freq: a sample's rate shifted to a MIDI key, with
                // a fraction of a key in r2.
                let rate = self.load32(r0.wrapping_add(4)) as f32;
                let keys = 180.0 - r1 as f32 - r2 as f32 / 256.0;
                self.cpu.r[0] = (rate / (keys / 12.0).exp2()) as u32;
            }
            0x25 => self.cpu.r[0] = 1,
            0x26 => self.soft_reset(),
            0x27 => self.cpu.halted = true,
            // The BIOS's own sound driver, which games (with theirs in the
            // ROM) do not use.
            _ => {}
        }
        self.bios.latch = AFTER_SWI;
    }

    /// Clears the top of the chip's work RAM and starts the game again,
    /// from the cartridge or (if the byte at 0300_7FFA says so) from the
    /// board's work RAM.
    fn soft_reset(&mut self) {
        let from_ram = self.iwram[0x7ffa] != 0;
        self.iwram[0x7e00..].fill(0);
        self.cpu = Cpu::after_boot();
        self.bios.waiting = false;
        self.jump(if from_ram { 0x0200_0000 } else { 0x0800_0000 });
    }

    /// Clears the memories and registers r0 picks.
    fn register_ram_reset(&mut self, flags: u32) {
        self.ppu.write_register(0, 0x80);
        self.ppu.write_register(1, 0);
        if flags & 1 != 0 {
            self.ewram.fill(0);
        }
        if flags & 2 != 0 {
            self.iwram[..0x7e00].fill(0);
        }
        if flags & 4 != 0 {
            self.ppu.palette.fill(0);
            self.ppu.palette_written(0, 0x400);
        }
        if flags & 8 != 0 {
            self.ppu.vram.fill(0);
        }
        if flags & 0x10 != 0 {
            self.ppu.oam.fill(0);
            self.ppu.oam_changed = true;
        }
        if flags & 0x20 != 0 {
            self.system.serial = [0; 8];
            self.system.rcnt = 0x8000;
            self.system.joycnt = 0;
        }
        if flags & 0x40 != 0 {
            for reg in 0x060..0x0a0 {
                self.io_write8(reg, 0);
            }
            self.apu.set_bias_level(0x200);
        }
        if flags & 0x80 != 0 {
            for reg in (0x002..0x060).chain(0x0b0..0x120).chain(0x200..0x20c) {
                self.io_write8(reg, 0);
            }
            // The affine backgrounds' matrices go back to the identity.
            for reg in [0x21, 0x27, 0x31, 0x37] {
                self.io_write8(reg, 1);
            }
            self.system.requested = 0;
            self.update_irq();
        }
    }

    /// IntrWait: waits, interrupts on, until one of the interrupts asked
    /// for has been handled: the game's handler marks those it handles at
    /// 0300_7FF8. Until then the call runs again after each interrupt.
    fn intr_wait(&mut self, discard: bool, flags: u16) {
        self.system.ime = true;
        self.update_irq();
        let marked = self.load16(0x0300_7ff8);
        if discard && !self.bios.waiting {
            self.store16(0x0300_7ff8, marked & !flags);
        } else if marked & flags != 0 {
            self.store16(0x0300_7ff8, marked & !flags);
            self.bios.waiting = false;
            return;
        }
        self.bios.waiting = true;
        self.cpu.halted = true;
        // Back to the SWI, for the interrupt to return to.
        let size = if self.cpu.thumb() { 4 } else { 8 };
        let swi = self.cpu.r[15].wrapping_sub(size);
        self.jump(swi);
    }

    /// Div: r0 / r1 into r0, the remainder into r1 and the quotient's size
    /// into r3. Dividing by 0 gives 1 or -1 (by the sign) and the number.
    fn divide(&mut self, number: i32, by: i32) {
        let (quotient, remainder) = if by == 0 {
            (if number < 0 { -1 } else { 1 }, number)
        } else {
            (number.wrapping_div(by), number.wrapping_rem(by))
        };
        self.cpu.r[0] = quotient as u32;
        self.cpu.r[1] = remainder as u32;
        self.cpu.r[3] = quotient.unsigned_abs();
        self.idle(50);
    }

    /// CpuSet: copies (or with bit 24, fills with the first unit) r2's
    /// count of halfwords, or words with bit 26. Like all the BIOS's
    /// copies it will not read the BIOS.
    fn cpu_set(&mut self, src: u32, dst: u32, control: u32) {
        let (count, fill, words) = (control & 0x1f_ffff, control & 1 << 24 != 0, control & 1 << 26 != 0);
        if src >> 25 == 0 {
            return;
        }
        let size = if words { 4 } else { 2 };
        let (mut src, mut dst) = (src & !(size - 1), dst & !(size - 1));
        for i in 0..count {
            let seq = i > 0;
            if words {
                let value = self.read32(src, seq);
                self.write32(dst, value, seq);
            } else {
                let value = self.read16(src, seq);
                self.write16(dst, value, seq);
            }
            if !fill {
                src = src.wrapping_add(size);
            }
            dst = dst.wrapping_add(size);
        }
    }

    /// CpuFastSet: words, in blocks of eight.
    fn cpu_fast_set(&mut self, src: u32, dst: u32, control: u32) {
        let count = (control & 0x1f_ffff).div_ceil(8) * 8;
        self.cpu_set(src, dst, control & 1 << 24 | 1 << 26 | count);
    }

    /// BgAffineSet: from a centre, a scale and an angle, the matrix and
    /// starting point of an affine background, for each of r2 sets.
    fn bg_affine_set(&mut self, mut src: u32, mut dst: u32, count: u32) {
        for _ in 0..count {
            let ox = self.load32(src) as i32;
            let oy = self.load32(src + 4) as i32;
            let cx = i32::from(self.load16(src + 8) as i16);
            let cy = i32::from(self.load16(src + 10) as i16);
            let sx = i32::from(self.load16(src + 12) as i16);
            let sy = i32::from(self.load16(src + 14) as i16);
            let angle = self.load16(src + 16);
            let (sin, cos) = (self.bios.sin(angle), self.bios.cos(angle));
            let pa = (sx * cos) >> 14;
            let pb = -(sx * sin) >> 14;
            let pc = (sy * sin) >> 14;
            let pd = (sy * cos) >> 14;
            self.store16(dst, pa as u16);
            self.store16(dst + 2, pb as u16);
            self.store16(dst + 4, pc as u16);
            self.store16(dst + 6, pd as u16);
            let start =
                |o: i32, a: i32, b: i32| o.wrapping_sub(a.wrapping_mul(cx).wrapping_add(b.wrapping_mul(cy))) as u32;
            self.store32(dst + 8, start(ox, pa, pb));
            self.store32(dst + 12, start(oy, pc, pd));
            src += 20;
            dst += 16;
            self.idle(60);
        }
    }

    /// ObjAffineSet: the same matrix for sprites, its four numbers r3
    /// bytes apart (2 for a table, 8 to write OAM).
    fn obj_affine_set(&mut self, mut src: u32, mut dst: u32, count: u32, stride: u32) {
        for _ in 0..count {
            let sx = i32::from(self.load16(src) as i16);
            let sy = i32::from(self.load16(src + 2) as i16);
            let angle = self.load16(src + 4);
            let (sin, cos) = (self.bios.sin(angle), self.bios.cos(angle));
            for value in [(sx * cos) >> 14, -(sx * sin) >> 14, (sy * sin) >> 14, (sy * cos) >> 14] {
                self.store16(dst, value as u16);
                dst = dst.wrapping_add(stride);
            }
            src += 8;
            self.idle(40);
        }
    }

    /// BitUnPack: widens units of 1, 2, 4 or 8 bits to 1-32 bits, adding
    /// an offset (to zeros too, if asked).
    fn bit_unpack(&mut self, mut src: u32, mut dst: u32, info: u32) {
        let length = u32::from(self.load16(info));
        let from = u32::from(self.load8(info + 2));
        let to = u32::from(self.load8(info + 3));
        let offset = self.load32(info + 4);
        let (add, zeros) = (offset & 0x7fff_ffff, offset & 0x8000_0000 != 0);
        if !matches!(from, 1 | 2 | 4 | 8) || !matches!(to, 1 | 2 | 4 | 8 | 16 | 32) {
            return;
        }
        let (mut out, mut bits) = (0u32, 0);
        for _ in 0..length {
            let byte = u32::from(self.read8(src, true));
            src += 1;
            for i in (0..8).step_by(from as usize) {
                let mut unit = byte >> i & ((1 << from) - 1);
                if unit != 0 || zeros {
                    unit = unit.wrapping_add(add);
                }
                out |= (unit & (u32::MAX >> (32 - to))) << bits;
                bits += to;
                if bits == 32 {
                    self.write32(dst, out, true);
                    dst += 4;
                    out = 0;
                    bits = 0;
                }
            }
        }
    }

    /// The header of compressed data: its size once uncompressed, or None
    /// for data in the BIOS.
    fn compressed(&mut self, src: u32) -> Option<usize> {
        if src >> 25 == 0 {
            return None;
        }
        Some((self.load32(src) >> 8) as usize)
    }

    /// Writes what was uncompressed: bytes to work RAM, or halfwords for
    /// VRAM, which takes no single bytes.
    fn write_out(&mut self, dst: u32, out: &[u8], halfwords: bool) {
        if halfwords {
            for (i, pair) in out.chunks(2).enumerate() {
                let value = u16::from(pair[0]) | u16::from(*pair.get(1).unwrap_or(&0)) << 8;
                self.write16(dst.wrapping_add(2 * i as u32), value, i > 0);
            }
        } else {
            for (i, &byte) in out.iter().enumerate() {
                self.write8(dst.wrapping_add(i as u32), byte, i > 0);
            }
        }
    }

    /// LZ77UnComp: each flag bit says whether a byte follows, or a copy of
    /// 3-18 bytes from up to 4 KB back.
    fn lz77(&mut self, src: u32, dst: u32, halfwords: bool) {
        let Some(size) = self.compressed(src) else { return };
        let mut out = Vec::with_capacity(size);
        let mut src = src + 4;
        let mut byte = |gba: &mut Gba| {
            let b = gba.read8(src, true);
            src += 1;
            b
        };
        while out.len() < size {
            let flags = byte(self);
            for bit in 0..8 {
                if out.len() >= size {
                    break;
                }
                if flags & 0x80 >> bit == 0 {
                    out.push(byte(self));
                    continue;
                }
                let (high, low) = (usize::from(byte(self)), usize::from(byte(self)));
                let back = ((high & 0xf) << 8 | low) + 1;
                for _ in 0..(high >> 4) + 3 {
                    let value = out.len().checked_sub(back).map_or(0, |i| out[i]);
                    out.push(value);
                }
            }
        }
        out.truncate(size);
        self.write_out(dst, &out, halfwords);
    }

    /// HuffUnComp: 4- or 8-bit symbols, coded by a tree after the header,
    /// the code read from 32-bit words, highest bit first.
    fn huffman(&mut self, src: u32, dst: u32) {
        let Some(size) = self.compressed(src) else { return };
        let symbol_bits = self.load8(src) as u32 & 15;
        if !matches!(symbol_bits, 4 | 8) {
            return;
        }
        let tree = src + 5;
        let mut stream = src + 4 + (u32::from(self.load8(src + 4)) + 1) * 2;
        let (mut node, mut word, mut bits) = (tree, 0u32, 0);
        let (mut out, mut filled) = (0u32, 0);
        let mut written = 0;
        while written < size {
            if bits == 0 {
                word = self.read32(stream, true);
                stream += 4;
                bits = 32;
            }
            bits -= 1;
            let right = word >> bits & 1;
            let entry = self.load8(node);
            let child = (node & !1) + u32::from(entry & 0x3f) * 2 + 2 + right;
            if entry & (0x80 >> right) == 0 {
                node = child;
                continue;
            }
            out |= (u32::from(self.load8(child)) & ((1 << symbol_bits) - 1)) << filled;
            filled += symbol_bits;
            node = tree;
            if filled == 32 {
                self.write32(dst.wrapping_add(written as u32), out, written > 0);
                written += 4;
                out = 0;
                filled = 0;
            }
        }
    }

    /// RLUnComp: runs of one byte repeated 3-130 times, and stretches of
    /// 1-128 bytes as they are.
    fn run_length(&mut self, src: u32, dst: u32, halfwords: bool) {
        let Some(size) = self.compressed(src) else { return };
        let mut out = Vec::with_capacity(size);
        let mut src = src + 4;
        while out.len() < size {
            let flag = self.read8(src, true);
            src += 1;
            if flag & 0x80 != 0 {
                let value = self.read8(src, true);
                src += 1;
                out.extend(std::iter::repeat_n(value, usize::from(flag & 0x7f) + 3));
            } else {
                for _ in 0..=flag {
                    out.push(self.read8(src, true));
                    src += 1;
                }
            }
        }
        out.truncate(size);
        self.write_out(dst, &out, halfwords);
    }

    /// Diff8bitUnFilter and Diff16bitUnFilter: each unit is the last plus
    /// a difference.
    fn unfilter(&mut self, src: u32, dst: u32, unit: u32, halfwords: bool) {
        let Some(size) = self.compressed(src) else { return };
        let mut out = Vec::with_capacity(size);
        let mut value = 0u16;
        for i in (0..size as u32).step_by(unit as usize) {
            let delta =
                if unit == 1 { u16::from(self.read8(src + 4 + i, true)) } else { self.read16(src + 4 + i, true) };
            value = value.wrapping_add(delta);
            if unit == 1 {
                out.push(value as u8);
            } else {
                out.extend_from_slice(&value.to_le_bytes());
            }
        }
        self.write_out(dst, &out, halfwords);
    }
}
