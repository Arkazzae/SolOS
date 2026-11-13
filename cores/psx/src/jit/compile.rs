//! Translates a block of R3000A instructions to ARM. The load delay is
//! worked out as the block is compiled: a loaded value waits in a register
//! (PENDING) and is stored to its MIPS register after the next instruction,
//! unless that one writes the register itself; a load still on its way
//! when the block ends is left where the interpreter keeps one.
//!
//! Instructions not translated end the block before them, and the
//! interpreter runs them.
//!
//! What seldom runs (calls into Rust for what is not RAM, exceptions, code
//! written over) goes out of line, after the block's epilogue, so that the
//! code that runs is together in the instruction cache.
//!
//! The scratch registers R0-R3 are remembered holding a MIPS register's
//! value after it was read into or stored from them, until anything
//! writes them: the next instruction reading that register takes it from
//! there instead of memory (in-order cores wait for a load's result).

use super::arm::*;
use crate::Psx;
use crate::cpu::Cpu;
use std::mem::offset_of;

// The registers compiled code keeps things in.
const PSX: Reg = 10;
const RAM: Reg = 9;
const CYC: Reg = 8;
const PAGES: Reg = 7;
const PENDING: Reg = 6;
const SPARE: Reg = 5;
const TARGET: Reg = 4;
const BLOCKS: Reg = 11;

/// The words of a block's prologue: a block chained to starts after it.
pub const PROLOGUE: u32 = 6;

/// The stack pointer, and where the scratchpad's address (the fifth
/// argument) is on the stack once the prologue has pushed ten registers;
/// chained blocks keep the first one's frame.
const SP: Reg = 13;
const SCRATCHPAD: i32 = 40;

/// The most instructions a block holds.
const MOST: u32 = 64;

const fn cpu(field: usize) -> i32 {
    (offset_of!(Psx, cpu) + field) as i32
}

fn reg(n: u32) -> i32 {
    cpu(offset_of!(Cpu, r)) + 4 * n as i32
}

const HI: i32 = cpu(offset_of!(Cpu, hi));
const LO: i32 = cpu(offset_of!(Cpu, lo));
const PC_: i32 = cpu(offset_of!(Cpu, pc));
const NEXT_PC: i32 = cpu(offset_of!(Cpu, next_pc));
const LOAD_REG: i32 = cpu(offset_of!(Cpu, load) + offset_of!((usize, u32), 0));
const LOAD_VALUE: i32 = cpu(offset_of!(Cpu, load) + offset_of!((usize, u32), 1));
const CYCLES: i32 = offset_of!(Psx, cycles) as i32;
const SR: i32 = cpu(offset_of!(Cpu, sr));
const CAUSE: i32 = cpu(offset_of!(Cpu, cause));
const NEXT_EVENT: i32 = offset_of!(Psx, next_event) as i32;

// Exception codes, as the CPU's.
const ADDRESS_LOAD: u32 = 4;
const ADDRESS_STORE: u32 = 5;
const SYSCALL: u32 = 8;
const BREAK: u32 = 9;
const OVERFLOW: u32 = 12;

/// What an instruction does to the registers, for the load delay.
#[derive(Clone, Copy, PartialEq)]
enum Effect {
    None,
    Writes(u32),
    Loads(u32),
}

struct Compiler {
    e: Emitter,
    /// The MIPS register a loaded value in PENDING is on its way to.
    pending: Option<u32>,
    /// Instructions compiled so far: the cycles a block that ends here took.
    count: u32,
    /// Branches to the epilogue, to patch.
    exits: Vec<usize>,
    /// The instruction compiled last may let an interrupt through (MTC0,
    /// RFE): the block ends after it.
    end_after: bool,
    /// Where the code memory starts, for chaining to blocks in it.
    code_base: u32,
    /// The MIPS register R0-R3 each hold the value of, if the emitter has
    /// not written them since.
    known: [Option<u32>; 4],
    /// The code out of line, and whether it is being emitted (into `e`,
    /// swapped with its emitter).
    cold: Cold,
    in_cold: bool,
}

/// Code out of line: its words, the branches from the block into it and
/// from it back to the block (each with its target), and its exits.
#[derive(Default)]
struct Cold {
    e: Emitter,
    into: Vec<(usize, usize)>,
    back: Vec<(usize, usize)>,
    exits: Vec<usize>,
}

/// What to go back to if code is taken out: the scratch registers known
/// and written, and how far the code out of line had got.
type Mark = ([Option<u32>; 4], u32, [usize; 4]);

