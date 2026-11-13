//! An emitter of 64-bit ARM (A64) instructions, as many as the JIT needs on
//! a 64-bit host: each method appends one instruction's word (a few
//! append more, said so). Registers are numbers 0-30; 31 is the zero
//! register, or the stack pointer where an instruction takes it. Data
//! instructions work on the 32-bit W registers unless their name says X.

pub type Reg = u32;

pub const W0: Reg = 0;
pub const W1: Reg = 1;
pub const W2: Reg = 2;
pub const W3: Reg = 3;
/// The intra-procedure-call scratch registers: a call's address, and large
/// immediates.
pub const X16: Reg = 16;
pub const X17: Reg = 17;
pub const FP: Reg = 29;
pub const LR: Reg = 30;
pub const ZR: Reg = 31;
pub const SP: Reg = 31;

/// What a call may change: x0-x18 and the link register (AAPCS64).
pub const CALL_CLOBBERS: u32 = 0x7_ffff | 1 << LR;

/// Condition codes (all of them, though the compiler uses some).
#[allow(dead_code)]
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

/// Data operations between registers (shifted register form) and with a
/// logical immediate.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Op {
    Add,
    Adds,
    Sub,
    Subs,
    And,
    Orr,
    Eor,
}

#[derive(Default)]
pub struct Emitter {
    pub code: Vec<u32>,
    /// The registers instructions emitted have written (a call: those it
    /// may change), for the compiler to clear as it pleases.
    pub written: u32,
}

/// A 32-bit value as a logical immediate (N, immr, imms): a rotated run of
/// ones repeated in elements of 2 to 32 bits; None if it is not one.
pub fn logical_imm(value: u32) -> Option<(u32, u32, u32)> {
    if value == 0 || value == u32::MAX {
        return None;
    }
    // The smallest element the value repeats.
    let mut size = 32;
    while size > 2 {
        let half = size / 2;
        let mask = (1u32 << half) - 1;
        if value & mask != (value >> half) & mask {
            break;
        }
        size = half;
    }
    let mask = if size == 32 { u32::MAX } else { (1u32 << size) - 1 };
    let element = value & mask;
    let ones = element.count_ones();
    let run = (1u32 << ones) - 1;
    let ror = |v: u32, r: u32| if r == 0 { v } else { (v >> r | v << (size - r)) & mask };
    let r = (0..size).find(|&r| ror(element, r) == run)?;
    let immr = (size - r) % size;
    let imms = (!(2 * size - 1) & 0x3f) | (ones - 1);
    Some((0, immr, imms))
}

impl Emitter {
    pub fn emit(&mut self, word: u32) {
        self.code.push(word);
    }

    /// Where the next instruction goes, in words.
    pub fn here(&self) -> usize {
        self.code.len()
    }

    fn writes(&mut self, rd: Reg) {
        if rd < 31 {
            self.written |= 1 << rd;
        }
    }

    /// rd = rn op rm, 32-bit (Adds and Subs set the flags).
    pub fn alu(&mut self, op: Op, rd: Reg, rn: Reg, rm: Reg) {
        self.writes(rd);
        let base = match op {
            Op::Add => 0x0b00_0000,
            Op::Adds => 0x2b00_0000,
            Op::Sub => 0x4b00_0000,
            Op::Subs => 0x6b00_0000,
            Op::And => 0x0a00_0000,
            Op::Orr => 0x2a00_0000,
            Op::Eor => 0x4a00_0000,
        };
        self.emit(base | rm << 16 | rn << 5 | rd);
    }

    /// rd = !(rn | rm) as ORN rd, zr, (rn | rm) needs: rd = !rm.
    pub fn mvn(&mut self, rd: Reg, rm: Reg) {
        self.writes(rd);
        self.emit(0x2a20_03e0 | rm << 16 | rd);
    }

    /// Compares rn with rm (32-bit).
    pub fn cmp(&mut self, rn: Reg, rm: Reg) {
        self.emit(0x6b00_001f | rm << 16 | rn << 5);
    }

