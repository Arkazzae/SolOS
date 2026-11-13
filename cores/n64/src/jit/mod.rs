//! VR4300 blocks translated to the host's ARM, A32 for the console's
//! Cortex-A53 (compile.rs) and A64 for a 64-bit host (compile_a64.rs). A block
//! contains arithmetic, guarded reads and writes of ordinary RDRAM, and at
//! most one integer branch, its last instruction, with its delay slot when
//! that is arithmetic or an ordinary load or store; instructions without a
//! translation are run by the interpreter from inside it (`interpret`).
//! Other delay slots and exceptions retain the reference path. The dispatcher
//! stops at device events and checks source bytes before entering a block,
//! so every writer of RDRAM (including DMA and the RDP) can replace code.
//!
//! The RSP's microcode is compiled too (rsp.rs, rsp_compile.rs), with the
//! same emitter and code memory.

#[cfg(any(target_arch = "arm", test))]
mod arm;
#[cfg(any(target_arch = "arm", test))]
mod compile;
#[cfg(any(target_arch = "aarch64", test))]
mod a64;
#[cfg(any(target_arch = "aarch64", all(test, target_pointer_width = "64")))]
mod compile_a64;
mod rsp;
#[cfg(any(target_arch = "arm", test))]
mod rsp_compile;
#[cfg(any(target_arch = "aarch64", all(test, target_pointer_width = "64")))]
mod rsp_compile_a64;
#[cfg(any(target_arch = "arm", target_arch = "aarch64", test))]
mod shared;
#[cfg(all(test, any(target_arch = "arm", target_arch = "aarch64")))]
mod execution;

pub use rsp::RspJit;

#[cfg(target_arch = "arm")]
use compile as native;
#[cfg(target_arch = "aarch64")]
use compile_a64 as native;

#[cfg(any(target_arch = "arm", target_arch = "aarch64", test))]
use crate::N64;
#[cfg(any(target_arch = "arm", target_arch = "aarch64", test))]
use crate::cpu::Cpu;
#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
use crate::cpu::Target;
#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
use crate::decode::{Instr, decode};
#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
use crate::rdp_thread::{CODE_CHUNK, CODE_CHUNKS};

/// Profiling counters are host state, not part of an emulated machine.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub compiled_blocks: u64,
    pub executed_blocks: u64,
    pub instructions: u64,
    pub source_changes: u64,
    pub cached_instructions: u64,
    /// Instructions the interpreter ran from inside compiled blocks.
    pub interpreted_instructions: u64,
    /// Blocks entered from another's end, not from the dispatcher (among
    /// executed_blocks).
    pub linked_blocks: u64,
}

pub struct Jit {
    pub enabled: bool,
    pub profiling: bool,
    pub stats: Stats,
    pub interpreted_ops: std::collections::BTreeMap<crate::decode::Op, u64>,
    pub interpreted_mfc0_rd: [u64; 32],
    /// Counts what may have changed code: the machine writing a page of it
    /// (`N64::wrote_rdram`) and compiled stores into one (with the RDP
    /// thread's draws over one, RdpThread::code_drawn). A block's source
    /// is compared again only when that changed since its last comparison;
    /// with `checked_sources` it is compared each time, and must not have
    /// changed.
    #[cfg_attr(not(any(target_arch = "arm", target_arch = "aarch64")), allow(dead_code))]
    generation: u32,
    #[cfg_attr(not(any(target_arch = "arm", target_arch = "aarch64")), allow(dead_code))]
    pub checked_sources: bool,
    /// Counts emptyings of the code memory (`flush`), which a link into a
    /// block's code must not span.
    #[cfg_attr(not(any(target_arch = "arm", target_arch = "aarch64")), allow(dead_code))]
    flushes: u32,
    #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
    memory: Option<CodeMemory>,
    /// An index + 1 into entries, by physical RDRAM word. Allocated only
    /// when ARM execution first reaches compilable memory.
    #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
    slots: Vec<u32>,
    #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
    entries: Vec<Entry>,
    /// The mapped page code ran from last (`CodePage`).
    #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
    code_page: Option<CodePage>,
}

/// A page of mapped space (GoldenEye's code runs from one) translated for
/// the last block entered: blocks come one after another from the same
/// page, which the TLB maps the same while nothing changed it (its epoch),
/// the ASID or the mode.
#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
#[derive(Clone, Copy)]
struct CodePage {
    /// The virtual page number with the ASID below it.
    key: u64,
    epoch: u32,
    status: u32,
    physical: u32,
}

// Native execution is opt-in while its coverage/performance is developed.
#[allow(clippy::derivable_impls)]
impl Default for Jit {
    fn default() -> Self {
        Self {
            enabled: false,
            profiling: false,
            stats: Stats::default(),
            interpreted_ops: std::collections::BTreeMap::new(),
            interpreted_mfc0_rd: [0; 32],
            generation: 0,
            checked_sources: false,
            flushes: 0,
            #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
            memory: None,
            #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
            slots: Vec::new(),
            #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
            entries: Vec::new(),
            #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
            code_page: None,
        }
    }
}

/// A cloned machine owns a new code cache. In particular load_state's
/// transactional clone cannot keep translations of the old RDRAM.
impl Clone for Jit {
    fn clone(&self) -> Self {
        Self { enabled: self.enabled, profiling: self.profiling, ..Self::default() }
    }
}

impl Jit {
    pub fn flush(&mut self) {
        self.flushes = self.flushes.wrapping_add(1);
        #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
        {
            self.slots.fill(0);
            self.entries.clear();
            if let Some(memory) = &mut self.memory {
                // The routines are put first again (`CodeMemory::place`).
                memory.used = 0;
            }
        }
    }
}

#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
struct Entry {
    /// None caches an unsupported first word, with its bytes below so
    /// replacing that instruction can make it compilable on the next run.
    offset: Option<usize>,
    instructions: usize,
    /// The source words, as RDRAM holds them (big-endian bytes, read as
    /// native words): compared word by word, as musl's memcmp goes byte by
    /// byte.
    source: Vec<u32>,
    first: Instr,
    /// The generation its source was last found unchanged at.
    checked: u32,
    /// Whether its last instruction, a branch's slot, is in the next page
    /// (only where translation is unmapped, kseg0 and kseg1).
    crosses: bool,
}

#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
struct CachedBlock {
    code: *const u32,
    instructions: usize,
    first: Instr,
    /// The routine that starts blocks (`native::enter`).
    enter: usize,
}

#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
impl Entry {
    fn plan(&self, memory: &CodeMemory) -> CachedBlock {
        CachedBlock {
            code: self.offset.map_or(std::ptr::null(), |offset| memory.at(offset)),
            instructions: self.instructions,
            first: self.first,
            enter: memory.routines[native::ENTER],
        }
    }
}

#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
impl Jit {
    /// Each compiled block: where its code starts, the RDRAM address of its
    /// first instruction, and how many it covers, so that sampled host
    /// addresses can be told by guest code (run-n64 --samples).
    pub(crate) fn code_map(&self) -> Vec<(usize, u32, usize)> {
        let Some(memory) = &self.memory else { return Vec::new() };
        self.slots.iter().enumerate().filter(|&(_, &slot)| slot != 0).filter_map(|(word, &slot)| {
            let entry = &self.entries[slot as usize - 1];
            entry.offset.map(|offset| (memory.at(offset) as usize, (word * 4) as u32, entry.instructions))
        }).collect()
    }

    /// The machine wrote these bytes of RDRAM: if a piece of code is among
    /// them (first, whether a page of code is), sources are compared again.
    #[inline]
    pub(crate) fn wrote(&mut self, code: &[bool; crate::rdp_thread::PAGES], chunks: &[bool; CODE_CHUNKS],
        addr: usize, len: usize) {
        let pages = crate::rdp_thread::PAGES;
        let first = (addr & (crate::bus::RDRAM_SIZE - 1)) >> 12;
        let count = (((addr & 0xfff) + len.max(1) - 1) >> 12) + 1;
        if (0..count.min(pages)).any(|n| code[(first + n) % pages]) && wrote_chunk(chunks, addr, len) {
            self.generation = self.generation.wrapping_add(1);
        }
    }