/// The block at `pc` as ARM words, for code memory starting at
/// `code_base`; None if its first instruction is left to the interpreter.
pub fn block(psx: &Psx, pc: u32, code_base: u32) -> Option<Vec<u32>> {
    let mut c = Compiler {
        e: Emitter::default(),
        pending: None,
        count: 0,
        exits: Vec::new(),
        end_after: false,
        code_base,
        known: [None; 4],
        cold: Cold::default(),
        in_cold: false,
    };
    // Prologue: the callee-saved registers (ten, keeping the stack aligned),
    // then the machine, RAM, the code pages, the table of blocks and the
    // cycle count.
    c.e.push(0x4ff8);
    c.e.mov(PSX, R0);
    c.e.mov(RAM, R1);
    c.e.mov(PAGES, R2);
    c.e.mov(BLOCKS, R3);
    c.e.mov_imm(CYC, 0);
    assert_eq!(c.e.here() as u32, PROLOGUE);
    let page = pc >> 12;
    let mut at = pc;
    loop {
        if c.count >= MOST || at >> 12 != page {
            c.end(at);
            break;
        }
        let op = psx.peek32(at);
        if is_branch(op) {
            // The branch and its delay slot go together, or neither does.
            let (mark, pending, count, exits, known) = (c.e.here(), c.pending, c.count, c.exits.len(), c.remembered());
            let slot = psx.peek32(at.wrapping_add(4));
            let fits = (at + 4) >> 12 == page && !is_branch(slot) && !is_terminal(slot);
            if fits && c.branch(op, at) && c.instruction(slot, at + 4, true) {
                c.end_branch();
                break;
            }
            c.e.code.truncate(mark);
            c.exits.truncate(exits);
            (c.pending, c.count) = (pending, count);
            c.remember(known);
            if c.count == 0 {
                return None;
            }
            c.end(at);
            break;
        }
        if is_terminal(op) {
            if !c.terminal(op, at) {
                if c.count == 0 {
                    return None;
                }
                c.end(at);
            }
            break;
        }
        if !c.instruction(op, at, false) {
            if c.count == 0 {
                return None;
            }
            c.end(at);
            break;
        }
        at += 4;
        if std::mem::take(&mut c.end_after) {
            c.end(at);
            break;
        }
    }
    // Epilogue: the cycles into the machine's count, and back. (A chained
    // block's prologue was skipped, so its registers are popped here once.)
    let exit = c.e.here();
    for b in std::mem::take(&mut c.exits) {
        c.e.patch(b, exit);
    }
    c.e.ldr(R0, PSX, CYCLES);
    c.e.ldr(R1, PSX, CYCLES + 4);
    c.e.alu_shifted(Cond::Al, Op::Add, true, R0, R0, CYC, Shift::Lsl(0));
    c.e.alu_imm(Cond::Al, Op::Adc, false, R1, R1, 0);
    c.e.str(R0, PSX, CYCLES);
    c.e.str(R1, PSX, CYCLES + 4);
    c.e.pop(0x8ff8);
    // The code out of line after it.
    let (base, cold) = (c.e.here(), std::mem::take(&mut c.cold));
    c.e.code.extend_from_slice(&cold.e.code);
    for (b, target) in cold.into {
        c.e.patch(b, base + target);
    }
    for (b, target) in cold.back {
        c.e.patch(base + b, target);
    }
    for b in cold.exits {
        c.e.patch(base + b, exit);
    }
    Some(c.e.code)
}

pub(super) fn is_branch(op: u32) -> bool {
    match op >> 26 {
        0x00 => matches!(op & 0x3f, 0x08 | 0x09),
        0x01..=0x07 => true,
        _ => false,
    }
}

/// SYSCALL and BREAK: always an exception.
pub(super) fn is_terminal(op: u32) -> bool {
    op >> 26 == 0 && matches!(op & 0x3f, 0x0c | 0x0d)
}

impl Compiler {
    /// The scratch register holding MIPS register `n`'s value, `arm`
    /// first.
    fn holding(&self, n: u32, arm: Reg) -> Option<Reg> {
        let holds = |r: Reg| self.known[r as usize] == Some(n) && self.e.written & 1 << r == 0;
        if arm < 4 && holds(arm) { Some(arm) } else { (0..4).find(|&r| holds(r)) }
    }

    /// `arm` holds MIPS register `n`'s value as of now.
    fn know(&mut self, arm: Reg, n: u32) {
        if arm < 4 {
            self.known[arm as usize] = Some(n);
            self.e.written &= !(1 << arm);
        }
    }

    /// MIPS register `n` changes in memory: what holds its old value does
    /// not count.
    fn forget(&mut self, n: u32) {
        for k in &mut self.known {
            if *k == Some(n) {
                *k = None;
            }
        }
    }

    /// What to go back to if code is taken out.
    fn remembered(&self) -> Mark {
        let c = &self.cold;
        (self.known, self.e.written, [c.e.here(), c.into.len(), c.back.len(), c.exits.len()])
    }

    fn remember(&mut self, (known, written, cold): Mark) {
        (self.known, self.e.written) = (known, written);
        let c = &mut self.cold;
        c.e.code.truncate(cold[0]);
        c.into.truncate(cold[1]);
        c.back.truncate(cold[2]);
        c.exits.truncate(cold[3]);
    }

