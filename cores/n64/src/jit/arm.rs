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
    /// By 1 to 31 (0 would be RRX).
    Ror(u32),
    LslReg(Reg),
    LsrReg(Reg),
    AsrReg(Reg),
}

/// VFP's arithmetic, by its encoding's fixed bits (single precision; the
/// double forms add bit 8).
#[derive(Clone, Copy)]
#[repr(u32)]
pub enum Vfp {
    Add = 0xee30_0a00,
    Sub = 0xee30_0a40,
    Mul = 0xee20_0a00,
    Div = 0xee80_0a00,
}

/// A VFP register's number in an encoding's 4-bit field and its extra bit:
/// a single's low bit is the extra one, a double's high bit.
fn single(s: u32) -> (u32, u32) {
    (s >> 1, s & 1)
}

fn double(d: u32) -> (u32, u32) {
    (d & 15, d >> 4)
}

#[derive(Default)]
pub struct Emitter {
    pub code: Vec<u32>,
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
        let s = u32::from(set || matches!(op, Op::Tst | Op::Cmp | Op::Cmn));
        self.emit((cond as u32) << 28 | (op as u32) << 21 | s << 20 | rn << 16 | rd << 12 | operand);
    }

    /// `op rd, rn, rm` with rm shifted.
    #[allow(clippy::too_many_arguments)]
    pub fn alu_shifted(&mut self, cond: Cond, op: Op, set: bool, rd: Reg, rn: Reg, rm: Reg, shift: Shift) {
        let operand = match shift {
            Shift::Lsl(n) => (n & 31) << 7 | rm,
            Shift::Lsr(n) => (n & 31) << 7 | 1 << 5 | rm,
            Shift::Asr(n) => (n & 31) << 7 | 2 << 5 | rm,
            Shift::Ror(n) => {
                assert!((1..32).contains(&n), "ROR #0 is RRX");
                n << 7 | 3 << 5 | rm
            }
            Shift::LslReg(rs) => rs << 8 | 1 << 4 | rm,
            Shift::LsrReg(rs) => rs << 8 | 3 << 4 | rm,
            Shift::AsrReg(rs) => rs << 8 | 5 << 4 | rm,
        };
        self.dp(cond, op, set, rn, rd, operand);
    }

    /// `op rd, rn, rm`.
    pub fn alu(&mut self, op: Op, rd: Reg, rn: Reg, rm: Reg) {
        self.alu_cond(Cond::Al, op, rd, rn, rm);
    }

    /// `op{cond} rd, rn, rm`.
    pub fn alu_cond(&mut self, cond: Cond, op: Op, rd: Reg, rn: Reg, rm: Reg) {
        self.alu_shifted(cond, op, false, rd, rn, rm, Shift::Lsl(0));
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
        self.emit((cond as u32) << 28 | 0x0300_0000 | (imm >> 12 & 15) << 16 | rd << 12 | imm & 0xfff);
    }

    pub fn movt(&mut self, cond: Cond, rd: Reg, imm: u32) {
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
        self.emit(0xe7d0_0020 | rn << 16 | rt << 12 | shift << 7 | rm);
    }

    /// LDRD: rt (even) and the next register from `[rn, #offset]`, an
    /// offset below 256.
    pub fn ldrd(&mut self, rt: Reg, rn: Reg, offset: u32) {
        assert!(rt % 2 == 0 && rt < 14 && offset < 256);
        self.emit(0xe1c0_00d0 | rn << 16 | rt << 12 | (offset >> 4) << 8 | offset & 15);
    }

    /// STRD: rt (even) and the next register to `[rn, #offset]`, an offset
    /// below 256.
    pub fn strd(&mut self, rt: Reg, rn: Reg, offset: u32) {
        assert!(rt % 2 == 0 && rt < 14 && offset < 256);
        self.emit(0xe1c0_00f0 | rn << 16 | rt << 12 | (offset >> 4) << 8 | offset & 15);
    }

    /// STMIA without write-back: the registers to consecutive words from
    /// `[rn]`, the lowest first.
    pub fn stm(&mut self, rn: Reg, regs: u32) {
        self.emit(0xe880_0000 | rn << 16 | regs);
    }

    /// LDR with a scaled register offset: `[rn, rm, lsl #shift]`.
    pub fn ldr_lsl(&mut self, rt: Reg, rn: Reg, rm: Reg, shift: u32) {
        self.emit(0xe790_0000 | rn << 16 | rt << 12 | shift << 7 | rm);
    }

    /// The halfword and signed loads: `[rn, rm]`, sh = 1 (H), 2 (SB), 3 (SH).
    fn mem_half_reg(&mut self, load: bool, sh: u32, rt: Reg, rn: Reg, rm: Reg) {
        self.emit(0xe180_0090 | u32::from(load) << 20 | rn << 16 | rt << 12 | sh << 5 | rm);
    }

    pub fn ldrh_reg(&mut self, rt: Reg, rn: Reg, rm: Reg) {
        self.mem_half_reg(true, 1, rt, rn, rm);
    }

    pub fn strh_reg(&mut self, rt: Reg, rn: Reg, rm: Reg) {
        self.mem_half_reg(false, 1, rt, rn, rm);
    }

    /// Reverse the four bytes of a word, or the bytes of each halfword.
    pub fn rev(&mut self, rd: Reg, rm: Reg) {
        self.emit(0xe6bf_0f30 | rd << 12 | rm);
    }

    pub fn rev16(&mut self, rd: Reg, rm: Reg) {
        self.emit(0xe6bf_0fb0 | rd << 12 | rm);
    }

    pub fn ldrsb_reg(&mut self, rt: Reg, rn: Reg, rm: Reg) {
        self.mem_half_reg(true, 2, rt, rn, rm);
    }

    pub fn ldrsh_reg(&mut self, rt: Reg, rn: Reg, rm: Reg) {
        self.mem_half_reg(true, 3, rt, rn, rm);
    }

    /// LDRH/STRH/LDRSH with an immediate offset of up to 255 either way.
    fn mem_half(&mut self, load: bool, sh: u32, rt: Reg, rn: Reg, offset: i32) {
        let (up, magnitude) = if offset < 0 { (0, (-offset) as u32) } else { (1, offset as u32) };
        assert!(magnitude < 256, "offset out of reach");
        self.emit(0xe140_0090 | up << 23 | u32::from(load) << 20 | rn << 16 | rt << 12 | (magnitude >> 4) << 8 | sh << 5
            | magnitude & 15);
    }

    pub fn ldrh(&mut self, rt: Reg, rn: Reg, offset: i32) {
        self.mem_half(true, 1, rt, rn, offset);
    }

    pub fn strh(&mut self, rt: Reg, rn: Reg, offset: i32) {
        self.mem_half(false, 1, rt, rn, offset);
    }

    pub fn ldrsh(&mut self, rt: Reg, rn: Reg, offset: i32) {
        self.mem_half(true, 3, rt, rn, offset);
    }

    /// `ubfx rd, rn, #lsb, #width`: an unsigned bit field, from bit lsb.
    pub fn ubfx(&mut self, rd: Reg, rn: Reg, lsb: u32, width: u32) {
        self.emit(0xe7e0_0050 | (width - 1) << 16 | rd << 12 | lsb << 7 | rn);
    }

    /// The low halfword or byte, sign- or zero-extended.
    pub fn sxth(&mut self, rd: Reg, rm: Reg) {
        self.emit(0xe6bf_0070 | rd << 12 | rm);
    }

    pub fn sxtb(&mut self, rd: Reg, rm: Reg) {
        self.emit(0xe6af_0070 | rd << 12 | rm);
    }

    pub fn uxth(&mut self, rd: Reg, rm: Reg) {
        self.emit(0xe6ff_0070 | rd << 12 | rm);
    }

    pub fn uxtb(&mut self, rd: Reg, rm: Reg) {
        self.emit(0xe6ef_0070 | rd << 12 | rm);
    }

    pub fn mul(&mut self, rd: Reg, rn: Reg, rm: Reg) {
        self.emit(0xe000_0090 | rd << 16 | rm << 8 | rn);
    }

    /// SMULL/UMULL: `lo, hi = rn * rm`.
    pub fn mull(&mut self, signed: bool, lo: Reg, hi: Reg, rn: Reg, rm: Reg) {
        self.emit(0xe080_0090 | u32::from(signed) << 22 | hi << 16 | lo << 12 | rm << 8 | rn);
    }

    pub fn sdiv(&mut self, rd: Reg, rn: Reg, rm: Reg) {
        self.emit(0xe710_f010 | rd << 16 | rm << 8 | rn);
    }

    pub fn udiv(&mut self, rd: Reg, rn: Reg, rm: Reg) {
        self.emit(0xe730_f010 | rd << 16 | rm << 8 | rn);
    }

    pub fn bx(&mut self, rm: Reg) {
        self.emit(0xe12f_ff10 | rm);
    }

    pub fn blx(&mut self, rm: Reg) {
        self.emit(0xe12f_ff30 | rm);
    }

    pub fn push(&mut self, regs: u32) {
        self.emit(0xe92d_0000 | regs);
    }

    pub fn pop(&mut self, regs: u32) {
        self.emit(0xe8bd_0000 | regs);
    }

    /// VMOV Sn, Rt.
    pub fn vmov_to_single(&mut self, s: u32, rt: Reg) {
        let (n, extra) = single(s);
        self.emit(0xee00_0a10 | n << 16 | rt << 12 | extra << 7);
    }

    /// VMOV Rt, Sn.
    pub fn vmov_from_single(&mut self, rt: Reg, s: u32) {
        let (n, extra) = single(s);
        self.emit(0xee10_0a10 | n << 16 | rt << 12 | extra << 7);
    }

    /// Sd = Sn op Sm.
    pub fn vfp_single(&mut self, op: Vfp, d: u32, n: u32, m: u32) {
        let ((d, dx), (n, nx), (m, mx)) = (single(d), single(n), single(m));
        self.emit(op as u32 | dx << 22 | n << 16 | d << 12 | nx << 7 | mx << 5 | m);
    }

    /// Dd = Dn op Dm.
    pub fn vfp_double(&mut self, op: Vfp, d: u32, n: u32, m: u32) {
        let ((d, dx), (n, nx), (m, mx)) = (double(d), double(n), double(m));
        self.emit(op as u32 | 0x100 | dx << 22 | n << 16 | d << 12 | nx << 7 | mx << 5 | m);
    }

    /// VCVT.F64.F32 Dd, Sm: exact.
    pub fn vcvt_double_single(&mut self, d: u32, s: u32) {
        let ((d, dx), (m, mx)) = (double(d), single(s));
        self.emit(0xeeb7_0ac0 | dx << 22 | d << 12 | mx << 5 | m);
    }

    /// VCVT.S32.F32 Sd, Sm: toward zero, or else by FPSCR's mode.
    pub fn vcvt_int_single(&mut self, toward_zero: bool, d: u32, s: u32) {
        let ((d, dx), (m, mx)) = (single(d), single(s));
        self.emit(0xeebd_0a40 | u32::from(toward_zero) << 7 | dx << 22 | d << 12 | mx << 5 | m);
    }

    /// VCVT.S32.F64 Sd, Dm: toward zero, or else by FPSCR's mode.
    pub fn vcvt_int_double(&mut self, toward_zero: bool, d: u32, m: u32) {
        let ((d, dx), (m, mx)) = (single(d), double(m));
        self.emit(0xeebd_0b40 | u32::from(toward_zero) << 7 | dx << 22 | d << 12 | mx << 5 | m);
    }

    /// VCVT.F32.S32 Sd, Sm, by FPSCR's mode.
    pub fn vcvt_single_int(&mut self, d: u32, s: u32) {
        let ((d, dx), (m, mx)) = (single(d), single(s));
        self.emit(0xeeb8_0ac0 | dx << 22 | d << 12 | mx << 5 | m);
    }

    /// VMOV.F64 Dd, Dm.
    pub fn vmov_double(&mut self, d: u32, m: u32) {
        let ((d, dx), (m, mx)) = (double(d), double(m));
        self.emit(0xeeb0_0b40 | dx << 22 | d << 12 | mx << 5 | m);
    }

    /// VSQRT.F32 Sd, Sm.
    pub fn vsqrt_single(&mut self, d: u32, m: u32) {
        let ((d, dx), (m, mx)) = (single(d), single(m));
        self.emit(0xeeb1_0ac0 | dx << 22 | d << 12 | mx << 5 | m);
    }

    /// VSQRT.F64 Dd, Dm.
    pub fn vsqrt_double(&mut self, d: u32, m: u32) {
        let ((d, dx), (m, mx)) = (double(d), double(m));
        self.emit(0xeeb1_0bc0 | dx << 22 | d << 12 | mx << 5 | m);
    }

    /// VFMS.F64 Dd, Dn, Dm: Dd - Dn * Dm, rounded once.
    pub fn vfms_double(&mut self, d: u32, n: u32, m: u32) {
        let ((d, dx), (n, nx), (m, mx)) = (double(d), double(n), double(m));
        self.emit(0xeea0_0b40 | dx << 22 | n << 16 | d << 12 | nx << 7 | mx << 5 | m);
    }

    /// VCVT.F32.F64 Sd, Dm, by FPSCR's mode.
    pub fn vcvt_single_double(&mut self, s: u32, d: u32) {
        let ((d, dx), (m, mx)) = (single(s), double(d));
        self.emit(0xeeb7_0bc0 | dx << 22 | d << 12 | mx << 5 | m);
    }

    /// VCVT.F64.S32 Dd, Sm: exact.
    pub fn vcvt_double_int(&mut self, d: u32, s: u32) {
        let ((d, dx), (m, mx)) = (double(d), single(s));
        self.emit(0xeeb8_0bc0 | dx << 22 | d << 12 | mx << 5 | m);
    }

    /// VCMP.F32 Sd, Sm.
    pub fn vcmp_single(&mut self, d: u32, m: u32) {
        let ((d, dx), (m, mx)) = (single(d), single(m));
        self.emit(0xeeb4_0a40 | dx << 22 | d << 12 | mx << 5 | m);
    }

    /// VCMP.F64 Dd, Dm, and against zero: FPSCR's flags, for `vmrs_flags`.
    pub fn vcmp_double(&mut self, d: u32, m: u32) {
        let ((d, dx), (m, mx)) = (double(d), double(m));
        self.emit(0xeeb4_0b40 | dx << 22 | d << 12 | mx << 5 | m);
    }

    pub fn vcmp_double_zero(&mut self, d: u32) {
        let (d, dx) = double(d);
        self.emit(0xeeb5_0b40 | dx << 22 | d << 12);
    }

    /// VMRS APSR_nzcv, FPSCR: the last compare's flags to the CPU's.
    pub fn vmrs_flags(&mut self) {
        self.emit(0xeef1_fa10);
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
        e.rev(R0, R1);
        e.rev16(R2, R3);
        e.alu_shifted(Cond::Al, Op::Mov, false, R1, 0, R2, Shift::Ror(2));
        e.ldrd(R0, 11, 0);
        e.ldrd(R2, 4, 0x48);
        let expected = [
            0xe081_0002, 0xe59b_0100, 0xe301_1234, 0xe345_1678, 0xe730_f211, 0xe0c1_0392, 0xe12f_ff1e, 0xe92d_4ff0,
            0xe350_00ff, 0xb3a0_0001, 0xe1a0_0211, 0xe1a0_0fc1, 0xe12f_ff3c, 0xe7c1_0002, 0xe191_00d2, 0xe191_00b2,
            0xe511_0004, 0xe7d7_3621, 0xe6bf_0f31, 0xe6bf_2fb3, 0xe1a0_1162, 0xe1cb_00d0,
            0xe1c4_24d8,
        ];
        assert_eq!(e.code, expected);
    }

    /// The VFP instructions and the scaled load, against llvm-mc too.
    #[test]
    fn vfp_encodings() {
        let mut e = Emitter::default();
        e.vmov_to_single(0, R0);
        e.vmov_to_single(3, R12);
        e.vmov_from_single(R2, 2);
        e.vmov_from_single(R1, 31);
        e.vfp_single(Vfp::Add, 2, 0, 1);
        e.vfp_single(Vfp::Sub, 5, 3, 30);
        e.vfp_single(Vfp::Mul, 2, 0, 1);
        e.vfp_single(Vfp::Div, 2, 0, 1);
        e.vfp_double(Vfp::Add, 4, 2, 3);
        e.vfp_double(Vfp::Sub, 6, 17, 5);
        e.vfp_double(Vfp::Mul, 4, 2, 3);
        e.vfp_double(Vfp::Div, 4, 2, 3);
        e.vcvt_double_single(2, 0);
        e.vcvt_double_single(17, 31);
        e.vcmp_double(5, 4);
        e.vcmp_double_zero(6);
        e.vmrs_flags();
        e.ldr_lsl(R3, 8, R3, 2);
        e.vcvt_int_single(true, 1, 0);
        e.vcvt_int_single(false, 1, 0);
        e.vcvt_single_int(2, 1);
        e.vcmp_single(2, 0);
        e.vcvt_int_single(true, 31, 30);
        e.vcmp_single(3, 29);
        e.vcvt_double_int(1, 0);
        e.vcvt_double_int(17, 31);
        e.vfms_double(6, 0, 1);
        e.vfms_double(17, 18, 31);
        e.vcvt_single_double(1, 2);
        e.vmov_double(6, 2);
        e.vmov_double(17, 31);
        e.vsqrt_single(2, 0);
        e.vsqrt_single(3, 31);
        e.vsqrt_double(2, 0);
        e.vsqrt_double(17, 31);
        e.vcvt_int_double(true, 4, 0);
        e.vcvt_int_double(false, 4, 0);
        e.vcvt_int_double(true, 3, 17);
        e.vcvt_int_double(false, 3, 17);
        let expected = [
            0xee00_0a10, 0xee01_ca90, 0xee11_2a10, 0xee1f_1a90, 0xee30_1a20, 0xee71_2acf, 0xee20_1a20, 0xee80_1a20,
            0xee32_4b03, 0xee31_6bc5, 0xee22_4b03, 0xee82_4b03, 0xeeb7_2ac0, 0xeef7_1aef, 0xeeb4_5b44, 0xeeb5_6b40,
            0xeef1_fa10, 0xe798_3103, 0xeefd_0ac0, 0xeefd_0a40, 0xeeb8_1ae0, 0xeeb4_1a40, 0xeefd_facf, 0xeef4_1a6e,
            0xeeb8_1bc0, 0xeef8_1bef, 0xeea0_6b41, 0xeee2_1bef, 0xeef7_0bc2, 0xeeb0_6b42, 0xeef0_1b6f, 0xeeb1_1ac0,
            0xeef1_1aef, 0xeeb1_2bc0, 0xeef1_1bef, 0xeebd_2bc0, 0xeebd_2b40, 0xeefd_1be1, 0xeefd_1b61,
        ];
        assert_eq!(e.code, expected);
    }

    #[test]
    fn fields_halves_and_extensions() {
        let mut e = Emitter::default();
        e.ubfx(R0, R1, 2, 10);
        e.sxth(R2, R3);
        e.sxtb(R2, R3);
        e.uxth(R1, R0);
        e.uxtb(R1, R0);
        e.ldrh(R0, R1, 6);
        e.strh(R0, R1, 6);
        e.ldrsh(R0, R1, 6);
        e.ldrh(R0, R1, -0x12);
        let expected =
            [0xe7e9_0151, 0xe6bf_2073, 0xe6af_2073, 0xe6ff_1070, 0xe6ef_1070, 0xe1d1_00b6, 0xe1c1_00b6, 0xe1d1_00f6, 0xe151_01b2];
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
