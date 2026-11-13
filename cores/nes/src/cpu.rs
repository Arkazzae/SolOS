//! The 2A03's CPU: a 6502 without decimal mode. Every bus access is one
//! cycle, the dummy reads and writes the real chip makes included, so each
//! instruction takes exactly its cycles and the PPU stays in step. The
//! undocumented opcodes games and test ROMs use are here too; the ones that
//! jam the real chip stop this one.

use crate::Nes;
use savestate::state;

// Status flags. B and U are not flags in the chip, only bits of what it
// pushes; U is kept set here so the register reads as the pushed byte does.
const C: u8 = 0x01;
const Z: u8 = 0x02;
pub const I: u8 = 0x04;
const B: u8 = 0x10;
const U: u8 = 0x20;
const V: u8 = 0x40;
const N: u8 = 0x80;

#[derive(Clone, Copy, Debug, Default)]
pub struct Cpu {
    pub a: u8,
    pub x: u8,
    pub y: u8,
    pub s: u8,
    pub p: u8,
    pub pc: u16,
    /// A jam opcode stopped the CPU; only a reset restarts it.
    pub jammed: bool,
    /// An NMI edge was seen and not yet taken; and the same a cycle ago.
    pub nmi: bool,
    pub prev_nmi: bool,
    /// The IRQ line is low and interrupts are enabled; and a cycle ago.
    pub irq: bool,
    pub prev_irq: bool,
}

state!(Cpu { a, x, y, s, p, pc, jammed, nmi, prev_nmi, irq, prev_irq });

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// No operand, or the accumulator; also branches, jumps and the like,
    /// which handle their operands themselves.
    Implied,
    Immediate,
    ZeroPage,
    ZeroPageX,
    ZeroPageY,
    Absolute,
    AbsoluteX,
    AbsoluteY,
    IndirectX,
    IndirectY,
}

/// The addressing mode of an opcode, from its bits aaabbbcc: bbb picks the
/// mode, with a few exceptions by group (cc) and operation (aaa).
const fn mode_of(op: u8) -> Mode {
    let (a, b, c) = (op >> 5, op >> 2 & 7, op & 3);
    // STX and LDX, and their undocumented cousins SAX and LAX, index with Y.
    let by_y = c >= 2 && (a == 4 || a == 5);
    match b {
        0 if c & 1 == 1 => Mode::IndirectX,
        0 if a < 4 => Mode::Implied,
        0 => Mode::Immediate,
        1 => Mode::ZeroPage,
        2 if c & 1 == 1 => Mode::Immediate,
        3 => Mode::Absolute,
        4 if c & 1 == 1 => Mode::IndirectY,
        5 if by_y => Mode::ZeroPageY,
        5 => Mode::ZeroPageX,
        6 if c & 1 == 1 => Mode::AbsoluteY,
        7 if by_y => Mode::AbsoluteY,
        7 => Mode::AbsoluteX,
        _ => Mode::Implied,
    }
}

const MODES: [Mode; 256] = {
    let mut modes = [Mode::Implied; 256];
    let mut op = 0;
    while op < 256 {
        modes[op] = mode_of(op as u8);
        op += 1;
    }
    modes
};

impl Nes {
    /// The power-on and reset sequence: an interrupt whose pushes are reads,
    /// 7 cycles, ending at the reset vector.
    pub(crate) fn reset_cpu(&mut self) {
        self.cpu.jammed = false;
        self.idle();
        self.idle();
        for _ in 0..3 {
            self.read(0x100 | u16::from(self.cpu.s));
            self.cpu.s = self.cpu.s.wrapping_sub(1);
        }
        self.cpu.p |= I | U;
        let lo = self.read(0xfffc);
        let hi = self.read(0xfffd);
        self.cpu.pc = u16::from_le_bytes([lo, hi]);
    }

    /// Runs one instruction, then takes an interrupt the CPU saw in time,
    /// before the instruction's last cycle. The first instruction of an
    /// interrupt handler always runs before the next interrupt.
    pub(crate) fn step(&mut self) {
        if self.cpu.jammed {
            self.tick();
            return;
        }
        self.execute();
        if self.cpu.prev_nmi || self.cpu.prev_irq {
            self.interrupt(0xfffe, false);
        }
    }

