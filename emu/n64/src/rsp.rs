//! The RSP (the RCP's signal processor): its two memories, DMEM for data
//! and IMEM for its code (4 KB each, repeated through 0400_0000-0403_FFFF),
//! its registers and DMA, and its processor: a 32-bit MIPS scalar unit
//! without multiplies, 64-bit instructions, likely branches or exceptions,
//! and the vector unit (vu.rs) as its COP2. Its COP0 is the SP's and the
//! RDP's registers.
//!
//! It runs at two thirds of the CPU's clock, in bursts between the CPU's:
//! while it is not halted, an event runs its instructions up to the first
//! that reaches outside it once a slice of its cycles has passed (or for
//! a burst's length at most), and the next comes when the CPU has caught
//! up with the time it stopped at. What it does within itself (its
//! memories, registers and vector unit) may run ahead of the CPU, which
//! leaves SP memory alone while a task runs; what reaches outside (BREAK,
//! the SP's status, DMA and semaphore, the RDP's registers) comes at most
//! a slice ahead of the CPU, so it sees the CPU's writes and the CPU sees
//! it as with slices of that length. Its
//! instructions take a cycle each, but for what its pipeline adds and saves
//! (n64brew, RSP pipeline): a branch's delay slot is followed by a cycle
//! of nothing; a load, or a move from COP0 or COP2, gives its register
//! three cycles later, and an instruction that reads it sooner waits; and a
//! vector instruction next to a scalar one runs in the same cycle. That
//! the vector unit's registers too take a while to be written, and that a
//! pair does not issue when both use the same one, is left out. Boot code
//! counts on it: CIC 6105's has the RSP wait for the CPU in a loop that
//! gives up after so many turns, which the pipeline makes long enough.
//!
//! On ARM, jit/rsp.rs runs most of its code compiled, timed the same way;
//! this interpreter stays the reference it is checked against.

use crate::vu::Vu;
use crate::{N64, RSP, SP_INTR};
use savestate::state;

// SP_STATUS, as read.
const HALTED: u32 = 1 << 0;
const BROKE: u32 = 1 << 1;
const INTBREAK: u32 = 1 << 6;

/// RSP cycles a burst runs before it stops at an instruction that reaches
/// outside the RSP (`outside`), and at most.
const SLICE: u64 = 256;
const BURST: u64 = 1 << 14;

#[derive(Clone)]
pub struct Rsp {
    /// Host execution preference, excluded from the emulated state. The
    /// fixed-size path is ordinary Rust and is safe without SIMD support.
    pub(crate) fast_memory: bool,
    pub(crate) profiling: bool,
    pub(crate) fast_memory_count: u64,
    /// Host state, for the JIT to notice new code: writes to IMEM so far,
    /// and the count as of the last write to each 256-byte page of it.
    pub(crate) imem_writes: u32,
    pub(crate) imem_pages: [u32; 16],
    /// Host state, for profiling: the kind of task the RSP was started on
    /// (OSTask's type in DMEM at 0xFC0, as libultra leaves it: 1 graphics,
    /// 2 sound, 3 video, 4 JPEG; 0 for anything else), and its cycles by it.
    pub(crate) task: usize,
    pub(crate) task_cycles: [u64; 5],
    /// DMEM then IMEM, big-endian.
    pub memory: Vec<u8>,
    /// SP_STATUS's halted, broke, single-step and interrupt-on-break bits
    /// and the eight signals (bits 7-14).
    pub(crate) status: u32,
    /// The DMA registers: SP memory address (bit 12 picks IMEM), RDRAM
    /// address, and the length of the last transfer as it reads back.
    pub(crate) mem_addr: u32,
    pub(crate) dram_addr: u32,
    pub(crate) length: u32,
    pub(crate) semaphore: bool,
    /// The instruction to run next (in IMEM), the one after it (a branch's
    /// target), and the scalar registers.
    pub(crate) pc: u32,
    pub(crate) next_pc: u32,
    pub(crate) gpr: [u32; 32],
    pub vu: Vu,
    /// The pipeline: the RSP's cycles so far, the cycle each scalar
    /// register is ready on, and what the last instruction was: whether the
    /// next can issue with it, whether it was a vector instruction, and
    /// whether it was a branch (BRANCH) or a branch's delay slot (SLOT).
    pub(crate) clock: u64,
    pub(crate) ready: [u64; 32],
    pub(crate) pairs: bool,
    pub(crate) vector: bool,
    pub(crate) after: u8,
    /// Host state, for the burst running: the low word of the clock from
    /// which it stops before an instruction that reaches outside, for
    /// compiled code to compare.
    pub(crate) slice_end: u32,
}

state!(Rsp { memory, status, mem_addr, dram_addr, length, semaphore, pc, next_pc, gpr, vu, clock, ready, pairs, vector,
    after });

// What the last instruction was, for the pipeline.
pub(crate) const BRANCH: u8 = 1;
pub(crate) const SLOT: u8 = 2;

impl Rsp {
    pub fn new() -> Rsp {
        Rsp {
            fast_memory: true,
            profiling: false,
            fast_memory_count: 0,
            imem_writes: 0,
            imem_pages: [0; 16],
            task: 0,
            task_cycles: [0; 5],
            memory: vec![0; 0x2000],
            status: HALTED,
            mem_addr: 0,
            dram_addr: 0,
            length: 0,
            semaphore: false,
            pc: 0,
            next_pc: 4,
            gpr: [0; 32],
            vu: Vu::new(),
            clock: 0,
            ready: [0; 32],
            pairs: false,
            vector: false,
            after: 0,
            slice_end: 0,
        }
    }

