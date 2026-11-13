//! The RSP's JIT: its compiled code, by IMEM address and the pipeline's
//! state on entry. IMEM holds 4 KB of code that a game replaces several
//! times a frame (graphics and sound microcode, the overlays of each, and
//! a few words written as data), so each block keeps the words it came
//! from and a few versions are kept for each address. A write to IMEM (by
//! DMA or the CPU) counts in `Rsp::imem_writes` and stamps the 256-byte
//! pages it touched; a block known to match as of a count still matches
//! while none of its pages is written after it, and only then are its
//! words compared again.

#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
use super::CodeMemory;
#[cfg(target_arch = "arm")]
use super::rsp_compile;
#[cfg(target_arch = "aarch64")]
use super::rsp_compile_a64 as rsp_compile;
#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
use crate::N64;
#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
use crate::rsp::{BRANCH, Rsp};
#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
use crate::vu::Vu;

/// Versions kept of a block at one address and entry state.
#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
const VERSIONS: usize = 4;

/// Blocks kept before all are dropped.
#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
const BLOCKS: usize = 1 << 16;

#[derive(Default)]
pub struct RspJit {
    pub enabled: bool,
    /// Instructions run compiled, while the RSP's profiling is on, and
    /// (always) blocks whose words were compared and blocks compiled.
    pub instructions: u64,
    pub comparisons: u64,
    pub blocks_compiled: u64,
    #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
    memory: Option<CodeMemory>,
    /// By word of IMEM and entry state (rsp_compile::variant): an index + 1
    /// into blocks of the version last run, 0 for none.
    #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
    slots: Vec<u32>,
    #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
    blocks: Vec<Block>,
    /// For compiled code going on into the next block, by slot: where the
    /// block's code goes on after its prologue (0 for none) and the count
    /// of IMEM writes it last matched at. Allocated once: its address is in
    /// the code.
    #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
    links: Option<Box<[[usize; 2]; 1024 * 8]>>,
}

#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
struct Block {
    /// Where its code is in the code memory, and where it goes on after
    /// its prologue; None where the interpreter runs the first instruction.
    code: Option<(usize, usize)>,
    /// The first word's index in IMEM, the words compiled (and looked at),
    /// and the count of IMEM writes as of which they were last the same.
    first: usize,
    words: Vec<u32>,
    checked: u32,
    /// An older version for the same slot: an index + 1, or 0.
    next: u32,
}

#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
impl Block {
    /// Whether IMEM still holds the block's words.
    fn current(&self, rsp: &Rsp) -> bool {
        let at = |i: usize| (self.first + i) & 0x3ff;
        let unwritten =
            (0..self.words.len()).all(|i| (rsp.imem_pages[at(i) >> 6].wrapping_sub(self.checked) as i32) <= 0);
        unwritten
            || self.words.iter().enumerate().all(|(i, &w)| {
                let b = 0x1000 + 4 * at(i);
                u32::from_be_bytes(rsp.memory[b..b + 4].try_into().unwrap()) == w
            })
    }
}

impl RspJit {
    /// No compiled code yet; `enabled` only counts on ARM.
    pub fn new(enabled: bool) -> RspJit {
        RspJit { enabled: enabled && cfg!(any(target_arch = "arm", target_arch = "aarch64")), ..RspJit::default() }
    }
}

/// A copy of the machine starts with no compiled code of its own.
impl Clone for RspJit {
    fn clone(&self) -> RspJit {
        RspJit::new(self.enabled)
    }
}

#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
impl RspJit {
    /// Drops all compiled code; nothing compiled is running.
    fn flush(&mut self) {
        self.slots.fill(0);
        self.blocks.clear();
        if let Some(links) = &mut self.links {
            links.fill([0; 2]);
        }
        if let Some(memory) = &mut self.memory {
            memory.used = 0;
        }
    }

    /// The code of the block at the RSP's pc for the entry state
    /// `variant`, compiled now if no kept version matches IMEM; None where
    /// the interpreter runs.
    fn code(&mut self, rsp: &Rsp, variant: usize) -> Option<*const u32> {
        if self.slots.is_empty() {
            self.slots = vec![0; 1024 * 8];
            self.links = Some(Box::new([[0; 2]; 1024 * 8]));
        }
        let slot = (rsp.pc as usize >> 2 & 0x3ff) * 8 + variant;
        let now = rsp.imem_writes;
        let (mut index, mut previous, mut depth) = (self.slots[slot], 0, 0);
        while index != 0 {
            let i = index as usize - 1;
            let block = &self.blocks[i];
            let matches = block.checked == now || {
                self.comparisons += 1;
                block.current(rsp)
            };
            if matches {
                // The version run moves to the front.
                if previous != 0 {
                    self.blocks[previous as usize - 1].next = block.next;
                    self.blocks[i].next = self.slots[slot];
                    self.slots[slot] = index;
                }
                self.blocks[i].checked = now;
                let code = self.blocks[i].code;
                self.link(slot, code, now);
                return code.map(|(offset, _)| self.memory.as_ref().unwrap().at(offset));
            }
            let next = block.next;
            depth += 1;
            if depth == VERSIONS {
                self.blocks[i].next = 0;
            }
            (previous, index) = (index, next);
        }
        self.compile(rsp, slot, variant)
    }

