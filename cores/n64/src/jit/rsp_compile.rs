//! RSP microcode translated to A32. A block runs from an address in IMEM
//! to a branch and its delay slot, a move to COP0 or a BREAK (after which
//! the RSP may halt or have new code), or 64 instructions. Scalar
//! arithmetic, branches, DMEM loads and stores, reads of the SP's and
//! RDP's registers, the DMA addresses, moves of whole lanes to and from
//! the vector unit and its plain loads and stores (an even element, no
//! wrap past DMEM's end) are compiled; the vector unit's computations are
//! a call to it, and the rest, with any DMEM access that wraps, call the
//! interpreter's own execution of the instruction.
//!
//! Timing is the interpreter's (`rsp::pipe`), worked out while compiling
//! for the state the block is entered with (whether the last instruction
//! may pair, was a vector one, was a delay slot). The clock is a base in
//! two registers plus a constant; only where a register loaded within the
//! last three cycles is read, and the compiler cannot know that load's
//! time, does code compare against `ready` and move the base. Before each
//! instruction the block leaves if the burst is over, and before one that
//! reaches outside the RSP (`rsp::outside`) if its slice is, as the
//! interpreter's loop does, so the RSP and the CPU interleave the same.
//!
//! Generated functions take `*mut Rsp`, DMEM, `*mut N64` (for calls) and
//! the low word of the burst's end; they write back the pc, the clock and
//! the pipeline's state, and return how many instructions they ran.
//!
//! Where a block ends in order or after a branch's delay slot, it goes on
//! into the next block itself if the burst is not over and the dispatcher's
//! table of links has that block for IMEM as it is now (`Calls::links`, an
//! entry for each address and entry state: where its code goes on after
//! its prologue, and the count of IMEM writes it was found matching at).
//! Loops then run without returning until the burst ends.

use super::arm::*;
use crate::N64;
use crate::rdp::Rdp;
use crate::rsp::{BRANCH, Pipe, Rsp, SLOT, outside, pipe};
use crate::vu::Vu;
use std::mem::offset_of;

const RSP: Reg = 4;
const DMEM: Reg = 5;
const MACHINE: Reg = 6;
/// The burst's end minus the base: the block leaves at an instruction
/// whose clock (base + k) reaches it.
const BUDGET: Reg = 7;
const BASE_LO: Reg = 8;
const BASE_HI: Reg = 9;
const END: Reg = 10;
/// The pc a branch goes on to after its delay slot.
const DEST: Reg = 11;
/// Pushed: r3-r11 and lr, ten words, keeping the stack 8-byte aligned.
const SAVED: u32 = 0x0ff8;

const GPR: i32 = offset_of!(Rsp, gpr) as i32;
const PC_AT: i32 = offset_of!(Rsp, pc) as i32;
const NEXT_PC: i32 = offset_of!(Rsp, next_pc) as i32;
const CLOCK: i32 = offset_of!(Rsp, clock) as i32;
const READY: i32 = offset_of!(Rsp, ready) as i32;
const PAIRS: i32 = offset_of!(Rsp, pairs) as i32;
const VECTOR: i32 = offset_of!(Rsp, vector) as i32;
const AFTER: i32 = offset_of!(Rsp, after) as i32;
const IMEM_WRITES: i32 = offset_of!(Rsp, imem_writes) as i32;
const SLICE_END: i32 = offset_of!(Rsp, slice_end) as i32;
const VU: u32 = offset_of!(Rsp, vu) as u32;
const VU_REGS: u32 = VU + offset_of!(Vu, regs) as u32;
/// The SP's registers as COP0 reads them (0-4), and the RDP's (8-11),
/// from the machine.
const SP_REGISTERS: [i32; 5] = [
    offset_of!(Rsp, mem_addr) as i32,
    offset_of!(Rsp, dram_addr) as i32,
    offset_of!(Rsp, length) as i32,
    offset_of!(Rsp, length) as i32,
    offset_of!(Rsp, status) as i32,
];
const SEMAPHORE: i32 = offset_of!(Rsp, semaphore) as i32;
const DP_REGISTERS: [u32; 4] = [
    (offset_of!(N64, rdp) + offset_of!(Rdp, start)) as u32,
    (offset_of!(N64, rdp) + offset_of!(Rdp, end)) as u32,
    (offset_of!(N64, rdp) + offset_of!(Rdp, current)) as u32,
    (offset_of!(N64, rdp) + offset_of!(Rdp, status)) as u32,
];