    pub fn read_memory(&self, addr: u32) -> u32 {
        let i = addr as usize & 0x1ffc;
        u32::from_be_bytes(self.memory[i..i + 4].try_into().unwrap())
    }

    pub fn write_memory(&mut self, addr: u32, value: u32, mask: u32) {
        let i = addr as usize & 0x1ffc;
        if i & 0x1000 != 0 {
            self.imem_writes = self.imem_writes.wrapping_add(1);
            self.imem_pages[i >> 8 & 15] = self.imem_writes;
        }
        let old = u32::from_be_bytes(self.memory[i..i + 4].try_into().unwrap());
        self.memory[i..i + 4].copy_from_slice(&(old & !mask | value & mask).to_be_bytes());
    }

    pub fn halted(&self) -> bool {
        self.status & HALTED != 0
    }

    // DMEM, as the processor reaches it: by the low 12 bits, any alignment.
    fn dmem(&self, addr: u32) -> u8 {
        self.memory[addr as usize & 0xfff]
    }

    fn set_dmem(&mut self, addr: u32, value: u8) {
        self.memory[addr as usize & 0xfff] = value;
    }

    fn dmem16(&self, addr: u32) -> u16 {
        u16::from(self.dmem(addr)) << 8 | u16::from(self.dmem(addr.wrapping_add(1)))
    }

    fn dmem32(&self, addr: u32) -> u32 {
        u32::from(self.dmem16(addr)) << 16 | u32::from(self.dmem16(addr.wrapping_add(2)))
    }

    fn branch(&mut self, taken: bool, target: u32) {
        if taken {
            self.next_pc = target & 0xffc;
        }
    }

    /// Moves the clock on by the cycles an instruction takes, as the
    /// pipeline has it.
    fn time(&mut self, word: u32, pc: u32) {
        let p = pipe(word);
        let slot = self.after == BRANCH;
        let paired = self.pairs && !slot && p.vector != self.vector;
        let issue = if paired { self.clock - 1 } else { self.clock }.max(self.ready[p.reads[0]]).max(self.ready[p.reads[1]]);
        self.clock = issue + 1;
        if p.loads != 0 {
            self.ready[p.loads] = issue + 3;
        }
        // A delay slot pairs with nothing, and nor does a branch's target
        // that is not 8-byte aligned.
        self.pairs = !paired && !p.branch && !slot && !(self.after == SLOT && pc & 4 != 0);
        self.vector = p.vector;
        if slot {
            self.clock += 1;
        }
        self.after = if p.branch { BRANCH } else if slot { SLOT } else { 0 };
    }
}

/// Whether an instruction reaches outside the RSP, so that a burst stops
/// before it once its slice has passed: BREAK; a move from the SP's status
/// or semaphore or from the RDP's start, end, current or status; a move to
/// the SP's DMA lengths (which start a DMA), status or semaphore, or to the
/// RDP's start, end or status. The DMA addresses, and the lengths as they
/// read back, change only with the RSP's own DMAs while it runs, and the
/// other registers read as 0 and take nothing. The interpreter and the JIT
/// stop at the same instructions.
pub(crate) fn outside(word: u32) -> bool {
    let reg = word >> 11 & 15;
    match word >> 26 {
        0x00 => word & 63 == 0x0d,
        0x10 if word >> 21 & 31 == 0 => matches!(reg, 4 | 7..=11),
        0x10 if word >> 21 & 31 == 4 => matches!(reg, 2..=4 | 7..=9 | 11),
        _ => false,
    }
}

/// What the pipeline needs of an instruction: the scalar registers it
/// reads (0 for none), the one a load or a move from COP0 or COP2 gives
/// three cycles later (0 for none), and whether it is a vector instruction
/// or a branch. The interpreter and the JIT time instructions by it alike.
#[derive(Clone, Copy)]
pub(crate) struct Pipe {
    pub reads: [usize; 2],
    pub loads: usize,
    pub vector: bool,
    pub branch: bool,
}

pub(crate) fn pipe(word: u32) -> Pipe {
    let op = word >> 26;
    let (rs, rt) = ((word >> 21 & 31) as usize, (word >> 16 & 31) as usize);
    let vector = op == 0x12 && word & 1 << 25 != 0;
    let moves = matches!(op, 0x10 | 0x12) && !vector;
    let reads = match op {
        0x00 | 0x04 | 0x05 | 0x28..=0x2f => [rs, rt],
        0x01 | 0x06..=0x0f | 0x20..=0x27 | 0x32 | 0x3a => [rs, 0],
        // MTC0, MTC2, CTC2.
        _ if moves && rs & 4 != 0 => [rt, 0],
        _ => [0, 0],
    };
    // Loads, MFC0, MFC2 and CFC2.
    let loads = if matches!(op, 0x20..=0x27) || moves && rs & 4 == 0 { rt } else { 0 };
    let branch = matches!(op, 0x01..=0x07) || op == 0 && matches!(word & 63, 0x08 | 0x09);
    Pipe { reads, loads, vector, branch }
}

impl N64 {
    /// Reads one of the SP's registers (0-7, by number): from the CPU, or
    /// from the RSP through its COP0.
    fn sp_register(&mut self, reg: u32) -> u32 {
        let rsp = &mut self.rsp;
        match reg & 7 {
            0 => rsp.mem_addr,
            1 => rsp.dram_addr,
            2 | 3 => rsp.length,
            4 => rsp.status,
            // DMA_FULL and DMA_BUSY: DMAs are over as soon as they start.
            5 | 6 => 0,
            _ => {
                // Reading takes the semaphore.
                let taken = rsp.semaphore;
                rsp.semaphore = true;
                u32::from(taken)
            }
        }
    }

