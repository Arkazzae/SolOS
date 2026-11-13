//! VR4300 blocks translated to A32. A guest register is a pair of ARM
//! words; 32-bit results are explicitly sign-extended, while the
//! doubleword operations preserve all 64 bits. Instructions which can
//! change control flow end the block after the branch itself, and its
//! delay slot when that is plain arithmetic, an ordinary load or store or a
//! native FPU instruction (see `terminal`); any other slot remains with the
//! interpreter, as does one whose guard fails. Ordinary loads and stores
//! have an
//! RDRAM path; a failed address guard exits before the instruction, so the
//! interpreter performs any device access or raises the precise exception.
//! A store into a page of code (the block's own among them) ends it after
//! the store, so a changed later instruction is never run stale.
//!
//! The FPU's loads, stores and moves are native too, after a check of
//! Status.CU1 (once a block) that exits before them if it is clear; the odd
//! registers read Status.FR, which pairs them with the even ones below. So
//! are CFC1, CTC1 and the single-precision ADD, SUB, MUL and DIV where the
//! VR4300 rounds as the host's VFP does (see `fpu_arithmetic`).
//!
//! An instruction without a translation (the FPU's arithmetic, COP0's,
//! devices', unaligned and 64-bit multiplies among them) is run by the interpreter
//! from inside the block (`super::interpret`), the instructions before it
//! accounted first; the block goes on unless that one stops it. Those that
//! always would (exceptions, ERET, MTC0 but of Status, the TLB) end the block
//! before them. MFC0 and MTC0 of Status are native (`status_write`).
//!
//! Generated functions take `*mut Cpu`, `*mut u8` (readable 8 MB RDRAM), the
//! machine and a context of words: the RDP thread's page marks (a pointer
//! to the pages' numbers, the draws known finished, and the pieces of RDRAM
//! compiled code came from; see `N64::rdram_marks`), the generation of code
//! (doubled), and out, the instructions and count of blocks linked to and
//! the last block's links, then the machine, the cycles left to the next
//! event (which links are charged) and out, where a store into code stored.
//! They branch directly to the routines they share, the link routines
//! (`link_routine`), the FPU's
//! (`fpu_routine`) and the RDRAM guards (`guard_routine`), first in the
//! same code memory (`CodeMemory::place` binds the branches). An
//! RDRAM access to a page whose number is above the marks exits before it,
//! for the interpreter to wait for the drawing, and a store into a piece of
//! code ends the block after it with WROTE_CODE, for the dispatcher to
//! compare sources again. They change GPRs, HI/LO, the FPU's registers or
//! RDRAM, and return the instructions completed since the last interpreted
//! one, with the flags below. The dispatcher owns timing and sequential PC
//! metadata for those; a completed terminal branch writes its exact control
//! state itself, once, after its slot.
//!
//! Within a block, guest registers' low words are kept in five ARM
//! registers (`Held`): read from Cpu once, and written back only where the
//! block leaves (each exit through a stub that stores those it changed), and
//! before an interpreted instruction or the block's branch.

use super::Context;
use super::arm::*;
use super::shared::*;
#[cfg(target_arch = "arm")]
use crate::N64;
use crate::cop0::{CU0, CU1, ERL, EXL, FR, IE, constant_register};
use crate::cpu::Cpu;
use crate::decode::{Instr, Op as Mips, decode};
use std::mem::offset_of;

const CPU: Reg = 4;
const RAM: Reg = 5;
/// The block's branch's condition, from the branch to the block's end
/// (where a likely branch not taken uncounts its slot); a register of the
/// pool before the branch.
const CONDITION: Reg = 6;
/// The context: the marks, the generation, what links count, the machine.
const CTX: Reg = 11;
/// The registers holding guest registers within a block (`Held`): saved
/// by the callee, so the routines' and the interpreter's calls keep them.
const POOL: [Reg; 5] = [6, 7, 8, 9, 10];
/// The callee-saved registers pushed: an even count keeps the stack
/// aligned to the C ABI's 8 bytes (r3, the context's pointer, with LR).
const SAVED: u32 = 1 << R3 | 1 << CPU | 1 << RAM | 1 << 6 | 1 << 7 | 1 << 8 | 1 << 9 | 1 << 10 | 1 << CTX;
/// The prologue's words: a block linked to is entered past them.
pub const BODY: usize = 4;
/// The routines blocks call (`Compiled::calls`), by number: the link
/// routines (to one target, and to a jump to a register's last four), the
/// FPU's, the RDRAM guards for loads then stores by size, and the exit that
/// stores changed registers.
pub const LINK_ROUTINE: usize = 0;
pub const LINK_SCAN: usize = 1;
pub const FPU_ROUTINES: usize = 2;
pub const GUARD_ROUTINES: usize = 12;
pub const EXIT_ROUTINE: usize = 20;
/// How many routines blocks call (`routine`); two more, the link routines
/// that count, follow them.
#[cfg_attr(not(target_arch = "arm"), allow(dead_code))]
pub const ROUTINES: usize = 21;
/// The routine `enter` starts blocks with: none, as a block has its prologue.
#[cfg_attr(not(target_arch = "arm"), allow(dead_code))]
pub const ENTER: usize = 0;
/// Byte offsets of the context's words (see the module's documentation).
const MARKS_AT: i32 = offset_of!(Context, marks) as i32;
const CODE_AT: i32 = offset_of!(Context, code) as i32;
const LINKING_AT: i32 = offset_of!(Context, linking) as i32;
const COUNTED_AT: i32 = offset_of!(Context, counted) as i32;
const LINKED_AT: i32 = offset_of!(Context, linked) as i32;
const LINKS_AT: i32 = offset_of!(Context, links) as i32;
const LEFT_AT: i32 = offset_of!(Context, left) as i32;
const MISSED_AT: i32 = offset_of!(Context, missed) as i32;
const WRITTEN_AT: i32 = offset_of!(Context, written) as i32;
// One LDRD takes the marks and the draws known finished.
#[cfg(target_arch = "arm")]
const _: () = assert!(offset_of!(Context, known) as i32 == MARKS_AT + 4);

// The control state in one STM (PC and next PC), an STRD and an STRH.
const _: () = assert!(CPU_PC == 0 && NEXT_PC == 8 && CURRENT_PC % 8 == 0 && CURRENT_PC < 256
    && DELAY_SLOT % 2 == 0 && CURRENT_DELAY == DELAY_SLOT + 1 && DELAY_SLOT < 256);
// Every field a block reads or writes is within an LDR's immediate offset.
const _: () = assert!(STATUS < 4096 && CAUSE < 4096 && INDEX < 4096 && CONTEXT + 8 <= 4096
    && BAD_VADDR + 8 <= 4096 && ENTRY_HI + 8 <= 4096 && COMPARE < 4096 && EPC + 8 <= 4096
    && CONFIG < 4096 && ERROR_EPC + 8 <= 4096 && LATCH + 4 < 4096 && FPR + 8 * 32 <= 4096 && FCR31 < 4096);

pub struct Compiled {
    #[cfg_attr(not(target_arch = "arm"), allow(dead_code))]
    pub code: Vec<u32>,
    pub instructions: usize,
    #[cfg_attr(not(test), allow(dead_code))]
    pub ends_in_branch: bool,
    /// Its branches to the routines (`LINK_ROUTINE` on), by word and
    /// routine, to be bound where the code is placed (`CodeMemory::place`):
    /// direct branches, which the host predicts without its target buffer.
    #[cfg_attr(not(target_arch = "arm"), allow(dead_code))]
    pub calls: Vec<(usize, usize)>,
}