    fn execute(&mut self) {
        let op = self.fetch();
        let mode = MODES[usize::from(op)];
        match op {
            // Loads, logic, arithmetic and compares: they read an operand.
            0xa1 | 0xa5 | 0xa9 | 0xad | 0xb1 | 0xb5 | 0xb9 | 0xbd => {
                let v = self.operand(mode);
                self.cpu.a = self.nz(v);
            }
            0xa2 | 0xa6 | 0xae | 0xb6 | 0xbe => {
                let v = self.operand(mode);
                self.cpu.x = self.nz(v);
            }
            0xa0 | 0xa4 | 0xac | 0xb4 | 0xbc => {
                let v = self.operand(mode);
                self.cpu.y = self.nz(v);
            }
            0x01 | 0x05 | 0x09 | 0x0d | 0x11 | 0x15 | 0x19 | 0x1d => {
                let v = self.operand(mode);
                self.ora(v);
            }
            0x21 | 0x25 | 0x29 | 0x2d | 0x31 | 0x35 | 0x39 | 0x3d => {
                let v = self.operand(mode);
                self.and(v);
            }
            0x41 | 0x45 | 0x49 | 0x4d | 0x51 | 0x55 | 0x59 | 0x5d => {
                let v = self.operand(mode);
                self.eor(v);
            }
            0x61 | 0x65 | 0x69 | 0x6d | 0x71 | 0x75 | 0x79 | 0x7d => {
                let v = self.operand(mode);
                self.adc(v);
            }
            0xe1 | 0xe5 | 0xe9 | 0xeb | 0xed | 0xf1 | 0xf5 | 0xf9 | 0xfd => {
                let v = self.operand(mode);
                self.adc(!v);
            }
            0xc1 | 0xc5 | 0xc9 | 0xcd | 0xd1 | 0xd5 | 0xd9 | 0xdd => {
                let v = self.operand(mode);
                self.compare(self.cpu.a, v);
            }
            0xe0 | 0xe4 | 0xec => {
                let v = self.operand(mode);
                self.compare(self.cpu.x, v);
            }
            0xc0 | 0xc4 | 0xcc => {
                let v = self.operand(mode);
                self.compare(self.cpu.y, v);
            }
            0x24 | 0x2c => {
                let v = self.operand(mode);
                self.set(Z, self.cpu.a & v == 0);
                self.set(V, v & 0x40 != 0);
                self.set(N, v & 0x80 != 0);
            }
            // Undocumented reads.
            0xa3 | 0xa7 | 0xab | 0xaf | 0xb3 | 0xb7 | 0xbf => {
                // LAX; LXA (immediate) is unstable on the chip, taken as LAX.
                let v = self.operand(mode);
                self.cpu.a = self.nz(v);
                self.cpu.x = v;
            }
            0x0b | 0x2b => {
                // ANC: AND, with bit 7 also into carry.
                let v = self.operand(mode);
                self.and(v);
                self.set(C, self.cpu.a & 0x80 != 0);
            }
            0x4b => {
                // ALR: AND, then LSR A.
                let v = self.operand(mode);
                self.cpu.a &= v;
                self.cpu.a = self.lsr(self.cpu.a);
            }
            0x6b => {
                // ARR: AND, then ROR A, with carry and overflow from bits 6 and 5.
                let v = self.operand(mode);
                let a = (self.cpu.a & v) >> 1 | (self.cpu.p & C) << 7;
                self.cpu.a = self.nz(a);
                self.set(C, a & 0x40 != 0);
                self.set(V, (a ^ a << 1) & 0x40 != 0);
            }
            0xcb => {
                // AXS: X = (A AND X) - operand, carry as in a compare.
                let v = self.operand(mode);
                let ax = self.cpu.a & self.cpu.x;
                self.set(C, ax >= v);
                self.cpu.x = self.nz(ax.wrapping_sub(v));
            }
            0x8b => {
                // XAA, unstable on the chip; the common magic constant.
                let v = self.operand(mode);
                self.cpu.a = self.nz((self.cpu.a | 0xee) & self.cpu.x & v);
            }
            0xbb => {
                // LAS: A, X and S all become operand AND S.
                let v = self.operand(mode) & self.cpu.s;
                self.cpu.a = self.nz(v);
                self.cpu.x = v;
                self.cpu.s = v;
            }
            0x04 | 0x0c | 0x14 | 0x1c | 0x34 | 0x3c | 0x44 | 0x54 | 0x5c | 0x64 | 0x74 | 0x7c | 0x80 | 0x82 | 0x89
            | 0xc2 | 0xd4 | 0xdc | 0xe2 | 0xf4 | 0xfc => {
                // NOPs that read an operand.
                self.operand(mode);
            }

            // Stores.
            0x81 | 0x85 | 0x8d | 0x91 | 0x95 | 0x99 | 0x9d => {
                let addr = self.address(mode, true);
                self.write(addr, self.cpu.a);
            }
            0x86 | 0x8e | 0x96 => {
                let addr = self.address(mode, true);
                self.write(addr, self.cpu.x);
            }
            0x84 | 0x8c | 0x94 => {
                let addr = self.address(mode, true);
                self.write(addr, self.cpu.y);
            }
            0x83 | 0x87 | 0x8f | 0x97 => {
                // SAX
                let addr = self.address(mode, true);
                self.write(addr, self.cpu.a & self.cpu.x);
            }
            0x93 | 0x9f => self.store_high(mode, self.cpu.a & self.cpu.x),
            0x9c => self.store_high(mode, self.cpu.y),
            0x9e => self.store_high(mode, self.cpu.x),
            0x9b => {
                // TAS: S = A AND X, then stored like SHA.
                self.cpu.s = self.cpu.a & self.cpu.x;
                self.store_high(mode, self.cpu.s);
            }

            // Read-modify-write, and their undocumented combinations.
            0x06 | 0x0a | 0x0e | 0x16 | 0x1e => self.modify(mode, Nes::asl),
            0x46 | 0x4a | 0x4e | 0x56 | 0x5e => self.modify(mode, Nes::lsr),
            0x26 | 0x2a | 0x2e | 0x36 | 0x3e => self.modify(mode, Nes::rol),
            0x66 | 0x6a | 0x6e | 0x76 | 0x7e => self.modify(mode, Nes::ror),
            0xe6 | 0xee | 0xf6 | 0xfe => self.modify(mode, |nes, v| nes.nz(v.wrapping_add(1))),
            0xc6 | 0xce | 0xd6 | 0xde => self.modify(mode, |nes, v| nes.nz(v.wrapping_sub(1))),
            0x03 | 0x07 | 0x0f | 0x13 | 0x17 | 0x1b | 0x1f => self.modify(mode, |nes, v| {
                let r = nes.asl(v);
                nes.ora(r);
                r
            }),
            0x23 | 0x27 | 0x2f | 0x33 | 0x37 | 0x3b | 0x3f => self.modify(mode, |nes, v| {
                let r = nes.rol(v);
                nes.and(r);
                r
            }),
            0x43 | 0x47 | 0x4f | 0x53 | 0x57 | 0x5b | 0x5f => self.modify(mode, |nes, v| {
                let r = nes.lsr(v);
                nes.eor(r);
                r
            }),
            0x63 | 0x67 | 0x6f | 0x73 | 0x77 | 0x7b | 0x7f => self.modify(mode, |nes, v| {
                let r = nes.ror(v);
                nes.adc(r);
                r
            }),
            0xc3 | 0xc7 | 0xcf | 0xd3 | 0xd7 | 0xdb | 0xdf => self.modify(mode, |nes, v| {
                let r = v.wrapping_sub(1);
                nes.compare(nes.cpu.a, r);
                r
            }),
            0xe3 | 0xe7 | 0xef | 0xf3 | 0xf7 | 0xfb | 0xff => self.modify(mode, |nes, v| {
                let r = v.wrapping_add(1);
                nes.adc(!r);
                r
            }),

            // Branches.
            0x10 => self.branch(self.cpu.p & N == 0),
            0x30 => self.branch(self.cpu.p & N != 0),
            0x50 => self.branch(self.cpu.p & V == 0),
            0x70 => self.branch(self.cpu.p & V != 0),
            0x90 => self.branch(self.cpu.p & C == 0),
            0xb0 => self.branch(self.cpu.p & C != 0),
            0xd0 => self.branch(self.cpu.p & Z == 0),
            0xf0 => self.branch(self.cpu.p & Z != 0),

            // Jumps, subroutines and interrupts.
            0x4c => self.cpu.pc = self.fetch16(),
            0x6c => {
                // The pointer's high byte comes from the same page, a bug of the chip.
                let ptr = self.fetch16();
                let lo = self.read(ptr);
                let hi = self.read(ptr & 0xff00 | ptr.wrapping_add(1) & 0x00ff);
                self.cpu.pc = u16::from_le_bytes([lo, hi]);
            }
            0x20 => {
                let lo = self.fetch();
                self.read(0x100 | u16::from(self.cpu.s));
                let [pc_lo, pc_hi] = self.cpu.pc.to_le_bytes();
                self.push(pc_hi);
                self.push(pc_lo);
                let hi = self.read(self.cpu.pc);
                self.cpu.pc = u16::from_le_bytes([lo, hi]);
            }
            0x60 => {
                self.idle();
                self.read(0x100 | u16::from(self.cpu.s));
                let lo = self.pull();
                let hi = self.pull();
                self.cpu.pc = u16::from_le_bytes([lo, hi]);
                self.fetch();
            }
            0x40 => {
                self.idle();
                self.read(0x100 | u16::from(self.cpu.s));
                let p = self.pull();
                self.cpu.p = p & !B | U;
                let lo = self.pull();
                let hi = self.pull();
                self.cpu.pc = u16::from_le_bytes([lo, hi]);
            }
            0x00 => self.interrupt(0xfffe, true),

            // The stack.
            0x48 => {
                self.idle();
                self.push(self.cpu.a);
            }
            0x08 => {
                self.idle();
                self.push(self.cpu.p | B | U);
            }
            0x68 => {
                self.idle();
                self.read(0x100 | u16::from(self.cpu.s));
                let v = self.pull();
                self.cpu.a = self.nz(v);
            }
            0x28 => {
                self.idle();
                self.read(0x100 | u16::from(self.cpu.s));
                let p = self.pull();
                self.cpu.p = p & !B | U;
            }

            // Two-cycle instructions with no operand.
            _ if mode == Mode::Implied && op & 0x1f != 0x12 && op & 0x9f != 0x02 => {
                self.idle();
                self.implied(op);
            }

            // The rest jam the chip: 02, 12, 22, ... F2, but for 82, A2, C2, E2.
            _ => self.cpu.jammed = true,
        }
    }

