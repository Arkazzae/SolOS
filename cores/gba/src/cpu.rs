//! The ARM7TDMI CPU: its registers and modes, exceptions, and what ARM
//! (arm.rs) and Thumb (thumb.rs) instructions share. The CPU fetches two
//! opcodes ahead, as the chip's pipeline does: while the instruction at X
//! runs, r15 is X+8 (Thumb: X+4), and the opcode at r15 is fetched in its
//! first cycle. Every memory access costs the cycles of its region
//! (bus.rs), and instructions add the internal cycles they spend.

use crate::Gba;
use savestate::state;

// CPSR: the condition flags, interrupts off, and the Thumb state.
pub const N: u32 = 1 << 31;
pub const Z: u32 = 1 << 30;
pub const C: u32 = 1 << 29;
pub const V: u32 = 1 << 28;
pub const I: u32 = 1 << 7;
pub const T: u32 = 1 << 5;

// Modes, in CPSR's low five bits.
pub const USER: u32 = 0x10;
pub const FIQ: u32 = 0x11;
pub const IRQ: u32 = 0x12;
pub const SUPERVISOR: u32 = 0x13;
pub const ABORT: u32 = 0x17;
pub const UNDEFINED: u32 = 0x1b;
pub const SYSTEM: u32 = 0x1f;

/// For each condition, the flag combinations (NZCV as a 4-bit number)
/// that pass it, as bits.
const CONDITIONS: [u16; 16] = {
    let mut table = [0u16; 16];
    let mut flags = 0;
    while flags < 16 {
        let (n, z, c, v) = (flags & 8 != 0, flags & 4 != 0, flags & 2 != 0, flags & 1 != 0);
        let passes =
            [z, !z, c, !c, n, !n, v, !v, c && !z, !c || z, n == v, n != v, !z && n == v, z || n != v, true, false];
        let mut cond = 0;
        while cond < 16 {
            if passes[cond] {
                table[cond] |= 1 << flags;
            }
            cond += 1;
        }
        flags += 1;
    }
    table
};

/// Which set of r13 and r14 a mode has: user and system share one, and
/// a mode that is none of the chip's uses theirs.
fn bank(mode: u32) -> usize {
    match mode {
        FIQ => 1,
        IRQ => 2,
        SUPERVISOR => 3,
        ABORT => 4,
        UNDEFINED => 5,
        _ => 0,
    }
}

#[derive(Clone, Debug)]
pub struct Cpu {
    /// r0-r15 as the current mode sees them.
    pub r: [u32; 16],
    pub cpsr: u32,
    /// The saved PSRs of FIQ, IRQ, supervisor, abort and undefined modes.
    spsr: [u32; 5],
    /// r13 and r14 of each bank (user and system, then the modes with
    /// their own); the current mode's are in r.
    banked: [[u32; 2]; 6],
    /// r8-r12 of FIQ mode and of all the others: the set not in r.
    fiq_high: [u32; 5],
    other_high: [u32; 5],
    /// The two opcodes fetched ahead of the running one.
    pub pipe: [u32; 2],
    /// The next opcode fetch follows on from the last one (an S cycle,
    /// rather than N after a branch, a data access or an internal cycle).
    pub seq: bool,
    /// The running instruction refilled the pipeline.
    pub flushed: bool,
    /// Halted (HALTCNT) until an interrupt is requested and enabled.
    pub halted: bool,
}

state!(Cpu { r, cpsr, spsr, banked, fiq_high, other_high, pipe, seq, flushed, halted });

impl Cpu {
    /// The CPU as the BIOS leaves it for the game: in system mode, each
    /// mode's stack set up, about to run the cartridge's first opcode.
    pub fn after_boot() -> Cpu {
        let mut banked = [[0; 2]; 6];
        banked[bank(IRQ)][0] = 0x0300_7fa0;
        banked[bank(SUPERVISOR)][0] = 0x0300_7fe0;
        let mut r = [0; 16];
        r[13] = 0x0300_7f00;
        r[15] = 0x0800_0000;
        Cpu {
            r,
            cpsr: SYSTEM,
            spsr: [0; 5],
            banked,
            fiq_high: [0; 5],
            other_high: [0; 5],
            pipe: [0; 2],
            seq: false,
            flushed: false,
            halted: false,
        }
    }

    pub fn mode(&self) -> u32 {
        self.cpsr & 0x1f
    }

    pub fn thumb(&self) -> bool {
        self.cpsr & T != 0
    }

    pub fn condition(&self, cond: u32) -> bool {
        CONDITIONS[cond as usize] & 1 << (self.cpsr >> 28) != 0
    }

    pub fn flag(&self, flag: u32) -> bool {
        self.cpsr & flag != 0
    }

    pub fn set_flag(&mut self, flag: u32, on: bool) {
        if on {
            self.cpsr |= flag;
        } else {
            self.cpsr &= !flag;
        }
    }

    pub fn set_nz(&mut self, value: u32) {
        self.cpsr = self.cpsr & !(N | Z) | value & N | u32::from(value == 0) << 30;
    }

