//! The Sharp SM83 CPU. Every memory access is a machine cycle, and the
//! instructions that take longer than their accesses tick their internal
//! cycles, so each takes exactly its time.

use crate::{Gb, Registers};
use savestate::state;

// Flags, in F's top four bits.
const Z: u8 = 0x80;
const N: u8 = 0x40;
const H: u8 = 0x20;
const C: u8 = 0x10;

#[derive(Clone, Copy, Debug)]
pub struct Cpu {
    a: u8,
    f: u8,
    b: u8,
    c: u8,
    d: u8,
    e: u8,
    h: u8,
    l: u8,
    sp: u16,
    pc: u16,
    /// Interrupts enabled; EI enables them after the next instruction.
    ime: bool,
    ime_next: bool,
    halted: bool,
    /// HALT with interrupts off and one pending: the next byte is read twice.
    halt_bug: bool,
    /// An opcode the chip does not have locks it up.
    locked: bool,
    /// LD B,B ran: the Mooneye tests' signal that they are done.
    pub breakpoint: bool,
}

state!(Cpu { a, f, b, c, d, e, h, l, sp, pc, ime, ime_next, halted, halt_bug, locked });

impl Cpu {
    /// The registers as the boot ROM leaves them, the DMG's or the Color's
    /// (whose A tells games they run on a Color).
    pub fn after_boot(cgb: bool) -> Cpu {
        let [a, f, b, c, d, e, h, l] =
            if cgb { [0x11, 0x80, 0x00, 0x00, 0xff, 0x56, 0x00, 0x0d] } else { [0x01, 0xb0, 0x00, 0x13, 0x00, 0xd8, 0x01, 0x4d] };
        Cpu {
            a,
            f,
            b,
            c,
            d,
            e,
            h,
            l,
            sp: 0xfffe,
            pc: 0x0100,
            ime: false,
            ime_next: false,
            halted: false,
            halt_bug: false,
            locked: false,
            breakpoint: false,
        }
    }

    pub fn registers(&self) -> Registers {
        Registers {
            a: self.a,
            f: self.f,
            b: self.b,
            c: self.c,
            d: self.d,
            e: self.e,
            h: self.h,
            l: self.l,
            sp: self.sp,
            pc: self.pc,
        }
    }

    fn hl(&self) -> u16 {
        u16::from_be_bytes([self.h, self.l])
    }

    fn set_hl(&mut self, v: u16) {
        [self.h, self.l] = v.to_be_bytes();
    }

    /// BC, DE, HL or SP, by the two bits of an opcode.
    fn pair(&self, i: u8) -> u16 {
        match i & 3 {
            0 => u16::from_be_bytes([self.b, self.c]),
            1 => u16::from_be_bytes([self.d, self.e]),
            2 => self.hl(),
            _ => self.sp,
        }
    }

    fn set_pair(&mut self, i: u8, v: u16) {
        let [hi, lo] = v.to_be_bytes();
        match i & 3 {
            0 => (self.b, self.c) = (hi, lo),
            1 => (self.d, self.e) = (hi, lo),
            2 => (self.h, self.l) = (hi, lo),
            _ => self.sp = v,
        }
    }

    fn flag(&self, f: u8) -> bool {
        self.f & f != 0
    }

    fn set_flags(&mut self, z: bool, n: bool, h: bool, c: bool) {
        self.f = if z { Z } else { 0 } | if n { N } else { 0 } | if h { H } else { 0 } | if c { C } else { 0 };
    }

    /// NZ, Z, NC or C, by the two bits of an opcode.
    fn condition(&self, i: u8) -> bool {
        match i & 3 {
            0 => !self.flag(Z),
            1 => self.flag(Z),
            2 => !self.flag(C),
            _ => self.flag(C),
        }
    }
}

