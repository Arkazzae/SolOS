//! The R3000A: a MIPS I CPU without an FPU. Its quirks the games see are
//! here: the instruction after a branch runs before it lands, a load's
//! value reaches its register an instruction late, and exceptions go
//! through coprocessor 0 (SR, CAUSE, EPC).

use crate::Psx;
use savestate::state;

// Exception codes, as CAUSE has them.
pub const INTERRUPT: u32 = 0x00;
const LOAD_ERROR: u32 = 0x04;
const STORE_ERROR: u32 = 0x05;
const FETCH_BUS_ERROR: u32 = 0x06;
pub const SYSCALL: u32 = 0x08;
const BREAK: u32 = 0x09;
const RESERVED: u32 = 0x0a;
const UNUSABLE: u32 = 0x0b;
const OVERFLOW: u32 = 0x0c;

#[derive(Clone, Default)]
pub struct Cpu {
    pub r: [u32; 32],
    pub hi: u32,
    pub lo: u32,
    /// The instruction to run, and the one after it: a branch changes the
    /// second, so the first (its delay slot) still runs.
    pub pc: u32,
    pub next_pc: u32,
    /// The address of the instruction running.
    pub(crate) current: u32,
    /// The instruction running is in a branch's delay slot; the one after
    /// will be.
    pub(crate) delay_slot: bool,
    pub(crate) branched: bool,
    /// A load whose value reaches its register after the next instruction,
    /// and the register an instruction wrote (whose write a pending load to
    /// it loses to).
    pub(crate) load: (usize, u32),
    written: usize,
    /// The load reaching its register as the instruction runs: what LWL and
    /// LWR merge with.
    arriving: (usize, u32),
    /// Coprocessor 0: SR, CAUSE, EPC, BadVaddr.
    pub sr: u32,
    pub cause: u32,
    pub epc: u32,
    pub bad_vaddr: u32,
}

state!(Cpu { r, hi, lo, pc, next_pc, current, delay_slot, branched, load, written, sr, cause, epc, bad_vaddr });

impl Cpu {
    /// At reset: the BIOS's first instruction.
    pub fn new() -> Cpu {
        Cpu { pc: 0xbfc0_0000, next_pc: 0xbfc0_0004, sr: 0x0040_0000, ..Cpu::default() }
    }

    fn set(&mut self, n: usize, value: u32) {
        self.r[n] = value;
        self.r[0] = 0;
        self.written = n;
    }
}

impl Psx {
    /// Runs one instruction, or takes an interrupt first.
    pub(crate) fn step(&mut self) {
        let pc = self.cpu.pc;
        if self.cpu.sr & 1 != 0 && self.cpu.sr & self.cpu.cause & 0xff00 != 0 {
            self.exception(INTERRUPT);
            return;
        }
        let physical = pc & 0x1fff_ffff;
        let op = if physical < 0x80_0000 && pc & 3 == 0 {
            // The BIOS's vectors are in the first 100h bytes.
            if physical < 0x100 && self.hle_trap(pc) {
                return;
            }
            self.cpu.current = pc;
            self.cpu.delay_slot = self.cpu.branched;
            self.ram_word(physical)
        } else {
            match self.fetch_elsewhere(pc) {
                Some(op) => op,
                None => return,
            }
        };
        self.cpu.branched = false;
        self.cpu.pc = self.cpu.next_pc;
        self.cpu.next_pc = self.cpu.next_pc.wrapping_add(4);
        self.cpu.arriving = std::mem::take(&mut self.cpu.load);
        self.cpu.written = 0;
        self.execute(op);
        // The load before reaches its register now, unless this instruction
        // wrote it (or loads it again).
        let (n, value) = std::mem::take(&mut self.cpu.arriving);
        if n != 0 && n != self.cpu.written && n != self.cpu.load.0 {
            self.cpu.r[n] = value;
        }
        // An instruction a cycle, its code in the instruction cache (which
        // is not emulated); loads wait for the bus on top.
        self.cycles += 1;
    }

