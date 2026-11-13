//! RSP microcode translated to A64, as rsp_compile.rs does to A32: the same blocks, the same timing
//! worked out while compiling (`rsp::pipe`) and the same exits, so the RSP and the CPU interleave as
//! in the interpreter. The machine's: the clock is one register, the instructions run over blocks
//! gone on into another (no stack slot), a condition is a CSEL, and calls and the table of links take
//! 64-bit addresses (`Calls`).
//!
//! Generated functions take `*mut Rsp`, DMEM, `*mut N64` (for calls) and the low word of the burst's
//! end; they write back the pc, the clock and the pipeline's state, and return how many instructions
//! they ran. Registers x19-x26 last across calls; x0-x3, x16 and x17 are scratch.

use super::a64::*;
use crate::N64;
use crate::rdp::Rdp;
use crate::rsp::{BRANCH, Pipe, Rsp, SLOT, outside, pipe};
use crate::vu::Vu;
use std::mem::offset_of;

const RSP: Reg = 19;
const DMEM: Reg = 20;
const MACHINE: Reg = 21;
/// The burst's end minus the base: the block leaves at an instruction whose clock (base + k) reaches
/// it.
const BUDGET: Reg = 22;
/// The clock's base.
const BASE: Reg = 23;
const END: Reg = 24;
/// The pc a branch goes on to after its delay slot.
const DEST: Reg = 25;
/// The instructions run, over blocks gone on into.
const RAN: Reg = 26;
/// The frame: the frame record and the callee-saved registers.
const FRAME: i32 = 80;
const SAVED: [(Reg, Reg, i32); 4] = [(19, 20, 16), (21, 22, 32), (23, 24, 48), (25, 26, 64)];

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
const VU: u64 = offset_of!(Rsp, vu) as u64;
const VU_REGS: u64 = VU + offset_of!(Vu, regs) as u64;
/// The SP's registers as COP0 reads them (0-4), and the RDP's (8-11), from the machine.
const SP_REGISTERS: [i32; 5] = [
    offset_of!(Rsp, mem_addr) as i32,
    offset_of!(Rsp, dram_addr) as i32,
    offset_of!(Rsp, length) as i32,
    offset_of!(Rsp, length) as i32,
    offset_of!(Rsp, status) as i32,
];
const SEMAPHORE: i32 = offset_of!(Rsp, semaphore) as i32;
const DP_REGISTERS: [u64; 4] = [
    (offset_of!(N64, rdp) + offset_of!(Rdp, start)) as u64,
    (offset_of!(N64, rdp) + offset_of!(Rdp, end)) as u64,
    (offset_of!(N64, rdp) + offset_of!(Rdp, current)) as u64,
    (offset_of!(N64, rdp) + offset_of!(Rdp, status)) as u64,
];

const MAX_INSTRUCTIONS: u32 = 64;
const MAX_CYCLES: i32 = 200;

/// A slot of the table of links, in bytes: two words of the host's size.
const LINK: u64 = 16;

/// The pipeline's state a block is entered with, as a variant of its cached code (0-7).
pub fn variant(pairs: bool, vector: bool, after: u8) -> usize {
    usize::from(after == SLOT) | usize::from(pairs) << 1 | usize::from(vector) << 2
}

/// The functions compiled code calls: to run an instruction (machine, word, pc), and the vector
/// unit's computations (vu, word); and the table of links between blocks, two words for each slot (8
/// for each IMEM word, by variant): the code to go on at, and IMEM's count of writes.
#[derive(Clone, Copy)]
pub struct Calls {
    pub interpret: usize,
    pub vector: usize,
    pub links: usize,
}