impl Gb {
    /// Runs one instruction, or takes an interrupt, or waits a cycle in
    /// HALT.
    pub(crate) fn step(&mut self) {
        if self.cpu.locked {
            self.tick();
            return;
        }
        // The Color's VRAM DMA copies a block as each horizontal blank
        // starts, the CPU waiting.
        if std::mem::take(&mut self.ppu.hblank_started) && self.hdma_hblank && self.ppu.line() < 144 {
            self.hdma_block();
        }
        let enable = self.cpu.ime_next;
        // The opcode is fetched as the last instruction ends, and an
        // interrupt requested by then takes its place. HALT keeps fetching
        // until one is requested, and goes on with what it fetched last.
        let op = if self.cpu.halted {
            self.tick();
            if self.requested & self.enabled & 0x1f == 0 {
                return;
            }
            self.cpu.halted = false;
            self.bus_read(self.cpu.pc)
        } else {
            self.read(self.cpu.pc)
        };
        if self.cpu.ime && self.requested & self.enabled & 0x1f != 0 {
            self.interrupt();
            return;
        }
        if self.cpu.halt_bug {
            self.cpu.halt_bug = false;
        } else {
            self.cpu.pc = self.cpu.pc.wrapping_add(1);
        }
        self.execute(op);
        if enable && self.cpu.ime_next {
            self.cpu.ime = true;
            self.cpu.ime_next = false;
        }
    }

    /// After the opcode fetch it replaces: a cycle of waiting, the return
    /// address pushed, and a jump to the vector of the highest priority
    /// interrupt pending, which is looked at after the high byte is pushed
    /// (writing IE there can cancel it, and then the jump goes to 0000).
    fn interrupt(&mut self) {
        self.tick();
        let [hi, lo] = self.cpu.pc.to_be_bytes();
        self.cpu.sp = self.cpu.sp.wrapping_sub(1);
        self.write(self.cpu.sp, hi);
        let pending = self.requested & self.enabled & 0x1f;
        self.cpu.sp = self.cpu.sp.wrapping_sub(1);
        self.write(self.cpu.sp, lo);
        self.cpu.ime = false;
        self.cpu.pc = if pending == 0 {
            0
        } else {
            let bit = pending.trailing_zeros();
            self.requested &= !(1 << bit);
            0x40 + 8 * bit as u16
        };
        self.tick();
    }

    fn fetch(&mut self) -> u8 {
        let v = self.read(self.cpu.pc);
        self.cpu.pc = self.cpu.pc.wrapping_add(1);
        v
    }

    fn fetch16(&mut self) -> u16 {
        let lo = self.fetch();
        let hi = self.fetch();
        u16::from_le_bytes([lo, hi])
    }

    fn push(&mut self, v: u16) {
        let [hi, lo] = v.to_be_bytes();
        self.cpu.sp = self.cpu.sp.wrapping_sub(1);
        self.write(self.cpu.sp, hi);
        self.cpu.sp = self.cpu.sp.wrapping_sub(1);
        self.write(self.cpu.sp, lo);
    }

    fn pop(&mut self) -> u16 {
        let lo = self.read(self.cpu.sp);
        self.cpu.sp = self.cpu.sp.wrapping_add(1);
        let hi = self.read(self.cpu.sp);
        self.cpu.sp = self.cpu.sp.wrapping_add(1);
        u16::from_le_bytes([lo, hi])
    }

    /// B, C, D, E, H, L, (HL) or A, by three bits of an opcode.
    fn get(&mut self, i: u8) -> u8 {
        match i & 7 {
            0 => self.cpu.b,
            1 => self.cpu.c,
            2 => self.cpu.d,
            3 => self.cpu.e,
            4 => self.cpu.h,
            5 => self.cpu.l,
            6 => self.read(self.cpu.hl()),
            _ => self.cpu.a,
        }
    }

    fn set(&mut self, i: u8, v: u8) {
        match i & 7 {
            0 => self.cpu.b = v,
            1 => self.cpu.c = v,
            2 => self.cpu.d = v,
            3 => self.cpu.e = v,
            4 => self.cpu.h = v,
            5 => self.cpu.l = v,
            6 => self.write(self.cpu.hl(), v),
            _ => self.cpu.a = v,
        }
    }

