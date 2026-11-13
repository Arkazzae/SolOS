//! VR4300 blocks translated to A64, for a 64-bit host (the TrimUI Smart Pro's game host), as
//! compile.rs does to A32: the same blocks with the same exits, the same instructions left to the
//! interpreter and the same results, to the dispatcher in mod.rs. What differs is the machine's:
//!
//! - A guest register fits a register of the host. So a held register is the guest's whole value,
//!   a 32-bit result is sign-extended where it is made, and a 64-bit one needs no pair of words and
//!   is held like any other. Addresses and branches are decided on all 64 bits.
//! - Eleven registers hold guest registers (`POOL`), the caller-saved ones among them: every
//!   interpreted instruction stores and forgets what is held anyway, and the routines keep to
//!   their scratch registers.
//! - No block has a prologue: `enter` starts them and the routine at EXIT_ROUTINE ends them, shared
//!   by all. A block linked to is entered at its first word, and the machine's pointers stay in
//!   registers across links.
//! - There is no conditional execution: CSEL, CSET and short branches do its work.
//!
//! Generated code keeps the machine in CPU, RAM and CTX, the RDP's marks and the draws known
//! finished in MARKS and KNOWN, and a branch's condition in CONDITION. x0-x8, x16, x17 and the link
//! register are scratch. See compile.rs for why each rule of a block is what it is.

use super::Context;
use super::a64::*;
use super::shared::*;
use crate::cop0::{CU0, CU1, ERL, EXL, FR, IE, constant_register};
#[cfg(target_arch = "aarch64")]
use crate::N64;
#[cfg(target_arch = "aarch64")]
use crate::cpu::Cpu;
use crate::decode::{Instr, Op as Mips, decode};
use crate::fpu::Format;
use std::mem::offset_of;

const CPU: Reg = 19;
const RAM: Reg = 20;
const CTX: Reg = 21;
const MARKS: Reg = 22;
const KNOWN: Reg = 23;
/// The block's branch's condition, from the branch to the block's end (where a likely branch not
/// taken uncounts its slot).
const CONDITION: Reg = 24;
/// The registers holding guest registers within a block (`Held`).
const POOL: [Reg; 11] = [9, 10, 11, 12, 13, 14, 15, 25, 26, 27, 28];
/// A block is entered at its first word: it has no prologue.
#[cfg_attr(not(target_arch = "aarch64"), allow(dead_code))]
pub const BODY: usize = 0;
/// The routines blocks call (`Compiled::calls`), by number: the link routines (to one target, and
/// to a jump to a register's last four), the FPU's, the RDRAM guards for loads then stores by size,
/// the end of a block, and what starts one.
pub const LINK_ROUTINE: usize = 0;
#[cfg_attr(not(target_arch = "aarch64"), allow(dead_code))]
pub const LINK_SCAN: usize = 1;
const FPU_ROUTINES: usize = 2;
const GUARD_ROUTINES: usize = 12;
const EXIT_ROUTINE: usize = 20;
#[cfg_attr(not(target_arch = "aarch64"), allow(dead_code))]
pub const ENTER: usize = 21;
/// How many there are; two more, the link routines that count, follow them.
pub const ROUTINES: usize = 22;
/// The frame `enter` makes: the frame record and the ten callee-saved registers.
const FRAME: i32 = 96;
const SAVED: [(Reg, Reg, i32); 5] = [(19, 20, 16), (21, 22, 32), (23, 24, 48), (25, 26, 64), (27, 28, 80)];

const MARKS_AT: i32 = offset_of!(Context, marks) as i32;
const KNOWN_AT: i32 = offset_of!(Context, known) as i32;
const CODE_AT: i32 = offset_of!(Context, code) as i32;
const LINKING_AT: i32 = offset_of!(Context, linking) as i32;
const COUNTED_AT: i32 = offset_of!(Context, counted) as i32;
const LINKED_AT: i32 = offset_of!(Context, linked) as i32;
const LINKS_AT: i32 = offset_of!(Context, links) as i32;
const LEFT_AT: i32 = offset_of!(Context, left) as i32;
const MISSED_AT: i32 = offset_of!(Context, missed) as i32;
const WRITTEN_AT: i32 = offset_of!(Context, written) as i32;
const INTERPRET_AT: i32 = offset_of!(Context, interpret) as i32;

// The control state in one STP (PC and next PC), and the two flags in one STRH.
const _: () = assert!(CPU_PC == 0 && NEXT_PC == 8 && DELAY_SLOT % 2 == 0 && CURRENT_DELAY == DELAY_SLOT + 1);

/// FCR31's cause bits.
const CAUSE_BITS: u64 = 63 << 12;

/// How ROUND.W, TRUNC.W, CEIL.W, FLOOR.W, ROUND.L, TRUNC.L, CEIL.L and FLOOR.L round, by their funct.
fn rounding(funct: u32) -> Round {
    match funct {
        8 | 12 => Round::Nearest,
        9 | 13 => Round::Zero,
        10 | 14 => Round::Up,
        11 | 15 => Round::Down,
        _ => unreachable!("funct {funct} has no rounding of its own"),
    }
}

pub struct Compiled {
    #[cfg_attr(not(target_arch = "aarch64"), allow(dead_code))]
    pub code: Vec<u32>,
    pub instructions: usize,
    #[cfg_attr(not(test), allow(dead_code))]
    pub ends_in_branch: bool,
    /// Its branches to the routines (`LINK_ROUTINE` on), by word and routine, to be bound where the
    /// code is placed (`CodeMemory::place`).
    #[cfg_attr(not(target_arch = "aarch64"), allow(dead_code))]
    pub calls: Vec<(usize, usize)>,
}

/// What a delay slot run in the block may be: what compile's may, and what only this compiler has with
/// no interpreted path (`Compiler::instruction` picks them): the L conversions, and the 64-bit multiplications
/// and divisions.
fn slot_in_block(slot: Instr) -> bool {
    use Mips::*;
    let (funct, fmt) = (slot.funct(), slot.fmt());
    plain(slot.op) || memory(slot.op) || native_fpu(slot) || matches!(slot.op, Dmult | Dmultu | Ddiv | Ddivu)
        || slot.op == FpuOp && matches!(funct, 8..=11 | 37) && matches!(fmt, 16 | 17)
        || slot.op == FpuCvtInt && matches!(funct, 32 | 33) && fmt == 21
}

/// As compile::block: at most 64 instructions, ending at the first branch, and the delay slot of one
/// that is the 64th. The interpreter's entry comes through the context.
pub fn block(words: &[u32], physical: u32, _interpret: usize) -> Option<Compiled> {
    let last_branch = words.get(63).is_some_and(|&word| is_branch(decode(word).op));
    let words = &words[..words.len().min(if last_branch { 65 } else { 64 })];
    let mut c = Compiler::new((physical, 4 * words.len() as u32));
    let mut ends_in_branch = None;
    for (n, &word) in words.iter().enumerate() {
        let i = decode(word);
        if is_branch(i.op) {
            ends_in_branch = Some(i);
            match words.get(n + 1).map(|&slot| decode(slot)) {
                // Not a branch to itself: `skip_idle_loop` must see that one with its slot still to
                // run.
                Some(slot) if !c.to_itself(i) && slot_in_block(slot) => c.terminal(i, slot),
                _ => {
                    c.branch(i);
                    c.instructions += 1;
                }
            }
            break;
        }
        if !c.instruction(i) {
            break;
        }
        c.instructions += 1;
    }
    let branch = ends_in_branch;
    let ends_in_branch = branch.is_some();
    if c.instructions == 0 || c.synced == c.instructions && !ends_in_branch && c.instructions == 1 {
        // Nothing worth a block: the dispatcher interprets that one itself.
        return None;
    }
    // What the block changed of guest registers, into Cpu for the dispatcher and the next block.
    c.flush();
    let branched = if ends_in_branch { BRANCHED } else { 0 };
    match branch {
        Some(i) if c.linked => c.link(i, c.since() | branched),
        _ => {
            c.e.mov_imm(W, R0, u64::from(c.since() | branched));
            c.routine(EXIT_ROUTINE, false);
        }
    }
    c.stubs();
    if !c.stops.is_empty() {
        let target = c.e.here();
        for stop in std::mem::take(&mut c.stops) {
            c.e.patch(stop, target);
        }
        c.e.mov_imm(W, R0, u64::from(PROGRESS | STOPPED));
        c.routine(EXIT_ROUTINE, false);
    }
    Some(Compiled { code: c.e.code, instructions: c.instructions, ends_in_branch, calls: c.calls })
}

/// The routine of this number, as `CodeMemory` puts them first: the ROUTINES, then the link routines
/// that count (for profiling).
pub fn routine(k: usize) -> Vec<u32> {
    match k {
        LINK_ROUTINE | LINK_SCAN => link_routine(k == LINK_SCAN, false),
        FPU_ROUTINES..GUARD_ROUTINES => fpu_routine(k - FPU_ROUTINES),
        GUARD_ROUTINES..EXIT_ROUTINE => guard_routine(k >= GUARD_ROUTINES + 4, 1 << ((k - GUARD_ROUTINES) % 4)),
        EXIT_ROUTINE => {
            let mut e = Emitter::default();
            leave(&mut e);
            e.code
        }
        ENTER => enter_routine(),
        _ => link_routine(k == ROUTINES + 1, true),
    }
}

/// A branch's word at `from` bound to the routine at `to`.
#[cfg_attr(not(target_arch = "aarch64"), allow(dead_code))]
pub fn bound(word: u32, from: usize, to: usize) -> u32 {
    patched(word, (to as i64 - from as i64) >> 2)
}

/// A link's word for the code it goes to: its distance from the link, as an address has 64 bits.
#[cfg_attr(not(target_arch = "aarch64"), allow(dead_code))]
pub fn link_code(link: *const u32, code: *const u32) -> u32 {
    (code as isize - link as isize) as u32
}

/// Runs a block through the routine at `routine` (ENTER).
///
/// # Safety
///
/// `routine` and `code` are in live code memory, the others what a block takes.
#[cfg(target_arch = "aarch64")]
pub unsafe fn enter(routine: usize, code: *const u32, cpu: *mut Cpu, rdram: *mut u8, _machine: *mut N64,
    context: *mut Context) -> u32 {
    type Enter = extern "C" fn(*const u32, *mut Cpu, *mut u8, *mut Context) -> u32;
    let enter: Enter = unsafe { std::mem::transmute(routine) };
    enter(code, cpu, rdram, context)
}

/// Saves what the C ABI wants kept, sets the machine's registers and goes to the block in x0.
fn enter_routine() -> Vec<u32> {
    let mut e = Emitter::default();
    e.stp(FP, LR, SP, -FRAME, true);
    for (a, b, at) in SAVED {
        e.stp(a, b, SP, at, false);
    }
    e.mov(X, CPU, R1);
    e.mov(X, RAM, R2);
    e.mov(X, CTX, R3);
    e.ldr(X, MARKS, R3, MARKS_AT);
    e.ldr(W, KNOWN, R3, KNOWN_AT);
    e.br(R0);
    e.code
}

/// Back to the dispatcher, with the result in w0.
fn leave(e: &mut Emitter) {
    for (a, b, at) in SAVED {
        e.ldp(a, b, SP, at, false);
    }
    e.ldp(FP, LR, SP, FRAME, true);
    e.ret();
}

