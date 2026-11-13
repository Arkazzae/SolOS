//! An emitter of 32-bit ARM (A32) instructions, as many as the JIT needs:
//! each method appends one instruction's word. Registers are numbers 0-15;
//! conditions are the instruction's top four bits.

// It covers more of A32 than the compiler uses yet.
#![allow(dead_code)]

pub type Reg = u32;

pub const R0: Reg = 0;
pub const R1: Reg = 1;
pub const R2: Reg = 2;
pub const R3: Reg = 3;
pub const R12: Reg = 12;
pub const SP: Reg = 13;
pub const LR: Reg = 14;
pub const PC: Reg = 15;

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

/// Data-processing operations, by their opcode field.
#[derive(Clone, Copy)]
pub enum Op {
    And = 0,
    Eor = 1,
    Sub = 2,
    Rsb = 3,
    Add = 4,
    Adc = 5,
    Sbc = 6,
    Tst = 8,
    Cmp = 10,
    Cmn = 11,
    Orr = 12,
    Mov = 13,
    Bic = 14,
    Mvn = 15,
}

/// A shift of the second operand: by an immediate, or by a register.
#[derive(Clone, Copy)]
pub enum Shift {
    Lsl(u32),
    Lsr(u32),
    Asr(u32),
    LslReg(Reg),
    LsrReg(Reg),
    AsrReg(Reg),
}

#[derive(Default)]
pub struct Emitter {
    pub code: Vec<u32>,
    /// The registers instructions emitted have written (a call: those it
    /// may change), for the compiler to clear as it pleases.
    pub written: u32,
}

/// An 8-bit value rotated right by an even amount, as data-processing
/// immediates are encoded; None if the value is not one.
pub fn encode_imm(value: u32) -> Option<u32> {
    (0..16).find_map(|rot| {
        let v = value.rotate_left(2 * rot);
        (v <= 0xff).then_some(rot << 8 | v)
    })
}

impl Emitter {
    pub fn emit(&mut self, word: u32) {
        self.code.push(word);
    }

    /// Where the next instruction goes, in words.
    pub fn here(&self) -> usize {
        self.code.len()
    }

    fn dp(&mut self, cond: Cond, op: Op, set: bool, rn: Reg, rd: Reg, operand: u32) {
        let compare = matches!(op, Op::Tst | Op::Cmp | Op::Cmn);
        if !compare {
            self.written |= 1 << rd;
        }
        let s = u32::from(set || compare);
        self.emit((cond as u32) << 28 | (op as u32) << 21 | s << 20 | rn << 16 | rd << 12 | operand);
    }

    /// `op rd, rn, rm` with rm shifted.
    #[allow(clippy::too_many_arguments)]
    pub fn alu_shifted(&mut self, cond: Cond, op: Op, set: bool, rd: Reg, rn: Reg, rm: Reg, shift: Shift) {
        let operand = match shift {
            Shift::Lsl(n) => (n & 31) << 7 | rm,
            Shift::Lsr(n) => (n & 31) << 7 | 1 << 5 | rm,
            Shift::Asr(n) => (n & 31) << 7 | 2 << 5 | rm,
            Shift::LslReg(rs) => rs << 8 | 1 << 4 | rm,
            Shift::LsrReg(rs) => rs << 8 | 3 << 4 | rm,
            Shift::AsrReg(rs) => rs << 8 | 5 << 4 | rm,
        };
        self.dp(cond, op, set, rn, rd, operand);
    }

    /// `op rd, rn, rm`.
    pub fn alu(&mut self, op: Op, rd: Reg, rn: Reg, rm: Reg) {
        self.alu_shifted(Cond::Al, op, false, rd, rn, rm, Shift::Lsl(0));
    }

    /// `op rd, rn, #imm`, the immediate encodable.
    pub fn alu_imm(&mut self, cond: Cond, op: Op, set: bool, rd: Reg, rn: Reg, imm: u32) -> bool {
        let Some(e) = encode_imm(imm) else { return false };
        self.dp(cond, op, set, rn, rd, 1 << 25 | e);
        true
    }

    pub fn mov(&mut self, rd: Reg, rm: Reg) {
        self.alu(Op::Mov, rd, 0, rm);
    }

    pub fn movw(&mut self, cond: Cond, rd: Reg, imm: u32) {
        self.written |= 1 << rd;
        self.emit((cond as u32) << 28 | 0x0300_0000 | (imm >> 12 & 15) << 16 | rd << 12 | imm & 0xfff);
    }

    pub fn movt(&mut self, cond: Cond, rd: Reg, imm: u32) {
        self.written |= 1 << rd;
        self.emit((cond as u32) << 28 | 0x0340_0000 | (imm >> 12 & 15) << 16 | rd << 12 | imm & 0xfff);
    }

