//! An emitter of 64-bit ARM (A64) instructions, as many as the JIT needs: each method appends one
//! instruction's word. Registers are numbers 0-30; 31 is the zero register, or the stack pointer
//! where an instruction takes it. A method's first argument says whether it works on the 32-bit W
//! registers (`W`) or the 64-bit X ones (`X`); a W result clears the X register's top half.

// It covers more of A64 than the compiler uses yet.
#![allow(dead_code)]

pub type Reg = u32;

/// The registers' sizes, as an instruction's top bit has them.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Size {
    W = 0,
    X = 1,
}
pub use Size::{W, X};

/// How FCVTNS, FCVTPS, FCVTMS and FCVTZS round to an integer, as their rmode field has it.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Round {
    Nearest = 0,
    Up = 1,
    Down = 2,
    Zero = 3,
}

pub const R0: Reg = 0;
pub const R1: Reg = 1;
pub const R2: Reg = 2;
pub const R3: Reg = 3;
pub const R4: Reg = 4;
pub const R5: Reg = 5;
pub const R6: Reg = 6;
/// Scratch for what an instruction cannot hold (large immediates, Status).
pub const IP0: Reg = 16;
pub const IP1: Reg = 17;
pub const FP: Reg = 29;
pub const LR: Reg = 30;
pub const ZR: Reg = 31;
pub const SP: Reg = 31;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Cond {
    Eq = 0,
    Ne = 1,
    Hs = 2,
    Lo = 3,
    Mi = 4,
    Pl = 5,
    Vs = 6,
    Vc = 7,
    Hi = 8,
    Ls = 9,
    Ge = 10,
    Lt = 11,
    Gt = 12,
    Le = 13,
    Al = 14,
}

impl Cond {
    fn inverted(self) -> u32 {
        self as u32 ^ 1
    }
}

/// Data-processing operations on two registers (with the second one shifted) or a register and an
/// immediate.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Op {
    Add,
    Adds,
    Sub,
    Subs,
    And,
    Ands,
    Orr,
    Eor,
    Bic,
    Orn,
}

impl Op {
    fn register(self) -> u32 {
        match self {
            Op::Add => 0x0b00_0000,
            Op::Adds => 0x2b00_0000,
            Op::Sub => 0x4b00_0000,
            Op::Subs => 0x6b00_0000,
            Op::And => 0x0a00_0000,
            Op::Ands => 0x6a00_0000,
            Op::Orr => 0x2a00_0000,
            Op::Eor => 0x4a00_0000,
            Op::Bic => 0x0a20_0000,
            Op::Orn => 0x2a20_0000,
        }
    }
}

/// A shift of the second operand.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Shift {
    Lsl(u32),
    Lsr(u32),
    Asr(u32),
}

/// Floating-point arithmetic.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Fp {
    Add = 0x2800,
    Sub = 0x3800,
    Mul = 0x0800,
    Div = 0x1800,
}

#[derive(Default)]
pub struct Emitter {
    pub code: Vec<u32>,
}

/// A value as a logical immediate of this size (N, immr, imms): a rotated run of ones repeated in
/// elements of 2 to 64 bits; None if it is not one.
pub fn logical_imm(size: Size, value: u64) -> Option<(u32, u32, u32)> {
    let value = if size == W { value as u32 as u64 | (value as u32 as u64) << 32 } else { value };
    if value == 0 || value == u64::MAX {
        return None;
    }
    // The smallest element the value repeats.
    let mut bits = 64;
    while bits > 2 {
        let half = bits / 2;
        let mask = (1u64 << half) - 1;
        if value & mask != (value >> half) & mask {
            break;
        }
        bits = half;
    }
    let mask = if bits == 64 { u64::MAX } else { (1u64 << bits) - 1 };
    let element = value & mask;
    let ones = element.count_ones();
    let run = (1u64 << ones) - 1;
    let ror = |v: u64, r: u32| if r == 0 { v } else { (v >> r | v << (bits - r)) & mask };
    let r = (0..bits).find(|&r| ror(element, r) == run)?;
    let immr = (bits - r) % bits;
    let imms = (!(2 * bits - 1) & 0x3f) | (ones - 1);
    Some((u32::from(bits == 64), immr, imms))
}

impl Emitter {
    pub fn emit(&mut self, word: u32) {
        self.code.push(word);
    }

    /// Where the next instruction goes, in words.
    pub fn here(&self) -> usize {
        self.code.len()
    }

    /// rd = rn op rm.
    pub fn alu(&mut self, op: Op, size: Size, rd: Reg, rn: Reg, rm: Reg) {
        self.emit((size as u32) << 31 | op.register() | rm << 16 | rn << 5 | rd);
    }

    /// rd = rn op (rm shifted).
    pub fn alu_shifted(&mut self, op: Op, size: Size, rd: Reg, rn: Reg, rm: Reg, shift: Shift) {
        let (kind, amount) = match shift {
            Shift::Lsl(n) => (0, n),
            Shift::Lsr(n) => (1, n),
            Shift::Asr(n) => (2, n),
        };
        self.emit((size as u32) << 31 | op.register() | kind << 22 | rm << 16 | amount << 10 | rn << 5 | rd);
    }