const MAX_INSTRUCTIONS: u32 = 64;
const MAX_CYCLES: i32 = 200;

/// The pipeline's state a block is entered with, as a variant of its
/// cached code (0-7).
pub fn variant(pairs: bool, vector: bool, after: u8) -> usize {
    usize::from(after == SLOT) | usize::from(pairs) << 1 | usize::from(vector) << 2
}

/// The functions compiled code calls: to run an instruction (machine,
/// word, pc), and the vector unit's computations (vu, word); and the table
/// of links between blocks, two words for each slot (8 for each IMEM
/// word, by variant): the code to go on at, and IMEM's count of writes.
#[derive(Clone, Copy)]
pub struct Calls {
    pub interpret: usize,
    pub vector: usize,
    pub links: usize,
}

/// Compiles the block at `pc` of `imem` (1024 words), entered with the
/// pipeline state of `variant`: its code, how many words from `pc` on it
/// depends on, and where another block goes on into it (after the prologue,
/// in words); None if its first instruction cannot start one.
pub fn block(imem: &[u32], pc: u32, variant: usize, calls: Calls) -> Option<(Vec<u32>, usize, usize)> {
    let mut c = Compiler {
        e: Emitter::default(),
        calls,
        exits: Vec::new(),
        slow: Vec::new(),
        k: 0,
        total: 0,
        epoch: 0,
        pairs: variant & 2 != 0,
        vector: variant & 4 != 0,
        after: if variant & 1 != 0 { SLOT } else { 0 },
        loaded: [None; 32],
        count: 0,
    };
    c.e.push(SAVED | 1 << LR);
    c.e.mov(RSP, R0);
    c.e.mov(DMEM, R1);
    c.e.mov(MACHINE, R2);
    c.e.mov(END, R3);
    c.e.ldr(BASE_LO, RSP, CLOCK);
    c.e.ldr(BASE_HI, RSP, CLOCK + 4);
    c.e.alu(Op::Sub, BUDGET, END, BASE_LO);
    // The instructions run, over blocks gone on into, in the stack slot of
    // the saved r3.
    c.e.mov_imm(R0, 0);
    c.e.str(R0, SP, 0);
    let entry = c.e.here();
    let mut pc = pc & 0xffc;
    let mut looked = 0;
    loop {
        let in_slot = c.after == BRANCH;
        if !in_slot && (c.count >= MAX_INSTRUCTIONS || c.k > MAX_CYCLES) {
            c.go_on(Some(pc));
            c.leave_sequential(pc);
            break;
        }
        let word = imem[pc as usize >> 2];
        let p = pipe(word);
        // This word, and a branch's delay slot, decide what is compiled.
        looked = c.count as usize + if p.branch { 2 } else { 1 };
        if p.branch && (in_slot || pipe(imem[((pc + 4) & 0xffc) as usize >> 2]).branch) {
            // A branch in a delay slot, or with one in its own, is left to
            // the interpreter.
            if c.count == 0 {
                return None;
            }
            c.go_on(Some(pc));
            c.leave_sequential(pc);
            break;
        }
        if c.count > 0 {
            c.check_slice(pc);
        }
        // Also first in a block: another may go on into this one.
        if outside(word) {
            c.check_outside(pc);
        }
        c.time(pc, p);
        let ends = c.instruction(word, pc);
        c.count += 1;
        if in_slot {
            // Not after a move to COP0 or a BREAK in the slot: the RSP may
            // have halted, or have new code.
            if !ends {
                c.go_on(None);
            }
            c.leave_after_slot();
            break;
        }
        let next = (pc + 4) & 0xffc;
        if ends {
            c.leave_sequential(next);
            break;
        }
        pc = next;
    }
    c.finish();
    Some((c.e.code, looked.max(c.count as usize), entry))
}

#[derive(Clone, Copy)]
struct Loaded {
    /// Which base the ready time is relative to, the time, and the time
    /// counted from the block's entry without stalls the code measures.
    epoch: u32,
    ready: i32,
    total: i32,
}

/// The state written back on leaving: the pc, the next pc (a constant, or
/// a branch's destination), the clock (base + k), the pipeline's flags
/// and the instructions run.
#[derive(Clone, Copy)]
struct Exit {
    pc: u32,
    next: Option<u32>,
    k: i32,
    flags: u32,
    count: u32,
}

