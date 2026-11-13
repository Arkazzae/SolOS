//! The VR4300 CPU, interpreted: its 64-bit registers, branches with their
//! delay slots, exceptions and interrupts, and the integer instructions
//! (COP0's are here too; the FPU's are in fpu.rs).
//!
//! Each instruction costs one cycle, as it does on the chip when it hits
//! the caches; the stalls of cache misses, multiplies and divides are not
//! counted.
//!
//! A branch's delay slot runs before its target: `pc` is the instruction
//! to run next and `next_pc` the one after it, which a branch sets to its
//! target while its delay slot is about to run. Exceptions in a delay slot
//! return to the branch (EPC points to it, with Cause.BD set).

use crate::cop0::{Access, BD, CU0, CU1, CU2, Cop0, ERL, EXL, Fault, constant_register};
use crate::decode::{Instr, Op, decode};
use crate::fpu::Fpu;
use crate::{N64, Registers, TIMER};
use savestate::state;

pub use crate::cop0::{IP2, IP7};

// Exception codes, in Cause bits 2-6.
const INTERRUPT: u32 = 0;
const TLB_MODIFIED: u32 = 1;
const TLB_LOAD: u32 = 2;
const TLB_STORE: u32 = 3;
const ADDRESS_LOAD: u32 = 4;
const ADDRESS_STORE: u32 = 5;
const SYSCALL: u32 = 8;
const BREAKPOINT: u32 = 9;
const RESERVED: u32 = 10;
const UNUSABLE: u32 = 11;
const OVERFLOW: u32 = 12;
const TRAP: u32 = 13;
pub const FLOATING_POINT: u32 = 15;

/// Where an instruction reads or writes memory: nowhere, a physical
/// address, or one not known without the TLB.
#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
pub(crate) enum Target {
    None,
    Load(u32),
    Store(u32),
    Unknown,
}

/// The control state comes first, adjacent, where compiled blocks write it
/// with paired and multiple stores (jit/compile.rs).
#[derive(Clone)]
#[repr(C)]
pub struct Cpu {
    pub pc: u64,
    pub(crate) next_pc: u64,
    /// The instruction running and whether it is in a delay slot, for the
    /// exceptions it may raise.
    pub(crate) current_pc: u64,
    /// The instruction at pc is in a branch's delay slot.
    pub(crate) delay_slot: bool,
    pub(crate) current_delay: bool,
    pub gpr: [u64; 32],
    pub hi: u64,
    pub lo: u64,
    /// LL's link, which SC needs to still be there.
    ll_bit: bool,
    pub cop0: Cop0,
    pub fpu: Fpu,
    /// What there is of COP2, which the VR4300 does not have: one 64-bit
    /// latch, which every move to or from COP2 reaches whatever its register
    /// number. Saved as a record (lib.rs), after the fields below.
    pub(crate) cop2: u64,
}

state!(Cpu { gpr, hi, lo, pc, next_pc, delay_slot, current_pc, current_delay, ll_bit, cop0, fpu });

/// A 32-bit result, sign-extended to 64 bits as the VR4300 keeps them.
fn sext(value: u32) -> u64 {
    value as i32 as i64 as u64
}

impl Cpu {
    pub fn new() -> Cpu {
        Cpu {
            gpr: [0; 32],
            hi: 0,
            lo: 0,
            pc: 0xffff_ffff_bfc0_0000,
            next_pc: 0xffff_ffff_bfc0_0004,
            delay_slot: false,
            current_pc: 0,
            current_delay: false,
            ll_bit: false,
            cop0: Cop0::new(),
            fpu: Fpu::new(),
            cop2: 0,
        }
    }

    pub fn registers(&self) -> Registers {
        Registers {
            gpr: self.gpr,
            hi: self.hi,
            lo: self.lo,
            pc: self.pc,
            status: self.cop0.status,
            cause: self.cop0.cause,
            epc: self.cop0.epc,
        }
    }

    /// Starts running at an address (as the boot does).
    pub fn jump(&mut self, pc: u64) {
        self.pc = pc;
        self.next_pc = pc.wrapping_add(4);
        self.delay_slot = false;
    }

    /// Pure compiled blocks can only begin on an ordinary sequential
    /// instruction, with no exception due before it.
    #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
    pub(crate) fn linear_block_pc(&self) -> Option<u64> {
        (!self.delay_slot && self.next_pc == self.pc.wrapping_add(4)
            && self.gpr[0] == 0 && !self.cop0.interrupt_pending()).then_some(self.pc)
    }

    /// Where an instruction about to run reads or writes memory, for a
    /// compiled block deciding whether it may go on after it.
    #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
    pub(crate) fn access_target(&self, i: Instr) -> Target {
        use Op::*;
        let store = match i.op {
            Lb | Lbu | Lh | Lhu | Lw | Lwu | Ld | Lwl | Lwr | Ldl | Ldr | Ll | Lld | Lwc1 | Ldc1 | Lwc2 | Ldc2 => false,
            Sb | Sh | Sw | Sd | Swl | Swr | Sdl | Sdr | Sc | Scd | Swc1 | Sdc1 | Swc2 | Sdc2 => true,
            _ => return Target::None,
        };
        let address = self.gpr[i.rs()].wrapping_add(i.simm());
        match self.cop0.translate_fast(address) {
            Some(physical) if store => Target::Store(physical),
            Some(physical) => Target::Load(physical),
            None => Target::Unknown,
        }
    }

    /// The bookkeeping left by a straight-line run of non-faulting
    /// instructions. Arithmetic and ordinary memory accesses were native.
    #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
    pub(crate) fn finish_linear_block(&mut self, instructions: usize) {
        self.current_pc = self.pc.wrapping_add((instructions as u64 - 1) * 4);
        self.current_delay = false;
        self.pc = self.pc.wrapping_add(instructions as u64 * 4);
        self.next_pc = self.pc.wrapping_add(4);
        self.delay_slot = false;
        self.gpr[0] = 0;
    }
}

impl N64 {
    /// Runs to the next device event. Unlike `step_cpu`, this can let time
    /// pass in a loop that only branches to itself with a NOP delay slot, or
    /// one that counts in it (ADDIU or DADDIU of a register the branch does
    /// not read, as Resident Evil 2's idle thread does). There is no memory
    /// polling here: reads from devices and other changes made by a delay
    /// slot must still run instruction by instruction.
    pub(crate) fn run_cpu_until_event(&mut self) {
        while self.cycles < self.next_event {
            #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
            let ran = self.jit.enabled && self.run_jit_blocks();
            #[cfg(not(any(target_arch = "arm", target_arch = "aarch64")))]
            let ran = false;
            if !ran {
                self.step_cpu();
            }
            if self.cpu.delay_slot
                && !self.cpu.current_delay
                && self.cpu.next_pc == self.cpu.current_pc
                && self.cpu.pc == self.cpu.current_pc.wrapping_add(4)
            {
                self.skip_idle_loop();
            }
        }
    }