    fn execute(&mut self, op: u8) {
        let (y, z) = (op >> 3 & 7, op & 7);
        match op {
            0x00 => {}
            0x10 => {
                // STOP: its second byte, then on as if nothing happened, but
                // for the Color's speed switch.
                self.fetch();
                self.stop();
            }
            0x76 => {
                if !self.cpu.ime && self.requested & self.enabled & 0x1f != 0 {
                    self.cpu.halt_bug = true;
                } else {
                    self.cpu.halted = true;
                }
            }
            // LD rr,d16
            0x01 | 0x11 | 0x21 | 0x31 => {
                let v = self.fetch16();
                self.cpu.set_pair(y >> 1, v);
            }
            // LD (BC),A; LD (DE),A; LD (HL+),A; LD (HL-),A and the loads back.
            0x02 | 0x12 | 0x22 | 0x32 | 0x0a | 0x1a | 0x2a | 0x3a => {
                let addr = match y >> 1 {
                    0 => self.cpu.pair(0),
                    1 => self.cpu.pair(1),
                    _ => self.cpu.hl(),
                };
                match y >> 1 {
                    2 => self.cpu.set_hl(addr.wrapping_add(1)),
                    3 => self.cpu.set_hl(addr.wrapping_sub(1)),
                    _ => {}
                }
                if y & 1 == 0 {
                    self.write(addr, self.cpu.a);
                } else {
                    self.cpu.a = self.read(addr);
                }
            }
            // INC rr, DEC rr
            0x03 | 0x13 | 0x23 | 0x33 | 0x0b | 0x1b | 0x2b | 0x3b => {
                let v = self.cpu.pair(y >> 1);
                self.cpu.set_pair(y >> 1, if y & 1 == 0 { v.wrapping_add(1) } else { v.wrapping_sub(1) });
                self.tick();
            }
            // INC r, DEC r
            0x04 | 0x0c | 0x14 | 0x1c | 0x24 | 0x2c | 0x34 | 0x3c | 0x05 | 0x0d | 0x15 | 0x1d | 0x25 | 0x2d | 0x35
            | 0x3d => {
                let v = self.get(y);
                let (r, half) = if z == 4 { (v.wrapping_add(1), v & 0x0f == 0x0f) } else { (v.wrapping_sub(1), v & 0x0f == 0) };
                self.set(y, r);
                let carry = self.cpu.flag(C);
                self.cpu.set_flags(r == 0, z == 5, half, carry);
            }
            // LD r,d8
            0x06 | 0x0e | 0x16 | 0x1e | 0x26 | 0x2e | 0x36 | 0x3e => {
                let v = self.fetch();
                self.set(y, v);
            }
            0x07 | 0x0f | 0x17 | 0x1f => {
                // RLCA, RRCA, RLA, RRA: as the CB rotations, but Z is always clear.
                let r = self.rotate(y, self.cpu.a);
                self.cpu.a = r;
                self.cpu.f &= !Z;
            }
            0x08 => {
                let addr = self.fetch16();
                let [hi, lo] = self.cpu.sp.to_be_bytes();
                self.write(addr, lo);
                self.write(addr.wrapping_add(1), hi);
            }
            // ADD HL,rr
            0x09 | 0x19 | 0x29 | 0x39 => {
                let (hl, v) = (self.cpu.hl(), self.cpu.pair(y >> 1));
                let (r, carry) = hl.overflowing_add(v);
                let zero = self.cpu.flag(Z);
                self.cpu.set_flags(zero, false, (hl & 0x0fff) + (v & 0x0fff) > 0x0fff, carry);
                self.cpu.set_hl(r);
                self.tick();
            }
            // JR e, JR cc,e
            0x18 | 0x20 | 0x28 | 0x30 | 0x38 => {
                let e = self.fetch() as i8;
                if op == 0x18 || self.cpu.condition(y - 4) {
                    self.cpu.pc = self.cpu.pc.wrapping_add(e as u16);
                    self.tick();
                }
            }
            0x27 => self.daa(),
            0x2f => {
                self.cpu.a = !self.cpu.a;
                self.cpu.f |= N | H;
            }
            0x37 => self.cpu.f = self.cpu.f & Z | C,
            0x3f => self.cpu.f = self.cpu.f & (Z | C) ^ C,
            // LD r,r'
            0x40..=0x7f => {
                if op == 0x40 {
                    self.cpu.breakpoint = true;
                }
                let v = self.get(z);
                self.set(y, v);
            }
            // ALU A,r
            0x80..=0xbf => {
                let v = self.get(z);
                self.alu(y, v);
            }
            // ALU A,d8
            0xc6 | 0xce | 0xd6 | 0xde | 0xe6 | 0xee | 0xf6 | 0xfe => {
                let v = self.fetch();
                self.alu(y, v);
            }
            // RET cc
            0xc0 | 0xc8 | 0xd0 | 0xd8 => {
                self.tick();
                if self.cpu.condition(y) {
                    self.cpu.pc = self.pop();
                    self.tick();
                }
            }
            // POP rr (AF's low four bits are always 0)
            0xc1 | 0xd1 | 0xe1 | 0xf1 => {
                let v = self.pop();
                if op == 0xf1 {
                    let [a, f] = v.to_be_bytes();
                    self.cpu.a = a;
                    self.cpu.f = f & 0xf0;
                } else {
                    self.cpu.set_pair(y >> 1, v);
                }
            }
            // JP cc,a16; JP a16
            0xc2 | 0xca | 0xd2 | 0xda | 0xc3 => {
                let addr = self.fetch16();
                if op == 0xc3 || self.cpu.condition(y) {
                    self.cpu.pc = addr;
                    self.tick();
                }
            }
            // CALL cc,a16; CALL a16
            0xc4 | 0xcc | 0xd4 | 0xdc | 0xcd => {
                let addr = self.fetch16();
                if op == 0xcd || self.cpu.condition(y) {
                    self.tick();
                    self.push(self.cpu.pc);
                    self.cpu.pc = addr;
                }
            }
            // PUSH rr
            0xc5 | 0xd5 | 0xe5 | 0xf5 => {
                let v = if op == 0xf5 { u16::from_be_bytes([self.cpu.a, self.cpu.f]) } else { self.cpu.pair(y >> 1) };
                self.tick();
                self.push(v);
            }
            // RST
            0xc7 | 0xcf | 0xd7 | 0xdf | 0xe7 | 0xef | 0xf7 | 0xff => {
                self.tick();
                self.push(self.cpu.pc);
                self.cpu.pc = u16::from(y) * 8;
            }
            0xc9 | 0xd9 => {
                self.cpu.pc = self.pop();
                self.tick();
                if op == 0xd9 {
                    self.cpu.ime = true;
                }
            }
            0xcb => {
                let op = self.fetch();
                self.prefixed(op);
            }
            0xe0 => {
                let a = 0xff00 | u16::from(self.fetch());
                self.write(a, self.cpu.a);
            }
            0xf0 => {
                let a = 0xff00 | u16::from(self.fetch());
                self.cpu.a = self.read(a);
            }
            0xe2 => self.write(0xff00 | u16::from(self.cpu.c), self.cpu.a),
            0xf2 => self.cpu.a = self.read(0xff00 | u16::from(self.cpu.c)),
            0xe8 => {
                let r = self.sp_plus_e();
                self.tick();
                self.tick();
                self.cpu.sp = r;
            }
            0xf8 => {
                let r = self.sp_plus_e();
                self.tick();
                self.cpu.set_hl(r);
            }
            0xe9 => self.cpu.pc = self.cpu.hl(),
            0xf9 => {
                self.cpu.sp = self.cpu.hl();
                self.tick();
            }
            0xea => {
                let a = self.fetch16();
                self.write(a, self.cpu.a);
            }
            0xfa => {
                let a = self.fetch16();
                self.cpu.a = self.read(a);
            }
            0xf3 => {
                self.cpu.ime = false;
                self.cpu.ime_next = false;
            }
            0xfb => self.cpu.ime_next = true,
            // D3, DB, DD, E3, E4, EB, EC, ED, F4, FC, FD
            _ => self.cpu.locked = true,
        }
    }