    /// Emits code out of line, for the branch `from` (in the block) to go
    /// to.
    fn out_of_line(&mut self, from: usize, f: impl FnOnce(&mut Compiler)) {
        std::mem::swap(&mut self.e, &mut self.cold.e);
        let start = self.e.here();
        self.in_cold = true;
        f(self);
        self.in_cold = false;
        std::mem::swap(&mut self.e, &mut self.cold.e);
        self.cold.into.push((from, start));
    }

    /// From code out of line back into the block at `to`, which then
    /// cannot know what a call there may have changed.
    fn back_to(&mut self, to: usize) {
        let b = self.e.b(Cond::Al);
        self.cold.back.push((b, to));
    }

    fn load_reg(&mut self, arm: Reg, n: u32) {
        match self.holding(n, arm) {
            Some(r) if r == arm => {}
            Some(r) => self.e.mov(arm, r),
            None if n == 0 => self.e.mov_imm(arm, 0),
            None => self.e.ldr(arm, PSX, reg(n)),
        }
        self.know(arm, n);
    }

    fn store_reg(&mut self, arm: Reg, n: u32) {
        if n != 0 {
            self.e.str(arm, PSX, reg(n));
            self.forget(n);
            self.know(arm, n);
        }
    }

    /// The load on its way lands in its register.
    fn land(&mut self, p: u32) {
        self.e.str(PENDING, PSX, reg(p));
        self.forget(p);
    }

    fn call(&mut self, f: usize) {
        self.e.mov_imm(R12, f as u32);
        self.e.blx(R12);
    }

    /// The load on its way lands now (before an exit or an exception).
    fn land_pending(&mut self) {
        if let Some(p) = self.pending {
            self.land(p);
        }
    }

    /// Exits the block with these cycles counted.
    fn exit(&mut self, cycles: u32) {
        self.e.alu_imm(Cond::Al, Op::Add, false, CYC, CYC, cycles);
        let b = self.e.b(Cond::Al);
        if self.in_cold { self.cold.exits.push(b) } else { self.exits.push(b) }
    }

    /// The block ends before `at`, which runs next.
    fn end(&mut self, at: u32) {
        self.e.mov_imm(TARGET, at);
        self.end_branch();
    }

    /// A load still on its way is left for the interpreter.
    fn leave_pending(&mut self) {
        if let Some(p) = self.pending.take() {
            self.e.str(PENDING, PSX, LOAD_VALUE);
            self.e.mov_imm(R0, p);
            self.e.str(R0, PSX, LOAD_REG);
        }
    }

    /// On at TARGET: straight into its compiled block if there is one and
    /// nothing is due (no event, no interrupt), else back to the
    /// dispatcher.
    fn end_branch(&mut self) {
        if self.pending.is_none() {
            self.chain();
        }
        self.leave_pending();
        self.e.str(TARGET, PSX, PC_);
        self.e.add_imm(R0, TARGET, 4, R1);
        self.e.str(R0, PSX, NEXT_PC);
        self.exit(0);
    }

    /// The block's cycles go to the machine's count; then, if nothing is
    /// due and TARGET's block is compiled, on into it past its prologue.
    /// Falls through when it cannot.
    fn chain(&mut self) {
        let e = &mut self.e;
        e.ldr(R0, PSX, CYCLES);
        e.ldr(R1, PSX, CYCLES + 4);
        e.alu_imm(Cond::Al, Op::Add, false, CYC, CYC, self.count);
        e.alu_shifted(Cond::Al, Op::Add, true, R0, R0, CYC, Shift::Lsl(0));
        e.alu_imm(Cond::Al, Op::Adc, false, R1, R1, 0);
        e.str(R0, PSX, CYCLES);
        e.str(R1, PSX, CYCLES + 4);
        e.mov_imm(CYC, 0);
        self.count = 0;
        let mut out = Vec::new();
        // An event due: cycles >= next_event.
        e.ldr(R2, PSX, NEXT_EVENT);
        e.ldr(R3, PSX, NEXT_EVENT + 4);
        e.alu_shifted(Cond::Al, Op::Cmp, true, 0, R0, R2, Shift::Lsl(0));
        e.alu_shifted(Cond::Al, Op::Sbc, true, R12, R1, R3, Shift::Lsl(0));
        out.push(e.b(Cond::Hs));
        // An interrupt let through, or the cache isolated.
        e.ldr(R0, PSX, SR);
        e.ldr(R1, PSX, CAUSE);
        e.alu_imm(Cond::Al, Op::Tst, true, 0, R0, 0x1_0000);
        out.push(e.b(Cond::Ne));
        e.alu(Op::And, R1, R1, R0);
        e.alu_imm(Cond::Al, Op::And, false, R1, R1, 0xff00);
        e.alu_imm(Cond::Al, Op::Tst, true, 0, R0, 1);
        let no_irq = e.b(Cond::Eq);
        e.alu_imm(Cond::Al, Op::Cmp, true, 0, R1, 0);
        out.push(e.b(Cond::Ne));
        e.patch_here(no_irq);
        // TARGET in RAM past the BIOS's vectors, a word, compiled.
        e.alu_imm(Cond::Al, Op::Bic, false, R0, TARGET, 0xe000_0000);
        e.alu_imm(Cond::Al, Op::Cmp, true, 0, R0, 0x80_0000);
        out.push(e.b(Cond::Hs));
        e.alu_imm(Cond::Al, Op::Cmp, true, 0, R0, 0x100);
        out.push(e.b(Cond::Lo));
        e.alu_imm(Cond::Al, Op::Tst, true, 0, R0, 3);
        out.push(e.b(Cond::Ne));
        e.alu_imm(Cond::Al, Op::Bic, false, R0, R0, 0x60_0000);
        e.ldr_reg(R0, BLOCKS, R0);
        e.alu_imm(Cond::Al, Op::Cmp, true, 0, R0, 0);
        out.push(e.b(Cond::Eq));
        // Nor NONE or WAIT, the two last values.
        e.alu_imm(Cond::Al, Op::Cmn, true, 0, R0, 2);
        out.push(e.b(Cond::Hs));
        // Code at code_base + 4 * (entry - 1 + PROLOGUE).
        e.mov_imm(R1, self.code_base.wrapping_add(4 * (PROLOGUE - 1)));
        e.alu_shifted(Cond::Al, Op::Add, false, R0, R1, R0, Shift::Lsl(2));
        e.bx(R0);
        for b in out {
            e.patch_here(b);
        }
    }