/// A DMEM access that wraps: the interpreter runs it.
struct Slow {
    branch: usize,
    word: u32,
    pc: u32,
    resume: usize,
}

struct Compiler {
    e: Emitter,
    calls: Calls,
    exits: Vec<(usize, Exit)>,
    slow: Vec<Slow>,
    /// The clock before the next instruction: base + k.
    k: i32,
    /// The same, from the block's entry, without measured stalls: a lower
    /// bound of the real clock.
    total: i32,
    /// Bumped each time code moves the base.
    epoch: u32,
    pairs: bool,
    vector: bool,
    after: u8,
    loaded: [Option<Loaded>; 32],
    count: u32,
}

fn reg(n: usize) -> i32 {
    GPR + 4 * n as i32
}

impl Compiler {
    fn flags(&self) -> u32 {
        u32::from(self.pairs) | u32::from(self.vector) << 8 | u32::from(self.after) << 16
    }

    fn exit_here(&self, pc: u32, next: Option<u32>) -> Exit {
        Exit { pc, next, k: self.k, flags: self.flags(), count: self.count }
    }

    /// Leaves before the instruction at `pc` once the clock has reached
    /// the burst's end.
    fn check_slice(&mut self, pc: u32) {
        let next = if self.after == BRANCH { None } else { Some((pc + 4) & 0xffc) };
        let exit = self.exit_here(pc, next);
        if !self.e.alu_imm(Cond::Al, Op::Cmp, true, 0, BUDGET, self.k as u32) {
            self.e.mov_imm(R12, self.k as u32);
            self.e.alu(Op::Cmp, 0, BUDGET, R12);
        }
        let branch = self.e.b(Cond::Le);
        self.exits.push((branch, exit));
    }

    /// Leaves before the instruction at `pc`, which reaches outside the
    /// RSP, once the clock has reached the end of the burst's slice.
    fn check_outside(&mut self, pc: u32) {
        let next = if self.after == BRANCH { None } else { Some((pc + 4) & 0xffc) };
        let exit = self.exit_here(pc, next);
        self.e.ldr(R0, RSP, SLICE_END);
        self.e.alu(Op::Sub, R0, R0, BASE_LO);
        if !self.e.alu_imm(Cond::Al, Op::Cmp, true, 0, R0, self.k as u32) {
            self.e.mov_imm(R12, self.k as u32);
            self.e.alu(Op::Cmp, 0, R0, R12);
        }
        let branch = self.e.b(Cond::Le);
        self.exits.push((branch, exit));
    }

    /// `lo:hi = base + offset`.
    fn base_plus(&mut self, lo: Reg, hi: Reg, offset: i32) {
        let (op, carry, magnitude) =
            if offset >= 0 { (Op::Add, Op::Adc, offset as u32) } else { (Op::Sub, Op::Sbc, offset.unsigned_abs()) };
        if !self.e.alu_imm(Cond::Al, op, true, lo, BASE_LO, magnitude) {
            self.e.mov_imm(R12, magnitude);
            self.e.alu_shifted(Cond::Al, op, true, lo, BASE_LO, R12, Shift::Lsl(0));
        }
        self.e.alu_imm(Cond::Al, carry, false, hi, BASE_HI, 0);
    }