    /// An instruction from outside RAM, or from an address that is not a
    /// word's: from the BIOS, or the BIOS answered here, or an exception.
    #[cold]
    fn fetch_elsewhere(&mut self, pc: u32) -> Option<u32> {
        if self.hle_trap(pc) {
            return None;
        }
        self.cpu.current = pc;
        self.cpu.delay_slot = self.cpu.branched;
        if pc & 3 != 0 {
            self.cpu.bad_vaddr = pc;
            self.exception(LOAD_ERROR);
            return None;
        }
        // Code runs from RAM and the BIOS, and from the ports of DMA and the
        // SPU: anywhere else (the scratchpad too) the fetch gets a bus error.
        let physical = pc & 0x1fff_ffff;
        let fetchable = (0x1fc0_0000..0x1fc8_0000).contains(&physical)
            || (0x1f80_1080..0x1f80_1100).contains(&physical)
            || (0x1f80_1c00..0x1f80_2000).contains(&physical);
        if !fetchable {
            self.exception(FETCH_BUS_ERROR);
            return None;
        }
        Some(self.load32(pc))
    }

    /// Takes an exception: the mode stack in SR is pushed, CAUSE and EPC
    /// say what and where (the branch, for one in a delay slot), and the
    /// CPU goes to the handler.
    pub(crate) fn exception(&mut self, code: u32) {
        let cpu = &mut self.cpu;
        // The instruction that did not run (an interrupt comes before it)
        // or did not finish.
        let (at, in_delay) = if code == INTERRUPT { (cpu.pc, cpu.branched) } else { (cpu.current, cpu.delay_slot) };
        cpu.epc = if in_delay { at.wrapping_sub(4) } else { at };
        cpu.cause = cpu.cause & !0x8000_007c | code << 2 | u32::from(in_delay) << 31;
        cpu.sr = cpu.sr & !0x3f | (cpu.sr & 0x0f) << 2;
        let handler = if cpu.sr & 1 << 22 != 0 { 0xbfc0_0180 } else { 0x8000_0080 };
        cpu.pc = handler;
        cpu.next_pc = handler.wrapping_add(4);
        cpu.branched = false;
        cpu.delay_slot = false;
        // A load on its way still reaches its register.
        let (n, value) = std::mem::take(&mut cpu.load);
        if n != 0 {
            cpu.r[n] = value;
        }
        self.cycles += 2;
    }

    fn branch(&mut self, target: u32) {
        self.cpu.next_pc = target;
        self.cpu.branched = true;
    }

    fn reg(&self, n: u32) -> u32 {
        self.cpu.r[n as usize]
    }

    fn set_reg(&mut self, n: u32, value: u32) {
        self.cpu.set(n as usize, value);
    }

    /// A load: the value reaches the register after the next instruction.
    fn delay_load(&mut self, n: u32, value: u32) {
        if n != 0 {
            self.cpu.load = (n as usize, value);
        }
    }