    /// An exception at the instruction at `pc` (the load before it having
    /// landed): the CPU goes to its handler and the block ends.
    fn exception_stub(&mut self, code: u32, pc: u32, delay: bool, bad_vaddr: Reg) {
        self.land_pending();
        if bad_vaddr != R3 {
            self.e.mov(R3, bad_vaddr);
        }
        self.e.mov_imm(R1, code | u32::from(delay) << 8);
        self.e.mov_imm(R2, pc);
        self.e.mov(R0, PSX);
        self.call(super::exception as usize);
        self.exit(self.count + 1);
    }

    /// An instruction and its part in the load delay; false if it is left
    /// to the interpreter (nothing emitted).
    fn instruction(&mut self, op: u32, pc: u32, delay: bool) -> bool {
        let (mark, exits, known) = (self.e.here(), self.exits.len(), self.remembered());
        let Some(effect) = self.op(op, pc, delay) else {
            self.e.code.truncate(mark);
            self.exits.truncate(exits);
            self.remember(known);
            return false;
        };
        if let Some(p) = self.pending.take() {
            let superseded = matches!(effect, Effect::Writes(r) | Effect::Loads(r) if r == p);
            if !superseded {
                self.land(p);
            }
        }
        if let Effect::Loads(r) = effect
            && r != 0
        {
            self.e.mov(PENDING, SPARE);
            self.pending = Some(r);
        }
        self.count += 1;
        true
    }

    /// SYSCALL or BREAK, which end the block.
    fn terminal(&mut self, op: u32, pc: u32) -> bool {
        let code = if op & 0x3f == 0x0c { SYSCALL } else { BREAK };
        self.e.mov_imm(R3, 0);
        self.exception_stub(code, pc, false, R3);
        self.pending = None;
        true
    }

    /// A branch or jump: its target (or the address after its delay slot)
    /// into TARGET, its link written.
    fn branch(&mut self, op: u32, pc: u32) -> bool {
        let (rs, rt, rd) = (op >> 21 & 31, op >> 16 & 31, op >> 11 & 31);
        let offset = ((op & 0xffff) as i16 as i32 as u32) << 2;
        let taken = pc.wrapping_add(4).wrapping_add(offset);
        let after = pc.wrapping_add(8);
        let mut link = None;
        let cond = match op >> 26 {
            0x00 => {
                // JR, JALR.
                self.load_reg(TARGET, rs);
                if op & 0x3f == 0x09 {
                    link = Some(rd);
                }
                None
            }
            0x02 | 0x03 => {
                self.e.mov_imm(TARGET, (pc.wrapping_add(4) & 0xf000_0000) | (op & 0x03ff_ffff) << 2);
                if op >> 26 == 0x03 {
                    link = Some(31);
                }
                None
            }
            0x04 | 0x05 => {
                self.load_reg(R0, rs);
                self.load_reg(R1, rt);
                self.e.alu_shifted(Cond::Al, Op::Cmp, true, 0, R0, R1, Shift::Lsl(0));
                Some(if op >> 26 == 0x04 { Cond::Eq } else { Cond::Ne })
            }
            0x06 | 0x07 => {
                self.load_reg(R0, rs);
                self.e.alu_imm(Cond::Al, Op::Cmp, true, 0, R0, 0);
                Some(if op >> 26 == 0x06 { Cond::Le } else { Cond::Gt })
            }
            0x01 => {
                self.load_reg(R0, rs);
                self.e.alu_imm(Cond::Al, Op::Cmp, true, 0, R0, 0);
                if rt & 0x1e == 0x10 {
                    link = Some(31);
                }
                Some(if rt & 1 != 0 { Cond::Ge } else { Cond::Lt })
            }
            _ => return false,
        };
        if let Some(cond) = cond {
            self.e.mov_imm(TARGET, after);
            self.e.movw(cond, TARGET, taken & 0xffff);
            self.e.movt(cond, TARGET, taken >> 16);
        }
        // The link is written as the branch runs; a load landing in it
        // loses.
        if let Some(r) = link {
            self.e.mov_imm(R0, after);
            self.store_reg(R0, r);
        }
        if let Some(p) = self.pending.take()
            && link != Some(p)
        {
            self.land(p);
        }
        self.count += 1;
        true
    }