    fn compile(&mut self, rsp: &Rsp, slot: usize, variant: usize) -> Option<*const u32> {
        let imem: Vec<u32> =
            rsp.memory[0x1000..0x2000].chunks_exact(4).map(|b| u32::from_be_bytes(b.try_into().unwrap())).collect();
        let calls = rsp_compile::Calls {
            interpret: interpret as extern "C" fn(*mut N64, u32, u32) as usize,
            vector: vector as extern "C" fn(*mut Vu, u32) as usize,
            links: self.links.as_ref().unwrap().as_ptr() as usize,
        };
        let compiled = rsp_compile::block(&imem, rsp.pc, variant, calls);
        if self.memory.is_none() {
            self.memory = CodeMemory::new();
            if self.memory.is_none() {
                self.enabled = false;
                return None;
            }
        }
        let size = compiled.as_ref().map_or(0, |(code, _, _)| code.len());
        if self.memory.as_ref()?.used + size > super::CODE_BYTES / 4 || self.blocks.len() >= BLOCKS {
            self.flush();
        }
        let first = rsp.pc as usize >> 2 & 0x3ff;
        let (code, looked) = match compiled {
            Some((code, looked, entry)) => {
                let memory = self.memory.as_mut()?;
                let offset = memory.used;
                memory.append(&code)?;
                self.blocks_compiled += 1;
                (Some((offset, offset + entry)), looked)
            }
            None => (None, 2),
        };
        let words = (0..looked).map(|i| imem[(first + i) & 0x3ff]).collect();
        let checked = rsp.imem_writes;
        self.blocks.push(Block { code, first, words, checked, next: self.slots[slot] });
        self.slots[slot] = self.blocks.len() as u32;
        self.link(slot, code, checked);
        code.map(|(offset, _)| self.memory.as_ref().unwrap().at(offset))
    }

    /// The table of links has the block for a slot, as matching IMEM at
    /// `now` writes.
    fn link(&mut self, slot: usize, code: Option<(usize, usize)>, now: u32) {
        let entry = code.map_or(0, |(_, entry)| self.memory.as_ref().unwrap().at(entry) as usize);
        if let Some(links) = &mut self.links {
            links[slot] = [entry, now as usize];
        }
    }
}

#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
impl N64 {
    /// Runs a compiled block of the RSP if one can start here (not in a
    /// delay slot), until `end` at the latest; false where the interpreter
    /// must run the next instruction.
    pub(crate) fn run_rsp_block(&mut self, end: u64) -> bool {
        let rsp = &self.rsp;
        if rsp.after == BRANCH || rsp.next_pc != (rsp.pc + 4) & 0xffc {
            return false;
        }
        let variant = rsp_compile::variant(rsp.pairs, rsp.vector, rsp.after);
        let Some(code) = self.rsp_jit.code(rsp, variant) else { return false };
        let machine: *mut N64 = self;
        // Safety: the code was compiled for Rsp's layout from what IMEM
        // holds, returns as the C ABI wants, and reaches the rest of the
        // machine only through `interpret`, while nothing else runs.
        let ran = unsafe {
            let block: extern "C" fn(*mut crate::rsp::Rsp, *mut u8, *mut N64, u32) -> u32 = std::mem::transmute(code);
            block(&raw mut (*machine).rsp, (*machine).rsp.memory.as_mut_ptr(), machine, end as u32)
        };
        if self.rsp.profiling {
            self.rsp_jit.instructions += u64::from(ran);
        }
        true
    }
}

/// What compiled code calls for an instruction it leaves to the
/// interpreter (already timed).
#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
extern "C" fn interpret(machine: *mut N64, word: u32, pc: u32) {
    unsafe { (*machine).rsp_interpret(word, pc) }
}

/// What compiled code calls for a computation of the vector unit.
#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
extern "C" fn vector(vu: *mut Vu, word: u32) {
    unsafe { (*vu).execute(word) }
}