    /// Compares rn with rm (64-bit).
    pub fn cmp_x(&mut self, rn: Reg, rm: Reg) {
        self.emit(0xeb00_001f | rm << 16 | rn << 5);
    }

    /// rd = rn + imm or - imm (imm12, or imm12 << 12): ADD, ADDS, SUB, SUBS
    /// with an immediate; false (nothing emitted) if it does not fit.
    pub fn alu_imm(&mut self, op: Op, rd: Reg, rn: Reg, imm: u32) -> bool {
        let (field, shift) = if imm < 0x1000 {
            (imm, 0)
        } else if imm & 0xfff == 0 && imm < 0x100_0000 {
            (imm >> 12, 1)
        } else {
            return false;
        };
        let base = match op {
            Op::Add => 0x1100_0000,
            Op::Adds => 0x3100_0000,
            Op::Sub => 0x5100_0000,
            Op::Subs => 0x7100_0000,
            _ => return self.logical(op, rd, rn, imm),
        };
        if rd != ZR {
            self.writes(rd);
        }
        self.emit(base | shift << 22 | field << 10 | rn << 5 | rd);
        true
    }

    /// AND, ORR or EOR with a logical immediate; false (nothing emitted) if
    /// the value is not one.
    pub fn logical(&mut self, op: Op, rd: Reg, rn: Reg, imm: u32) -> bool {
        let Some((n, immr, imms)) = logical_imm(imm) else { return false };
        let base = match op {
            Op::And => 0x1200_0000,
            Op::Orr => 0x3200_0000,
            Op::Eor => 0x5200_0000,
            _ => return false,
        };
        self.writes(rd);
        self.emit(base | n << 22 | immr << 16 | imms << 10 | rn << 5 | rd);
        true
    }

    /// Compares rn with an immediate (CMP, or CMN for its negative): one
    /// instruction, else through `scratch`.
    pub fn cmp_imm(&mut self, rn: Reg, imm: u32, scratch: Reg) {
        if !self.alu_imm(Op::Subs, ZR, rn, imm) && !self.alu_imm(Op::Adds, ZR, rn, imm.wrapping_neg()) {
            self.mov_imm(scratch, imm);
            self.cmp(rn, scratch);
        }
    }

    /// Tests rn's bits against a logical immediate (TST).
    pub fn tst_imm(&mut self, rn: Reg, imm: u32) {
        let (n, immr, imms) = logical_imm(imm).expect("TST takes a logical immediate");
        self.emit(0x7200_001f | n << 22 | immr << 16 | imms << 10 | rn << 5);
    }

    /// rd = rm (32-bit).
    pub fn mov(&mut self, rd: Reg, rm: Reg) {
        self.writes(rd);
        self.emit(0x2a00_03e0 | rm << 16 | rd);
    }

    /// xd = xm (64-bit).
    pub fn mov_x(&mut self, rd: Reg, rm: Reg) {
        self.writes(rd);
        self.emit(0xaa00_03e0 | rm << 16 | rd);
    }

    /// rd = a 32-bit value: MOVZ (or MOVN) and MOVK, one or two words.
    pub fn mov_imm(&mut self, rd: Reg, value: u32) {
        self.writes(rd);
        let (lo, hi) = (value & 0xffff, value >> 16);
        if hi == 0 {
            self.emit(0x5280_0000 | lo << 5 | rd);
        } else if lo == 0 {
            self.emit(0x52a0_0000 | hi << 5 | rd);
        } else if hi == 0xffff {
            // MOVN: the complement of the low half.
            self.emit(0x1280_0000 | (!value & 0xffff) << 5 | rd);
        } else {
            self.emit(0x5280_0000 | lo << 5 | rd);
            self.emit(0x72a0_0000 | hi << 5 | rd);
        }
    }

    /// xd = a 64-bit value (an address): MOVZ and the MOVKs it needs.
    pub fn mov_imm_x(&mut self, rd: Reg, value: u64) {
        self.writes(rd);
        self.emit(0xd280_0000 | ((value & 0xffff) as u32) << 5 | rd);
        for hw in 1..4u32 {
            let part = (value >> (16 * hw) & 0xffff) as u32;
            if part != 0 {
                self.emit(0xf280_0000 | hw << 21 | part << 5 | rd);
            }
        }
    }