    /// The instruction's code; None if it is left to the interpreter.
    fn op(&mut self, op: u32, pc: u32, delay: bool) -> Option<Effect> {
        let (rs, rt) = (op >> 21 & 31, op >> 16 & 31);
        let imm = op & 0xffff;
        let simm = imm as i16 as i32 as u32;
        let e = match op >> 26 {
            0x00 => return self.special(op, pc, delay),
            0x08 | 0x09 => {
                // ADDI (overflow traps), ADDIU.
                self.load_reg(R0, rs);
                if op >> 26 == 0x08 {
                    self.e.mov_imm(R1, simm);
                    self.e.alu_shifted(Cond::Al, Op::Add, true, R0, R0, R1, Shift::Lsl(0));
                    self.overflow_check(pc, delay);
                } else {
                    self.e.add_imm(R0, R0, simm, R1);
                }
                self.store_reg(R0, rt);
                Effect::Writes(rt)
            }
            0x0a | 0x0b => {
                // SLTI, SLTIU.
                self.load_reg(R0, rs);
                self.e.mov_imm(R1, simm);
                self.e.alu_shifted(Cond::Al, Op::Cmp, true, 0, R0, R1, Shift::Lsl(0));
                let (yes, no) = if op >> 26 == 0x0a { (Cond::Lt, Cond::Ge) } else { (Cond::Lo, Cond::Hs) };
                self.e.alu_imm(yes, Op::Mov, false, R0, 0, 1);
                self.e.alu_imm(no, Op::Mov, false, R0, 0, 0);
                self.store_reg(R0, rt);
                Effect::Writes(rt)
            }
            0x0c..=0x0e => {
                // ANDI, ORI, XORI: the immediate zero-extended.
                self.load_reg(R0, rs);
                let o = [Op::And, Op::Orr, Op::Eor][(op >> 26) as usize - 0x0c];
                if !self.e.alu_imm(Cond::Al, o, false, R0, R0, imm) {
                    self.e.mov_imm(R1, imm);
                    self.e.alu(o, R0, R0, R1);
                }
                self.store_reg(R0, rt);
                Effect::Writes(rt)
            }
            0x0f => {
                self.e.mov_imm(R0, imm << 16);
                self.store_reg(R0, rt);
                Effect::Writes(rt)
            }
            0x20 | 0x21 | 0x23 | 0x24 | 0x25 => {
                self.load(op >> 26, rs, simm, pc, delay);
                Effect::Loads(rt)
            }
            0x28 | 0x29 | 0x2b => {
                self.store(op >> 26, rs, Some(rt), simm, pc, delay);
                Effect::None
            }
            0x10 => return self.cop0(op),
            0x12 => {
                self.cop2_usable(pc, delay);
                let rd = op >> 11 & 31;
                if op & 1 << 25 != 0 {
                    self.e.mov_imm(R1, op);
                    self.e.mov(R0, PSX);
                    self.call(super::gte_command as usize);
                    Effect::None
                } else {
                    match rs {
                        0x00 | 0x02 => {
                            // MFC2, CFC2: loads, with their delay.
                            self.e.mov_imm(R1, rd + if rs == 0x02 { 32 } else { 0 });
                            self.e.mov(R0, PSX);
                            self.call(super::gte_read as usize);
                            self.e.mov(SPARE, R0);
                            Effect::Loads(rt)
                        }
                        0x04 | 0x06 => {
                            self.load_reg(R2, rt);
                            self.e.mov_imm(R1, rd + if rs == 0x06 { 32 } else { 0 });
                            self.e.mov(R0, PSX);
                            self.call(super::gte_write as usize);
                            Effect::None
                        }
                        _ => return None,
                    }
                }
            }
            0x22 | 0x26 => {
                // LWL, LWR: merged with the register as a load on its way
                // to it leaves it.
                self.address(rs, simm);
                if self.pending == Some(rt) {
                    self.e.mov(R2, PENDING);
                } else {
                    self.load_reg(R2, rt);
                }
                self.e.mov(R1, R0);
                self.e.mov_imm(R3, u32::from(op >> 26 == 0x22));
                self.e.mov(R0, PSX);
                self.call(super::load_unaligned as usize);
                self.e.mov(SPARE, R0);
                Effect::Loads(rt)
            }
            0x2a | 0x2e => {
                // SWL, SWR.
                self.address(rs, simm);
                self.load_reg(R2, rt);
                self.e.mov(R1, R0);
                self.e.mov_imm(R3, u32::from(op >> 26 == 0x2a));
                self.e.mov(R0, PSX);
                self.call(super::store_unaligned as usize);
                Effect::None
            }
            0x32 => {
                // LWC2: a word from memory into a GTE register.
                self.cop2_usable(pc, delay);
                self.load(0x23, rs, simm, pc, delay);
                self.e.mov(R2, SPARE);
                self.e.mov_imm(R1, rt);
                self.e.mov(R0, PSX);
                self.call(super::gte_write as usize);
                Effect::None
            }
            0x3a => {
                // SWC2: a GTE register to memory.
                self.cop2_usable(pc, delay);
                self.e.mov_imm(R1, rt);
                self.e.mov(R0, PSX);
                self.call(super::gte_read as usize);
                self.e.mov(SPARE, R0);
                self.store(0x2b, rs, None, simm, pc, delay);
                Effect::None
            }
            _ => return None,
        };
        Some(e)
    }