    /// rd = rn op an immediate, if it can be one: for arithmetic 12 bits, or 12 bits shifted left
    /// 12; for logic a run of ones (`logical_imm`). Whether it could.
    pub fn alu_imm(&mut self, op: Op, size: Size, rd: Reg, rn: Reg, value: u64) -> bool {
        let top = (size as u32) << 31;
        let arithmetic = match op {
            Op::Add => Some(0x1100_0000),
            Op::Adds => Some(0x3100_0000),
            Op::Sub => Some(0x5100_0000),
            Op::Subs => Some(0x7100_0000),
            _ => None,
        };
        if let Some(word) = arithmetic {
            let shifted = value & 0xfff == 0 && value >> 12 < 0x1000;
            if value < 0x1000 {
                self.emit(top | word | (value as u32) << 10 | rn << 5 | rd);
            } else if shifted {
                self.emit(top | word | 1 << 22 | (value as u32 >> 12) << 10 | rn << 5 | rd);
            } else {
                return false;
            }
            return true;
        }
        let word = match op {
            Op::And => 0x1200_0000,
            Op::Orr => 0x3200_0000,
            Op::Eor => 0x5200_0000,
            Op::Ands => 0x7200_0000,
            _ => panic!("no immediate for {op:?}"),
        };
        let Some((n, immr, imms)) = logical_imm(size, value) else { return false };
        self.emit(top | word | n << 22 | immr << 16 | imms << 10 | rn << 5 | rd);
        true
    }

    /// rd = rn op an immediate, through `scratch` if it cannot be one.
    pub fn imm(&mut self, op: Op, size: Size, rd: Reg, rn: Reg, value: u64, scratch: Reg) {
        if !self.alu_imm(op, size, rd, rn, value) {
            self.mov_imm(size, scratch, value);
            self.alu(op, size, rd, rn, scratch);
        }
    }

    /// rd = rn plus a signed value, through `scratch` if it is large (rn is not the zero register).
    pub fn add_signed(&mut self, size: Size, rd: Reg, rn: Reg, value: i64, scratch: Reg) {
        if value >= 0 {
            self.imm(Op::Add, size, rd, rn, value as u64, scratch);
        } else if !self.alu_imm(Op::Sub, size, rd, rn, value.unsigned_abs()) {
            self.mov_imm(size, scratch, value as u64);
            self.alu(Op::Add, size, rd, rn, scratch);
        }
    }

    pub fn cmp(&mut self, size: Size, rn: Reg, rm: Reg) {
        self.alu(Op::Subs, size, ZR, rn, rm);
    }

    /// Compares with an immediate of 12 bits.
    pub fn cmp_imm(&mut self, size: Size, rn: Reg, value: u64) {
        assert!(self.alu_imm(Op::Subs, size, ZR, rn, value));
    }

    /// Sets the flags from rn and a logical immediate.
    pub fn tst_imm(&mut self, size: Size, rn: Reg, value: u64) {
        assert!(self.alu_imm(Op::Ands, size, ZR, rn, value));
    }

    pub fn tst(&mut self, size: Size, rn: Reg, rm: Reg) {
        self.alu(Op::Ands, size, ZR, rn, rm);
    }

    /// Whether rn, a whole X register, is its low word sign-extended: EQ if so.
    pub fn cmp_sign_extended(&mut self, rn: Reg) {
        self.emit(0xeb20_c01f | rn << 16 | rn << 5);
    }

    pub fn mov(&mut self, size: Size, rd: Reg, rm: Reg) {
        self.alu(Op::Orr, size, rd, ZR, rm);
    }

    /// The fewest instructions that make a value: MOVZ or MOVN and MOVKs.
    pub fn mov_imm(&mut self, size: Size, rd: Reg, value: u64) {
        let value = if size == W { value & 0xffff_ffff } else { value };
        let parts = if size == W { 2 } else { 4 };
        let top = (size as u32) << 31;
        let part = |k: u32| (value >> (16 * k)) as u32 & 0xffff;
        let zeros = (0..parts).filter(|&k| part(k) == 0).count();
        let ones = (0..parts).filter(|&k| part(k) == 0xffff).count();
        if ones > zeros {
            // MOVN sets all but the part it names; the rest are MOVKs.
            let first = (0..parts).find(|&k| part(k) != 0xffff).unwrap_or(0);
            self.emit(top | 0x1280_0000 | first << 21 | (!part(first) & 0xffff) << 5 | rd);
            for k in (first + 1..parts).filter(|&k| part(k) != 0xffff) {
                self.emit(top | 0x7280_0000 | k << 21 | part(k) << 5 | rd);
            }
        } else {
            let first = (0..parts).find(|&k| part(k) != 0).unwrap_or(0);
            self.emit(top | 0x5280_0000 | first << 21 | part(first) << 5 | rd);
            for k in (first + 1..parts).filter(|&k| part(k) != 0) {
                self.emit(top | 0x7280_0000 | k << 21 | part(k) << 5 | rd);
            }
        }
    }

    fn bitfield(&mut self, base: u32, size: Size, rd: Reg, rn: Reg, immr: u32, imms: u32) {
        let s = size as u32;
        self.emit(s << 31 | base | s << 22 | immr << 16 | imms << 10 | rn << 5 | rd);
    }

    fn bits(size: Size) -> u32 {
        if size == X { 64 } else { 32 }
    }

    pub fn lsl(&mut self, size: Size, rd: Reg, rn: Reg, amount: u32) {
        let bits = Self::bits(size);
        self.bitfield(0x5300_0000, size, rd, rn, (bits - amount) % bits, bits - 1 - amount);
    }

    pub fn lsr(&mut self, size: Size, rd: Reg, rn: Reg, amount: u32) {
        self.bitfield(0x5300_0000, size, rd, rn, amount, Self::bits(size) - 1);
    }

    pub fn asr(&mut self, size: Size, rd: Reg, rn: Reg, amount: u32) {
        self.bitfield(0x1300_0000, size, rd, rn, amount, Self::bits(size) - 1);
    }