    pub(crate) fn set_sp_register(&mut self, reg: u32, value: u32) {
        match reg & 7 {
            0 => self.rsp.mem_addr = value & 0x1ff8,
            1 => self.rsp.dram_addr = value & 0xff_fff8,
            2 => self.sp_dma(value, false),
            3 => self.sp_dma(value, true),
            4 => self.write_sp_status(value),
            7 => self.rsp.semaphore = false,
            _ => {}
        }
    }

    pub(crate) fn read_sp_register(&mut self, addr: u32) -> u32 {
        if addr >= 0x0408_0000 {
            return self.rsp.pc;
        }
        self.sp_register(addr >> 2)
    }

    pub(crate) fn write_sp_register(&mut self, addr: u32, value: u32) {
        if addr >= 0x0408_0000 {
            self.rsp.pc = value & 0xffc;
            self.rsp.next_pc = (self.rsp.pc + 4) & 0xffc;
            return;
        }
        self.set_sp_register(addr >> 2, value);
    }

    /// SP_STATUS takes pairs of bits that clear and set each flag (both
    /// together change nothing).
    fn write_sp_status(&mut self, value: u32) {
        let was_halted = self.rsp.halted();
        let mut status = self.rsp.status;
        let mut flag = |clear: u32, set: u32, bit: u32| {
            if value & clear != 0 && value & set == 0 {
                status &= !bit;
            }
            if value & set != 0 && value & clear == 0 {
                status |= bit;
            }
        };
        flag(1 << 0, 1 << 1, HALTED);
        flag(1 << 2, 0, BROKE);
        flag(1 << 5, 1 << 6, 1 << 5);
        flag(1 << 7, 1 << 8, INTBREAK);
        for signal in 0..8 {
            flag(1 << (9 + 2 * signal), 1 << (10 + 2 * signal), 1 << (7 + signal));
        }
        self.rsp.status = status;
        if value & 1 << 3 != 0 && value & 1 << 4 == 0 {
            self.acknowledge(SP_INTR);
        }
        if value & 1 << 4 != 0 && value & 1 << 3 == 0 {
            self.request(SP_INTR);
        }
        if !was_halted && self.rsp.halted() {
            self.rsp_stopped();
        }
        if was_halted && !self.rsp.halted() {
            let kind = self.rsp.read_memory(0xfc0) as usize;
            self.rsp.task = if kind < 5 { kind } else { 0 };
            if kind == 1 && self.gfx_task_started() || kind == 2 && self.audio_task_started() {
                return;
            }
        }
        if was_halted && !self.rsp.halted() && self.events[RSP] == u64::MAX {
            let at = self.cycles + 1;
            self.schedule(RSP, at);
        }
    }

    /// A DMA between RDRAM and SP memory: rows of a length (rounded up to
    /// 8 bytes), RDRAM skipping a gap after each. It is done at once.
    fn sp_dma(&mut self, value: u32, to_rdram: bool) {
        let length = (value & 0xff8) + 8;
        let rows = (value >> 12 & 0xff) + 1;
        let skip = value >> 20 & 0xff8;
        let reach = (self.rsp.dram_addr as usize, (rows * (length + skip)) as usize);
        if to_rdram {
            self.wrote_rdram(reach.0, reach.1);
        } else {
            self.touch_rdram(reach.0, reach.1);
        }
        let rsp = &mut self.rsp;
        let bank = rsp.mem_addr & 0x1000;
        let code = bank != 0 && !to_rdram;
        if code {
            rsp.imem_writes = rsp.imem_writes.wrapping_add(1);
        }
        let (mut mem, mut dram) = (rsp.mem_addr & 0xff8, rsp.dram_addr);
        for _ in 0..rows {
            for i in (0..length).step_by(8) {
                let m = (bank | (mem + i) & 0xff8) as usize;
                let d = (dram + i) as usize & 0xff_fff8;
                if d + 8 > self.rdram.len() {
                    continue;
                }
                if to_rdram {
                    self.rdram[d..d + 8].copy_from_slice(&rsp.memory[m..m + 8]);
                } else {
                    rsp.memory[m..m + 8].copy_from_slice(&self.rdram[d..d + 8]);
                    if code {
                        rsp.imem_pages[m >> 8 & 15] = rsp.imem_writes;
                    }
                }
            }
            mem = (mem + length) & 0xff8;
            dram = (dram + length + skip) & 0xff_fff8;
        }
        rsp.mem_addr = bank | mem;
        rsp.dram_addr = dram;
        rsp.length = skip << 20 | 0xff8;
    }

    /// Runs a burst of the RSP's cycles, and plans the next while it is
    /// not halted.
    pub(crate) fn rsp_event(&mut self) {
        if self.gfx_task_finished() {
            return;
        }
        let start = self.rsp.clock;
        self.rsp_slice();
        let task = self.rsp.task;
        self.rsp.task_cycles[task] += self.rsp.clock - start;
        if task == 2 && self.rsp.halted() {
            self.audio_task_ran();
        }
        if task == 1 && self.rsp.halted() {
            self.video_task_ran();
        }
    }

    /// A burst: the RSP's instructions up to the first that reaches outside
    /// once SLICE cycles have passed, or for BURST cycles. The next is due
    /// when the CPU has caught up, with the instruction it stopped before.
    pub(crate) fn rsp_slice(&mut self) {
        let start = self.rsp.clock;
        let (slice_end, end) = (start + SLICE, start + BURST);
        self.rsp.slice_end = slice_end as u32;
        loop {
            if self.rsp.halted() {
                return;
            }
            let clock = self.rsp.clock;
            if clock >= end || clock >= slice_end && outside(self.rsp.read_memory(0x1000 | self.rsp.pc)) {
                break;
            }
            #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
            if self.rsp_jit.enabled && self.run_rsp_block(end) {
                continue;
            }
            self.rsp_step();
        }
        // The CPU's cycles of the RSP's since `start`: three for every two,
        // counted from the RSP clock's zero so that the halves add up.
        let at = self.cycles + self.rsp.clock * 3 / 2 - start * 3 / 2;
        self.schedule(RSP, at);
    }