    /// MFC0 of a register that exists, MTC0 and RFE (which end the block);
    /// the rest is the interpreter's.
    fn cop0(&mut self, op: u32) -> Option<Effect> {
        let (rs, rt, rd) = (op >> 21 & 31, op >> 16 & 31, op >> 11 & 31);
        match rs {
            0x00 if matches!(rd, 3 | 5..=9 | 11..=15) => {
                self.e.mov_imm(R1, rd);
                self.e.mov(R0, PSX);
                self.call(super::mfc0 as usize);
                self.e.mov(SPARE, R0);
                Some(Effect::Loads(rt))
            }
            0x04 => {
                self.load_reg(R2, rt);
                self.e.mov_imm(R1, rd);
                self.e.mov(R0, PSX);
                self.call(super::mtc0 as usize);
                self.end_after = true;
                Some(Effect::None)
            }
            0x10..=0x1f if op & 0x3f == 0x10 => {
                self.e.mov(R0, PSX);
                self.call(super::rfe as usize);
                self.end_after = true;
                Some(Effect::None)
            }
            _ => None,
        }
    }

    /// COP2 instructions trap unless SR enables the GTE.
    fn cop2_usable(&mut self, pc: u32, delay: bool) {
        self.e.ldr(R0, PSX, SR);
        self.e.alu_imm(Cond::Al, Op::Tst, true, 0, R0, 0x4000_0000);
        let unusable = self.e.b(Cond::Eq);
        self.out_of_line(unusable, |c| {
            c.land_pending();
            c.e.mov_imm(R2, pc);
            c.e.mov_imm(R1, u32::from(delay));
            c.e.mov(R0, PSX);
            c.call(super::cop2_unusable as usize);
            c.exit(c.count + 1);
        });
    }

    fn special(&mut self, op: u32, pc: u32, delay: bool) -> Option<Effect> {
        let (rs, rt, rd, sa) = (op >> 21 & 31, op >> 16 & 31, op >> 11 & 31, op >> 6 & 31);
        let e = match op & 0x3f {
            0x00 | 0x02 | 0x03 => {
                // SLL, SRL, SRA by an immediate (NOP among them).
                if rd != 0 {
                    self.load_reg(R0, rt);
                    if sa != 0 {
                        let shift = match op & 0x3f {
                            0x00 => Shift::Lsl(sa),
                            0x02 => Shift::Lsr(sa),
                            _ => Shift::Asr(sa),
                        };
                        self.e.alu_shifted(Cond::Al, Op::Mov, false, R0, 0, R0, shift);
                    }
                    self.store_reg(R0, rd);
                }
                Effect::Writes(rd)
            }
            0x04 | 0x06 | 0x07 => {
                // SLLV, SRLV, SRAV: by the low five bits of rs.
                self.load_reg(R0, rt);
                self.load_reg(R1, rs);
                self.e.alu_imm(Cond::Al, Op::And, false, R1, R1, 31);
                let shift = match op & 0x3f {
                    0x04 => Shift::LslReg(R1),
                    0x06 => Shift::LsrReg(R1),
                    _ => Shift::AsrReg(R1),
                };
                self.e.alu_shifted(Cond::Al, Op::Mov, false, R0, 0, R0, shift);
                self.store_reg(R0, rd);
                Effect::Writes(rd)
            }
            0x10 | 0x12 => {
                // MFHI, MFLO.
                self.e.ldr(R0, PSX, if op & 0x3f == 0x10 { HI } else { LO });
                self.store_reg(R0, rd);
                Effect::Writes(rd)
            }
            0x11 | 0x13 => {
                // MTHI, MTLO.
                self.load_reg(R0, rs);
                self.e.str(R0, PSX, if op & 0x3f == 0x11 { HI } else { LO });
                Effect::None
            }
            0x18 | 0x19 => {
                // MULT, MULTU.
                self.load_reg(R2, rs);
                self.load_reg(R3, rt);
                self.e.mull(op & 0x3f == 0x18, R0, R1, R2, R3);
                self.e.str(R0, PSX, LO);
                self.e.str(R1, PSX, HI);
                Effect::None
            }
            0x1a | 0x1b => {
                // DIV, DIVU, with their edge cases, in Rust.
                self.load_reg(R1, rs);
                self.load_reg(R2, rt);
                self.e.mov(R0, PSX);
                let f = if op & 0x3f == 0x1a { super::div as usize } else { super::divu as usize };
                self.call(f);
                Effect::None
            }
            0x20..=0x27 => {
                self.load_reg(R0, rs);
                self.load_reg(R1, rt);
                match op & 0x3f {
                    0x20 | 0x22 => {
                        // ADD, SUB: overflow traps.
                        let o = if op & 0x3f == 0x20 { Op::Add } else { Op::Sub };
                        self.e.alu_shifted(Cond::Al, o, true, R0, R0, R1, Shift::Lsl(0));
                        self.overflow_check(pc, delay);
                    }
                    0x21 => self.e.alu(Op::Add, R0, R0, R1),
                    0x23 => self.e.alu(Op::Sub, R0, R0, R1),
                    0x24 => self.e.alu(Op::And, R0, R0, R1),
                    0x25 => self.e.alu(Op::Orr, R0, R0, R1),
                    0x26 => self.e.alu(Op::Eor, R0, R0, R1),
                    _ => {
                        self.e.alu(Op::Orr, R0, R0, R1);
                        self.e.alu(Op::Mvn, R0, 0, R0);
                    }
                }
                self.store_reg(R0, rd);
                Effect::Writes(rd)
            }
            0x2a | 0x2b => {
                // SLT, SLTU.
                self.load_reg(R0, rs);
                self.load_reg(R1, rt);
                self.e.alu_shifted(Cond::Al, Op::Cmp, true, 0, R0, R1, Shift::Lsl(0));
                let (yes, no) = if op & 0x3f == 0x2a { (Cond::Lt, Cond::Ge) } else { (Cond::Lo, Cond::Hs) };
                self.e.alu_imm(yes, Op::Mov, false, R0, 0, 1);
                self.e.alu_imm(no, Op::Mov, false, R0, 0, 0);
                self.store_reg(R0, rd);
                Effect::Writes(rd)
            }
            _ => return None,
        };
        Some(e)
    }