    fn skip_idle_loop(&mut self) {
        if self.cycles >= self.next_event || self.rdram_unreadable {
            return;
        }
        // Only ordinary RDRAM, direct or through the TLB: looking ahead must
        // neither touch MMIO nor hide a fetch/TLB exception (a translation
        // that would fault leaves the loop alone). DMA cannot change these
        // words before the event we are stopping at, nor the loop the TLB.
        let fetch = |cop0: &Cop0, pc: u64| {
            cop0.translate_fast(pc).or_else(|| cop0.translate(pc, Access::Fetch).ok())
        };
        let Some(branch) = fetch(&self.cpu.cop0, self.cpu.current_pc) else { return };
        let Some(slot) = fetch(&self.cpu.cop0, self.cpu.pc) else { return };
        if branch & 3 != 0 || slot & 3 != 0
            || branch as usize > self.rdram.len() - 4
            || slot as usize > self.rdram.len() - 4
        {
            return;
        }
        self.touch_rdram(branch as usize, 4);
        self.touch_rdram(slot as usize, 4);
        let word = u32::from_be_bytes(self.rdram[branch as usize..branch as usize + 4].try_into().unwrap());
        let branch = decode(word);
        let reads = match branch.op {
            Op::J => [0, 0],
            Op::Jr | Op::Blez | Op::Bgtz | Op::Bltz | Op::Bgez | Op::Blezl | Op::Bgtzl | Op::Bltzl | Op::Bgezl => {
                [branch.rs(), 0]
            }
            Op::Beq | Op::Bne | Op::Beql | Op::Bnel => [branch.rs(), branch.rt()],
            _ => return,
        };
        // The slot: a NOP, or a count kept in a register the branch does not read.
        let slot = decode(u32::from_be_bytes(self.rdram[slot as usize..slot as usize + 4].try_into().unwrap()));
        let count = match slot.op {
            _ if slot.word == 0 => None,
            Op::Addiu | Op::Daddiu if slot.rs() == slot.rt() && slot.rt() != 0 && !reads.contains(&slot.rt()) => {
                Some(slot)
            }
            _ => return,
        };
        // The branch just ran, so its condition is known and, with such a
        // slot and no link register, cannot change before a device event.
        // Keep even the last executed instruction and delay-slot state:
        // an interrupt at the boundary must get exactly the same EPC/BD.
        let remaining = self.next_event - self.cycles;
        // The slot runs every other cycle from now, first.
        if let Some(slot) = count {
            let times = remaining.div_ceil(2);
            let r = &mut self.cpu.gpr[slot.rt()];
            let step = slot.simm().wrapping_mul(times);
            *r = if slot.op == Op::Addiu { sext((*r as u32).wrapping_add(step as u32)) } else { r.wrapping_add(step) };
        }
        if remaining & 1 != 0 {
            let slot_pc = self.cpu.pc;
            std::mem::swap(&mut self.cpu.pc, &mut self.cpu.next_pc);
            self.cpu.delay_slot = false;
            self.cpu.current_pc = slot_pc;
            self.cpu.current_delay = true;
        }
        self.cycles = self.next_event;
    }

    /// Runs one instruction, or takes an interrupt.
    pub(crate) fn step_cpu(&mut self) {
        self.cycles += 1;
        let pc = self.cpu.pc;
        self.cpu.current_pc = pc;
        self.cpu.current_delay = self.cpu.delay_slot;
        if self.cpu.cop0.interrupt_pending() {
            self.exception(INTERRUPT, 0x180);
            return;
        }
        let Some(word) = self.fetch(pc) else { return };
        self.execute_fetched(decode(word));
    }

    /// The cache already checked a sequential kernel fetch, its source
    /// bytes and pending interrupts. Reuse the interpreter's execution
    /// without fetching and decoding that same word a second time.
    #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
    pub(crate) fn step_cached(&mut self, instruction: Instr) {
        debug_assert!(self.cpu.linear_block_pc().is_some());
        self.cycles += 1;
        self.cpu.current_pc = self.cpu.pc;
        self.cpu.current_delay = false;
        self.execute_fetched(instruction);
    }

    #[inline]
    fn execute_fetched(&mut self, instruction: Instr) {
        self.cpu.pc = self.cpu.next_pc;
        self.cpu.next_pc = self.cpu.pc.wrapping_add(4);
        self.cpu.delay_slot = false;
        self.execute(instruction);
        self.cpu.gpr[0] = 0;
    }

    fn fetch(&mut self, pc: u64) -> Option<u32> {
        if let Some(phys) = self.cpu.cop0.translate_fast(pc)
            && pc & 3 == 0
        {
            return Some(self.read_u32(phys));
        }
        let phys = self.translate(pc, 4, Access::Fetch)?;
        Some(self.read_u32(phys))
    }

    /// Translates a data address of an access of `size` bytes, or raises
    /// the exception it causes (misaligned, out of reach, or the TLB's).
    #[inline]
    pub(crate) fn translate(&mut self, vaddr: u64, size: u64, access: Access) -> Option<u32> {
        if vaddr & (size - 1) == 0
            && let Some(phys) = self.cpu.cop0.translate_fast(vaddr)
        {
            return Some(phys);
        }
        let result = if vaddr & (size - 1) != 0 { Err(Fault::Address) } else { self.cpu.cop0.translate(vaddr, access) };
        match result {
            Ok(phys) => Some(phys),
            Err(fault) => {
                self.address_exception(vaddr, access, fault);
                None
            }
        }
    }

    fn address_exception(&mut self, vaddr: u64, access: Access, fault: Fault) {
        let store = access == Access::Store;
        let (code, offset) = match fault {
            Fault::Address => (if store { ADDRESS_STORE } else { ADDRESS_LOAD }, 0x180),
            // A miss outside an exception goes to the refill handler of the
            // mode's address size: the 64-bit one (XTLB) while the mode's KX,
            // SX or UX is set, for a 32-bit address too (n64-systemtest;
            // Lemmy's tests cannot tell, their handler at 0x000 runs on into
            // 0x080's).
            Fault::TlbMiss if self.cpu.cop0.status & EXL == 0 => {
                (if store { TLB_STORE } else { TLB_LOAD }, if self.cpu.cop0.wide() { 0x080 } else { 0x000 })
            }
            Fault::TlbMiss | Fault::TlbInvalid => (if store { TLB_STORE } else { TLB_LOAD }, 0x180),
            Fault::TlbModified => (TLB_MODIFIED, 0x180),
        };
        self.cpu.cop0.bad_address(vaddr);
        self.exception(code, offset);
    }

    /// Raises an exception: the code in Cause, the return address in EPC
    /// (unless one is being handled already), EXL set, and on to the
    /// vector.
    pub(crate) fn exception(&mut self, code: u32, offset: u64) {
        self.exception_in(code, offset, 0);
    }

    fn exception_in(&mut self, code: u32, offset: u64, coprocessor: u32) {
        let cpu = &mut self.cpu;
        let cop0 = &mut cpu.cop0;
        if cop0.status & EXL == 0 {
            if cpu.current_delay {
                cop0.epc = cpu.current_pc.wrapping_sub(4);
                cop0.cause |= BD;
            } else {
                cop0.epc = cpu.current_pc;
                cop0.cause &= !BD;
            }
        }
        cop0.cause = cop0.cause & !0x3000_007c | code << 2 | coprocessor << 28;
        cop0.status |= EXL;
        let vector = cop0.vector(offset);
        cpu.jump(vector);
    }

    fn unusable(&mut self, coprocessor: u32) {
        self.exception_in(UNUSABLE, 0x180, coprocessor);
    }

    /// Whether the FPU may be used; if not, the exception that says so.
    pub(crate) fn cop1_usable(&mut self) -> bool {
        if self.cpu.cop0.status & CU1 == 0 {
            self.unusable(1);
            return false;
        }
        true
    }

    fn reg(&self, i: usize) -> u64 {
        self.cpu.gpr[i]
    }

    fn set(&mut self, i: usize, value: u64) {
        self.cpu.gpr[i] = value;
    }

    /// A branch whose delay slot runs whether or not it is taken.
    fn branch(&mut self, taken: bool, target: u64) {
        self.cpu.delay_slot = true;
        if taken {
            self.cpu.next_pc = target;
        }
    }

    /// A likely branch: its delay slot runs only if it is taken.
    pub(crate) fn branch_likely(&mut self, taken: bool, target: u64) {
        if taken {
            self.cpu.delay_slot = true;
            self.cpu.next_pc = target;
        } else {
            let after = self.cpu.next_pc;
            self.cpu.jump(after);
        }
    }

    /// A branch's target, from the address of its delay slot.
    pub(crate) fn branch_target(&self, i: Instr) -> u64 {
        self.cpu.pc.wrapping_add(i.simm() << 2)
    }

    pub(crate) fn branch_on(&mut self, taken: bool, i: Instr) {
        let target = self.branch_target(i);
        self.branch(taken, target);
    }

