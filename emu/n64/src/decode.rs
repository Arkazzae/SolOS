//! Decoding VR4300 instructions, apart from running them: an opcode word
//! becomes what it does (`Op`) and its fields. The interpreter (cpu.rs,
//! fpu.rs) runs decoded instructions; a recompiler can translate the same
//! decoded instructions instead.

/// What an instruction does. Instructions the VR4300 does not have are
/// `Reserved` (the reserved instruction exception); those of a coprocessor
/// it does not have are `Cop3` and, but for COP2's moves, loads and stores
/// (which reach a latch), `Cop2`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Op {
    // SPECIAL: shifts, jumps through registers, HI/LO, arithmetic, traps.
    Sll,
    Srl,
    Sra,
    Sllv,
    Srlv,
    Srav,
    Jr,
    Jalr,
    Syscall,
    Break,
    Sync,
    Mfhi,
    Mthi,
    Mflo,
    Mtlo,
    Dsllv,
    Dsrlv,
    Dsrav,
    Mult,
    Multu,
    Div,
    Divu,
    Dmult,
    Dmultu,
    Ddiv,
    Ddivu,
    Add,
    Addu,
    Sub,
    Subu,
    And,
    Or,
    Xor,
    Nor,
    Slt,
    Sltu,
    Dadd,
    Daddu,
    Dsub,
    Dsubu,
    Tge,
    Tgeu,
    Tlt,
    Tltu,
    Teq,
    Tne,
    Dsll,
    Dsrl,
    Dsra,
    Dsll32,
    Dsrl32,
    Dsra32,
    // REGIMM: branches on the sign of a register, and traps on immediates.
    Bltz,
    Bgez,
    Bltzl,
    Bgezl,
    Tgei,
    Tgeiu,
    Tlti,
    Tltiu,
    Teqi,
    Tnei,
    Bltzal,
    Bgezal,
    Bltzall,
    Bgezall,
    // Jumps, branches and immediates.
    J,
    Jal,
    Beq,
    Bne,
    Blez,
    Bgtz,
    Addi,
    Addiu,
    Slti,
    Sltiu,
    Andi,
    Ori,
    Xori,
    Lui,
    Beql,
    Bnel,
    Blezl,
    Bgtzl,
    Daddi,
    Daddiu,
    // Loads and stores.
    Ldl,
    Ldr,
    Lb,
    Lh,
    Lwl,
    Lw,
    Lbu,
    Lhu,
    Lwr,
    Lwu,
    Sb,
    Sh,
    Swl,
    Sw,
    Sdl,
    Sdr,
    Swr,
    Cache,
    Ll,
    Lwc1,
    Lld,
    Ldc1,
    Ld,
    Sc,
    Swc1,
    Scd,
    Sdc1,
    Sd,
    // COP0: the system control coprocessor.
    Mfc0,
    Dmfc0,
    Mtc0,
    Dmtc0,
    Tlbr,
    Tlbwi,
    Tlbwr,
    Tlbp,
    Eret,
    /// A COP0 operation with function 0x20-0x3F: the VR4300 does nothing
    /// for it (n64-systemtest probes it for an emulator's extensions before
    /// it has an exception handler).
    Cop0Inert,
    // COP1, the FPU: moves, branches, and the operations, by format (S, D,
    // W, L) where the format matters to decoding.
    Mfc1,
    Dmfc1,
    Cfc1,
    Mtc1,
    Dmtc1,
    Ctc1,
    Bc1f,
    Bc1t,
    Bc1fl,
    Bc1tl,
    /// An arithmetic operation on S or D (fmt in the rs field, the
    /// operation in the low six bits).
    FpuOp,
    /// A conversion from W or L (fmt 20 or 21).
    FpuCvtInt,
    /// What COP1 does not decode: the unimplemented operation exception.
    FpuUnimplemented,
    // COP2, which is not there but for one 64-bit latch: its moves, loads
    // and stores, and the rest of its encodings.
    Mfc2,
    Dmfc2,
    Cfc2,
    Mtc2,
    Dmtc2,
    Ctc2,
    Lwc2,
    Ldc2,
    Swc2,
    Sdc2,
    Cop2,
    Cop3,
    Reserved,
}

/// A decoded instruction: what it does, and the word with its fields.
#[derive(Clone, Copy, Debug)]
pub struct Instr {
    pub op: Op,
    pub word: u32,
}

impl Instr {
    pub fn rs(self) -> usize {
        (self.word >> 21 & 31) as usize
    }