    /// The instructions that take two cycles and no operand.
    fn implied(&mut self, op: u8) {
        match op {
            0x18 => self.cpu.p &= !C,
            0x38 => self.cpu.p |= C,
            0x58 => self.cpu.p &= !I,
            0x78 => self.cpu.p |= I,
            0xb8 => self.cpu.p &= !V,
            // Decimal mode is set and cleared, but does nothing on the 2A03.
            0xd8 => self.cpu.p &= !0x08,
            0xf8 => self.cpu.p |= 0x08,
            0xaa => self.cpu.x = self.nz(self.cpu.a),
            0xa8 => self.cpu.y = self.nz(self.cpu.a),
            0x8a => self.cpu.a = self.nz(self.cpu.x),
            0x98 => self.cpu.a = self.nz(self.cpu.y),
            0xba => self.cpu.x = self.nz(self.cpu.s),
            0x9a => self.cpu.s = self.cpu.x,
            0xe8 => self.cpu.x = self.nz(self.cpu.x.wrapping_add(1)),
            0xc8 => self.cpu.y = self.nz(self.cpu.y.wrapping_add(1)),
            0xca => self.cpu.x = self.nz(self.cpu.x.wrapping_sub(1)),
            0x88 => self.cpu.y = self.nz(self.cpu.y.wrapping_sub(1)),
            // NOP: EA and the undocumented 1A, 3A, 5A, 7A, DA, FA.
            _ => {}
        }
    }