    fn execute(&mut self, i: Instr) {
        let (rs, rt, rd) = (i.rs(), i.rt(), i.rd());
        let (s, t) = (self.reg(rs), self.reg(rt));
        match i.op {
            // The 64-bit instructions, reserved in supervisor and user mode
            // without 64-bit addresses (before any address is translated).
            Op::Dadd | Op::Daddi | Op::Daddiu | Op::Daddu | Op::Ddiv | Op::Ddivu | Op::Dmult | Op::Dmultu
            | Op::Dsll | Op::Dsll32 | Op::Dsllv | Op::Dsra | Op::Dsra32 | Op::Dsrav | Op::Dsrl | Op::Dsrl32
            | Op::Dsrlv | Op::Dsub | Op::Dsubu | Op::Ld | Op::Ldl | Op::Ldr | Op::Lld | Op::Lwu | Op::Scd
            | Op::Sd | Op::Sdl | Op::Sdr
                if !self.cpu.cop0.allows_64_bit() =>
            {
                self.exception(RESERVED, 0x180)
            }
            Op::Sll => self.set(rd, sext((t as u32) << i.sa())),
            Op::Srl => self.set(rd, sext(t as u32 >> i.sa())),
            // SRA shifts the whole 64-bit register, then keeps 32 bits.
            Op::Sra => self.set(rd, sext((t as i64 >> i.sa()) as u32)),
            Op::Sllv => self.set(rd, sext((t as u32) << (s & 31))),
            Op::Srlv => self.set(rd, sext(t as u32 >> (s & 31))),
            Op::Srav => self.set(rd, sext((t as i64 >> (s & 31)) as u32)),
            Op::Jr => self.branch(true, s),
            Op::Jalr => {
                let link = self.cpu.next_pc;
                self.branch(true, s);
                self.set(rd, link);
            }
            Op::Syscall => self.exception(SYSCALL, 0x180),
            Op::Break => self.exception(BREAKPOINT, 0x180),
            Op::Sync => {}
            Op::Mfhi => self.set(rd, self.cpu.hi),
            Op::Mthi => self.cpu.hi = s,
            Op::Mflo => self.set(rd, self.cpu.lo),
            Op::Mtlo => self.cpu.lo = s,
            Op::Dsllv => self.set(rd, t << (s & 63)),
            Op::Dsrlv => self.set(rd, t >> (s & 63)),
            Op::Dsrav => self.set(rd, (t as i64 >> (s & 63)) as u64),
            Op::Mult => {
                let product = i64::from(s as i32) * i64::from(t as i32);
                self.cpu.lo = sext(product as u32);
                self.cpu.hi = sext((product >> 32) as u32);
            }
            Op::Multu => {
                let product = u64::from(s as u32) * u64::from(t as u32);
                self.cpu.lo = sext(product as u32);
                self.cpu.hi = sext((product >> 32) as u32);
            }
            Op::Div => {
                let (n, d) = (s as i32, t as i32);
                (self.cpu.lo, self.cpu.hi) = match d {
                    // Dividing by zero gives -1 (1 for a negative dividend)
                    // and leaves the dividend as the remainder.
                    0 => (if n < 0 { 1 } else { u64::MAX }, sext(n as u32)),
                    _ => (sext(n.wrapping_div(d) as u32), sext(n.wrapping_rem(d) as u32)),
                };
            }
            Op::Divu => {
                let (n, d) = (s as u32, t as u32);
                (self.cpu.lo, self.cpu.hi) = match d {
                    0 => (u64::MAX, sext(n)),
                    _ => (sext(n / d), sext(n % d)),
                };
            }
            Op::Dmult => {
                let product = i128::from(s as i64) * i128::from(t as i64);
                (self.cpu.lo, self.cpu.hi) = (product as u64, (product >> 64) as u64);
            }
            Op::Dmultu => {
                let product = u128::from(s) * u128::from(t);
                (self.cpu.lo, self.cpu.hi) = (product as u64, (product >> 64) as u64);
            }
            Op::Ddiv => {
                let (n, d) = (s as i64, t as i64);
                (self.cpu.lo, self.cpu.hi) = match d {
                    0 => (if n < 0 { 1 } else { u64::MAX }, n as u64),
                    _ => (n.wrapping_div(d) as u64, n.wrapping_rem(d) as u64),
                };
            }
            Op::Ddivu => {
                (self.cpu.lo, self.cpu.hi) = match t {
                    0 => (u64::MAX, s),
                    _ => (s / t, s % t),
                };
            }
            Op::Add => match (s as i32).checked_add(t as i32) {
                Some(sum) => self.set(rd, sext(sum as u32)),
                None => self.exception(OVERFLOW, 0x180),
            },
            Op::Addu => self.set(rd, sext((s as u32).wrapping_add(t as u32))),
            Op::Sub => match (s as i32).checked_sub(t as i32) {
                Some(difference) => self.set(rd, sext(difference as u32)),
                None => self.exception(OVERFLOW, 0x180),
            },
            Op::Subu => self.set(rd, sext((s as u32).wrapping_sub(t as u32))),
            Op::And => self.set(rd, s & t),
            Op::Or => self.set(rd, s | t),
            Op::Xor => self.set(rd, s ^ t),
            Op::Nor => self.set(rd, !(s | t)),
            Op::Slt => self.set(rd, u64::from((s as i64) < (t as i64))),
            Op::Sltu => self.set(rd, u64::from(s < t)),
            Op::Dadd => match (s as i64).checked_add(t as i64) {
                Some(sum) => self.set(rd, sum as u64),
                None => self.exception(OVERFLOW, 0x180),
            },
            Op::Daddu => self.set(rd, s.wrapping_add(t)),
            Op::Dsub => match (s as i64).checked_sub(t as i64) {
                Some(difference) => self.set(rd, difference as u64),
                None => self.exception(OVERFLOW, 0x180),
            },
            Op::Dsubu => self.set(rd, s.wrapping_sub(t)),
            Op::Tge => self.trap((s as i64) >= (t as i64)),
            Op::Tgeu => self.trap(s >= t),
            Op::Tlt => self.trap((s as i64) < (t as i64)),
            Op::Tltu => self.trap(s < t),
            Op::Teq => self.trap(s == t),
            Op::Tne => self.trap(s != t),
            Op::Dsll => self.set(rd, t << i.sa()),
            Op::Dsrl => self.set(rd, t >> i.sa()),
            Op::Dsra => self.set(rd, (t as i64 >> i.sa()) as u64),
            Op::Dsll32 => self.set(rd, t << (i.sa() + 32)),
            Op::Dsrl32 => self.set(rd, t >> (i.sa() + 32)),
            Op::Dsra32 => self.set(rd, (t as i64 >> (i.sa() + 32)) as u64),

            Op::Bltz => self.branch_on((s as i64) < 0, i),
            Op::Bgez => self.branch_on((s as i64) >= 0, i),
            Op::Bltzl => self.branch_likely((s as i64) < 0, self.branch_target(i)),
            Op::Bgezl => self.branch_likely((s as i64) >= 0, self.branch_target(i)),
            Op::Tgei => self.trap((s as i64) >= i.simm() as i64),
            Op::Tgeiu => self.trap(s >= i.simm()),
            Op::Tlti => self.trap((s as i64) < i.simm() as i64),
            Op::Tltiu => self.trap(s < i.simm()),
            Op::Teqi => self.trap(s == i.simm()),
            Op::Tnei => self.trap(s != i.simm()),
            // The links are made whether or not the branch is taken.
            Op::Bltzal => {
                self.set(31, self.cpu.next_pc);
                self.branch_on((s as i64) < 0, i);
            }
            Op::Bgezal => {
                self.set(31, self.cpu.next_pc);
                self.branch_on((s as i64) >= 0, i);
            }
            Op::Bltzall => {
                self.set(31, self.cpu.next_pc);
                self.branch_likely((s as i64) < 0, self.branch_target(i));
            }
            Op::Bgezall => {
                self.set(31, self.cpu.next_pc);
                self.branch_likely((s as i64) >= 0, self.branch_target(i));
            }

            Op::J => self.branch(true, self.cpu.pc & !0x0fff_ffff | i.target()),
            Op::Jal => {
                self.set(31, self.cpu.next_pc);
                self.branch(true, self.cpu.pc & !0x0fff_ffff | i.target());
            }
            Op::Beq => self.branch_on(s == t, i),
            Op::Bne => self.branch_on(s != t, i),
            Op::Blez => self.branch_on((s as i64) <= 0, i),
            Op::Bgtz => self.branch_on((s as i64) > 0, i),
            Op::Beql => self.branch_likely(s == t, self.branch_target(i)),
            Op::Bnel => self.branch_likely(s != t, self.branch_target(i)),
            Op::Blezl => self.branch_likely((s as i64) <= 0, self.branch_target(i)),
            Op::Bgtzl => self.branch_likely((s as i64) > 0, self.branch_target(i)),
            Op::Addi => match (s as i32).checked_add(i.simm() as i32) {
                Some(sum) => self.set(rt, sext(sum as u32)),
                None => self.exception(OVERFLOW, 0x180),
            },
            Op::Addiu => self.set(rt, sext((s as u32).wrapping_add(i.simm() as u32))),
            Op::Slti => self.set(rt, u64::from((s as i64) < i.simm() as i64)),
            Op::Sltiu => self.set(rt, u64::from(s < i.simm())),
            Op::Andi => self.set(rt, s & i.uimm()),
            Op::Ori => self.set(rt, s | i.uimm()),
            Op::Xori => self.set(rt, s ^ i.uimm()),
            Op::Lui => self.set(rt, sext((i.word & 0xffff) << 16)),
            Op::Daddi => match (s as i64).checked_add(i.simm() as i64) {
                Some(sum) => self.set(rt, sum as u64),
                None => self.exception(OVERFLOW, 0x180),
            },
            Op::Daddiu => self.set(rt, s.wrapping_add(i.simm())),

            Op::Lb | Op::Lbu | Op::Lh | Op::Lhu | Op::Lw | Op::Lwu | Op::Ld | Op::Ll | Op::Lld => self.load(i),
            Op::Lwl | Op::Lwr | Op::Ldl | Op::Ldr => self.load_unaligned(i),
            Op::Sb | Op::Sh | Op::Sw | Op::Sd => self.store(i),
            Op::Swl | Op::Swr | Op::Sdl | Op::Sdr => self.store_unaligned(i),
            Op::Sc | Op::Scd => self.store_conditional(i),
            Op::Cache => {
                if !self.cpu.cop0.kernel() && self.cpu.cop0.status & CU0 == 0 {
                    self.unusable(0);
                }
            }

            Op::Mfc0 | Op::Dmfc0 | Op::Mtc0 | Op::Dmtc0 | Op::Tlbr | Op::Tlbwi | Op::Tlbwr | Op::Tlbp | Op::Eret
            | Op::Cop0Inert => self.cop0_instruction(i),
            Op::Mfc1
            | Op::Dmfc1
            | Op::Cfc1
            | Op::Mtc1
            | Op::Dmtc1
            | Op::Ctc1
            | Op::Bc1f
            | Op::Bc1t
            | Op::Bc1fl
            | Op::Bc1tl
            | Op::FpuOp
            | Op::FpuCvtInt
            | Op::FpuUnimplemented
            | Op::Lwc1
            | Op::Ldc1
            | Op::Swc1
            | Op::Sdc1 => self.fpu_instruction(i),
            Op::Mfc2
            | Op::Dmfc2
            | Op::Cfc2
            | Op::Mtc2
            | Op::Dmtc2
            | Op::Ctc2
            | Op::Lwc2
            | Op::Ldc2
            | Op::Swc2
            | Op::Sdc2
            | Op::Cop2 => self.cop2_instruction(i),
            // There is no COP3: whether usable or not, its instructions are
            // reserved ones, and Cause does not name it.
            Op::Cop3 | Op::Reserved => self.exception(RESERVED, 0x180),
        }
    }