/// Where a block's end goes on (`Compiler::link`), with the PC its branch went to in x0, its result
/// in w2 and in x4 the links to try: one (a jump's, or one way of a branch), or with `scan` four (a
/// jump to a register's last targets). As compile::link_routine: into the block linked to that PC
/// if it is a 32-bit address in kseg0 or kseg1 or in the page of the branch's slot, the link was
/// made at the dispatcher's generation and the target ends by the next event, its cycles charged.
/// Otherwise back to the dispatcher with the links in `missed`; they are in `links` either way.
/// Uses x0-x6 and the flags.
fn link_routine(scan: bool, counting: bool) -> Vec<u32> {
    let mut e = Emitter::default();
    let mut back = Vec::new();
    e.str(X, R4, CTX, LINKS_AT);
    e.cmp_sign_extended(R0);
    back.push(e.b(Cond::Ne));
    // The link for the PC (its PC, generation, code and instructions) into x3.
    if scan {
        let mut found = Vec::new();
        for k in 0..4 {
            let at = 8 + 16 * k;
            e.ldr(W, R1, R4, at);
            e.cmp(W, R1, R0);
            assert!(e.alu_imm(Op::Add, X, R3, R4, at as u64));
            if k < 3 {
                found.push(e.b(Cond::Eq));
            } else {
                back.push(e.b(Cond::Ne));
            }
        }
        for branch in found {
            e.patch_here(branch);
        }
    } else {
        e.ldr(W, R1, R4, 8);
        e.cmp(W, R1, R0);
        back.push(e.b(Cond::Ne));
        assert!(e.alu_imm(Op::Add, X, R3, R4, 8));
    }
    e.ldr(W, R1, R3, 4);
    e.ldr(W, R5, CTX, LINKING_AT);
    e.cmp(W, R1, R5);
    back.push(e.b(Cond::Ne));
    // The block's cycles charged, if the target's still fit before the event.
    e.ldr(W, R5, CTX, LEFT_AT);
    e.ubfx(W, R6, R2, 0, 16);
    e.ldr(W, R1, R3, 12);
    e.alu(Op::Sub, W, R5, R5, R6);
    e.cmp(W, R5, R1);
    back.push(e.b(Cond::Lo));
    e.str(W, R5, CTX, LEFT_AT);
    if counting {
        e.ldr(W, R1, CTX, COUNTED_AT);
        e.alu(Op::Add, W, R1, R1, R6);
        e.str(W, R1, CTX, COUNTED_AT);
        e.ldr(W, R1, CTX, LINKED_AT);
        assert!(e.alu_imm(Op::Add, W, R1, R1, 1));
        e.str(W, R1, CTX, LINKED_AT);
    }
    e.ldrsw(R1, R3, 8);
    e.alu(Op::Add, X, R1, R1, R3);
    e.br(R1);
    for branch in back {
        e.patch_here(branch);
    }
    e.str(X, R4, CTX, MISSED_AT);
    e.mov(W, R0, R2);
    leave(&mut e);
    e.code
}

/// The checks of an access of `size` bytes to RDRAM, with the effective address in x2: its RDRAM
/// offset in x2 and EQ, or NE for the block to exit before the access. For a store, also in w3
/// whether its page holds code. Uses x0, x3 and the flags.
fn guard_routine(store: bool, size: u32) -> Vec<u32> {
    let mut e = Emitter::default();
    // kseg0's and kseg1's addresses become their offsets in the two windows, and any other above
    // them; kseg1's bit cleared leaves the physical address.
    assert!(e.alu_imm(Op::Eor, X, R2, R2, 0xffff_ffff_8000_0000));
    assert!(e.alu_imm(Op::And, X, R2, R2, !0x2000_0000));
    // Aligned and below RDRAM's size at once: the address's low bits rotated to the top would make
    // it larger. Alignment and the power-of-two size also prove the whole access is in bounds and
    // within one piece of marks.
    let k = size.trailing_zeros();
    e.ror(X, R3, R2, k);
    e.cmp_imm(X, R3, 0x80_0000 >> k);
    let mut fail = vec![e.b(Cond::Hs)];
    // The RDP's thread may still draw to the piece if its number is above the draws known finished.
    e.lsr(W, R3, R2, crate::rdp_thread::MARK);
    e.ldr_scaled(R3, MARKS, R3);
    e.cmp(W, R3, KNOWN);
    fail.push(e.b(Cond::Hi));
    if store {
        e.ldr(X, R3, CTX, CODE_AT);
        e.lsr(W, R0, R2, 12);
        e.ldrb_reg(R3, R3, R0);
    }
    e.cmp(X, R0, R0);
    e.ret();
    for branch in fail {
        e.patch_here(branch);
    }
    e.mov_imm(W, R3, 1);
    e.cmp_imm(W, R3, 0);
    e.ret();
    e.code
}

/// The FPU's routines, k 0-3 ADD.S, SUB.S, MUL.S and DIV.S, 4-7 the same in double precision:
/// `arithmetic_single` or `arithmetic_double`, then EQ, or NE for the block to exit before the
/// instruction.
fn fpu_routine(k: usize) -> Vec<u32> {
    let mut c = Compiler::new((0, 0));
    c.cop1_checked = true;
    let funct = (k % 5) as u32;
    if k < 5 {
        c.arithmetic_single(funct);
    } else {
        c.arithmetic_double(funct);
    }
    c.e.cmp(X, R0, R0);
    c.e.ret();
    for exit in std::mem::take(&mut c.exits) {
        c.e.patch_here(exit.at);
    }
    c.e.mov_imm(W, IP0, 1);
    c.e.cmp_imm(W, IP0, 0);
    c.e.ret();
    c.e.code
}

/// A guest register held in a register of the pool within a block, all of it. A block reads it from
/// Cpu once, and stores a changed one where the block leaves (its exits' stubs, `Compiler::stubs`),
/// before an interpreted instruction and before the block's branch.
#[derive(Clone, Copy)]
struct Held {
    guest: usize,
    host: Reg,
    /// Changed in the block: Cpu's copy is stale.
    dirty: bool,
    /// When it was last used: the least recent is the one let go.
    used: u32,
}

/// A branch out of the block before an instruction, to a stub that stores the guest registers
/// changed by then and returns `value`.
#[derive(Clone, Copy)]
struct Exit {
    at: usize,
    value: u32,
    /// After a store into a piece of code (with WROTE_CODE and where, x2).
    code: bool,
    dirty: Dirty,
}

/// The guest registers changed and where they are held, in the guests' order: a set kept without
/// allocating, as compiling is frequent.
#[derive(Clone, Copy, PartialEq)]
struct Dirty {
    count: usize,
    held: [(usize, Reg); POOL.len()],
}

impl Dirty {
    fn iter(&self) -> impl Iterator<Item = &(usize, Reg)> {
        self.held[..self.count].iter()
    }
}

struct Compiler {
    e: Emitter,
    /// Instructions compiled, and those accounted by an interpreted one.
    instructions: usize,
    synced: usize,
    /// The exits (`exit`, `code_exit`), and the branches to the one after an interpreted instruction
    /// stopped the block.
    exits: Vec<Exit>,
    stops: Vec<usize>,
    /// The physical bytes the block's words came from.
    source: (u32, u32),
    /// Status.CU1 was checked earlier in the block.
    cop1_checked: bool,
    /// COP0 was found usable earlier in the block.
    cop0_checked: bool,
    /// The block ends with a likely branch's slot, which is uncounted when the branch (its condition
    /// still in CONDITION) is not taken.
    annulled: bool,
    /// The slot is being compiled: its exits write the branch's control state (`before_slot`), of
    /// this branch at these bytes past the entry.
    in_slot: bool,
    terminal_branch: Option<(Instr, i64)>,
    /// The block's branch ran with its slot: it ends in the link routine.
    linked: bool,
    /// The guest registers held and the count of their uses.
    held: Vec<Held>,
    uses: u32,
    /// The branches to the routines (`Compiled::calls`).
    calls: Vec<(usize, usize)>,
}

impl Compiler {
    fn new(source: (u32, u32)) -> Self {
        Self {
            e: Emitter { code: Vec::with_capacity(1024) },
            instructions: 0,
            synced: 0,
            exits: Vec::with_capacity(64),
            stops: Vec::new(),
            source,
            cop1_checked: false,
            cop0_checked: false,
            annulled: false,
            in_slot: false,
            terminal_branch: None,
            linked: false,
            held: Vec::with_capacity(POOL.len()),
            uses: 0,
            calls: Vec::with_capacity(16),
        }
    }

    /// A branch (with link, a call) to a routine, bound where the block is placed.
    fn routine(&mut self, routine: usize, call: bool) {
        self.calls.push((self.e.here(), routine));
        self.e.emit(if call { 0x9400_0000 } else { 0x1400_0000 });
    }

    /// The instructions completed before the current one since the last interpreted instruction, as
    /// a result (with PROGRESS after one).
    fn since(&self) -> u32 {
        (self.instructions - self.synced) as u32 | if self.synced > 0 { PROGRESS } else { 0 }
    }

    /// What an exit before the current instruction returns: in a slot, the branch completed and its
    /// control state written (BRANCHED), the slot still to run.
    fn exit_value(&self) -> u32 {
        self.since() | if self.in_slot { BRANCHED } else { 0 }
    }

    /// An exit before the current instruction through the branch at `at`: through a stub that stores
    /// the guest registers changed.
    fn exit_at(&mut self, at: usize) {
        let (value, dirty) = (self.exit_value(), self.dirty());
        self.exits.push(Exit { at, value, code: false, dirty });
    }

    /// An exit before the current instruction if `cond`.
    fn exit(&mut self, cond: Cond) {
        let at = self.e.b(cond);
        self.exit_at(at);
    }

    /// An exit before the current instruction if a register is zero, or with `nonzero` if it is not.
    fn exit_if_zero(&mut self, nonzero: bool, size: Size, rt: Reg) {
        let at = self.e.cbz(nonzero, size, rt);
        self.exit_at(at);
    }

    /// An exit after the current instruction, done, if `cond`.
    fn exit_after(&mut self, cond: Cond) {
        let at = self.e.b(cond);
        let (value, dirty) = (self.exit_value() + 1, self.dirty());
        self.exits.push(Exit { at, value, code: false, dirty });
    }

    /// The exit after a store into a piece of code (with the address in x2), the store done, if w3
    /// says so.
    fn code_exit(&mut self) {
        let at = self.e.cbz(true, W, R3);
        let (value, dirty) = (self.exit_value() + 1, self.dirty());
        self.exits.push(Exit { at, value, code: true, dirty });
    }

    /// The guest registers changed, and where they are held.
    fn dirty(&self) -> Dirty {
        let mut dirty = Dirty { count: 0, held: [(0, 0); POOL.len()] };
        for h in self.held.iter().filter(|h| h.dirty) {
            dirty.held[dirty.count] = (h.guest, h.host);
            dirty.count += 1;
        }
        dirty.held[..dirty.count].sort_unstable();
        dirty
    }

    /// The exits' stubs, one for each value and set of changed registers: in a branch's slot
    /// (BRANCHED), the control state first (one of two routines of the block's: as the branch leaves
    /// it, `before_slot`, or after a store into code as the slot does, `after_slot`), then the
    /// registers into Cpu, then the value returned (after a store into code with WROTE_CODE, and where
    /// in the context).
    fn stubs(&mut self) {
        type Stub = (u32, bool, Dirty, usize);
        let mut stubs: Vec<Stub> = Vec::with_capacity(self.exits.len());
        let (mut before_slot, mut after_slot) = (Vec::new(), Vec::new());
        for exit in std::mem::take(&mut self.exits) {
            let same = |s: &&Stub| s.0 == exit.value && s.1 == exit.code && s.2 == exit.dirty;
            let target = match stubs.iter().find(same) {
                Some(stub) => stub.3,
                None => {
                    let here = self.e.here();
                    if exit.code {
                        // Where (an aligned store is within one piece of code): the dispatcher
                        // compares sources again only if code is there.
                        self.e.str(W, R2, CTX, WRITTEN_AT);
                    }
                    if exit.value & BRANCHED != 0 {
                        let call = self.e.bl();
                        if exit.code { &mut after_slot } else { &mut before_slot }.push(call);
                    }
                    for &(guest, host) in exit.dirty.iter() {
                        self.e.str(X, host, CPU, reg(guest));
                    }
                    let value = if exit.code { exit.value | WROTE_CODE } else { exit.value };
                    self.e.mov_imm(W, R0, u64::from(value));
                    self.routine(EXIT_ROUTINE, false);
                    stubs.push((exit.value, exit.code, exit.dirty, here));
                    here
                }
            };
            self.e.patch(exit.at, target);
        }
        for (calls, before) in [(before_slot, true), (after_slot, false)] {
            if calls.is_empty() {
                continue;
            }
            let at = self.e.here();
            for call in calls {
                self.e.patch(call, at);
            }
            let (i, b) = self.terminal_branch.expect("an exit in a branch's slot");
            if before {
                self.before_slot(i, b);
            } else {
                self.after_slot(i, b);
            }
            self.e.ret();
        }
    }