    pub fn rt(self) -> usize {
        (self.word >> 16 & 31) as usize
    }

    pub fn rd(self) -> usize {
        (self.word >> 11 & 31) as usize
    }

    pub fn sa(self) -> u32 {
        self.word >> 6 & 31
    }

    /// The immediate, sign-extended.
    pub fn simm(self) -> u64 {
        self.word as i16 as i64 as u64
    }

    /// The immediate, zero-extended.
    pub fn uimm(self) -> u64 {
        u64::from(self.word & 0xffff)
    }

    /// A jump's 26-bit target.
    pub fn target(self) -> u64 {
        u64::from(self.word & 0x03ff_ffff) << 2
    }

    // The FPU's fields: format, its registers, and the function.
    pub fn fmt(self) -> u32 {
        self.word >> 21 & 31
    }

    pub fn ft(self) -> usize {
        self.rt()
    }

    pub fn fs(self) -> usize {
        self.rd()
    }

    pub fn fd(self) -> usize {
        self.sa() as usize
    }

    pub fn funct(self) -> u32 {
        self.word & 63
    }
}

pub fn decode(word: u32) -> Instr {
    Instr { op: op(word), word }
}

fn op(word: u32) -> Op {
    use Op::*;
    match word >> 26 {
        0x00 => special(word),
        0x01 => match word >> 16 & 31 {
            0x00 => Bltz,
            0x01 => Bgez,
            0x02 => Bltzl,
            0x03 => Bgezl,
            0x08 => Tgei,
            0x09 => Tgeiu,
            0x0a => Tlti,
            0x0b => Tltiu,
            0x0c => Teqi,
            0x0e => Tnei,
            0x10 => Bltzal,
            0x11 => Bgezal,
            0x12 => Bltzall,
            0x13 => Bgezall,
            _ => Reserved,
        },
        0x02 => J,
        0x03 => Jal,
        0x04 => Beq,
        0x05 => Bne,
        0x06 => Blez,
        0x07 => Bgtz,
        0x08 => Addi,
        0x09 => Addiu,
        0x0a => Slti,
        0x0b => Sltiu,
        0x0c => Andi,
        0x0d => Ori,
        0x0e => Xori,
        0x0f => Lui,
        0x10 => cop0(word),
        0x11 => cop1(word),
        0x12 => match word >> 21 & 31 {
            0x00 => Mfc2,
            0x01 => Dmfc2,
            0x02 => Cfc2,
            0x04 => Mtc2,
            0x05 => Dmtc2,
            0x06 => Ctc2,
            _ => Cop2,
        },
        0x13 => Cop3,
        0x14 => Beql,
        0x15 => Bnel,
        0x16 => Blezl,
        0x17 => Bgtzl,
        0x18 => Daddi,
        0x19 => Daddiu,
        0x1a => Ldl,
        0x1b => Ldr,
        0x20 => Lb,
        0x21 => Lh,
        0x22 => Lwl,
        0x23 => Lw,
        0x24 => Lbu,
        0x25 => Lhu,
        0x26 => Lwr,
        0x27 => Lwu,
        0x28 => Sb,
        0x29 => Sh,
        0x2a => Swl,
        0x2b => Sw,
        0x2c => Sdl,
        0x2d => Sdr,
        0x2e => Swr,
        0x2f => Cache,
        0x30 => Ll,
        0x31 => Lwc1,
        0x32 => Lwc2,
        0x34 => Lld,
        0x35 => Ldc1,
        0x36 => Ldc2,
        0x37 => Ld,
        0x38 => Sc,
        0x39 => Swc1,
        0x3a => Swc2,
        0x3c => Scd,
        0x3d => Sdc1,
        0x3e => Sdc2,
        0x3f => Sd,
        _ => Reserved,
    }
}