    /// Any 32-bit constant: one instruction when it can, else two.
    pub fn mov_imm(&mut self, rd: Reg, value: u32) {
        if self.alu_imm(Cond::Al, Op::Mov, false, rd, 0, value) || self.alu_imm(Cond::Al, Op::Mvn, false, rd, 0, !value) {
            return;
        }
        self.movw(Cond::Al, rd, value & 0xffff);
        if value >> 16 != 0 {
            self.movt(Cond::Al, rd, value >> 16);
        }
    }

    /// `rd = rn + value`, through `scratch` when the value is not an
    /// immediate.
    pub fn add_imm(&mut self, rd: Reg, rn: Reg, value: u32, scratch: Reg) {
        if value == 0 {
            if rd != rn {
                self.mov(rd, rn);
            }
        } else if !self.alu_imm(Cond::Al, Op::Add, false, rd, rn, value)
            && !self.alu_imm(Cond::Al, Op::Sub, false, rd, rn, value.wrapping_neg())
        {
            self.mov_imm(scratch, value);
            self.alu(Op::Add, rd, rn, scratch);
        }
    }

    /// LDR/STR (word, unsigned byte) with a 12-bit offset either way.
    fn mem(&mut self, cond: Cond, load: bool, byte: bool, rt: Reg, rn: Reg, offset: i32) {
        let (up, magnitude) = if offset < 0 { (0, (-offset) as u32) } else { (1, offset as u32) };
        assert!(magnitude < 4096, "offset out of reach");
        self.written |= u32::from(load) << rt;
        self.emit(
            (cond as u32) << 28
                | 0x0500_0000
                | up << 23
                | u32::from(byte) << 22
                | u32::from(load) << 20
                | rn << 16
                | rt << 12
                | magnitude,
        );
    }

    pub fn ldr(&mut self, rt: Reg, rn: Reg, offset: i32) {
        self.mem(Cond::Al, true, false, rt, rn, offset);
    }

    pub fn str(&mut self, rt: Reg, rn: Reg, offset: i32) {
        self.mem(Cond::Al, false, false, rt, rn, offset);
    }

    pub fn ldrb(&mut self, rt: Reg, rn: Reg, offset: i32) {
        self.mem(Cond::Al, true, true, rt, rn, offset);
    }

    pub fn strb(&mut self, rt: Reg, rn: Reg, offset: i32) {
        self.mem(Cond::Al, false, true, rt, rn, offset);
    }

    pub fn ldr_cond(&mut self, cond: Cond, rt: Reg, rn: Reg, offset: i32) {
        self.mem(cond, true, false, rt, rn, offset);
    }

    pub fn str_cond(&mut self, cond: Cond, rt: Reg, rn: Reg, offset: i32) {
        self.mem(cond, false, false, rt, rn, offset);
    }

    /// LDR/STR with a register offset: `[rn, rm]`.
    fn mem_reg(&mut self, load: bool, byte: bool, rt: Reg, rn: Reg, rm: Reg) {
        self.written |= u32::from(load) << rt;
        self.emit(0xe780_0000 | u32::from(byte) << 22 | u32::from(load) << 20 | rn << 16 | rt << 12 | rm);
    }

    pub fn ldr_reg(&mut self, rt: Reg, rn: Reg, rm: Reg) {
        self.mem_reg(true, false, rt, rn, rm);
    }

    pub fn str_reg(&mut self, rt: Reg, rn: Reg, rm: Reg) {
        self.mem_reg(false, false, rt, rn, rm);
    }

    pub fn ldrb_reg(&mut self, rt: Reg, rn: Reg, rm: Reg) {
        self.mem_reg(true, true, rt, rn, rm);
    }

    pub fn strb_reg(&mut self, rt: Reg, rn: Reg, rm: Reg) {
        self.mem_reg(false, true, rt, rn, rm);
    }

    /// `ldrb rt, [rn, rm, lsr #shift]`.
    pub fn ldrb_lsr(&mut self, rt: Reg, rn: Reg, rm: Reg, shift: u32) {
        self.written |= 1 << rt;
        self.emit(0xe7d0_0020 | rn << 16 | rt << 12 | shift << 7 | rm);
    }

    /// The halfword and signed loads: `[rn, rm]`, sh = 1 (H), 2 (SB), 3 (SH).
    fn mem_half_reg(&mut self, load: bool, sh: u32, rt: Reg, rn: Reg, rm: Reg) {
        self.written |= u32::from(load) << rt;
        self.emit(0xe180_0090 | u32::from(load) << 20 | rn << 16 | rt << 12 | sh << 5 | rm);
    }

    pub fn ldrh_reg(&mut self, rt: Reg, rn: Reg, rm: Reg) {
        self.mem_half_reg(true, 1, rt, rn, rm);
    }