    fn execute(&mut self, op: u32) {
        let (rs, rt, rd) = (op >> 21 & 31, op >> 16 & 31, op >> 11 & 31);
        let imm = op & 0xffff;
        let simm = imm as i16 as i32 as u32;
        match op >> 26 {
            0x00 => self.special(op, rs, rt, rd),
            0x01 => {
                // BLTZ, BGEZ, BLTZAL, BGEZAL (the link bits are 0x10 of rt).
                let taken = (self.reg(rs) as i32) < 0;
                let taken = taken ^ (rt & 1 != 0);
                if rt & 0x1e == 0x10 {
                    self.set_reg(31, self.cpu.next_pc);
                }
                if taken {
                    self.branch(self.cpu.pc.wrapping_add(simm << 2));
                }
            }
            0x02 => self.branch(self.cpu.pc & 0xf000_0000 | (op & 0x03ff_ffff) << 2),
            0x03 => {
                self.set_reg(31, self.cpu.next_pc);
                self.branch(self.cpu.pc & 0xf000_0000 | (op & 0x03ff_ffff) << 2);
            }
            0x04 => {
                if self.reg(rs) == self.reg(rt) {
                    self.branch(self.cpu.pc.wrapping_add(simm << 2));
                }
            }
            0x05 => {
                if self.reg(rs) != self.reg(rt) {
                    self.branch(self.cpu.pc.wrapping_add(simm << 2));
                }
            }
            0x06 => {
                if self.reg(rs) as i32 <= 0 {
                    self.branch(self.cpu.pc.wrapping_add(simm << 2));
                }
            }
            0x07 => {
                if self.reg(rs) as i32 > 0 {
                    self.branch(self.cpu.pc.wrapping_add(simm << 2));
                }
            }
            0x08 => match (self.reg(rs) as i32).checked_add(simm as i32) {
                Some(v) => self.set_reg(rt, v as u32),
                None => self.exception(OVERFLOW),
            },
            0x09 => self.set_reg(rt, self.reg(rs).wrapping_add(simm)),
            0x0a => self.set_reg(rt, u32::from((self.reg(rs) as i32) < simm as i32)),
            0x0b => self.set_reg(rt, u32::from(self.reg(rs) < simm)),
            0x0c => self.set_reg(rt, self.reg(rs) & imm),
            0x0d => self.set_reg(rt, self.reg(rs) | imm),
            0x0e => self.set_reg(rt, self.reg(rs) ^ imm),
            0x0f => self.set_reg(rt, imm << 16),
            0x10 => self.cop0(op, rs, rt, rd),
            0x12 => self.cop2_op(op),
            // Coprocessors 1 and 3 are not there: enabled, their
            // instructions do nothing.
            0x11 | 0x13 => self.no_coprocessor(op >> 26 & 3),
            0x20 => {
                let addr = self.reg(rs).wrapping_add(simm);
                let v = self.load8(addr) as i8 as i32 as u32;
                self.delay_load(rt, v);
            }
            0x21 => {
                let addr = self.reg(rs).wrapping_add(simm);
                if addr & 1 != 0 {
                    return self.address_error(addr, LOAD_ERROR);
                }
                let v = self.load16(addr) as i16 as i32 as u32;
                self.delay_load(rt, v);
            }
            0x22 | 0x26 => {
                // LWL, LWR: the bytes of an unaligned word, merged with the
                // register as a load in progress leaves it.
                let addr = self.reg(rs).wrapping_add(simm);
                let arriving = self.cpu.arriving;
                let current = if arriving.0 == rt as usize { arriving.1 } else { self.reg(rt) };
                let word = self.load32(addr & !3);
                let v = merge_load(op >> 26 == 0x22, addr, word, current);
                self.delay_load(rt, v);
            }
            0x23 => {
                let addr = self.reg(rs).wrapping_add(simm);
                if addr & 3 != 0 {
                    return self.address_error(addr, LOAD_ERROR);
                }
                let v = self.load32(addr);
                self.delay_load(rt, v);
            }
            0x24 => {
                let addr = self.reg(rs).wrapping_add(simm);
                let v = u32::from(self.load8(addr));
                self.delay_load(rt, v);
            }
            0x25 => {
                let addr = self.reg(rs).wrapping_add(simm);
                if addr & 1 != 0 {
                    return self.address_error(addr, LOAD_ERROR);
                }
                let v = u32::from(self.load16(addr));
                self.delay_load(rt, v);
            }
            0x28 => {
                let addr = self.reg(rs).wrapping_add(simm);
                self.store8(addr, self.reg(rt) as u8);
            }
            0x29 => {
                let addr = self.reg(rs).wrapping_add(simm);
                if addr & 1 != 0 {
                    return self.address_error(addr, STORE_ERROR);
                }
                self.store16(addr, self.reg(rt) as u16);
            }
            0x2a | 0x2e => {
                // SWL, SWR.
                let addr = self.reg(rs).wrapping_add(simm);
                let word = self.load32(addr & !3);
                let merged = merge_store(op >> 26 == 0x2a, addr, word, self.reg(rt));
                self.store32(addr & !3, merged);
            }
            0x2b => {
                let addr = self.reg(rs).wrapping_add(simm);
                if addr & 3 != 0 {
                    return self.address_error(addr, STORE_ERROR);
                }
                self.store32(addr, self.reg(rt));
            }
            // LWC2, SWC2: GTE data registers.
            0x32 => {
                if self.cpu.sr & 1 << 30 == 0 {
                    return self.unusable(2);
                }
                let addr = self.reg(rs).wrapping_add(simm);
                if addr & 3 != 0 {
                    return self.address_error(addr, LOAD_ERROR);
                }
                let v = self.load32(addr);
                self.gte.write_data(rt as usize, v);
            }
            0x3a => {
                if self.cpu.sr & 1 << 30 == 0 {
                    return self.unusable(2);
                }
                let addr = self.reg(rs).wrapping_add(simm);
                if addr & 3 != 0 {
                    return self.address_error(addr, STORE_ERROR);
                }
                let v = self.gte.read_data(rt as usize);
                self.store32(addr, v);
            }
            // LWC0/1/3, SWC0/1/3: no such coprocessors (or registers).
            0x30 | 0x31 | 0x33 | 0x38 | 0x39 | 0x3b => self.no_coprocessor(op >> 26 & 3),
            _ => self.exception(RESERVED),
        }
    }