    fn trap(&mut self, condition: bool) {
        if condition {
            self.exception(TRAP, 0x180);
        }
    }

    fn address(&self, i: Instr) -> u64 {
        self.reg(i.rs()).wrapping_add(i.simm())
    }

    fn load(&mut self, i: Instr) {
        let vaddr = self.address(i);
        let size = match i.op {
            Op::Lb | Op::Lbu => 1,
            Op::Lh | Op::Lhu => 2,
            Op::Lw | Op::Lwu | Op::Ll => 4,
            _ => 8,
        };
        let Some(phys) = self.translate(vaddr, size, Access::Load) else { return };
        let value = match i.op {
            Op::Lb => self.read_u8(phys) as i8 as u64,
            Op::Lbu => u64::from(self.read_u8(phys)),
            Op::Lh => self.read_u16(phys) as i16 as u64,
            Op::Lhu => u64::from(self.read_u16(phys)),
            Op::Lw | Op::Ll => sext(self.read_u32(phys)),
            Op::Lwu => u64::from(self.read_u32(phys)),
            _ => self.read_u64(phys),
        };
        if matches!(i.op, Op::Ll | Op::Lld) {
            self.cpu.ll_bit = true;
            self.cpu.cop0.ll_addr = phys >> 4;
        }
        self.set(i.rt(), value);
    }

    /// LWL, LWR, LDL and LDR: the part of an unaligned word (or doubleword)
    /// that lies in one aligned word, merged into the register. LWL takes
    /// the bytes from the address to the end of the word into the top of
    /// the register; LWR those from the start of the word to the address
    /// into the bottom. LWL always writes the word's sign bit and extends
    /// it; LWR only when it takes the whole word, and otherwise leaves bits
    /// 32-63 as they were, so a sign-extended register stays one
    /// (n64-systemtest's loads in 64-bit addressing mode; RustyN64 does the
    /// same in either mode).
    fn load_unaligned(&mut self, i: Instr) {
        let vaddr = self.address(i);
        let Some(phys) = self.translate(vaddr, 1, Access::Load) else { return };
        let old = self.reg(i.rt());
        let value = match i.op {
            Op::Lwl => {
                let shift = 8 * (phys & 3);
                let word = self.read_u32(phys & !3);
                sext(old as u32 & !(u32::MAX << shift) | word << shift)
            }
            Op::Lwr => {
                let shift = 8 * (3 - (phys & 3));
                let word = self.read_u32(phys & !3);
                let low = old as u32 & !(u32::MAX >> shift) | word >> shift;
                if shift == 0 { sext(low) } else { old & !0xffff_ffff | u64::from(low) }
            }
            Op::Ldl => {
                let shift = 8 * (phys & 7);
                let dword = self.read_u64(phys & !7);
                old & !(u64::MAX << shift) | dword << shift
            }
            _ => {
                let shift = 8 * (7 - (phys & 7));
                let dword = self.read_u64(phys & !7);
                old & !(u64::MAX >> shift) | dword >> shift
            }
        };
        self.set(i.rt(), value);
    }

    fn store(&mut self, i: Instr) {
        let vaddr = self.address(i);
        let t = self.reg(i.rt());
        let size = match i.op {
            Op::Sb => 1,
            Op::Sh => 2,
            Op::Sw => 4,
            _ => 8,
        };
        let Some(phys) = self.translate(vaddr, size, Access::Store) else { return };
        match i.op {
            Op::Sb => self.write_u8(phys, t as u32),
            Op::Sh => self.write_u16(phys, t as u32),
            Op::Sw => self.write_u32(phys, t as u32),
            _ => self.write_u64(phys, t),
        }
    }

    /// SWL, SWR, SDL and SDR, the stores that mirror LWL and the others.
    fn store_unaligned(&mut self, i: Instr) {
        let vaddr = self.address(i);
        let t = self.reg(i.rt());
        let Some(phys) = self.translate(vaddr, 1, Access::Store) else { return };
        match i.op {
            Op::Swl => {
                let shift = 8 * (phys & 3);
                let word = self.read_u32(phys & !3);
                self.write_u32(phys & !3, word & !(u32::MAX >> shift) | (t as u32) >> shift);
            }
            Op::Swr => {
                let shift = 8 * (3 - (phys & 3));
                let word = self.read_u32(phys & !3);
                self.write_u32(phys & !3, word & !(u32::MAX << shift) | (t as u32) << shift);
            }
            Op::Sdl => {
                let shift = 8 * (phys & 7);
                let dword = self.read_u64(phys & !7);
                self.write_u64(phys & !7, dword & !(u64::MAX >> shift) | t >> shift);
            }
            _ => {
                let shift = 8 * (7 - (phys & 7));
                let dword = self.read_u64(phys & !7);
                self.write_u64(phys & !7, dword & !(u64::MAX << shift) | t << shift);
            }
        }
    }