    fn holding(&self, guest: usize) -> Option<usize> {
        self.held.iter().position(|h| h.guest == guest)
    }

    /// Marks the k-th held register used now: its register.
    fn touch(&mut self, k: usize) -> Reg {
        self.uses += 1;
        self.held[k].used = self.uses;
        self.held[k].host
    }

    /// A register of the pool for another guest register: a free one, or the least recently used
    /// one's, stored first if changed. The operands an instruction read before are never the least
    /// recent: the pool has more than two registers.
    fn allocate(&mut self) -> Reg {
        if let Some(&host) = POOL.iter().find(|&&host| self.held.iter().all(|h| h.host != host)) {
            return host;
        }
        let k = (0..self.held.len()).min_by_key(|&k| self.held[k].used).unwrap();
        let Held { guest, host, dirty, .. } = self.held.swap_remove(k);
        if dirty {
            self.e.str(X, host, CPU, reg(guest));
        }
        host
    }

    /// The register holding a guest register, read from Cpu if it is not held yet; the zero register
    /// for the guest's. Only an operand that takes the zero register can be that one.
    fn read(&mut self, guest: usize) -> Reg {
        if guest == 0 {
            return ZR;
        }
        if let Some(k) = self.holding(guest) {
            return self.touch(k);
        }
        let host = self.allocate();
        self.e.ldr(X, host, CPU, reg(guest));
        self.held.push(Held { guest, host, dirty: false, used: 0 });
        self.touch(self.held.len() - 1)
    }

    /// The register for a result of a guest register (not the zero register); taken after the
    /// instruction's last exit, which would store it.
    fn write(&mut self, guest: usize) -> Reg {
        assert!(guest != 0);
        let k = match self.holding(guest) {
            Some(k) => k,
            None => {
                let host = self.allocate();
                self.held.push(Held { guest, host, dirty: false, used: 0 });
                self.held.len() - 1
            }
        };
        self.held[k].dirty = true;
        self.touch(k)
    }

    /// Stores the guest registers changed into Cpu; they stay held.
    fn flush(&mut self) {
        for k in 0..self.held.len() {
            if self.held[k].dirty {
                let Held { guest, host, .. } = self.held[k];
                self.e.str(X, host, CPU, reg(guest));
                self.held[k].dirty = false;
            }
        }
    }

    /// Lets go of a guest register without storing it: Cpu's copy is written next.
    fn drop_guest(&mut self, guest: usize) {
        if let Some(k) = self.holding(guest) {
            self.held.swap_remove(k);
        }
    }

    /// An instruction run by the interpreter from here; the block ends if it says so. The interpreter
    /// then accounts this one too. It reads and may write any guest register, in Cpu, and the
    /// caller-saved registers do not last the call.
    fn interpreted(&mut self, i: Instr) {
        assert!(!self.in_slot, "{:08x} taken by slot_in_block has no native path", i.word);
        self.flush();
        self.held.clear();
        self.e.mov(X, R0, CTX);
        self.e.mov_imm(W, R1, (self.instructions - self.synced) as u64);
        self.e.mov_imm(W, R2, u64::from(i.word));
        self.e.mov_imm(W, R3, u64::from(self.source.0 | self.source.1 << 23));
        self.e.ldr(X, IP0, CTX, INTERPRET_AT);
        self.e.blr(IP0);
        self.stops.push(self.e.cbz(true, W, R0));
        self.synced = self.instructions + 1;
    }

    /// x0 as a 32-bit result: its sign extension.
    fn word_result(&mut self, guest: usize) {
        if guest != 0 {
            let d = self.write(guest);
            self.e.sxtw(d, R0);
        }
    }

    /// The live entry PC plus `offset` into `rd`: a translation is cached by physical source address,
    /// so every PC, target and link comes from the PC in Cpu, which the block leaves as it was until
    /// its branch's control state.
    fn pc(&mut self, rd: Reg, offset: i64) {
        self.e.ldr(X, rd, CPU, CPU_PC);
        self.e.add_signed(X, rd, rd, offset, IP0);
    }

    /// Preserve a branch's decision in CONDITION before a possible link changes rs.
    fn branch_condition(&mut self, i: Instr) {
        use Mips::*;
        if matches!(i.op, Bc1f | Bc1t | Bc1fl | Bc1tl) {
            // The FPU's: unusable without CU1 (an exit before); FCR31's cause cleared, then its
            // condition bit.
            self.cop1_usable();
            self.e.ldr(W, IP0, CPU, FCR31);
            assert!(self.e.alu_imm(Op::And, W, IP0, IP0, !CAUSE_BITS & 0xffff_ffff));
            self.e.str(W, IP0, CPU, FCR31);
            self.e.ubfx(W, CONDITION, IP0, 23, 1);
            if matches!(i.op, Bc1f | Bc1fl) {
                assert!(self.e.alu_imm(Op::Eor, W, CONDITION, CONDITION, 1));
            }
            return;
        }
        let a = self.read(i.rs());
        let cond = match i.op {
            Beq | Beql | Bne | Bnel => {
                let b = self.read(i.rt());
                self.e.cmp(X, a, b);
                if matches!(i.op, Beq | Beql) { Cond::Eq } else { Cond::Ne }
            }
            Bltz | Bltzl | Bltzal | Bltzall => {
                self.e.cmp(X, a, ZR);
                Cond::Lt
            }
            Bgez | Bgezl | Bgezal | Bgezall => {
                self.e.cmp(X, a, ZR);
                Cond::Ge
            }
            Blez | Blezl => {
                self.e.cmp(X, a, ZR);
                Cond::Le
            }
            _ => {
                self.e.cmp(X, a, ZR);
                Cond::Gt
            }
        };
        self.e.cset(W, CONDITION, cond);
    }

    /// The register a branch links (writes its return address to), or 0.
    fn linked_register(i: Instr) -> usize {
        use Mips::*;
        match i.op {
            Jal | Bltzal | Bgezal | Bltzall | Bgezall => 31,
            Jalr => i.rd(),
            _ => 0,
        }
    }

    /// The start of a branch at `b` bytes past the entry PC: the guest registers into Cpu, so that
    /// every way on (a likely branch's slot runs on one) starts from Cpu; its decision in CONDITION
    /// (a jump to a register: its target into Cpu's next PC, where the control state takes it from),
    /// then its link written, which the slot may read. In JALR rd == rs, the target is the old value
    /// of that register.
    fn branch_head(&mut self, i: Instr, b: i64) {
        use Mips::*;
        self.flush();
        match i.op {
            Jr | Jalr => {
                let target = self.read(i.rs());
                self.e.str(X, target, CPU, NEXT_PC);
            }
            J | Jal => {}
            _ => self.branch_condition(i),
        }
        let link = Self::linked_register(i);
        if link != 0 {
            // REGIMM links are written even when the branch is not taken.
            self.drop_guest(link);
            self.pc(R0, b + 8);
            self.e.str(X, R0, CPU, reg(link));
        }
    }

    /// A jump's target in its slot's 256 MB region, with the slot's PC in x0: into `rd`. Uses x16.
    fn jump_target(&mut self, i: Instr, rd: Reg) {
        assert!(self.e.alu_imm(Op::And, X, rd, R0, !0x0fff_ffff));
        self.e.imm(Op::Orr, X, rd, rd, i.target(), IP0);
    }

    /// The control state after a branch at `b` (as the interpreter leaves it), its slot still to run:
    /// the branch the last instruction run, its slot next, in a delay slot, then the target, or B + 8
    /// where the branch is not taken (CONDITION 0). A likely branch's slot runs only taken. Uses x0,
    /// x2, x3, x16 and the flags.
    fn before_slot(&mut self, i: Instr, b: i64) {
        use Mips::*;
        self.pc(R0, b);
        self.e.str(X, R0, CPU, CURRENT_PC);
        self.e.mov_imm(W, IP0, 1);
        self.e.strh(IP0, CPU, DELAY_SLOT);
        assert!(self.e.alu_imm(Op::Add, X, R0, R0, 4));
        match i.op {
            Jr | Jalr => {
                // The target is in the next PC already.
                self.e.str(X, R0, CPU, CPU_PC);
                return;
            }
            J | Jal => self.jump_target(i, R2),
            _ => {
                self.e.add_signed(X, R2, R0, (i.simm() as i64).wrapping_shl(2), IP0);
                if !is_likely(i.op) {
                    assert!(self.e.alu_imm(Op::Add, X, R3, R0, 4));
                    self.e.cmp_imm(W, CONDITION, 0);
                    self.e.csel(X, R2, R2, R3, Cond::Ne);
                }
            }
        }
        self.e.stp(R0, R2, CPU, CPU_PC, false);
    }

    /// The control state after a branch at `b` and its slot (as `step_cpu` leaves it): the slot the
    /// last instruction run, in a delay slot, and next the target, or B + 8 where the branch was not
    /// taken (CONDITION 0; a likely branch's slot runs only taken). Leaves the PC in x0. Uses x0, x2,
    /// x16 and the flags.
    fn after_slot(&mut self, i: Instr, b: i64) {
        use Mips::*;
        self.pc(R0, b + 4);
        self.e.str(X, R0, CPU, CURRENT_PC);
        self.e.mov_imm(W, IP0, 0x100);
        self.e.strh(IP0, CPU, DELAY_SLOT);
        match i.op {
            Jr | Jalr => self.e.ldr(X, R0, CPU, NEXT_PC),
            J | Jal => {
                self.jump_target(i, R2);
                self.e.mov(X, R0, R2);
            }
            _ if is_likely(i.op) => self.e.add_signed(X, R0, R0, (i.simm() as i64).wrapping_shl(2), IP0),
            _ => {
                self.e.add_signed(X, R2, R0, (i.simm() as i64).wrapping_shl(2), IP0);
                assert!(self.e.alu_imm(Op::Add, X, R0, R0, 4));
                self.e.cmp_imm(W, CONDITION, 0);
                self.e.csel(X, R0, R2, R0, Cond::Ne);
            }
        }
        assert!(self.e.alu_imm(Op::Add, X, R2, R0, 4));
        self.e.stp(R0, R2, CPU, CPU_PC, false);
    }

    /// The control state after a likely branch at `b` not taken: its slot annulled, the branch the
    /// last instruction run and B + 8 next. Leaves the PC in x0. Uses x0, x2 and x16.
    fn annulled_slot(&mut self, b: i64) {
        self.pc(R0, b);
        self.e.str(X, R0, CPU, CURRENT_PC);
        self.e.strh(ZR, CPU, DELAY_SLOT);
        assert!(self.e.alu_imm(Op::Add, X, R0, R0, 8));
        assert!(self.e.alu_imm(Op::Add, X, R2, R0, 4));
        self.e.stp(R0, R2, CPU, CPU_PC, false);
    }

    /// The block's branch where its slot is left to the dispatcher (which runs it in the
    /// interpreter): the control state as the branch leaves it.
    fn branch(&mut self, i: Instr) {
        let b = 4 * (self.instructions - self.synced) as i64;
        self.branch_head(i, b);
        if is_likely(i.op) {
            // Not taken, the slot is annulled, without a cycle, as Cpu::branch_likely does.
            let annulled = self.e.cbz(false, W, CONDITION);
            self.before_slot(i, b);
            let done = self.e.b(Cond::Al);
            self.e.patch_here(annulled);
            self.annulled_slot(b);
            self.e.patch_here(done);
        } else {
            self.before_slot(i, b);
        }
    }