    /// The interpreter's `time`, at compile time where it can be.
    fn time(&mut self, pc: u32, p: Pipe) {
        let slot = self.after == BRANCH;
        let paired = self.pairs && !slot && p.vector != self.vector;
        let mut issue = if paired { self.k - 1 } else { self.k };
        let mut total = self.total + (issue - self.k);
        let reads = if p.reads[1] == p.reads[0] { [p.reads[0], 0] } else { p.reads };
        // Loads earlier in this stretch of the same base are known.
        for r in reads {
            if let Some(l) = self.loaded[r]
                && r != 0
                && l.epoch == self.epoch
                && l.ready > issue
            {
                total += l.ready - issue;
                issue = l.ready;
            }
        }
        // The rest may still be loading: one loaded before the block
        // (ready at most two cycles after its entry), or before the base
        // last moved.
        let unknown: Vec<usize> = reads
            .into_iter()
            .filter(|&r| {
                r != 0
                    && match self.loaded[r] {
                        Some(l) => l.epoch != self.epoch && total < l.total,
                        None => total < 3,
                    }
            })
            .collect();
        if !unknown.is_empty() {
            self.base_plus(R0, R1, issue);
            for r in unknown {
                let at = READY + 8 * r as i32;
                self.e.ldr(R2, RSP, at);
                self.e.ldr(R3, RSP, at + 4);
                // Unsigned r0:r1 < r2:r3 leaves the carry clear.
                self.e.alu(Op::Cmp, 0, R0, R2);
                self.e.alu_shifted(Cond::Al, Op::Sbc, true, R12, R1, R3, Shift::Lsl(0));
                self.e.alu_shifted(Cond::Lo, Op::Mov, false, R0, 0, R2, Shift::Lsl(0));
                self.e.alu_shifted(Cond::Lo, Op::Mov, false, R1, 0, R3, Shift::Lsl(0));
            }
            self.e.mov(BASE_LO, R0);
            self.e.mov(BASE_HI, R1);
            self.e.alu(Op::Sub, BUDGET, END, BASE_LO);
            self.epoch += 1;
            issue = 0;
        }
        if p.loads != 0 {
            self.base_plus(R0, R1, issue + 3);
            let at = READY + 8 * p.loads as i32;
            self.e.str(R0, RSP, at);
            self.e.str(R1, RSP, at + 4);
            self.loaded[p.loads] = Some(Loaded { epoch: self.epoch, ready: issue + 3, total: total + 3 });
        }
        self.k = issue + 1;
        self.total = total + 1;
        self.pairs = !(paired || p.branch || slot || self.after == SLOT && pc & 4 != 0);
        self.vector = p.vector;
        if slot {
            self.k += 1;
            self.total += 1;
        }
        self.after = if p.branch { BRANCH } else if slot { SLOT } else { 0 };
    }

    fn get(&mut self, arm: Reg, guest: usize) {
        if guest == 0 {
            self.e.mov_imm(arm, 0);
        } else {
            self.e.ldr(arm, RSP, reg(guest));
        }
    }

    fn set(&mut self, arm: Reg, guest: usize) {
        if guest != 0 {
            self.e.str(arm, RSP, reg(guest));
        }
    }

    /// The interpreter runs the instruction.
    fn call(&mut self, word: u32, pc: u32) {
        self.e.mov(R0, MACHINE);
        self.e.mov_imm(R1, word);
        self.e.mov_imm(R2, pc);
        self.e.mov_imm(R12, self.calls.interpret as u32);
        self.e.blx(R12);
    }

    /// r2 = the address of a vector register.
    fn vector_register(&mut self, vt: usize) {
        self.e.add_imm(R2, RSP, VU_REGS + 16 * vt as u32, R12);
    }

    /// A wrapping DMEM access of `size` bytes at r0 goes to the
    /// interpreter, out of line; the caller marks where to come back.
    fn unless_wrapping(&mut self, size: u32, word: u32, pc: u32) {
        if size > 1 {
            self.e.alu_imm(Cond::Al, Op::Add, false, R1, R0, size - 1);
            self.e.alu_imm(Cond::Al, Op::Tst, true, 0, R1, 0x1000);
            let branch = self.e.b(Cond::Ne);
            self.slow.push(Slow { branch, word, pc, resume: 0 });
        }
    }

    fn resume_here(&mut self, size: u32) {
        if size > 1 {
            let here = self.e.here();
            self.slow.last_mut().unwrap().resume = here;
        }
    }