    /// SC and SCD: store only if LL's link is still there, and say so in
    /// the register.
    fn store_conditional(&mut self, i: Instr) {
        let vaddr = self.address(i);
        let size = if i.op == Op::Sc { 4 } else { 8 };
        let Some(phys) = self.translate(vaddr, size, Access::Store) else { return };
        if self.cpu.ll_bit {
            let t = self.reg(i.rt());
            if i.op == Op::Sc {
                self.write_u32(phys, t as u32);
            } else {
                self.write_u64(phys, t);
            }
        }
        self.set(i.rt(), u64::from(self.cpu.ll_bit));
    }

    fn cop0_instruction(&mut self, i: Instr) {
        if !self.cpu.cop0.kernel() && self.cpu.cop0.status & CU0 == 0 {
            self.unusable(0);
            return;
        }
        match i.op {
            Op::Mfc0 => {
                let value = self.read_cop0(i.rd());
                self.set(i.rt(), sext(value as u32));
            }
            Op::Dmfc0 => {
                let value = self.read_cop0(i.rd());
                self.set(i.rt(), value);
            }
            Op::Mtc0 => self.write_cop0(i.rd(), sext(self.reg(i.rt()) as u32)),
            Op::Dmtc0 => self.write_cop0(i.rd(), self.reg(i.rt())),
            Op::Tlbr => self.cpu.cop0.tlb_read(),
            Op::Tlbwi => {
                let index = self.cpu.cop0.index as usize;
                self.cpu.cop0.tlb_write(index);
            }
            Op::Tlbwr => {
                let random = self.cpu.cop0.random(self.cycles) as usize;
                self.cpu.cop0.tlb_write(random);
            }
            Op::Tlbp => self.cpu.cop0.tlb_probe(),
            Op::Cop0Inert => {}
            _ => {
                // ERET: back from an error (ERL) or an exception (EXL),
                // without a delay slot, and LL's link broken.
                let cop0 = &mut self.cpu.cop0;
                let target = if cop0.status & ERL != 0 {
                    cop0.status &= !ERL;
                    cop0.error_epc
                } else {
                    cop0.status &= !EXL;
                    cop0.epc
                };
                self.cpu.ll_bit = false;
                self.cpu.jump(target);
            }
        }
    }

    /// COP2, which the VR4300 does not have (n64-systemtest's findings on
    /// hardware). With Status.CU2 clear any COP2 instruction is unusable.
    /// Otherwise its moves reach one 64-bit latch, whatever the register
    /// number: MTC2, DMTC2 and CTC2 write the whole source register, MFC2
    /// and CFC2 read the latch's low word sign-extended, DMFC2 all of it.
    /// LWC2 and LDC2 load the aligned doubleword that holds the address,
    /// SWC2 stores the low word and SDC2 the whole latch (from a test the
    /// suite marks poorly understood and leaves out by default). The rest
    /// (DCFC2, DCTC2, BC2 and the operations) are reserved instructions with
    /// Cause naming COP2.
    fn cop2_instruction(&mut self, i: Instr) {
        if self.cpu.cop0.status & CU2 == 0 {
            self.unusable(2);
            return;
        }
        match i.op {
            Op::Mfc2 | Op::Cfc2 => self.set(i.rt(), sext(self.cpu.cop2 as u32)),
            Op::Dmfc2 => self.set(i.rt(), self.cpu.cop2),
            Op::Mtc2 | Op::Dmtc2 | Op::Ctc2 => self.cpu.cop2 = self.reg(i.rt()),
            Op::Lwc2 | Op::Ldc2 => {
                let size = if i.op == Op::Lwc2 { 4 } else { 8 };
                let Some(phys) = self.translate(self.address(i), size, Access::Load) else { return };
                self.cpu.cop2 = self.read_u64(phys & !7);
            }
            Op::Swc2 => {
                let Some(phys) = self.translate(self.address(i), 4, Access::Store) else { return };
                self.write_u32(phys, self.cpu.cop2 as u32);
            }
            Op::Sdc2 => {
                let Some(phys) = self.translate(self.address(i), 8, Access::Store) else { return };
                self.write_u64(phys, self.cpu.cop2);
            }
            _ => self.exception_in(RESERVED, 0x180, 2),
        }
    }

    fn read_cop0(&mut self, reg: usize) -> u64 {
        let cop0 = &self.cpu.cop0;
        match reg {
            0 => u64::from(cop0.index),
            1 => u64::from(cop0.random(self.cycles)),
            2 | 3 => cop0.entry_lo[reg - 2],
            4 => cop0.context,
            5 => cop0.page_mask,
            6 => u64::from(cop0.wired),
            8 => cop0.bad_vaddr,
            9 => u64::from(self.count()),
            10 => cop0.entry_hi,
            11 => u64::from(cop0.compare),
            12 => u64::from(cop0.status),
            13 => u64::from(cop0.cause),
            14 => cop0.epc,
            16 => u64::from(cop0.config),
            17 => u64::from(cop0.ll_addr),
            18 => u64::from(cop0.watch_lo),
            19 => u64::from(cop0.watch_hi),
            20 => cop0.x_context,
            26 => u64::from(cop0.parity_error),
            28 => u64::from(cop0.tag_lo),
            30 => cop0.error_epc,
            _ => constant_register(reg).unwrap_or(cop0.latch),
        }
    }

    fn write_cop0(&mut self, reg: usize, value: u64) {
        let cycles = self.cycles;
        let cop0 = &mut self.cpu.cop0;
        cop0.latch = value;
        let low = value as u32;
        match reg {
            0 => cop0.index = low & 0x8000_003f,
            2 | 3 => cop0.entry_lo[reg - 2] = value & 0x3fff_ffff,
            4 => cop0.context = cop0.context & 0x7f_fff0 | value & !0x7f_ffff,
            5 => cop0.page_mask = value & 0x01ff_e000,
            6 => {
                cop0.wired = low & 0x3f;
                cop0.random_cycle = cycles;
            }
            9 => {
                cop0.count = low;
                cop0.count_cycle = cycles;
                self.schedule_timer();
            }
            10 => cop0.entry_hi = value & 0xc000_00ff_ffff_e0ff,
            11 => {
                cop0.compare = low;
                cop0.set_ip(IP7, false);
                self.schedule_timer();
            }
            12 => cop0.status = low & 0xff57_ffff,
            // Only the two software interrupts can be set.
            13 => cop0.cause = cop0.cause & !0x300 | low & 0x300,
            14 => cop0.epc = value,
            16 => cop0.config = cop0.config & !0x0f00_800f | low & 0x0f00_800f,
            17 => cop0.ll_addr = low,
            18 => cop0.watch_lo = low & 0xffff_fffb,
            19 => cop0.watch_hi = low & 0xf,
            20 => cop0.x_context = cop0.x_context & 0x1_ffff_fff0 | value & !0x1_ffff_ffff,
            26 => cop0.parity_error = low & 0xff,
            28 => cop0.tag_lo = low & 0x0fff_ffc0,
            30 => cop0.error_epc = value,
            _ => {}
        }
    }

    /// Count: one more every two cycles.
    fn count(&self) -> u32 {
        let cop0 = &self.cpu.cop0;
        cop0.count.wrapping_add(((self.cycles - cop0.count_cycle) >> 1) as u32)
    }

    /// Plans the timer's interrupt for when Count next equals Compare.
    pub(crate) fn schedule_timer(&mut self) {
        let cop0 = &self.cpu.cop0;
        let ticks = (self.cycles - cop0.count_cycle) >> 1;
        let left = match cop0.compare.wrapping_sub(self.count()) {
            0 => 1 << 32,
            n => u64::from(n),
        };
        let at = cop0.count_cycle + 2 * (ticks + left);
        self.schedule(TIMER, at);
    }

    pub(crate) fn timer_event(&mut self, _at: u64) {
        self.cpu.cop0.set_ip(IP7, true);
        self.schedule_timer();
    }
}

#[cfg(test)]
mod idle_tests {
    use super::*;
    use crate::cop0::IE;
    use savestate::State;

    const PC: u64 = 0xffff_ffff_8000_1000;