/// Compiles the block at `pc` of `imem` (1024 words), entered with the pipeline state of `variant`:
/// its code, how many words from `pc` on it depends on, and where another block goes on into it
/// (after the prologue, in words); None if its first instruction cannot start one.
pub fn block(imem: &[u32], pc: u32, variant: usize, calls: Calls) -> Option<(Vec<u32>, usize, usize)> {
    let mut c = Compiler {
        e: Emitter::default(),
        calls,
        exits: Vec::new(),
        slow: Vec::new(),
        ends: Vec::new(),
        k: 0,
        total: 0,
        epoch: 0,
        pairs: variant & 2 != 0,
        vector: variant & 4 != 0,
        after: if variant & 1 != 0 { SLOT } else { 0 },
        loaded: [None; 32],
        count: 0,
    };
    c.e.stp(FP, LR, SP, -FRAME, true);
    for (a, b, at) in SAVED {
        c.e.stp(a, b, SP, at, false);
    }
    c.e.mov(X, RSP, R0);
    c.e.mov(X, DMEM, R1);
    c.e.mov(X, MACHINE, R2);
    c.e.mov(W, END, R3);
    c.e.ldr(X, BASE, RSP, CLOCK);
    c.e.alu(Op::Sub, W, BUDGET, END, BASE);
    c.e.mov_imm(W, RAN, 0);
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
            // A branch in a delay slot, or with one in its own, is left to the interpreter.
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
            // Not after a move to COP0 or a BREAK in the slot: the RSP may have halted, or have new
            // code.
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
    /// Which base the ready time is relative to, the time, and the time counted from the block's entry
    /// without stalls the code measures.
    epoch: u32,
    ready: i32,
    total: i32,
}