    /// The block at `physical`, compiled if it is not cached or its source
    /// changed. `generation` says what may have changed code; the pages and
    /// pieces of RDRAM compiled from are set in `code` and `chunks`. Where
    /// it runs `unmapped` (in kseg0 or kseg1), the next page follows it, so
    /// a branch in the page's last word takes its slot from there.
    #[inline]
    fn block(&mut self, physical: usize, rdram: &[u8], code: &mut [bool; crate::rdp_thread::PAGES],
        chunks: &mut [bool; CODE_CHUNKS], generation: u32, unmapped: bool) -> Option<CachedBlock> {
        // Mostly cached and current.
        if let Some(memory) = &self.memory
            && let Some(&cached) = self.slots.get(physical / 4)
            && cached != 0
        {
            let entry = &self.entries[cached as usize - 1];
            if entry.checked == generation && !self.checked_sources && (unmapped || !entry.crosses) {
                return Some(entry.plan(memory));
            }
        }
        self.compiled_block(physical, rdram, code, chunks, generation, unmapped)
    }

    /// `block` where it is not cached and current: compares the source or
    /// compiles.
    #[inline(never)]
    fn compiled_block(&mut self, physical: usize, rdram: &[u8], code: &mut [bool; crate::rdp_thread::PAGES],
        chunks: &mut [bool; CODE_CHUNKS], generation: u32, unmapped: bool) -> Option<CachedBlock> {
        if self.memory.is_none() {
            self.memory = CodeMemory::new();
            if self.memory.is_none() {
                self.enabled = false;
                return None;
            }
            self.slots = vec![0; crate::bus::RDRAM_SIZE / 4];
        }
        let slot = physical / 4;
        let cached = self.slots[slot];
        if cached != 0 && (unmapped || !self.entries[cached as usize - 1].crosses) {
            let checked_sources = self.checked_sources;
            let entry = &mut self.entries[cached as usize - 1];
            let current = entry.checked == generation;
            if current && !checked_sources || same_words(rdram, physical, &entry.source) {
                entry.checked = generation;
                return Some(entry.plan(self.memory.as_ref().unwrap()));
            }
            assert!(!current, "code at {physical:x} changed though nothing wrote its page");
            if self.profiling {
                self.stats.source_changes += 1;
            }
        }

        // Do not compile across a page (but for that slot) or the end of
        // actual RDRAM. The direct kernel mapping and source bytes are
        // stable until an event.
        let page_end = (physical | 0xfff) + 1;
        let mut end = (physical + 64 * 4).min(page_end).min(rdram.len());
        let last = |at: usize| u32::from_be_bytes(rdram[at - 4..at].try_into().unwrap());
        if unmapped && end == page_end && end + 4 <= rdram.len() && shared::branches(last(end)) {
            end += 4;
        }
        let words: Vec<u32> = rdram[physical..end].chunks_exact(4)
            .map(|bytes| u32::from_be_bytes(bytes.try_into().unwrap())).collect();
        let compiled = native::block(&words, physical as u32, interpret as *const () as usize);
        let (offset, instructions, bytes) = if let Some(compiled) = compiled {
            let memory = self.memory.as_mut().unwrap();
            if memory.used + compiled.code.len() > CODE_BYTES / 4 {
                // No generated code is on the host stack at this point.
                self.flush();
            }
            let memory = self.memory.as_mut().unwrap();
            // With profiling the link routine that counts what it links
            // (`--verify-jit` checks the one that does not, which runs
            // otherwise).
            let counting = self.profiling && !self.checked_sources;
            memory.place(&compiled, counting)?;
            let offset = memory.used - compiled.code.len();
            if self.profiling {
                self.stats.compiled_blocks += 1;
            }
            (Some(offset), compiled.instructions, 4 * compiled.instructions)
        } else {
            (None, 0, 4)
        };
        let entry = Entry {
            offset, instructions, source: rdram[physical..physical + bytes].chunks_exact(4).map(native_word).collect(),
            first: decode(words[0]),
            checked: generation,
            crosses: physical + bytes > page_end,
        };
        // Within RDRAM.
        code[physical >> 12..=(physical + bytes - 1) >> 12].fill(true);
        chunks[physical >> CODE_CHUNK..=(physical + bytes - 1) >> CODE_CHUNK].fill(true);
        // A flush above invalidated cached as well as every other index.
        let old = self.slots[slot];
        let index = if old == 0 {
            self.entries.push(entry);
            self.entries.len() - 1
        } else {
            let index = old as usize - 1;
            self.entries[index] = entry;
            index
        };
        self.slots[slot] = index as u32 + 1;
        Some(self.entries[index].plan(self.memory.as_ref().unwrap()))
    }
}

/// Whether a piece of code is among these bytes of RDRAM.
#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
#[inline(never)]
fn wrote_chunk(chunks: &[bool; CODE_CHUNKS], addr: usize, len: usize) -> bool {
    let first = (addr & (crate::bus::RDRAM_SIZE - 1)) >> CODE_CHUNK;
    let count = (((addr & ((1 << CODE_CHUNK) - 1)) + len.max(1) - 1) >> CODE_CHUNK) + 1;
    (0..count.min(CODE_CHUNKS)).any(|n| chunks[(first + n) % CODE_CHUNKS])
}