    pub(crate) fn rsp_step(&mut self) {
        let pc = self.rsp.pc;
        let i = 0x1000 + pc as usize;
        let word = u32::from_be_bytes(self.rsp.memory[i..i + 4].try_into().unwrap());
        self.rsp.time(word, pc);
        self.rsp.pc = self.rsp.next_pc;
        self.rsp.next_pc = (self.rsp.pc + 4) & 0xffc;
        self.rsp_execute(word, pc);
        self.rsp.gpr[0] = 0;
    }

    /// Runs one instruction, already timed, as rsp_step does: for the JIT,
    /// which times instructions itself and leaves some to this.
    #[cfg_attr(not(any(target_arch = "arm", target_arch = "aarch64")), allow(dead_code))]
    pub(crate) fn rsp_interpret(&mut self, word: u32, pc: u32) {
        self.rsp_execute(word, pc);
        self.rsp.gpr[0] = 0;
    }

    fn rsp_execute(&mut self, word: u32, pc: u32) {
        let rsp = &mut self.rsp;
        let (rs, rt, rd) = ((word >> 21 & 31) as usize, (word >> 16 & 31) as usize, (word >> 11 & 31) as usize);
        let (s, t) = (rsp.gpr[rs], rsp.gpr[rt]);
        let simm = word as i16 as i32 as u32;
        let uimm = word & 0xffff;
        let sa = word >> 6 & 31;
        // A branch's target, from the delay slot.
        let target = (pc + 4).wrapping_add(simm << 2);
        let link = (pc + 8) & 0xfff;
        match word >> 26 {
            0x00 => match word & 63 {
                0x00 => rsp.gpr[rd] = t << sa,
                0x02 => rsp.gpr[rd] = t >> sa,
                0x03 => rsp.gpr[rd] = (t as i32 >> sa) as u32,
                0x04 => rsp.gpr[rd] = t << (s & 31),
                0x06 => rsp.gpr[rd] = t >> (s & 31),
                0x07 => rsp.gpr[rd] = (t as i32 >> (s & 31)) as u32,
                0x08 => rsp.branch(true, s),
                0x09 => {
                    rsp.branch(true, s);
                    rsp.gpr[rd] = link;
                }
                0x0d => {
                    // BREAK: halts, and interrupts the CPU if asked to.
                    rsp.status |= HALTED | BROKE;
                    if rsp.status & INTBREAK != 0 {
                        self.request(SP_INTR);
                    }
                    self.rsp_stopped();
                }
                0x20 | 0x21 => rsp.gpr[rd] = s.wrapping_add(t),
                0x22 | 0x23 => rsp.gpr[rd] = s.wrapping_sub(t),
                0x24 => rsp.gpr[rd] = s & t,
                0x25 => rsp.gpr[rd] = s | t,
                0x26 => rsp.gpr[rd] = s ^ t,
                0x27 => rsp.gpr[rd] = !(s | t),
                0x2a => rsp.gpr[rd] = u32::from((s as i32) < (t as i32)),
                0x2b => rsp.gpr[rd] = u32::from(s < t),
                _ => {}
            },
            0x01 => {
                let taken = match rt & 0xf {
                    0 => (s as i32) < 0,
                    1 => (s as i32) >= 0,
                    _ => false,
                };
                rsp.branch(taken, target);
                // BLTZAL and BGEZAL link whether or not they branch.
                if rt & 0x10 != 0 {
                    rsp.gpr[31] = link;
                }
            }
            0x02 => rsp.branch(true, (word & 0x3ff) << 2),
            0x03 => {
                rsp.branch(true, (word & 0x3ff) << 2);
                rsp.gpr[31] = link;
            }
            0x04 => rsp.branch(s == t, target),
            0x05 => rsp.branch(s != t, target),
            0x06 => rsp.branch(s as i32 <= 0, target),
            0x07 => rsp.branch(s as i32 > 0, target),
            0x08 | 0x09 => rsp.gpr[rt] = s.wrapping_add(simm),
            0x0a => rsp.gpr[rt] = u32::from((s as i32) < simm as i32),
            0x0b => rsp.gpr[rt] = u32::from(s < simm),
            0x0c => rsp.gpr[rt] = s & uimm,
            0x0d => rsp.gpr[rt] = s | uimm,
            0x0e => rsp.gpr[rt] = s ^ uimm,
            0x0f => rsp.gpr[rt] = uimm << 16,
            0x10 => match rs {
                // MFC0, MTC0: the SP's registers, then the RDP's.
                0x00 => {
                    let reg = rd as u32;
                    let value = if reg & 8 == 0 { self.sp_register(reg) } else { self.read_dp((reg & 7) << 2) };
                    self.rsp.gpr[rt] = value;
                }
                0x04 => {
                    if rd & 8 == 0 {
                        self.set_sp_register(rd as u32, t);
                    } else {
                        self.hle.from_rsp = true;
                        self.write_dp((rd as u32 & 7) << 2, t);
                        self.hle.from_rsp = false;
                    }
                }
                _ => {}
            },
            0x12 => {
                let e = (word >> 7 & 15) as usize;
                let vu = &mut rsp.vu;
                if word & 1 << 25 != 0 {
                    vu.execute(word);
                    return;
                }
                match rs {
                    // MFC2, CFC2, MTC2, CTC2.
                    0x00 => {
                        let value = u16::from(vu.byte(rd, e)) << 8 | u16::from(vu.byte(rd, (e + 1) & 15));
                        rsp.gpr[rt] = value as i16 as i32 as u32;
                    }
                    0x02 => rsp.gpr[rt] = vu.control(rd),
                    0x04 => {
                        vu.set_byte(rd, e, (t >> 8) as u8);
                        if e != 15 {
                            vu.set_byte(rd, e + 1, t as u8);
                        }
                    }
                    0x06 => vu.set_control(rd, t),
                    _ => {}
                }
            }
            0x20 => rsp.gpr[rt] = rsp.dmem(s.wrapping_add(simm)) as i8 as i32 as u32,
            0x21 => rsp.gpr[rt] = rsp.dmem16(s.wrapping_add(simm)) as i16 as i32 as u32,
            0x23 | 0x27 => rsp.gpr[rt] = rsp.dmem32(s.wrapping_add(simm)),
            0x24 => rsp.gpr[rt] = u32::from(rsp.dmem(s.wrapping_add(simm))),
            0x25 => rsp.gpr[rt] = u32::from(rsp.dmem16(s.wrapping_add(simm))),
            0x28 => rsp.set_dmem(s.wrapping_add(simm), t as u8),
            0x29 => {
                let addr = s.wrapping_add(simm);
                rsp.set_dmem(addr, (t >> 8) as u8);
                rsp.set_dmem(addr.wrapping_add(1), t as u8);
            }
            0x2b => {
                let addr = s.wrapping_add(simm);
                for (i, byte) in t.to_be_bytes().into_iter().enumerate() {
                    rsp.set_dmem(addr.wrapping_add(i as u32), byte);
                }
            }
            0x32 => rsp.load_vector(word, s),
            0x3a => rsp.store_vector(word, s),
            _ => {}
        }
    }
}