    fn machine(branch: u32, slot: u32) -> N64 {
        let mut rom = vec![0; 0x1000];
        rom[..4].copy_from_slice(&0x8037_1240u32.to_be_bytes());
        let mut n64 = N64::new(&rom).unwrap();
        n64.set_jit(true);
        n64.cpu.jump(PC);
        n64.cpu.cop0.status = 0;
        n64.cpu.cop0.cause = 0;
        n64.cpu.gpr[1] = 1;
        n64.cpu.gpr[3] = u64::MAX;
        n64.cpu.gpr[4] = PC;
        n64.write_u32(0x1000, branch);
        n64.write_u32(0x1004, slot);
        n64
    }

    fn run_reference(n64: &mut N64) {
        while n64.cycles < n64.next_event {
            n64.step_cpu();
        }
    }

    fn same(fast: &N64, reference: &N64) {
        let (mut a, mut b) = (Vec::new(), Vec::new());
        fast.cpu.save(&mut a);
        reference.cpu.save(&mut b);
        assert_eq!(fast.cycles, reference.cycles);
        assert_eq!(a, b, "CPU state differs at cycle {}", fast.cycles);
        assert_eq!(fast.cpu.cop2, reference.cpu.cop2);
        assert_eq!(fast.rdram, reference.rdram);
        assert_eq!(fast.events, reference.events);
        assert_eq!(fast.next_event, reference.next_event);
    }

    #[test]
    fn self_branches_match_every_instruction_at_event_boundaries() {
        let branches = [
            0x1000_ffff, // beq zero, zero, self
            0x1420_ffff, // bne at, zero, self
            0x1800_ffff, // blez zero, self
            0x1c20_ffff, // bgtz at, self
            0x0460_ffff, // bltz v1, self
            0x0401_ffff, // bgez zero, self
            0x5000_ffff, // beql zero, zero, self
            0x5420_ffff, // bnel at, zero, self
            0x5800_ffff, // blezl zero, self
            0x5c20_ffff, // bgtzl at, self
            0x0462_ffff, // bltzl v1, self
            0x0403_ffff, // bgezl zero, self
            0x0800_0400, // j self
            0x0080_0008, // jr a0 (self)
        ];
        for branch in branches {
            let start = machine(branch, 0);
            for initial_steps in 0..=1 {
                for deadline in [0, 1, 2, 3, 4, 5, 8, 31, 32, 101] {
                    let mut fast = start.clone();
                    for _ in 0..initial_steps {
                        fast.step_cpu();
                    }
                    fast.next_event = deadline;
                    let mut reference = fast.clone();
                    fast.run_cpu_until_event();
                    run_reference(&mut reference);
                    same(&fast, &reference);
                }
            }
        }
    }

    #[test]
    fn self_branches_counting_in_the_slot_match_every_instruction() {
        let branches = [
            0x1000_ffff, // beq zero, zero, self
            0x0800_0400, // j self
            0x1c20_ffff, // bgtz at, self: reads the count below, which then runs instruction by instruction
            0x1420_ffff, // bne at, zero, self
        ];
        let slots = [
            0x6442_0001, // daddiu v0, v0, 1
            0x2442_fffd, // addiu v0, v0, -3
            0x2463_0001, // addiu v1, v1, 1
            0x6421_ffff, // daddiu at, at, -1
            0x2422_0001, // addiu v0, at, 1: not a count
        ];
        for branch in branches {
            for slot in slots {
                let mut start = machine(branch, slot);
                start.cpu.gpr[1] = 40;
                // Not sign-extended from 32 bits: ADDIU takes the low half.
                start.cpu.gpr[2] = 0x1_7fff_fffe;
                for deadline in [0, 1, 2, 3, 4, 5, 8, 31, 32, 101] {
                    let mut fast = start.clone();
                    fast.next_event = deadline;
                    let mut reference = fast.clone();
                    fast.run_cpu_until_event();
                    run_reference(&mut reference);
                    same(&fast, &reference);
                }
            }
        }
    }

    #[test]
    fn interrupt_after_idle_has_the_same_epc_and_branch_delay_bit() {
        for deadline in [1, 2, 31, 32] {
            let mut fast = machine(0x1000_ffff, 0);
            fast.cpu.cop0.status = IE | IP7;
            fast.next_event = deadline;
            let mut reference = fast.clone();
            fast.run_cpu_until_event();
            run_reference(&mut reference);
            for n64 in [&mut fast, &mut reference] {
                n64.cpu.cop0.set_ip(IP7, true);
                n64.step_cpu();
                assert_eq!(n64.cpu.cop0.epc, PC);
                assert_eq!(n64.cpu.cop0.cause & BD != 0, deadline & 1 != 0);
            }
            same(&fast, &reference);
        }
    }

    #[test]
    fn pending_interrupts_and_delay_slot_side_effects_still_execute() {
        for case in 0..3 {
            let slot = match case {
                1 => 0x0000_000c, // syscall in the delay slot
                2 => 0x8c88_0000, // lw t0, 0(a0): taking the SP semaphore
                _ => 0,
            };
            let mut fast = machine(0x1000_ffff, slot);
            fast.cpu.cop0.status = IE | IP7;
            if case == 0 {
                fast.cpu.cop0.set_ip(IP7, true);
            }
            if case == 2 {
                fast.cpu.gpr[4] = 0xffff_ffff_a404_001c;
            }
            fast.next_event = if case == 2 { 101 } else { 2 };
            let mut reference = fast.clone();
            fast.run_cpu_until_event();
            run_reference(&mut reference);
            same(&fast, &reference);
            assert!(fast.save_state() == reference.save_state());
            match case {
                0 => assert_eq!(fast.cpu.cop0.cause & BD, 0),
                1 => {
                    assert_eq!(fast.cpu.cop0.cause & 0x7c, SYSCALL << 2);
                    assert_ne!(fast.cpu.cop0.cause & BD, 0);
                    assert_eq!(fast.cpu.cop0.epc, PC);
                }
                _ => assert_eq!(fast.cpu.gpr[8], 1),
            }
        }
    }

    #[test]
    fn side_effects_and_link_registers_are_never_skipped() {
        for (branch, slot) in [
            (0x1000_ffff, 0x2508_0001), // addiu t0, t0, 1 in the slot
            (0x1421_ffff, 0), // not-taken bne at, at
            (0x5421_ffff, 0x2508_0001), // not-taken likely branch annuls the slot
            (0x0020_0809, 0), // jalr at, at: linking changes the next target
            (0x0430_ffff, 0), // bltzal at: links even when not taken
        ] {
            let mut fast = machine(branch, slot);
            if branch == 0x0020_0809 {
                fast.cpu.gpr[1] = PC;
            }
            fast.next_event = 101;
            let mut reference = fast.clone();
            fast.run_cpu_until_event();
            run_reference(&mut reference);
            same(&fast, &reference);
        }
    }

    #[test]
    fn code_is_rechecked_after_each_device_event() {
        let mut fast = machine(0x1000_ffff, 0);
        let mut reference = fast.clone();
        for (deadline, slot) in [(31, 0), (60, 0x2508_0001), (91, 0)] {
            for n64 in [&mut fast, &mut reference] {
                n64.write_u32(0x1004, slot);
                n64.next_event = deadline;
            }
            fast.run_cpu_until_event();
            run_reference(&mut reference);
            same(&fast, &reference);
        }
        assert_ne!(fast.cpu.gpr[8], 0);
    }

    #[test]
    fn lookahead_does_not_hide_fetch_faults_or_read_other_memory() {
        for case in 0..5 {
            let mut fast = machine(0x1000_ffff, 0);
            match case {
                0 => fast.cpu.cop0.status = 0x10, // user mode cannot fetch kseg0
                1 => fast.cpu.jump(PC + 2), // misaligned fetch
                2 => {
                    fast.cpu.cop0.status = ERL; // valid unmapped kuseg, outside the fast path
                    fast.cpu.jump(0x1000);
                }
                3 => {
                    fast.write_u32(0x7f_fffc, 0x1000_ffff);
                    fast.cpu.jump(0xffff_ffff_807f_fffc); // slot lies beyond RDRAM
                }
                _ => fast.rdram_unreadable = true, // boot-time RDRAM calibration
            }
            fast.next_event = 31;
            let mut reference = fast.clone();
            fast.run_cpu_until_event();
            run_reference(&mut reference);
            same(&fast, &reference);
        }
    }
}