    /// The block's branch and its slot, which is plain arithmetic, an RDRAM load or store or a native
    /// FPU instruction: the control state written once, after the slot, as `step_cpu` leaves it. The
    /// slot's exits write the state as the branch leaves it (`stubs`, `before_slot`): the interpreter
    /// then does the slot; a store into code ends the block with the state after the slot
    /// (`after_slot`). A likely branch not taken annuls the slot.
    fn terminal(&mut self, i: Instr, slot: Instr) {
        let b = 4 * (self.instructions - self.synced) as i64;
        self.branch_head(i, b);
        self.instructions += 1;
        let likely = is_likely(i.op);
        let annulled = likely.then(|| self.e.cbz(false, W, CONDITION));
        self.terminal_branch = Some((i, b));
        self.in_slot = true;
        assert!(self.instruction(slot));
        self.in_slot = false;
        self.after_slot(i, b);
        self.instructions += 1;
        if let Some(annulled) = annulled {
            // Both ways meet with Cpu current and nothing held.
            self.flush();
            self.held.clear();
            let done = self.e.b(Cond::Al);
            self.e.patch_here(annulled);
            self.annulled_slot(b);
            self.e.patch_here(done);
        }
        self.linked = true;
        self.annulled = likely;
    }

    /// Whether this branch, the next instruction, jumps to itself.
    fn to_itself(&self, branch: Instr) -> bool {
        use Mips::*;
        let at = self.source.0 + 4 * self.instructions as u32;
        match branch.op {
            J | Jal => branch.target() as u32 == at & 0x0fff_ffff,
            Jr | Jalr => false,
            _ => branch.word & 0xffff == 0xffff,
        }
    }

    /// The end of a block whose branch and slot ran in it, with the PC it went to in x0 (and the
    /// condition in CONDITION): its result into w2 (a likely branch not taken does not count its
    /// slot), and to the link routine (`link_routine`) with the address of the links to try in x4,
    /// which follow: for a jump one set, for a conditional branch two (taken, then not taken), for a
    /// jump to a register one set of four. A set is as compile::link has it, but for the target's
    /// code, its distance from the link (`link_code`).
    fn link(&mut self, i: Instr, result: u32) {
        use Mips::*;
        let (conditional, scan) = (!matches!(i.op, J | Jal | Jr | Jalr), matches!(i.op, Jr | Jalr));
        self.e.mov_imm(W, R2, u64::from(result));
        if self.annulled {
            self.e.cmp_imm(W, CONDITION, 0);
            self.e.cset(W, R1, Cond::Eq);
            self.e.alu(Op::Sub, W, R2, R2, R1);
        }
        let (sets, links) = if scan { (1, 4) } else if conditional { (2, 1) } else { (1, 1) };
        // The links' words follow the branch to the routine.
        let start = self.e.here();
        if conditional {
            self.e.adr(R4, 5);
            self.e.adr(R1, 4 + 2 + 4);
            self.e.cmp_imm(W, CONDITION, 0);
            self.e.csel(X, R4, R1, R4, Cond::Eq);
        } else {
            self.e.adr(R4, 2);
        }
        self.routine(if scan { LINK_SCAN } else { LINK_ROUTINE }, false);
        assert_eq!(self.e.here(), start + if conditional { 5 } else { 2 });
        for _ in 0..sets {
            self.e.emit(links);
            self.e.emit(0);
            for _ in 0..links {
                for word in [1, 1, 0, u32::MAX] {
                    self.e.emit(word);
                }
            }
        }
    }

    /// x0 is LO and x1 is HI, each a whole value.
    fn hilo(&mut self) {
        self.e.str(X, R0, CPU, LO);
        self.e.str(X, R1, CPU, HI);
    }

    /// Dividing by zero, with the dividend, its remainder, whole in x1: x0 is -1 (1 for a negative
    /// dividend, signed).
    fn divide_by_zero(&mut self, signed: bool) {
        self.e.mov_imm(X, R0, u64::MAX);
        if signed {
            let positive = self.e.tbz(false, R1, 63);
            self.e.mov_imm(X, R0, 1);
            self.e.patch_here(positive);
        }
    }

    fn divide_word(&mut self, signed: bool, n: Reg, d: Reg) {
        let zero = self.e.cbz(false, W, d);
        if signed {
            self.e.sdiv(W, R2, n, d);
        } else {
            self.e.udiv(W, R2, n, d);
        }
        self.e.msub(W, R3, R2, d, n);
        self.e.sxtw(R0, R2);
        self.e.sxtw(R1, R3);
        let done = self.e.b(Cond::Al);
        self.e.patch_here(zero);
        self.e.sxtw(R1, n);
        self.divide_by_zero(signed);
        self.e.patch_here(done);
        self.hilo();
    }

    fn divide_double(&mut self, signed: bool, n: Reg, d: Reg) {
        let zero = self.e.cbz(false, X, d);
        if signed {
            self.e.sdiv(X, R0, n, d);
        } else {
            self.e.udiv(X, R0, n, d);
        }
        self.e.msub(X, R1, R0, d, n);
        let done = self.e.b(Cond::Al);
        self.e.patch_here(zero);
        self.e.mov(X, R1, n);
        self.divide_by_zero(signed);
        self.e.patch_here(done);
        self.hilo();
    }

    /// Guards a complete effective address, leaving its RDRAM offset in x2: an exit before the
    /// instruction unless it is a kseg0 or kseg1 address of RDRAM, aligned, in a piece of RDRAM the
    /// RDP's thread is not still drawing to (`guard_routine`, shared: less code for the host's
    /// caches); for a store, whether its page holds code in w3. No destination or value register is
    /// read until the address is resolved, so rs == rt is safe too. Uses x0, x2, x3, the link
    /// register and the flags.
    fn rdram_address(&mut self, i: Instr, size: u32, store: bool) {
        let offset = i.simm() as i64;
        if i.rs() == 0 {
            self.e.mov_imm(X, R2, offset as u64);
        } else {
            let base = self.read(i.rs());
            self.e.add_signed(X, R2, base, offset, R3);
        }
        let routine = size.trailing_zeros() as usize + if store { 4 } else { 0 };
        self.routine(GUARD_ROUTINES + routine, true);
        self.exit(Cond::Ne);
    }

    fn load(&mut self, i: Instr) {
        use Mips::*;
        let size = match i.op { Lb | Lbu => 1, Lh | Lhu => 2, Lw | Lwu => 4, _ => 8 };
        self.rdram_address(i, size, false);
        let rt = i.rt();
        if rt == 0 {
            // Only the guards' exits (reading RDRAM has no effect).
            return;
        }
        let d = self.write(rt);
        match i.op {
            Lb => self.e.ldrsb_reg(d, RAM, R2),
            Lbu => self.e.ldrb_reg(d, RAM, R2),
            Lh | Lhu => {
                self.e.ldrh_reg(d, RAM, R2);
                self.e.rev16(d, d);
                if i.op == Lh {
                    self.e.sbfx(X, d, d, 0, 16);
                }
            }
            Lw | Lwu => {
                self.e.ldr_reg(W, d, RAM, R2);
                self.e.rev(W, d, d);
                if i.op == Lw {
                    self.e.sxtw(d, d);
                }
            }
            _ => {
                self.e.ldr_reg(X, d, RAM, R2);
                self.e.rev(X, d, d);
            }
        }
    }

    fn store(&mut self, i: Instr) {
        use Mips::*;
        let size = match i.op { Sb => 1, Sh => 2, Sw | Swc1 => 4, _ => 8 };
        if matches!(i.op, Swc1 | Sdc1) {
            self.cop1_usable();
        }
        self.rdram_address(i, size, true);
        // x2 holds the address, independent of the value's register, which is read next.
        let value = match i.op {
            Swc1 => {
                self.fpr_word_read(R0, i.ft());
                R0
            }
            Sdc1 => {
                self.fpr_double_read(R0, i.ft());
                R0
            }
            _ => self.read(i.rt()),
        };
        // The dispatcher permits this path only with MI_REPEAT clear. A store into a piece of code
        // (the block's own among them, w3 from the guard) ends it, this store included.
        match size {
            1 => self.e.strb_reg(value, RAM, R2),
            2 => {
                self.e.rev16(R0, value);
                self.e.strh_reg(R0, RAM, R2);
            }
            4 => {
                self.e.rev(W, R0, value);
                self.e.str_reg(W, R0, RAM, R2);
            }
            _ => {
                self.e.rev(X, R0, value);
                self.e.str_reg(X, R0, RAM, R2);
            }
        }
        self.code_exit();
    }

    /// An exit before the FPU's first instruction in the block if Status.CU1 is clear, for the
    /// interpreter to raise the exception. Nothing the block goes on after changes Status (MTC0, ERET
    /// and exceptions stop it), so the instructions after that one need no check.
    fn cop1_usable(&mut self) {
        if self.cop1_checked {
            return;
        }
        self.e.ldr(W, IP0, CPU, STATUS);
        let at = self.e.tbz(false, IP0, CU1.trailing_zeros());
        self.exit_at(at);
        self.cop1_checked = true;
    }

    /// An exit before the first COP0 instruction in the block if COP0 is unusable (outside kernel mode
    /// without Status.CU0), for the interpreter to raise the exception. The block changes no bit of
    /// Status that decides it (`status_write`).
    fn cop0_usable(&mut self) {
        if self.cop0_checked {
            return;
        }
        self.e.ldr(W, IP0, CPU, STATUS);
        self.e.tst_imm(W, IP0, 0x18);
        let kernel = self.e.b(Cond::Eq);
        self.e.tst_imm(W, IP0, u64::from(EXL | ERL));
        let exception = self.e.b(Cond::Ne);
        let at = self.e.tbz(false, IP0, CU0.trailing_zeros());
        self.exit_at(at);
        self.e.patch_here(kernel);
        self.e.patch_here(exception);
        self.cop0_checked = true;
    }

    /// MFC0 and DMFC0 of CP0 registers.
    fn cop0_read(&mut self, i: Instr) {
        let (offset, double) = match i.rd() {
            0 => (INDEX, false),
            4 => (CONTEXT, true),
            8 => (BAD_VADDR, true),
            10 => (ENTRY_HI, true),
            11 => (COMPARE, false),
            12 => (STATUS, false),
            13 => (CAUSE, false),
            14 => (EPC, true),
            16 => (CONFIG, false),
            30 => (ERROR_EPC, true),
            _ => return self.interpreted(i),
        };
        self.cop0_usable();
        if i.rt() == 0 {
            return;
        }
        let d = self.write(i.rt());
        if i.op == Mips::Dmfc0 {
            if double {
                self.e.ldr(X, d, CPU, offset);
            } else {
                self.e.ldr(W, d, CPU, offset);
            }
        } else {
            self.e.ldrsw(d, CPU, offset);
        }
    }

    /// MFC0 and DMFC0 of Count (register 9).
    #[allow(dead_code)]
    fn count_read(&mut self, i: Instr) {
        self.cop0_usable();
        if i.rt() != 0 {
            self.e.mov(X, R0, CTX);
            self.e.mov_imm(W, R1, (self.instructions - self.synced) as u64);
            self.e.mov_imm(X, IP0, super::mfc0_count as *const () as usize as u64);
            self.e.blr(IP0);
            let d = self.write(i.rt());
            if i.op == Mips::Dmfc0 {
                self.e.mov(W, d, R0);
            } else {
                self.e.sxtw(d, R0);
            }
        }
    }

    /// MFC0 and DMFC0 of a register that reads as a constant (`constant_register`), each of them a
    /// positive word, so that the two read it alike.
    fn constant_read(&mut self, i: Instr) {
        self.cop0_usable();
        if i.rt() != 0 {
            let value = constant_register(i.rd()).unwrap();
            let d = self.write(i.rt());
            self.e.mov_imm(X, d, value);
        }
    }

    /// MTC0 to Status (`write_cop0`), where it changes only IE and the interrupt mask: otherwise an
    /// exit before it, as a change of mode, mapping or coprocessor use stops a block. The value also
    /// goes into the latch. An interrupt it lets through ends the block after it, for the dispatcher
    /// to take. Uses x0-x2, x16 and the flags.
    fn status_write(&mut self, i: Instr) {
        self.cop0_usable();
        let value = self.read(i.rt());
        self.e.mov_imm(W, R1, 0xff57_ffff);
        self.e.alu(Op::And, W, R1, value, R1);
        self.e.ldr(W, IP0, CPU, STATUS);
        self.e.alu(Op::Eor, W, R2, R1, IP0);
        assert!(self.e.alu_imm(Op::And, W, R2, R2, !0xff00 & 0xffff_ffff));
        assert!(self.e.alu_imm(Op::Ands, W, R2, R2, u64::from(!IE)));
        self.exit(Cond::Ne);
        self.e.str(W, R1, CPU, STATUS);
        self.e.sxtw(IP0, value);
        self.e.str(X, IP0, CPU, LATCH);
        // Pending (`Cop0::interrupt_pending`): IE without EXL and ERL, and a cause the mask lets
        // through.
        self.e.ldr(W, IP0, CPU, CAUSE);
        self.e.alu(Op::And, W, IP0, IP0, R1);
        assert!(self.e.alu_imm(Op::And, W, R2, R1, u64::from(IE | EXL | ERL)));
        self.e.cmp_imm(W, R2, u64::from(IE));
        self.e.csel(W, IP0, IP0, ZR, Cond::Eq);
        self.e.tst_imm(W, IP0, 0xff00);
        self.exit_after(Cond::Ne);
    }