#[cfg(all(test, any(target_arch = "arm", target_arch = "aarch64")))]
mod tests {
    use crate::N64;

    fn random(seed: &mut u64) -> u64 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        *seed
    }

    /// Registers 1-7 mostly, so that instructions depend on each other.
    fn register(seed: &mut u64) -> u32 {
        let r = random(seed);
        if r & 7 == 0 { (r >> 3) as u32 & 31 } else { (r >> 3) as u32 % 7 + 1 }
    }

    /// An instruction of the scalar unit, a branch, a vector instruction,
    /// a move or a DMA.
    fn instruction(seed: &mut u64, out: &mut Vec<u32>) {
        let r = random(seed);
        let (rs, rt, rd) = (register(seed), register(seed), register(seed));
        let imm = (random(seed) as u32) & 0xffff;
        let pick = r % 100;
        let word = match pick {
            0..=24 => {
                let functs = [0x00, 0x02, 0x03, 0x04, 0x06, 0x07, 0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27,
                    0x2a, 0x2b, 0x01, 0x30];
                let funct = functs[(r >> 8) as usize % functs.len()];
                rs << 21 | rt << 16 | rd << 11 | ((r >> 16) as u32 & 31) << 6 | funct
            }
            25..=39 => (0x08 + (r >> 8) as u32 % 8) << 26 | rs << 21 | rt << 16 | imm,
            40..=54 => {
                let ops = [0x20, 0x21, 0x23, 0x24, 0x25, 0x27, 0x28, 0x29, 0x2b];
                let op = ops[(r >> 8) as usize % ops.len()];
                // Offsets near DMEM's end and from base registers near it.
                let offset = if r >> 20 & 1 != 0 { 0xff8 + ((r >> 21) as u32 & 15) } else { imm };
                op << 26 | rs << 21 | rt << 16 | offset & 0xffff
            }
            55..=66 => {
                // Branches a few instructions either way.
                let offset = ((r >> 8) as i32 % 9 - 3) as u32 & 0xffff;
                match (r >> 16) % 8 {
                    0 => 0x01 << 26 | rs << 21 | ((r >> 24) as u32 & 0x13) << 16 | offset,
                    1 => 0x02 << 26 | (random(seed) as u32 & 0x3ff),
                    2 => 0x03 << 26 | (random(seed) as u32 & 0x3ff),
                    3 => rs << 21 | 0x08,
                    4 => rs << 21 | rd << 11 | 0x09,
                    n => (0x04 + n as u32 - 4) << 26 | rs << 21 | rt << 16 | offset,
                }
            }
            67..=78 => {
                // Vector arithmetic, element forms and all.
                0x4a00_0000 | ((r >> 8) as u32 & 15) << 21 | ((r >> 12) as u32 & 31) << 16
                    | ((r >> 17) as u32 & 31) << 11 | ((r >> 22) as u32 & 31) << 6 | (r >> 27) as u32 & 63
            }
            79..=86 => {
                let op = if r >> 8 & 1 != 0 { 0x32 } else { 0x3a };
                // Every form, or mostly the plain ones at even elements
                // (from r0, so that quads are aligned).
                let (kind, e, base) = if r >> 9 & 1 != 0 {
                    ((r >> 10) as u32 % 12, (r >> 18) as u32 & 15, rs)
                } else {
                    ((r >> 10) as u32 % 4 + 1, (r >> 18) as u32 & 14, if r >> 14 & 1 != 0 { 0 } else { rs })
                };
                op << 26 | base << 21 | ((r >> 13) as u32 & 31) << 16 | kind << 11 | e << 7 | (r >> 22) as u32 & 0x7f
            }
            87..=92 => {
                // MFC2, CFC2, MTC2, CTC2.
                let rs = [0, 2, 4, 6][(r >> 8) as usize % 4];
                0x12 << 26 | rs << 21 | rt << 16 | ((r >> 12) as u32 & 31) << 11 | ((r >> 17) as u32 & 15) << 7
            }
            93..=95 => {
                // MFC0 of the SP's and RDP's registers.
                0x10 << 26 | rt << 16 | ((r >> 8) as u32 & 15) << 11
            }
            96..=97 => {
                // A DMA of a few bytes from RDRAM into DMEM or IMEM, through
                // r30, or the semaphore released.
                let mem = (random(seed) as u32 & 0x1ff8) | if r >> 8 & 1 != 0 { 0x1000 } else { 0 };
                for (reg, value) in [(0, mem), (1, random(seed) as u32 & 0xfff8), (2, (r >> 9) as u32 & 0x38)] {
                    out.push(0x0d << 26 | 30 << 16 | value & 0xffff);
                    out.push(0x10 << 26 | 4 << 21 | 30 << 16 | reg << 11);
                }
                0x10 << 26 | 4 << 21 | rt << 16 | 7 << 11
            }
            98 => 0x0000_000d,
            _ => 0,
        };
        out.push(word);
    }

    fn machine(seed: &mut u64) -> N64 {
        let mut rom = vec![0; 0x1000];
        rom[..4].copy_from_slice(&0x8037_1240u32.to_be_bytes());
        let mut n64 = N64::new(&rom).unwrap();
        for bytes in n64.rdram[..0x10000].chunks_exact_mut(8) {
            bytes.copy_from_slice(&random(seed).to_be_bytes());
        }
        let mut program = Vec::new();
        while program.len() < 1024 {
            instruction(seed, &mut program);
        }
        for (i, word) in program[..1024].iter().enumerate() {
            n64.rsp.memory[0x1000 + 4 * i..0x1004 + 4 * i].copy_from_slice(&word.to_be_bytes());
        }
        for byte in &mut n64.rsp.memory[..0x1000] {
            *byte = random(seed) as u8;
        }
        for reg in &mut n64.rsp.vu.regs {
            *reg = std::array::from_fn(|_| random(seed) as u16);
        }
        let rsp = &mut n64.rsp;
        for (i, reg) in rsp.gpr.iter_mut().enumerate().skip(1) {
            let r = random(seed);
            *reg = if r & 3 == 0 { 0xff0 + (r >> 8) as u32 % 16 } else { (r >> 8) as u32 };
            if i == 30 {
                *reg = 0;
            }
        }
        rsp.status = 1 << 6;
        rsp.pc = random(seed) as u32 & 0xffc;
        rsp.next_pc = (rsp.pc + 4) & 0xffc;
        rsp.clock = random(seed) >> 20;
        let clock = rsp.clock;
        rsp.ready = std::array::from_fn(|_| clock + random(seed) % 8 - 5);
        rsp.ready[0] = 0;
        rsp.pairs = random(seed) & 1 != 0;
        rsp.vector = random(seed) & 1 != 0;
        rsp.after = [0, 0, crate::rsp::SLOT, crate::rsp::BRANCH][random(seed) as usize % 4];
        if rsp.after == crate::rsp::BRANCH {
            rsp.next_pc = random(seed) as u32 & 0xffc;
        }
        n64
    }

    /// All the RSP can change: itself (with DMEM, IMEM and the vector
    /// unit), the RDRAM its DMAs reach, the interrupts and the schedule.
    fn same(a: &N64, b: &N64) -> bool {
        use savestate::State;
        let (mut left, mut right) = (Vec::new(), Vec::new());
        a.rsp.save(&mut left);
        b.rsp.save(&mut right);
        left == right && a.rdram[..0x11000] == b.rdram[..0x11000] && a.mi_intr == b.mi_intr && a.events == b.events
    }

    #[test]
    fn random_programs_match_the_interpreter_burst_by_burst() {
        let mut seed = 0x05e1_7a11_2026_1030u64;
        let mut compiled = 0;
        for case in 0..3000 {
            let mut fast = machine(&mut seed);
            fast.set_rsp_profiling(true);
            let mut reference = fast.clone();
            reference.set_rsp_jit(false);
            for burst in 0..24 {
                if burst % 7 == 6 {
                    // The CPU writes IMEM between bursts.
                    let addr = 0x0400_1000 | random(&mut seed) as u32 & 0xffc;
                    let mut program = Vec::new();
                    instruction(&mut seed, &mut program);
                    for n64 in [&mut fast, &mut reference] {
                        n64.rsp.write_memory(addr, program[0], !0);
                    }
                }
                fast.rsp_event();
                reference.rsp_event();
                if !same(&fast, &reference) {
                    let (a, b) = (&fast.rsp, &reference.rsp);
                    panic!(
                        "case {case}, burst {burst}: pc {:x}/{:x} next {:x}/{:x} clock {}/{} after {}/{} pairs {}/{} \
                         gpr {:x?} vs {:x?}",
                        a.pc, b.pc, a.next_pc, b.next_pc, a.clock, b.clock, a.after, b.after, a.pairs, b.pairs,
                        a.gpr, b.gpr
                    );
                }
            }
            compiled += fast.rsp_stats().compiled_instructions;
        }
        assert!(compiled > 1_000_000, "only {compiled} instructions ran compiled");
    }
}