    /// Adds with a carry in, setting all four flags if asked; a - b is
    /// a + !b + 1, and its carry is "no borrow".
    pub fn add(&mut self, a: u32, b: u32, carry: bool, flags: bool) -> u32 {
        let wide = u64::from(a) + u64::from(b) + u64::from(carry);
        let result = wide as u32;
        if flags {
            self.set_nz(result);
            self.set_flag(C, wide >> 32 != 0);
            self.set_flag(V, (a ^ result) & (b ^ result) & 0x8000_0000 != 0);
        }
        result
    }

    pub fn sub(&mut self, a: u32, b: u32, carry: bool, flags: bool) -> u32 {
        self.add(a, !b, carry, flags)
    }

    /// Changes mode, swapping in its banked registers.
    pub fn switch_mode(&mut self, mode: u32) {
        let (from, to) = (bank(self.mode()), bank(mode));
        if from != to {
            self.banked[from] = [self.r[13], self.r[14]];
            [self.r[13], self.r[14]] = self.banked[to];
            if from == 1 {
                self.fiq_high.copy_from_slice(&self.r[8..13]);
                self.r[8..13].copy_from_slice(&self.other_high);
            } else if to == 1 {
                self.other_high.copy_from_slice(&self.r[8..13]);
                self.r[8..13].copy_from_slice(&self.fiq_high);
            }
        }
        self.cpsr = self.cpsr & !0x1f | mode;
    }

    /// Puts a whole new CPSR in place (from an SPSR, or MSR), mode and all.
    pub fn set_cpsr(&mut self, value: u32) {
        self.switch_mode(value & 0x1f | 0x10);
        self.cpsr = value | 0x10;
    }

    /// The current mode's SPSR; user and system mode have none, and read
    /// the CPSR.
    pub fn spsr(&self) -> u32 {
        match bank(self.mode()) {
            0 => self.cpsr,
            b => self.spsr[b - 1],
        }
    }

    pub fn set_spsr(&mut self, value: u32) {
        let b = bank(self.mode());
        if b > 0 {
            self.spsr[b - 1] = value;
        }
    }

    /// A register of user mode, whatever the mode (LDM and STM with ^).
    pub fn user_reg(&self, n: usize) -> u32 {
        match n {
            8..=12 if bank(self.mode()) == 1 => self.other_high[n - 8],
            13 | 14 if bank(self.mode()) != 0 => self.banked[0][n - 13],
            _ => self.r[n],
        }
    }

    pub fn set_user_reg(&mut self, n: usize, value: u32) {
        match n {
            8..=12 if bank(self.mode()) == 1 => self.other_high[n - 8] = value,
            13 | 14 if bank(self.mode()) != 0 => self.banked[0][n - 13] = value,
            _ => self.r[n] = value,
        }
    }
}

/// A loop that may be waiting for an event: the registers it had when it
/// last branched back, and whether anything since changed memory, wrote
/// I/O or read what changes between events (a timer's count, the sound's
/// status). Changes to IWRAM are kept apart, as the end of the highest:
/// below the stack pointer the loop branches back with, they are what its
/// calls pushed and popped, which nothing reads again before writing.
///
/// A loop found idle waits on past the events after it, as long as none
/// of them changes what it sees: no interrupt for the CPU to take, no DMA,
/// and the loop reads no I/O that the events move (all but the keys, which
/// only change between frames).
#[derive(Clone, Default)]
pub struct IdleLoop {
    r: [u32; 16],
    cpsr: u32,
    pub touched: bool,
    pub iwram_end: u32,
    pub io_read: bool,
    pub waiting: bool,
    pub across_events: bool,
    pub dma_ran: bool,
}

/// A shift by an amount in the opcode: LSL, LSR, ASR or ROR, where an
/// amount of 0 means no shift for LSL, 32 for LSR and ASR, and RRX (a
/// rotate through carry) for ROR. Gives the result and the carry out.
pub fn shift_by_immediate(kind: u32, amount: u32, value: u32, carry: bool) -> (u32, bool) {
    match (kind, amount) {
        (0, 0) => (value, carry),
        (0, _) => (value << amount, value >> (32 - amount) & 1 != 0),
        (1, 0) => (0, value >> 31 != 0),
        (1, _) => (value >> amount, value >> (amount - 1) & 1 != 0),
        (2, 0) => (((value as i32) >> 31) as u32, value >> 31 != 0),
        (2, _) => (((value as i32) >> amount) as u32, value >> (amount - 1) & 1 != 0),
        (_, 0) => (u32::from(carry) << 31 | value >> 1, value & 1 != 0),
        _ => (value.rotate_right(amount), value >> (amount - 1) & 1 != 0),
    }
}

/// A shift by the bottom byte of a register, where 0 leaves the value and
/// the carry, and amounts of 32 and more shift everything out.
pub fn shift_by_register(kind: u32, amount: u32, value: u32, carry: bool) -> (u32, bool) {
    if amount == 0 {
        return (value, carry);
    }
    match kind {
        0 if amount < 32 => (value << amount, value >> (32 - amount) & 1 != 0),
        0 => (0, amount == 32 && value & 1 != 0),
        1 if amount < 32 => (value >> amount, value >> (amount - 1) & 1 != 0),
        1 => (0, amount == 32 && value >> 31 != 0),
        2 if amount < 32 => (((value as i32) >> amount) as u32, value >> (amount - 1) & 1 != 0),
        2 => (((value as i32) >> 31) as u32, value >> 31 != 0),
        _ => match amount & 31 {
            0 => (value, value >> 31 != 0),
            r => (value.rotate_right(r), value >> (r - 1) & 1 != 0),
        },
    }
}