    /// After an ADD, ADDI or SUB that set the flags: an overflow traps.
    fn overflow_check(&mut self, pc: u32, delay: bool) {
        let overflow = self.e.b(Cond::Vs);
        self.out_of_line(overflow, |c| {
            c.e.mov_imm(R3, 0);
            c.exception_stub(OVERFLOW, pc, delay, R3);
        });
    }

    /// The address rs + offset into R0.
    fn address(&mut self, rs: u32, offset: u32) {
        self.load_reg(R0, rs);
        self.e.add_imm(R0, R0, offset, R1);
    }

    /// An access not aligned to its width is an address error.
    fn alignment_check(&mut self, width: u32, code: u32, pc: u32, delay: bool) {
        if width == 1 {
            return;
        }
        self.e.alu_imm(Cond::Al, Op::Tst, true, 0, R0, width - 1);
        let misaligned = self.e.b(Cond::Ne);
        self.out_of_line(misaligned, |c| c.exception_stub(code, pc, delay, R0));
    }

    /// For an access not to RAM (its physical address in `physical`): R3
    /// = the scratchpad and `offset` the place in it, else on to the
    /// branch returned (to patch).
    fn scratchpad(&mut self, physical: Reg, offset: Reg) -> usize {
        self.e.alu_imm(Cond::Al, Op::Sub, false, offset, physical, 0x1f80_0000);
        self.e.alu_imm(Cond::Al, Op::Cmp, true, 0, offset, 0x400);
        let elsewhere = self.e.b(Cond::Hs);
        self.e.ldr(R3, SP, SCRATCHPAD);
        elsewhere
    }

    /// R1 = the address's place in RAM; to `slow` (patched by the caller)
    /// if it is not in RAM.
    fn ram_index(&mut self) -> usize {
        self.e.alu_imm(Cond::Al, Op::Bic, false, R1, R0, 0xe000_0000);
        self.e.alu_imm(Cond::Al, Op::Cmp, true, 0, R1, 0x80_0000);
        let slow = self.e.b(Cond::Hs);
        self.e.alu_imm(Cond::Al, Op::Bic, false, R1, R1, 0x60_0000);
        slow
    }