/// The state written back on leaving: the pc, the next pc (a constant, or a branch's destination), the
/// clock (base + k), the pipeline's flags and the instructions run.
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
    /// The branches to the block's end (`finish`).
    ends: Vec<usize>,
    /// The clock before the next instruction: base + k.
    k: i32,
    /// The same, from the block's entry, without measured stalls: a lower bound of the real clock.
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

    /// Compares `rn` with the clock's offset k. Uses x16.
    fn cmp_k(&mut self, rn: Reg, k: i32) {
        if !self.e.alu_imm(Op::Subs, W, ZR, rn, k as u32 as u64) {
            self.e.mov_imm(W, IP0, k as u32 as u64);
            self.e.cmp(W, rn, IP0);
        }
    }

    /// Leaves before the instruction at `pc` once the clock has reached the burst's end.
    fn check_slice(&mut self, pc: u32) {
        let next = if self.after == BRANCH { None } else { Some((pc + 4) & 0xffc) };
        let exit = self.exit_here(pc, next);
        self.cmp_k(BUDGET, self.k);
        let branch = self.e.b(Cond::Le);
        self.exits.push((branch, exit));
    }

    /// Leaves before the instruction at `pc`, which reaches outside the RSP, once the clock has reached
    /// the end of the burst's slice.
    fn check_outside(&mut self, pc: u32) {
        let next = if self.after == BRANCH { None } else { Some((pc + 4) & 0xffc) };
        let exit = self.exit_here(pc, next);
        self.e.ldr(W, R0, RSP, SLICE_END);
        self.e.alu(Op::Sub, W, R0, R0, BASE);
        self.cmp_k(R0, self.k);
        let branch = self.e.b(Cond::Le);
        self.exits.push((branch, exit));
    }

    /// `rd = base + offset`. Uses x16.
    fn base_plus(&mut self, rd: Reg, offset: i32) {
        self.e.add_signed(X, rd, BASE, i64::from(offset), IP0);
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
        // The rest may still be loading: one loaded before the block (ready at most two cycles after its
        // entry), or before the base last moved.
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
            self.base_plus(R0, issue);
            for r in unknown {
                self.e.ldr(X, R2, RSP, READY + 8 * r as i32);
                self.e.cmp(X, R0, R2);
                self.e.csel(X, R0, R2, R0, Cond::Lo);
            }
            self.e.mov(X, BASE, R0);
            self.e.alu(Op::Sub, W, BUDGET, END, BASE);
            self.epoch += 1;
            issue = 0;
        }
        if p.loads != 0 {
            self.base_plus(R0, issue + 3);
            self.e.str(X, R0, RSP, READY + 8 * p.loads as i32);
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

    fn get(&mut self, rd: Reg, guest: usize) {
        if guest == 0 {
            self.e.mov_imm(W, rd, 0);
        } else {
            self.e.ldr(W, rd, RSP, reg(guest));
        }
    }

    fn set(&mut self, rs: Reg, guest: usize) {
        if guest != 0 {
            self.e.str(W, rs, RSP, reg(guest));
        }
    }

    /// The interpreter runs the instruction.
    fn call(&mut self, word: u32, pc: u32) {
        self.e.mov(X, R0, MACHINE);
        self.e.mov_imm(W, R1, u64::from(word));
        self.e.mov_imm(W, R2, u64::from(pc));
        self.e.mov_imm(X, IP0, self.calls.interpret as u64);
        self.e.blr(IP0);
    }

    /// x2 = the address of a vector register.
    fn vector_register(&mut self, vt: usize) {
        self.e.imm(Op::Add, X, R2, RSP, VU_REGS + 16 * vt as u64, IP0);
    }

    /// A wrapping DMEM access of `size` bytes at w0 goes to the interpreter, out of line; the caller
    /// marks where to come back.
    fn unless_wrapping(&mut self, size: u32, word: u32, pc: u32) {
        if size > 1 {
            assert!(self.e.alu_imm(Op::Add, W, R1, R0, u64::from(size - 1)));
            let branch = self.e.tbz(true, R1, 12);
            self.slow.push(Slow { branch, word, pc, resume: 0 });
        }
    }

    fn resume_here(&mut self, size: u32) {
        if size > 1 {
            let here = self.e.here();
            self.slow.last_mut().unwrap().resume = here;
        }
    }

    /// LSV, LLV, LDV, LQV and their stores at an even element, all bytes within the register (and LQV
    /// and SQV whole and aligned); false for the other forms. DMEM's bytes are big-endian, a register's
    /// lanes the host's halfwords.
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
        let offset = i64::from((word as i32) << 25 >> 25) * i64::from(size);
        self.get(R0, rs);
        self.e.add_signed(W, R0, R0, offset, IP0);
        self.e.ubfx(W, R0, R0, 0, 12);
        if size == 16 {
            // Aligned, or the interpreter (which stops at the line's end).
            self.e.tst_imm(W, R0, 15);
            let branch = self.e.b(Cond::Ne);
            self.slow.push(Slow { branch, word, pc, resume: 0 });
        } else {
            self.unless_wrapping(size, word, pc);
        }
        self.vector_register(vt);
        self.e.alu(Op::Add, X, R0, DMEM, R0);
        let e = e as i32;
        match size {
            2 if store => {
                self.e.ldrh(R1, R2, e);
                self.e.rev16(R1, R1);
                self.e.strh(R1, R0, 0);
            }
            2 => {
                self.e.ldrh(R1, R0, 0);
                self.e.rev16(R1, R1);
                self.e.strh(R1, R2, e);
            }
            4 if store => {
                self.e.ldr_byte_offset(W, R1, R2, e);
                self.e.rev16(R1, R1);
                self.e.str(W, R1, R0, 0);
            }
            4 => {
                self.e.ldr(W, R1, R0, 0);
                self.e.rev16(R1, R1);
                self.e.str_byte_offset(W, R1, R2, e);
            }
            _ => {
                for at in (0..size as i32).step_by(8) {
                    if store {
                        self.e.ldr_byte_offset(X, R1, R2, e + at);
                        self.e.rev16_x(R1, R1);
                        self.e.str_byte_offset(X, R1, R0, at);
                    } else {
                        self.e.ldr_byte_offset(X, R1, R0, at);
                        self.e.rev16_x(R1, R1);
                        self.e.str_byte_offset(X, R1, R2, e + at);
                    }
                }
            }
        }
        let here = self.e.here();
        self.slow.last_mut().unwrap().resume = here;
        true
    }

    /// MFC0 and MTC0 that only read or set a register; false for the others (the semaphore's read, DMA,
    /// status and the RDP's writes).
    fn cop0(&mut self, word: u32) -> bool {
        let (rt, rd) = ((word >> 16 & 31) as usize, word >> 11 & 15);
        if word >> 21 & 31 == 0 {
            match rd {
                0..=4 => self.e.ldr(W, R0, RSP, SP_REGISTERS[rd as usize]),
                5 | 6 | 12..=15 => self.e.mov_imm(W, R0, 0),
                8..=11 => {
                    self.e.imm(Op::Add, X, R1, MACHINE, DP_REGISTERS[rd as usize - 8], IP0);
                    self.e.ldr(W, R0, R1, 0);
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
                self.e.imm(Op::And, W, R0, R0, mask, IP0);
                self.e.str(W, R0, RSP, SP_REGISTERS[rd as usize]);
            }
            5 | 6 => {}
            7 => self.e.strb(ZR, RSP, SEMAPHORE),
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

    /// DEST is `target` if the flags say `cond`, else what it was.
    fn dest_if(&mut self, cond: Cond, target: u32) {
        self.e.mov_imm(W, R1, u64::from(target));
        self.e.csel(W, DEST, R1, DEST, cond);
    }

    /// Code for an instruction; true if the block ends after it.
    fn instruction(&mut self, word: u32, pc: u32) -> bool {
        let (rs, rt, rd) = ((word >> 21 & 31) as usize, (word >> 16 & 31) as usize, (word >> 11 & 31) as usize);
        let simm = word as i16 as i32;
        let uimm = word & 0xffff;
        let sa = word >> 6 & 31;
        let target = (pc + 4).wrapping_add((simm as u32) << 2) & 0xffc;
        let link = (pc + 8) & 0xfff;
        match word >> 26 {
            0x00 => match word & 63 {
                0x00 | 0x02 | 0x03 if rd != 0 => {
                    self.get(R0, rt);
                    if sa != 0 {
                        match word & 63 {
                            0x00 => self.e.lsl(W, R0, R0, sa),
                            0x02 => self.e.lsr(W, R0, R0, sa),
                            _ => self.e.asr(W, R0, R0, sa),
                        }
                    }
                    self.set(R0, rd);
                }
                0x04 | 0x06 | 0x07 if rd != 0 => {
                    self.get(R0, rt);
                    self.get(R1, rs);
                    match word & 63 {
                        0x04 => self.e.lslv(W, R0, R0, R1),
                        0x06 => self.e.lsrv(W, R0, R0, R1),
                        _ => self.e.asrv(W, R0, R0, R1),
                    }
                    self.set(R0, rd);
                }
                0x08 | 0x09 => {
                    self.get(DEST, rs);
                    self.e.ubfx(W, DEST, DEST, 2, 10);
                    self.e.lsl(W, DEST, DEST, 2);
                    if word & 63 == 0x09 && rd != 0 {
                        self.e.mov_imm(W, R0, u64::from(link));
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
                        0x20 | 0x21 => self.e.alu(Op::Add, W, R0, R0, R1),
                        0x22 | 0x23 => self.e.alu(Op::Sub, W, R0, R0, R1),
                        0x24 => self.e.alu(Op::And, W, R0, R0, R1),
                        0x25 => self.e.alu(Op::Orr, W, R0, R0, R1),
                        0x26 => self.e.alu(Op::Eor, W, R0, R0, R1),
                        0x27 => {
                            self.e.alu(Op::Orr, W, R0, R0, R1);
                            self.e.alu(Op::Orn, W, R0, ZR, R0);
                        }
                        funct => {
                            self.e.cmp(W, R0, R1);
                            self.e.cset(W, R0, if funct == 0x2a { Cond::Lt } else { Cond::Lo });
                        }
                    }
                    self.set(R0, rd);
                }
                _ => {}
            },
            0x01 => {
                self.get(R0, rs);
                self.e.mov_imm(W, DEST, u64::from((pc + 8) & 0xffc));
                let cond = match rt & 0xf {
                    0 => Some(Cond::Lt),
                    1 => Some(Cond::Ge),
                    _ => None,
                };
                if let Some(cond) = cond {
                    self.e.cmp_imm(W, R0, 0);
                    self.dest_if(cond, target);
                }
                // BLTZAL and BGEZAL link whether or not they branch.
                if rt & 0x10 != 0 {
                    self.e.mov_imm(W, R0, u64::from(link));
                    self.set(R0, 31);
                }
            }
            0x02 | 0x03 => {
                self.e.mov_imm(W, DEST, u64::from((word & 0x3ff) << 2));
                if word >> 26 == 0x03 {
                    self.e.mov_imm(W, R0, u64::from(link));
                    self.set(R0, 31);
                }
            }
            op @ 0x04..=0x07 => {
                self.get(R0, rs);
                self.e.mov_imm(W, DEST, u64::from((pc + 8) & 0xffc));
                if op <= 0x05 {
                    self.get(R1, rt);
                    self.e.cmp(W, R0, R1);
                } else {
                    self.e.cmp_imm(W, R0, 0);
                }
                let cond = [Cond::Eq, Cond::Ne, Cond::Le, Cond::Gt][op as usize - 4];
                self.dest_if(cond, target);
            }
            op @ 0x08..=0x0e if rt != 0 => {
                self.get(R0, rs);
                match op {
                    0x08 | 0x09 => self.e.add_signed(W, R0, R0, i64::from(simm), IP0),
                    0x0a | 0x0b => {
                        self.e.mov_imm(W, R1, simm as u32 as u64);
                        self.e.cmp(W, R0, R1);
                        self.e.cset(W, R0, if op == 0x0a { Cond::Lt } else { Cond::Lo });
                    }
                    _ => {
                        let logic = [Op::And, Op::Orr, Op::Eor][op as usize - 0x0c];
                        self.e.imm(logic, W, R0, R0, u64::from(uimm), IP0);
                    }
                }
                self.set(R0, rt);
            }
            0x0f if rt != 0 => {
                self.e.mov_imm(W, R0, u64::from(uimm << 16));
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
                self.e.imm(Op::Add, X, R0, RSP, VU, IP0);
                self.e.mov_imm(W, R1, u64::from(word));
                // The instruction's function's own entry where it has one (the same call as
                // calls.vector's, without its dispatch).
                self.e.mov_imm(X, IP0, crate::vu::entry(word).map_or(self.calls.vector, |f| f as usize) as u64);
                self.e.blr(IP0);
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
                self.e.add_signed(W, R0, R0, i64::from(simm), IP0);
                self.e.ubfx(W, R0, R0, 0, 12);
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
                    0x20 => {
                        self.e.ldrsb_reg(R1, DMEM, R0);
                    }
                    0x24 => self.e.ldrb_reg(R1, DMEM, R0),
                    0x21 | 0x25 => {
                        self.e.ldrh_reg(R1, DMEM, R0);
                        self.e.rev16(R1, R1);
                        if op == 0x21 {
                            self.e.sbfx(W, R1, R1, 0, 16);
                        }
                    }
                    0x23 | 0x27 => {
                        self.e.ldr_reg(W, R1, DMEM, R0);
                        self.e.rev(W, R1, R1);
                    }
                    0x28 => self.e.strb_reg(R1, DMEM, R0),
                    0x29 => {
                        self.e.rev16(R1, R1);
                        self.e.strh_reg(R1, DMEM, R0);
                    }
                    _ => {
                        self.e.rev(W, R1, R1);
                        self.e.str_reg(W, R1, DMEM, R0);
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

    /// Goes on into the block at `pc` (or the branch's destination) for the pipeline's state now, if the
    /// burst is not over and the table of links has it for IMEM as it is; falls through to leaving
    /// otherwise.
    fn go_on(&mut self, pc: Option<u32>) {
        let k = self.k;
        let variant = variant(self.pairs, self.vector, self.after) as u64;
        let mut skips = Vec::new();
        self.cmp_k(BUDGET, k);
        skips.push(self.e.b(Cond::Le));
        let links = self.calls.links as u64;
        match pc {
            Some(pc) => self.e.mov_imm(X, IP1, links + (u64::from(pc >> 2) * 8 + variant) * LINK),
            None => {
                // DEST is a word's address: its slot's entry at DEST * 2 * LINK.
                self.e.mov_imm(X, IP1, links + variant * LINK);
                self.e.alu_shifted(Op::Add, X, IP1, IP1, DEST, Shift::Lsl((2 * LINK).trailing_zeros()));
            }
        }
        self.e.ldr(W, R0, IP1, 8);
        self.e.ldr(W, R1, RSP, IMEM_WRITES);
        self.e.cmp(W, R0, R1);
        skips.push(self.e.b(Cond::Ne));
        self.e.ldr(X, R2, IP1, 0);
        skips.push(self.e.cbz(false, X, R2));
        self.e.imm(Op::Add, W, RAN, RAN, u64::from(self.count), IP0);
        // The clock's base moves to the next block's start.
        self.base_plus(BASE, k);
        self.e.add_signed(W, BUDGET, BUDGET, -i64::from(k), IP0);
        self.e.br(R2);
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
        self.e.mov(W, R0, DEST);
        assert!(self.e.alu_imm(Op::Add, W, R1, DEST, 4));
        assert!(self.e.alu_imm(Op::And, W, R1, R1, !0x1000 & 0xffff_ffff));
        self.common_exit(self.k, self.flags(), self.count);
    }

    fn leave(&mut self, exit: Exit) {
        self.e.mov_imm(W, R0, u64::from(exit.pc));
        match exit.next {
            Some(next) => self.e.mov_imm(W, R1, u64::from(next)),
            None => self.e.mov(W, R1, DEST),
        }
        self.common_exit(exit.k, exit.flags, exit.count);
    }

    /// w0 the pc, w1 the next.
    fn common_exit(&mut self, k: i32, flags: u32, count: u32) {
        self.e.str(W, R0, RSP, PC_AT);
        self.e.str(W, R1, RSP, NEXT_PC);
        self.base_plus(R0, k);
        self.e.str(X, R0, RSP, CLOCK);
        self.e.mov_imm(W, R0, u64::from(flags & 0xff));
        self.e.strb(R0, RSP, PAIRS);
        self.e.mov_imm(W, R0, u64::from(flags >> 8 & 0xff));
        self.e.strb(R0, RSP, VECTOR);
        self.e.mov_imm(W, R0, u64::from(flags >> 16 & 0xff));
        self.e.strb(R0, RSP, AFTER);
        self.e.imm(Op::Add, W, R0, RAN, u64::from(count), IP0);
        self.ends.push(self.e.b(Cond::Al));
    }

    /// The code out of line: slice exits and wrapping DMEM accesses, and the block's end.
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
        for end in std::mem::take(&mut self.ends) {
            self.e.patch_here(end);
        }
        for (a, b, at) in SAVED {
            self.e.ldp(a, b, SP, at, false);
        }
        self.e.ldp(FP, LR, SP, FRAME, true);
        self.e.ret();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_word_compiles_or_declines() {
        // Each opcode and function field, with branches' slots and registers spread, at both ends of
        // IMEM.
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