    pub fn strh_reg(&mut self, rt: Reg, rn: Reg, rm: Reg) {
        self.mem_half_reg(false, 1, rt, rn, rm);
    }

    pub fn ldrsb_reg(&mut self, rt: Reg, rn: Reg, rm: Reg) {
        self.mem_half_reg(true, 2, rt, rn, rm);
    }

    pub fn ldrsh_reg(&mut self, rt: Reg, rn: Reg, rm: Reg) {
        self.mem_half_reg(true, 3, rt, rn, rm);
    }

    pub fn mul(&mut self, rd: Reg, rn: Reg, rm: Reg) {
        self.written |= 1 << rd;
        self.emit(0xe000_0090 | rd << 16 | rm << 8 | rn);
    }

    /// SMULL/UMULL: `lo, hi = rn * rm`.
    pub fn mull(&mut self, signed: bool, lo: Reg, hi: Reg, rn: Reg, rm: Reg) {
        self.written |= 1 << lo | 1 << hi;
        self.emit(0xe080_0090 | u32::from(signed) << 22 | hi << 16 | lo << 12 | rm << 8 | rn);
    }

    pub fn sdiv(&mut self, rd: Reg, rn: Reg, rm: Reg) {
        self.written |= 1 << rd;
        self.emit(0xe710_f010 | rd << 16 | rm << 8 | rn);
    }

    pub fn udiv(&mut self, rd: Reg, rn: Reg, rm: Reg) {
        self.written |= 1 << rd;
        self.emit(0xe730_f010 | rd << 16 | rm << 8 | rn);
    }

    pub fn bx(&mut self, rm: Reg) {
        self.emit(0xe12f_ff10 | rm);
    }

    /// A call, which may change r0-r3, r12 and lr.
    pub fn blx(&mut self, rm: Reg) {
        self.written |= 0x500f;
        self.emit(0xe12f_ff30 | rm);
    }

    pub fn push(&mut self, regs: u32) {
        self.emit(0xe92d_0000 | regs);
    }

    pub fn pop(&mut self, regs: u32) {
        self.written |= regs;
        self.emit(0xe8bd_0000 | regs);
    }

    /// A branch to be patched: its position.
    pub fn b(&mut self, cond: Cond) -> usize {
        self.emit((cond as u32) << 28 | 0x0a00_0000);
        self.here() - 1
    }

    /// Points a branch at the next instruction.
    pub fn patch_here(&mut self, branch: usize) {
        let target = self.here();
        self.patch(branch, target);
    }

    pub fn patch(&mut self, branch: usize, target: usize) {
        let offset = (target as i32 - branch as i32 - 2) as u32 & 0xff_ffff;
        self.code[branch] = self.code[branch] & 0xff00_0000 | offset;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each against what llvm-mc encodes the same instruction as.
    #[test]
    fn encodings() {
        let mut e = Emitter::default();
        e.alu(Op::Add, R0, R1, R2);
        e.ldr(R0, 11, 256);
        e.movw(Cond::Al, R1, 0x1234);
        e.movt(Cond::Al, R1, 0x5678);
        e.udiv(R0, R1, R2);
        e.mull(true, R0, R1, R2, R3);
        e.bx(LR);
        e.push(0x4ff0);
        e.alu_imm(Cond::Al, Op::Cmp, true, 0, R0, 255);
        e.alu_imm(Cond::Lt, Op::Mov, false, R0, 0, 1);
        e.alu_shifted(Cond::Al, Op::Mov, false, R0, 0, R1, Shift::LslReg(R2));
        e.alu_shifted(Cond::Al, Op::Mov, false, R0, 0, R1, Shift::Asr(31));
        e.blx(R12);
        e.strb_reg(R0, R1, R2);
        e.ldrsb_reg(R0, R1, R2);
        e.ldrh_reg(R0, R1, R2);
        e.ldr(R0, R1, -4);
        e.ldrb_lsr(R3, 7, R1, 12);
        let expected = [
            0xe081_0002, 0xe59b_0100, 0xe301_1234, 0xe345_1678, 0xe730_f211, 0xe0c1_0392, 0xe12f_ff1e, 0xe92d_4ff0,
            0xe350_00ff, 0xb3a0_0001, 0xe1a0_0211, 0xe1a0_0fc1, 0xe12f_ff3c, 0xe7c1_0002, 0xe191_00d2, 0xe191_00b2,
            0xe511_0004, 0xe7d7_3621,
        ];
        assert_eq!(e.code, expected);
    }

    #[test]
    fn immediates() {
        assert_eq!(encode_imm(0xff), Some(0xff));
        assert_eq!(encode_imm(0x3fc), Some(0xfff));
        assert_eq!(encode_imm(0xff00_0000), Some(0x4ff));
        assert_eq!(encode_imm(0x101), None);
    }
}