    /// rd = `width` bits of rn from bit `lsb`, zero-extended.
    pub fn ubfx(&mut self, size: Size, rd: Reg, rn: Reg, lsb: u32, width: u32) {
        self.bitfield(0x5300_0000, size, rd, rn, lsb, lsb + width - 1);
    }

    /// rd = `width` bits of rn from bit `lsb`, sign-extended.
    pub fn sbfx(&mut self, size: Size, rd: Reg, rn: Reg, lsb: u32, width: u32) {
        self.bitfield(0x1300_0000, size, rd, rn, lsb, lsb + width - 1);
    }

    /// rd = the low `width` bits of rn at bit `lsb`, sign-extended from there.
    pub fn sbfiz(&mut self, size: Size, rd: Reg, rn: Reg, lsb: u32, width: u32) {
        let bits = Self::bits(size);
        self.bitfield(0x1300_0000, size, rd, rn, (bits - lsb) % bits, width - 1);
    }

    /// rd = rn's low word, sign-extended.
    pub fn sxtw(&mut self, rd: Reg, rn: Reg) {
        self.sbfx(X, rd, rn, 0, 32);
    }

    /// rd = rn rotated right.
    pub fn ror(&mut self, size: Size, rd: Reg, rn: Reg, amount: u32) {
        let s = size as u32;
        self.emit(s << 31 | 0x1380_0000 | s << 22 | rn << 16 | amount << 10 | rn << 5 | rd);
    }

    fn two_source(&mut self, opcode: u32, size: Size, rd: Reg, rn: Reg, rm: Reg) {
        self.emit((size as u32) << 31 | 0x1ac0_0000 | rm << 16 | opcode << 10 | rn << 5 | rd);
    }

    pub fn lslv(&mut self, size: Size, rd: Reg, rn: Reg, rm: Reg) {
        self.two_source(8, size, rd, rn, rm);
    }

    pub fn lsrv(&mut self, size: Size, rd: Reg, rn: Reg, rm: Reg) {
        self.two_source(9, size, rd, rn, rm);
    }

    pub fn asrv(&mut self, size: Size, rd: Reg, rn: Reg, rm: Reg) {
        self.two_source(10, size, rd, rn, rm);
    }

    pub fn udiv(&mut self, size: Size, rd: Reg, rn: Reg, rm: Reg) {
        self.two_source(2, size, rd, rn, rm);
    }

    pub fn sdiv(&mut self, size: Size, rd: Reg, rn: Reg, rm: Reg) {
        self.two_source(3, size, rd, rn, rm);
    }

    /// rd = ra - rn * rm.
    pub fn msub(&mut self, size: Size, rd: Reg, rn: Reg, rm: Reg, ra: Reg) {
        self.emit((size as u32) << 31 | 0x1b00_8000 | rm << 16 | ra << 10 | rn << 5 | rd);
    }

    /// The X register rd = the W registers rn * rm.
    pub fn mull(&mut self, signed: bool, rd: Reg, rn: Reg, rm: Reg) {
        self.emit(if signed { 0x9b20_7c00 } else { 0x9ba0_7c00 } | rm << 16 | rn << 5 | rd);
    }

    /// rd = rn * rm.
    pub fn mul(&mut self, size: Size, rd: Reg, rn: Reg, rm: Reg) {
        self.emit((size as u32) << 31 | 0x1b00_7c00 | rm << 16 | rn << 5 | rd);
    }

    /// The high 64 bits of signed rn * rm.
    pub fn smulh(&mut self, rd: Reg, rn: Reg, rm: Reg) {
        self.emit(0x9b40_7c00 | rm << 16 | rn << 5 | rd);
    }

    /// The high 64 bits of unsigned rn * rm.
    pub fn umulh(&mut self, rd: Reg, rn: Reg, rm: Reg) {
        self.emit(0x9bc0_7c00 | rm << 16 | rn << 5 | rd);
    }

    /// The bytes of each register's size reversed.
    pub fn rev(&mut self, size: Size, rd: Reg, rn: Reg) {
        self.emit(if size == X { 0xdac0_0c00 } else { 0x5ac0_0800 } | rn << 5 | rd);
    }

    /// The bytes of each 16 bits reversed.
    pub fn rev16(&mut self, rd: Reg, rn: Reg) {
        self.emit(0x5ac0_0400 | rn << 5 | rd);
    }

    /// The same in a whole X register.
    pub fn rev16_x(&mut self, rd: Reg, rn: Reg) {
        self.emit(0xdac0_0400 | rn << 5 | rd);
    }

    /// rd = 1 if the condition holds, else 0.
    pub fn cset(&mut self, size: Size, rd: Reg, cond: Cond) {
        self.emit((size as u32) << 31 | 0x1a9f_07e0 | cond.inverted() << 12 | rd);
    }

    /// rd = rn if the condition holds, else rm.
    pub fn csel(&mut self, size: Size, rd: Reg, rn: Reg, rm: Reg, cond: Cond) {
        self.emit((size as u32) << 31 | 0x1a80_0000 | rm << 16 | (cond as u32) << 12 | rn << 5 | rd);
    }

    /// A load or store at [rn + offset], `word` the instruction for this size with no offset, and
    /// `scale` the size's log2.
    fn memory(&mut self, word: u32, scale: u32, rt: Reg, rn: Reg, offset: i32) {
        assert!(offset >= 0 && offset & ((1 << scale) - 1) == 0 && offset >> scale < 0x1000, "offset {offset}");
        self.emit(word | ((offset >> scale) as u32) << 10 | rn << 5 | rt);
    }

    pub fn ldr(&mut self, size: Size, rt: Reg, rn: Reg, offset: i32) {
        match size {
            W => self.memory(0xb940_0000, 2, rt, rn, offset),
            X => self.memory(0xf940_0000, 3, rt, rn, offset),
        }
    }