/// The internal cycles a multiply spends on its multiplier: one for each
/// byte, less the top bytes that are all 0 (or, signed, all 1).
pub fn multiply_cycles(multiplier: u32, signed: bool) -> u32 {
    let ones = if signed { !multiplier } else { multiplier };
    let top = |mask: u32| multiplier & mask == 0 || signed && ones & mask == 0;
    if top(0xffff_ff00) {
        1
    } else if top(0xffff_0000) {
        2
    } else if top(0xff00_0000) {
        3
    } else {
        4
    }
}

impl Gba {
    /// Runs one instruction, or takes an interrupt waiting for the CPU.
    pub(crate) fn step(&mut self) {
        if self.irq_line && self.cpu.cpsr & I == 0 {
            self.interrupt();
        }
        if self.cpu.thumb() { self.step_thumb() } else { self.step_arm() }
    }

    /// A taken branch backwards. A loop that comes round again with the
    /// same registers, having written nothing and read only what changes at
    /// events, goes round the same way until the next event: the CPU skips
    /// to it.
    pub(crate) fn loop_branch(&mut self) {
        let idle = &mut self.idle_loop;
        let sp = self.cpu.r[13];
        if idle.iwram_end > 0 && !(sp >> 24 == 3 && idle.iwram_end <= sp & 0x7fff) {
            idle.touched = true;
        }
        idle.iwram_end = 0;
        if !idle.touched && idle.r == self.cpu.r && idle.cpsr == self.cpu.cpsr {
            self.cycles = self.cycles.max(self.next_event);
            idle.waiting = true;
            idle.across_events = !idle.io_read;
            idle.dma_ran = false;
        } else {
            idle.r = self.cpu.r;
            idle.cpsr = self.cpu.cpsr;
        }
        idle.touched = false;
        idle.io_read = false;
    }

    /// Moves the pipeline on after an instruction, unless it refilled it.
    pub(crate) fn advance(&mut self, pc: u32, size: u32) {
        if self.cpu.flushed {
            self.cpu.flushed = false;
        } else {
            self.cpu.r[15] = pc.wrapping_add(size);
        }
    }

    /// Branches: fetches the opcodes at the address, in the state the CPU
    /// is in, so that r15 is two ahead of the next one to run.
    pub(crate) fn jump(&mut self, addr: u32) {
        self.cpu.seq = false;
        if self.cpu.thumb() {
            let addr = addr & !1;
            self.cpu.pipe[0] = u32::from(self.fetch16(addr));
            self.cpu.pipe[1] = u32::from(self.fetch16(addr.wrapping_add(2)));
            self.cpu.r[15] = addr.wrapping_add(4);
        } else {
            let addr = addr & !3;
            self.cpu.pipe[0] = self.fetch32(addr);
            self.cpu.pipe[1] = self.fetch32(addr.wrapping_add(4));
            self.cpu.r[15] = addr.wrapping_add(8);
        }
        self.cpu.flushed = true;
    }

    /// Writes a register; r15 is a branch.
    pub(crate) fn set_reg(&mut self, n: usize, value: u32) {
        if n == 15 {
            self.jump(value);
        } else {
            self.cpu.r[n] = value;
        }
    }

    /// Enters an exception's mode at its vector in the BIOS, in ARM state
    /// with interrupts off, saving the CPSR and the address to come back to.
    pub(crate) fn exception(&mut self, mode: u32, vector: u32, back: u32) {
        let cpsr = self.cpu.cpsr;
        self.cpu.switch_mode(mode);
        self.cpu.set_spsr(cpsr);
        self.cpu.r[14] = back;
        self.cpu.cpsr = self.cpu.cpsr & !T | I;
        self.jump(vector);
    }

    /// An IRQ, between instructions: it comes back (with SUBS PC, LR, #4)
    /// to the instruction that would have run.
    fn interrupt(&mut self) {
        let back = if self.cpu.thumb() { self.cpu.r[15] } else { self.cpu.r[15].wrapping_sub(4) };
        self.exception(IRQ, 0x18, back);
        self.cpu.flushed = false;
    }

    /// An opcode the CPU does not have (or a coprocessor's, which the GBA
    /// lacks): the undefined instruction exception.
    pub(crate) fn undefined(&mut self) {
        let back = self.cpu.r[15].wrapping_sub(if self.cpu.thumb() { 2 } else { 4 });
        self.exception(UNDEFINED, 0x04, back);
    }

    /// One internal cycle, after which the next fetch is non-sequential.
    pub(crate) fn idle(&mut self, cycles: u32) {
        self.cycles += u64::from(cycles);
        self.cpu.seq = false;
    }
}