/// A word of RDRAM's bytes, in the host's order: only for comparing.
#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
fn native_word(bytes: &[u8]) -> u32 {
    u32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

/// Whether RDRAM at `physical` still holds these source words.
#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
fn same_words(rdram: &[u8], physical: usize, source: &[u32]) -> bool {
    rdram.get(physical..physical + 4 * source.len())
        .is_some_and(|bytes| bytes.chunks_exact(4).zip(source).all(|(b, &w)| native_word(b) == w))
}

#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
impl N64 {
    /// A native block or a cached interpreter instruction; false only
    /// where a fetch still needs all of the reference path's checks.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn run_jit_block(&mut self) -> bool {
        self.run_jit(false)
    }

    /// As `run_jit_block`, then more blocks while the next starts on a
    /// sequential PC before an event, in the same virtual page or in kseg0
    /// or kseg1: its translation holds, as only an instruction that stops a
    /// block can change the TLB or the mode, and devices change only at
    /// events or through such an instruction.
    pub(crate) fn run_jit_blocks(&mut self) -> bool {
        self.run_jit(true)
    }

    fn run_jit(&mut self, chain: bool) -> bool {
        // Blocks run the 64-bit instructions natively, which supervisor and
        // user mode reserve without 64-bit addresses: there the interpreter
        // runs everything (libultra runs games in kernel mode). Only an
        // instruction that stops a block changes the mode.
        if self.rdram_unreadable || self.rdram.len() != crate::bus::RDRAM_SIZE
            || self.mi_mode & crate::bus::MI_REPEAT != 0
            || self.cycles >= self.next_event
            || !self.cpu.cop0.allows_64_bit()
        {
            return false;
        }
        let Some(mut pc) = self.cpu.linear_block_pc() else { return false };
        // Code in mapped space too (GoldenEye's is): blocks are kept by
        // physical address and take every PC from the live virtual one, and
        // a change of the TLB stops any block. A fault is the interpreter's.
        let cop0 = &self.cpu.cop0;
        let mut physical = match cop0.translate_fast(pc) {
            Some(physical) => physical,
            None => {
                let (key, epoch) = ((pc >> 12) << 8 | cop0.entry_hi & 0xff, cop0.tlb.epoch.get());
                match self.jit.code_page {
                    Some(page) if page.key == key && page.epoch == epoch && page.status == cop0.status => {
                        page.physical | pc as u32 & 0xfff
                    }
                    _ => {
                        let Ok(physical) = cop0.translate(pc, crate::cop0::Access::Fetch) else { return false };
                        self.jit.code_page = Some(CodePage { key, epoch, status: cop0.status, physical: physical & !0xfff });
                        physical
                    }
                }
            }
        };
        if physical & 3 != 0 || physical as usize + 4 > self.rdram.len().min(crate::bus::RDRAM_SIZE) {
            return false;
        }
        let mut first = true;
        // The links of the block run last, and where in its page it went on:
        // to be linked to the block found there.
        let mut link: Option<(*mut u32, u32)> = None;
        loop {
            // The block's source is within this page (or a slot in the next
            // one), which the RDP's thread must not still be drawing to (draws
            // marked over code count in code_drawn).
            self.touch_rdram(physical as usize, 4);
            let unmapped = pc >> 30 == 0x3_ffff_fffe; // kseg0 or kseg1
            if unmapped && physical & 0xfff >= 0x1000 - 65 * 4 && (physical | 0xfff) as usize + 5 <= self.rdram.len() {
                self.touch_rdram((physical | 0xfff) as usize + 1, 4);
            }
            // Without the RDP's thread, drawing writes RDRAM unmarked: sources
            // are compared every time.
            if !self.rdp_thread.enabled {
                self.jit.generation = self.jit.generation.wrapping_add(1);
            }
            let generation = self.jit.generation.wrapping_add(self.rdp_thread.code_drawn);
            let flushes = self.jit.flushes;
            let thread = &mut self.rdp_thread;
            let Some(block) = self.jit.block(physical as usize, &self.rdram, &mut thread.code, &mut thread.code_chunks,
                generation, unmapped)
            else {
                return !first;
            };
            if let Some((links, offset)) = link.take()
                && !block.code.is_null() && self.jit.flushes == flushes
            {
                // Safety: the links of the block just run, in code memory not
                // emptied since; no native code runs while they change.
                unsafe { set_link(links, offset, generation, block.code.add(native::BODY), block.instructions) };
            }
            if block.code.is_null() || block.instructions as u64 > self.next_event - self.cycles {
                self.step_cached(block.first);
                if self.jit.profiling {
                    self.jit.stats.cached_instructions += 1;
                }
                return true;
            }
            // Safety: this cache owns the published A32 code, compiled for
            // Cpu's C layout; native instructions do not fault, and those that
            // could are interpreted. No device event can change code during the
            // call: an interpreted access that could stops the block, as does a
            // store into its own source, so changed code is checked on the next
            // entry. The block reaches the machine only through `interpret`.
            // RDRAM pages the RDP's thread may draw to are only marked by the
            // machine, which an access to a device (stopping the block) makes
            // draw: the marks hold for the call, and the batches known drawn
            // only grow (a stale count makes the block exit more, not less).
            // The generation holds while native code runs: an interpreted
            // instruction that changes it stops the block, as does a store
            // into code. Links made at another are not taken.
            let (marks, known) = self.rdram_marks();
            // Outside kernel mode kseg0 and kseg1 cannot be run: no link is.
            let linking = if self.cpu.cop0.kernel() { generation << 1 } else { 1 };
            let machine: *mut N64 = self;
            let mut context = Context {
                marks,
                known: unsafe { *known },
                code: self.rdp_thread.code.as_ptr(),
                linking,
                counted: 0,
                linked: 0,
                links: std::ptr::null(),
                machine,
                left: self.cycles_left(),
                missed: std::ptr::null_mut(),
                written: 0,
                interpret,
            };
            let result = unsafe {
                native::enter(block.enter, block.code, &raw mut (*machine).cpu, (*machine).rdram.as_mut_ptr(), machine,
                    &raw mut context)
            };
            let ran = (result & 0xffff) as usize;
            // Blocks linked to charged their cycles to the cycles left, since
            // the dispatcher or an interpreted instruction last counted them;
            // `ran` is the last one's, which may be a later block's once one
            // reached its end (only the link routine that counts counts the
            // blocks linked).
            self.cycles += u64::from(self.cycles_left() - context.left);
            let ended = !context.links.is_null();
            assert!(ran <= if ended { 65 } else { block.instructions }, "compiled block returned an invalid length");
            if self.jit.profiling {
                let linked = context.linked;
                self.jit.stats.executed_blocks += 1 + u64::from(linked);
                self.jit.stats.linked_blocks += u64::from(linked);
                self.jit.stats.instructions += ran as u64 + u64::from(context.counted);
            }
            // A store into a page of code: into a piece of it, or only next
            // to code.
            let written = context.written as usize;
            if result & shared::WROTE_CODE != 0
                && self.rdp_thread.code_chunks[written >> CODE_CHUNK & (CODE_CHUNKS - 1)]
            {
                self.jit.generation = self.jit.generation.wrapping_add(1);
            }
            if result & shared::STOPPED != 0 {
                // The interpreted instruction left everything as it should be.
                return true;
            }
            if ran == 0 && result & shared::PROGRESS == 0 {
                if ended {
                    // A later block exited before its first instruction (a
                    // block reaching its end has run one, or interpreted
                    // one, which PROGRESS says).
                    return true;
                }
                self.step_cached(block.first);
                if self.jit.profiling {
                    self.jit.stats.cached_instructions += 1;
                }
                return true;
            }
            // A full terminal branch already wrote its exact control state.
            // An earlier guarded exit has not reached it yet and remains a
            // straight-line prefix, even if the cached block ends in a branch.
            if result & shared::BRANCHED == 0 && ran > 0 {
                self.cpu.finish_linear_block(ran);
            }
            self.cycles += ran as u64;
            if !chain || result & shared::WROTE_CODE != 0 || self.cycles >= self.next_event {
                return true;
            }
            let Some(next) = self.cpu.linear_block_pc() else { return true };
            if next & 3 != 0 {
                return true;
            }
            // (With sources checked, as `--verify-jit` does, only blocks
            // entered from here are compared: links keep the run as it is.)
            if !context.missed.is_null() && self.rdp_thread.enabled
                && (next >> 12 == self.cpu.current_pc >> 12 || self.cpu.cop0.translate_fast(next).is_some())
            {
                link = Some((context.missed, next as u32));
            }
            physical = if next >> 12 == pc >> 12 {
                physical & !0xfff | next as u32 & 0xfff
            } else {
                match self.cpu.cop0.translate_fast(next) {
                    Some(physical) if physical as usize + 4 <= self.rdram.len().min(crate::bus::RDRAM_SIZE) => physical,
                    _ => return true,
                }
            };
            pc = next;
            first = false;
        }
    }
}

/// What a block's code reads and leaves besides the machine, kept by the dispatcher on its stack
/// while blocks run: the compilers read it by these fields' offsets.
#[cfg(any(target_arch = "arm", target_arch = "aarch64", test))]
#[repr(C)]
pub(crate) struct Context {
    /// RDRAM's pieces by the last draw that may reach them, and the draws known finished
    /// (`N64::rdram_marks`).
    marks: *const u32,
    known: u32,
    /// Whether each page of RDRAM holds code.
    code: *const bool,
    /// The generation of links that may be taken (doubled; odd outside kernel mode, when none is).
    linking: u32,
    /// Out, with the link routine that counts: the instructions of blocks linked to, and how many.
    counted: u32,
    linked: u32,
    /// Out: the links the last block tried, set once a block reached its end.
    links: *const u32,
    /// For the interpreter.
    machine: *mut N64,
    /// The cycles left to the next event (`N64::cycles_left`), which links are charged against.
    left: u32,
    /// Out: the links the last block failed to take, for the dispatcher to fill (only those: an
    /// earlier block's would lose a link).
    missed: *mut u32,
    /// Out: where a store into a page of code stored (for WROTE_CODE).
    written: u32,
    /// Called for an instruction a block does not translate (64-bit ARM's calls reach no further
    /// than this from the code memory).
    interpret: extern "C" fn(*mut Context, u32, u32, u32) -> u32,
}

/// The routines, put first in code memory: their addresses, then those of
/// the link routines that count (for profiling).
#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
fn routines(memory: &mut CodeMemory) -> Option<[usize; native::ROUTINES + 2]> {
    let mut at = [0; native::ROUTINES + 2];
    for (k, at) in at.iter_mut().enumerate() {
        *at = memory.append(&native::routine(k))? as usize;
    }
    Some(at)
}

/// Links a block's end (its links, see `compile::link`) going to this PC
/// to code with this many instructions, at this generation: the link for
/// that PC, or a free one, or the next one in turn.
///
/// # Safety
///
/// `links` points to a compiled block's links in live code memory.
#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
unsafe fn set_link(links: *mut u32, pc: u32, generation: u32, code: *const u32, instructions: usize) {
    unsafe {
        let count = *links as usize;
        let at = |k: usize| links.add(2 + 4 * k);
        let k = match (0..count).find(|&k| *at(k) == pc).or_else(|| (0..count).find(|&k| *at(k) & 1 != 0)) {
            Some(k) => k,
            None => {
                let k = *links.add(1) as usize % count;
                *links.add(1) = (k + 1) as u32;
                k
            }
        };
        let link = at(k);
        *link = pc;
        *link.add(1) = generation << 1;
        *link.add(2) = native::link_code(link, code);
        *link.add(3) = instructions as u32;
    }
}