    pub fn str(&mut self, size: Size, rt: Reg, rn: Reg, offset: i32) {
        match size {
            W => self.memory(0xb900_0000, 2, rt, rn, offset),
            X => self.memory(0xf900_0000, 3, rt, rn, offset),
        }
    }

    /// A load at [rn + offset] of any alignment, within 256 bytes (LDUR).
    pub fn ldr_byte_offset(&mut self, size: Size, rt: Reg, rn: Reg, offset: i32) {
        let word = if size == X { 0xf840_0000 } else { 0xb840_0000 };
        self.emit(word | (offset as u32 & 0x1ff) << 12 | rn << 5 | rt);
    }

    /// A store at [rn + offset] of any alignment, within 256 bytes (STUR).
    pub fn str_byte_offset(&mut self, size: Size, rt: Reg, rn: Reg, offset: i32) {
        let word = if size == X { 0xf800_0000 } else { 0xb800_0000 };
        self.emit(word | (offset as u32 & 0x1ff) << 12 | rn << 5 | rt);
    }

    /// A 32-bit load, sign-extended into the X register.
    pub fn ldrsw(&mut self, rt: Reg, rn: Reg, offset: i32) {
        self.memory(0xb980_0000, 2, rt, rn, offset);
    }

    pub fn strh(&mut self, rt: Reg, rn: Reg, offset: i32) {
        self.memory(0x7900_0000, 1, rt, rn, offset);
    }

    pub fn ldrh(&mut self, rt: Reg, rn: Reg, offset: i32) {
        self.memory(0x7940_0000, 1, rt, rn, offset);
    }

    /// A halfword, sign-extended into the W register.
    pub fn ldrsh(&mut self, rt: Reg, rn: Reg, offset: i32) {
        self.memory(0x79c0_0000, 1, rt, rn, offset);
    }

    pub fn strb(&mut self, rt: Reg, rn: Reg, offset: i32) {
        self.memory(0x3900_0000, 0, rt, rn, offset);
    }

    /// Loads and stores at [rn + rm], the X registers.
    fn indexed(&mut self, word: u32, rt: Reg, rn: Reg, rm: Reg) {
        self.emit(word | rm << 16 | rn << 5 | rt);
    }

    pub fn ldr_reg(&mut self, size: Size, rt: Reg, rn: Reg, rm: Reg) {
        self.indexed(if size == X { 0xf860_6800 } else { 0xb860_6800 }, rt, rn, rm);
    }

    pub fn str_reg(&mut self, size: Size, rt: Reg, rn: Reg, rm: Reg) {
        self.indexed(if size == X { 0xf820_6800 } else { 0xb820_6800 }, rt, rn, rm);
    }

    pub fn ldrb_reg(&mut self, rt: Reg, rn: Reg, rm: Reg) {
        self.indexed(0x3860_6800, rt, rn, rm);
    }

    /// A byte, sign-extended into the X register.
    pub fn ldrsb_reg(&mut self, rt: Reg, rn: Reg, rm: Reg) {
        self.indexed(0x38a0_6800, rt, rn, rm);
    }

    pub fn strb_reg(&mut self, rt: Reg, rn: Reg, rm: Reg) {
        self.indexed(0x3820_6800, rt, rn, rm);
    }

    pub fn ldrh_reg(&mut self, rt: Reg, rn: Reg, rm: Reg) {
        self.indexed(0x7860_6800, rt, rn, rm);
    }

    pub fn strh_reg(&mut self, rt: Reg, rn: Reg, rm: Reg) {
        self.indexed(0x7820_6800, rt, rn, rm);
    }

    /// A word at [rn + rm * 4].
    pub fn ldr_scaled(&mut self, rt: Reg, rn: Reg, rm: Reg) {
        self.indexed(0xb860_7800, rt, rn, rm);
    }

    /// Stores a pair of X registers at [rn + offset], with `writeback` before the store.
    pub fn stp(&mut self, a: Reg, b: Reg, rn: Reg, offset: i32, writeback: bool) {
        let imm7 = (offset / 8) as u32 & 0x7f;
        self.emit(if writeback { 0xa980_0000 } else { 0xa900_0000 } | imm7 << 15 | b << 10 | rn << 5 | a);
    }

    /// Loads a pair of X registers from [rn + offset], with `writeback` after the load.
    pub fn ldp(&mut self, a: Reg, b: Reg, rn: Reg, offset: i32, writeback: bool) {
        let imm7 = (offset / 8) as u32 & 0x7f;
        self.emit(if writeback { 0xa8c0_0000 } else { 0xa940_0000 } | imm7 << 15 | b << 10 | rn << 5 | a);
    }

    pub fn blr(&mut self, rn: Reg) {
        self.emit(0xd63f_0000 | rn << 5);
    }

    pub fn br(&mut self, rn: Reg) {
        self.emit(0xd61f_0000 | rn << 5);
    }

    pub fn ret(&mut self) {
        self.emit(0xd65f_03c0);
    }

    /// A branch to patch: B for Al, else B.cond. Its index.
    pub fn b(&mut self, cond: Cond) -> usize {
        let at = self.here();
        self.emit(if cond == Cond::Al { 0x1400_0000 } else { 0x5400_0000 | cond as u32 });
        at
    }

    /// A call to patch. Its index.
    pub fn bl(&mut self) -> usize {
        let at = self.here();
        self.emit(0x9400_0000);
        at
    }

    /// A branch to patch, taken if rt is zero, or with `nonzero` if it is not. Its index.
    pub fn cbz(&mut self, nonzero: bool, size: Size, rt: Reg) -> usize {
        let at = self.here();
        self.emit((size as u32) << 31 | if nonzero { 0x3500_0000 } else { 0x3400_0000 } | rt);
        at
    }