#[cfg(test)]
mod coprocessor_tests {
    use super::*;

    const PC: u64 = 0xffff_ffff_8000_1000;
    const DATA: u64 = 0xffff_ffff_8000_2000;

    /// A machine about to run `program` in kernel mode, with COP2 usable or
    /// not.
    fn machine(program: &[u32], usable: bool) -> N64 {
        let mut rom = vec![0; 0x1000];
        rom[..4].copy_from_slice(&0x8037_1240u32.to_be_bytes());
        let mut n64 = N64::new(&rom).unwrap();
        n64.cpu.jump(PC);
        n64.cpu.cop0.status = if usable { CU2 } else { 0 };
        n64.cpu.cop0.cause = 0;
        n64.cpu.gpr[4] = 0x0123_4567_89ab_cdef;
        n64.cpu.gpr[12] = DATA;
        for (k, &word) in program.iter().enumerate() {
            n64.write_u32(0x1000 + 4 * k as u32, word);
        }
        n64.write_u64(0x2000, 0xfedc_ba98_7654_3210);
        n64
    }

    fn run(n64: &mut N64, instructions: usize) {
        for _ in 0..instructions {
            n64.step_cpu();
        }
    }

    // The moves, with the register number in rd; loads and stores off r12.
    fn mfc2(rt: u32, rd: u32) -> u32 {
        0x4800_0000 | rt << 16 | rd << 11
    }

    fn dmfc2(rt: u32, rd: u32) -> u32 {
        0x4820_0000 | rt << 16 | rd << 11
    }

    fn cop2_memory(op: u32, rt: u32, offset: u32) -> u32 {
        op << 26 | 12 << 21 | rt << 16 | offset
    }

    #[test]
    fn moves_reach_one_latch_whatever_the_register() {
        let mut n64 = machine(&[
            0x4884_2800,   // mtc2 a0, 5: all 64 bits
            mfc2(5, 6),    // the low word, sign-extended
            dmfc2(6, 31),  // all of it
            0x4847_0000,   // cfc2 a3, 0: as MFC2
            0x48a0_1800,   // dmtc2 zero, 3
            dmfc2(8, 9),
            0x48c4_0800,   // ctc2 a0, 1: all 64 bits too
            dmfc2(9, 2),
        ], true);
        run(&mut n64, 8);
        let gpr = n64.cpu.gpr;
        assert_eq!(gpr[5], 0xffff_ffff_89ab_cdef);
        assert_eq!(gpr[6], 0x0123_4567_89ab_cdef);
        assert_eq!(gpr[7], 0xffff_ffff_89ab_cdef);
        assert_eq!((gpr[8], gpr[9]), (0, 0x0123_4567_89ab_cdef));
        assert_eq!((n64.cpu.pc, n64.cpu.cop0.cause), (PC + 32, 0));
    }

    #[test]
    fn loads_and_stores_go_through_the_latch() {
        let mut n64 = machine(&[
            cop2_memory(0x32, 9, 4),     // lwc2: the doubleword holding the word
            dmfc2(5, 9),
            cop2_memory(0x3e, 10, 16),   // sdc2: all 64 bits
            cop2_memory(0x3a, 11, 28),   // swc2: the low word
            0x4880_0000,                 // mtc2 zero, 0
            cop2_memory(0x36, 9, 0),     // ldc2
            dmfc2(6, 0),
        ], true);
        run(&mut n64, 7);
        assert_eq!((n64.cpu.gpr[5], n64.cpu.gpr[6]), (0xfedc_ba98_7654_3210, 0xfedc_ba98_7654_3210));
        assert_eq!(n64.read_u64(0x2010), 0xfedc_ba98_7654_3210);
        assert_eq!(n64.read_u64(0x2018), 0x7654_3210);
        assert_eq!(n64.cpu.cop0.cause, 0);
        // An unaligned LDC2 is an address error.
        let mut n64 = machine(&[cop2_memory(0x36, 9, 4)], true);
        run(&mut n64, 1);
        assert_eq!(n64.cpu.cop0.cause & 0x7c, ADDRESS_LOAD << 2);
    }

    #[test]
    fn the_rest_is_reserved_and_names_cop2_and_cop3_is_not_there() {
        for (word, usable, code, unit) in [
            (0x4860_0000, true, RESERVED, 2),    // dcfc2
            (0x48e0_0000, true, RESERVED, 2),    // dctc2
            (0x4900_0000, true, RESERVED, 2),    // bc2f
            (0x4a00_0000, true, RESERVED, 2),    // an operation
            (mfc2(5, 6), false, UNUSABLE, 2),
            (0x4860_0000, false, UNUSABLE, 2),
            (cop2_memory(0x32, 9, 0), false, UNUSABLE, 2),
            (0x4c00_0000, true, RESERVED, 0),    // COP3
            (0x4c00_0000, false, RESERVED, 0),
        ] {
            let mut n64 = machine(&[word], usable);
            run(&mut n64, 1);
            assert_eq!(n64.cpu.cop0.cause, code << 2 | unit << 28, "{word:08x} {usable}");
            assert_eq!(n64.cpu.cop0.epc, PC);
            assert_eq!(n64.cpu.gpr[5], 0);
        }
    }

    #[test]
    fn cop0_functions_from_0x20_do_nothing() {
        let mut n64 = machine(&[0x4200_0020, 0x4200_003f, 0x4210_0025], false);
        run(&mut n64, 3);
        assert_eq!((n64.cpu.pc, n64.cpu.cop0.cause), (PC + 12, 0));
        // But, as other COP0 instructions, not in user mode without CU0 (run
        // as if fetched: user code cannot be in kseg0).
        let mut n64 = machine(&[], false);
        n64.cpu.cop0.status = 0x10;
        n64.cpu.current_pc = PC;
        n64.execute_fetched(decode(0x4200_0020));
        assert_eq!((n64.cpu.cop0.epc, n64.cpu.cop0.cause), (PC, UNUSABLE << 2));
    }

    #[test]
    fn the_latch_is_saved_and_a_version_3_state_clears_it() {
        let mut n64 = machine(&[], true);
        n64.cpu.cop2 = 0x0123_4567_89ab_cdef;
        let state = n64.save_state();
        let old = n64.save_state_version_3();
        let mut other = machine(&[], true);
        other.load_state(&state).unwrap();
        assert_eq!(other.cpu.cop2, 0x0123_4567_89ab_cdef);
        other.load_state(&old).unwrap();
        assert_eq!(other.cpu.cop2, 0);
    }
}

#[cfg(test)]
mod mode_tests {
    use super::*;

    const PC: u64 = 0xffff_ffff_8000_1000;
    const DATA: u64 = 0xffff_ffff_8000_2000;
    const KX: u32 = 0x80;

    /// A machine with this Status, r12 pointing at a doubleword of data.
    fn machine(status: u32) -> N64 {
        let mut rom = vec![0; 0x1000];
        rom[..4].copy_from_slice(&0x8037_1240u32.to_be_bytes());
        let mut n64 = N64::new(&rom).unwrap();
        n64.cpu.jump(PC);
        n64.cpu.cop0.status = status;
        n64.cpu.cop0.cause = 0;
        n64.cpu.gpr[12] = DATA;
        n64.write_u64(0x2000, 0xbadd_ecaf_0123_4567);
        n64
    }

    /// Runs `word` as if fetched from PC (so in any mode).
    fn run(n64: &mut N64, word: u32) {
        n64.cpu.current_pc = PC;
        n64.execute_fetched(decode(word));
    }

    /// PRId, CacheErr and TagHi keep their value through a write (MTC0 sets
    /// the latch, which a register with nothing behind it reads).
    #[test]
    fn prid_cacheerr_and_taghi_read_the_same_whatever_is_written() {
        let mut n64 = machine(0);
        n64.cpu.gpr[8] = 0x1234_5678;
        for (rd, value) in [(15u32, 0x0b22u64), (27, 0), (29, 0)] {
            run(&mut n64, 0x4088_0000 | rd << 11);
            run(&mut n64, 0x4009_0000 | rd << 11);
            assert_eq!(n64.cpu.gpr[9], value, "MFC0 of {rd}");
            n64.cpu.gpr[9] = 0xdead;
            run(&mut n64, 0x4029_0000 | rd << 11);
            assert_eq!(n64.cpu.gpr[9], value, "DMFC0 of {rd}");
        }
        run(&mut n64, 0x4009_3800);
        assert_eq!(n64.cpu.gpr[9], 0x1234_5678, "the latch");
    }