/// An instruction a block does not translate, run by the interpreter from
/// inside it, with the block's context: the cycles of blocks linked to and
/// the `since` instructions the block ran before it are accounted first, as
/// the dispatcher would. Whether the block must stop here (1) or go on (0):
/// it stops after an exception or a jump, an interrupt let through, an
/// event come due or moved, a change of mode or mapping, an access to
/// anything but RDRAM (devices can do all of those), a store into the
/// block's own source (`window`: its physical start, and its length in
/// bytes << 23), or a change of the generation of code (which links between
/// blocks rely on). Either way the cycles left are counted from here.
#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
extern "C" fn interpret(context: *mut Context, since: u32, word: u32, window: u32) -> u32 {
    // Safety: the dispatcher's context of the running block and its
    // machine, which the block holds no references into across this call.
    let context = unsafe { &mut *context };
    let n = unsafe { &mut *context.machine };
    n.cycles += u64::from(n.cycles_left() - context.left);
    if since > 0 {
        n.cpu.finish_linear_block(since as usize);
        n.cycles += u64::from(since);
    }
    let instruction = decode(word);
    if n.jit.profiling {
        n.jit.stats.interpreted_instructions += 1;
        *n.jit.interpreted_ops.entry(instruction.op).or_insert(0) += 1;
        if instruction.op == crate::decode::Op::Mfc0 {
            n.jit.interpreted_mfc0_rd[instruction.rd()] += 1;
        }
    }
    let target = n.cpu.access_target(instruction);
    let event = n.next_event;
    let generation = n.jit.generation.wrapping_add(n.rdp_thread.code_drawn);
    n.step_cached(instruction);
    let c = &n.cpu;
    let sequential = !c.delay_slot && c.pc == c.current_pc.wrapping_add(4) && c.next_pc == c.pc.wrapping_add(4);
    let (start, length) = (window & 0x7f_ffff, window >> 23);
    let touched = match target {
        Target::None => false,
        Target::Load(physical) => physical >= 0x80_0000,
        Target::Store(physical) => physical >= 0x80_0000 || physical.wrapping_sub(start) < length,
        Target::Unknown => true,
    };
    let stop = !sequential || touched || shared::stops(instruction.op)
        || n.next_event != event || n.cycles >= n.next_event
        || n.jit.generation.wrapping_add(n.rdp_thread.code_drawn) != generation
        || n.cpu.cop0.interrupt_pending()
        || n.mi_mode & crate::bus::MI_REPEAT != 0 || n.rdram_unreadable;
    context.left = n.cycles_left();
    u32::from(stop)
}

#[allow(dead_code)]
#[cfg(any(target_arch = "arm", target_arch = "aarch64", test))]
pub(crate) extern "C" fn mfc0_count(context: *mut Context, since: u32) -> u32 {
    let context = unsafe { &mut *context };
    let n = unsafe { &mut *context.machine };
    let elapsed = u64::from(n.cycles_left() - context.left) + u64::from(since);
    let cycles = n.cycles + elapsed;
    let cop0 = &n.cpu.cop0;
    cop0.count.wrapping_add(((cycles - cop0.count_cycle) >> 1) as u32)
}

#[cfg(any(target_arch = "arm", target_arch = "aarch64", test))]
pub(crate) extern "C" fn dmult(cpu: *mut Cpu, rs: u32, rt: u32) {
    unsafe {
        let s = (*cpu).gpr[rs as usize];
        let t = (*cpu).gpr[rt as usize];
        let product = i128::from(s as i64) * i128::from(t as i64);
        (*cpu).lo = product as u64;
        (*cpu).hi = (product >> 64) as u64;
    }
}

#[cfg(any(target_arch = "arm", target_arch = "aarch64", test))]
pub(crate) extern "C" fn dmultu(cpu: *mut Cpu, rs: u32, rt: u32) {
    unsafe {
        let s = (*cpu).gpr[rs as usize];
        let t = (*cpu).gpr[rt as usize];
        let product = u128::from(s) * u128::from(t);
        (*cpu).lo = product as u64;
        (*cpu).hi = (product >> 64) as u64;
    }
}

#[cfg(any(target_arch = "arm", target_arch = "aarch64", test))]
pub(crate) extern "C" fn ddiv(cpu: *mut Cpu, rs: u32, rt: u32) {
    unsafe {
        let s = (*cpu).gpr[rs as usize];
        let t = (*cpu).gpr[rt as usize];
        let (n, d) = (s as i64, t as i64);
        let (lo, hi) = match d {
            0 => (if n < 0 { 1 } else { u64::MAX }, n as u64),
            _ => (n.wrapping_div(d) as u64, n.wrapping_rem(d) as u64),
        };
        (*cpu).lo = lo;
        (*cpu).hi = hi;
    }
}

#[cfg(any(target_arch = "arm", target_arch = "aarch64", test))]
pub(crate) extern "C" fn ldl(cpu: *mut Cpu, rt: u32, phys: u32, ram: *const u8) {
    unsafe {
        let old = (*cpu).gpr[rt as usize];
        let aligned = (phys & !7) as usize;
        let dword = u64::from_be_bytes(*(ram.add(aligned) as *const [u8; 8]));
        let shift = 8 * (phys & 7);
        let value = old & !(u64::MAX << shift) | (dword << shift);
        (*cpu).gpr[rt as usize] = value;
    }
}

#[cfg(any(target_arch = "arm", target_arch = "aarch64", test))]
pub(crate) extern "C" fn ldr(cpu: *mut Cpu, rt: u32, phys: u32, ram: *const u8) {
    unsafe {
        let old = (*cpu).gpr[rt as usize];
        let aligned = (phys & !7) as usize;
        let dword = u64::from_be_bytes(*(ram.add(aligned) as *const [u8; 8]));
        let shift = 8 * (7 - (phys & 7));
        let value = old & !(u64::MAX >> shift) | (dword >> shift);
        (*cpu).gpr[rt as usize] = value;
    }
}

#[cfg(any(target_arch = "arm", target_arch = "aarch64", test))]
pub(crate) extern "C" fn sdl(cpu: *mut Cpu, rt: u32, phys: u32, ram: *mut u8) {
    unsafe {
        let t = (*cpu).gpr[rt as usize];
        let aligned = (phys & !7) as usize;
        let dword = u64::from_be_bytes(*(ram.add(aligned) as *const [u8; 8]));
        let shift = 8 * (phys & 7);
        let value = dword & !(u64::MAX >> shift) | (t >> shift);
        *(ram.add(aligned) as *mut [u8; 8]) = value.to_be_bytes();
    }
}

#[cfg(any(target_arch = "arm", target_arch = "aarch64", test))]
pub(crate) extern "C" fn sdr(cpu: *mut Cpu, rt: u32, phys: u32, ram: *mut u8) {
    unsafe {
        let t = (*cpu).gpr[rt as usize];
        let aligned = (phys & !7) as usize;
        let dword = u64::from_be_bytes(*(ram.add(aligned) as *const [u8; 8]));
        let shift = 8 * (7 - (phys & 7));
        let value = dword & !(u64::MAX << shift) | (t << shift);
        *(ram.add(aligned) as *mut [u8; 8]) = value.to_be_bytes();
    }
}

#[cfg(any(target_arch = "arm", target_arch = "aarch64", test))]
pub(crate) extern "C" fn ddivu(cpu: *mut Cpu, rs: u32, rt: u32) {
    unsafe {
        let s = (*cpu).gpr[rs as usize];
        let t = (*cpu).gpr[rt as usize];
        let (lo, hi) = match t {
            0 => (u64::MAX, s),
            _ => (s / t, s % t),
        };
        (*cpu).lo = lo;
        (*cpu).hi = hi;
    }
}

/// Room for the code of each cache, which a full one empties: Ocarina of
/// Time's blocks in one scene (about 17 thousand, a kilobyte each) need
/// more than 16 MB. Only the pages written take memory.
#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
const CODE_BYTES: usize = 32 << 20;

/// Owned executable storage. Pointers are private and are only used while
/// the owning machine is mutably borrowed; moving that machine is safe.
#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
pub(super) struct CodeMemory {
    pointer: *mut u32,
    used: usize,
    /// The routines blocks branch to (`routines`), first in the memory
    /// whenever it is used from its start.
    routines: [usize; native::ROUTINES + 2],
}

#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
unsafe impl Send for CodeMemory {}

#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
impl CodeMemory {
    pub(super) fn new() -> Option<Self> {
        memory::map(CODE_BYTES).map(|pointer| Self { pointer, used: 0, routines: [0; native::ROUTINES + 2] })
    }

    fn at(&self, offset: usize) -> *const u32 {
        // Only offsets of already published blocks leave the cache.
        unsafe { self.pointer.add(offset) }
    }