    /// A branch to patch, taken if a bit of rt is clear, or with `set` if it is. Its index.
    pub fn tbz(&mut self, set: bool, rt: Reg, bit: u32) -> usize {
        let at = self.here();
        self.emit((bit >> 5) << 31 | if set { 0x3700_0000 } else { 0x3600_0000 } | (bit & 31) << 19 | rt);
        at
    }

    /// The address of the word at `offset` words from here, into rd.
    pub fn adr(&mut self, rd: Reg, offset: i32) {
        let bytes = 4 * offset as u32;
        self.emit(0x1000_0000 | (bytes & 3) << 29 | (bytes >> 2 & 0x7_ffff) << 5 | rd);
    }

    /// The branch at `branch` goes to the next instruction.
    pub fn patch_here(&mut self, branch: usize) {
        let here = self.here();
        self.patch(branch, here);
    }

    /// The branch at `branch` goes to the instruction at `target`.
    pub fn patch(&mut self, branch: usize, target: usize) {
        self.code[branch] = patched(self.code[branch], target as i64 - branch as i64);
    }

    // Floating point. Registers are the D or S views of v0-v31.

    /// The S register rd = a W register.
    pub fn fmov_to_single(&mut self, rd: Reg, rn: Reg) {
        self.emit(0x1e27_0000 | rn << 5 | rd);
    }

    /// A W register = the S register rn.
    pub fn fmov_from_single(&mut self, rd: Reg, rn: Reg) {
        self.emit(0x1e26_0000 | rn << 5 | rd);
    }

    /// The D register rd = an X register.
    pub fn fmov_to_double(&mut self, rd: Reg, rn: Reg) {
        self.emit(0x9e67_0000 | rn << 5 | rd);
    }

    /// An X register = the D register rn.
    pub fn fmov_from_double(&mut self, rd: Reg, rn: Reg) {
        self.emit(0x9e66_0000 | rn << 5 | rd);
    }

    /// rd = rn op rm, single precision.
    pub fn fp_single(&mut self, op: Fp, rd: Reg, rn: Reg, rm: Reg) {
        self.emit(0x1e20_0000 | op as u32 | rm << 16 | rn << 5 | rd);
    }

    /// rd = rn op rm, double precision.
    pub fn fp_double(&mut self, op: Fp, rd: Reg, rn: Reg, rm: Reg) {
        self.emit(0x1e60_0000 | op as u32 | rm << 16 | rn << 5 | rd);
    }

    /// rd = ra - rn * rm, double precision, rounded once.
    pub fn fmsub_double(&mut self, rd: Reg, rn: Reg, rm: Reg, ra: Reg) {
        self.emit(0x1f40_8000 | rm << 16 | ra << 10 | rn << 5 | rd);
    }

    /// rd = sqrt(rn), single precision.
    pub fn fsqrt_single(&mut self, rd: Reg, rn: Reg) {
        self.emit(0x1e21_c000 | rn << 5 | rd);
    }

    /// rd = sqrt(rn), double precision.
    pub fn fsqrt_double(&mut self, rd: Reg, rn: Reg) {
        self.emit(0x1e61_c000 | rn << 5 | rd);
    }

    /// The D register rd = the S register rn.
    pub fn fcvt_to_double(&mut self, rd: Reg, rn: Reg) {
        self.emit(0x1e22_c000 | rn << 5 | rd);
    }

    /// The S register rd = the D register rn.
    pub fn fcvt_to_single(&mut self, rd: Reg, rn: Reg) {
        self.emit(0x1e62_4000 | rn << 5 | rd);
    }

    pub fn fcmp_single(&mut self, rn: Reg, rm: Reg) {
        self.emit(0x1e20_2000 | rm << 16 | rn << 5);
    }

    pub fn fcmp_double(&mut self, rn: Reg, rm: Reg) {
        self.emit(0x1e60_2000 | rm << 16 | rn << 5);
    }

    pub fn fcmp_double_zero(&mut self, rn: Reg) {
        self.emit(0x1e60_2008 | rn << 5);
    }

    /// A W or X register = the S register rn as an integer, rounded as `round` says (to nearest: ties
    /// to even).
    pub fn fcvt_single_to_int(&mut self, size: Size, round: Round, rd: Reg, rn: Reg) {
        self.emit((size as u32) << 31 | 0x1e20_0000 | (round as u32) << 19 | rn << 5 | rd);
    }

    /// A W or X register = the D register rn as an integer, rounded as `round` says (to nearest: ties
    /// to even).
    pub fn fcvt_double_to_int(&mut self, size: Size, round: Round, rd: Reg, rn: Reg) {
        self.emit((size as u32) << 31 | 0x1e60_0000 | (round as u32) << 19 | rn << 5 | rd);
    }

    /// The S register rd = a W or X register as a signed integer.
    pub fn scvtf_single(&mut self, size: Size, rd: Reg, rn: Reg) {
        self.emit((size as u32) << 31 | 0x1e22_0000 | rn << 5 | rd);
    }

    /// The D register rd = a W or X register as a signed integer.
    pub fn scvtf_double(&mut self, size: Size, rd: Reg, rn: Reg) {
        self.emit((size as u32) << 31 | 0x1e62_0000 | rn << 5 | rd);
    }
}