    /// LSV, LLV, LDV, LQV and their stores at an even element, all bytes
    /// within the register (and LQV and SQV whole and aligned); false for
    /// the other forms. DMEM's bytes are big-endian, a register's lanes
    /// the host's halfwords.
    fn vector_memory(&mut self, word: u32, pc: u32) -> bool {
        let store = word >> 26 == 0x3a;
        let (rs, vt) = ((word >> 21 & 31) as usize, (word >> 16 & 31) as usize);
        let e = word >> 7 & 15;
        let size = match word >> 11 & 31 {
            1 => 2,
            2 => 4,
            3 => 8,
            4 if e == 0 => 16,
            _ => return false,
        };
        if e & 1 != 0 || e + size > 16 {
            return false;
        }
        let offset = ((word as i32) << 25 >> 25) as u32;
        self.get(R0, rs);
        self.e.add_imm(R0, R0, offset.wrapping_mul(size), R12);
        self.e.ubfx(R0, R0, 0, 12);
        if size == 16 {
            // Aligned, or the interpreter (which stops at the line's end).
            self.e.alu_imm(Cond::Al, Op::Tst, true, 0, R0, 15);
            let branch = self.e.b(Cond::Ne);
            self.slow.push(Slow { branch, word, pc, resume: 0 });
        } else {
            self.unless_wrapping(size, word, pc);
        }
        self.vector_register(vt);
        self.e.alu(Op::Add, R0, DMEM, R0);
        if size == 2 {
            if store {
                self.e.ldrh(R1, R2, e as i32);
                self.e.rev16(R1, R1);
                self.e.strh(R1, R0, 0);
            } else {
                self.e.ldrh(R1, R0, 0);
                self.e.rev16(R1, R1);
                self.e.strh(R1, R2, e as i32);
            }
        } else {
            for at in (0..size as i32).step_by(4) {
                if store {
                    self.e.ldr(R1, R2, e as i32 + at);
                    self.e.rev16(R1, R1);
                    self.e.str(R1, R0, at);
                } else {
                    self.e.ldr(R1, R0, at);
                    self.e.rev16(R1, R1);
                    self.e.str(R1, R2, e as i32 + at);
                }
            }
        }
        let here = self.e.here();
        self.slow.last_mut().unwrap().resume = here;
        true
    }

    /// MFC0 and MTC0 that only read or set a register; false for the
    /// others (the semaphore's read, DMA, status and the RDP's writes).
    fn cop0(&mut self, word: u32) -> bool {
        let (rt, rd) = ((word >> 16 & 31) as usize, word >> 11 & 15);
        if word >> 21 & 31 == 0 {
            match rd {
                0..=4 => self.e.ldr(R0, RSP, SP_REGISTERS[rd as usize]),
                5 | 6 | 12..=15 => self.e.mov_imm(R0, 0),
                8..=11 => {
                    self.e.add_imm(R1, MACHINE, DP_REGISTERS[rd as usize - 8], R12);
                    self.e.ldr(R0, R1, 0);
                }
                _ => return false,
            }
            self.set(R0, rt);
            return true;
        }
        match rd {
            0 | 1 => {
                self.get(R0, rt);
                let mask = if rd == 0 { 0x1ff8 } else { 0xff_fff8 };
                self.e.mov_imm(R1, mask);
                self.e.alu(Op::And, R0, R0, R1);
                self.e.str(R0, RSP, SP_REGISTERS[rd as usize]);
            }
            5 | 6 => {}
            7 => {
                self.e.mov_imm(R0, 0);
                self.e.strb(R0, RSP, SEMAPHORE);
            }
            _ => return false,
        }
        true
    }

    /// MFC2 and MTC2 of a whole lane (an even element).
    fn cop2_move(&mut self, word: u32) -> bool {
        let (rs, rt, rd) = (word >> 21 & 31, (word >> 16 & 31) as usize, (word >> 11 & 31) as usize);
        let e = word >> 7 & 15;
        if e & 1 != 0 || !matches!(rs, 0 | 4) {
            return false;
        }
        self.vector_register(rd);
        if rs == 0 {
            self.e.ldrsh(R0, R2, e as i32);
            self.set(R0, rt);
        } else {
            self.get(R0, rt);
            self.e.strh(R0, R2, e as i32);
        }
        true
    }