    /// LB, LH, LW, LBU, LHU: the value into SPARE.
    fn load(&mut self, kind: u32, rs: u32, offset: u32, pc: u32, delay: bool) {
        self.address(rs, offset);
        let width = match kind {
            0x20 | 0x24 => 1,
            0x21 | 0x25 => 2,
            _ => 4,
        };
        self.alignment_check(width, ADDRESS_LOAD, pc, delay);
        let slow = self.ram_index();
        match kind {
            0x20 => self.e.ldrsb_reg(SPARE, RAM, R1),
            0x21 => self.e.ldrsh_reg(SPARE, RAM, R1),
            0x23 => self.e.ldr_reg(SPARE, RAM, R1),
            0x24 => self.e.ldrb_reg(SPARE, RAM, R1),
            _ => self.e.ldrh_reg(SPARE, RAM, R1),
        }
        self.e.alu_imm(Cond::Al, Op::Add, false, CYC, CYC, 4);
        let done = self.e.here();
        self.out_of_line(slow, |c| {
            // The scratchpad, without waiting, else Rust.
            let elsewhere = c.scratchpad(R1, R2);
            match kind {
                0x20 => c.e.ldrsb_reg(SPARE, R3, R2),
                0x21 => c.e.ldrsh_reg(SPARE, R3, R2),
                0x23 => c.e.ldr_reg(SPARE, R3, R2),
                0x24 => c.e.ldrb_reg(SPARE, R3, R2),
                _ => c.e.ldrh_reg(SPARE, R3, R2),
            }
            c.back_to(done);
            c.e.patch_here(elsewhere);
            c.e.mov(R1, R0);
            c.e.mov(R0, PSX);
            let f = match kind {
                0x20 => super::load8_signed as usize,
                0x21 => super::load16_signed as usize,
                0x23 => super::load32 as usize,
                0x24 => super::load8 as usize,
                _ => super::load16 as usize,
            };
            c.call(f);
            c.e.mov(SPARE, R0);
            c.back_to(done);
        });
        self.e.written |= 0x500f;
    }

    /// SB, SH, SW; a write into compiled code drops it (and the block ends
    /// there, unless in a delay slot).
    /// The value is the MIPS register `rt`'s, or (None) in SPARE.
    fn store(&mut self, kind: u32, rs: u32, rt: Option<u32>, offset: u32, pc: u32, delay: bool) {
        self.address(rs, offset);
        let width = match kind {
            0x28 => 1,
            0x29 => 2,
            _ => 4,
        };
        self.alignment_check(width, ADDRESS_STORE, pc, delay);
        match rt {
            Some(rt) => self.load_reg(R2, rt),
            None => self.e.mov(R2, SPARE),
        }
        let slow = self.ram_index();
        match kind {
            0x28 => self.e.strb_reg(R2, RAM, R1),
            0x29 => self.e.strh_reg(R2, RAM, R1),
            _ => self.e.str_reg(R2, RAM, R1),
        }
        self.e.ldrb_lsr(R3, PAGES, R1, 12);
        self.e.alu_imm(Cond::Al, Op::Cmp, true, 0, R3, 0);
        let code = self.e.b(Cond::Ne);
        let done = self.e.here();
        self.out_of_line(code, |c| {
            // Code was written over.
            c.e.mov(R1, R0);
            c.e.mov(R0, PSX);
            c.call(super::invalidate as usize);
            if delay {
                c.back_to(done);
            } else {
                c.land_pending();
                let next = pc.wrapping_add(4);
                c.e.mov_imm(R0, next);
                c.e.str(R0, PSX, PC_);
                c.e.add_imm(R0, R0, 4, R1);
                c.e.str(R0, PSX, NEXT_PC);
                c.exit(c.count + 1);
            }
        });
        self.out_of_line(slow, |c| {
            // The scratchpad (which holds no code), else Rust.
            let elsewhere = c.scratchpad(R1, R1);
            match kind {
                0x28 => c.e.strb_reg(R2, R3, R1),
                0x29 => c.e.strh_reg(R2, R3, R1),
                _ => c.e.str_reg(R2, R3, R1),
            }
            c.back_to(done);
            c.e.patch_here(elsewhere);
            c.e.mov(R1, R0);
            c.e.mov(R0, PSX);
            let f = match kind {
                0x28 => super::store8 as usize,
                0x29 => super::store16 as usize,
                _ => super::store32 as usize,
            };
            c.call(f);
            c.back_to(done);
        });
        self.e.written |= 0x500f;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The words of the block compiled from these instructions at
    /// 8001_0000.
    fn compiled(code: &[u32]) -> Vec<u32> {
        let mut psx = Psx::new(&vec![0; 512 * 1024]).unwrap();
        for (i, &op) in code.iter().enumerate() {
            psx.ram[0x1_0000 + 4 * i..][..4].copy_from_slice(&op.to_le_bytes());
        }
        block(&psx, 0x8001_0000, 0).unwrap()
    }

    /// Loads of a MIPS register from the machine.
    fn loads_of(words: &[u32], n: u32) -> usize {
        let load = |w: u32| w & 0x0ff0_0000 == 0x0590_0000 && w >> 16 & 15 == PSX && (w & 0xfff) as i32 == reg(n);
        words.iter().filter(|&&w| load(w)).count()
    }

    #[test]
    fn a_register_just_written_is_not_loaded_again() {
        // lui r1, 0x1234; ori r1, r1, 0x5678; addu r2, r1, r1; jr r31; nop
        let words = compiled(&[0x3c01_1234, 0x3421_5678, 0x0021_1021, 0x03e0_0008, 0]);
        assert_eq!(loads_of(&words, 1), 0);
    }
}