    /// A compiled block's code, its branches to the routines bound (with
    /// `counting`, to the link routines that count), after the routines
    /// if the memory is empty: one mapping, so that every branch reaches.
    pub(super) fn place(&mut self, compiled: &native::Compiled, counting: bool) -> Option<*const u32> {
        if self.used == 0 {
            self.routines = routines(self)?;
        }
        let routines = self.routines;
        self.append_with(&compiled.code, |code| {
            let at = code.as_ptr() as usize;
            for &(word, routine) in &compiled.calls {
                let routine = match routine {
                    native::LINK_ROUTINE | native::LINK_SCAN if counting => native::ROUTINES + routine,
                    routine => routine,
                };
                code[word] = native::bound(code[word], at + 4 * word, routines[routine]);
            }
        })
    }

    pub(super) fn append(&mut self, words: &[u32]) -> Option<*const u32> {
        self.append_with(words, |_| {})
    }

    /// The words copied in, then changed in place by `finish` (before the
    /// instruction cache sees them).
    fn append_with(&mut self, words: &[u32], finish: impl FnOnce(&mut [u32])) -> Option<*const u32> {
        if words.len() > CODE_BYTES / 4 - self.used {
            return None;
        }
        let destination = unsafe { self.pointer.add(self.used) };
        unsafe {
            std::ptr::copy_nonoverlapping(words.as_ptr(), destination, words.len());
            finish(std::slice::from_raw_parts_mut(destination, words.len()));
            memory::publish(destination, words.len());
        }
        self.used += words.len();
        Some(destination)
    }
}

#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
impl Drop for CodeMemory {
    fn drop(&mut self) {
        // Including when a transactional save-state clone replaces the old
        // machine, or when a failed load drops the temporary copy.
        unsafe { memory::unmap(self.pointer, CODE_BYTES) };
    }
}

/// Read, write and execute memory on 32-bit ARM, through system calls alone: the console's
/// programs carry no more of libc than they use.
#[cfg(target_arch = "arm")]
mod memory {
    /// Linux AArch32 mmap2: anonymous/private RWX storage. The cache is
    /// not exposed as a mutable byte slice while native code runs.
    pub fn map(bytes: usize) -> Option<*mut u32> {
        let result: usize;
        unsafe {
            core::arch::asm!("svc #0",
                in("r7") 192usize,
                inlateout("r0") 0usize => result,
                in("r1") bytes,
                in("r2") 7usize,
                in("r3") 0x22usize,
                in("r4") usize::MAX,
                in("r5") 0usize,
            );
        }
        (result != 0 && result < usize::MAX - 4095).then_some(result as *mut u32)
    }

    /// Linux's ARM cacheflush makes data writes visible to fetch.
    ///
    /// # Safety
    ///
    /// The words are in code memory.
    pub unsafe fn publish(start: *const u32, words: usize) {
        unsafe {
            core::arch::asm!("svc #0",
                in("r7") 0x0f_0002usize,
                inlateout("r0") start as usize => _,
                in("r1") start as usize + words * 4,
                in("r2") 0usize,
            );
        }
    }

    /// munmap.
    ///
    /// # Safety
    ///
    /// Nothing runs from it any more.
    pub unsafe fn unmap(pointer: *mut u32, bytes: usize) {
        unsafe {
            core::arch::asm!("svc #0",
                in("r7") 91usize,
                inlateout("r0") pointer as usize => _,
                in("r1") bytes,
            );
        }
    }
}

/// Read, write and execute memory on 64-bit ARM.
#[cfg(target_arch = "aarch64")]
mod memory {
    use std::ffi::c_void;

    unsafe extern "C" {
        fn mmap(addr: *mut c_void, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut c_void;
        fn munmap(addr: *mut c_void, len: usize) -> i32;
    }

    /// PROT_READ | PROT_WRITE | PROT_EXEC.
    const RWX: i32 = 7;
    /// MAP_PRIVATE | MAP_ANONYMOUS.
    const FLAGS: i32 = 0x02 | 0x20;

    pub fn map(bytes: usize) -> Option<*mut u32> {
        // Safety: a new mapping, at an address of the kernel's choice.
        let p = unsafe { mmap(std::ptr::null_mut(), bytes, RWX, FLAGS, -1, 0) };
        (p as isize != -1).then_some(p.cast())
    }

    /// Makes code written as data visible to instruction fetch: each data cache line cleaned to
    /// the point of unification, each instruction cache line invalidated, by the line sizes
    /// CTR_EL0 gives.
    ///
    /// # Safety
    ///
    /// The words are in code memory.
    pub unsafe fn publish(start: *const u32, words: usize) {
        use core::arch::asm;
        unsafe {
            let ctr: u64;
            asm!("mrs {}, ctr_el0", out(reg) ctr);
            let (dline, iline) = (4usize << (ctr >> 16 & 15), 4usize << (ctr & 15));
            let (from, to) = (start as usize, start as usize + 4 * words);
            let mut at = from & !(dline - 1);
            while at < to {
                asm!("dc cvau, {}", in(reg) at);
                at += dline;
            }
            asm!("dsb ish");
            let mut at = from & !(iline - 1);
            while at < to {
                asm!("ic ivau, {}", in(reg) at);
                at += iline;
            }
            asm!("dsb ish", "isb");
        }
    }

    /// # Safety
    ///
    /// Nothing runs from it any more.
    pub unsafe fn unmap(pointer: *mut u32, bytes: usize) {
        unsafe { munmap(pointer.cast(), bytes) };
    }
}

#[cfg(all(test, any(target_arch = "arm", target_arch = "aarch64")))]
mod tests {
    use super::*;
    use crate::cop0::{CU1, IE, IP7};

    const PC: u64 = 0xffff_ffff_8000_1000;
    const ADDIU: u32 = 0x2508_0001; // addiu t0, t0, 1
    const BREAK: u32 = 0x0000_000d;

    fn machine(words: &[u32]) -> N64 {
        let mut rom = vec![0; 0x1000];
        rom[..4].copy_from_slice(&0x8037_1240u32.to_be_bytes());
        let mut n64 = N64::new(&rom).unwrap();
        n64.jit.enabled = true;
        n64.jit.profiling = true;
        n64.cpu.jump(PC);
        n64.cpu.cop0.status = 0;
        n64.cpu.cop0.cause = 0;
        for (i, &word) in words.iter().chain([&BREAK]).enumerate() {
            n64.write_u32(0x1000 + 4 * i as u32, word);
        }
        n64.next_event = 1000;
        n64
    }

    fn reference_until_event(n64: &mut N64) {
        while n64.cycles < n64.next_event {
            n64.step_cpu();
        }
    }

    fn same(a: &N64, b: &N64) {
        // Includes every architectural CPU field and the event schedule;
        // compiled code and profiling counters are deliberately excluded.
        assert!(a.save_state() == b.save_state(), "machine states differ");
    }

    #[test]
    fn dispatcher_matches_instruction_steps_at_exact_event_boundaries() {
        for deadline in 1..=10 {
            let mut fast = machine(&[ADDIU; 3]);
            fast.next_event = deadline;
            let mut reference = fast.clone();
            reference.set_jit(false);
            fast.run_cpu_until_event();
            reference_until_event(&mut reference);
            same(&fast, &reference);
            if deadline == 3 {
                assert_eq!(fast.jit_stats().instructions, 3);
            }
        }
    }

    #[test]
    fn chained_blocks_match_instruction_steps_at_every_event_boundary() {
        // A loop in one page (addiu at; bne at,v0 back, its slot addiu v1),
        // then a jump to the next page, which ends a chain, and a loop
        // there.
        let mut words = vec![0x2421_0001, 0x1422_fffe, 0x2463_0001, 0x0800_0800, 0x2484_0001];
        words.resize(0x400, 0);
        words.extend([0x24a5_0001, 0x1000_fffe, 0x24c6_0001]);
        // With the link routine that counts (profiling) and without.
        for (deadline, counting) in (1..=40).flat_map(|deadline| [(deadline, true), (deadline, false)]) {
            let mut fast = machine(&words);
            fast.jit.profiling = counting;
            fast.cpu.gpr[2] = 5;
            fast.next_event = deadline;
            let mut reference = fast.clone();
            reference.set_jit(false);
            fast.run_cpu_until_event();
            reference_until_event(&mut reference);
            same(&fast, &reference);
        }
    }