    /// `access` on the FPU register at `with` if Status.FR is set, else on the one at `without`. Uses
    /// x16.
    fn by_fr(&mut self, with: i32, without: i32, mut access: impl FnMut(&mut Self, i32)) {
        self.e.ldr(W, IP0, CPU, STATUS);
        let clear = self.e.tbz(false, IP0, FR.trailing_zeros());
        access(self, with);
        let done = self.e.b(Cond::Al);
        self.e.patch_here(clear);
        access(self, without);
        self.e.patch_here(done);
    }

    /// An FPU register into `rd` as the 32-bit moves see it, sign-extended. With Status.FR clear an
    /// odd register is the top half of the even one below.
    fn fpr_word_read(&mut self, rd: Reg, n: usize) {
        if n & 1 == 0 {
            self.e.ldrsw(rd, CPU, fpr(n));
        } else {
            self.by_fr(fpr(n), fpr(n & !1) + 4, |c, at| c.e.ldrsw(rd, CPU, at));
        }
    }

    /// `rs`'s low word into an FPU register as the 32-bit moves see it.
    fn fpr_word_write(&mut self, rs: Reg, n: usize) {
        if n & 1 == 0 {
            self.e.str(W, rs, CPU, fpr(n));
        } else {
            self.by_fr(fpr(n), fpr(n & !1) + 4, |c, at| c.e.str(W, rs, CPU, at));
        }
    }

    /// An FPU register into `rd` as the 64-bit moves see it: with Status.FR clear an odd register is
    /// the whole even one below.
    fn fpr_double_read(&mut self, rd: Reg, n: usize) {
        if n & 1 == 0 {
            self.e.ldr(X, rd, CPU, fpr(n));
        } else {
            self.by_fr(fpr(n), fpr(n & !1), |c, at| c.e.ldr(X, rd, CPU, at));
        }
    }

    fn fpr_double_write(&mut self, rs: Reg, n: usize) {
        if n & 1 == 0 {
            self.e.str(X, rs, CPU, fpr(n));
        } else {
            self.by_fr(fpr(n), fpr(n & !1), |c, at| c.e.str(X, rs, CPU, at));
        }
    }

    /// CFC1 and CTC1. Moving to FCR31 a cause that is enabled (or E) raises the exception: that exits
    /// before, for the interpreter.
    fn fpu_control(&mut self, i: Instr) {
        self.cop1_usable();
        if i.op == Mips::Cfc1 {
            match i.fs() {
                31 => self.e.ldr(W, R0, CPU, FCR31),
                0 => self.e.mov_imm(W, R0, 0x0a00),
                _ => self.e.mov_imm(W, R0, 0),
            }
            self.word_result(i.rt());
        } else if i.fs() == 31 {
            let value = self.read(i.rt());
            self.e.mov_imm(W, R1, 0x0183_ffff);
            self.e.alu(Op::And, W, R0, value, R1);
            self.e.ubfx(W, R1, R0, 12, 6);
            self.e.ubfx(W, R2, R0, 7, 5);
            assert!(self.e.alu_imm(Op::Orr, W, R2, R2, 32));
            self.e.tst(W, R1, R2);
            self.exit(Cond::Ne);
            self.e.str(W, R0, CPU, FCR31);
        }
    }

    /// CACHE does nothing here but outside kernel mode without CU0, where it raises the coprocessor
    /// exception: that exits before.
    fn cache(&mut self) {
        self.e.ldr(W, IP0, CPU, STATUS);
        self.e.tst_imm(W, IP0, 0x18);
        let kernel = self.e.b(Cond::Eq);
        self.e.mov_imm(W, R3, u64::from(EXL | ERL | CU0));
        self.e.tst(W, IP0, R3);
        self.exit(Cond::Eq);
        self.e.patch_here(kernel);
    }

    /// An exit unless the single in `r` is ordinary: normal or zero (not denormal, infinite or NaN).
    /// Uses x16 and the flags.
    fn ordinary(&mut self, r: Reg) {
        self.e.lsl(W, IP0, r, 1);
        let zero = self.e.cbz(false, W, IP0);
        self.e.ubfx(W, IP0, r, 23, 8);
        assert!(self.e.alu_imm(Op::Sub, W, IP0, IP0, 1));
        self.e.cmp_imm(W, IP0, 0xfe);
        self.exit(Cond::Hs);
        self.e.patch_here(zero);
    }

    /// `rd`: arithmetic's single fs, the even register of a pair while Status.FR is clear (its low word,
    /// unlike the moves'). Uses x16.
    fn fs_single(&mut self, fs: usize, rd: Reg) {
        if fs & 1 == 0 {
            self.e.ldr(W, rd, CPU, fpr(fs));
        } else {
            self.by_fr(fpr(fs), fpr(fs & !1), |c, at| c.e.ldr(W, rd, CPU, at));
        }
    }

    /// An exit before the instruction if any of these bits of FCR31 is set (the rounding mode's, or
    /// inexact's enable). Uses x16, x17 and the flags.
    fn fcr31_any(&mut self, bits: u64) {
        self.e.ldr(W, IP0, CPU, FCR31);
        self.e.mov_imm(W, IP1, bits);
        self.e.tst(W, IP0, IP1);
        self.exit(Cond::Ne);
    }

    /// FCR31 after an operation whose only cause can be inexact (w1, 0 or 1): the cause cleared first,
    /// then inexact in cause and flags.
    fn fcr31_inexact(&mut self) {
        self.e.ldr(W, IP0, CPU, FCR31);
        assert!(self.e.alu_imm(Op::And, W, IP0, IP0, !CAUSE_BITS & 0xffff_ffff));
        self.e.alu_shifted(Op::Orr, W, IP0, IP0, R1, Shift::Lsl(12));
        self.e.alu_shifted(Op::Orr, W, IP0, IP0, R1, Shift::Lsl(2));
        self.e.str(W, IP0, CPU, FCR31);
    }

    /// x0: fs in its format, an exit unless it is ordinary (`ordinary`, `ordinary_double`). Uses x16
    /// and the flags.
    fn fs_ordinary(&mut self, format: Format, fs: usize) {
        match format {
            Format::Single => {
                self.fs_single(fs, R0);
                self.ordinary(R0);
            }
            Format::Double => {
                self.fs_double(fs, R0);
                self.ordinary_double(R0, 1);
            }
        }
    }

    /// ROUND.W, TRUNC.W, CEIL.W, FLOOR.W and CVT.W of a single or a double, as compile::fpu_to_word: an
    /// ordinary value below 2^31 in size whose rounded value fits in a 32-bit signed integer (CVT.W
    /// rounding to nearest or toward zero, as FCR31 says), inexact unless it was an integer. Anything
    /// else exits before, for the interpreter.
    fn fpu_to_word(&mut self, i: Instr, format: Format) {
        let funct = i.funct();
        let double = format == Format::Double;
        // Below 2^31 in size, past the sign: a biased exponent below 158 or, for a double, 1054. A
        // double is converted whole, as it may round to more than a word.
        let (size, limit) = if double { (X, 1054u64 << 53) } else { (W, 158 << 24) };
        let to_int = |c: &mut Self, round: Round| {
            if double {
                c.e.fcvt_double_to_int(size, round, R2, 0);
            } else {
                c.e.fcvt_single_to_int(size, round, R2, 0);
            }
        };
        self.cop1_usable();
        self.fcr31_any(if funct == 36 { 0x82 } else { 0x80 });
        self.fs_ordinary(format, i.fs());
        self.e.lsl(size, R2, R0, 1);
        self.e.mov_imm(size, R3, limit);
        self.e.cmp(size, R2, R3);
        self.exit(Cond::Hs);
        if double {
            self.e.fmov_to_double(0, R0);
        } else {
            self.e.fmov_to_single(0, R0);
        }
        if funct == 36 {
            // FCR31's mode: toward zero (1) or to nearest (0).
            self.e.ldr(W, IP0, CPU, FCR31);
            let nearest = self.e.tbz(false, IP0, 0);
            to_int(self, Round::Zero);
            let done = self.e.b(Cond::Al);
            self.e.patch_here(nearest);
            to_int(self, Round::Nearest);
            self.e.patch_here(done);
        } else {
            to_int(self, rounding(funct));
        }
        if double {
            self.e.cmp_sign_extended(R2);
            self.exit(Cond::Ne);
            self.e.scvtf_double(W, 2, R2);
            self.e.fcmp_double(2, 0);
        } else {
            self.e.scvtf_single(W, 2, R2);
            self.e.fcmp_single(2, 0);
        }
        self.e.cset(W, R1, Cond::Ne);
        self.fcr31_inexact();
        // A word result fills fd's 64 bits, its top half zero.
        if double {
            self.e.mov(W, R2, R2);
        }
        self.e.str(X, R2, CPU, fpr(i.fd()));
    }

    /// ROUND.L, TRUNC.L, CEIL.L, FLOOR.L and CVT.L of a single or a double: an ordinary value below
    /// 2^53 in size (CVT.L rounding to nearest or toward zero, as FCR31 says), inexact unless it was
    /// an integer. Anything else exits before, for the interpreter.
    fn fpu_to_long(&mut self, i: Instr, format: Format) {
        let funct = i.funct();
        let double = format == Format::Double;
        // Below 2^53 in size, past the sign: a biased exponent below 180 or, for a double, 1076.
        let (size, limit) = if double { (X, 1076u64 << 53) } else { (W, 180 << 24) };
        self.cop1_usable();
        self.fcr31_any(if funct == 37 { 0x82 } else { 0x80 });
        self.fs_ordinary(format, i.fs());
        self.e.lsl(size, R2, R0, 1);
        self.e.mov_imm(size, R3, limit);
        self.e.cmp(size, R2, R3);
        self.exit(Cond::Hs);
        if double {
            self.e.fmov_to_double(0, R0);
        } else {
            self.e.fmov_to_single(0, R0);
            self.e.fcvt_to_double(0, 0);
        }
        if funct == 37 {
            // FCR31's mode: toward zero (1) or to nearest (0).
            self.e.ldr(W, IP0, CPU, FCR31);
            let nearest = self.e.tbz(false, IP0, 0);
            self.e.fcvt_double_to_int(X, Round::Zero, R2, 0);
            let done = self.e.b(Cond::Al);
            self.e.patch_here(nearest);
            self.e.fcvt_double_to_int(X, Round::Nearest, R2, 0);
            self.e.patch_here(done);
        } else {
            self.e.fcvt_double_to_int(X, rounding(funct), R2, 0);
        }
        self.e.scvtf_double(X, 2, R2);
        self.e.fcmp_double(2, 0);
        self.e.cset(W, R1, Cond::Ne);
        self.fcr31_inexact();
        self.e.str(X, R2, CPU, fpr(i.fd()));
    }

    /// ADD.S, SUB.S, MUL.S, DIV.S and SQRT.S where the VR4300 would round as the host's FPU does; see
    /// compile::fpu_arithmetic for when that is, and how inexact is worked out.
    fn fpu_arithmetic(&mut self, i: Instr) {
        self.cop1_usable();
        // ft is the register as named (SQRT has one operand).
        self.fs_single(i.fs(), R0);
        if i.funct() != 4 {
            self.e.ldr(W, R1, CPU, fpr(i.ft()));
        }
        self.fpu_routine(i.funct() as usize);
        // A single result fills fd's 64 bits, its top half zero.
        self.e.str(X, R2, CPU, fpr(i.fd()));
    }