impl Rsp {
    #[inline]
    pub(crate) fn load_vector(&mut self, word: u32, base: u32) {
        if self.fast_memory && self.load_vector_fast(word, base) {
            if self.profiling {
                self.fast_memory_count += 1;
            }
            return;
        }
        self.load_vector_reference(word, base);
    }

    /// Full contiguous quads and either contiguous half of a vector. The
    /// other element, partial-line and DMEM-wrap cases keep the byte loop.
    #[inline]
    fn load_vector_fast(&mut self, word: u32, base: u32) -> bool {
        let op = word >> 11 & 31;
        let e = (word >> 7 & 15) as usize;
        let size = match (op, e) {
            (4, 0) => 16,
            (3, 0 | 8) => 8,
            _ => return false,
        };
        let offset = ((word as i32) << 25 >> 25) as u32;
        let at = base.wrapping_add(offset.wrapping_mul(size)) as usize & 0xfff;
        if at > 0x1000 - size as usize || size == 16 && at & 15 != 0 {
            return false;
        }
        let vt = (word >> 16 & 31) as usize;
        if size == 16 {
            let bytes: &[u8; 16] = self.memory[at..at + 16].try_into().unwrap();
            self.vu.regs[vt] = std::array::from_fn(|n| u16::from_be_bytes([bytes[2 * n], bytes[2 * n + 1]]));
        } else {
            let bytes: &[u8; 8] = self.memory[at..at + 8].try_into().unwrap();
            let lanes: [u16; 4] = std::array::from_fn(|n| u16::from_be_bytes([bytes[2 * n], bytes[2 * n + 1]]));
            self.vu.regs[vt][e / 2..e / 2 + 4].copy_from_slice(&lanes);
        }
        true
    }

    #[inline]
    pub(crate) fn store_vector(&mut self, word: u32, base: u32) {
        if self.fast_memory && self.store_vector_fast(word, base) {
            if self.profiling {
                self.fast_memory_count += 1;
            }
            return;
        }
        self.store_vector_reference(word, base);
    }

    #[inline]
    fn store_vector_fast(&mut self, word: u32, base: u32) -> bool {
        let op = word >> 11 & 31;
        let e = (word >> 7 & 15) as usize;
        let size = match (op, e) {
            (4, 0) => 16,
            (3, 0 | 8) => 8,
            _ => return false,
        };
        let offset = ((word as i32) << 25 >> 25) as u32;
        let at = base.wrapping_add(offset.wrapping_mul(size)) as usize & 0xfff;
        if at > 0x1000 - size as usize || size == 16 && at & 15 != 0 {
            return false;
        }
        let vt = (word >> 16 & 31) as usize;
        if size == 16 {
            let lanes = self.vu.regs[vt];
            let bytes: [u8; 16] = std::array::from_fn(|n| lanes[n / 2].to_be_bytes()[n & 1]);
            self.memory[at..at + 16].copy_from_slice(&bytes);
        } else {
            let lanes = &self.vu.regs[vt][e / 2..e / 2 + 4];
            let bytes: [u8; 8] = std::array::from_fn(|n| lanes[n / 2].to_be_bytes()[n & 1]);
            self.memory[at..at + 8].copy_from_slice(&bytes);
        }
        true
    }