    /// rd = rn + value (any 32-bit value, wrapping), through `scratch` when
    /// it fits no immediate.
    pub fn add_imm(&mut self, rd: Reg, rn: Reg, value: u32, scratch: Reg) {
        if value == 0 {
            if rd != rn {
                self.mov(rd, rn);
            }
        } else if !self.alu_imm(Op::Add, rd, rn, value) && !self.alu_imm(Op::Sub, rd, rn, value.wrapping_neg()) {
            self.mov_imm(scratch, value);
            self.alu(Op::Add, rd, rn, scratch);
        }
    }

    /// Shifts by an immediate (1-31).
    pub fn lsl(&mut self, rd: Reg, rn: Reg, sh: u32) {
        self.writes(rd);
        self.emit(0x5300_0000 | (32 - sh) % 32 << 16 | (31 - sh) << 10 | rn << 5 | rd);
    }

    pub fn lsr(&mut self, rd: Reg, rn: Reg, sh: u32) {
        self.writes(rd);
        self.emit(0x5300_7c00 | sh << 16 | rn << 5 | rd);
    }

    pub fn asr(&mut self, rd: Reg, rn: Reg, sh: u32) {
        self.writes(rd);
        self.emit(0x1300_7c00 | sh << 16 | rn << 5 | rd);
    }

    /// xd = xn >> sh (64-bit, logical).
    pub fn lsr_x(&mut self, rd: Reg, rn: Reg, sh: u32) {
        self.writes(rd);
        self.emit(0xd340_fc00 | sh << 16 | rn << 5 | rd);
    }

    /// Shifts by a register: the low five bits of rm, as the R3000A's.
    pub fn lslv(&mut self, rd: Reg, rn: Reg, rm: Reg) {
        self.writes(rd);
        self.emit(0x1ac0_2000 | rm << 16 | rn << 5 | rd);
    }

    pub fn lsrv(&mut self, rd: Reg, rn: Reg, rm: Reg) {
        self.writes(rd);
        self.emit(0x1ac0_2400 | rm << 16 | rn << 5 | rd);
    }

    pub fn asrv(&mut self, rd: Reg, rn: Reg, rm: Reg) {
        self.writes(rd);
        self.emit(0x1ac0_2800 | rm << 16 | rn << 5 | rd);
    }

    /// xd = wn * wm, 64-bit result (SMULL or UMULL).
    pub fn mull(&mut self, signed: bool, rd: Reg, rn: Reg, rm: Reg) {
        self.writes(rd);
        self.emit(if signed { 0x9b20_7c00 } else { 0x9ba0_7c00 } | rm << 16 | rn << 5 | rd);
    }

    /// rd = 1 if the condition holds, else 0.
    pub fn cset(&mut self, rd: Reg, cond: Cond) {
        self.writes(rd);
        self.emit(0x1a9f_07e0 | (cond as u32 ^ 1) << 12 | rd);
    }

    /// rd = the condition holds ? rn : rm.
    pub fn csel(&mut self, rd: Reg, rn: Reg, rm: Reg, cond: Cond) {
        self.writes(rd);
        self.emit(0x1a80_0000 | rm << 16 | (cond as u32) << 12 | rn << 5 | rd);
    }

    /// xd = xn + (wm zero-extended << shift), 64-bit (ADD, extended register).
    pub fn add_uxtw(&mut self, rd: Reg, rn: Reg, rm: Reg, shift: u32) {
        self.writes(rd);
        self.emit(0x8b20_4000 | rm << 16 | shift << 10 | rn << 5 | rd);
    }

    /// xd = xn + imm12 (64-bit).
    pub fn add_x_imm(&mut self, rd: Reg, rn: Reg, imm: u32) {
        assert!(imm < 0x1000);
        self.writes(rd);
        self.emit(0x9100_0000 | imm << 10 | rn << 5 | rd);
    }