    /// LWR at each byte of a word, into a register whose upper half is not
    /// its lower half's sign extension: only the whole word (the last byte)
    /// is sign-extended; the others leave bits 32-63 alone, in 32-bit and
    /// in 64-bit addressing mode. LWL always writes the sign bit.
    #[test]
    fn lwr_keeps_the_upper_half_unless_it_loads_the_whole_word() {
        for status in [0, KX] {
            for (byte, expected) in [
                (0, 0xbeef_0000_0102_03ba),
                (1, 0xbeef_0000_0102_badd),
                (2, 0xbeef_0000_01ba_ddec),
                (3, 0xffff_ffff_badd_ecaf),
            ] {
                let mut n64 = machine(status);
                n64.cpu.gpr[5] = 0xbeef_0000_0102_0304;
                run(&mut n64, 0x9985_0000 | byte); // lwr a1, byte(t4)
                assert_eq!(n64.cpu.gpr[5], expected, "LWR at byte {byte}, Status {status:x}");
            }
            let mut n64 = machine(status);
            n64.cpu.gpr[5] = 0x1234_5678_0102_0304;
            run(&mut n64, 0x8985_0001); // lwl a1, 1(t4)
            assert_eq!(n64.cpu.gpr[5], 0xffff_ffff_ddec_af04);
        }
    }

    /// The 64-bit instructions (as n64-systemtest's privilege test lists
    /// them, and LWU and the shifts by 32 or more) are reserved ones in
    /// supervisor and user mode unless SX or UX gives the mode 64-bit
    /// addresses; kernel mode (KSU 0, or EXL or ERL set) runs them whatever
    /// KX says. The 32-bit ones run everywhere.
    #[test]
    fn the_64_bit_instructions_are_reserved_in_32_bit_supervisor_and_user_mode() {
        let wide = [
            0x0109_502c, 0x610a_1234, 0x650a_1234, 0x0109_502d, 0x0109_001e, 0x0109_001f, 0x0109_001c, 0x0109_001d,
            0x0009_5178, 0x0009_517c, 0x0109_5014, 0x0009_517b, 0x0009_517f, 0x0109_5017, 0x0009_517a, 0x0009_517e,
            0x0109_5016, 0x0109_502e, 0x0109_502f, 0xdd85_0000, 0x6985_0000, 0x6d85_0000, 0xd185_0000, 0x9d85_0000,
            0xf185_0000, 0xfd85_0000, 0xb185_0000, 0xb585_0000,
        ];
        let narrow = [0x0109_5021, 0x0109_0018, 0x0109_001a, 0x0009_5140, 0x8d85_0000, 0x8985_0000];
        let (supervisor, user, sx, ux) = (0x08, 0x10, 0x40, 0x20);
        for (status, reserved) in [
            (0, false),
            (KX, false),
            (user | EXL, false),
            (supervisor | ERL, false),
            (supervisor, true),
            (supervisor | ux | KX, true),
            (supervisor | sx, false),
            (user, true),
            (user | sx | KX, true),
            (user | ux, false),
            (0x18, true),
            (0x18 | ux, false),
        ] {
            for (word, wide) in wide.iter().map(|&w| (w, true)).chain(narrow.iter().map(|&w| (w, false))) {
                let mut n64 = machine(status);
                run(&mut n64, word);
                let code = n64.cpu.cop0.cause >> 2 & 31;
                if wide && reserved {
                    assert_eq!((code, n64.cpu.pc, n64.cpu.cop0.epc), (RESERVED, 0xffff_ffff_8000_0180, PC),
                        "{word:08x}, Status {status:x}");
                } else {
                    assert!(n64.cpu.cop0.cause == 0 || code != RESERVED, "{word:08x}, Status {status:x}");
                }
            }
        }
    }

    /// LW a1, 0(t4) from `address` with this Status, and what it left: the
    /// exception's vector and code, EntryHi, BadVAddr, Context and XContext.
    fn load(address: u64, status: u32) -> (u64, u32, u64, u64, u64, u64) {
        let mut n64 = machine(status);
        n64.cpu.cop0.entry_hi = 0x5a;
        n64.cpu.gpr[12] = address;
        run(&mut n64, 0x8d85_0000);
        let cop0 = &n64.cpu.cop0;
        (n64.cpu.pc, cop0.cause >> 2 & 31, cop0.entry_hi, cop0.bad_vaddr, cop0.context, cop0.x_context)
    }

    /// A TLB miss goes to the refill handler of the mode's address size,
    /// whatever the address's: kuseg's 32-bit addresses with KX set go to
    /// the 64-bit one (0x080), as n64-systemtest's do; in user and
    /// supervisor mode UX and SX decide.
    #[test]
    fn a_tlb_miss_takes_the_refill_vector_of_the_modes_address_size() {
        let refill = 0xffff_ffff_8000_0000;
        for (status, address, offset) in [
            (0, 0x0020_fff4, 0),
            (KX, 0x0020_fff4, 0x80),
            (KX, 0, 0x80),
            (KX, 0x00ff_ffff_e000, 0x80),
            (KX, 0x4000_0000_0000_0000, 0x80),
            (KX, 0xffff_ffff_c000_0000, 0x80),
            (0, 0xffff_ffff_c000_0000, 0),
            (0x10, 0x1000, 0),
            (0x10 | 0x20, 0x1000, 0x80),
            (0x08 | 0x40, 0x1000, 0x80),
            (0x08 | 0x20, 0x1000, 0),
        ] {
            let (pc, code, entry_hi, bad, _, _) = load(address, status);
            assert_eq!((pc, code), (refill + offset, TLB_LOAD), "Status {status:x}, {address:x}");
            assert_eq!((entry_hi, bad), (address & 0xc000_00ff_ffff_e000 | 0x5a, address));
        }
    }

    /// xkseg ends 2 GB short of 2^40: past it is an address error. An
    /// address error leaves the address's region and VPN2 in EntryHi (the
    /// ASID kept), Context and XContext as a TLB exception does (the
    /// expectations of n64-systemtest's "LW TLB Miss or Address Exception
    /// (64 bit addressing mode)").
    #[test]
    fn address_errors_fill_in_entry_hi_and_xkseg_ends_2_gb_short() {
        let general = 0xffff_ffff_8000_0180;
        for (address, code) in [
            (0xc000_00ff_7fff_fff0, TLB_LOAD),
            (0xc000_00ff_8000_0000, ADDRESS_LOAD),
            (0xc000_00ff_f000_0004, ADDRESS_LOAD),
            (0xc000_00ff_ffff_fff4, ADDRESS_LOAD),
            (0xc000_0100_0020_fff4, ADDRESS_LOAD),
            (0xc000_0000_0000_0001, ADDRESS_LOAD),
            (0x0000_0000_0000_0001, ADDRESS_LOAD),
            (0x00ff_ffff_fffb, ADDRESS_LOAD),
            (0x0100_0020_fff4, ADDRESS_LOAD),
            (0x4000_00ff_ffff_fffb, ADDRESS_LOAD),
            (0x4000_0100_0020_fff4, ADDRESS_LOAD),
        ] {
            let (pc, got, entry_hi, bad, context, x_context) = load(address, KX);
            let vector = if code == TLB_LOAD { general - 0x100 } else { general };
            let vpn2 = address >> 13 & 0x7ff_ffff;
            assert_eq!((pc, got), (vector, code), "{address:x}");
            assert_eq!((entry_hi, bad), (vpn2 << 13 | address & 0xc000_0000_0000_0000 | 0x5a, address), "{address:x}");
            assert_eq!(context & 0x7f_fff0, (vpn2 & 0x7_ffff) << 4, "{address:x}");
            assert_eq!(x_context & 0x1_ffff_fff0, vpn2 << 4 | (address >> 62) << 31, "{address:x}");
        }
    }
}