    /// An instruction of a coprocessor that is not there: unusable unless
    /// SR enables it.
    fn no_coprocessor(&mut self, cop: u32) {
        if self.cpu.sr & 1 << (28 + cop) == 0 {
            self.unusable(cop);
        }
    }

    /// A coprocessor that is not there or not enabled; CAUSE says which.
    fn unusable(&mut self, cop: u32) {
        self.cpu.cause = self.cpu.cause & !0x3000_0000 | cop << 28;
        self.exception(UNUSABLE);
    }

    fn address_error(&mut self, addr: u32, code: u32) {
        self.cpu.bad_vaddr = addr;
        self.exception(code);
    }

    fn special(&mut self, op: u32, rs: u32, rt: u32, rd: u32) {
        let shamt = op >> 6 & 31;
        match op & 0x3f {
            0x00 => self.set_reg(rd, self.reg(rt) << shamt),
            0x02 => self.set_reg(rd, self.reg(rt) >> shamt),
            0x03 => self.set_reg(rd, ((self.reg(rt) as i32) >> shamt) as u32),
            0x04 => self.set_reg(rd, self.reg(rt) << (self.reg(rs) & 31)),
            0x06 => self.set_reg(rd, self.reg(rt) >> (self.reg(rs) & 31)),
            0x07 => self.set_reg(rd, ((self.reg(rt) as i32) >> (self.reg(rs) & 31)) as u32),
            0x08 => self.branch(self.reg(rs)),
            0x09 => {
                let target = self.reg(rs);
                self.set_reg(rd, self.cpu.next_pc);
                self.branch(target);
            }
            0x0c => self.exception(SYSCALL),
            0x0d => self.exception(BREAK),
            0x10 => self.set_reg(rd, self.cpu.hi),
            0x11 => self.cpu.hi = self.reg(rs),
            0x12 => self.set_reg(rd, self.cpu.lo),
            0x13 => self.cpu.lo = self.reg(rs),
            0x18 => {
                let v = i64::from(self.reg(rs) as i32) * i64::from(self.reg(rt) as i32);
                (self.cpu.hi, self.cpu.lo) = ((v >> 32) as u32, v as u32);
            }
            0x19 => {
                let v = u64::from(self.reg(rs)) * u64::from(self.reg(rt));
                (self.cpu.hi, self.cpu.lo) = ((v >> 32) as u32, v as u32);
            }
            0x1a => {
                (self.cpu.lo, self.cpu.hi) = divide(self.reg(rs) as i32, self.reg(rt) as i32);
            }
            0x1b => {
                let (n, d) = (self.reg(rs), self.reg(rt));
                (self.cpu.lo, self.cpu.hi) = if d == 0 { (0xffff_ffff, n) } else { (n / d, n % d) };
            }
            0x20 => match (self.reg(rs) as i32).checked_add(self.reg(rt) as i32) {
                Some(v) => self.set_reg(rd, v as u32),
                None => self.exception(OVERFLOW),
            },
            0x21 => self.set_reg(rd, self.reg(rs).wrapping_add(self.reg(rt))),
            0x22 => match (self.reg(rs) as i32).checked_sub(self.reg(rt) as i32) {
                Some(v) => self.set_reg(rd, v as u32),
                None => self.exception(OVERFLOW),
            },
            0x23 => self.set_reg(rd, self.reg(rs).wrapping_sub(self.reg(rt))),
            0x24 => self.set_reg(rd, self.reg(rs) & self.reg(rt)),
            0x25 => self.set_reg(rd, self.reg(rs) | self.reg(rt)),
            0x26 => self.set_reg(rd, self.reg(rs) ^ self.reg(rt)),
            0x27 => self.set_reg(rd, !(self.reg(rs) | self.reg(rt))),
            0x2a => self.set_reg(rd, u32::from((self.reg(rs) as i32) < self.reg(rt) as i32)),
            0x2b => self.set_reg(rd, u32::from(self.reg(rs) < self.reg(rt))),
            _ => self.exception(RESERVED),
        }
    }