    /// The next byte of the instruction stream.
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

    /// The read of the next byte that instructions without one make anyway.
    fn idle(&mut self) {
        self.read(self.cpu.pc);
    }

    fn push(&mut self, v: u8) {
        self.write(0x100 | u16::from(self.cpu.s), v);
        self.cpu.s = self.cpu.s.wrapping_sub(1);
    }

    fn pull(&mut self) -> u8 {
        self.cpu.s = self.cpu.s.wrapping_add(1);
        self.read(0x100 | u16::from(self.cpu.s))
    }

    /// The address an instruction works on. Indexing that crosses a page
    /// costs a cycle, a dummy read from the wrong page; writes and
    /// read-modify-writes always take it.
    fn address(&mut self, mode: Mode, write: bool) -> u16 {
        match mode {
            Mode::Immediate => {
                let addr = self.cpu.pc;
                self.cpu.pc = self.cpu.pc.wrapping_add(1);
                addr
            }
            Mode::ZeroPage => u16::from(self.fetch()),
            Mode::ZeroPageX | Mode::ZeroPageY => {
                let base = self.fetch();
                self.read(u16::from(base));
                let index = if mode == Mode::ZeroPageX { self.cpu.x } else { self.cpu.y };
                u16::from(base.wrapping_add(index))
            }
            Mode::Absolute => self.fetch16(),
            Mode::AbsoluteX | Mode::AbsoluteY => {
                let base = self.fetch16();
                let index = if mode == Mode::AbsoluteX { self.cpu.x } else { self.cpu.y };
                self.indexed(base, index, write)
            }
            Mode::IndirectX => {
                let ptr = self.fetch();
                self.read(u16::from(ptr));
                let ptr = ptr.wrapping_add(self.cpu.x);
                let lo = self.read(u16::from(ptr));
                let hi = self.read(u16::from(ptr.wrapping_add(1)));
                u16::from_le_bytes([lo, hi])
            }
            Mode::IndirectY => {
                let base = self.pointer();
                self.indexed(base, self.cpu.y, write)
            }
            Mode::Implied => unreachable!("an implied instruction has no address"),
        }
    }