    /// SP plus a signed byte, with H and C from the low byte's addition.
    fn sp_plus_e(&mut self) -> u16 {
        let e = self.fetch() as i8 as u16;
        let sp = self.cpu.sp;
        self.cpu.set_flags(false, false, (sp & 0x0f) + (e & 0x0f) > 0x0f, (sp & 0xff) + (e & 0xff) > 0xff);
        sp.wrapping_add(e)
    }

    /// ADD, ADC, SUB, SBC, AND, XOR, OR, CP, by three bits of an opcode.
    fn alu(&mut self, op: u8, v: u8) {
        let a = self.cpu.a;
        let carry = u8::from(self.cpu.flag(C) && (op == 1 || op == 3));
        match op {
            0 | 1 => {
                let r = u16::from(a) + u16::from(v) + u16::from(carry);
                self.cpu.a = r as u8;
                self.cpu.set_flags(r as u8 == 0, false, (a & 0x0f) + (v & 0x0f) + carry > 0x0f, r > 0xff);
            }
            2 | 3 | 7 => {
                let r = i16::from(a) - i16::from(v) - i16::from(carry);
                let half = i16::from(a & 0x0f) - i16::from(v & 0x0f) - i16::from(carry) < 0;
                self.cpu.set_flags(r as u8 == 0, true, half, r < 0);
                if op != 7 {
                    self.cpu.a = r as u8;
                }
            }
            4 => {
                self.cpu.a = a & v;
                self.cpu.set_flags(self.cpu.a == 0, false, true, false);
            }
            5 => {
                self.cpu.a = a ^ v;
                self.cpu.set_flags(self.cpu.a == 0, false, false, false);
            }
            _ => {
                self.cpu.a = a | v;
                self.cpu.set_flags(self.cpu.a == 0, false, false, false);
            }
        }
    }