    /// A call of an FPU routine (`fpu_routine`), with the block's exit if it says so (NE).
    fn fpu_routine(&mut self, k: usize) {
        self.routine(FPU_ROUTINES + k, true);
        self.exit(Cond::Ne);
    }

    /// The body of `fpu_arithmetic`'s routine for `funct`: the operands in w0 and w1, the result in w2
    /// and FCR31 set. Uses x3, x16, x17, d0-d7 and d16.
    fn arithmetic_single(&mut self, funct: u32) {
        self.fcr31_any(0x83);
        self.ordinary(R0);
        if funct != 4 {
            self.ordinary(R1);
        }
        if funct == 3 {
            self.e.lsl(W, R2, R1, 1);
            self.exit_if_zero(false, W, R2);
        } else if funct == 4 {
            let not_neg = self.e.tbz(false, R0, 31);
            self.e.lsl(W, IP0, R0, 1);
            self.exit_if_zero(true, W, IP0);
            self.e.patch_here(not_neg);
        }
        // s0, s1: the operands; d2, d3: the same as doubles; s4: the result; d5-d7, d16: working.
        self.e.fmov_to_single(0, R0);
        if funct != 4 {
            self.e.fmov_to_single(1, R1);
        }
        self.e.fcvt_to_double(2, 0);
        if funct != 4 {
            self.e.fcvt_to_double(3, 1);
        }
        match funct {
            0 | 1 => {
                let op = if funct == 0 { Fp::Add } else { Fp::Sub };
                self.e.fp_single(op, 4, 0, 1);
                // two_sum(a, ±b): x, and its error e.
                self.e.fp_double(op, 5, 2, 3);
                self.e.fp_double(Fp::Sub, 6, 5, 2);
                self.e.fp_double(Fp::Sub, 7, 5, 6);
                self.e.fp_double(Fp::Sub, 7, 2, 7);
                if funct == 0 {
                    self.e.fp_double(Fp::Sub, 16, 3, 6);
                    self.e.fp_double(Fp::Add, 7, 7, 16);
                } else {
                    self.e.fp_double(Fp::Add, 16, 3, 6);
                    self.e.fp_double(Fp::Sub, 7, 7, 16);
                }
                // Inexact unless the single is x and x is exact.
                self.e.fcvt_to_double(16, 4);
                self.e.fcmp_double(16, 5);
                self.e.cset(W, R1, Cond::Ne);
                self.e.fcmp_double_zero(7);
                self.e.cset(W, R2, Cond::Ne);
                self.e.alu(Op::Orr, W, R1, R1, R2);
            }
            2 => {
                self.e.fp_single(Fp::Mul, 4, 0, 1);
                self.e.fp_double(Fp::Mul, 5, 2, 3);
                self.e.fcvt_to_double(6, 4);
                self.e.fcmp_double(6, 5);
                self.e.cset(W, R1, Cond::Ne);
            }
            3 => {
                self.e.fp_single(Fp::Div, 4, 0, 1);
                self.e.fcvt_to_double(5, 4);
                self.e.fp_double(Fp::Mul, 6, 5, 3);
                self.e.fp_double(Fp::Sub, 6, 2, 6);
                self.e.fcmp_double_zero(6);
                self.e.cset(W, R1, Cond::Ne);
            }
            _ => {
                self.e.fsqrt_single(4, 0);
                self.e.fcvt_to_double(5, 4);
                self.e.fp_double(Fp::Mul, 6, 5, 5);
                self.e.fcmp_double(6, 2);
                self.e.cset(W, R1, Cond::Ne);
            }
        }
        // w1: inexact. The result in w2.
        self.e.fmov_from_single(R2, 4);
        self.single_result();
        // FCR31's cause starts cleared; inexact goes to cause and flags.
        self.fcr31_inexact();
    }

    /// The exits for a single result in w2 (w1 1 if it is inexact): unless it is normal or a zero that
    /// is exact, and if inexact, unless it is above the smallest normal magnitude (the FPU rounds
    /// values just below that up to it, and the VR4300 finds some of them tiny: `fpu::round`). Uses
    /// x3, x16 and the flags.
    fn single_result(&mut self) {
        self.e.lsl(W, R3, R2, 1);
        let zero = self.e.cbz(false, W, R3);
        self.e.ubfx(W, IP0, R2, 23, 8);
        assert!(self.e.alu_imm(Op::Sub, W, IP0, IP0, 1));
        self.e.cmp_imm(W, IP0, 0xfe);
        self.exit(Cond::Hs);
        self.e.mov_imm(W, IP0, 0x0100_0000);
        self.e.cmp(W, R3, IP0);
        let above = self.e.b(Cond::Ne);
        self.e.patch_here(zero);
        self.exit_if_zero(true, W, R1);
        self.e.patch_here(above);
    }

    /// C.cond.S and C.cond.D unless an operand is a NaN, which may raise invalid (that exits before,
    /// for the interpreter): the cause cleared, then FCR31's condition set if fs < ft and the
    /// condition has bit 2, or fs = ft and it has bit 1.
    fn fpu_compare(&mut self, i: Instr) {
        let condition = i.funct() & 15;
        self.cop1_usable();
        if i.fmt() == 16 {
            self.fs_single(i.fs(), R0);
            self.e.ldr(W, R1, CPU, fpr(i.ft()));
            // A NaN's bits, shifted past the sign, are above the infinities'.
            self.e.mov_imm(W, R3, 0xff00_0000);
            for operand in [R0, R1] {
                self.e.lsl(W, R2, operand, 1);
                self.e.cmp(W, R2, R3);
                self.exit(Cond::Hi);
            }
            self.e.fmov_to_single(0, R0);
            self.e.fmov_to_single(1, R1);
            self.e.fcmp_single(0, 1);
        } else {
            self.fs_double(i.fs(), R0);
            self.e.ldr(X, R1, CPU, fpr(i.ft()));
            self.not_nan_double(R0);
            self.not_nan_double(R1);
            self.e.fmov_to_double(0, R0);
            self.e.fmov_to_double(1, R1);
            self.e.fcmp_double(0, 1);
        }
        // Less is MI, equal EQ, and with no NaN either one LS.
        match (condition & 4 != 0, condition & 2 != 0) {
            (false, false) => self.e.mov_imm(W, R2, 0),
            (true, false) => self.e.cset(W, R2, Cond::Mi),
            (false, true) => self.e.cset(W, R2, Cond::Eq),
            (true, true) => self.e.cset(W, R2, Cond::Ls),
        }
        self.e.ldr(W, IP0, CPU, FCR31);
        assert!(self.e.alu_imm(Op::And, W, IP0, IP0, !CAUSE_BITS & 0xffff_ffff));
        assert!(self.e.alu_imm(Op::And, W, IP0, IP0, !(1 << 23) & 0xffff_ffff));
        self.e.alu_shifted(Op::Orr, W, IP0, IP0, R2, Shift::Lsl(23));
        self.e.str(W, IP0, CPU, FCR31);
    }

    /// ADD.D, SUB.D, MUL.D, DIV.D and SQRT.D where the VR4300 would round as the host's FPU does, as
    /// compile::fpu_arithmetic_double says.
    fn fpu_arithmetic_double(&mut self, i: Instr) {
        self.cop1_usable();
        // a in x0, b (ft as named) in x1 (SQRT has one operand).
        self.fs_double(i.fs(), R0);
        if i.funct() != 4 {
            self.e.ldr(X, R1, CPU, fpr(i.ft()));
        }
        self.fpu_routine(5 + i.funct() as usize);
        self.e.str(X, R2, CPU, fpr(i.fd()));
    }

    /// The body of `fpu_arithmetic_double`'s routine for `funct`: the operands in x0 and x1, the result
    /// in x2 and FCR31 set. Uses x3, x16, x17 and d0-d7.
    fn arithmetic_double(&mut self, funct: u32) {
        self.fcr31_any(0x83);
        self.ordinary_double(R0, 1);
        if funct != 4 {
            self.ordinary_double(R1, 1);
        }
        if funct == 3 {
            self.e.lsl(X, IP0, R1, 1);
            self.exit_if_zero(false, X, IP0);
            self.ordinary_double(R0, 56);
        } else if funct == 4 {
            let not_neg = self.e.tbz(false, R0, 63);
            self.e.lsl(X, IP0, R0, 1);
            self.exit_if_zero(true, X, IP0);
            self.e.patch_here(not_neg);
        }
        // d0, d1: the operands; d2: the result; d5-d7: working.
        self.e.fmov_to_double(0, R0);
        if funct != 4 {
            self.e.fmov_to_double(1, R1);
        }
        match funct {
            0 | 1 => {
                let op = if funct == 0 { Fp::Add } else { Fp::Sub };
                // two_sum(a, ±b): x, and its error.
                self.e.fp_double(op, 2, 0, 1);
                self.e.fp_double(Fp::Sub, 5, 2, 0);
                self.e.fp_double(Fp::Sub, 6, 2, 5);
                self.e.fp_double(Fp::Sub, 6, 0, 6);
                if funct == 0 {
                    self.e.fp_double(Fp::Sub, 7, 1, 5);
                    self.e.fp_double(Fp::Add, 6, 6, 7);
                } else {
                    self.e.fp_double(Fp::Add, 7, 1, 5);
                    self.e.fp_double(Fp::Sub, 6, 6, 7);
                }
            }
            2 => {
                self.e.fp_double(Fp::Mul, 2, 0, 1);
                self.e.fmsub_double(6, 0, 1, 2);
            }
            3 => {
                self.e.fp_double(Fp::Div, 2, 0, 1);
                self.e.fmsub_double(6, 2, 1, 0);
            }
            _ => {
                self.e.fsqrt_double(2, 0);
                self.e.fmsub_double(6, 2, 2, 0);
            }
        }
        self.e.fcmp_double_zero(6);
        self.e.cset(W, R3, Cond::Ne);
        // w3: inexact. The result in x2, normal (of 2^-967 and up for a product or quotient) or a zero
        // that is exact.
        self.e.fmov_from_double(R2, 2);
        // The exponent plus one, in 11 bits: 0 for an infinity or a NaN.
        self.e.ubfx(X, IP0, R2, 52, 11);
        assert!(self.e.alu_imm(Op::Add, W, IP0, IP0, 1));
        self.e.ubfx(W, IP0, IP0, 0, 11);
        self.exit_if_zero(false, W, IP0);
        self.e.cmp_imm(W, IP0, if funct >= 2 && funct != 4 { 57 } else { 2 });
        let normal = self.e.b(Cond::Hs);
        self.e.lsl(X, IP1, R2, 1);
        self.exit_if_zero(true, X, IP1);
        if funct == 2 {
            // A zero product is exact only where an operand is zero: of two others it lies below the
            // doubles, and so does its error, which the fused x - a * b rounded to zero as well.
            self.e.lsl(X, IP1, R0, 1);
            let zero = self.e.cbz(false, X, IP1);
            self.e.lsl(X, IP1, R1, 1);
            self.exit_if_zero(true, X, IP1);
            self.e.patch_here(zero);
        }
        self.exit_if_zero(true, W, R3);
        self.e.patch_here(normal);
        self.e.mov(W, R1, R3);
        self.fcr31_inexact();
    }

    /// CVT.S.D where the VR4300 rounds as the host's FPU does (as `fpu_arithmetic`): a normal or zero
    /// double, to a normal single (not an inexact one of the smallest normal magnitude) or a zero that
    /// is exact; inexact unless the single is the double.
    fn fpu_single_from_double(&mut self, i: Instr) {
        self.cop1_usable();
        self.fcr31_any(0x83);
        self.fs_double(i.fs(), R0);
        self.ordinary_double(R0, 1);
        self.e.fmov_to_double(0, R0);
        // s4: the single; d3: it as a double again.
        self.e.fcvt_to_single(4, 0);
        self.e.fcvt_to_double(3, 4);
        self.e.fcmp_double(3, 0);
        self.e.cset(W, R1, Cond::Ne);
        self.e.fmov_from_single(R2, 4);
        self.single_result();
        self.fcr31_inexact();
        self.e.str(X, R2, CPU, fpr(i.fd()));
    }