    /// Code for an instruction; true if the block ends after it.
    fn instruction(&mut self, word: u32, pc: u32) -> bool {
        let (rs, rt, rd) = ((word >> 21 & 31) as usize, (word >> 16 & 31) as usize, (word >> 11 & 31) as usize);
        let simm = word as i16 as i32 as u32;
        let uimm = word & 0xffff;
        let sa = word >> 6 & 31;
        let target = (pc + 4).wrapping_add(simm << 2) & 0xffc;
        let link = (pc + 8) & 0xfff;
        match word >> 26 {
            0x00 => match word & 63 {
                0x00 | 0x02 | 0x03 if rd != 0 => {
                    self.get(R0, rt);
                    if sa != 0 {
                        let shift = match word & 63 {
                            0x00 => Shift::Lsl(sa),
                            0x02 => Shift::Lsr(sa),
                            _ => Shift::Asr(sa),
                        };
                        self.e.alu_shifted(Cond::Al, Op::Mov, false, R0, 0, R0, shift);
                    }
                    self.set(R0, rd);
                }
                0x04 | 0x06 | 0x07 if rd != 0 => {
                    self.get(R0, rt);
                    self.get(R1, rs);
                    self.e.alu_imm(Cond::Al, Op::And, false, R1, R1, 31);
                    let shift = match word & 63 {
                        0x04 => Shift::LslReg(R1),
                        0x06 => Shift::LsrReg(R1),
                        _ => Shift::AsrReg(R1),
                    };
                    self.e.alu_shifted(Cond::Al, Op::Mov, false, R0, 0, R0, shift);
                    self.set(R0, rd);
                }
                0x08 | 0x09 => {
                    self.get(DEST, rs);
                    self.e.ubfx(DEST, DEST, 2, 10);
                    self.e.alu_shifted(Cond::Al, Op::Mov, false, DEST, 0, DEST, Shift::Lsl(2));
                    if word & 63 == 0x09 && rd != 0 {
                        self.e.mov_imm(R0, link);
                        self.set(R0, rd);
                    }
                }
                0x0d => {
                    self.call(word, pc);
                    return true;
                }
                0x20..=0x27 | 0x2a | 0x2b if rd != 0 => {
                    self.get(R0, rs);
                    self.get(R1, rt);
                    match word & 63 {
                        0x20 | 0x21 => self.e.alu(Op::Add, R0, R0, R1),
                        0x22 | 0x23 => self.e.alu(Op::Sub, R0, R0, R1),
                        0x24 => self.e.alu(Op::And, R0, R0, R1),
                        0x25 => self.e.alu(Op::Orr, R0, R0, R1),
                        0x26 => self.e.alu(Op::Eor, R0, R0, R1),
                        0x27 => {
                            self.e.alu(Op::Orr, R0, R0, R1);
                            self.e.alu(Op::Mvn, R0, 0, R0);
                        }
                        funct => self.compare(funct == 0x2a),
                    }
                    self.set(R0, rd);
                }
                _ => {}
            },
            0x01 => {
                self.get(R0, rs);
                self.e.mov_imm(DEST, (pc + 8) & 0xffc);
                let cond = match rt & 0xf {
                    0 => Some(Cond::Lt),
                    1 => Some(Cond::Ge),
                    _ => None,
                };
                if let Some(cond) = cond {
                    self.e.alu_imm(Cond::Al, Op::Cmp, true, 0, R0, 0);
                    self.e.movw(cond, DEST, target);
                }
                // BLTZAL and BGEZAL link whether or not they branch.
                if rt & 0x10 != 0 {
                    self.e.mov_imm(R0, link);
                    self.set(R0, 31);
                }
            }
            0x02 | 0x03 => {
                self.e.mov_imm(DEST, (word & 0x3ff) << 2);
                if word >> 26 == 0x03 {
                    self.e.mov_imm(R0, link);
                    self.set(R0, 31);
                }
            }
            op @ 0x04..=0x07 => {
                self.get(R0, rs);
                self.e.mov_imm(DEST, (pc + 8) & 0xffc);
                if op <= 0x05 {
                    self.get(R1, rt);
                    self.e.alu(Op::Cmp, 0, R0, R1);
                } else {
                    self.e.alu_imm(Cond::Al, Op::Cmp, true, 0, R0, 0);
                }
                let cond = [Cond::Eq, Cond::Ne, Cond::Le, Cond::Gt][op as usize - 4];
                self.e.movw(cond, DEST, target);
            }
            op @ 0x08..=0x0e if rt != 0 => {
                self.get(R0, rs);
                match op {
                    0x08 | 0x09 => self.e.add_imm(R0, R0, simm, R12),
                    0x0a | 0x0b => {
                        self.e.mov_imm(R1, simm);
                        self.e.alu(Op::Cmp, 0, R0, R1);
                        self.set_flag(if op == 0x0a { Cond::Lt } else { Cond::Lo });
                    }
                    _ => {
                        let logic = [Op::And, Op::Orr, Op::Eor][op as usize - 0x0c];
                        if !self.e.alu_imm(Cond::Al, logic, false, R0, R0, uimm) {
                            self.e.mov_imm(R1, uimm);
                            self.e.alu(logic, R0, R0, R1);
                        }
                    }
                }
                self.set(R0, rt);
            }
            0x0f if rt != 0 => {
                self.e.mov_imm(R0, uimm << 16);
                self.set(R0, rt);
            }
            0x10 => match rs {
                0x00 | 0x04 if self.cop0(word) => {}
                0x00 => self.call(word, pc),
                0x04 => {
                    self.call(word, pc);
                    return true;
                }
                _ => {}
            },
            0x12 if word & 1 << 25 != 0 => {
                self.e.add_imm(R0, RSP, VU, R12);
                self.e.mov_imm(R1, word);
                // The instruction's function's own entry where it has one
                // (the same call as calls.vector's, without its dispatch).
                self.e.mov_imm(R12, crate::vu::entry(word).map_or(self.calls.vector, |f| f as usize) as u32);
                self.e.blx(R12);
            }
            0x12 if self.cop2_move(word) => {}
            0x32 | 0x3a if self.vector_memory(word, pc) => {}
            0x12 | 0x32 | 0x3a => self.call(word, pc),
            op @ (0x20 | 0x21 | 0x23..=0x25 | 0x27 | 0x28 | 0x29 | 0x2b) => {
                let store = op >= 0x28;
                if !store && rt == 0 {
                    return false;
                }
                self.get(R0, rs);
                self.e.add_imm(R0, R0, simm, R12);
                self.e.ubfx(R0, R0, 0, 12);
                // The last byte the access reaches must not wrap.
                let width = match op & 3 {
                    0 => 1,
                    1 => 2,
                    _ => 4,
                };
                self.unless_wrapping(width, word, pc);
                if store {
                    self.get(R1, rt);
                }
                match op {
                    0x20 => self.e.ldrsb_reg(R1, DMEM, R0),
                    0x24 => self.e.ldrb_reg(R1, DMEM, R0),
                    0x21 | 0x25 => {
                        self.e.ldrh_reg(R1, DMEM, R0);
                        self.e.rev16(R1, R1);
                        if op == 0x21 {
                            self.e.sxth(R1, R1);
                        }
                    }
                    0x23 | 0x27 => {
                        self.e.ldr_reg(R1, DMEM, R0);
                        self.e.rev(R1, R1);
                    }
                    0x28 => self.e.strb_reg(R1, DMEM, R0),
                    0x29 => {
                        self.e.rev16(R1, R1);
                        self.e.strh_reg(R1, DMEM, R0);
                    }
                    _ => {
                        self.e.rev(R1, R1);
                        self.e.str_reg(R1, DMEM, R0);
                    }
                }
                if !store {
                    self.set(R1, rt);
                }
                self.resume_here(width);
            }
            _ => {}
        }
        false
    }

