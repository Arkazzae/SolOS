//! What the compilers for 32- and 64-bit ARM have in common: a block's result, where blocks find
//! the machine's fields, and which instructions go where in a block.

use crate::cop0::Cop0;
use crate::cpu::Cpu;
use crate::decode::{Instr, Op as Mips, decode};
use crate::fpu::Fpu;
use std::mem::offset_of;

/// In a block's result: an instruction was interpreted on the way (so the
/// first one is not still to run); the interpreted one stopped the block
/// with all its state written; the terminal branch completed.
pub const PROGRESS: u32 = 1 << 29;
/// In a block's result: it stopped after storing into a page of code.
pub const WROTE_CODE: u32 = 1 << 28;
pub const STOPPED: u32 = 1 << 30;
pub const BRANCHED: u32 = 1 << 31;
pub const HI: i32 = offset_of!(Cpu, hi) as i32;
pub const LO: i32 = offset_of!(Cpu, lo) as i32;
pub const CPU_PC: i32 = offset_of!(Cpu, pc) as i32;
pub const NEXT_PC: i32 = offset_of!(Cpu, next_pc) as i32;
pub const CURRENT_PC: i32 = offset_of!(Cpu, current_pc) as i32;
pub const CURRENT_DELAY: i32 = offset_of!(Cpu, current_delay) as i32;
pub const DELAY_SLOT: i32 = offset_of!(Cpu, delay_slot) as i32;
pub const STATUS: i32 = (offset_of!(Cpu, cop0) + offset_of!(Cop0, status)) as i32;
pub const CAUSE: i32 = (offset_of!(Cpu, cop0) + offset_of!(Cop0, cause)) as i32;
pub const INDEX: i32 = (offset_of!(Cpu, cop0) + offset_of!(Cop0, index)) as i32;
pub const CONTEXT: i32 = (offset_of!(Cpu, cop0) + offset_of!(Cop0, context)) as i32;
pub const BAD_VADDR: i32 = (offset_of!(Cpu, cop0) + offset_of!(Cop0, bad_vaddr)) as i32;
pub const ENTRY_HI: i32 = (offset_of!(Cpu, cop0) + offset_of!(Cop0, entry_hi)) as i32;
pub const COMPARE: i32 = (offset_of!(Cpu, cop0) + offset_of!(Cop0, compare)) as i32;
pub const EPC: i32 = (offset_of!(Cpu, cop0) + offset_of!(Cop0, epc)) as i32;
pub const CONFIG: i32 = (offset_of!(Cpu, cop0) + offset_of!(Cop0, config)) as i32;
pub const ERROR_EPC: i32 = (offset_of!(Cpu, cop0) + offset_of!(Cop0, error_epc)) as i32;
pub const LATCH: i32 = (offset_of!(Cpu, cop0) + offset_of!(Cop0, latch)) as i32;
pub const FPR: i32 = (offset_of!(Cpu, fpu) + offset_of!(Fpu, regs)) as i32;
pub const FCR31: i32 = (offset_of!(Cpu, fpu) + offset_of!(Fpu, fcr31)) as i32;

/// An instruction that would always stop a block, left to the dispatcher
/// (the block ends before it): a return from an exception, an exception,
/// or a change of mode or mapping.
pub fn stops(op: Mips) -> bool {
    use Mips::*;
    matches!(op, Eret | Syscall | Break | Mtc0 | Dmtc0 | Tlbwi | Tlbwr | Tlbr | Tlbp)
}

/// Arithmetic that cannot fault, is never interpreted and leaves CONDITION
/// alone: what a delay slot run in the block may be.
pub fn plain(op: Mips) -> bool {
    use Mips::*;
    matches!(op, Sll | Srl | Sra | Sllv | Srlv | Srav | Dsll | Dsrl | Dsra | Dsll32 | Dsrl32 | Dsra32
        | Dsllv | Dsrlv | Dsrav | Mfhi | Mflo | Mthi | Mtlo | Mult | Multu | Div | Divu | Addu | Subu
        | Addiu | Daddiu | Daddu | Dsubu | And | Or | Xor | Nor | Slt | Sltu | Slti | Sltiu | Andi | Ori
        | Xori | Lui | Sync)
}

/// Loads and stores with an RDRAM path, which a delay slot run in the block
/// may be too: a guard exit before one leaves it to the interpreter, the
/// branch done.
pub fn memory(op: Mips) -> bool {
    use Mips::*;
    matches!(op, Lb | Lbu | Lh | Lhu | Lw | Lwu | Ld | Lwc1 | Ldc1 | Sb | Sh | Sw | Sd | Swc1 | Sdc1)
}

/// The FPU's instructions compiled with no interpreted path, which a delay
/// slot run in the block may be too (as `Compiler::instruction` picks
/// them).
pub fn native_fpu(i: Instr) -> bool {
    use Mips::*;
    let (funct, fmt) = (i.funct(), i.fmt());
    match i.op {
        Mfc1 | Dmfc1 | Mtc1 | Dmtc1 => true,
        FpuOp => funct == 6 || funct <= 4 && matches!(fmt, 16 | 17) || funct == 32 && fmt == 17
            || matches!(funct, 12..=15 | 36) && matches!(fmt, 16 | 17) || funct >= 48 && matches!(fmt, 16 | 17)
            || matches!(funct, 5 | 7) && matches!(fmt, 16 | 17) || funct == 33 && fmt == 16,
        FpuCvtInt => matches!(funct, 32 | 33) && fmt == 20,
        _ => false,
    }
}

/// Whether this word is a branch (with a delay slot).
#[cfg_attr(not(any(target_arch = "arm", target_arch = "aarch64")), allow(dead_code))]
pub fn branches(word: u32) -> bool {
    is_branch(decode(word).op)
}

pub fn is_branch(op: Mips) -> bool {
    use Mips::*;
    matches!(op, J | Jal | Jr | Jalr | Beq | Bne | Blez | Bgtz | Beql | Bnel | Blezl | Bgtzl
        | Bltz | Bgez | Bltzl | Bgezl | Bltzal | Bgezal | Bltzall | Bgezall | Bc1f | Bc1t | Bc1fl | Bc1tl)
}

pub fn is_likely(op: Mips) -> bool {
    use Mips::*;
    matches!(op, Beql | Bnel | Blezl | Bgtzl | Bltzl | Bgezl | Bltzall | Bgezall | Bc1fl | Bc1tl)
}

pub fn reg(n: usize) -> i32 {
    (offset_of!(Cpu, gpr) + 8 * n) as i32
}

pub fn fpr(n: usize) -> i32 {
    FPR + 8 * n as i32
}