    /// An exit unless the double in `r` is a zero or normal with a biased exponent of `least` and up
    /// (1: any). Uses x16 and the flags.
    fn ordinary_double(&mut self, r: Reg, least: u64) {
        // The exponent plus one, in 11 bits: 0 for infinities and NaNs.
        self.e.ubfx(X, IP0, r, 52, 11);
        assert!(self.e.alu_imm(Op::Add, W, IP0, IP0, 1));
        self.e.ubfx(W, IP0, IP0, 0, 11);
        self.e.cmp_imm(W, IP0, least + 1);
        let normal = self.e.b(Cond::Hs);
        self.e.lsl(X, IP0, r, 1);
        self.exit_if_zero(true, X, IP0);
        self.e.patch_here(normal);
    }

    /// An exit if the double in `r` is a NaN: shifted past the sign, above an infinity. Uses x16, x17
    /// and the flags.
    fn not_nan_double(&mut self, r: Reg) {
        self.e.mov_imm(X, IP1, 0xffe0_0000_0000_0000);
        self.e.lsl(X, IP0, r, 1);
        self.e.cmp(X, IP0, IP1);
        self.exit(Cond::Hi);
    }

    /// `rd`: arithmetic's double fs, the even register of a pair while Status.FR is clear. Uses x16.
    fn fs_double(&mut self, fs: usize, rd: Reg) {
        if fs & 1 == 0 {
            self.e.ldr(X, rd, CPU, fpr(fs));
        } else {
            self.by_fr(fpr(fs), fpr(fs & !1), |c, at| c.e.ldr(X, rd, CPU, at));
        }
    }

    /// FCR31's cause cleared, as every FPU operation but MOV starts.
    fn fcr31_clear_cause(&mut self) {
        self.e.ldr(W, IP0, CPU, FCR31);
        assert!(self.e.alu_imm(Op::And, W, IP0, IP0, !CAUSE_BITS & 0xffff_ffff));
        self.e.str(W, IP0, CPU, FCR31);
    }

    /// NEG and ABS of an ordinary single or double (`fs_ordinary`; a NaN or a denormal raises its
    /// exception, an infinity is left too): its sign flipped or cleared.
    fn fpu_sign(&mut self, i: Instr, format: Format) {
        let (size, sign) = if format == Format::Double { (X, 1 << 63) } else { (W, 1 << 31) };
        self.cop1_usable();
        self.fs_ordinary(format, i.fs());
        if i.funct() == 7 {
            assert!(self.e.alu_imm(Op::Eor, size, R2, R0, sign));
        } else {
            assert!(self.e.alu_imm(Op::And, size, R2, R0, sign - 1));
        }
        self.fcr31_clear_cause();
        self.e.str(X, R2, CPU, fpr(i.fd()));
    }

    /// CVT.D.S of an ordinary single (`ordinary`), which is exact.
    fn fpu_double_from_single(&mut self, i: Instr) {
        self.cop1_usable();
        self.fs_single(i.fs(), R0);
        self.ordinary(R0);
        self.e.fmov_to_single(0, R0);
        self.e.fcvt_to_double(1, 0);
        self.double_result(i.fd());
    }

    /// CVT.D.W, which is exact.
    fn fpu_double_from_word(&mut self, i: Instr) {
        self.cop1_usable();
        self.fs_single(i.fs(), R0);
        self.e.scvtf_double(W, 1, R0);
        self.double_result(i.fd());
    }

    /// d1, an exact result: to fd as named, the cause cleared.
    fn double_result(&mut self, fd: usize) {
        self.e.fmov_from_double(R2, 1);
        self.fcr31_clear_cause();
        self.e.str(X, R2, CPU, fpr(fd));
    }

    /// CVT.S.W where the VR4300 rounds as the host's FPU does: to nearest, with inexact (the only cause
    /// there can be) not enabled; anything else exits before, for the interpreter. Inexact unless the
    /// single is the word, compared as doubles (which hold both exactly).
    fn fpu_from_word(&mut self, i: Instr) {
        self.cop1_usable();
        self.fcr31_any(0x83);
        // The word as arithmetic reads fs. s1: the single; d2, d3: both as doubles.
        self.fs_single(i.fs(), R0);
        self.e.scvtf_single(W, 1, R0);
        self.e.scvtf_double(W, 2, R0);
        self.e.fcvt_to_double(3, 1);
        self.e.fcmp_double(2, 3);
        self.e.cset(W, R1, Cond::Ne);
        self.fcr31_inexact();
        // A single result fills fd's 64 bits, its top half zero.
        self.e.fmov_from_single(R2, 1);
        self.e.str(X, R2, CPU, fpr(i.fd()));
    }

    /// CVT.S.L and CVT.D.L where the VR4300 rounds as the host's FPU does: to nearest, with inexact not
    /// enabled; anything else or a long of 2^55 or more exits before, for the interpreter.
    fn fpu_from_long(&mut self, i: Instr, double: bool) {
        self.cop1_usable();
        self.fcr31_any(0x83);
        self.fs_double(i.fs(), R0);
        self.e.sbfx(X, IP0, R0, 0, 56);
        self.e.cmp(X, R0, IP0);
        self.exit(Cond::Ne);
        if double {
            self.e.scvtf_double(X, 1, R0);
            self.e.fcvt_double_to_int(X, Round::Zero, R2, 1);
        } else {
            self.e.scvtf_single(X, 1, R0);
            self.e.fcvt_single_to_int(X, Round::Zero, R2, 1);
        }
        self.e.cmp(X, R0, R2);
        self.e.cset(W, R1, Cond::Ne);
        self.fcr31_inexact();
        if double {
            self.e.fmov_from_double(R2, 1);
        } else {
            // A single result fills fd's 64 bits, its top half zero.
            self.e.fmov_from_single(R2, 1);
        }
        self.e.str(X, R2, CPU, fpr(i.fd()));
    }

    fn fpu_load(&mut self, i: Instr) {
        self.cop1_usable();
        if i.op == Mips::Ldc1 {
            self.rdram_address(i, 8, false);
            self.e.ldr_reg(X, R0, RAM, R2);
            self.e.rev(X, R0, R0);
            self.fpr_double_write(R0, i.ft());
        } else {
            self.rdram_address(i, 4, false);
            self.e.ldr_reg(W, R0, RAM, R2);
            self.e.rev(W, R0, R0);
            self.fpr_word_write(R0, i.ft());
        }
    }

    /// MFC1, DMFC1, MTC1, DMTC1 and MOV.fmt, which copies all 64 bits into fd as named and leaves
    /// FCR31's cause alone.
    fn fpu_move(&mut self, i: Instr) {
        use Mips::*;
        self.cop1_usable();
        match i.op {
            Mfc1 => {
                if i.rt() != 0 {
                    let d = self.write(i.rt());
                    self.fpr_word_read(d, i.fs());
                }
            }
            Dmfc1 => {
                if i.rt() != 0 {
                    let d = self.write(i.rt());
                    self.fpr_double_read(d, i.fs());
                }
            }
            Mtc1 => {
                let value = self.read(i.rt());
                self.fpr_word_write(value, i.fs());
            }
            Dmtc1 => {
                let value = self.read(i.rt());
                self.fpr_double_write(value, i.fs());
            }
            _ => {
                self.fpr_double_read(R0, i.fs());
                self.e.str(X, R0, CPU, fpr(i.fd()));
            }
        }
    }