    /// (zp),Y's base: the 16-bit pointer at a zero-page address, which wraps.
    fn pointer(&mut self) -> u16 {
        let ptr = self.fetch();
        let lo = self.read(u16::from(ptr));
        let hi = self.read(u16::from(ptr.wrapping_add(1)));
        u16::from_le_bytes([lo, hi])
    }

    fn indexed(&mut self, base: u16, index: u8, write: bool) -> u16 {
        let addr = base.wrapping_add(u16::from(index));
        if write || addr & 0xff00 != base & 0xff00 {
            self.read(base & 0xff00 | addr & 0x00ff);
        }
        addr
    }

    /// An instruction's operand: its own byte, or the byte at its address.
    fn operand(&mut self, mode: Mode) -> u8 {
        let addr = self.address(mode, false);
        self.read(addr)
    }

    /// SHA, SHX, SHY and TAS store a register ANDed with the high byte of
    /// the address plus one; when indexing crosses a page, that value also
    /// becomes the high byte of where it goes.
    fn store_high(&mut self, mode: Mode, value: u8) {
        let (base, index) = match mode {
            Mode::AbsoluteX => (self.fetch16(), self.cpu.x),
            Mode::AbsoluteY => (self.fetch16(), self.cpu.y),
            _ => (self.pointer(), self.cpu.y),
        };
        let addr = base.wrapping_add(u16::from(index));
        self.read(base & 0xff00 | addr & 0x00ff);
        let v = value & ((base >> 8) as u8).wrapping_add(1);
        let addr = if (addr ^ base) & 0xff00 != 0 { u16::from(v) << 8 | addr & 0x00ff } else { addr };
        self.write(addr, v);
    }

    /// Read, write the old value back (the chip does), write the new one.
    fn modify(&mut self, mode: Mode, op: fn(&mut Nes, u8) -> u8) {
        if mode == Mode::Implied {
            // On the accumulator.
            self.idle();
            let a = self.cpu.a;
            self.cpu.a = op(self, a);
            return;
        }
        let addr = self.address(mode, true);
        let v = self.read(addr);
        self.write(addr, v);
        let r = op(self, v);
        self.write(addr, r);
    }

    fn branch(&mut self, taken: bool) {
        let offset = self.fetch() as i8;
        if taken {
            // A taken branch does not look at an IRQ that has just come in
            // during its extra cycle: one more instruction runs first.
            if self.cpu.irq && !self.cpu.prev_irq {
                self.cpu.irq = false;
            }
            self.idle();
            let pc = self.cpu.pc;
            let target = pc.wrapping_add(offset as u16);
            if target & 0xff00 != pc & 0xff00 {
                self.read(pc & 0xff00 | target & 0x00ff);
            }
            self.cpu.pc = target;
        }
    }