/// Compile at most 64 instructions, ending at the first branch, and the
/// delay slot of one that is the 64th. The caller supplies source words
/// from a single readable page of guest memory, at `physical`; `interpret`
/// is the address of the interpreter's entry.
pub fn block(words: &[u32], physical: u32, interpret: usize) -> Option<Compiled> {
    let last_branch = words.get(63).is_some_and(|&word| is_branch(decode(word).op));
    let words = &words[..words.len().min(if last_branch { 65 } else { 64 })];
    let mut c = Compiler::new((physical, 4 * words.len() as u32), interpret as u32);
    c.e.push(SAVED | 1 << LR);
    c.e.mov(CPU, R0);
    c.e.mov(RAM, R1);
    c.e.mov(CTX, R3);
    assert_eq!(c.e.here(), BODY);
    let mut ends_in_branch = None;
    for (n, &word) in words.iter().enumerate() {
        let i = decode(word);
        if is_branch(i.op) {
            ends_in_branch = Some(i);
            match words.get(n + 1).map(|&slot| decode(slot)) {
                // Not a branch to itself: `skip_idle_loop` must see that one
                // with its slot still to run.
                Some(slot) if !c.to_itself(i) && (plain(slot.op) || memory(slot.op) || native_fpu(slot)) => {
                    c.terminal(i, slot);
                }
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
    // What the block changed of guest registers, into Cpu for the dispatcher
    // and the next block.
    c.flush();
    let branched = if ends_in_branch { BRANCHED } else { 0 };
    match branch {
        Some(i) if c.linked => c.link(i, c.since() | branched),
        _ => {
            c.e.mov_imm(R0, c.since() | branched);
            c.e.pop(SAVED | 1 << PC);
        }
    }
    c.stubs();
    if !c.stops.is_empty() {
        let target = c.e.here();
        for stop in std::mem::take(&mut c.stops) {
            c.e.patch(stop, target);
        }
        c.e.mov_imm(R0, PROGRESS | STOPPED);
        c.e.pop(SAVED | 1 << PC);
    }
    Some(Compiled { code: c.e.code, instructions: c.instructions, ends_in_branch, calls: c.calls })
}

/// The routine of this number, as `CodeMemory` puts them first: the ROUTINES,
/// then the link routines that count (for profiling).
#[cfg_attr(not(target_arch = "arm"), allow(dead_code))]
pub fn routine(k: usize) -> Vec<u32> {
    match k {
        LINK_ROUTINE | LINK_SCAN => link_routine(k == LINK_SCAN, false),
        FPU_ROUTINES..GUARD_ROUTINES => fpu_routine(k - FPU_ROUTINES),
        GUARD_ROUTINES..EXIT_ROUTINE => guard_routine(k >= GUARD_ROUTINES + 4, 1 << ((k - GUARD_ROUTINES) % 4)),
        EXIT_ROUTINE => exit_routine(),
        _ => link_routine(k == ROUTINES + 1, true),
    }
}

/// A branch's word at `from` bound to the routine at `to`.
#[cfg_attr(not(target_arch = "arm"), allow(dead_code))]
pub fn bound(word: u32, from: usize, to: usize) -> u32 {
    // A branch reads the PC 8 bytes ahead; its offset is in words.
    let offset = (to as isize - (from + 8) as isize) >> 2;
    assert!((-(1 << 23)..1 << 23).contains(&offset), "a routine out of a branch's reach");
    word & 0xff00_0000 | offset as u32 & 0xff_ffff
}

/// A link's word for the code it goes to: its address.
#[cfg_attr(not(target_arch = "arm"), allow(dead_code))]
pub fn link_code(_link: *const u32, code: *const u32) -> u32 {
    code as u32
}

/// Runs a block, its `code` from its start.
///
/// # Safety
///
/// `code` is a compiled block in live code memory, the others what it takes.
#[cfg(target_arch = "arm")]
pub unsafe fn enter(_routine: usize, code: *const u32, cpu: *mut Cpu, rdram: *mut u8, machine: *mut N64,
    context: *mut Context) -> u32 {
    type Native = extern "C" fn(*mut Cpu, *mut u8, *mut N64, *mut Context) -> u32;
    let native: Native = unsafe { std::mem::transmute(code) };
    native(cpu, rdram, machine, context)
}

/// Where a block's end goes on (`Compiler::link`), with the block's
/// registers, the PC its branch went to in r0 (low word) and r1, its result
/// in r2 and in r12 the links to try: one (a jump's, or one way of a
/// branch), or with `scan` four (a jump to a register's last targets). Into
/// the block linked to that PC if it is a 32-bit address (its high word the
/// The link routine: enters the block linked to that PC, if its sign was
/// correct (a valid 32-bit value in a 64-bit register matches the
/// low word's sign), the link was made at the dispatcher's
/// generation (context word 3, odd outside kernel mode) and the target ends
/// by the next event: its instructions within the cycles left (context word
/// LEFT), which are then charged the block's, and with `counting` the block
/// and its instructions are counted (words 5 and 4, for profiling).
/// Otherwise back to the dispatcher with the result and the links' address
/// in word MISSED, for the dispatcher to link them; it is in word 6 either
/// way, for the dispatcher to know that a block reached its end. Uses r0-r3,
/// r12, LR and the flags.
#[cfg_attr(not(target_arch = "arm"), allow(dead_code))]
pub fn link_routine(scan: bool, counting: bool) -> Vec<u32> {
    let mut e = Emitter::default();
    let mut back = Vec::new();
    // The link for the PC (its PC, generation, code and instructions) into
    // r3, and that generation checked.
    if scan {
        e.str(R12, CTX, LINKS_AT);
        e.str(R12, CTX, MISSED_AT);
        e.alu_shifted(Cond::Al, Op::Cmp, true, 0, R1, R0, Shift::Asr(31));
        back.push(e.b(Cond::Ne));
        let mut found = Vec::new();
        for k in 0..4 {
            let at = 8 + 16 * k;
            e.ldr(LR, R12, at);
            e.alu_shifted(Cond::Al, Op::Cmp, true, 0, LR, R0, Shift::Lsl(0));
            assert!(e.alu_imm(Cond::Eq, Op::Add, false, R3, R12, at as u32));
            if k < 3 {
                found.push(e.b(Cond::Eq));
            } else {
                back.push(e.b(Cond::Ne));
            }
        }
        for branch in found {
            e.patch_here(branch);
        }
        e.ldr(LR, R3, 4);
        e.ldr(R1, CTX, LINKING_AT);
        e.alu_shifted(Cond::Al, Op::Cmp, true, 0, LR, R1, Shift::Lsl(0));
    } else {
        e.ldr(LR, R12, 8);
        e.ldr(R3, R12, 12);
        e.str(R12, CTX, LINKS_AT);
        e.str(R12, CTX, MISSED_AT);
        e.alu_shifted(Cond::Al, Op::Cmp, true, 0, R1, R0, Shift::Asr(31));
        e.ldr(R1, CTX, LINKING_AT);
        e.alu_shifted(Cond::Eq, Op::Cmp, true, 0, LR, R0, Shift::Lsl(0));
        e.alu_shifted(Cond::Eq, Op::Cmp, true, 0, R3, R1, Shift::Lsl(0));
        assert!(e.alu_imm(Cond::Al, Op::Add, false, R3, R12, 8));
    }
    back.push(e.b(Cond::Ne));
    // In kseg0 or kseg1, or in the page of the branch's slot; and the
    // block's cycles charged, if the target's still fit before the event.
    e.ldr(LR, CTX, LEFT_AT);
    e.alu_shifted(Cond::Al, Op::Mov, false, R1, 0, R0, Shift::Lsr(30));
    assert!(e.alu_imm(Cond::Al, Op::Cmp, true, 0, R1, 2));
    e.ldr_cond(Cond::Ne, R1, CPU, CURRENT_PC);
    e.alu_shifted(Cond::Ne, Op::Eor, false, R1, R1, R0, Shift::Lsl(0));
    e.alu_shifted(Cond::Ne, Op::Mov, true, R1, 0, R1, Shift::Lsr(12));
    back.push(e.b(Cond::Ne));
    e.ldrd(R0, R3, 8);
    e.uxth(R12, R2);
    e.alu(Op::Sub, LR, LR, R12);
    e.alu_shifted(Cond::Al, Op::Cmp, true, 0, LR, R1, Shift::Lsl(0));
    back.push(e.b(Cond::Lo));
    e.str(LR, CTX, LEFT_AT);
    if counting {
        e.ldr(R1, CTX, COUNTED_AT);
        e.alu(Op::Add, R1, R1, R12);
        e.str(R1, CTX, COUNTED_AT);
        e.ldr(R1, CTX, LINKED_AT);
        assert!(e.alu_imm(Cond::Al, Op::Add, false, R1, R1, 1));
        e.str(R1, CTX, LINKED_AT);
    }
    e.bx(R0);
    for branch in back {
        e.patch_here(branch);
    }
    e.mov(R0, R2);
    e.pop(SAVED | 1 << PC);
    e.code
}

/// A block's exit that stores the guest registers it changed (`stubs`),
/// with the block's registers, its result in r0 and in r1, for each register
/// of the pool by six bits from r6's, 32 if it holds a changed guest
/// register and that one's number. Uses r3 and r12.
#[cfg_attr(not(target_arch = "arm"), allow(dead_code))]
pub fn exit_routine() -> Vec<u32> {
    let mut e = Emitter::default();
    let gpr = offset_of!(Cpu, gpr) as i32;
    for (k, &arm) in POOL.iter().enumerate() {
        let k = 6 * k as u32;
        assert!(e.alu_imm(Cond::Al, Op::Tst, true, 0, R1, 32 << k));
        let next = e.b(Cond::Eq);
        e.ubfx(R12, R1, k, 5);
        e.alu_shifted(Cond::Al, Op::Add, false, R12, CPU, R12, Shift::Lsl(3));
        e.str(arm, R12, gpr);
        e.alu_shifted(Cond::Al, Op::Mov, false, R3, 0, arm, Shift::Asr(31));
        e.str(R3, R12, gpr + 4);
        e.patch_here(next);
    }
    e.pop(SAVED | 1 << PC);
    e.code
}

/// The checks of an access of `size` bytes to RDRAM (`rdram_address`), with
/// a block's registers, the effective address's low word in r2 and V set if
/// that sum overflowed: its RDRAM offset in r2 and EQ, or NE for the block
/// to exit before the access. For a store, also in r3 whether its page
/// holds code (`Compiler::store`). Uses r0, r1, r3 and the flags.
#[cfg_attr(not(target_arch = "arm"), allow(dead_code))]
pub fn guard_routine(store: bool, size: u32) -> Vec<u32> {
    let mut e = Emitter::default();
    let mut fail = vec![e.b(Cond::Vs)];
    // The marks and the draws known finished (context words 0 and 1).
    e.ldrd(R0, CTX, MARKS_AT as u32);
    // Less kseg0's start and kseg1's bit, only RDRAM through those two
    // windows is below its size (kuseg wraps above, kseg2 and 3 stay
    // above); what is left is the physical address.
    assert!(e.alu_imm(Cond::Al, Op::Sub, false, R2, R2, 0x8000_0000));
    assert!(e.alu_imm(Cond::Al, Op::Bic, false, R2, R2, 0x2000_0000));
    // Aligned and below the size at once: the address's low bits rotated
    // to the top would make it larger. Alignment and the power-of-two RDRAM
    // size also prove the entire access is in bounds, including an LD at
    // the last doubleword, and within one piece of marks.
    let k = size.trailing_zeros();
    if k == 0 {
        assert!(e.alu_imm(Cond::Al, Op::Cmp, true, 0, R2, 0x80_0000));
    } else {
        e.alu_shifted(Cond::Al, Op::Mov, false, R3, 0, R2, Shift::Ror(k));
        assert!(e.alu_imm(Cond::Al, Op::Cmp, true, 0, R3, 0x80_0000 >> k));
    }
    fail.push(e.b(Cond::Hs));
    // The RDP's thread may still draw to the piece if its number is above
    // the draws known finished.
    e.alu_shifted(Cond::Al, Op::Mov, false, R3, 0, R2, Shift::Lsr(crate::rdp_thread::MARK));
    e.ldr_lsl(R3, R0, R3, 2);
    if store {
        e.ldr(R12, CTX, CODE_AT);
        e.alu_shifted(Cond::Al, Op::Cmp, true, 0, R3, R1, Shift::Lsl(0));
        fail.push(e.b(Cond::Hi));
        e.ldrb_lsr(R3, R12, R2, 12);
    } else {
        e.alu_shifted(Cond::Al, Op::Cmp, true, 0, R3, R1, Shift::Lsl(0));
        fail.push(e.b(Cond::Hi));
    }
    e.alu_shifted(Cond::Al, Op::Cmp, true, 0, R0, R0, Shift::Lsl(0));
    e.bx(LR);
    for branch in fail {
        e.patch_here(branch);
    }
    assert!(e.alu_imm(Cond::Al, Op::Mov, true, R3, 0, 1));
    e.bx(LR);
    e.code
}

/// The FPU's routines, k 0-4 ADD.S, SUB.S, MUL.S, DIV.S and SQRT.S, 5-9 the same in
/// double precision, with a block's registers: `arithmetic_single` or
/// `arithmetic_double`, then EQ, or NE for the block to exit before the
/// instruction.
#[cfg_attr(not(target_arch = "arm"), allow(dead_code))]
pub fn fpu_routine(k: usize) -> Vec<u32> {
    let mut c = Compiler::new((0, 0), 0);
    c.cop1_checked = true;
    let funct = (k % 5) as u32;
    if k < 5 {
        c.arithmetic_single(funct);
    } else {
        c.arithmetic_double(funct);
    }
    c.e.alu_shifted(Cond::Al, Op::Cmp, true, 0, R0, R0, Shift::Lsl(0));
    c.e.bx(LR);
    for exit in std::mem::take(&mut c.exits) {
        c.e.patch_here(exit.at);
    }
    assert!(c.e.alu_imm(Cond::Al, Op::Mov, true, R12, 0, 1));
    c.e.bx(LR);
    c.e.code
}

#[derive(Clone, Copy)]
enum Direction {
    Left,
    Right,
    Arithmetic,
}

/// A guest register's low word held in a register of the pool within a
/// block. A block reads it from Cpu once; a 32-bit result goes only into
/// the register, its sign extension the whole value, and is stored where
/// the block leaves (its exits' stubs, `Compiler::stubs`), before an
/// interpreted instruction and before the block's branch. A 64-bit result is
/// stored into Cpu at once, and the register lets go of the guest's.
#[derive(Clone, Copy)]
struct Held {
    guest: usize,
    arm: Reg,
    /// Changed in the block: Cpu's words are stale.
    dirty: bool,
    /// The guest's high word is this one's sign extension (always so when
    /// changed); otherwise it is the one in Cpu.
    word: bool,
    /// When it was last used: the least recent is the one let go.
    used: u32,
}

/// A branch out of the block before an instruction, to a stub that stores
/// the guest registers changed by then and returns `value`.
#[derive(Clone, Copy)]
struct Exit {
    at: usize,
    value: u32,
    /// After a store into a piece of code (with WROTE_CODE and where, r2).
    code: bool,
    dirty: Dirty,
}

/// The guest registers changed and where they are held, in the guests'
/// order: a set kept without allocating, as compiling is frequent.
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
    /// The exits (`exit`, `code_exit`), and the branches to the one after an
    /// interpreted instruction stopped the block.
    exits: Vec<Exit>,
    stops: Vec<usize>,
    /// The physical bytes the block's words came from.
    source: (u32, u32),
    interpret: u32,
    /// Status.CU1 was checked earlier in the block.
    cop1_checked: bool,
    /// COP0 was found usable earlier in the block.
    cop0_checked: bool,
    /// The block ends with a likely branch's slot, which is uncounted when
    /// the branch (its condition still in CONDITION) is not taken.
    annulled: bool,
    /// The slot is being compiled: its exits write the branch's control
    /// state (`before_slot`), of this branch at these bytes past the entry.
    in_slot: bool,
    terminal_branch: Option<(Instr, i64)>,
    /// The block's branch ran with its slot: it ends in the link routine.
    linked: bool,
    /// The guest registers held, the count of their uses, and the registers
    /// of the pool taken out of it (CONDITION from the branch on).
    held: Vec<Held>,
    uses: u32,
    reserved: u32,
    /// The branches to the routines (`Compiled::calls`).
    calls: Vec<(usize, usize)>,
}

impl Compiler {
    fn new(source: (u32, u32), interpret: u32) -> Self {
        Self {
            e: Emitter { code: Vec::with_capacity(1024) },
            instructions: 0,
            synced: 0,
            exits: Vec::with_capacity(64),
            stops: Vec::new(),
            source,
            interpret,
            cop1_checked: false,
            cop0_checked: false,
            annulled: false,
            in_slot: false,
            terminal_branch: None,
            linked: false,
            held: Vec::with_capacity(POOL.len()),
            uses: 0,
            reserved: 0,
            calls: Vec::with_capacity(16),
        }
    }

    /// A branch (with link, a call) to a routine, bound where the block is
    /// placed.
    fn routine(&mut self, routine: usize, call: bool) {
        self.calls.push((self.e.here(), routine));
        self.e.emit(if call { 0xeb00_0000 } else { 0xea00_0000 });
    }

    /// The instructions completed before the current one since the last
    /// interpreted instruction, as a result (with PROGRESS after one).
    fn since(&self) -> u32 {
        (self.instructions - self.synced) as u32 | if self.synced > 0 { PROGRESS } else { 0 }
    }

    /// What an exit before the current instruction returns: in a slot, the
    /// branch completed and its control state written (BRANCHED), the slot
    /// still to run.
    fn exit_value(&self) -> u32 {
        self.since() | if self.in_slot { BRANCHED } else { 0 }
    }

    /// An exit before the current instruction (as `exit_value` counts it)
    /// if `cond`: through a stub that stores the guest registers changed.
    fn exit(&mut self, cond: Cond) {
        let at = self.e.b(cond);
        let (value, dirty) = (self.exit_value(), self.dirty());
        self.exits.push(Exit { at, value, code: false, dirty });
    }

    /// An exit after the current instruction, done, if `cond`.
    fn exit_after(&mut self, cond: Cond) {
        let at = self.e.b(cond);
        let (value, dirty) = (self.exit_value() + 1, self.dirty());
        self.exits.push(Exit { at, value, code: false, dirty });
    }

    /// The exit after a store into a piece of code (with the address in r2),
    /// the store done, if `cond`.
    fn code_exit(&mut self, cond: Cond) {
        let at = self.e.b(cond);
        let (value, dirty) = (self.exit_value() + 1, self.dirty());
        self.exits.push(Exit { at, value, code: true, dirty });
    }

    /// The guest registers changed, and where they are held.
    fn dirty(&self) -> Dirty {
        let mut dirty = Dirty { count: 0, held: [(0, 0); POOL.len()] };
        for h in self.held.iter().filter(|h| h.dirty) {
            dirty.held[dirty.count] = (h.guest, h.arm);
            dirty.count += 1;
        }
        dirty.held[..dirty.count].sort_unstable();
        dirty
    }

    /// The exits' stubs, one for each value and set of changed registers:
    /// in a branch's slot (BRANCHED), the control state first (one of two
    /// routines of the block's: as the branch leaves it, `before_slot`, or
    /// after a store into code as the slot does, `after_slot`), then the
    /// registers into Cpu, then the value returned (after a store into code
    /// with WROTE_CODE, and where in context word CONTEXT - 1).
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
                        // Where (an aligned store is within one piece of code):
                        // the dispatcher compares sources again only if code is
                        // there.
                        self.e.str(R2, CTX, WRITTEN_AT);
                    }
                    if exit.value & BRANCHED != 0 {
                        if exit.code { &mut after_slot } else { &mut before_slot }.push(self.e.here());
                        self.e.emit(0xeb00_0000);
                    }
                    let shared = exit.dirty.count >= 2;
                    if !shared {
                        for &(guest, arm) in exit.dirty.iter() {
                            self.store_word(arm, guest);
                        }
                    }
                    if exit.code {
                        self.e.mov_imm(R0, exit.value | WROTE_CODE);
                    } else {
                        self.e.mov_imm(R0, exit.value);
                    }
                    if shared {
                        // Less code: the registers to store described for the
                        // routine shared by all blocks (`exit_routine`).
                        let described = exit.dirty.iter().fold(0, |word, &(guest, arm)| {
                            word | (32 | guest as u32) << (6 * (arm - POOL[0]))
                        });
                        self.e.mov_imm(R1, described);
                        self.routine(EXIT_ROUTINE, false);
                    } else {
                        self.e.pop(SAVED | 1 << PC);
                    }
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
            self.e.bx(LR);
        }
    }

    /// A held 32-bit result into the guest's words in Cpu. Uses r12.
    fn store_word(&mut self, arm: Reg, guest: usize) {
        self.e.str(arm, CPU, reg(guest));
        self.shift(R12, arm, Shift::Asr(31));
        self.e.str(R12, CPU, reg(guest) + 4);
    }

    fn holding(&self, guest: usize) -> Option<usize> {
        self.held.iter().position(|h| h.guest == guest)
    }

    /// Marks the k-th held register used now: its register.
    fn touch(&mut self, k: usize) -> Reg {
        self.uses += 1;
        self.held[k].used = self.uses;
        self.held[k].arm
    }

    /// A register of the pool for another guest register: a free one, or
    /// the least recently used one's, stored first if changed. The operands
    /// an instruction read before are never the least recent: the pool has
    /// more than two registers besides those reserved.
    fn allocate(&mut self) -> Reg {
        let free = POOL.iter().find(|&&arm| self.reserved & 1 << arm == 0 && self.held.iter().all(|h| h.arm != arm));
        if let Some(&arm) = free {
            return arm;
        }
        let k = (0..self.held.len()).min_by_key(|&k| self.held[k].used).unwrap();
        let Held { guest, arm, dirty, .. } = self.held.swap_remove(k);
        if dirty {
            self.store_word(arm, guest);
        }
        arm
    }

    /// The register holding a guest register's low word (not the zero
    /// register's), read from Cpu if it is not held yet. Uses r12.
    fn read(&mut self, guest: usize) -> Reg {
        assert!(guest != 0);
        if let Some(k) = self.holding(guest) {
            return self.touch(k);
        }
        let arm = self.allocate();
        self.e.ldr(arm, CPU, reg(guest));
        self.held.push(Held { guest, arm, dirty: false, word: false, used: 0 });
        self.touch(self.held.len() - 1)
    }

    /// The register for a 32-bit result of a guest register (not the zero
    /// register), whose sign extension is its new value; taken after the
    /// instruction's last exit, which would store it. Uses r12.
    fn write(&mut self, guest: usize) -> Reg {
        assert!(guest != 0);
        let k = match self.holding(guest) {
            Some(k) => k,
            None => {
                let arm = self.allocate();
                self.held.push(Held { guest, arm, dirty: false, word: false, used: 0 });
                self.held.len() - 1
            }
        };
        self.held[k].dirty = true;
        self.held[k].word = true;
        self.touch(k)
    }

    /// An operand's low word: its register, or `zero` set to 0 for the zero
    /// register. Uses r12.
    fn source(&mut self, guest: usize, zero: Reg) -> Reg {
        if guest == 0 {
            self.e.mov_imm(zero, 0);
            zero
        } else {
            self.read(guest)
        }
    }

    /// Whether a guest register is known to be its low word sign-extended
    /// (as every 32-bit result is), so that its low word alone decides.
    fn known_word(&self, guest: usize) -> bool {
        guest == 0 || self.holding(guest).is_some_and(|k| self.held[k].word)
    }

    /// The register holding a guest register's low word, checked once in
    /// the block to be its whole value's sign extension (an exit before the
    /// instruction if not, as every RDRAM address is). Uses r12.
    fn word(&mut self, guest: usize) -> Reg {
        let arm = self.read(guest);
        let k = self.holding(guest).unwrap();
        if !self.held[k].word {
            self.e.ldr(R12, CPU, reg(guest) + 4);
            self.e.alu_shifted(Cond::Al, Op::Cmp, true, 0, R12, arm, Shift::Asr(31));
            self.exit(Cond::Ne);
            self.held[k].word = true;
        }
        arm
    }

    /// Stores the guest registers changed into Cpu; they stay held.
    fn flush(&mut self) {
        for k in 0..self.held.len() {
            if self.held[k].dirty {
                let Held { guest, arm, .. } = self.held[k];
                self.store_word(arm, guest);
                self.held[k].dirty = false;
            }
        }
    }

    /// Lets go of a guest register without storing it: its words in Cpu are
    /// written next.
    fn drop_guest(&mut self, guest: usize) {
        if let Some(k) = self.holding(guest) {
            self.held.swap_remove(k);
        }
    }

    /// Lets go of what `arm` holds, stored first if changed: it is about to
    /// be used otherwise.
    fn evict(&mut self, arm: Reg) {
        if let Some(k) = self.held.iter().position(|h| h.arm == arm) {
            let Held { guest, dirty, .. } = self.held.swap_remove(k);
            if dirty {
                self.store_word(arm, guest);
            }
        }
    }

    /// Takes CONDITION out of the pool for the branch's condition, the guest
    /// register it held stored first if changed.
    fn reserve_condition(&mut self) {
        self.evict(CONDITION);
        self.reserved |= 1 << CONDITION;
    }

    /// An instruction run by the interpreter from here; the block ends if
    /// it says so. The interpreter then accounts this one too. It reads and
    /// may write any guest register, in Cpu.
    fn interpreted(&mut self, i: Instr) {
        self.flush();
        self.held.clear();
        self.e.mov(R0, CTX);
        self.e.mov_imm(R1, (self.instructions - self.synced) as u32);
        self.e.mov_imm(R2, i.word);
        self.e.mov_imm(R3, self.source.0 | self.source.1 << 23);
        self.e.mov_imm(R12, self.interpret);
        self.e.blx(R12);
        self.cmp_imm(R0, 0);
        self.stops.push(self.e.b(Cond::Ne));
        self.synced = self.instructions + 1;
    }

    /// A guest register's low word into `arm` (r0-r3).
    fn low(&mut self, arm: Reg, guest: usize) {
        if guest == 0 {
            self.e.mov_imm(arm, 0);
        } else if let Some(k) = self.holding(guest) {
            let held = self.touch(k);
            self.e.mov(arm, held);
        } else {
            self.e.ldr(arm, CPU, reg(guest));
        }
    }

    /// A guest register's words into `lo` and `hi` (r0-r3).
    fn pair(&mut self, lo: Reg, hi: Reg, guest: usize) {
        if guest == 0 {
            self.e.mov_imm(lo, 0);
            self.e.mov_imm(hi, 0);
        } else if let Some(k) = self.holding(guest) {
            let word = self.held[k].word;
            let held = self.touch(k);
            self.e.mov(lo, held);
            if word {
                self.shift(hi, held, Shift::Asr(31));
            } else {
                self.e.ldr(hi, CPU, reg(guest) + 4);
            }
        } else if lo % 2 == 0 && hi == lo + 1 && (0..256).contains(&reg(guest)) {
            self.e.ldrd(lo, CPU, reg(guest) as u32);
        } else {
            self.e.ldr(lo, CPU, reg(guest));
            self.e.ldr(hi, CPU, reg(guest) + 4);
        }
    }

    fn constant(&mut self, lo: Reg, hi: Reg, value: u64) {
        self.e.mov_imm(lo, value as u32);
        self.e.mov_imm(hi, (value >> 32) as u32);
    }

    /// The result in r0:r1, without ever changing the guest's zero register.
    fn result(&mut self, guest: usize) {
        if guest != 0 {
            self.drop_guest(guest);
            let at = reg(guest);
            if (0..256).contains(&at) {
                self.e.strd(R0, CPU, at as u32);
            } else {
                self.e.str(R0, CPU, at);
                self.e.str(R1, CPU, at + 4);
            }
        }
    }

    /// r0 as a 32-bit result: its sign extension.
    fn word_result(&mut self, guest: usize) {
        if guest != 0 {
            let arm = self.write(guest);
            self.e.mov(arm, R0);
        }
    }

    fn shift(&mut self, dst: Reg, src: Reg, shift: Shift) {
        self.e.alu_shifted(Cond::Al, Op::Mov, false, dst, 0, src, shift);
    }

    fn cmp(&mut self, left: Reg, right: Reg) {
        self.e.alu_shifted(Cond::Al, Op::Cmp, true, 0, left, right, Shift::Lsl(0));
    }

    fn cmp_imm(&mut self, left: Reg, immediate: u32) {
        assert!(self.e.alu_imm(Cond::Al, Op::Cmp, true, 0, left, immediate));
    }

    fn immediate(&mut self, op: Op, dst: Reg, src: Reg, value: u32) {
        if !self.e.alu_imm(Cond::Al, op, false, dst, src, value) {
            self.e.mov_imm(R12, value);
            self.e.alu(op, dst, src, R12);
        }
    }

    fn add_pair_immediate(&mut self, lo: Reg, hi: Reg, value: i64) {
        if value == 0 {
            return;
        }
        let (first, second, magnitude) = if value > 0 {
            (Op::Add, Op::Adc, value as u32)
        } else {
            (Op::Sub, Op::Sbc, value.wrapping_neg() as u32)
        };
        if !self.e.alu_imm(Cond::Al, first, true, lo, lo, magnitude) {
            self.e.mov_imm(R12, magnitude);
            self.e.alu_shifted(Cond::Al, first, true, lo, lo, R12, Shift::Lsl(0));
        }
        self.e.alu_imm(Cond::Al, second, false, hi, hi, 0);
    }

    /// `from_lo` and `from_hi` plus a signed 32-bit value into `lo` and
    /// `hi`. Uses r12.
    fn offset_pair(&mut self, lo: Reg, hi: Reg, from_lo: Reg, from_hi: Reg, value: i64) {
        let (first, second, magnitude) = if value >= 0 {
            (Op::Add, Op::Adc, value as u32)
        } else {
            (Op::Sub, Op::Sbc, value.wrapping_neg() as u32)
        };
        if self.e.alu_imm(Cond::Al, first, true, lo, from_lo, magnitude) {
            assert!(self.e.alu_imm(Cond::Al, second, false, hi, from_hi, 0));
        } else {
            self.e.mov(lo, from_lo);
            self.e.mov(hi, from_hi);
            self.add_pair_immediate(lo, hi, value);
        }
    }

    fn write_pair(&mut self, lo: Reg, hi: Reg, at: i32) {
        if lo % 2 == 0 && hi == lo + 1 && (0..256).contains(&at) {
            self.e.strd(lo, CPU, at as u32);
        } else {
            self.e.str(lo, CPU, at);
            self.e.str(hi, CPU, at + 4);
        }
    }

    /// The start of the block's branch: the guest registers changed into Cpu,
    /// so that every way on (a likely branch's slot runs on one) starts from
    /// Cpu, and CONDITION out of the pool.
    fn branch_start(&mut self) {
        self.flush();
        self.reserve_condition();
    }

    /// Preserve a branch's decision in CONDITION before a possible link
    /// changes rs.
    fn branch_condition(&mut self, i: Instr) {
        use Mips::*;
        if matches!(i.op, Bc1f | Bc1t | Bc1fl | Bc1tl) {
            // The FPU's: unusable without CU1 (an exit before); FCR31's
            // cause cleared, then its condition bit.
            self.cop1_usable();
            self.e.ldr(R12, CPU, FCR31);
            assert!(self.e.alu_imm(Cond::Al, Op::Bic, false, R12, R12, 63 << 12));
            self.e.str(R12, CPU, FCR31);
            self.shift(CONDITION, R12, Shift::Lsr(23));
            self.immediate(Op::And, CONDITION, CONDITION, 1);
            if matches!(i.op, Bc1f | Bc1fl) {
                assert!(self.e.alu_imm(Cond::Al, Op::Eor, false, CONDITION, CONDITION, 1));
            }
            return;
        }
        let compared = matches!(i.op, Beq | Bne | Beql | Bnel);
        if self.known_word(i.rs()) && (!compared || self.known_word(i.rt())) {
            // Sign-extended words: their low words decide.
            let a = self.source(i.rs(), R0);
            if compared {
                let b = self.source(i.rt(), R2);
                self.cmp(a, b);
            } else {
                self.cmp_imm(a, 0);
            }
            let cond = match i.op {
                Beq | Beql => Cond::Eq,
                Bne | Bnel => Cond::Ne,
                Bltz | Bltzl | Bltzal | Bltzall => Cond::Lt,
                Bgez | Bgezl | Bgezal | Bgezall => Cond::Ge,
                Blez | Blezl => Cond::Le,
                _ => Cond::Gt,
            };
            self.e.mov_imm(CONDITION, 0);
            assert!(self.e.alu_imm(cond, Op::Mov, false, CONDITION, 0, 1));
            return;
        }
        self.pair(R0, R1, i.rs());
        self.e.mov_imm(CONDITION, 0);
        match i.op {
            Beq | Bne | Beql | Bnel => {
                if i.rt() != 0 {
                    self.pair(R2, R3, i.rt());
                    self.e.alu(Op::Eor, R0, R0, R2);
                    self.e.alu(Op::Eor, R1, R1, R3);
                }
                self.e.alu_shifted(Cond::Al, Op::Orr, true, R12, R0, R1, Shift::Lsl(0));
                self.e.alu_imm(if matches!(i.op, Beq | Beql) { Cond::Eq } else { Cond::Ne }, Op::Mov, false, CONDITION, 0, 1);
            }
            Bltz | Bltzl | Bltzal | Bltzall | Bgez | Bgezl | Bgezal | Bgezall => {
                self.cmp_imm(R1, 0);
                self.e.alu_imm(if matches!(i.op, Bltz | Bltzl | Bltzal | Bltzall) { Cond::Mi } else { Cond::Pl }, Op::Mov, false, CONDITION, 0, 1);
            }
            Blez | Blezl => {
                self.cmp_imm(R1, 0);
                self.e.alu_imm(Cond::Mi, Op::Mov, false, CONDITION, 0, 1);
                self.e.alu_shifted(Cond::Al, Op::Orr, true, R12, R0, R1, Shift::Lsl(0));
                self.e.alu_imm(Cond::Eq, Op::Mov, false, CONDITION, 0, 1);
            }
            _ => {
                // BGTZ: nonnegative, then exclude a full 64-bit zero.
                self.cmp_imm(R1, 0);
                self.e.alu_imm(Cond::Pl, Op::Mov, false, CONDITION, 0, 1);
                self.e.alu_shifted(Cond::Al, Op::Orr, true, R12, R0, R1, Shift::Lsl(0));
                self.e.alu_imm(Cond::Eq, Op::Mov, false, CONDITION, 0, 0);
            }
        }
    }

    /// The live entry PC plus `offset` into `lo` and `hi` (r0 or r2 and the
    /// next): a translation is cached by physical source address, so every
    /// PC, target and link comes from the PC in Cpu, which the block leaves
    /// as it was until its branch's control state.
    fn pc_pair(&mut self, lo: Reg, hi: Reg, offset: i64) {
        assert_eq!(hi, lo + 1);
        self.e.ldrd(lo, CPU, CPU_PC as u32);
        self.add_pair_immediate(lo, hi, offset);
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

    /// The start of a branch at `b` bytes past the entry PC: the guest
    /// registers into Cpu, its decision in CONDITION (a jump to a register:
    /// its target into Cpu's next PC, where the control state takes it
    /// from), then its link written, which the slot may read. In JALR rd ==
    /// rs, the target is the old value of that register.
    fn branch_head(&mut self, i: Instr, b: i64) {
        use Mips::*;
        self.branch_start();
        match i.op {
            Jr | Jalr => {
                self.pair(R2, R3, i.rs());
                self.e.strd(R2, CPU, NEXT_PC as u32);
            }
            J | Jal => {}
            _ => self.branch_condition(i),
        }
        let link = Self::linked_register(i);
        if link != 0 {
            // REGIMM links are written even when the branch is not taken.
            self.drop_guest(link);
            self.pc_pair(R0, R1, b + 8);
            self.write_pair(R0, R1, reg(link));
        }
    }

    /// A jump's target in its slot's 256 MB region, with the slot's PC in r0
    /// and r1: into `lo` and `hi`. Uses r12.
    fn jump_target(&mut self, i: Instr, lo: Reg, hi: Reg) {
        self.shift(lo, R0, Shift::Lsr(28));
        self.shift(lo, lo, Shift::Lsl(28));
        self.immediate(Op::Orr, lo, lo, i.target() as u32);
        if hi != R1 {
            self.e.mov(hi, R1);
        }
    }

    /// The control state after a branch at `b` (as the interpreter leaves
    /// it), its slot still to run: the branch the last instruction run, its
    /// slot next, in a delay slot, then the target, or B + 8 where the branch
    /// is not taken (CONDITION 0). A likely branch's slot runs only taken.
    /// Uses r0-r3, r12 and the flags.
    fn before_slot(&mut self, i: Instr, b: i64) {
        use Mips::*;
        self.pc_pair(R0, R1, b);
        self.e.strd(R0, CPU, CURRENT_PC as u32);
        self.e.mov_imm(R12, 1);
        self.e.strh(R12, CPU, DELAY_SLOT);
        self.add_pair_immediate(R0, R1, 4);
        match i.op {
            Jr | Jalr => {
                // The target is in the next PC already.
                self.e.strd(R0, CPU, CPU_PC as u32);
                return;
            }
            J | Jal => self.jump_target(i, R2, R3),
            _ => {
                self.offset_pair(R2, R3, R0, R1, (i.simm() as i64).wrapping_shl(2));
                if !is_likely(i.op) {
                    self.cmp_imm(CONDITION, 0);
                    let taken = self.e.b(Cond::Ne);
                    self.offset_pair(R2, R3, R0, R1, 4);
                    self.e.patch_here(taken);
                }
            }
        }
        self.e.stm(CPU, 0xf);
    }

    /// The control state after a branch at `b` and its slot (as `step_cpu`
    /// leaves it): the slot the last instruction run, in a delay slot, and
    /// next the target, or B + 8 where the branch was not taken (CONDITION
    /// 0; a likely branch's slot runs only taken). Leaves the PC in r0 and
    /// r1. Uses r0-r3, r12 and the flags.
    fn after_slot(&mut self, i: Instr, b: i64) {
        use Mips::*;
        self.pc_pair(R0, R1, b + 4);
        self.e.strd(R0, CPU, CURRENT_PC as u32);
        self.e.mov_imm(R12, 0x100);
        self.e.strh(R12, CPU, DELAY_SLOT);
        match i.op {
            Jr | Jalr => self.e.ldrd(R0, CPU, NEXT_PC as u32),
            J | Jal => self.jump_target(i, R0, R1),
            _ if is_likely(i.op) => self.add_pair_immediate(R0, R1, (i.simm() as i64).wrapping_shl(2)),
            _ => {
                self.offset_pair(R2, R3, R0, R1, (i.simm() as i64).wrapping_shl(2));
                self.add_pair_immediate(R0, R1, 4);
                self.cmp_imm(CONDITION, 0);
                self.e.alu_shifted(Cond::Ne, Op::Mov, false, R0, 0, R2, Shift::Lsl(0));
                self.e.alu_shifted(Cond::Ne, Op::Mov, false, R1, 0, R3, Shift::Lsl(0));
            }
        }
        self.offset_pair(R2, R3, R0, R1, 4);
        self.e.stm(CPU, 0xf);
    }

    /// The control state after a likely branch at `b` not taken: its slot
    /// annulled, the branch the last instruction run and B + 8 next. Leaves
    /// the PC in r0 and r1. Uses r0-r3, r12 and the flags.
    fn annulled_slot(&mut self, b: i64) {
        self.pc_pair(R0, R1, b);
        self.e.strd(R0, CPU, CURRENT_PC as u32);
        self.e.mov_imm(R12, 0);
        self.e.strh(R12, CPU, DELAY_SLOT);
        self.add_pair_immediate(R0, R1, 8);
        self.offset_pair(R2, R3, R0, R1, 4);
        self.e.stm(CPU, 0xf);
    }

    /// The block's branch where its slot is left to the dispatcher (which
    /// runs it in the interpreter): the control state as the branch leaves
    /// it.
    fn branch(&mut self, i: Instr) {
        let b = 4 * (self.instructions - self.synced) as i64;
        self.branch_head(i, b);
        if is_likely(i.op) {
            // Not taken, the slot is annulled, without a cycle, as
            // Cpu::branch_likely does.
            self.cmp_imm(CONDITION, 0);
            let annulled = self.e.b(Cond::Eq);
            self.before_slot(i, b);
            let done = self.e.b(Cond::Al);
            self.e.patch_here(annulled);
            self.annulled_slot(b);
            self.e.patch_here(done);
        } else {
            self.before_slot(i, b);
        }
    }

    /// The block's branch and its slot, which is plain arithmetic, an RDRAM
    /// load or store or a native FPU instruction: the control state written
    /// once, after the slot, as `step_cpu` leaves it. The slot's exits write
    /// the state as the branch leaves it (`stubs`, `before_slot`): the
    /// interpreter then does the slot; a store into code ends the block with
    /// the state after the slot (`after_slot`). A likely branch not taken
    /// annuls the slot.
    fn terminal(&mut self, i: Instr, slot: Instr) {
        let b = 4 * (self.instructions - self.synced) as i64;
        self.branch_head(i, b);
        self.instructions += 1;
        let likely = is_likely(i.op);
        let annulled = likely.then(|| {
            self.cmp_imm(CONDITION, 0);
            self.e.b(Cond::Eq)
        });
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

    /// The end of a block whose branch and slot ran in it, with the PC it
    /// went to in r0 and r1 (and the condition in CONDITION): its result
    /// into r2 (a likely branch not taken does not count its slot), and to
    /// the link routine (`link_routine`) with the address of the links to
    /// try in r12, which follow: for a jump one set, for a conditional
    /// branch two (taken, then not taken), for a jump to a register one set
    /// of four. A set is the count of its links, the one the dispatcher
    /// replaces next when all are taken, then for each the PC (its low word;
    /// an odd one is none), the generation it was made at (doubled; an odd
    /// one is none), the target's code past its prologue and its
    /// instructions, all set by the dispatcher.
    fn link(&mut self, i: Instr, result: u32) {
        use Mips::*;
        let (conditional, scan) = (!matches!(i.op, J | Jal | Jr | Jalr), matches!(i.op, Jr | Jalr));
        self.e.mov_imm(R2, result);
        if conditional || self.annulled {
            self.cmp_imm(CONDITION, 0);
        }
        if self.annulled {
            assert!(self.e.alu_imm(Cond::Eq, Op::Sub, false, R2, R2, 1));
        }
        // The PC reads 8 bytes ahead.
        assert!(self.e.alu_imm(Cond::Al, Op::Add, false, R12, PC, if conditional { 4 } else { 0 }));
        if conditional {
            assert!(self.e.alu_imm(Cond::Eq, Op::Add, false, R12, R12, 24));
        }
        self.routine(if scan { LINK_SCAN } else { LINK_ROUTINE }, false);
        let (sets, links) = if scan { (1, 4) } else if conditional { (2, 1) } else { (1, 1) };
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

    /// The guest register 1 if the flags say less (signed or not), else 0.
    /// Allocating its register keeps the flags.
    fn set_less(&mut self, guest: usize, signed: bool) {
        let d = self.write(guest);
        self.e.mov_imm(d, 0);
        assert!(self.e.alu_imm(if signed { Cond::Lt } else { Cond::Lo }, Op::Mov, false, d, 0, 1));
    }

    fn comparison(&mut self, signed: bool, guest: usize) {
        // Subtract low words first, then the high words with that borrow.
        // N/V in the high subtraction describe signed 64-bit ordering;
        // carry describes unsigned ordering. No need to combine Z flags.
        self.cmp(R0, R2);
        self.e.alu_shifted(Cond::Al, Op::Sbc, true, R12, R1, R3, Shift::Lsl(0));
        self.e.mov_imm(R0, 0);
        self.e.mov_imm(R1, 0);
        self.e.alu_imm(if signed { Cond::Lt } else { Cond::Lo }, Op::Mov, false, R0, 0, 1);
        self.result(guest);
    }

    fn shift_double_immediate(&mut self, direction: Direction, count: u32) {
        if count == 0 {
            return;
        }
        if count < 32 {
            match direction {
                Direction::Left => {
                    self.shift(R12, R0, Shift::Lsr(32 - count));
                    self.shift(R1, R1, Shift::Lsl(count));
                    self.e.alu(Op::Orr, R1, R1, R12);
                    self.shift(R0, R0, Shift::Lsl(count));
                }
                Direction::Right | Direction::Arithmetic => {
                    self.shift(R12, R1, Shift::Lsl(32 - count));
                    self.shift(R0, R0, Shift::Lsr(count));
                    self.e.alu(Op::Orr, R0, R0, R12);
                    self.shift(R1, R1, match direction {
                        Direction::Arithmetic => Shift::Asr(count),
                        _ => Shift::Lsr(count),
                    });
                }
            }
        } else {
            let rest = count - 32;
            match direction {
                Direction::Left => {
                    self.shift(R1, R0, Shift::Lsl(rest));
                    self.e.mov_imm(R0, 0);
                }
                Direction::Right | Direction::Arithmetic => {
                    if rest == 0 {
                        self.e.mov(R0, R1);
                    } else {
                        self.shift(R0, R1, match direction {
                            Direction::Arithmetic => Shift::Asr(rest),
                            _ => Shift::Lsr(rest),
                        });
                    }
                    match direction {
                        Direction::Arithmetic => self.shift(R1, R1, Shift::Asr(31)),
                        _ => self.e.mov_imm(R1, 0),
                    }
                }
            }
        }
    }

    /// r0:r1 shifted by r2's low six bits. The explicit large-count path
    /// also avoids A32's #0 encoding meaning #32 for LSR and ASR.
    fn shift_double_variable(&mut self, direction: Direction) {
        self.immediate(Op::And, R2, R2, 63);
        self.cmp_imm(R2, 32);
        let large = self.e.b(Cond::Hs);
        self.e.mov_imm(R3, 32);
        self.e.alu(Op::Sub, R3, R3, R2);
        match direction {
            Direction::Left => {
                self.shift(R12, R0, Shift::LsrReg(R3));
                self.shift(R1, R1, Shift::LslReg(R2));
                self.e.alu(Op::Orr, R1, R1, R12);
                self.shift(R0, R0, Shift::LslReg(R2));
            }
            Direction::Right | Direction::Arithmetic => {
                self.shift(R12, R1, Shift::LslReg(R3));
                self.shift(R0, R0, Shift::LsrReg(R2));
                self.e.alu(Op::Orr, R0, R0, R12);
                self.shift(R1, R1, match direction {
                    Direction::Arithmetic => Shift::AsrReg(R2),
                    _ => Shift::LsrReg(R2),
                });
            }
        }
        let done = self.e.b(Cond::Al);
        self.e.patch_here(large);
        self.immediate(Op::Sub, R2, R2, 32);
        match direction {
            Direction::Left => {
                self.shift(R1, R0, Shift::LslReg(R2));
                self.e.mov_imm(R0, 0);
            }
            Direction::Right | Direction::Arithmetic => {
                self.shift(R0, R1, match direction {
                    Direction::Arithmetic => Shift::AsrReg(R2),
                    _ => Shift::LsrReg(R2),
                });
                match direction {
                    Direction::Arithmetic => self.shift(R1, R1, Shift::Asr(31)),
                    _ => self.e.mov_imm(R1, 0),
                }
            }
        }
        self.e.patch_here(done);
    }

    fn hilo_words(&mut self) {
        // r0 is LO and r1 is HI, each a signed 32-bit result, including
        // the halves of MULTU and DIVU as the VR4300 stores them.
        self.e.str(R0, CPU, LO);
        self.e.str(R1, CPU, HI);
        self.shift(R2, R0, Shift::Asr(31));
        self.e.str(R2, CPU, LO + 4);
        self.shift(R2, R1, Shift::Asr(31));
        self.e.str(R2, CPU, HI + 4);
    }

    fn divide_word(&mut self, signed: bool) {
        self.cmp_imm(R1, 0);
        let zero = self.e.b(Cond::Eq);
        if signed {
            self.e.sdiv(R2, R0, R1);
        } else {
            self.e.udiv(R2, R0, R1);
        }
        self.e.mul(R3, R2, R1);
        self.e.alu(Op::Sub, R3, R0, R3);
        self.e.mov(R0, R2);
        self.e.mov(R1, R3);
        let done = self.e.b(Cond::Al);
        self.e.patch_here(zero);
        self.e.mov(R1, R0);
        self.e.mov_imm(R0, u32::MAX);
        if signed {
            self.cmp_imm(R1, 0);
            self.e.alu_imm(Cond::Mi, Op::Mov, false, R0, 0, 1);
        }
        self.e.patch_here(done);
        self.hilo_words();
    }

    /// Guard a complete effective address, leaving its RDRAM offset in r2:
    /// an exit before the instruction unless it is a 32-bit kseg0 or kseg1
    /// address of RDRAM, aligned, in a piece of RDRAM the RDP's thread is not
    /// still drawing to (`guard_routine`, shared: less code for the host's
    /// caches); for a store, whether its page holds code in r3. Loads and
    /// stores share every check; no destination/value register is read until
    /// the address is resolved, so rs == rt is safe too. Uses r0-r3, r12, LR
    /// and the flags.
    fn rdram_address(&mut self, i: Instr, size: u32, store: bool) {
        // The base is the sign extension of its low word, or an exit (`word`):
        // no other makes a kseg0 or kseg1 address but one just below kseg0
        // with an offset that carries into it, which code does not use (the
        // interpreter performs that access). The address is then the low
        // words' sum sign-extended unless that overflows (V), which no
        // kseg0 or kseg1 address does.
        let base = if i.rs() == 0 {
            self.e.mov_imm(R2, 0);
            R2
        } else {
            self.word(i.rs())
        };
        let offset = i.simm() as i32;
        if !(offset >= 0 && self.e.alu_imm(Cond::Al, Op::Add, true, R2, base, offset as u32)
            || offset < 0 && self.e.alu_imm(Cond::Al, Op::Sub, true, R2, base, offset.unsigned_abs()))
        {
            self.e.mov_imm(R3, offset as u32);
            self.e.alu_shifted(Cond::Al, Op::Add, true, R2, base, R3, Shift::Lsl(0));
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
        match i.op {
            Lwu => {
                self.e.ldr_reg(R0, RAM, R2);
                self.e.rev(R0, R0);
                self.e.mov_imm(R1, 0);
                self.result(rt);
            }
            Ld => {
                self.e.ldr_reg(R1, RAM, R2);
                self.e.add_imm(R2, R2, 4, R12);
                self.e.ldr_reg(R0, RAM, R2);
                self.e.rev(R0, R0);
                self.e.rev(R1, R1);
                self.result(rt);
            }
            // Only the guards' exits (reading RDRAM has no effect).
            _ if rt == 0 => {}
            // The rest are 32-bit results, unsigned bytes and halves too.
            Lb => {
                let d = self.write(rt);
                self.e.ldrsb_reg(d, RAM, R2);
            }
            Lbu => {
                let d = self.write(rt);
                self.e.ldrb_reg(d, RAM, R2);
            }
            Lh | Lhu => {
                let d = self.write(rt);
                self.e.ldrh_reg(d, RAM, R2);
                self.e.rev16(d, d);
                if i.op == Lh {
                    self.e.sxth(d, d);
                }
            }
            _ => {
                let d = self.write(rt);
                self.e.ldr_reg(d, RAM, R2);
                self.e.rev(d, d);
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
        // r2 holds the address, independent of the value's register, which
        // is read next (an FPU one may need the flags for Status.FR).
        let value = match i.op {
            Swc1 => {
                self.fpr(true, R0, None, i.ft());
                R0
            }
            Sdc1 => {
                self.fpr(true, R0, Some(R1), i.ft());
                R0
            }
            Sd => {
                self.pair(R0, R1, i.rt());
                R0
            }
            _ => self.source(i.rt(), R0),
        };
        // The dispatcher permits this path only with MI_REPEAT clear. A
        // store into a piece of code (the block's own among them, r3 from
        // the guard) ends it, this store included: the flags stay through
        // the store, which sets none.
        self.cmp_imm(R3, 0);
        match size {
            1 => self.e.strb_reg(value, RAM, R2),
            2 => {
                self.e.rev16(R0, value);
                self.e.strh_reg(R0, RAM, R2);
            }
            4 => {
                self.e.rev(R0, value);
                self.e.str_reg(R0, RAM, R2);
            }
            _ => {
                self.e.rev(R1, R1);
                self.e.str_reg(R1, RAM, R2);
                self.e.add_imm(R2, R2, 4, R12);
                self.e.rev(R0, R0);
                self.e.str_reg(R0, RAM, R2);
            }
        }
        self.code_exit(Cond::Ne);
    }

    fn load_unaligned(&mut self, i: Instr) {
        use Mips::*;
        self.rdram_address(i, 1, false);
        let rt = i.rt();
        if rt == 0 {
            return;
        }
        let old = self.source(rt, R0);
        self.e.alu_imm(Cond::Al, Op::Bic, false, R1, R2, 3);
        self.e.ldr_reg(R12, RAM, R1);
        self.e.rev(R12, R12);
        self.e.alu_imm(Cond::Al, Op::And, false, R3, R2, 3);
        if i.op == Lwl {
            self.e.alu_shifted(Cond::Al, Op::Mov, false, R3, 0, R3, Shift::Lsl(3));
            self.e.alu_shifted(Cond::Al, Op::Mov, false, R12, 0, R12, Shift::LslReg(R3));
            self.e.mov_imm(R1, 0xffff_ffff);
            self.e.alu_shifted(Cond::Al, Op::Mov, false, R1, 0, R1, Shift::LslReg(R3));
            self.e.alu(Op::Bic, R1, old, R1);
            let d = self.write(rt);
            self.e.alu(Op::Orr, d, R1, R12);
        } else {
            self.e.alu_imm(Cond::Al, Op::Rsb, false, R3, R3, 3);
            self.e.alu_shifted(Cond::Al, Op::Mov, false, R3, 0, R3, Shift::Lsl(3));
            self.e.alu_shifted(Cond::Al, Op::Mov, false, R12, 0, R12, Shift::LsrReg(R3));
            self.e.mov_imm(R1, 0xffff_ffff);
            self.e.alu_shifted(Cond::Al, Op::Mov, false, R1, 0, R1, Shift::LsrReg(R3));
            self.e.alu(Op::Bic, R1, old, R1);
            let d = self.write(rt);
            self.e.alu(Op::Orr, d, R1, R12);
        }
    }

    fn store_unaligned(&mut self, i: Instr) {
        use Mips::*;
        self.rdram_address(i, 1, true);
        let value = self.source(i.rt(), R0);
        self.cmp_imm(R3, 0);
        self.e.alu_imm(Cond::Al, Op::Bic, false, R1, R2, 3);
        self.e.ldr_reg(R12, RAM, R1);
        self.e.rev(R12, R12);
        self.e.alu_imm(Cond::Al, Op::And, false, R2, R2, 3);
        if i.op == Swl {
            self.e.alu_shifted(Cond::Al, Op::Mov, false, R2, 0, R2, Shift::Lsl(3));
            self.e.mov_imm(R3, 0xffff_ffff);
            self.e.alu_shifted(Cond::Al, Op::Mov, false, R3, 0, R3, Shift::LsrReg(R2));
            self.e.alu(Op::Bic, R12, R12, R3);
            self.e.alu_shifted(Cond::Al, Op::Mov, false, R3, 0, value, Shift::LsrReg(R2));
            self.e.alu(Op::Orr, R12, R12, R3);
        } else {
            self.e.alu_imm(Cond::Al, Op::Rsb, false, R2, R2, 3);
            self.e.alu_shifted(Cond::Al, Op::Mov, false, R2, 0, R2, Shift::Lsl(3));
            self.e.mov_imm(R3, 0xffff_ffff);
            self.e.alu_shifted(Cond::Al, Op::Mov, false, R3, 0, R3, Shift::LslReg(R2));
            self.e.alu(Op::Bic, R12, R12, R3);
            self.e.alu_shifted(Cond::Al, Op::Mov, false, R3, 0, value, Shift::LslReg(R2));
            self.e.alu(Op::Orr, R12, R12, R3);
        }
        self.e.rev(R12, R12);
        self.e.str_reg(R12, RAM, R1);
        self.code_exit(Cond::Ne);
    }

    fn load_double_unaligned(&mut self, i: Instr) {
        if i.rt() == 0 {
            return;
        }
        self.rdram_address(i, 1, false);
        self.flush();
        self.drop_guest(i.rt());
        self.e.mov(R0, CPU);
        self.e.mov_imm(R1, i.rt() as u32);
        self.e.mov(R3, RAM);
        let target = if i.op == Mips::Ldl { super::ldl as *const () as usize as u32 } else { super::ldr as *const () as usize as u32 };
        self.e.mov_imm(R12, target);
        self.e.blx(R12);
    }

    fn store_double_unaligned(&mut self, i: Instr) {
        self.rdram_address(i, 1, true);
        self.flush();
        self.reserve_condition();
        self.e.mov(CONDITION, R3);
        self.e.mov(R0, CPU);
        self.e.mov_imm(R1, i.rt() as u32);
        self.e.mov(R3, RAM);
        let target = if i.op == Mips::Sdl { super::sdl as *const () as usize as u32 } else { super::sdr as *const () as usize as u32 };
        self.e.mov_imm(R12, target);
        self.e.blx(R12);
        self.cmp_imm(CONDITION, 0);
        self.code_exit(Cond::Ne);
    }

    /// An exit before the FPU's first instruction in the block if Status.CU1
    /// is clear, for the interpreter to raise the exception. Nothing the
    /// block goes on after changes Status (MTC0, ERET and exceptions stop
    /// it), so the instructions after that one need no check.
    fn cop1_usable(&mut self) {
        if self.cop1_checked {
            return;
        }
        self.e.ldr(R12, CPU, STATUS);
        assert!(self.e.alu_imm(Cond::Al, Op::Tst, true, 0, R12, CU1));
        self.exit(Cond::Eq);
        self.cop1_checked = true;
    }

    /// An exit before the first COP0 instruction in the block if COP0 is
    /// unusable (outside kernel mode without Status.CU0), for the interpreter
    /// to raise the exception. The block changes no bit of Status that
    /// decides it (`status_write`).
    fn cop0_usable(&mut self) {
        if self.cop0_checked {
            return;
        }
        self.e.ldr(R12, CPU, STATUS);
        assert!(self.e.alu_imm(Cond::Al, Op::Tst, true, 0, R12, 0x18));
        let kernel = self.e.b(Cond::Eq);
        assert!(self.e.alu_imm(Cond::Al, Op::Tst, true, 0, R12, EXL | ERL));
        let exception = self.e.b(Cond::Ne);
        assert!(self.e.alu_imm(Cond::Al, Op::Tst, true, 0, R12, CU0));
        self.exit(Cond::Eq);
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
        if i.op == Mips::Mfc0 {
            let d = self.write(i.rt());
            self.e.ldr(d, CPU, offset);
        } else if double {
            self.e.ldr(R0, CPU, offset);
            self.e.ldr(R1, CPU, offset + 4);
            self.result(i.rt());
        } else {
            self.e.ldr(R0, CPU, offset);
            self.e.mov_imm(R1, 0);
            self.result(i.rt());
        }
    }

    /// MFC0 and DMFC0 of Count (register 9).
    #[allow(dead_code)]
    fn count_read(&mut self, i: Instr) {
        self.cop0_usable();
        if i.rt() != 0 {
            self.e.mov(R0, CTX);
            self.e.mov_imm(R1, (self.instructions - self.synced) as u32);
            self.e.mov_imm(R12, super::mfc0_count as *const () as usize as u32);
            self.e.blx(R12);
            if i.op == Mips::Dmfc0 {
                self.e.mov_imm(R1, 0);
                self.result(i.rt());
            } else {
                let d = self.write(i.rt());
                self.e.mov(d, R0);
            }
        }
    }

    /// 64-bit multiplication: DMULT, DMULTU.
    fn dmult(&mut self, i: Instr) {
        self.flush();
        self.e.mov(R0, CPU);
        self.e.mov_imm(R1, i.rs() as u32);
        self.e.mov_imm(R2, i.rt() as u32);
        let target = if i.op == Mips::Dmult { super::dmult as *const () as usize as u32 } else { super::dmultu as *const () as usize as u32 };
        self.e.mov_imm(R12, target);
        self.e.blx(R12);
    }

    /// 64-bit division: DDIV, DDIVU.
    fn ddiv(&mut self, i: Instr) {
        self.flush();
        self.e.mov(R0, CPU);
        self.e.mov_imm(R1, i.rs() as u32);
        self.e.mov_imm(R2, i.rt() as u32);
        let target = if i.op == Mips::Ddiv { super::ddiv as *const () as usize as u32 } else { super::ddivu as *const () as usize as u32 };
        self.e.mov_imm(R12, target);
        self.e.blx(R12);
    }

    /// MFC0 and DMFC0 of a register that reads as a constant (`constant_register`), each of them a
    /// positive word, sign-extended as 32-bit results are.
    fn constant_read(&mut self, i: Instr) {
        self.cop0_usable();
        if i.rt() != 0 {
            let value = constant_register(i.rd()).unwrap() as u32;
            let d = self.write(i.rt());
            self.e.mov_imm(d, value);
        }
    }

    /// MTC0 to Status (`write_cop0`), where it changes only IE and the
    /// interrupt mask: otherwise an exit before it, as a change of mode,
    /// mapping or coprocessor use stops a block. The value also goes into
    /// the latch. An interrupt it lets through ends the block after it, for
    /// the dispatcher to take. Uses r0-r2, r12 and the flags.
    fn status_write(&mut self, i: Instr) {
        self.cop0_usable();
        let value = self.source(i.rt(), R0);
        assert!(self.e.alu_imm(Cond::Al, Op::Bic, false, R1, value, !0xff57_ffff));
        self.e.ldr(R12, CPU, STATUS);
        self.e.alu(Op::Eor, R2, R1, R12);
        assert!(self.e.alu_imm(Cond::Al, Op::Bic, false, R2, R2, 0xff00));
        assert!(self.e.alu_imm(Cond::Al, Op::Bic, true, R2, R2, IE));
        self.exit(Cond::Ne);
        self.e.str(R1, CPU, STATUS);
        self.e.str(value, CPU, LATCH);
        self.shift(R12, value, Shift::Asr(31));
        self.e.str(R12, CPU, LATCH + 4);
        // Pending (`Cop0::interrupt_pending`): IE without EXL and ERL, and a
        // cause the mask lets through.
        self.e.ldr(R12, CPU, CAUSE);
        self.e.alu(Op::And, R12, R12, R1);
        assert!(self.e.alu_imm(Cond::Al, Op::And, false, R2, R1, IE | EXL | ERL));
        self.cmp_imm(R2, IE);
        assert!(self.e.alu_imm(Cond::Ne, Op::Mov, false, R12, 0, 0));
        assert!(self.e.alu_imm(Cond::Al, Op::Tst, true, 0, R12, 0xff00));
        self.exit_after(Cond::Ne);
    }

    /// Reads (or writes) an FPU register into `lo` as the 32-bit moves see
    /// it, or into `lo` and `hi` as the 64-bit ones do. With Status.FR
    /// clear an odd register is the top half of the even one below, or that
    /// whole even one. Uses r12 and the flags.
    fn fpr(&mut self, read: bool, lo: Reg, hi: Option<Reg>, n: usize) {
        let access = |c: &mut Self, cond: Cond, arm: Reg, at: i32| {
            if read { c.e.ldr_cond(cond, arm, CPU, at) } else { c.e.str_cond(cond, arm, CPU, at) }
        };
        let words = |c: &mut Self, cond: Cond, at: i32| {
            access(c, cond, lo, at);
            if let Some(hi) = hi {
                access(c, cond, hi, at + 4);
            }
        };
        if n & 1 == 0 {
            words(self, Cond::Al, fpr(n));
            return;
        }
        self.e.ldr(R12, CPU, STATUS);
        assert!(self.e.alu_imm(Cond::Al, Op::Tst, true, 0, R12, FR));
        words(self, Cond::Ne, fpr(n));
        words(self, Cond::Eq, fpr(n & !1) + if hi.is_some() { 0 } else { 4 });
    }

    /// CFC1 and CTC1. Moving to FCR31 a cause that is enabled (or E)
    /// raises the exception: that exits before, for the interpreter.
    fn fpu_control(&mut self, i: Instr) {
        self.cop1_usable();
        if i.op == Mips::Cfc1 {
            match i.fs() {
                31 => self.e.ldr(R0, CPU, FCR31),
                0 => self.e.mov_imm(R0, 0x0a00),
                _ => self.e.mov_imm(R0, 0),
            }
            self.word_result(i.rt());
        } else if i.fs() == 31 {
            self.low(R0, i.rt());
            self.immediate(Op::And, R0, R0, 0x0183_ffff);
            self.shift(R1, R0, Shift::Lsr(12));
            self.immediate(Op::And, R1, R1, 63);
            self.shift(R2, R0, Shift::Lsr(7));
            self.immediate(Op::And, R2, R2, 31);
            self.immediate(Op::Orr, R2, R2, 32);
            self.e.alu_shifted(Cond::Al, Op::Tst, true, 0, R1, R2, Shift::Lsl(0));
            self.exit(Cond::Ne);
            self.e.str(R0, CPU, FCR31);
        }
    }

    /// CACHE does nothing here but outside kernel mode without CU0, where it
    /// raises the coprocessor exception: that exits before.
    fn cache(&mut self) {
        self.e.ldr(R12, CPU, STATUS);
        assert!(self.e.alu_imm(Cond::Al, Op::Tst, true, 0, R12, 0x18));
        let kernel = self.e.b(Cond::Eq);
        self.e.mov_imm(R3, EXL | ERL | CU0);
        self.e.alu_shifted(Cond::Al, Op::Tst, true, 0, R12, R3, Shift::Lsl(0));
        self.exit(Cond::Eq);
        self.e.patch_here(kernel);
    }

    /// An exit unless the single in `arm` is ordinary: normal or zero (not
    /// denormal, infinite or NaN). Uses r2, r3 and the flags.
    fn ordinary(&mut self, arm: Reg) {
        self.shift(R2, arm, Shift::Lsl(1));
        assert!(self.e.alu_imm(Cond::Al, Op::Sub, false, R3, R2, 0x0100_0000));
        self.cmp_imm(R3, 0xfe00_0000);
        assert!(self.e.alu_imm(Cond::Hs, Op::Cmp, true, 0, R2, 0));
        self.exit(Cond::Hi);
    }

    /// r0: arithmetic's single fs, the even register of a pair while
    /// Status.FR is clear (its low word, unlike the moves'). Uses r12 and
    /// the flags.
    fn fs_single(&mut self, fs: usize) {
        if fs & 1 == 0 {
            self.e.ldr(R0, CPU, fpr(fs));
        } else {
            self.e.ldr(R12, CPU, STATUS);
            assert!(self.e.alu_imm(Cond::Al, Op::Tst, true, 0, R12, FR));
            self.e.ldr_cond(Cond::Ne, R0, CPU, fpr(fs));
            self.e.ldr_cond(Cond::Eq, R0, CPU, fpr(fs & !1));
        }
    }

    /// FCR31 after an operation whose only cause can be inexact (r1): the
    /// cause cleared first, then inexact in cause and flags.
    fn fcr31_inexact(&mut self) {
        self.e.ldr(R12, CPU, FCR31);
        assert!(self.e.alu_imm(Cond::Al, Op::Bic, false, R12, R12, 63 << 12));
        self.cmp_imm(R1, 0);
        assert!(self.e.alu_imm(Cond::Ne, Op::Orr, false, R12, R12, 1 << 12));
        assert!(self.e.alu_imm(Cond::Ne, Op::Orr, false, R12, R12, 1 << 2));
        self.e.str(R12, CPU, FCR31);
    }

    /// ROUND.W.S, TRUNC.W.S and CVT.W.S (that one rounding to nearest or
    /// toward zero, as FCR31 says), where VFP converts as the VR4300 does:
    /// an ordinary single (see `ordinary`) below 2^31 in size. It is inexact
    /// unless it was an integer (back in a single it compares equal), which
    /// only inexact's enable could make an exception of. Anything else exits
    /// before, for the interpreter.
    /// ROUND.W, TRUNC.W, CEIL.W, FLOOR.W and CVT.W of a single or double, as compile_a64::fpu_to_word: an
    /// ordinary value below 2^31 in size (CVT.W rounding to nearest or toward zero, as FCR31 says),
    /// inexact unless it was an integer. Anything else exits before, for the interpreter.
    fn fpu_to_word(&mut self, i: Instr) {
        let funct = i.funct();
        let double = i.fmt() == 17;
        self.cop1_usable();
        self.e.ldr(R12, CPU, FCR31);
        assert!(self.e.alu_imm(Cond::Al, Op::Tst, true, 0, R12, if funct == 36 { 0x82 } else { 0x80 }));
        self.exit(Cond::Ne);
        if double {
            self.fs_double(i.fs());
            self.ordinary_double(R0, R1, 1);
            // Below 2^31 in size: a biased exponent below 1054.
            self.shift(R2, R1, Shift::Lsl(1));
            self.e.mov_imm(R3, 1054 << 21);
            self.cmp(R2, R3);
            self.exit(Cond::Hs);
            // Positive doubles in [2^31 - 1, 2^31) can round up to 2^31:
            if funct != 13 && funct != 15 {
                self.e.mov_imm(R3, 0x41df_ffff);
                self.cmp(R1, R3);
                self.exit(Cond::Eq);
            }
            self.e.mov(R3, R1);
            self.e.vmov_to_single(0, R0);
            self.e.vmov_to_single(1, R1);
        } else {
            self.fs_single(i.fs());
            self.ordinary(R0);
            // Below 2^31 in size: a biased exponent below 158.
            self.shift(R2, R0, Shift::Lsl(1));
            self.cmp_imm(R2, 158 << 24);
            self.exit(Cond::Hs);
            self.e.mov(R3, R0);
            self.e.vmov_to_single(0, R0);
        }
        if funct == 36 {
            // FCR31's mode: toward zero (1, NE) or to nearest (0, EQ, as
            // FPSCR has it).
            self.e.ldr(R12, CPU, FCR31);
            assert!(self.e.alu_imm(Cond::Al, Op::Tst, true, 0, R12, 1));
            for (toward_zero, cond) in [(true, Cond::Ne), (false, Cond::Eq)] {
                if double {
                    self.e.vcvt_int_double(toward_zero, 4, 0);
                } else {
                    self.e.vcvt_int_single(toward_zero, 4, 0);
                }
                let at = self.e.here() - 1;
                self.e.code[at] = self.e.code[at] & 0x0fff_ffff | (cond as u32) << 28;
            }
        } else {
            // ROUND.W (12) uses FPSCR's mode (nearest).
            // TRUNC.W (13), CEIL.W (14), FLOOR.W (15) convert toward zero first.
            if double {
                self.e.vcvt_int_double(funct != 12, 4, 0);
            } else {
                self.e.vcvt_int_single(funct != 12, 4, 0);
            }
        }
        if double {
            self.e.vcvt_double_int(1, 4);
            self.e.vcmp_double(1, 0);
        } else {
            self.e.vcvt_single_int(1, 4);
            self.e.vcmp_single(1, 0);
        }
        self.e.vmrs_flags();
        self.e.mov_imm(R1, 0);
        assert!(self.e.alu_imm(Cond::Ne, Op::Mov, false, R1, 0, 1));
        self.e.vmov_from_single(R2, 4);
        if funct == 14 {
            assert!(self.e.alu_imm(Cond::Al, Op::Tst, true, 0, R3, 0x8000_0000));
            self.e.alu_cond(Cond::Eq, Op::Add, R2, R2, R1);
        } else if funct == 15 {
            assert!(self.e.alu_imm(Cond::Al, Op::Tst, true, 0, R3, 0x8000_0000));
            self.e.alu_cond(Cond::Ne, Op::Sub, R2, R2, R1);
        }
        self.fcr31_inexact();
        // A word result fills fd's 64 bits, its top half zero.
        self.e.mov_imm(R3, 0);
        self.write_pair(R2, R3, fpr(i.fd()));
    }

    /// ADD.S, SUB.S, MUL.S and DIV.S where the VR4300 would round as the
    /// host's VFP does: to nearest, ordinary operands (see `ordinary`), no
    /// division by zero, and an ordinary result that, if zero, is exact and,
    /// if of the smallest normal magnitude, exact too (the VFP rounds a
    /// value just below it up to it, where the VR4300 may find it tiny:
    /// `fpu::round`). Then inexact is the only cause there can be, so only
    /// its enable matters (libultra enables invalid). Anything else exits
    /// before, for the interpreter. Whether the result is inexact, for
    /// FCR31's cause and flags, is worked out exactly in double precision:
    /// the product of two singles is exact there, a sum's error is
    /// two_sum's, and a quotient's remainder a - q * b is exact (q * b is,
    /// and is close to a).
    fn fpu_arithmetic(&mut self, i: Instr) {
        self.cop1_usable();
        // ft is the register as named.
        self.fs_single(i.fs());
        if i.funct() != 4 {
            self.e.ldr(R1, CPU, fpr(i.ft()));
        }
        self.fpu_routine(i.funct() as usize);
        // A single result fills fd's 64 bits, its top half zero.
        self.e.mov_imm(R3, 0);
        self.write_pair(R2, R3, fpr(i.fd()));
    }

    /// A call of an FPU routine (`fpu_routine`), with
    /// the block's exit if it says so (NE).
    fn fpu_routine(&mut self, k: usize) {
        self.routine(FPU_ROUTINES + k, true);
        self.exit(Cond::Ne);
    }

    /// The body of `fpu_arithmetic`'s routine for `funct`: the operands in
    /// r0 and r1, the result in r2 and FCR31 set. Uses r3, r12 and VFP's
    /// d0-d7.
    fn arithmetic_single(&mut self, funct: u32) {
        self.e.ldr(R12, CPU, FCR31);
        assert!(self.e.alu_imm(Cond::Al, Op::Tst, true, 0, R12, 0x83));
        self.exit(Cond::Ne);
        self.ordinary(R0);
        if funct != 4 {
            self.ordinary(R1);
        }
        if funct == 3 {
            self.shift(R2, R1, Shift::Lsl(1));
            self.cmp_imm(R2, 0);
            self.exit(Cond::Eq);
        } else if funct == 4 {
            assert!(self.e.alu_imm(Cond::Al, Op::Tst, true, 0, R0, 0x8000_0000));
            let not_neg = self.e.b(Cond::Eq);
            self.shift(R2, R0, Shift::Lsl(1));
            self.cmp_imm(R2, 0);
            self.exit(Cond::Ne);
            self.e.patch_here(not_neg);
        }
        // s0, s1: the operands; s2: the result; d2, d3: the operands as
        // doubles; d4-d7: working.
        self.e.vmov_to_single(0, R0);
        if funct != 4 {
            self.e.vmov_to_single(1, R1);
        }
        self.e.vcvt_double_single(2, 0);
        if funct != 4 {
            self.e.vcvt_double_single(3, 1);
        }
        match funct {
            0 | 1 => {
                let op = if funct == 0 { Vfp::Add } else { Vfp::Sub };
                self.e.vfp_single(op, 2, 0, 1);
                // two_sum(a, ±b): x, and its error e.
                self.e.vfp_double(op, 4, 2, 3);
                self.e.vfp_double(Vfp::Sub, 5, 4, 2);
                self.e.vfp_double(Vfp::Sub, 6, 4, 5);
                self.e.vfp_double(Vfp::Sub, 6, 2, 6);
                if funct == 0 {
                    self.e.vfp_double(Vfp::Sub, 7, 3, 5);
                    self.e.vfp_double(Vfp::Add, 6, 6, 7);
                } else {
                    self.e.vfp_double(Vfp::Add, 7, 3, 5);
                    self.e.vfp_double(Vfp::Sub, 6, 6, 7);
                }
                // Inexact unless the single is x and x is exact.
                self.e.vcvt_double_single(7, 2);
                self.e.vcmp_double(7, 4);
                self.e.vmrs_flags();
                self.e.mov_imm(R1, 0);
                assert!(self.e.alu_imm(Cond::Ne, Op::Mov, false, R1, 0, 1));
                self.e.vcmp_double_zero(6);
                self.e.vmrs_flags();
                assert!(self.e.alu_imm(Cond::Ne, Op::Mov, false, R1, 0, 1));
            }
            2 => {
                self.e.vfp_single(Vfp::Mul, 2, 0, 1);
                self.e.vfp_double(Vfp::Mul, 4, 2, 3);
                self.e.vcvt_double_single(5, 2);
                self.e.vcmp_double(5, 4);
                self.e.vmrs_flags();
                self.e.mov_imm(R1, 0);
                assert!(self.e.alu_imm(Cond::Ne, Op::Mov, false, R1, 0, 1));
            }
            3 => {
                self.e.vfp_single(Vfp::Div, 2, 0, 1);
                self.e.vcvt_double_single(4, 2);
                self.e.vfp_double(Vfp::Mul, 5, 4, 3);
                self.e.vfp_double(Vfp::Sub, 5, 2, 5);
                self.e.vcmp_double_zero(5);
                self.e.vmrs_flags();
                self.e.mov_imm(R1, 0);
                assert!(self.e.alu_imm(Cond::Ne, Op::Mov, false, R1, 0, 1));
            }
            _ => {
                self.e.vsqrt_single(2, 0);
                self.e.vcvt_double_single(5, 2);
                self.e.vfp_double(Vfp::Mul, 6, 5, 5);
                self.e.vcmp_double(6, 2);
                self.e.vmrs_flags();
                self.e.mov_imm(R1, 0);
                assert!(self.e.alu_imm(Cond::Ne, Op::Mov, false, R1, 0, 1));
            }
        }
        // r1: inexact. The result in r2.
        self.e.vmov_from_single(R2, 2);
        self.single_result();
        // FCR31's cause starts cleared; inexact goes to cause and flags.
        self.fcr31_inexact();
    }

    /// The exits for a single result in r2 (r1 1 if it is inexact): unless
    /// it is normal or a zero that is exact, and if inexact, unless it is
    /// above the smallest normal magnitude (the VFP rounds values just below
    /// that up to it, and the VR4300 finds some of them tiny: `fpu::round`).
    /// r3: the result shifted past its sign. Uses r12 and the flags.
    fn single_result(&mut self) {
        self.shift(R3, R2, Shift::Lsl(1));
        assert!(self.e.alu_imm(Cond::Al, Op::Sub, false, R12, R3, 0x0100_0000));
        self.cmp_imm(R12, 0xfe00_0000);
        // HS: a zero, a denormal, an infinity or a NaN; then HI unless zero.
        assert!(self.e.alu_imm(Cond::Hs, Op::Cmp, true, 0, R3, 0));
        self.exit(Cond::Hi);
        // EQ: a zero (HS), or the smallest normal magnitude (LO, normal);
        // then EQ if inexact.
        assert!(self.e.alu_imm(Cond::Lo, Op::Cmp, true, 0, R12, 0));
        assert!(self.e.alu_imm(Cond::Eq, Op::Cmp, true, 0, R1, 1));
        self.exit(Cond::Eq);
    }

    /// C.cond.S and C.cond.D unless an operand is a NaN, which may raise
    /// invalid (that exits before, for the interpreter): the cause cleared,
    /// then FCR31's condition set if fs < ft and the condition has bit 2, or
    /// fs = ft and it has bit 1, as VCMP orders them (denormals as they
    /// are).
    fn fpu_compare(&mut self, i: Instr) {
        let condition = i.funct() & 15;
        self.cop1_usable();
        if i.fmt() == 16 {
            self.fs_single(i.fs());
            self.e.ldr(R1, CPU, fpr(i.ft()));
            // A NaN's bits, shifted past the sign, are above the
            // infinities'.
            self.e.mov_imm(R3, 0xff00_0000);
            for operand in [R0, R1] {
                self.shift(R2, operand, Shift::Lsl(1));
                self.cmp(R2, R3);
                self.exit(Cond::Hi);
            }
            self.e.vmov_to_single(0, R0);
            self.e.vmov_to_single(1, R1);
            self.e.vcmp_single(0, 1);
        } else {
            // fs in r0:r1 (low, high), ft as named in r2:r3: d0 and d1.
            self.fs_double(i.fs());
            self.e.ldr(R2, CPU, fpr(i.ft()));
            self.e.ldr(R3, CPU, fpr(i.ft()) + 4);
            self.not_nan_double(R0, R1);
            self.not_nan_double(R2, R3);
            for (s, r) in [(0, R0), (1, R1), (2, R2), (3, R3)] {
                self.e.vmov_to_single(s, r);
            }
            self.e.vcmp_double(0, 1);
        }
        self.e.vmrs_flags();
        self.e.mov_imm(R2, 0);
        if condition & 4 != 0 {
            assert!(self.e.alu_imm(Cond::Mi, Op::Mov, false, R2, 0, 1));
        }
        if condition & 2 != 0 {
            assert!(self.e.alu_imm(Cond::Eq, Op::Mov, false, R2, 0, 1));
        }
        self.e.ldr(R12, CPU, FCR31);
        assert!(self.e.alu_imm(Cond::Al, Op::Bic, false, R12, R12, 63 << 12));
        assert!(self.e.alu_imm(Cond::Al, Op::Bic, false, R12, R12, 1 << 23));
        self.cmp_imm(R2, 0);
        assert!(self.e.alu_imm(Cond::Ne, Op::Orr, false, R12, R12, 1 << 23));
        self.e.str(R12, CPU, FCR31);
    }

    /// ADD.D, SUB.D, MUL.D, DIV.D and SQRT.D where the VR4300 would round as the
    /// host's VFP does, as `fpu_arithmetic` (to nearest, inexact not
    /// enabled, operands normal or zero, no division by zero, a normal
    /// result or a zero that is exact); anything else exits before, for the
    /// interpreter. Inexact exactly: a sum's error is two_sum's, a product's
    /// a fused x - a * b, and a quotient's remainder a fused a - q * b,
    /// which are exact for results (and a dividend) of 2^-967 and up, so
    /// smaller ones are left too, and so is a zero product of operands that
    /// are not zero (far below the doubles, which the VR4300 finds tiny).
    fn fpu_arithmetic_double(&mut self, i: Instr) {
        self.cop1_usable();
        // a in r0:r1, b (ft as named) in r2:r3.
        self.fs_double(i.fs());
        if i.funct() != 4 {
            self.e.ldr(R2, CPU, fpr(i.ft()));
            self.e.ldr(R3, CPU, fpr(i.ft()) + 4);
        }
        self.fpu_routine(5 + i.funct() as usize);
        self.write_pair(R2, R3, fpr(i.fd()));
    }

    /// The body of `fpu_arithmetic_double`'s routine for `funct`: the
    /// operands in r0:r1 and r2:r3, the result in r2:r3 and FCR31 set. Uses
    /// r12 and VFP's d0-d7.
    fn arithmetic_double(&mut self, funct: u32) {
        self.e.ldr(R12, CPU, FCR31);
        assert!(self.e.alu_imm(Cond::Al, Op::Tst, true, 0, R12, 0x83));
        self.exit(Cond::Ne);
        self.ordinary_double(R0, R1, 1);
        if funct != 4 {
            self.ordinary_double(R2, R3, 1);
        }
        if funct == 3 {
            self.e.alu_shifted(Cond::Al, Op::Orr, true, R12, R2, R3, Shift::Lsl(1));
            self.exit(Cond::Eq);
            self.ordinary_double(R0, R1, 56);
        } else if funct == 4 {
            assert!(self.e.alu_imm(Cond::Al, Op::Tst, true, 0, R1, 0x8000_0000));
            let not_neg = self.e.b(Cond::Eq);
            self.e.alu_shifted(Cond::Al, Op::Orr, true, R12, R0, R1, Shift::Lsl(1));
            self.exit(Cond::Ne);
            self.e.patch_here(not_neg);
        }
        // d0, d1: the operands; d2: the result; d4-d7: working.
        if funct != 4 {
            for (s, r) in [(0, R0), (1, R1), (2, R2), (3, R3)] {
                self.e.vmov_to_single(s, r);
            }
        } else {
            self.e.vmov_to_single(0, R0);
            self.e.vmov_to_single(1, R1);
        }
        match funct {
            0 | 1 => {
                let op = if funct == 0 { Vfp::Add } else { Vfp::Sub };
                // two_sum(a, ±b): x, and its error.
                self.e.vfp_double(op, 2, 0, 1);
                self.e.vfp_double(Vfp::Sub, 5, 2, 0);
                self.e.vfp_double(Vfp::Sub, 6, 2, 5);
                self.e.vfp_double(Vfp::Sub, 6, 0, 6);
                if funct == 0 {
                    self.e.vfp_double(Vfp::Sub, 7, 1, 5);
                    self.e.vfp_double(Vfp::Add, 6, 6, 7);
                } else {
                    self.e.vfp_double(Vfp::Add, 7, 1, 5);
                    self.e.vfp_double(Vfp::Sub, 6, 6, 7);
                }
            }
            2 => {
                self.e.vfp_double(Vfp::Mul, 2, 0, 1);
                self.e.vmov_double(6, 2);
                self.e.vfms_double(6, 0, 1);
            }
            3 => {
                self.e.vfp_double(Vfp::Div, 2, 0, 1);
                self.e.vmov_double(6, 0);
                self.e.vfms_double(6, 2, 1);
            }
            _ => {
                self.e.vsqrt_double(2, 0);
                self.e.vmov_double(6, 0);
                self.e.vfms_double(6, 2, 2);
            }
        }
        self.e.vcmp_double_zero(6);
        self.e.vmrs_flags();
        self.e.mov_imm(R1, 0);
        assert!(self.e.alu_imm(Cond::Ne, Op::Mov, false, R1, 0, 1));
        // r1: inexact. The result in r2:r3, normal (of 2^-967 and up for a
        // product or quotient) or a zero that is exact.
        self.e.vmov_from_single(R2, 4);
        self.e.vmov_from_single(R3, 5);
        // The exponent plus one, in 11 bits: 0 for an infinity or a NaN.
        self.e.ubfx(R12, R3, 20, 11);
        assert!(self.e.alu_imm(Cond::Al, Op::Add, false, R12, R12, 1));
        self.e.ubfx(R12, R12, 0, 11);
        self.cmp_imm(R12, 0);
        self.exit(Cond::Eq);
        self.cmp_imm(R12, if funct >= 2 && funct != 4 { 57 } else { 2 });
        let normal = self.e.b(Cond::Hs);
        self.e.alu_shifted(Cond::Al, Op::Orr, true, R12, R2, R3, Shift::Lsl(1));
        self.exit(Cond::Ne);
        if funct == 2 {
            // A zero product is exact only where an operand is zero (a's
            // words in r0 and s1, b's in s2 and s3): of two others it lies
            // below the doubles, and so does its error, which the fused
            // x - a * b rounded to zero as well.
            self.e.vmov_from_single(R12, 1);
            self.e.alu_shifted(Cond::Al, Op::Orr, true, R12, R0, R12, Shift::Lsl(1));
            let zero = self.e.b(Cond::Eq);
            self.e.vmov_from_single(R1, 2);
            self.e.vmov_from_single(R12, 3);
            self.e.alu_shifted(Cond::Al, Op::Orr, true, R12, R1, R12, Shift::Lsl(1));
            self.exit(Cond::Ne);
            self.e.patch_here(zero);
        }
        self.cmp_imm(R1, 0);
        self.exit(Cond::Ne);
        self.e.patch_here(normal);
        self.fcr31_inexact();
    }

    /// CVT.S.D where the VR4300 rounds as the host's VFP does (as
    /// `fpu_arithmetic`): a normal or zero double, to a normal single (not
    /// an inexact one of the smallest normal magnitude) or a zero that is
    /// exact; inexact unless the single is the double.
    fn fpu_single_from_double(&mut self, i: Instr) {
        self.cop1_usable();
        self.e.ldr(R12, CPU, FCR31);
        assert!(self.e.alu_imm(Cond::Al, Op::Tst, true, 0, R12, 0x83));
        self.exit(Cond::Ne);
        self.fs_double(i.fs());
        self.ordinary_double(R0, R1, 1);
        self.e.vmov_to_single(0, R0);
        self.e.vmov_to_single(1, R1);
        // s4: the single; d3: it as a double again.
        self.e.vcvt_single_double(4, 0);
        self.e.vcvt_double_single(3, 4);
        self.e.vcmp_double(3, 0);
        self.e.vmrs_flags();
        self.e.mov_imm(R1, 0);
        assert!(self.e.alu_imm(Cond::Ne, Op::Mov, false, R1, 0, 1));
        self.e.vmov_from_single(R2, 4);
        self.single_result();
        self.fcr31_inexact();
        self.e.mov_imm(R3, 0);
        self.write_pair(R2, R3, fpr(i.fd()));
    }

    /// An exit unless the double in `lo` and `hi` is a zero or normal with
    /// a biased exponent of `least` and up (1: any). Uses r12 and the flags.
    fn ordinary_double(&mut self, lo: Reg, hi: Reg, least: u32) {
        // The exponent plus one, in 11 bits: 0 for infinities and NaNs.
        self.e.ubfx(R12, hi, 20, 11);
        assert!(self.e.alu_imm(Cond::Al, Op::Add, false, R12, R12, 1));
        self.e.ubfx(R12, R12, 0, 11);
        self.cmp_imm(R12, least + 1);
        let normal = self.e.b(Cond::Hs);
        self.e.alu_shifted(Cond::Al, Op::Orr, true, R12, lo, hi, Shift::Lsl(1));
        self.exit(Cond::Ne);
        self.e.patch_here(normal);
    }

    /// An exit if the double in `lo` and `hi` is a NaN: its high word,
    /// shifted past the sign, above an infinity's, or one with any low bit.
    /// Uses r12, LR and the flags.
    fn not_nan_double(&mut self, lo: Reg, hi: Reg) {
        self.e.mov_imm(LR, 0xffe0_0000);
        self.shift(R12, hi, Shift::Lsl(1));
        self.cmp(R12, LR);
        self.exit(Cond::Hi);
        let below = self.e.b(Cond::Ne);
        self.cmp_imm(lo, 0);
        self.exit(Cond::Ne);
        self.e.patch_here(below);
    }

    /// r0 (low) and r1: arithmetic's double fs, the even register of a pair
    /// while Status.FR is clear. Uses r12 and the flags.
    fn fs_double(&mut self, fs: usize) {
        if fs & 1 == 0 {
            self.e.ldr(R0, CPU, fpr(fs));
            self.e.ldr(R1, CPU, fpr(fs) + 4);
        } else {
            self.e.ldr(R12, CPU, STATUS);
            assert!(self.e.alu_imm(Cond::Al, Op::Tst, true, 0, R12, FR));
            for (cond, at) in [(Cond::Ne, fpr(fs)), (Cond::Eq, fpr(fs & !1))] {
                self.e.ldr_cond(cond, R0, CPU, at);
                self.e.ldr_cond(cond, R1, CPU, at + 4);
            }
        }
    }

    /// FCR31's cause cleared, as every FPU operation but MOV starts.
    fn fcr31_clear_cause(&mut self) {
        self.e.ldr(R12, CPU, FCR31);
        assert!(self.e.alu_imm(Cond::Al, Op::Bic, false, R12, R12, 63 << 12));
        self.e.str(R12, CPU, FCR31);
    }

    /// NEG and ABS of an ordinary single or double (`ordinary`; a NaN or a
    /// denormal raises its exception, an infinity is left too): its sign
    /// flipped or cleared.
    fn fpu_sign(&mut self, i: Instr) {
        let double = i.fmt() == 17;
        self.cop1_usable();
        let op = if i.funct() == 7 { Op::Eor } else { Op::Bic };
        if double {
            self.fs_double(i.fs());
            self.ordinary_double(R0, R1, 1);
            assert!(self.e.alu_imm(Cond::Al, op, false, R3, R1, 0x8000_0000));
            self.fcr31_clear_cause();
            self.write_pair(R0, R3, fpr(i.fd()));
        } else {
            self.fs_single(i.fs());
            self.ordinary(R0);
            assert!(self.e.alu_imm(Cond::Al, op, false, R2, R0, 0x8000_0000));
            self.fcr31_clear_cause();
            self.e.mov_imm(R3, 0);
            self.write_pair(R2, R3, fpr(i.fd()));
        }
    }

    /// CVT.D.S of an ordinary single (`ordinary`), which is exact.
    fn fpu_double_from_single(&mut self, i: Instr) {
        self.cop1_usable();
        self.fs_single(i.fs());
        self.ordinary(R0);
        self.e.vmov_to_single(0, R0);
        self.e.vcvt_double_single(1, 0);
        self.double_result(i.fd());
    }

    /// CVT.D.W, which is exact.
    fn fpu_double_from_word(&mut self, i: Instr) {
        self.cop1_usable();
        self.fs_single(i.fs());
        self.e.vmov_to_single(0, R0);
        self.e.vcvt_double_int(1, 0);
        self.double_result(i.fd());
    }

    /// d1, an exact result: to fd as named, the cause cleared.
    fn double_result(&mut self, fd: usize) {
        self.e.vmov_from_single(R2, 2);
        self.e.vmov_from_single(R3, 3);
        self.fcr31_clear_cause();
        self.write_pair(R2, R3, fpr(fd));
    }

    /// CVT.S.W where the VR4300 rounds as the host's VFP does: to nearest,
    /// with inexact (the only cause there can be) not enabled; anything else
    /// exits before, for the interpreter. Inexact unless the single is the
    /// word, compared as doubles (which hold both exactly).
    fn fpu_from_word(&mut self, i: Instr) {
        self.cop1_usable();
        self.e.ldr(R12, CPU, FCR31);
        assert!(self.e.alu_imm(Cond::Al, Op::Tst, true, 0, R12, 0x83));
        self.exit(Cond::Ne);
        // The word as arithmetic reads fs.
        self.fs_single(i.fs());
        // s0: the word; s1: the single; d1, d2: both as doubles.
        self.e.vmov_to_single(0, R0);
        self.e.vcvt_single_int(1, 0);
        self.e.vcvt_double_int(1, 0);
        self.e.vcvt_double_single(2, 1);
        self.e.vcmp_double(1, 2);
        self.e.vmrs_flags();
        self.e.mov_imm(R1, 0);
        assert!(self.e.alu_imm(Cond::Ne, Op::Mov, false, R1, 0, 1));
        self.fcr31_inexact();
        // A single result fills fd's 64 bits, its top half zero.
        self.e.vmov_from_single(R2, 1);
        self.e.mov_imm(R3, 0);
        self.write_pair(R2, R3, fpr(i.fd()));
    }

    fn fpu_load(&mut self, i: Instr) {
        self.cop1_usable();
        if i.op == Mips::Ldc1 {
            self.rdram_address(i, 8, false);
            self.e.ldr_reg(R1, RAM, R2);
            self.e.add_imm(R2, R2, 4, R12);
            self.e.ldr_reg(R0, RAM, R2);
            self.e.rev(R0, R0);
            self.e.rev(R1, R1);
            self.fpr(false, R0, Some(R1), i.ft());
        } else {
            self.rdram_address(i, 4, false);
            self.e.ldr_reg(R0, RAM, R2);
            self.e.rev(R0, R0);
            self.fpr(false, R0, None, i.ft());
        }
    }

    /// MFC1, DMFC1, MTC1, DMTC1 and MOV.fmt, which copies all 64 bits into
    /// fd as named and leaves FCR31's cause alone.
    fn fpu_move(&mut self, i: Instr) {
        use Mips::*;
        self.cop1_usable();
        match i.op {
            Mfc1 => {
                if i.rt() != 0 {
                    let d = self.write(i.rt());
                    self.fpr(true, d, None, i.fs());
                }
            }
            Dmfc1 => {
                self.fpr(true, R0, Some(R1), i.fs());
                self.result(i.rt());
            }
            Mtc1 => {
                let value = self.source(i.rt(), R0);
                self.fpr(false, value, None, i.fs());
            }
            Dmtc1 => {
                self.pair(R0, R1, i.rt());
                self.fpr(false, R0, Some(R1), i.fs());
            }
            _ => {
                self.fpr(true, R0, Some(R1), i.fs());
                self.write_pair(R0, R1, fpr(i.fd()));
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
            // These instructions have no effects beyond their destination;
            // NOP is the common case. Trapping arithmetic is not accepted.
            return true;
        }
        match i.op {
            op if is_branch(op) => self.branch(i),
            Lb | Lbu | Lh | Lhu | Lw | Lwu | Ld => self.load(i),
            Lwl | Lwr => self.load_unaligned(i),
            Ldl | Ldr => self.load_double_unaligned(i),
            Sb | Sh | Sw | Sd | Swc1 | Sdc1 => self.store(i),
            Swl | Swr => self.store_unaligned(i),
            Sdl | Sdr => self.store_double_unaligned(i),
            Lwc1 | Ldc1 => self.fpu_load(i),
            Mfc1 | Dmfc1 | Mtc1 | Dmtc1 => self.fpu_move(i),
            FpuOp if i.funct() == 6 => self.fpu_move(i),
            FpuOp if i.funct() <= 4 && i.fmt() == 16 => self.fpu_arithmetic(i),
            FpuOp if i.funct() <= 4 && i.fmt() == 17 => self.fpu_arithmetic_double(i),
            FpuOp if i.funct() == 32 && i.fmt() == 17 => self.fpu_single_from_double(i),
            FpuOp if matches!(i.funct(), 12..=15 | 36) && matches!(i.fmt(), 16 | 17) => self.fpu_to_word(i),
            FpuOp if i.funct() >= 48 && matches!(i.fmt(), 16 | 17) => self.fpu_compare(i),
            FpuOp if matches!(i.funct(), 5 | 7) && matches!(i.fmt(), 16 | 17) => self.fpu_sign(i),
            FpuOp if i.funct() == 33 && i.fmt() == 16 => self.fpu_double_from_single(i),
            FpuCvtInt if i.funct() == 32 && i.fmt() == 20 => self.fpu_from_word(i),
            FpuCvtInt if i.funct() == 33 && i.fmt() == 20 => self.fpu_double_from_word(i),
            Cfc1 | Ctc1 => self.fpu_control(i),
            Mfc0 | Dmfc0 if constant_register(i.rd()).is_some() => self.constant_read(i),
            // Mfc0 | Dmfc0 if i.rd() == 9 => self.count_read(i),
            Mfc0 | Dmfc0 if matches!(i.rd(), 0 | 4 | 8 | 10 | 11 | 12 | 13 | 14 | 16 | 30) => self.cop0_read(i),
            Mtc0 if i.rd() == 12 => self.status_write(i),
            Cache => self.cache(),
            Sync => {}
            // 32-bit results of low words, held (see `Held`): of any operand,
            // or where the high words matter, of operands known to be
            // sign-extended words, whose low words decide.
            Sll | Srl | Sra if i.op != Sra || i.sa() == 0 || self.known_word(rt) => {
                // SRA shifts the high word's bits in: the sign's, when known.
                let s = self.source(rt, R0);
                let d = self.write(rd);
                match (i.op, i.sa()) {
                    (_, 0) => self.e.mov(d, s),
                    (Sll, sa) => self.shift(d, s, Shift::Lsl(sa)),
                    (Srl, sa) => self.shift(d, s, Shift::Lsr(sa)),
                    (_, sa) => self.shift(d, s, Shift::Asr(sa)),
                }
            }
            Sllv | Srlv | Srav if i.op != Srav || self.known_word(rt) => {
                let a = self.source(rs, R2);
                self.immediate(Op::And, R2, a, 31);
                let s = self.source(rt, R0);
                let d = self.write(rd);
                self.shift(d, s, match i.op {
                    Sllv => Shift::LslReg(R2),
                    Srlv => Shift::LsrReg(R2),
                    _ => Shift::AsrReg(R2),
                });
            }
            Addu | Subu => {
                let a = self.source(rs, R0);
                let b = self.source(rt, R2);
                let d = self.write(rd);
                self.e.alu(if i.op == Addu { Op::Add } else { Op::Sub }, d, a, b);
            }
            Addiu if rs == 0 => {
                let d = self.write(rt);
                self.e.mov_imm(d, i.simm() as u32);
            }
            Addiu => {
                let a = self.read(rs);
                let d = self.write(rt);
                self.e.add_imm(d, a, i.simm() as u32, R12);
            }
            Lui => {
                let d = self.write(rt);
                self.e.mov_imm(d, i.word << 16);
            }
            Ori | Xori if rs == 0 => {
                let d = self.write(rt);
                self.e.mov_imm(d, i.uimm() as u32);
            }
            // ANDI's result is below 2^16; ORI and XORI keep bits 16-63.
            Andi | Ori | Xori if i.op == Andi || self.known_word(rs) => {
                let a = self.source(rs, R0);
                let d = self.write(rt);
                let op = match i.op { Andi => Op::And, Ori => Op::Orr, _ => Op::Eor };
                self.immediate(op, d, a, i.uimm() as u32);
            }
            And | Or | Xor | Nor if self.known_word(rs) && self.known_word(rt) => {
                let a = self.source(rs, R0);
                let b = self.source(rt, R2);
                let d = self.write(rd);
                let op = match i.op { And => Op::And, Xor => Op::Eor, _ => Op::Orr };
                self.e.alu(op, d, a, b);
                if i.op == Nor {
                    self.e.alu(Op::Mvn, d, 0, d);
                }
            }
            // Sign-extended words order as their low words do, signed or not.
            Slt | Sltu if self.known_word(rs) && self.known_word(rt) => {
                let a = self.source(rs, R0);
                let b = self.source(rt, R2);
                self.cmp(a, b);
                self.set_less(rd, i.op == Slt);
            }
            Slti | Sltiu if self.known_word(rs) => {
                let a = self.source(rs, R0);
                let value = i.simm() as u32;
                if !self.e.alu_imm(Cond::Al, Op::Cmp, true, 0, a, value)
                    && !self.e.alu_imm(Cond::Al, Op::Cmn, true, 0, a, value.wrapping_neg())
                {
                    self.e.mov_imm(R12, value);
                    self.cmp(a, R12);
                }
                self.set_less(rt, i.op == Slti);
            }
            Sll | Srl | Sra => {
                self.pair(R0, R1, rt);
                match i.op {
                    Sll => self.shift(R0, R0, Shift::Lsl(i.sa())),
                    Srl if i.sa() != 0 => self.shift(R0, R0, Shift::Lsr(i.sa())),
                    Sra => self.shift_double_immediate(Direction::Arithmetic, i.sa()),
                    _ => {}
                }
                self.word_result(rd);
            }
            Sllv | Srlv | Srav => {
                self.pair(R0, R1, rt);
                self.low(R2, rs);
                self.immediate(Op::And, R2, R2, 31);
                match i.op {
                    Sllv => self.shift(R0, R0, Shift::LslReg(R2)),
                    Srlv => self.shift(R0, R0, Shift::LsrReg(R2)),
                    _ => {
                        self.e.mov_imm(R3, 32);
                        self.e.alu(Op::Sub, R3, R3, R2);
                        self.shift(R12, R1, Shift::LslReg(R3));
                        self.shift(R0, R0, Shift::LsrReg(R2));
                        self.e.alu(Op::Orr, R0, R0, R12);
                    }
                }
                self.word_result(rd);
            }
            Dsll | Dsrl | Dsra | Dsll32 | Dsrl32 | Dsra32 => {
                self.pair(R0, R1, rt);
                let direction = match i.op {
                    Dsll | Dsll32 => Direction::Left,
                    Dsrl | Dsrl32 => Direction::Right,
                    _ => Direction::Arithmetic,
                };
                self.shift_double_immediate(direction, i.sa() + if matches!(i.op, Dsll32 | Dsrl32 | Dsra32) { 32 } else { 0 });
                self.result(rd);
            }
            Dsllv | Dsrlv | Dsrav => {
                self.pair(R0, R1, rt);
                self.low(R2, rs);
                let direction = match i.op {
                    Dsllv => Direction::Left,
                    Dsrlv => Direction::Right,
                    _ => Direction::Arithmetic,
                };
                self.shift_double_variable(direction);
                self.result(rd);
            }
            Mfhi | Mflo => {
                let at = if i.op == Mfhi { HI } else { LO };
                self.e.ldr(R0, CPU, at);
                self.e.ldr(R1, CPU, at + 4);
                self.result(rd);
            }
            Mthi | Mtlo => {
                let at = if i.op == Mthi { HI } else { LO };
                self.pair(R0, R1, rs);
                self.e.str(R0, CPU, at);
                self.e.str(R1, CPU, at + 4);
            }
            Mult | Multu => {
                self.low(R2, rs);
                self.low(R3, rt);
                self.e.mull(i.op == Mult, R0, R1, R2, R3);
                self.hilo_words();
            }
            Div | Divu => {
                self.low(R0, rs);
                self.low(R1, rt);
                self.divide_word(i.op == Div);
            }
            Dmult | Dmultu => self.dmult(i),
            Ddiv | Ddivu => self.ddiv(i),
            Daddiu => {
                self.pair(R0, R1, rs);
                self.constant(R2, R3, i.simm());
                self.e.alu_shifted(Cond::Al, Op::Add, true, R0, R0, R2, Shift::Lsl(0));
                self.e.alu(Op::Adc, R1, R1, R3);
                self.result(rt);
            }
            Daddu | Dsubu | And | Or | Xor | Nor | Slt | Sltu => {
                self.pair(R0, R1, rs);
                self.pair(R2, R3, rt);
                match i.op {
                    Daddu | Dsubu => {
                        self.e.alu_shifted(Cond::Al, if i.op == Daddu { Op::Add } else { Op::Sub }, true, R0, R0, R2, Shift::Lsl(0));
                        self.e.alu(if i.op == Daddu { Op::Adc } else { Op::Sbc }, R1, R1, R3);
                    }
                    Slt | Sltu => {
                        self.comparison(i.op == Slt, rd);
                        return true;
                    }
                    _ => {
                        let op = match i.op { And => Op::And, Xor => Op::Eor, _ => Op::Orr };
                        self.e.alu(op, R0, R0, R2);
                        self.e.alu(op, R1, R1, R3);
                        if i.op == Nor {
                            self.e.alu(Op::Mvn, R0, 0, R0);
                            self.e.alu(Op::Mvn, R1, 0, R1);
                        }
                    }
                }
                self.result(rd);
            }
            Slti | Sltiu => {
                self.pair(R0, R1, rs);
                self.constant(R2, R3, i.simm());
                self.comparison(i.op == Slti, rt);
            }
            Andi | Ori | Xori => {
                self.pair(R0, R1, rs);
                self.immediate(match i.op { Andi => Op::And, Ori => Op::Orr, _ => Op::Eor }, R0, R0, i.uimm() as u32);
                if i.op == Andi {
                    self.e.mov_imm(R1, 0);
                }
                self.result(rt);
            }
            // Trapping arithmetic: the sum with the flags, an exit before the
            // instruction on an overflow (the interpreter takes the
            // exception), then the result.
            Add | Sub | Addi => {
                let a = self.source(rs, R0);
                if i.op == Addi {
                    let value = i.simm() as u32;
                    if !self.e.alu_imm(Cond::Al, Op::Add, true, R0, a, value)
                        && !self.e.alu_imm(Cond::Al, Op::Sub, true, R0, a, value.wrapping_neg())
                    {
                        self.e.mov_imm(R12, value);
                        self.e.alu_shifted(Cond::Al, Op::Add, true, R0, a, R12, Shift::Lsl(0));
                    }
                } else {
                    let b = self.source(rt, R2);
                    self.e.alu_shifted(Cond::Al, if i.op == Add { Op::Add } else { Op::Sub }, true, R0, a, b, Shift::Lsl(0));
                }
                self.exit(Cond::Vs);
                self.word_result(if i.op == Addi { rt } else { rd });
            }
            Dadd | Dsub | Daddi => {
                self.pair(R0, R1, rs);
                if i.op == Daddi {
                    self.constant(R2, R3, i.simm());
                } else {
                    self.pair(R2, R3, rt);
                }
                let (low, high) = if i.op == Dsub { (Op::Sub, Op::Sbc) } else { (Op::Add, Op::Adc) };
                self.e.alu_shifted(Cond::Al, low, true, R0, R0, R2, Shift::Lsl(0));
                self.e.alu_shifted(Cond::Al, high, true, R1, R1, R3, Shift::Lsl(0));
                self.exit(Cond::Vs);
                self.result(if i.op == Daddi { rt } else { rd });
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

    fn branches(immediate: u16) -> Vec<u32> {
        let mut words = vec![0x0800_0400, 0x0c00_0400, 0x0020_0008, 0x0020_1809];
        for op in [0x04, 0x05, 0x06, 0x07, 0x14, 0x15, 0x16, 0x17] {
            let rt = if matches!(op, 0x04 | 0x05 | 0x14 | 0x15) { 2 } else { 0 };
            words.push(op << 26 | 1 << 21 | rt << 16 | u32::from(immediate));
        }
        for rt in [0x00, 0x01, 0x02, 0x03, 0x10, 0x11, 0x12, 0x13] {
            words.push(0x0400_0000 | 1 << 21 | rt << 16 | u32::from(immediate));
        }
        words
    }

    /// Blocks at a source address of 0x1000, with no interpreter to call.
    fn block(words: &[u32]) -> Option<Compiled> {
        super::block(words, 0x1000, 0)
    }

    #[test]
    fn untranslated_instructions_are_interpreted_inside_the_block() {
        let addiu = 0x2422_ffff;
        // MFC0 of Random, LL (untranslated), CVT.L.S, ROUND.L.S: the block goes on.
        for word in [0x4002_0800, 0xc022_0000, 0x4600_0025, 0x4600_0008] {
            assert!(block(&[word]).is_none(), "{word:08x} alone is left to the dispatcher");
            assert_eq!(block(&[addiu, word, addiu]).unwrap().instructions, 3, "{word:08x}");
        }
        // SYSCALL and MTC0 (of Compare) would always stop it: it ends before
        // them.
        for word in [0x0000_000c, 0x4082_5800] {
            assert!(block(&[word]).is_none(), "{word:08x}");
            assert_eq!(block(&[addiu, word, addiu]).unwrap().instructions, 1, "{word:08x}");
        }
        assert!(block(&[]).is_none());
        assert_eq!(block(&[addiu; 80]).unwrap().instructions, 64);
        // A branch as the 64th takes its slot along (left out, the slot
        // would go to the interpreter, the branch done).
        let mut words = vec![addiu; 63];
        words.extend([0x1022_0003, addiu, addiu]);
        let c = block(&words).unwrap();
        assert!(c.ends_in_branch);
        assert_eq!(c.instructions, 65);
    }

    #[test]
    fn slots_run_in_the_block_unless_the_branch_jumps_to_itself() {
        let addiu = 0x2422_0001;
        for (words, instructions) in [
            (vec![0x1022_0003, addiu], 2),        // beq at,v0: slot in
            (vec![0x5022_0003, addiu], 2),        // beql: slot in, annulled when not taken
            (vec![0x03e0_0008, 0x27bd_0018], 2),  // jr ra; addiu sp,sp,24
            (vec![0x0c00_0800, 0x0000_0000], 2),  // jal elsewhere; nop
            (vec![0x1000_ffff, 0x0000_0000], 1),  // b .: left for skip_idle_loop
            (vec![0x0800_0400, 0x0000_0000], 1),  // j to its own address (0x1000)
            (vec![0x1022_0003, 0x8c22_0000], 2),  // a load in the slot
            (vec![0x03e0_0008, 0xafbf_0010], 2),  // jr ra; sw ra,16(sp)
            (vec![0x5022_0003, 0xe7a0_0010], 2),  // beql; swc1 f0,16(sp)
            (vec![0x1000_ffff, 0x8c22_0000], 1),  // b . with a load: still left
            (vec![0x1022_0003, 0x4600_0088], 1),  // an interpreted one
            (vec![0x1022_0003], 1),               // no slot in the words
        ] {
            let c = block(&words).unwrap();
            assert!(c.ends_in_branch);
            assert_eq!(c.instructions, instructions, "{words:08x?}");
        }
    }

    #[test]
    fn fpu_loads_stores_and_moves_are_compiled() {
        // LWC1, SWC1, LDC1, SDC1, MFC1, MTC1, DMFC1, DMTC1, MOV.S, MOV.D.
        for word in [0xc422_0000, 0xe422_0000, 0xd422_0000, 0xf422_0000, 0x4402_0800, 0x4482_0800,
            0x4422_0800, 0x44a2_0800, 0x4600_0886, 0x4620_0886]
        {
            assert_eq!(block(&[word]).map(|c| c.instructions), Some(1), "{word:08x}");
        }
    }

    #[test]
    fn stores_do_not_end_the_block() {
        for op in [0x28, 0x29, 0x2b, 0x3f] {
            let store = op << 26 | 1 << 21 | 2 << 16;
            let c = block(&[store, 0x2422_0001]).unwrap();
            assert_eq!(c.instructions, 2);
            assert!(!c.ends_in_branch);
            assert_eq!(block(&[0x2422_0001, store, 0x2422_0001]).unwrap().instructions, 3);
        }
    }

    #[test]
    fn every_integer_branch_ends_the_block_before_a_slot_it_cannot_run() {
        let words = branches(0xffff);
        assert_eq!(words.len(), 20);
        for branch in words {
            for prefix in [0, 2] {
                let mut source = vec![0x2421_0001; prefix];
                source.extend([branch, 0x4600_0088]); // round.l.s: interpreted
                let c = block(&source).unwrap();
                assert_eq!(c.instructions, prefix + 1, "{branch:08x}");
                assert!(c.ends_in_branch, "{branch:08x}");
            }
        }
        assert!(!block(&[0x2421_0001]).unwrap().ends_in_branch);
    }
}