    fn instruction(&mut self, i: Instr) -> bool {
        use Mips::*;
        let (rs, rt, rd) = (i.rs(), i.rt(), i.rd());
        if (rd == 0 && matches!(i.op,
            Sll | Srl | Sra | Sllv | Srlv | Srav | Dsll | Dsrl | Dsra
            | Dsll32 | Dsrl32 | Dsra32 | Dsllv | Dsrlv | Dsrav | Mfhi | Mflo
            | Addu | Subu | Daddu | Dsubu | And | Or | Xor | Nor | Slt | Sltu))
            || (rt == 0 && matches!(i.op, Addiu | Daddiu | Slti | Sltiu | Andi | Ori | Xori | Lui))
        {
            // These instructions have no effects beyond their destination; NOP is the common case.
            // Trapping arithmetic is not accepted.
            return true;
        }
        match i.op {
            op if is_branch(op) => self.branch(i),
            Lb | Lbu | Lh | Lhu | Lw | Lwu | Ld => self.load(i),
            Sb | Sh | Sw | Sd | Swc1 | Sdc1 => self.store(i),
            Lwc1 | Ldc1 => self.fpu_load(i),
            Mfc1 | Dmfc1 | Mtc1 | Dmtc1 => self.fpu_move(i),
            FpuOp if i.funct() == 6 => self.fpu_move(i),
            FpuOp if i.funct() <= 4 && i.fmt() == 16 => self.fpu_arithmetic(i),
            FpuOp if i.funct() <= 4 && i.fmt() == 17 => self.fpu_arithmetic_double(i),
            FpuOp if i.funct() == 32 && i.fmt() == 17 => self.fpu_single_from_double(i),
            FpuOp if matches!(i.funct(), 12..=15 | 36) && i.fmt() == 16 => self.fpu_to_word(i, Format::Single),
            FpuOp if matches!(i.funct(), 12..=15 | 36) && i.fmt() == 17 => self.fpu_to_word(i, Format::Double),
            FpuOp if matches!(i.funct(), 8..=11 | 37) && i.fmt() == 16 => self.fpu_to_long(i, Format::Single),
            FpuOp if matches!(i.funct(), 8..=11 | 37) && i.fmt() == 17 => self.fpu_to_long(i, Format::Double),
            FpuOp if i.funct() >= 48 && matches!(i.fmt(), 16 | 17) => self.fpu_compare(i),
            FpuOp if matches!(i.funct(), 5 | 7) && i.fmt() == 16 => self.fpu_sign(i, Format::Single),
            FpuOp if matches!(i.funct(), 5 | 7) && i.fmt() == 17 => self.fpu_sign(i, Format::Double),
            FpuOp if i.funct() == 33 && i.fmt() == 16 => self.fpu_double_from_single(i),
            FpuCvtInt if i.funct() == 32 && i.fmt() == 20 => self.fpu_from_word(i),
            FpuCvtInt if i.funct() == 33 && i.fmt() == 20 => self.fpu_double_from_word(i),
            FpuCvtInt if i.funct() == 32 && i.fmt() == 21 => self.fpu_from_long(i, false),
            FpuCvtInt if i.funct() == 33 && i.fmt() == 21 => self.fpu_from_long(i, true),
            Cfc1 | Ctc1 => self.fpu_control(i),
            Mfc0 | Dmfc0 if constant_register(i.rd()).is_some() => self.constant_read(i),
            Mfc0 | Dmfc0 if matches!(i.rd(), 0 | 4 | 8 | 10 | 11 | 12 | 13 | 14 | 16 | 30) => self.cop0_read(i),
            Mtc0 if i.rd() == 12 => self.status_write(i),
            Cache => self.cache(),
            Sync => {}
            Sll => {
                let s = self.read(rt);
                let d = self.write(rd);
                match i.sa() {
                    0 => self.e.sxtw(d, s),
                    sa => self.e.sbfiz(X, d, s, sa, 32 - sa),
                }
            }
            Srl => {
                let s = self.read(rt);
                let d = self.write(rd);
                match i.sa() {
                    0 => self.e.sxtw(d, s),
                    // Bit 31 is clear: the W register's zero extension is the sign's.
                    sa => self.e.lsr(W, d, s, sa),
                }
            }
            // The whole value shifts: the high word's bits come in.
            Sra => {
                let s = self.read(rt);
                let d = self.write(rd);
                self.e.sbfx(X, d, s, i.sa(), 32);
            }
            Sllv | Srlv => {
                let a = self.read(rs);
                let s = self.read(rt);
                let d = self.write(rd);
                if i.op == Sllv {
                    self.e.lslv(W, d, s, a);
                } else {
                    self.e.lsrv(W, d, s, a);
                }
                self.e.sxtw(d, d);
            }
            Srav => {
                let a = self.read(rs);
                let s = self.read(rt);
                assert!(self.e.alu_imm(Op::And, W, R0, a, 31));
                self.e.asrv(X, R0, s, R0);
                self.word_result(rd);
            }
            Addu | Subu => {
                let a = self.read(rs);
                let b = self.read(rt);
                let d = self.write(rd);
                self.e.alu(if i.op == Addu { Op::Add } else { Op::Sub }, W, d, a, b);
                self.e.sxtw(d, d);
            }
            Addiu | Daddiu if rs == 0 => {
                let d = self.write(rt);
                self.e.mov_imm(X, d, i.simm());
            }
            Addiu => {
                let a = self.read(rs);
                let d = self.write(rt);
                self.e.add_signed(W, d, a, i.simm() as i64, IP0);
                self.e.sxtw(d, d);
            }
            Daddiu => {
                let a = self.read(rs);
                let d = self.write(rt);
                self.e.add_signed(X, d, a, i.simm() as i64, IP0);
            }
            Lui => {
                let d = self.write(rt);
                self.e.mov_imm(X, d, (i.word << 16) as i32 as i64 as u64);
            }
            Ori | Xori if rs == 0 => {
                let d = self.write(rt);
                self.e.mov_imm(X, d, i.uimm());
            }
            Andi | Ori | Xori => {
                let a = self.read(rs);
                let d = self.write(rt);
                let op = match i.op { Andi => Op::And, Ori => Op::Orr, _ => Op::Eor };
                self.e.imm(op, X, d, a, i.uimm(), IP0);
            }
            And | Or | Xor | Nor => {
                let a = self.read(rs);
                let b = self.read(rt);
                let d = self.write(rd);
                let op = match i.op { And => Op::And, Xor => Op::Eor, _ => Op::Orr };
                self.e.alu(op, X, d, a, b);
                if i.op == Nor {
                    self.e.alu(Op::Orn, X, d, ZR, d);
                }
            }
            Daddu | Dsubu => {
                let a = self.read(rs);
                let b = self.read(rt);
                let d = self.write(rd);
                self.e.alu(if i.op == Daddu { Op::Add } else { Op::Sub }, X, d, a, b);
            }
            Slt | Sltu => {
                let a = self.read(rs);
                let b = self.read(rt);
                self.e.cmp(X, a, b);
                let d = self.write(rd);
                self.e.cset(X, d, if i.op == Slt { Cond::Lt } else { Cond::Lo });
            }
            Slti | Sltiu => {
                let a = self.read(rs);
                let value = i.simm();
                if a != ZR && value < 0x1000 {
                    self.e.cmp_imm(X, a, value);
                } else if a != ZR && value.wrapping_neg() < 0x1000 {
                    assert!(self.e.alu_imm(Op::Adds, X, ZR, a, value.wrapping_neg()));
                } else {
                    self.e.mov_imm(X, IP0, value);
                    self.e.cmp(X, a, IP0);
                }
                let d = self.write(rt);
                self.e.cset(X, d, if i.op == Slti { Cond::Lt } else { Cond::Lo });
            }
            Dsll | Dsll32 | Dsrl | Dsrl32 | Dsra | Dsra32 => {
                let s = self.read(rt);
                let d = self.write(rd);
                let count = i.sa() + if matches!(i.op, Dsll32 | Dsrl32 | Dsra32) { 32 } else { 0 };
                match i.op {
                    Dsll | Dsll32 => self.e.lsl(X, d, s, count),
                    Dsrl | Dsrl32 => self.e.lsr(X, d, s, count),
                    _ => self.e.asr(X, d, s, count),
                }
            }
            Dsllv | Dsrlv | Dsrav => {
                let a = self.read(rs);
                let s = self.read(rt);
                let d = self.write(rd);
                match i.op {
                    Dsllv => self.e.lslv(X, d, s, a),
                    Dsrlv => self.e.lsrv(X, d, s, a),
                    _ => self.e.asrv(X, d, s, a),
                }
            }
            Mfhi | Mflo => {
                let d = self.write(rd);
                self.e.ldr(X, d, CPU, if i.op == Mfhi { HI } else { LO });
            }
            Mthi | Mtlo => {
                let a = self.read(rs);
                self.e.str(X, a, CPU, if i.op == Mthi { HI } else { LO });
            }
            // LO and HI are signed 32-bit results, the halves of MULTU too, as the VR4300 stores them.
            Mult | Multu => {
                let a = self.read(rs);
                let b = self.read(rt);
                self.e.mull(i.op == Mult, R2, a, b);
                self.e.sxtw(R0, R2);
                self.e.asr(X, R1, R2, 32);
                self.hilo();
            }
            Dmult | Dmultu => {
                let a = self.read(rs);
                let b = self.read(rt);
                self.e.mul(X, R0, a, b);
                if i.op == Dmult {
                    self.e.smulh(R1, a, b);
                } else {
                    self.e.umulh(R1, a, b);
                }
                self.hilo();
            }
            Div | Divu => {
                let a = self.read(rs);
                let b = self.read(rt);
                self.divide_word(i.op == Div, a, b);
            }
            Ddiv | Ddivu => {
                let a = self.read(rs);
                let b = self.read(rt);
                self.divide_double(i.op == Ddiv, a, b);
            }
            // Trapping arithmetic: the sum with the flags into IP1, an exit before the instruction on
            // an overflow (the interpreter takes the exception), then the result.
            Add | Sub | Addi | Dadd | Dsub | Daddi => {
                let size = if matches!(i.op, Add | Sub | Addi) { W } else { X };
                let a = self.read(rs);
                if matches!(i.op, Addi | Daddi) {
                    let value = i.simm() as i64;
                    // An immediate's form takes register 31 as SP, not the zero register.
                    let direct = rs != 0 && if value >= 0 {
                        self.e.alu_imm(Op::Adds, size, IP1, a, value as u64)
                    } else {
                        self.e.alu_imm(Op::Subs, size, IP1, a, value.unsigned_abs())
                    };
                    if !direct {
                        self.e.mov_imm(size, IP0, value as u64);
                        self.e.alu(Op::Adds, size, IP1, a, IP0);
                    }
                } else {
                    let b = self.read(rt);
                    self.e.alu(if matches!(i.op, Add | Dadd) { Op::Adds } else { Op::Subs }, size, IP1, a, b);
                }
                self.exit(Cond::Vs);
                let target = if matches!(i.op, Addi | Daddi) { rt } else { rd };
                if target != 0 {
                    let d = self.write(target);
                    if size == W {
                        self.e.sxtw(d, IP1);
                    } else {
                        self.e.mov(X, d, IP1);
                    }
                }
            }
            op if stops(op) => return false,
            _ => self.interpreted(i),
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compile(words: &[u32]) -> Option<Compiled> {
        block(words, 0x1000, 0)
    }

    #[test]
    fn blocks_end_as_compile_rs_ends_them() {
        // addiu t0, t0, 1 twice, then BREAK: the block stops before it.
        let plain = compile(&[0x2508_0001, 0x2508_0001, 0x0000_000d]).unwrap();
        assert_eq!(plain.instructions, 2);
        assert!(!plain.ends_in_branch);
        // beq with a plain slot runs both, and ends in the link routine.
        let branch = compile(&[0x1000_0002, 0x2508_0001, 0x2508_0001]).unwrap();
        assert_eq!(branch.instructions, 2);
        assert!(branch.ends_in_branch);
        assert!(branch.calls.iter().any(|&(_, routine)| routine == LINK_ROUTINE));
        // A branch to itself keeps its slot for the idle loop's skip.
        let idle = compile(&[0x1000_ffff, 0]).unwrap();
        assert_eq!(idle.instructions, 1);
        // BREAK alone is nothing worth a block.
        assert!(compile(&[0x0000_000d]).is_none());
    }

    #[test]
    fn every_routine_is_built() {
        for k in 0..ROUTINES + 2 {
            assert!(!routine(k).is_empty(), "routine {k}");
        }
    }

    /// Every instruction a block compiles assembles without an immediate it cannot hold: a block with
    /// each register in the pool's reach.
    #[test]
    fn immediates_fit() {
        let words: Vec<u32> = (0..32).flat_map(|r| [
            0x3c00_8000 | r << 16,             // lui r, 0x8000
            0x3400_ffff | r << 21 | r << 16,   // ori r, r, 0xffff
            0x2400_8000 | r << 21 | r << 16,   // addiu r, r, -0x8000
            0x2c00_1234 | r << 21 | r << 16,   // sltiu r, r, 0x1234
        ]).collect();
        for chunk in words.chunks(60) {
            assert!(compile(chunk).is_some());
        }
    }

    /// With Status.FR clear an odd fs names its pair's even register (`fs_bits`), which only a choice in
    /// the code can give: an even fs has none to make.
    #[test]
    fn fpu_double_signs_choose_an_odd_fs_by_fr() {
        for funct in [5u32, 7] {
            let length = |fs: u32| compile(&[0x4620_0000 | fs << 11 | 4 << 6 | funct]).unwrap().code.len();
            assert!(length(1) > length(2), "funct {funct}");
        }
    }

    /// A block of one interpreted instruction is left to the interpreter, so these compiling at all
    /// means they are native.
    #[test]
    fn constant_cop0_reads_are_native() {
        for op in [0x4000_0000u32, 0x4020_0000] {
            for rd in [15u32, 27, 29] {
                assert!(compile(&[op | 8 << 16 | rd << 11]).is_some(), "{op:08x} rd {rd}");
            }
        }
    }

    /// `slot_in_block` and `Compiler::instruction` are two lists: what the first takes must find a
    /// native path in the second (`interpreted` stops a slot that does not), whatever the opcode.
    #[test]
    fn every_slot_run_in_the_block_is_native() {
        let mut slots = 0;
        for op in 0..64u32 {
            for rs in 0..32u32 {
                for funct in 0..64u32 {
                    let word = op << 26 | rs << 21 | 2 << 16 | 4 << 11 | 6 << 6 | funct;
                    if slot_in_block(decode(word)) {
                        assert_eq!(compile(&[0x1000_0002, word]).unwrap().instructions, 2, "{word:08x}");
                        slots += 1;
                    }
                }
            }
        }
        assert!(slots > 1000, "{slots}");
    }

    #[test]
    fn fpu_to_word_compiles() {
        for fmt in [16u32, 17] {
            for funct in [12u32, 13, 14, 15, 36] {
                let word = 0x4400_0000 | fmt << 21 | 2 << 11 | 4 << 6 | funct;
                let c = compile(&[word]).unwrap();
                assert_eq!(c.instructions, 1);
                let branch = 0x1000_0002;
                let b = compile(&[branch, word]).unwrap();
                assert_eq!(b.instructions, 2);
                assert!(b.ends_in_branch);
            }
        }
    }

    #[test]
    fn fpu_sqrt_compiles() {
        for fmt in [16u32, 17] {
            let word = 0x4400_0000 | fmt << 21 | 2 << 11 | 4 << 6 | 4;
            let c = compile(&[word]).unwrap();
            assert_eq!(c.instructions, 1);
            let branch = 0x1000_0002;
            let b = compile(&[branch, word]).unwrap();
            assert_eq!(b.instructions, 2);
            assert!(b.ends_in_branch);
        }
    }

    #[test]
    fn fpu_to_long_compiles() {
        for fmt in [16u32, 17] {
            for funct in [8u32, 9, 10, 11, 37] {
                let word = 0x4400_0000 | fmt << 21 | 2 << 11 | 4 << 6 | funct;
                let c = compile(&[word]).unwrap();
                assert_eq!(c.instructions, 1);
                let branch = 0x1000_0002;
                let b = compile(&[branch, word]).unwrap();
                assert_eq!(b.instructions, 2);
                assert!(b.ends_in_branch);
            }
        }
    }

    #[test]
    fn fpu_from_long_compiles() {
        for funct in [32u32, 33] {
            let word = 0x4400_0000 | 21 << 21 | 2 << 11 | 4 << 6 | funct;
            let c = compile(&[word]).unwrap();
            assert_eq!(c.instructions, 1);
            let branch = 0x1000_0002;
            let b = compile(&[branch, word]).unwrap();
            assert_eq!(b.instructions, 2);
            assert!(b.ends_in_branch);
        }
    }
}