    #[test]
    fn a_branch_in_a_pages_last_word_takes_its_slot_from_the_next() {
        // A loop at the end of a page (addiu at; bne at,v0 back) whose slot
        // (addiu v1) starts the next page, then b . there.
        let mut words = vec![0; 0x3fe];
        words.extend([0x2421_0001, 0x1422_fffe, 0x2463_0001, 0x1000_ffff, 0]);
        for deadline in 1..=40 {
            let mut fast = machine(&words);
            fast.cpu.jump(0xffff_ffff_8000_1ff8);
            fast.cpu.gpr[2] = 5;
            fast.next_event = deadline;
            let mut reference = fast.clone();
            reference.set_jit(false);
            fast.run_cpu_until_event();
            reference_until_event(&mut reference);
            same(&fast, &reference);
            if deadline > 3 {
                assert!(fast.jit.entries.iter().any(|entry| entry.crosses), "the slot was not taken along");
            }
        }
    }

    #[test]
    fn calls_and_returns_across_pages_match_instruction_steps() {
        // A loop calling into the next page (jal, its slot) and returning
        // (jr ra, its slot), then waiting (b .); in kseg0, where blocks are
        // linked across pages.
        let mut words = vec![0x2421_0001, 0x0c00_0800, 0x2463_0001, 0x1422_fffc, 0x0000_0000, 0x1000_ffff, 0];
        words.resize(0x400, 0);
        words.extend([0x2484_0001, 0x03e0_0008, 0x24a5_0001]);
        for (deadline, counting) in (1..=80).flat_map(|deadline| [(deadline, true), (deadline, false)]) {
            let mut fast = machine(&words);
            fast.jit.profiling = counting;
            fast.cpu.gpr[2] = 6;
            fast.next_event = deadline;
            let mut reference = fast.clone();
            reference.set_jit(false);
            fast.run_cpu_until_event();
            reference_until_event(&mut reference);
            same(&fast, &reference);
        }
        let mut fast = machine(&words);
        fast.cpu.gpr[2] = 40;
        fast.next_event = 400;
        fast.run_cpu_until_event();
        assert!(fast.jit_stats().linked_blocks > 0, "blocks were linked");
    }

    #[test]
    fn interrupt_after_native_block_sees_the_same_instruction() {
        let mut fast = machine(&[ADDIU; 3]);
        fast.next_event = 3;
        fast.cpu.cop0.status = IE | IP7;
        let mut reference = fast.clone();
        assert!(fast.run_jit_block());
        reference_until_event(&mut reference);
        for n64 in [&mut fast, &mut reference] {
            n64.cpu.cop0.set_ip(IP7, true);
            n64.step_cpu();
            assert_eq!(n64.cpu.cop0.epc, PC + 12);
        }
        same(&fast, &reference);
    }

    /// A change of code without wrote_rdram goes unseen (sources are not
    /// compared while nothing wrote code), and checked sources stop there;
    /// a read through touch_rdram is no write.
    #[test]
    #[should_panic(expected = "changed though nothing wrote its page")]
    fn checked_sources_catch_writes_around_wrote_rdram() {
        let mut fast = machine(&[ADDIU; 3]);
        fast.set_jit_source_checks(true);
        assert!(fast.run_jit_block());
        fast.cpu.jump(PC);
        fast.touch_rdram(0x1004, 4);
        fast.rdram[0x1004..0x1008].copy_from_slice(&BREAK.to_be_bytes());
        fast.run_jit_block();
    }

    #[test]
    fn source_validation_observes_writes_and_unsupported_word_replacement() {
        let mut fast = machine(&[ADDIU; 3]);
        let mut reference = fast.clone();
        for (address, word, count) in [(0x1000, ADDIU, 3), (0x1004, BREAK, 1), (0x1000, 0x3508_0040, 1)] {
            for n64 in [&mut fast, &mut reference] {
                n64.cpu.jump(PC);
                // Direct memory writes, as DMA makes them: after wrote_rdram,
                // which every machine-side write goes through.
                n64.wrote_rdram(address, 4);
                n64.rdram[address..address + 4].copy_from_slice(&word.to_be_bytes());
            }
            assert!(fast.run_jit_block());
            for _ in 0..count {
                reference.step_cpu();
            }
            same(&fast, &reference);
        }
        assert_eq!(fast.jit_stats().source_changes, 2);

        for n64 in [&mut fast, &mut reference] {
            n64.cpu.jump(PC);
            n64.write_u32(0x1000, BREAK);
        }
        let negative = {
            let (g, t) = (fast.jit.generation, &mut fast.rdp_thread);
            fast.jit.block(0x1000, &fast.rdram, &mut t.code, &mut t.code_chunks, g, true)
        }.unwrap();
        assert!(negative.code.is_null());
        for n64 in [&mut fast, &mut reference] {
            n64.write_u32(0x1000, ADDIU);
        }
        assert!(fast.run_jit_block());
        reference.step_cpu();
        same(&fast, &reference);
    }

    #[test]
    fn clone_and_state_restore_never_share_native_code() {
        let mut fast = machine(&[ADDIU; 3]);
        let state = fast.save_state();
        assert!(fast.run_jit_block());
        let original = fast.jit.memory.as_ref().unwrap().pointer;
        let clone = fast.clone();
        assert!(clone.jit.memory.is_none());
        assert!(clone.jit.slots.is_empty());

        assert!(fast.load_state(&state[..state.len() - 1]).is_err());
        assert_eq!(fast.jit.memory.as_ref().unwrap().pointer, original);
        fast.load_state(&state).unwrap();
        assert!(fast.jit.memory.is_none());
        assert_eq!(fast.jit_stats(), Stats::default());
        let mut reference = machine(&[ADDIU; 3]);
        assert!(fast.run_jit_block());
        for _ in 0..3 {
            reference.step_cpu();
        }
        same(&fast, &reference);
    }

    #[test]
    fn guarded_entries_fall_back_without_changing_the_machine() {
        for case in 0..5 {
            let mut fast = machine(&[ADDIU; 3]);
            match case {
                0 => fast.rdram_unreadable = true,
                1 => fast.cpu.cop0.status = 0x10, // user mode, kernel PC
                2 => fast.cpu.jump(PC + 2),
                3 => {
                    fast.cpu.cop0.status = IE | IP7;
                    fast.cpu.cop0.set_ip(IP7, true);
                }
                _ => {
                    fast.write_u32(0x1000, 0x1000_0002); // branch, next is its slot
                    fast.step_cpu();
                }
            }
            let before = fast.save_state();
            assert!(!fast.run_jit_block());
            assert!(before == fast.save_state());
            assert_eq!(fast.jit_stats().executed_blocks, 0);
        }
    }

    /// User code in kuseg (mapped to where `machine` put the words) that
    /// reaches a DADDU: without UX it is a reserved instruction, which no
    /// block may run natively, so none runs in that mode; with UX blocks
    /// run. Either way the machine ends as instruction steps leave it.
    #[test]
    fn no_block_runs_where_the_64_bit_instructions_are_reserved() {
        for ux in [0, 0x20] {
            let mut fast = machine(&[ADDIU, 0x0108_402d, ADDIU]); // daddu t0, t0, t0
            let cop0 = &mut fast.cpu.cop0;
            (cop0.page_mask, cop0.entry_hi, cop0.entry_lo) = (0, 0, [0x7, 1 << 6 | 0x7]);
            cop0.tlb_write(0);
            cop0.status = 0x10 | ux;
            fast.cpu.jump(0x1000);
            fast.cpu.gpr[8] = 0x4000_0000;
            fast.next_event = 100;
            let mut reference = fast.clone();
            reference.set_jit(false);
            assert_eq!(fast.run_jit_block(), ux != 0);
            assert_eq!(fast.jit_stats().executed_blocks, u64::from(ux != 0));
            fast.run_cpu_until_event();
            reference_until_event(&mut reference);
            same(&fast, &reference);
            // The reserved instruction, or (after the block) BREAK.
            assert_eq!(fast.cpu.cop0.cause >> 2 & 31, if ux == 0 { 10 } else { 9 });
        }
    }

    #[test]
    fn blocks_stop_at_page_edges_and_code_arena_reuse_drops_old_entries() {
        let mut fast = machine(&[ADDIU]);
        fast.cpu.jump(PC + 0xffc);
        fast.write_u32(0x1ffc, ADDIU);
        fast.write_u32(0x2000, ADDIU);
        let mut reference = fast.clone();
        assert!(fast.run_jit_block());
        reference.step_cpu();
        same(&fast, &reference);
        assert_eq!(fast.jit_stats().instructions, 1);

        fast.jit.memory.as_mut().unwrap().used = CODE_BYTES / 4 - 1;
        fast.cpu.jump(PC);
        reference.cpu.jump(PC);
        assert!(fast.run_jit_block());
        reference.step_cpu();
        same(&fast, &reference);
        // The link routine, then the one block.
        let mut memory = CodeMemory::new().expect("code memory");
        routines(&mut memory).expect("code space");
        assert!(fast.jit.memory.as_ref().unwrap().used < memory.used + 64);
        assert_eq!(fast.jit.entries.len(), 1);
    }