    /// A load or store at [xn + offset] of `bytes` (1, 2, 4 or 8): scaled,
    /// unscaled, or through X17 for a far offset. `base` is the
    /// unsigned-offset form's opcode, `unscaled` the 9-bit one's,
    /// `indexed` the register-offset one's (LSL, X index).
    fn mem(&mut self, base: u32, unscaled: u32, indexed: u32, bytes: u32, rt: Reg, rn: Reg, offset: i32) {
        let scaled = offset as u32 / bytes;
        if offset >= 0 && offset as u32 % bytes == 0 && scaled < 0x1000 {
            self.emit(base | scaled << 10 | rn << 5 | rt);
        } else if (-256..256).contains(&offset) {
            self.emit(unscaled | (offset as u32 & 0x1ff) << 12 | rn << 5 | rt);
        } else {
            self.mov_imm_x(X17, offset as i64 as u64);
            self.emit(indexed | X17 << 16 | rn << 5 | rt);
        }
    }

    /// wt = the word at [xn + offset].
    pub fn ldr(&mut self, rt: Reg, rn: Reg, offset: i32) {
        self.writes(rt);
        self.mem(0xb940_0000, 0xb840_0000, 0xb860_6800, 4, rt, rn, offset);
    }

    pub fn str(&mut self, rt: Reg, rn: Reg, offset: i32) {
        self.mem(0xb900_0000, 0xb800_0000, 0xb820_6800, 4, rt, rn, offset);
    }

    /// xt = the doubleword at [xn + offset].
    pub fn ldr_x(&mut self, rt: Reg, rn: Reg, offset: i32) {
        self.writes(rt);
        self.mem(0xf940_0000, 0xf840_0000, 0xf860_6800, 8, rt, rn, offset);
    }

    pub fn str_x(&mut self, rt: Reg, rn: Reg, offset: i32) {
        self.mem(0xf900_0000, 0xf800_0000, 0xf820_6800, 8, rt, rn, offset);
    }

    /// Loads and stores at [xn + wm zero-extended]: the word, halfword or
    /// byte, signed or not.
    fn indexed(&mut self, word: u32, rt: Reg, rn: Reg, rm: Reg) {
        self.emit(word | rm << 16 | rn << 5 | rt);
    }

    pub fn ldr_uxtw(&mut self, rt: Reg, rn: Reg, rm: Reg) {
        self.writes(rt);
        self.indexed(0xb860_4800, rt, rn, rm);
    }

    pub fn str_uxtw(&mut self, rt: Reg, rn: Reg, rm: Reg) {
        self.indexed(0xb820_4800, rt, rn, rm);
    }

    pub fn ldrh_uxtw(&mut self, rt: Reg, rn: Reg, rm: Reg) {
        self.writes(rt);
        self.indexed(0x7860_4800, rt, rn, rm);
    }

    pub fn strh_uxtw(&mut self, rt: Reg, rn: Reg, rm: Reg) {
        self.indexed(0x7820_4800, rt, rn, rm);
    }

    pub fn ldrsh_uxtw(&mut self, rt: Reg, rn: Reg, rm: Reg) {
        self.writes(rt);
        self.indexed(0x78e0_4800, rt, rn, rm);
    }

    pub fn ldrb_uxtw(&mut self, rt: Reg, rn: Reg, rm: Reg) {
        self.writes(rt);
        self.indexed(0x3860_4800, rt, rn, rm);
    }

    pub fn strb_uxtw(&mut self, rt: Reg, rn: Reg, rm: Reg) {
        self.indexed(0x3820_4800, rt, rn, rm);
    }

    pub fn ldrsb_uxtw(&mut self, rt: Reg, rn: Reg, rm: Reg) {
        self.writes(rt);
        self.indexed(0x38e0_4800, rt, rn, rm);
    }