    /// LWC2: the vector loads, by the field in bits 11-15. Each moves bytes
    /// between DMEM and a register's bytes from the element field on, as
    /// ares has them.
    fn load_vector_reference(&mut self, word: u32, base: u32) {
        let vt = (word >> 16 & 31) as usize;
        let e = (word >> 7 & 15) as usize;
        let offset = ((word as i32) << 25 >> 25) as u32;
        let scaled = |size: u32| base.wrapping_add(offset.wrapping_mul(size));
        match word >> 11 & 31 {
            // LBV, LSV, LLV, LDV: 1, 2, 4 or 8 bytes.
            op @ 0..=3 => {
                let size = 1 << op;
                let mut addr = scaled(size);
                for i in e..(e + size as usize).min(16) {
                    let byte = self.dmem(addr);
                    self.vu.set_byte(vt, i, byte);
                    addr = addr.wrapping_add(1);
                }
            }
            // LQV: up to the end of the 16-byte line.
            4 => {
                let mut addr = scaled(16);
                let end = (16 + e).saturating_sub(addr as usize & 15).min(16);
                for i in e..end {
                    let byte = self.dmem(addr);
                    self.vu.set_byte(vt, i, byte);
                    addr = addr.wrapping_add(1);
                }
            }
            // LRV: the line's start up to the address, into the end.
            5 => {
                let addr = scaled(16);
                let start = 16 - ((addr & 15) as i32 - e as i32);
                let mut from = addr & !15;
                for i in start.max(0)..16 {
                    let byte = self.dmem(from);
                    self.vu.set_byte(vt, i as usize, byte);
                    from = from.wrapping_add(1);
                }
            }
            // LPV, LUV, LHV: bytes into the top of each lane (LUV and LHV
            // as unsigned 7-bit fractions; LHV every other byte).
            op @ 6..=8 => {
                let addr = if op == 8 { scaled(16) } else { scaled(8) };
                let index = (addr & 7).wrapping_sub(e as u32);
                let line = addr & !7;
                let (shift, step) = match op {
                    6 => (8, 1),
                    7 => (7, 1),
                    _ => (7, 2),
                };
                for n in 0..8u32 {
                    let byte = self.dmem(line.wrapping_add(index.wrapping_add(n * step) & 15));
                    self.vu.regs[vt][n as usize] = u16::from(byte) << shift;
                }
            }
            // LFV: every fourth byte into four lanes, from the element on.
            9 => {
                let addr = scaled(16);
                let index = (addr & 7).wrapping_sub(e as u32);
                let line = addr & !7;
                let mut lanes = [0u16; 8];
                for n in 0..4u32 {
                    lanes[n as usize] = u16::from(self.dmem(line.wrapping_add(index.wrapping_add(n * 4) & 15))) << 7;
                    lanes[n as usize + 4] = u16::from(self.dmem(line.wrapping_add(index.wrapping_add(n * 4 + 8) & 15))) << 7;
                }
                for i in e..(e + 8).min(16) {
                    let byte = if i & 1 == 0 { (lanes[i >> 1] >> 8) as u8 } else { lanes[i >> 1] as u8 };
                    self.vu.set_byte(vt, i, byte);
                }
            }
            // LTV: a transposed load, a lane into each of eight registers.
            11 => {
                let addr = scaled(16);
                let begin = addr & !7;
                let mut from = begin.wrapping_add((e as u32 + (addr & 8)) & 15);
                let (group, mut lane) = (vt & !7, e >> 1);
                for n in 0..8 {
                    for half in 0..2 {
                        let byte = self.dmem(from);
                        self.vu.set_byte(group + lane, n * 2 + half, byte);
                        from = from.wrapping_add(1);
                        if from == begin.wrapping_add(16) {
                            from = begin;
                        }
                    }
                    lane = (lane + 1) & 7;
                }
            }
            // LWV is not on the N64's RSP.
            _ => {}
        }
    }