/// A branch's word going `offset` words on: B and BL, B.cond, CBZ and CBNZ, TBZ and TBNZ.
pub fn patched(word: u32, offset: i64) -> u32 {
    if word & 0x7c00_0000 == 0x1400_0000 {
        assert!((-(1 << 25)..1 << 25).contains(&offset), "branch too far");
        word & 0xfc00_0000 | (offset as u32 & 0x3ff_ffff)
    } else if word & 0x7e00_0000 == 0x3600_0000 {
        assert!((-(1 << 13)..1 << 13).contains(&offset), "branch too far");
        word & 0xfff8_001f | (offset as u32 & 0x3fff) << 5
    } else {
        assert!((-(1 << 18)..1 << 18).contains(&offset), "branch too far");
        word & 0xff00_001f | (offset as u32 & 0x7_ffff) << 5
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Words checked against an assembler (llvm-mc --triple=aarch64 --show-encoding).
    #[test]
    fn instructions_encode_as_the_assembler_does() {
        let mut e = Emitter::default();
        e.alu(Op::Add, W, 0, 1, 2); // add w0, w1, w2
        e.alu(Op::Add, X, 0, 1, 2); // add x0, x1, x2
        e.alu(Op::Subs, X, 3, 4, 5); // subs x3, x4, x5
        e.alu(Op::Orn, X, 6, ZR, 7); // mvn x6, x7
        e.alu_shifted(Op::Orr, W, 16, 16, 1, Shift::Lsl(12)); // orr w16, w16, w1, lsl #12
        e.cmp(X, 9, ZR); // cmp x9, xzr
        assert!(e.alu_imm(Op::Add, X, 2, 9, 0x7ff)); // add x2, x9, #0x7ff
        assert!(e.alu_imm(Op::Subs, X, ZR, 3, 0x80_0000)); // cmp x3, #0x800, lsl #12
        assert!(e.alu_imm(Op::Eor, X, 2, 2, 0xffff_ffff_8000_0000)); // eor x2, x2, #0xffffffff80000000
        assert!(e.alu_imm(Op::And, X, 2, 2, !0x2000_0000)); // and x2, x2, #0xffffffffdfffffff
        assert!(e.alu_imm(Op::And, W, 2, 2, 0xffff_00ff)); // and w2, w2, #0xffff00ff
        assert!(e.alu_imm(Op::Ands, W, 2, 2, 0xffff_fffe)); // ands w2, w2, #0xfffffffe
        assert!(!e.alu_imm(Op::And, W, 2, 2, 0xff57_ffff));
        e.tst_imm(W, 16, 0x18); // tst w16, #0x18
        e.cmp_sign_extended(0); // cmp x0, w0, sxtw
        e.mov(X, 19, 1); // mov x19, x1
        e.mov_imm(W, 25, 0x8001_0040); // mov w25, #0x40; movk w25, #0x8001, lsl #16
        e.mov_imm(W, 1, 0xffff_fffc); // mov w1, #-4
        e.mov_imm(X, 16, 0x0000_0001_0203_0405); // movz, movk, movk
        e.mov_imm(X, 2, 0xffff_ffff_ffff_8000); // mov x2, #-0x8000
        e.mov_imm(X, 2, 0xffff_ffff_8000_0000); // movn x2, #0xffff; movk x2, #0x8000, lsl #16
        e.mov_imm(W, 3, 0); // mov w3, #0
        e.lsl(W, 2, 0, 1); // lsl w2, w0, #1
        e.lsl(X, 1, 0, 40); // lsl x1, x0, #40
        e.lsr(W, 3, 1, 12); // lsr w3, w1, #12
        e.lsr(X, 3, 1, 33); // lsr x3, x1, #33
        e.asr(X, 3, 1, 32); // asr x3, x1, #32
        e.ubfx(W, 3, 0, 23, 8); // ubfx w3, w0, #23, #8
        e.sbfx(X, 9, 10, 5, 32); // sbfx x9, x10, #5, #32
        e.sbfiz(X, 9, 10, 5, 27); // sbfiz x9, x10, #5, #27
        e.sxtw(9, 10); // sxtw x9, w10
        e.ror(X, 3, 2, 2); // ror x3, x2, #2
        e.lslv(W, 0, 1, 2); // lsl w0, w1, w2
        e.lsrv(X, 0, 1, 2); // lsr x0, x1, x2
        e.asrv(X, 0, 1, 2); // asr x0, x1, x2
        e.sdiv(W, 2, 0, 1); // sdiv w2, w0, w1
        e.udiv(W, 2, 0, 1); // udiv w2, w0, w1
        e.msub(W, 1, 2, 3, 4); // msub w1, w2, w3, w4
        e.mull(true, 0, 2, 3); // smull x0, w2, w3
        e.mull(false, 0, 2, 3); // umull x0, w2, w3
        e.rev(W, 0, 1); // rev w0, w1
        e.rev(X, 0, 1); // rev x0, x1
        e.rev16(0, 1); // rev16 w0, w1
        e.cset(W, 23, Cond::Lt); // cset w23, lt
        e.csel(X, 0, 2, 0, Cond::Ne); // csel x0, x2, x0, ne
        e.ldr(W, 16, 19, 0x40); // ldr w16, [x19, #0x40]
        e.ldr(X, 9, 19, 0x88); // ldr x9, [x19, #0x88]
        e.str(W, 2, 21, 72); // str w2, [x21, #72]
        e.str(X, 9, 19, 0x88); // str x9, [x19, #0x88]
        e.ldrsw(9, 19, 0x40); // ldrsw x9, [x19, #0x40]
        e.strh(16, 19, 24); // strh w16, [x19, #24]
        e.ldr_reg(W, 9, 20, 2); // ldr w9, [x20, x2]
        e.ldr_reg(X, 9, 20, 2); // ldr x9, [x20, x2]
        e.str_reg(W, 0, 20, 2); // str w0, [x20, x2]
        e.str_reg(X, 0, 20, 2); // str x0, [x20, x2]
        e.ldrb_reg(9, 20, 2); // ldrb w9, [x20, x2]
        e.ldrsb_reg(9, 20, 2); // ldrsb x9, [x20, x2]
        e.strb_reg(9, 20, 2); // strb w9, [x20, x2]
        e.ldrh_reg(9, 20, 2); // ldrh w9, [x20, x2]
        e.strh_reg(0, 20, 2); // strh w0, [x20, x2]
        e.ldr_scaled(3, 22, 3); // ldr w3, [x22, x3, lsl #2]
        e.stp(FP, LR, SP, -96, true); // stp x29, x30, [sp, #-96]!
        e.stp(19, 20, SP, 16, false); // stp x19, x20, [sp, #16]
        e.stp(0, 2, 19, 0, false); // stp x0, x2, [x19]
        e.ldp(19, 20, SP, 16, false); // ldp x19, x20, [sp, #16]
        e.ldp(FP, LR, SP, 96, true); // ldp x29, x30, [sp], #96
        e.blr(16); // blr x16
        e.br(1); // br x1
        e.ret(); // ret
        e.adr(4, 3); // adr x4, #12
        e.fmov_to_single(0, 1); // fmov s0, w1
        e.fmov_from_single(2, 4); // fmov w2, s4
        e.fmov_to_double(0, 1); // fmov d0, x1
        e.fmov_from_double(2, 4); // fmov x2, d4
        e.fp_single(Fp::Add, 4, 0, 1); // fadd s4, s0, s1
        e.fp_single(Fp::Sub, 4, 0, 1); // fsub s4, s0, s1
        e.fp_single(Fp::Mul, 4, 0, 1); // fmul s4, s0, s1
        e.fp_single(Fp::Div, 4, 0, 1); // fdiv s4, s0, s1
        e.fp_double(Fp::Add, 4, 0, 1); // fadd d4, d0, d1
        e.fp_double(Fp::Sub, 4, 0, 1); // fsub d4, d0, d1
        e.fp_double(Fp::Mul, 4, 0, 1); // fmul d4, d0, d1
        e.fp_double(Fp::Div, 4, 0, 1); // fdiv d4, d0, d1
        e.fmsub_double(6, 0, 1, 2); // fmsub d6, d0, d1, d2
        e.fcvt_to_double(2, 0); // fcvt d2, s0
        e.fcvt_to_single(4, 0); // fcvt s4, d0
        e.fcmp_single(0, 1); // fcmp s0, s1
        e.fcmp_double(0, 1); // fcmp d0, d1
        e.fcmp_double_zero(6); // fcmp d6, #0.0
        e.fcvt_single_to_int(W, Round::Zero, 2, 0); // fcvtzs w2, s0
        e.fcvt_single_to_int(W, Round::Nearest, 2, 0); // fcvtns w2, s0
        e.scvtf_single(W, 1, 2); // scvtf s1, w2
        e.scvtf_double(W, 1, 2); // scvtf d1, w2
        e.ldrh(1, 2, 6); // ldrh w1, [x2, #6]
        e.ldrsh(0, 2, 14); // ldrsh w0, [x2, #14]
        e.strb(0, 19, 5); // strb w0, [x19, #5]
        e.rev16_x(1, 1); // rev16 x1, x1
        e.ldr_byte_offset(X, 1, 2, 6); // ldur x1, [x2, #6]
        e.str_byte_offset(X, 1, 0, 8); // stur x1, [x0, #8]
        e.ldr_byte_offset(W, 1, 2, 10); // ldur w1, [x2, #10]
        e.str_byte_offset(W, 1, 2, 14); // stur w1, [x2, #14]
        assert_eq!(e.code, EXPECTED);
    }

    const EXPECTED: &[u32] = &[
        0x0b02_0020, 0x8b02_0020, 0xeb05_0083, 0xaa27_03e6, 0x2a01_3210, 0xeb1f_013f, 0x911f_fd22, 0xf160_007f,
        0xd261_8042, 0x9262_f842, 0x1210_5c42, 0x721f_7842, 0x721d_061f, 0xeb20_c01f, 0xaa01_03f3, 0x5280_0819,
        0x72b0_0039, 0x1280_0061, 0xd280_80b0, 0xf2a0_4070, 0xf2c0_0030, 0x928f_ffe2, 0x929f_ffe2, 0xf2b0_0002,
        0x5280_0003, 0x531f_7802, 0xd358_5c01, 0x530c_7c23, 0xd361_fc23, 0x9360_fc23, 0x5317_7803, 0x9345_9149,
        0x937b_6949, 0x9340_7d49, 0x93c2_0843, 0x1ac2_2020, 0x9ac2_2420, 0x9ac2_2820, 0x1ac1_0c02, 0x1ac1_0802,
        0x1b03_9041, 0x9b23_7c40, 0x9ba3_7c40, 0x5ac0_0820, 0xdac0_0c20, 0x5ac0_0420, 0x1a9f_a7f7, 0x9a80_1040,
        0xb940_4270, 0xf940_4669, 0xb900_4aa2, 0xf900_4669, 0xb980_4269, 0x7900_3270, 0xb862_6a89, 0xf862_6a89,
        0xb822_6a80, 0xf822_6a80, 0x3862_6a89, 0x38a2_6a89, 0x3822_6a89, 0x7862_6a89, 0x7822_6a80, 0xb863_7ac3,
        0xa9ba_7bfd, 0xa901_53f3, 0xa900_0a60, 0xa941_53f3, 0xa8c6_7bfd, 0xd63f_0200, 0xd61f_0020, 0xd65f_03c0,
        0x1000_0064, 0x1e27_0020, 0x1e26_0082, 0x9e67_0020, 0x9e66_0082, 0x1e21_2804, 0x1e21_3804, 0x1e21_0804,
        0x1e21_1804, 0x1e61_2804, 0x1e61_3804, 0x1e61_0804, 0x1e61_1804, 0x1f41_8806, 0x1e22_c002, 0x1e62_4004,
        0x1e21_2000, 0x1e61_2000, 0x1e60_20c8, 0x1e38_0002, 0x1e20_0002, 0x1e22_0041, 0x1e62_0041, 0x7940_0c41,
        0x79c0_1c40, 0x3900_1660, 0xdac0_0421, 0xf840_6041, 0xf800_8001, 0xb840_a041, 0xb800_e041,
    ];

    #[test]
    fn logical_immediates_are_runs_of_ones() {
        assert_eq!(logical_imm(W, 1), Some((0, 0, 0)));
        assert_eq!(logical_imm(W, 0x8000_0000), Some((0, 1, 0)));
        assert_eq!(logical_imm(W, 0x5555_5555), Some((0, 0, 0x3c)));
        assert_eq!(logical_imm(X, 0xffff_ffff_8000_0000), Some((1, 33, 32)));
        assert_eq!(logical_imm(W, 0x1234), None);
        assert_eq!(logical_imm(X, 0), None);
    }

    #[test]
    fn multiply_encodes_correctly() {
        let mut e = Emitter::default();
        e.mul(X, 0, 2, 3);
        e.mul(W, 0, 2, 3);
        e.smulh(1, 2, 3);
        e.umulh(1, 2, 3);
        assert_eq!(e.code, &[0x9b03_7c40, 0x1b03_7c40, 0x9b43_7c41, 0x9bc3_7c41]);
    }

    #[test]
    fn branches_are_patched_both_ways() {
        let mut e = Emitter::default();
        let back = e.here();
        e.mov(W, 0, 1);
        let b = e.b(Cond::Al);
        e.patch(b, back);
        let c = e.b(Cond::Ne);
        let z = e.cbz(true, W, 3);
        let t = e.tbz(true, 16, 29);
        e.mov(W, 0, 1);
        e.patch_here(c);
        e.patch_here(z);
        e.patch_here(t);
        assert_eq!(e.code, [0x2a01_03e0, 0x17ff_ffff, 0x5400_0081, 0x3500_0063, 0x37e8_0050, 0x2a01_03e0]);
    }

    #[test]
    fn a_doubles_sign_is_a_logical_immediate() {
        let mut e = Emitter::default();
        assert!(e.alu_imm(Op::Eor, X, 2, 0, 1 << 63));
        assert!(e.alu_imm(Op::And, X, 2, 0, !(1 << 63)));
        assert_eq!(e.code, [
            0xd241_0002, // eor x2, x0, #0x8000000000000000
            0x9240_f802, // and x2, x0, #0x7fffffffffffffff
        ]);
    }

    #[test]
    fn fcvt_to_int_encodes_correctly() {
        let mut e = Emitter::default();
        e.fcvt_single_to_int(W, Round::Nearest, 0, 1);
        e.fcvt_single_to_int(W, Round::Zero, 0, 1);
        e.fcvt_single_to_int(W, Round::Up, 0, 1);
        e.fcvt_single_to_int(W, Round::Down, 0, 1);
        e.fcvt_double_to_int(W, Round::Nearest, 0, 1);
        e.fcvt_double_to_int(W, Round::Zero, 0, 1);
        e.fcvt_double_to_int(W, Round::Up, 0, 1);
        e.fcvt_double_to_int(W, Round::Down, 0, 1);
        e.fcvt_double_to_int(X, Round::Nearest, 0, 1);
        e.fcvt_double_to_int(X, Round::Zero, 0, 1);
        e.fcvt_double_to_int(X, Round::Up, 0, 1);
        e.fcvt_double_to_int(X, Round::Down, 0, 1);
        assert_eq!(e.code, [
            0x1e20_0020, // fcvtns w0, s1
            0x1e38_0020, // fcvtzs w0, s1
            0x1e28_0020, // fcvtps w0, s1
            0x1e30_0020, // fcvtms w0, s1
            0x1e60_0020, // fcvtns w0, d1
            0x1e78_0020, // fcvtzs w0, d1
            0x1e68_0020, // fcvtps w0, d1
            0x1e70_0020, // fcvtms w0, d1
            0x9e60_0020, // fcvtns x0, d1
            0x9e78_0020, // fcvtzs x0, d1
            0x9e68_0020, // fcvtps x0, d1
            0x9e70_0020, // fcvtms x0, d1
        ]);
    }

    #[test]
    fn fsqrt_matches_llvm_mc() {
        let mut e = Emitter::default();
        e.fsqrt_single(4, 0); // fsqrt s4, s0
        e.fsqrt_double(2, 0); // fsqrt d2, d0
        assert_eq!(e.code, [
            0x1e21_c004, // fsqrt s4, s0
            0x1e61_c002, // fsqrt d2, d0
        ]);
    }

    #[test]
    fn scvtf_matches_llvm_mc() {
        let mut e = Emitter::default();
        e.scvtf_single(W, 1, 2); // scvtf s1, w2
        e.scvtf_single(X, 1, 2); // scvtf s1, x2
        e.scvtf_double(W, 1, 2); // scvtf d1, w2
        e.scvtf_double(X, 1, 2); // scvtf d1, x2
        assert_eq!(e.code, [
            0x1e22_0041,
            0x9e22_0041,
            0x1e62_0041,
            0x9e62_0041,
        ]);
    }
}