    #[test]
    fn native_load_guards_exit_before_mmio_or_faults_with_exact_metadata() {
        for (address, word) in [
            (0xffff_ffff_a404_001c, 0x8c28_0000), // lw t0, SP semaphore (side effect)
            (0xffff_ffff_8000_3001, 0x8c28_0000), // unaligned lw
            (0x0000_0000_8000_3000, 0x8c28_0000), // noncanonical address
            (0xffff_ffff_a404_001c, 0x8c20_0000), // lw zero still reads the device
            (0xffff_ffff_8000_3001, 0x8c20_0000), // lw zero still faults
        ] {
            let mut fast = machine(&[ADDIU, word, ADDIU]);
            fast.cpu.gpr[1] = address;
            fast.next_event = 3;
            let mut reference = fast.clone();
            // Only the prefix before the guarded load can run natively.
            assert!(fast.run_jit_block());
            reference.step_cpu();
            assert_eq!(fast.cycles, 1);
            assert_eq!(fast.cpu.pc, PC + 4);
            same(&fast, &reference);
            assert!(fast.run_jit_block());
            reference.step_cpu();
            same(&fast, &reference);
        }
    }

    #[test]
    fn fpu_arithmetic_interpreted_between_native_fpu_loads_and_stores() {
        let words = [
            0x3c01_8000, // lui at, 0x8000
            0x3421_4000, // ori at, at, 0x4000
            0xc420_0000, // lwc1 f0, 0(at)
            0x4600_0088, // round.l.s f2, f0
            0xe422_0004, // swc1 f2, 4(at)
            0x2508_0001, // addiu t0, t0, 1
            0xac28_0008, // sw t0, 8(at)
            0x2508_0001, // addiu t0, t0, 1
        ];
        for value in [0x3f80_0000u32, 0x7f7f_ffff, 0x0000_0001] {
            let mut fast = machine(&words);
            fast.cpu.cop0.status = CU1;
            fast.write_u32(0x4000, value);
            let mut reference = fast.clone();
            reference.set_jit(false);
            fast.run_cpu_until_event();
            reference_until_event(&mut reference);
            same(&fast, &reference);
            // ROUND.L.S is untranslated on A32 (so it is interpreted inside the block); on A64 it compiles natively.
            if cfg!(target_arch = "arm") {
                assert!(fast.jit_stats().interpreted_instructions >= 1, "{value:08x}");
            }
        }
    }

    #[test]
    fn cached_cop0_reads_and_timer_writes_keep_instruction_cycles() {
        let words = [
            0x4008_4800, // mfc0 t0, Count
            0x4009_0800, // mfc0 t1, Random
            0x408a_5800, // mtc0 t2, Compare (moves the next event earlier)
            0x400b_4800, // mfc0 t3, Count
            0x1000_ffff, // idle until Compare interrupts
            0,
        ];
        for initial_cycles in 0..4 {
            let mut fast = machine(&words);
            fast.cycles = initial_cycles;
            fast.next_event = initial_cycles + 100;
            fast.cpu.gpr[10] = 4;
            fast.cpu.cop0.status = IE | IP7;
            let mut reference = fast.clone();
            fast.run_cpu_until_event();
            reference_until_event(&mut reference);
            assert_eq!(fast.cycles, 8);
            assert_eq!(fast.next_event, 8);
            let stats = fast.jit_stats();
            assert!(stats.cached_instructions + stats.interpreted_instructions >= 4);
            same(&fast, &reference);
            for n64 in [&mut fast, &mut reference] {
                n64.run_events();
                n64.step_cpu();
            }
            same(&fast, &reference);
        }
    }

    #[test]
    fn terminal_stores_observe_code_replacement_before_the_next_instruction() {
        let replacement = 0x3508_0040; // ori t0, t0, 0x40
        for target in [0x1000, 0x1004] {
            let mut fast = machine(&[0xac29_0000, ADDIU]); // sw t1,0(at)
            fast.cpu.gpr[1] = 0xffff_ffff_8000_0000 | target as u64;
            fast.cpu.gpr[9] = replacement as u64;
            // Compile the original next instruction before it is replaced.
            {
                let (g, t) = (fast.jit.generation, &mut fast.rdp_thread);
                fast.jit.block(0x1004, &fast.rdram, &mut t.code, &mut t.code_chunks, g, true)
            }.unwrap();
            let mut reference = fast.clone();
            assert!(fast.run_jit_block());
            reference.step_cpu();
            assert_eq!(fast.cycles, 1);
            assert_eq!(fast.jit_stats().instructions, 1, "store did not execute natively");
            assert_eq!(fast.cpu.gpr[8], 0, "store did not terminate its block");
            same(&fast, &reference);
            if target == 0x1000 {
                fast.cpu.jump(PC);
                reference.cpu.jump(PC);
            }
            assert!(fast.run_jit_block());
            for _ in 0..if target == 0x1000 { 2 } else { 1 } {
                reference.step_cpu();
            }
            same(&fast, &reference);
            assert!(fast.jit_stats().source_changes > 0);
        }
    }

    #[test]
    fn stores_next_to_code_end_the_block_but_compare_no_source_again() {
        // Its page holds code, but only a piece of it (CODE_CHUNK bytes)
        // with code is code.
        for (target, changes) in [(0x1800, 0), (0x1008, 1)] {
            let mut fast = machine(&[0xac29_0000, ADDIU]); // sw t1,0(at)
            fast.rdp_thread.enabled = true;
            fast.cpu.gpr[1] = 0xffff_ffff_8000_0000 | target as u64;
            fast.cpu.gpr[9] = 0x1234_5678;
            let generation = fast.jit.generation;
            assert!(fast.run_jit_block());
            assert_eq!(fast.jit_stats().instructions, 1, "store did not execute natively");
            assert_eq!(fast.cpu.gpr[8], 0, "store did not end its block");
            assert_eq!(fast.read_u32(target), 0x1234_5678);
            assert_eq!(fast.jit.generation.wrapping_sub(generation), changes, "{target:x}");
        }
    }

    #[test]
    fn stores_in_repeat_mode_and_guard_failures_use_the_reference_bus() {
        for (address, word, repeat) in [
            (0xffff_ffff_8000_4000, 0xac28_0000, true), // repeated RDRAM write
            (0xffff_ffff_a440_0008, 0xac28_0000, false), // VI_WIDTH register
            (0xffff_ffff_8000_4001, 0xac28_0000, false), // unaligned SW
            (0xffff_ffff_8000_4004, 0xfc28_0000, false), // unaligned SD
            (0x0000_0000_8000_4000, 0xac28_0000, false), // noncanonical
        ] {
            let mut fast = machine(&[ADDIU, word, ADDIU]);
            fast.cpu.gpr[1] = address;
            fast.cpu.gpr[8] = 0x1234_5678;
            fast.next_event = 2;
            if repeat {
                fast.mi_mode |= crate::bus::MI_REPEAT;
                let before = fast.save_state();
                assert!(!fast.run_jit_block());
                assert!(before == fast.save_state());
            }
            let mut reference = fast.clone();
            fast.run_cpu_until_event();
            reference_until_event(&mut reference);
            same(&fast, &reference);
        }
    }

    #[test]
    fn native_branch_event_boundaries_preserve_slot_and_interrupt_epc() {
        for (word, annulled) in [
            (0x1000_0002, false), // beq zero,zero,target
            (0x1421_0002, false), // bne at,at,target (not taken, slot still runs)
            (0x5022_0002, true), // beql at,v0,target (not taken, slot annulled)
        ] {
            // An interpreted slot (MFC0 t0, Status), which the block leaves
            // pending at the boundary.
            let mut fast = machine(&[ADDIU, word, 0x4008_6000, ADDIU]);
            fast.cpu.gpr[1] = 1;
            fast.cpu.gpr[2] = 2;
            fast.cpu.cop0.status = IE | IP7;
            let mut reference = fast.clone();
            for deadline in [1, 2] {
                fast.next_event = deadline;
                reference.next_event = deadline;
                fast.run_cpu_until_event();
                reference_until_event(&mut reference);
                same(&fast, &reference);
            }
            assert!(fast.jit_stats().instructions >= 1, "branch never ran natively");
            for n64 in [&mut fast, &mut reference] {
                n64.cpu.cop0.set_ip(IP7, true);
                n64.step_cpu();
                assert_eq!(n64.cpu.cop0.epc, PC + if annulled { 12 } else { 4 });
                assert_eq!(n64.cpu.cop0.cause & crate::cop0::BD != 0, !annulled);
            }
            same(&fast, &reference);
        }
    }