    fn daa(&mut self) {
        let mut a = self.cpu.a;
        let mut carry = self.cpu.flag(C);
        if self.cpu.flag(N) {
            if self.cpu.flag(H) {
                a = a.wrapping_sub(0x06);
            }
            if carry {
                a = a.wrapping_sub(0x60);
            }
        } else {
            if carry || a > 0x99 {
                a = a.wrapping_add(0x60);
                carry = true;
            }
            if self.cpu.flag(H) || a & 0x0f > 0x09 {
                a = a.wrapping_add(0x06);
            }
        }
        self.cpu.a = a;
        let n = self.cpu.flag(N);
        self.cpu.set_flags(a == 0, n, false, carry);
    }

    /// RLC, RRC, RL, RR, SLA, SRA, SWAP, SRL, by three bits of an opcode.
    fn rotate(&mut self, op: u8, v: u8) -> u8 {
        let carry_in = u8::from(self.cpu.flag(C));
        let (r, carry) = match op {
            0 => (v.rotate_left(1), v & 0x80 != 0),
            1 => (v.rotate_right(1), v & 1 != 0),
            2 => (v << 1 | carry_in, v & 0x80 != 0),
            3 => (v >> 1 | carry_in << 7, v & 1 != 0),
            4 => (v << 1, v & 0x80 != 0),
            5 => (v >> 1 | v & 0x80, v & 1 != 0),
            6 => (v.rotate_left(4), false),
            _ => (v >> 1, v & 1 != 0),
        };
        self.cpu.set_flags(r == 0, false, false, carry);
        r
    }

    /// The CB-prefixed instructions: rotations and shifts, BIT, RES, SET.
    fn prefixed(&mut self, op: u8) {
        let (x, y, z) = (op >> 6, op >> 3 & 7, op & 7);
        let v = self.get(z);
        match x {
            0 => {
                let r = self.rotate(y, v);
                self.set(z, r);
            }
            1 => {
                let carry = self.cpu.flag(C);
                self.cpu.set_flags(v & 1 << y == 0, false, true, carry);
            }
            2 => self.set(z, v & !(1 << y)),
            _ => self.set(z, v | 1 << y),
        }
    }
}