    /// SLT/SLTU of r0 and r1.
    fn compare(&mut self, signed: bool) {
        self.e.alu(Op::Cmp, 0, R0, R1);
        self.set_flag(if signed { Cond::Lt } else { Cond::Lo });
    }

    /// r0 = 1 where the condition holds, else 0.
    fn set_flag(&mut self, cond: Cond) {
        self.e.alu_imm(Cond::Al, Op::Mov, false, R0, 0, 0);
        self.e.alu_imm(cond, Op::Mov, false, R0, 0, 1);
    }

    /// Goes on into the block at `pc` (or the branch's destination) for the
    /// pipeline's state now, if the burst is not over and the table of
    /// links has it for IMEM as it is; falls through to leaving otherwise.
    fn go_on(&mut self, pc: Option<u32>) {
        let k = self.k;
        let variant = variant(self.pairs, self.vector, self.after) as u32;
        let mut skips = Vec::new();
        if !self.e.alu_imm(Cond::Al, Op::Cmp, true, 0, BUDGET, k as u32) {
            self.e.mov_imm(R12, k as u32);
            self.e.alu(Op::Cmp, 0, BUDGET, R12);
        }
        skips.push(self.e.b(Cond::Le));
        match pc {
            Some(pc) => self.e.mov_imm(R12, self.calls.links as u32 + ((pc >> 2) * 8 + variant) * 8),
            None => {
                // DEST is a word's address: its slot's entry at DEST * 16.
                self.e.mov_imm(R12, self.calls.links as u32 + variant * 8);
                self.e.alu_shifted(Cond::Al, Op::Add, false, R12, R12, DEST, Shift::Lsl(4));
            }
        }
        self.e.ldr(R0, R12, 4);
        self.e.ldr(R1, RSP, IMEM_WRITES);
        self.e.alu(Op::Cmp, 0, R0, R1);
        skips.push(self.e.b(Cond::Ne));
        self.e.ldr(R2, R12, 0);
        self.e.alu_imm(Cond::Al, Op::Cmp, true, 0, R2, 0);
        skips.push(self.e.b(Cond::Eq));
        self.e.ldr(R0, SP, 0);
        self.e.add_imm(R0, R0, self.count, R1);
        self.e.str(R0, SP, 0);
        // The clock's base moves to the next block's start.
        self.base_plus(BASE_LO, BASE_HI, k);
        if !self.e.alu_imm(Cond::Al, Op::Sub, false, BUDGET, BUDGET, k as u32) {
            self.e.mov_imm(R1, k as u32);
            self.e.alu(Op::Sub, BUDGET, BUDGET, R1);
        }
        self.e.bx(R2);
        for skip in skips {
            self.e.patch_here(skip);
        }
    }