fn special(word: u32) -> Op {
    use Op::*;
    match word & 63 {
        0x00 => Sll,
        0x02 => Srl,
        0x03 => Sra,
        0x04 => Sllv,
        0x06 => Srlv,
        0x07 => Srav,
        0x08 => Jr,
        0x09 => Jalr,
        0x0c => Syscall,
        0x0d => Break,
        0x0f => Sync,
        0x10 => Mfhi,
        0x11 => Mthi,
        0x12 => Mflo,
        0x13 => Mtlo,
        0x14 => Dsllv,
        0x16 => Dsrlv,
        0x17 => Dsrav,
        0x18 => Mult,
        0x19 => Multu,
        0x1a => Div,
        0x1b => Divu,
        0x1c => Dmult,
        0x1d => Dmultu,
        0x1e => Ddiv,
        0x1f => Ddivu,
        0x20 => Add,
        0x21 => Addu,
        0x22 => Sub,
        0x23 => Subu,
        0x24 => And,
        0x25 => Or,
        0x26 => Xor,
        0x27 => Nor,
        0x2a => Slt,
        0x2b => Sltu,
        0x2c => Dadd,
        0x2d => Daddu,
        0x2e => Dsub,
        0x2f => Dsubu,
        0x30 => Tge,
        0x31 => Tgeu,
        0x32 => Tlt,
        0x33 => Tltu,
        0x34 => Teq,
        0x36 => Tne,
        0x38 => Dsll,
        0x3a => Dsrl,
        0x3b => Dsra,
        0x3c => Dsll32,
        0x3e => Dsrl32,
        0x3f => Dsra32,
        _ => Reserved,
    }
}

fn cop0(word: u32) -> Op {
    use Op::*;
    match word >> 21 & 31 {
        0x00 => Mfc0,
        0x01 => Dmfc0,
        0x04 => Mtc0,
        0x05 => Dmtc0,
        0x10..=0x1f => match word & 63 {
            0x01 => Tlbr,
            0x02 => Tlbwi,
            0x06 => Tlbwr,
            0x08 => Tlbp,
            0x18 => Eret,
            0x20..=0x3f => Cop0Inert,
            _ => Reserved,
        },
        _ => Reserved,
    }
}

fn cop1(word: u32) -> Op {
    use Op::*;
    match word >> 21 & 31 {
        0x00 => Mfc1,
        0x01 => Dmfc1,
        0x02 => Cfc1,
        0x04 => Mtc1,
        0x05 => Dmtc1,
        0x06 => Ctc1,
        0x08 => match word >> 16 & 3 {
            0 => Bc1f,
            1 => Bc1t,
            2 => Bc1fl,
            _ => Bc1tl,
        },
        0x10 | 0x11 => FpuOp,
        0x14 | 0x15 => FpuCvtInt,
        _ => FpuUnimplemented,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields() {
        // addiu sp, sp, -24
        let i = decode(0x27bd_ffe8);
        assert_eq!(i.op, Op::Addiu);
        assert_eq!((i.rs(), i.rt(), i.simm() as i64), (29, 29, -24));
        // jal 0x80246050
        let i = decode(0x0c09_1814);
        assert_eq!((i.op, i.target()), (Op::Jal, 0x0024_6050));
        // dsll32 v0, v1, 4
        let i = decode(0x0003_113c);
        assert_eq!((i.op, i.rt(), i.rd(), i.sa()), (Op::Dsll32, 3, 2, 4));
        // mtc0 t1, Status; eret; add.s f0, f1, f2
        assert_eq!(decode(0x4089_6000).op, Op::Mtc0);
        assert_eq!(decode(0x4200_0018).op, Op::Eret);
        // COP0 functions 0x20-0x3F, in either CO encoding; 0x1F stays reserved.
        assert_eq!(decode(0x4200_0020).op, Op::Cop0Inert);
        assert_eq!(decode(0x43ff_ffff).op, Op::Cop0Inert);
        assert_eq!(decode(0x4200_001f).op, Op::Reserved);
        let i = decode(0x4602_0800);
        assert_eq!((i.op, i.fmt(), i.fs(), i.ft(), i.fd(), i.funct()), (Op::FpuOp, 16, 1, 2, 0, 0));
        assert_eq!(decode(0xfc00_0000).op, Op::Sd);
        // COP2: MFC2 … CTC2 by rs, DCFC2 (3), DCTC2 (7) and BC2 (8) not;
        // LWC2, LDC2, SWC2, SDC2.
        let cop2 = [0x4802_2800, 0x4822_2800, 0x4842_2800, 0x4882_2800, 0x48a2_2800, 0x48c2_2800, 0x4862_2800,
            0x48e2_2800, 0x4900_0000, 0x4a00_0000, 0xc822_0000, 0xd822_0000, 0xe822_0000, 0xf822_0000];
        let ops = cop2.map(|word| decode(word).op);
        assert_eq!(ops, [Op::Mfc2, Op::Dmfc2, Op::Cfc2, Op::Mtc2, Op::Dmtc2, Op::Ctc2, Op::Cop2, Op::Cop2, Op::Cop2,
            Op::Cop2, Op::Lwc2, Op::Ldc2, Op::Swc2, Op::Sdc2]);
        assert_eq!(decode(0x0000_0001).op, Op::Reserved);
    }
}