    /// SWC2: the vector stores.
    fn store_vector_reference(&mut self, word: u32, base: u32) {
        let vt = (word >> 16 & 31) as usize;
        let e = (word >> 7 & 15) as usize;
        let offset = ((word as i32) << 25 >> 25) as u32;
        let scaled = |size: u32| base.wrapping_add(offset.wrapping_mul(size));
        let lane = |vu: &Vu, n: usize| vu.regs[vt][n & 7];
        match word >> 11 & 31 {
            // SBV, SSV, SLV, SDV.
            op @ 0..=3 => {
                let size = 1 << op;
                let mut addr = scaled(size);
                for i in e..e + size as usize {
                    let byte = self.vu.byte(vt, i & 15);
                    self.set_dmem(addr, byte);
                    addr = addr.wrapping_add(1);
                }
            }
            // SQV: up to the end of the 16-byte line.
            4 => {
                let mut addr = scaled(16);
                for i in e..e + (16 - (addr as usize & 15)) {
                    let byte = self.vu.byte(vt, i & 15);
                    self.set_dmem(addr, byte);
                    addr = addr.wrapping_add(1);
                }
            }
            // SRV: the end of the register into the line's start.
            5 => {
                let addr = scaled(16);
                let rotate = 16 - (addr as usize & 15);
                let mut to = addr & !15;
                for i in e..e + (addr as usize & 15) {
                    let byte = self.vu.byte(vt, (i + rotate) & 15);
                    self.set_dmem(to, byte);
                    to = to.wrapping_add(1);
                }
            }
            // SPV, SUV: the tops of lanes as bytes (SUV as 7-bit
            // fractions), the other half of the 16 the other way.
            op @ 6 | op @ 7 => {
                let mut addr = scaled(8);
                for i in e..e + 8 {
                    let packed = ((i & 15) < 8) == (op == 6);
                    let byte = if packed { self.vu.byte(vt, (i & 7) << 1) } else { (lane(&self.vu, i) >> 7) as u8 };
                    self.set_dmem(addr, byte);
                    addr = addr.wrapping_add(1);
                }
            }
            // SHV: each lane's 7-bit fraction, every other byte.
            8 => {
                let addr = scaled(16);
                let (index, line) = (addr & 7, addr & !7);
                for n in 0..8 {
                    let at = e + n * 2;
                    let value = self.vu.byte(vt, at & 15) << 1 | self.vu.byte(vt, (at + 1) & 15) >> 7;
                    self.set_dmem(line.wrapping_add((index.wrapping_add(n as u32 * 2)) & 15), value);
                }
            }
            // SFV: four lanes' fractions, every fourth byte; which four by
            // the element.
            9 => {
                let addr = scaled(16);
                let (index, line) = (addr & 7, addr & !7);
                let lanes: Option<[usize; 4]> = match e {
                    0 | 15 => Some([0, 1, 2, 3]),
                    1 => Some([6, 7, 4, 5]),
                    4 => Some([1, 2, 3, 0]),
                    5 => Some([7, 4, 5, 6]),
                    8 => Some([4, 5, 6, 7]),
                    11 => Some([3, 0, 1, 2]),
                    12 => Some([5, 6, 7, 4]),
                    _ => None,
                };
                for n in 0..4 {
                    let byte = lanes.map_or(0, |l| (lane(&self.vu, l[n]) >> 7) as u8);
                    self.set_dmem(line.wrapping_add((index.wrapping_add(n as u32 * 4)) & 15), byte);
                }
            }
            // SWV: the whole register, rotated, wrapping in the line.
            10 => {
                let addr = scaled(16);
                let (mut index, line) = (addr & 7, addr & !7);
                for i in e..e + 16 {
                    let byte = self.vu.byte(vt, i & 15);
                    self.set_dmem(line.wrapping_add(index & 15), byte);
                    index += 1;
                }
            }
            // STV: a transposed store, a lane from each of eight registers.
            11 => {
                let addr = scaled(16);
                let element = e & !1;
                let mut from = 16 - element;
                let mut index = (addr & 7).wrapping_sub(element as u32);
                let line = addr & !7;
                for reg in vt & !7..(vt & !7) + 8 {
                    for _ in 0..2 {
                        let byte = self.vu.byte(reg, from & 15);
                        self.set_dmem(line.wrapping_add(index & 15), byte);
                        index = index.wrapping_add(1);
                        from += 1;
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod memory_tests {
    use super::*;
    use savestate::State;

    fn random(seed: &mut u64) -> u64 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        *seed
    }

    fn fixture() -> Rsp {
        let mut rsp = Rsp::new();
        let mut seed = 0x6420_d0e1_1234_5678;
        for bytes in rsp.memory.chunks_exact_mut(8) {
            bytes.copy_from_slice(&random(&mut seed).to_be_bytes());
        }
        for reg in &mut rsp.vu.regs {
            for lane in reg {
                *lane = random(&mut seed) as u16;
            }
        }
        for reg in &mut rsp.gpr {
            *reg = random(&mut seed) as u32;
        }
        rsp.status = 0x5a5;
        rsp.mem_addr = 0x1238;
        rsp.dram_addr = 0x7f_fff8;
        rsp.length = 0x1234_5ff8;
        rsp.semaphore = true;
        rsp.pc = 0xabc;
        rsp.next_pc = 0x1c0;
        rsp.clock = 0x1234_5678;
        rsp.ready = std::array::from_fn(|n| 0x1234_5670 + n as u64);
        rsp.pairs = true;
        rsp.vector = true;
        rsp.after = SLOT;
        for reg in 0..3 {
            rsp.vu.set_control(reg, random(&mut seed) as u32);
        }
        rsp
    }

    fn save(rsp: &Rsp, out: &mut Vec<u8>) {
        out.clear();
        rsp.save(out);
    }

    fn same(a: &Rsp, b: &Rsp, left: &mut Vec<u8>, right: &mut Vec<u8>, word: u32, base: u32) {
        save(a, left);
        save(b, right);
        if left != right {
            let at = left.iter().zip(right.iter()).position(|(a, b)| a != b).unwrap();
            panic!("word {word:08x}, base {base:08x}: state byte {at:x}: fast {:02x}, reference {:02x}", left[at], right[at]);
        }
    }

    #[test]
    fn all_vector_memory_forms_match_full_reference_state() {
        let initial = fixture();
        let mut seed = 0xa003_1f08_6420_1234;
        let (mut a, mut b) = (Vec::new(), Vec::new());
        // Every opcode, element and signed seven-bit offset, every vector
        // register and DMEM alignment, including high aliases and wrapping.
        for op in 0..32u32 {
            let scale = match op { 0 => 1, 1 => 2, 2 => 4, 3 | 6 | 7 => 8, _ => 16 };
            for element in 0..16u32 {
                for offset in 0..128u32 {
                    let low = offset & 15;
                    let addr = match offset >> 4 {
                        0 => low,
                        1 => 0xff0 + low,
                        2 => 0x1000 + low,
                        3 => u32::MAX - low,
                        4 => random(&mut seed) as u32 & !15 | low,
                        5 => 0x7fff_fff0 + low,
                        6 => 0x0400_0000 | low,
                        _ => 0xffff_0ff0 | low,
                    };
                    let signed = ((offset as i32) << 25 >> 25) as u32;
                    let base = addr.wrapping_sub(signed.wrapping_mul(scale));
                    let vt = (offset + op * 3 + element) & 31;
                    let fields = 1 << 21 | vt << 16 | op << 11 | element << 7 | offset;
                    for store in [false, true] {
                        let mut fast = initial.clone();
                        let mut reference = initial.clone();
                        reference.fast_memory = false;
                        let word = if store { 0x3a << 26 } else { 0x32 << 26 } | fields;
                        if store {
                            fast.store_vector(word, base);
                            reference.store_vector(word, base);
                        } else {
                            fast.load_vector(word, base);
                            reference.load_vector(word, base);
                        }
                        same(&fast, &reference, &mut a, &mut b, word, base);
                    }
                }
            }
        }
    }

    #[test]
    fn contiguous_paths_accept_only_complete_nonwrapping_transfers() {
        let initial = fixture();
        let (mut a, mut b) = (Vec::new(), Vec::new());
        for op in 0..32u32 {
            for element in 0..16u32 {
                for addr in [0, 1, 7, 8, 15, 16, 0xff0, 0xff8, 0xff9, 0xfff, 0x1000, u32::MAX] {
                    let word = 7 << 16 | op << 11 | element << 7;
                    let size = match (op, element) { (4, 0) => 16, (3, 0 | 8) => 8, _ => 0 };
                    let at = addr as usize & 0xfff;
                    let expected = size != 0 && at <= 0x1000 - size && (size != 16 || at & 15 == 0);
                    for store in [false, true] {
                        let mut fast = initial.clone();
                        let mut reference = initial.clone();
                        let taken = if store {
                            fast.store_vector_fast(word, addr)
                        } else {
                            fast.load_vector_fast(word, addr)
                        };
                        assert_eq!(taken, expected, "op {op}, e {element}, addr {addr:x}, store {store}");
                        if expected {
                            if store {
                                reference.store_vector_reference(word, addr);
                            } else {
                                reference.load_vector_reference(word, addr);
                            }
                        }
                        same(&fast, &reference, &mut a, &mut b, word, addr);
                    }
                }
            }
        }
    }

    #[test]
    fn memory_preference_does_not_change_or_load_from_save_state() {
        let mut rsp = fixture();
        assert!(rsp.fast_memory);
        let (mut enabled, mut disabled) = (Vec::new(), Vec::new());
        save(&rsp, &mut enabled);
        rsp.fast_memory = false;
        save(&rsp, &mut disabled);
        assert_eq!(enabled, disabled);
        rsp.load(&mut enabled.as_slice()).unwrap();
        assert!(!rsp.fast_memory);
    }

    #[test]
    fn memory_mode_and_counters_survive_machine_restore() {
        let mut rom = vec![0; 0x1000];
        rom[..4].copy_from_slice(&0x8037_1240u32.to_be_bytes());
        let mut fast = N64::new(&rom).unwrap();
        fast.rsp = fixture();
        fast.set_rsp_profiling(true);
        let initial = fast.save_state();
        let mut reference = fast.clone();
        reference.set_rsp_acceleration(false);
        let transfer = |machine: &mut N64| {
            machine.rsp.load_vector(0xc801_2000, 0x100); // LQV v1[0]
            machine.rsp.store_vector(0xe801_2000, 0x200); // SQV v1[0]
        };
        for machine in [&mut fast, &mut reference] {
            transfer(machine);
        }
        assert_eq!(fast.rsp_stats().fast_vector_memory_instructions, 2);
        assert_eq!(reference.rsp_stats().fast_vector_memory_instructions, 0);
        assert!(fast.save_state() == reference.save_state());

        fast.set_rsp_memory_acceleration(false);
        fast.load_state(&initial).unwrap();
        reference.load_state(&initial).unwrap();
        assert!(!fast.rsp.fast_memory);
        assert_eq!(fast.rsp_stats().fast_vector_memory_instructions, 0);
        transfer(&mut fast);
        transfer(&mut reference);
        assert_eq!(fast.rsp_stats().fast_vector_memory_instructions, 0);
        assert!(fast.save_state() == reference.save_state());

        fast.set_rsp_memory_acceleration(true);
        fast.load_state(&initial).unwrap();
        reference.load_state(&initial).unwrap();
        assert!(fast.rsp.fast_memory);
        assert!(!reference.rsp.fast_memory);
        transfer(&mut fast);
        transfer(&mut reference);
        assert_eq!(fast.rsp_stats().fast_vector_memory_instructions, 2);
        assert!(fast.save_state() == reference.save_state());
        let state = fast.save_state();
        assert!(fast.load_state(&state[..state.len() - 1]).is_err());
        assert_eq!(fast.rsp_stats().fast_vector_memory_instructions, 2);
        assert!(fast.save_state() == state);
        fast.set_rsp_profiling(false);
        transfer(&mut fast);
        assert_eq!(fast.rsp_stats().fast_vector_memory_instructions, 0);
    }

    #[test]
    #[ignore = "instruction-count microbenchmark; RSP_MEMORY_BENCH_FAST=0 selects byte loops"]
    fn vector_memory_benchmark() {
        use std::hint::black_box;
        let mut rsp = fixture();
        rsp.fast_memory = std::env::var("RSP_MEMORY_BENCH_FAST").as_deref() != Ok("0");
        let iterations = std::env::var("RSP_MEMORY_BENCH_ITERATIONS").ok()
            .map(|value| value.parse::<u32>().unwrap()).unwrap_or(100_000);
        let half = std::env::var("RSP_MEMORY_BENCH_KIND").as_deref() == Ok("half");
        let started = std::time::Instant::now();
        for n in 0..iterations {
            let (op, e, misalign) = if half { (3, (n & 1) * 8, n & 7) } else { (4, 0, 0) };
            let fields = (n & 31) << 16 | op << 11 | e << 7;
            let source = 0x100 + (n & 31) * 16 + misalign;
            let target = 0x800 + (n & 31) * 16 + misalign;
            rsp.load_vector(black_box(0x32 << 26 | fields), black_box(source));
            rsp.store_vector(black_box(0x3a << 26 | fields), black_box(target));
            black_box(&mut rsp);
        }
        let elapsed = started.elapsed();
        let mut bytes = Vec::new();
        rsp.save(&mut bytes);
        let hash = bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3));
        eprintln!("{iterations} vector load/store pairs, half={half}, fast={}, hash={hash:016x}, elapsed={elapsed:?}", rsp.fast_memory);
    }
}