    /// NMI, IRQ or BRK: push the return address and the flags, and jump
    /// through the vector. BRK skips a padding byte; an interrupt instead
    /// reads the next opcode and drops it. An NMI that comes in while the
    /// return address is pushed takes over the IRQ's or BRK's sequence.
    fn interrupt(&mut self, vector: u16, brk: bool) {
        if brk {
            self.fetch();
        } else {
            self.idle();
            self.idle();
        }
        let [lo, hi] = self.cpu.pc.to_le_bytes();
        self.push(hi);
        self.push(lo);
        let vector = if self.cpu.nmi {
            self.cpu.nmi = false;
            0xfffa
        } else {
            vector
        };
        self.push(self.cpu.p | U | if brk { B } else { 0 });
        self.cpu.p |= I;
        let lo = self.read(vector);
        let hi = self.read(vector + 1);
        self.cpu.pc = u16::from_le_bytes([lo, hi]);
        // An NMI that came too late to take over still waits for the
        // handler's first instruction.
        self.cpu.prev_nmi = false;
    }

    fn set(&mut self, flag: u8, on: bool) {
        if on {
            self.cpu.p |= flag;
        } else {
            self.cpu.p &= !flag;
        }
    }

    /// Sets Z and N from a result, and passes it on.
    fn nz(&mut self, v: u8) -> u8 {
        self.set(Z, v == 0);
        self.set(N, v & 0x80 != 0);
        v
    }

    fn ora(&mut self, v: u8) {
        self.cpu.a = self.nz(self.cpu.a | v);
    }

    fn and(&mut self, v: u8) {
        self.cpu.a = self.nz(self.cpu.a & v);
    }

    fn eor(&mut self, v: u8) {
        self.cpu.a = self.nz(self.cpu.a ^ v);
    }

    /// Add with carry; subtraction is adding the complement.
    fn adc(&mut self, v: u8) {
        let a = self.cpu.a;
        let sum = u16::from(a) + u16::from(v) + u16::from(self.cpu.p & C);
        let r = sum as u8;
        self.set(C, sum > 0xff);
        self.set(V, (a ^ r) & (v ^ r) & 0x80 != 0);
        self.cpu.a = self.nz(r);
    }

    fn compare(&mut self, reg: u8, v: u8) {
        self.set(C, reg >= v);
        self.nz(reg.wrapping_sub(v));
    }

    fn asl(&mut self, v: u8) -> u8 {
        self.set(C, v & 0x80 != 0);
        self.nz(v << 1)
    }

    fn lsr(&mut self, v: u8) -> u8 {
        self.set(C, v & 1 != 0);
        self.nz(v >> 1)
    }

    fn rol(&mut self, v: u8) -> u8 {
        let r = v << 1 | self.cpu.p & C;
        self.set(C, v & 0x80 != 0);
        self.nz(r)
    }

    fn ror(&mut self, v: u8) -> u8 {
        let r = v >> 1 | (self.cpu.p & C) << 7;
        self.set(C, v & 1 != 0);
        self.nz(r)
    }
}

#[cfg(test)]
mod tests {
    use crate::Nes;

    const ROMS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/roms/nes-test-roms/");

    /// nestest in its automation mode, from $C000, against the log of a
    /// real NES: registers, flags, stack, PPU position and cycle count
    /// before each of its 8991 instructions, the undocumented ones too.
    #[test]
    fn nestest() {
        let (Ok(rom), Ok(log)) =
            (std::fs::read(format!("{ROMS}other/nestest.nes")), std::fs::read_to_string(format!("{ROMS}other/nestest.log")))
        else {
            eprintln!("skipped: run tests/roms/fetch.sh");
            return;
        };
        let mut nes = Nes::new(&rom).unwrap();
        nes.cpu.pc = 0xc000;
        for (n, line) in log.lines().enumerate() {
            nes.sync_ppu();
            let cpu = nes.cpu;
            let state = format!(
                "{:04X} A:{:02X} X:{:02X} Y:{:02X} P:{:02X} SP:{:02X} PPU:{:3},{:3} CYC:{}",
                cpu.pc, cpu.a, cpu.x, cpu.y, cpu.p, cpu.s, nes.ppu.scanline, nes.ppu.dot, nes.cycles
            );
            assert_eq!(state, format!("{} {}", &line[..4], &line[48..]), "line {}: {}", n + 1, &line[..47]);
            nes.step();
        }
        // nestest's own verdict: no error codes.
        assert_eq!((nes.peek(0x02), nes.peek(0x03)), (0, 0));
        eprintln!("nestest: {} instructions match", log.lines().count());
    }
}