    /// Writes back the state for going on at `pc` in order, and returns.
    fn leave_sequential(&mut self, pc: u32) {
        let exit = self.exit_here(pc, Some((pc + 4) & 0xffc));
        self.leave(exit);
    }

    /// After a delay slot: on at the branch's destination.
    fn leave_after_slot(&mut self) {
        self.e.mov(R0, DEST);
        self.e.alu_imm(Cond::Al, Op::Add, false, R1, DEST, 4);
        self.e.alu_imm(Cond::Al, Op::Bic, false, R1, R1, 0x1000);
        self.common_exit(self.k, self.flags(), self.count);
    }

    fn leave(&mut self, exit: Exit) {
        self.e.mov_imm(R0, exit.pc);
        match exit.next {
            Some(next) => self.e.mov_imm(R1, next),
            None => self.e.mov(R1, DEST),
        }
        self.common_exit(exit.k, exit.flags, exit.count);
    }

    /// r0 the pc, r1 the next.
    fn common_exit(&mut self, k: i32, flags: u32, count: u32) {
        self.e.str(R0, RSP, PC_AT);
        self.e.str(R1, RSP, NEXT_PC);
        self.base_plus(R0, R1, k);
        self.e.str(R0, RSP, CLOCK);
        self.e.str(R1, RSP, CLOCK + 4);
        self.e.mov_imm(R0, flags);
        self.e.strb(R0, RSP, PAIRS);
        self.e.alu_shifted(Cond::Al, Op::Mov, false, R0, 0, R0, Shift::Lsr(8));
        self.e.strb(R0, RSP, VECTOR);
        self.e.alu_shifted(Cond::Al, Op::Mov, false, R0, 0, R0, Shift::Lsr(8));
        self.e.strb(R0, RSP, AFTER);
        self.e.ldr(R0, SP, 0);
        self.e.add_imm(R0, R0, count, R1);
        self.e.pop(SAVED | 1 << PC);
    }

    /// The code out of line: slice exits and wrapping DMEM accesses.
    fn finish(&mut self) {
        for (branch, exit) in std::mem::take(&mut self.exits) {
            self.e.patch_here(branch);
            self.leave(exit);
        }
        for slow in std::mem::take(&mut self.slow) {
            self.e.patch_here(slow.branch);
            self.call(slow.word, slow.pc);
            let back = self.e.b(Cond::Al);
            self.e.patch(back, slow.resume);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variants_cover_entry_states() {
        let mut seen = [false; 8];
        for after in [0, SLOT] {
            for pairs in [false, true] {
                for vector in [false, true] {
                    seen[variant(pairs, vector, after)] = true;
                }
            }
        }
        assert!(seen.iter().all(|&s| s));
    }

    #[test]
    fn every_word_compiles_or_declines() {
        // Each opcode and function field, with branches' slots and
        // registers spread, at both ends of IMEM.
        let mut imem = vec![0u32; 1024];
        let mut seed = 0x1234_5678_9abc_def0u64;
        for word in imem.iter_mut() {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            *word = seed as u32;
        }
        for pc in (0..0x1000).step_by(4) {
            for v in 0..8 {
                let _ = block(&imem, pc, v, Calls { interpret: 0x1000, vector: 0x2000, links: 0x10_0000 });
            }
        }
    }
}