    /// A COP0 register as MFC0 reads it; None for those that do not exist.
    pub(crate) fn cop0_read(&self, rd: u32) -> Option<u32> {
        Some(match rd {
            3 | 5 | 6 | 7 | 9 | 11 => 0,
            8 => self.cpu.bad_vaddr,
            12 => self.cpu.sr,
            13 => self.cpu.cause,
            14 => self.cpu.epc,
            // PRID: the R3000A's.
            15 => 2,
            _ => return None,
        })
    }

    pub(crate) fn cop0_write(&mut self, rd: u32, v: u32) {
        match rd {
            12 => self.cpu.sr = v,
            // Only the two software interrupt bits can be written.
            13 => self.cpu.cause = self.cpu.cause & !0x300 | v & 0x300,
            _ => {}
        }
    }

    fn cop0(&mut self, op: u32, rs: u32, rt: u32, rd: u32) {
        match rs {
            // MFC0: a load, with its delay.
            0x00 => {
                let Some(v) = self.cop0_read(rd) else { return self.exception(RESERVED) };
                self.delay_load(rt, v);
            }
            0x04 => self.cop0_write(rd, self.reg(rt)),
            // RFE: pops the mode stack; the other commands (the TLB's) do
            // nothing.
            0x10..=0x1f if op & 0x3f == 0x10 => self.cpu.sr = self.cpu.sr & !0x0f | self.cpu.sr >> 2 & 0x0f,
            _ => {}
        }
    }

    /// Coprocessor 2, the GTE: register moves and commands.
    fn cop2_op(&mut self, op: u32) {
        if self.cpu.sr & 1 << 30 == 0 {
            return self.unusable(2);
        }
        let (rt, rd) = (op >> 16 & 31, (op >> 11 & 31) as usize);
        if op & 1 << 25 != 0 {
            // The CPU waits for the command (it would only when reading
            // its results).
            self.cycles += self.gte.command(op);
            return;
        }
        match op >> 21 & 31 {
            0x00 => {
                let v = self.gte.read_data(rd);
                self.delay_load(rt, v);
            }
            0x02 => {
                let v = self.gte.read_control(rd);
                self.delay_load(rt, v);
            }
            0x04 => self.gte.write_data(rd, self.reg(rt)),
            0x06 => self.gte.write_control(rd, self.reg(rt)),
            _ => self.exception(RESERVED),
        }
    }
}

/// DIV's quotient and remainder: by 0 and the one overflow give fixed
/// results.
pub(crate) fn divide(n: i32, d: i32) -> (u32, u32) {
    match d {
        0 => (if n >= 0 { 0xffff_ffff } else { 1 }, n as u32),
        -1 if n == i32::MIN => (0x8000_0000, 0),
        _ => ((n / d) as u32, (n % d) as u32),
    }
}

/// LWL (left) and LWR: the bytes of the word at `addr` merged into the
/// register's current value.
pub(crate) fn merge_load(left: bool, addr: u32, word: u32, current: u32) -> u32 {
    let shift = (addr & 3) * 8;
    if left {
        let keep = 0x00ff_ffffu32.checked_shr(shift).unwrap_or(0);
        current & keep | word << (24 - shift)
    } else {
        let keep = !(0xffff_ffffu32 >> shift);
        current & keep | word >> shift
    }
}

/// SWL (left) and SWR: the register's bytes merged into the word there.
pub(crate) fn merge_store(left: bool, addr: u32, word: u32, value: u32) -> u32 {
    let shift = (addr & 3) * 8;
    if left {
        let keep = !(0xffff_ffffu32 >> (24 - shift));
        word & keep | value >> (24 - shift)
    } else {
        let keep = !(0xffff_ffffu32 << shift);
        word & keep | value << shift
    }
}