    /// Stores and loads a pair of X registers at [sp + offset]: STP with
    /// pre-index (`writeback`) or a plain offset, LDP with post-index or a
    /// plain offset.
    pub fn stp(&mut self, a: Reg, b: Reg, offset: i32, writeback: bool) {
        let imm7 = (offset / 8) as u32 & 0x7f;
        self.emit(if writeback { 0xa980_0000 } else { 0xa900_0000 } | imm7 << 15 | b << 10 | SP << 5 | a);
    }

    pub fn ldp(&mut self, a: Reg, b: Reg, offset: i32, writeback: bool) {
        self.writes(a);
        self.writes(b);
        let imm7 = (offset / 8) as u32 & 0x7f;
        self.emit(if writeback { 0xa8c0_0000 } else { 0xa940_0000 } | imm7 << 15 | b << 10 | SP << 5 | a);
    }

    /// A call to the address in xn, which may change what a call may.
    pub fn blr(&mut self, rn: Reg) {
        self.written |= CALL_CLOBBERS;
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

    /// A branch to patch, taken if rt is zero (CBZ) or not (CBNZ). Its index.
    pub fn cbz(&mut self, nonzero: bool, rt: Reg) -> usize {
        let at = self.here();
        self.emit(if nonzero { 0x3500_0000 } else { 0x3400_0000 } | rt);
        at
    }

    /// The branch at `branch` goes to the next instruction.
    pub fn patch_here(&mut self, branch: usize) {
        let here = self.here();
        self.patch(branch, here);
    }

    /// The branch at `branch` goes to the instruction at `target`.
    pub fn patch(&mut self, branch: usize, target: usize) {
        let offset = target as i64 - branch as i64;
        let w = self.code[branch];
        self.code[branch] = if w & 0xfc00_0000 == 0x1400_0000 {
            assert!((-(1 << 25)..1 << 25).contains(&offset), "branch too far");
            w & 0xfc00_0000 | (offset as u32 & 0x3ff_ffff)
        } else {
            assert!((-(1 << 18)..1 << 18).contains(&offset), "branch too far");
            w & 0xff00_001f | (offset as u32 & 0x7_ffff) << 5
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Words checked against an assembler (clang -target aarch64, llvm-objdump).
    #[test]
    fn instructions_encode_as_the_assembler_does() {
        let mut e = Emitter::default();
        e.alu(Op::Add, 0, 1, 2); // add w0, w1, w2
        e.alu(Op::Subs, 3, 4, 5); // subs w3, w4, w5
        e.mvn(6, 7); // mvn w6, w7
        e.cmp(0, 1); // cmp w0, w1
        e.cmp_x(0, 2); // cmp x0, x2
        assert!(e.alu_imm(Op::Add, 21, 21, 4)); // add w21, w21, #4
        assert!(e.alu_imm(Op::Subs, ZR, 1, 0x80_0000)); // cmp w1, #0x800, lsl #12
        assert!(e.alu_imm(Op::Adds, ZR, 0, 2)); // cmn w0, #2
        assert!(e.logical(Op::And, 1, 0, 0x1fff_ffff)); // and w1, w0, #0x1fffffff
        assert!(e.logical(Op::And, 1, 1, 0xff9f_ffff)); // and w1, w1, #0xff9fffff
        e.tst_imm(0, 3); // tst w0, #3
        e.tst_imm(0, 0x1_0000); // tst w0, #0x10000
        assert!(e.logical(Op::And, 1, 1, 0xff00)); // and w1, w1, #0xff00
        e.mov(0, 25); // mov w0, w25
        e.mov_x(19, 0); // mov x19, x0
        e.mov_imm(25, 0x8001_0040); // mov w25, #0x40; movk w25, #0x8001, lsl #16
        e.mov_imm(1, 0xffff_fffc); // mov w1, #-4
        e.mov_imm_x(16, 0x0000_0001_0203_0405); // movz x16 #0x405; movk #0x203, lsl 16; movk #1, lsl 32
        e.lsl(0, 0, 4); // lsl w0, w0, #4
        e.lsr(3, 1, 12); // lsr w3, w1, #12
        e.asr(0, 0, 31); // asr w0, w0, #31
        e.lsr_x(1, 0, 32); // lsr x1, x0, #32
        e.lslv(0, 0, 1); // lsl w0, w0, w1
        e.asrv(0, 0, 1); // asr w0, w0, w1
        e.mull(true, 0, 2, 3); // smull x0, w2, w3
        e.mull(false, 0, 2, 3); // umull x0, w2, w3
        e.cset(0, Cond::Lt); // cset w0, lt
        e.csel(25, 16, 25, Cond::Ne); // csel w25, w16, w25, ne
        e.add_uxtw(0, 28, 0, 2); // add x0, x28, w0, uxtw #2
        e.ldr(0, 19, 0x40); // ldr w0, [x19, #64]
        e.str(23, 19, -4); // stur w23, [x19, #-4]
        e.ldr_x(0, 19, 0x88); // ldr x0, [x19, #136]
        e.ldr_uxtw(24, 20, 1); // ldr w24, [x20, w1, uxtw]
        e.ldrsb_uxtw(24, 20, 1); // ldrsb w24, [x20, w1, uxtw]
        e.ldrsh_uxtw(24, 20, 1); // ldrsh w24, [x20, w1, uxtw]
        e.strh_uxtw(2, 20, 1); // strh w2, [x20, w1, uxtw]
        e.ldrb_uxtw(3, 22, 3); // ldrb w3, [x22, w3, uxtw]
        e.stp(FP, LR, -96, true); // stp x29, x30, [sp, #-96]!
        e.stp(19, 20, 16, false); // stp x19, x20, [sp, #16]
        e.ldp(19, 20, 16, false); // ldp x19, x20, [sp, #16]
        e.ldp(FP, LR, 96, true); // ldp x29, x30, [sp], #96
        e.blr(16); // blr x16
        e.br(0); // br x0
        e.ret();
        assert_eq!(
            e.code,
            [
                0x0b02_0020, 0x6b05_0083, 0x2a27_03e6, 0x6b01_001f, 0xeb02_001f, 0x1100_12b5, 0x7160_003f,
                0x3100_081f, 0x1200_7001, 0x1209_7421, 0x7200_041f, 0x7210_001f, 0x1218_1c21, 0x2a19_03e0,
                0xaa00_03f3, 0x5280_0819, 0x72b0_0039, 0x1280_0061, 0xd280_80b0, 0xf2a0_4070, 0xf2c0_0030,
                0x531c_6c00, 0x530c_7c23, 0x131f_7c00, 0xd360_fc01, 0x1ac1_2000, 0x1ac1_2800, 0x9b23_7c40,
                0x9ba3_7c40, 0x1a9f_a7e0, 0x1a99_1219, 0x8b20_4b80, 0xb940_4260, 0xb81f_c277, 0xf940_4660,
                0xb861_4a98, 0x38e1_4a98, 0x78e1_4a98, 0x7821_4a82, 0x3863_4ac3, 0xa9ba_7bfd, 0xa901_53f3,
                0xa941_53f3, 0xa8c6_7bfd, 0xd63f_0200, 0xd61f_0000, 0xd65f_03c0,
            ]
        );
    }

    #[test]
    fn logical_immediates_are_runs_of_ones() {
        assert_eq!(logical_imm(1), Some((0, 0, 0)));
        assert_eq!(logical_imm(0x8000_0000), Some((0, 1, 0)));
        assert_eq!(logical_imm(0x5555_5555), Some((0, 0, 0x3c)));
        assert_eq!(logical_imm(0x1234), None);
        assert_eq!(logical_imm(0), None);
    }

    #[test]
    fn branches_are_patched_both_ways() {
        let mut e = Emitter::default();
        let back = e.here();
        e.mov(0, 1);
        let b = e.b(Cond::Al);
        e.patch(b, back);
        let c = e.b(Cond::Ne);
        let z = e.cbz(true, 3);
        e.mov(0, 1);
        e.patch_here(c);
        e.patch_here(z);
        assert_eq!(e.code, [0x2a01_03e0, 0x17ff_ffff, 0x5400_0061, 0x3500_0043, 0x2a01_03e0]);
    }
}