    #[test]
    fn native_jr_defers_bad_targets_until_after_the_interpreted_slot() {
        for target in [0xffff_ffff_8000_3002, 0x4000] {
            for slot in [0, 0x0000_000c, 0x8c48_0000] { // NOP, SYSCALL, LW SP semaphore
                let mut fast = machine(&[0x0020_0008, slot]); // jr at
                fast.cpu.gpr[1] = target;
                fast.cpu.gpr[2] = 0xffff_ffff_a404_001c;
                let mut reference = fast.clone();
                assert!(fast.run_jit_block());
                // The NOP slot runs in the block, the others after it.
                let ran = if slot == 0 { 2 } else { 1 };
                for _ in 0..ran {
                    reference.step_cpu();
                }
                assert_eq!(fast.jit_stats().instructions, ran);
                assert_eq!(fast.cpu.cop0.status & crate::cop0::EXL, 0);
                same(&fast, &reference);
                for _ in 0..2 {
                    fast.step_cpu();
                    reference.step_cpu();
                    same(&fast, &reference);
                }
                if slot == 0x0000_000c {
                    assert_eq!(fast.cpu.cop0.epc, PC);
                    assert_ne!(fast.cpu.cop0.cause & crate::cop0::BD, 0);
                } else {
                    assert_eq!(fast.cpu.cop0.epc, target);
                    assert_eq!(fast.cpu.cop0.cause & crate::cop0::BD, 0);
                }
            }
        }
    }

    #[test]
    fn terminal_branch_at_page_edge_keeps_the_slot_on_the_next_page() {
        let mut fast = machine(&[]);
        fast.cpu.jump(PC + 0xffc);
        fast.write_u32(0x1ffc, 0x1000_0002);
        fast.write_u32(0x2000, 0x0000_000c);
        let mut reference = fast.clone();
        assert!(fast.run_jit_block());
        reference.step_cpu();
        assert_eq!(fast.jit_stats().instructions, 1);
        same(&fast, &reference);
        fast.step_cpu();
        reference.step_cpu();
        assert_eq!(fast.cpu.cop0.epc, PC + 0xffc);
        assert_ne!(fast.cpu.cop0.cause & crate::cop0::BD, 0);
        same(&fast, &reference);
    }

    #[test]
    fn physical_branch_cache_uses_the_current_virtual_pc_and_observes_rewrites() {
        let mut fast = machine(&[0x0c00_0c00, 0]); // jal to this virtual quarter's 0x3000
        let mut reference = fast.clone();
        for base in [0xffff_ffff_8000_0000, 0xffff_ffff_a000_0000] {
            for n64 in [&mut fast, &mut reference] {
                n64.cpu.jump(base | 0x1000);
            }
            // The JAL and its NOP slot.
            assert!(fast.run_jit_block());
            reference.step_cpu();
            reference.step_cpu();
            assert_eq!(fast.cpu.pc, base | 0x3000);
            assert_eq!(fast.cpu.gpr[31], base | 0x1008);
            same(&fast, &reference);
        }
        assert_eq!(fast.jit_stats().compiled_blocks, 1);
        for n64 in [&mut fast, &mut reference] {
            n64.cpu.jump(PC);
            n64.cpu.gpr[1] = 1;
            n64.cpu.gpr[2] = 2;
            n64.write_u32(0x1000, 0x5022_0002); // not-taken likely branch, its slot annulled
        }
        assert!(fast.run_jit_block());
        reference.step_cpu();
        assert_eq!(fast.cpu.pc, PC + 8);
        assert!(!fast.cpu.delay_slot);
        assert_eq!(fast.jit_stats().source_changes, 1);
        same(&fast, &reference);
    }

    #[test]
    fn guard_exit_before_terminal_branch_only_commits_the_linear_prefix() {
        let mut fast = machine(&[ADDIU, 0x8c28_0000, 0x1000_ffff, 0]);
        fast.cpu.gpr[1] = 0xffff_ffff_a404_001c; // SP semaphore, reference read
        let mut reference = fast.clone();
        for count in 1..=3 {
            assert!(fast.run_jit_block());
            reference.step_cpu();
            assert_eq!(fast.cycles, count);
            same(&fast, &reference);
        }
        assert_eq!(fast.cpu.current_pc, PC + 8);
        assert!(fast.cpu.delay_slot);
        assert!(fast.jit_stats().instructions >= 2); // prefix + native branch
    }

    #[test]
    fn status_moves_are_native_and_stop_where_an_interrupt_is_let_through() {
        use crate::cop0::{CU1, EXL, IP7};
        // __osDisableInt then __osRestoreInt, IE set again (or another bit
        // set too: one MTC0 that changes more is left to the interpreter):
        // mfc0 t0,status; addiu at,zero,-2; and t1,t0,at; mtc0 t1,status;
        // andi v0,t0,1; mfc0 t2,status; ori/or t2; mtc0 t2,status;
        // addiu t3,t3,1; b . (its slot a NOP).
        for (set, extra) in [(0x354a_0001, 0), (0x0144_5025, 1), (0x0144_5025, 0x10), (0x0144_5025, EXL),
            (0x0144_5025, CU1 | 1)]
        {
            let words = [0x4008_6000, 0x2401_fffe, 0x0101_4824, 0x4089_6000, 0x3102_0001, 0x400a_6000, set,
                0x408a_6000, 0x256b_0001, 0x1000_ffff, 0];
            for (status, pending) in [(IP7, true), (IP7, false), (IP7 | 1, false), (0, true)] {
                for deadline in [3, 4, 8, 9, 30] {
                    let mut fast = machine(&words);
                    fast.cpu.cop0.status = status;
                    fast.cpu.gpr[4] = u64::from(extra);
                    if pending {
                        fast.cpu.cop0.set_ip(IP7, true);
                    }
                    fast.next_event = deadline;
                    let mut reference = fast.clone();
                    reference.set_jit(false);
                    fast.run_cpu_until_event();
                    reference_until_event(&mut reference);
                    same(&fast, &reference);
                    if deadline == 30 && extra <= 1 && !pending {
                        let stats = fast.jit_stats();
                        let interpreted = stats.interpreted_instructions + stats.cached_instructions;
                        assert_eq!(interpreted, 0, "{set:08x} {status:x}");
                    }
                }
            }
        }
    }

    #[test]
    fn interpreted_count_reads_in_linked_blocks_see_their_cycles() {
        // A loop entered through its own link: addiu t0,t0,1; mfc0 t1,count
        // (interpreted, after the cycles linked blocks charged); addu t2,
        // t2,t1; bne t0,a0 back (its slot a NOP); then b . and its NOP.
        let words = [0x2508_0001, 0x4009_4800, 0x0149_5021, 0x1504_fffc, 0, 0x1000_ffff, 0];
        for (deadline, counting) in (1..=70).flat_map(|deadline| [(deadline, true), (deadline, false)]) {
            let mut fast = machine(&words);
            fast.jit.profiling = counting;
            fast.cpu.gpr[4] = 9;
            fast.cycles = 7;
            fast.next_event = 7 + deadline;
            let mut reference = fast.clone();
            reference.set_jit(false);
            fast.run_cpu_until_event();
            reference_until_event(&mut reference);
            same(&fast, &reference);
            if deadline == 70 && counting {
                assert!(fast.jit_stats().linked_blocks > 0, "blocks were linked");
            }
        }
    }

    #[test]
    fn held_registers_are_in_cpu_around_interpreted_instructions() {
        // addiu v0,v0,1; mfc0 v1,cause; addu a0,v0,v1; addiu v0,v0,2;
        // mfc0 v0,cause (over a changed one); addu a1,v0,a0; a loop of
        // them, closed by bne a1,v1 with its slot addiu v1,v1,1.
        let words = [0x2442_0001, 0x4003_6800, 0x0043_2021, 0x2442_0002, 0x4002_6800, 0x0044_2821, 0x14a3_fff9,
            0x2463_0001];
        for deadline in 1..=40 {
            let mut fast = machine(&words);
            fast.cpu.gpr[2] = 5;
            fast.cpu.gpr[3] = 3;
            fast.next_event = deadline;
            let mut reference = fast.clone();
            reference.set_jit(false);
            fast.run_cpu_until_event();
            reference_until_event(&mut reference);
            same(&fast, &reference);
        }
    }
}
