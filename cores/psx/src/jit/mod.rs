//! The JIT: blocks of R3000A code translated to the host's ARM, 32-bit
//! (compile.rs, the Pis' 32-bit builds) or 64-bit (compile_a64.rs, the
//! TrimUI Smart Pro and 64-bit builds); on anything else the interpreter
//! runs. A block runs
//! from an address to a branch and its delay slot, or until an instruction
//! the compiler leaves to the interpreter; the dispatcher runs blocks and,
//! where they cannot run (interrupts, a load in flight, the BIOS's vectors,
//! the cache isolated), single instructions in the interpreter, which
//! stays the reference the JIT is checked against.
//!
//! Compiled code reaches the machine through fixed offsets of `Psx` (C
//! layout) and calls back into Rust for what is not plain RAM: ports,
//! exceptions, and dropping blocks when code is written over.

#[cfg_attr(not(target_arch = "arm"), allow(dead_code))]
pub mod arm;
#[cfg_attr(not(target_arch = "arm"), allow(dead_code))]
mod compile;
#[cfg_attr(not(target_arch = "aarch64"), allow(dead_code))]
pub mod a64;
#[cfg_attr(not(target_arch = "aarch64"), allow(dead_code))]
mod compile_a64;
mod wait;

use crate::Psx;

/// Code memory: blocks are appended until it is full, then all dropped.
const CODE_BYTES: usize = 16 << 20;

/// Where the compiled code of an address is: offset + 1 into the code
/// memory, 0 for none yet, NONE for an instruction the interpreter runs,
/// or WAIT for a wait for the vertical blank (see `wait`).
const NONE: u32 = u32::MAX;
const WAIT: u32 = u32::MAX - 1;

pub struct Jit {
    /// Executable memory, mapped on first use.
    code: *mut u32,
    used: usize,
    /// By word of RAM (2 MB) and of the BIOS ROM (512 KB) after it.
    blocks: Vec<u32>,
    /// The waits found, by their first block.
    waits: Vec<wait::Wait>,
    pub enabled: bool,
}

// The code memory is only ever touched by the machine that owns it.
unsafe impl Send for Jit {}

impl Default for Jit {
    fn default() -> Jit {
        let enabled = cfg!(any(target_arch = "arm", target_arch = "aarch64"));
        Jit { code: std::ptr::null_mut(), used: 0, blocks: Vec::new(), waits: Vec::new(), enabled }
    }
}

/// A copy of the machine starts with no compiled code of its own.
impl Clone for Jit {
    fn clone(&self) -> Jit {
        Jit { enabled: self.enabled, ..Jit::default() }
    }
}

/// The slot in `blocks` of a physical address, if it holds code that can
/// be compiled.
fn slot(physical: u32) -> Option<usize> {
    match physical {
        0..0x80_0000 => Some((physical as usize & 0x1f_fffc) >> 2),
        0x1fc0_0000..0x1fc8_0000 => Some(0x8_0000 + ((physical as usize & 0x7_fffc) >> 2)),
        _ => None,
    }
}

impl Jit {
    /// Drops all compiled code (on a state loaded, or when full).
    pub fn flush(&mut self) {
        self.used = 0;
        self.blocks.iter_mut().for_each(|b| *b = 0);
        self.waits.clear();
    }
}

#[cfg(target_arch = "arm")]
mod memory {
    /// Read, write and execute memory from the kernel (mmap2).
    pub unsafe fn map(len: usize) -> *mut u32 {
        let ret: usize;
        unsafe {
            core::arch::asm!("svc #0",
                in("r7") 192usize,
                inlateout("r0") 0usize => ret,
                in("r1") len,
                in("r2") 7usize,
                in("r3") 0x22usize,
                in("r4") usize::MAX,
                in("r5") 0usize,
            );
        }
        if ret > usize::MAX - 4096 { std::ptr::null_mut() } else { ret as *mut u32 }
    }

    /// Code memory is writable and executable at once here.
    pub unsafe fn writable(_on: bool) {}

    /// Makes code written as data visible to instruction fetch.
    pub unsafe fn flush_icache(start: *const u32, words: usize) {
        unsafe {
            core::arch::asm!("svc #0",
                in("r7") 0x0f_0002usize,
                inlateout("r0") start as usize => _,
                in("r1") start as usize + 4 * words,
                in("r2") 0usize,
            );
        }
    }
}

#[cfg(target_arch = "aarch64")]
mod memory {
    use std::ffi::c_void;

    unsafe extern "C" {
        fn mmap(addr: *mut c_void, len: usize, prot: i32, flags: i32, fd: i32, offset: i64) -> *mut c_void;
        #[cfg(target_os = "macos")]
        fn pthread_jit_write_protect_np(enabled: i32);
        #[cfg(target_os = "macos")]
        fn sys_icache_invalidate(start: *mut c_void, len: usize);
    }

    /// PROT_READ | PROT_WRITE | PROT_EXEC.
    const RWX: i32 = 7;
    /// MAP_PRIVATE | MAP_ANON | MAP_JIT on macOS (where the JIT is tried on
    /// a PC), MAP_PRIVATE | MAP_ANONYMOUS on Linux.
    #[cfg(target_os = "macos")]
    const FLAGS: i32 = 0x0002 | 0x1000 | 0x0800;
    #[cfg(not(target_os = "macos"))]
    const FLAGS: i32 = 0x02 | 0x20;

    /// Read, write and execute memory.
    pub unsafe fn map(len: usize) -> *mut u32 {
        let p = unsafe { mmap(std::ptr::null_mut(), len, RWX, FLAGS, -1, 0) };
        if p as isize == -1 { std::ptr::null_mut() } else { p.cast() }
    }

    /// Code is being written (true) or run (false): on macOS a MAP_JIT page
    /// is one or the other for a thread at a time; on Linux both at once.
    pub unsafe fn writable(on: bool) {
        #[cfg(target_os = "macos")]
        unsafe {
            pthread_jit_write_protect_np(i32::from(!on))
        }
        #[cfg(not(target_os = "macos"))]
        let _ = on;
    }

    /// Makes code written as data visible to instruction fetch: each data
    /// cache line cleaned to the point of unification, each instruction
    /// cache line invalidated, by the line sizes CTR_EL0 gives.
    pub unsafe fn flush_icache(start: *const u32, words: usize) {
        #[cfg(target_os = "macos")]
        unsafe {
            sys_icache_invalidate(start as *mut c_void, 4 * words)
        }
        #[cfg(not(target_os = "macos"))]
        unsafe {
            use core::arch::asm;
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
}

impl Psx {
    /// Runs a block if one can run here, else an instruction in the
    /// interpreter.
    pub(crate) fn jit_step(&mut self) {
        #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
        match self.block_here() {
            Here::Block(code) => {
                let ram = self.ram.as_mut_ptr();
                let pages = self.code_pages.as_ptr();
                let blocks = self.jit.blocks.as_ptr();
                let scratchpad = self.scratchpad.as_mut_ptr();
                // Safety: the code was compiled for this machine's layout and
                // returns to its caller as the C ABI wants.
                #[cfg(target_arch = "arm")]
                {
                    let block: extern "C" fn(*mut Psx, *mut u8, *const u8, *const u32, *mut u8) =
                        unsafe { std::mem::transmute(code) };
                    block(self, ram, pages, blocks, scratchpad);
                }
                // A 64-bit block has the code memory too, for chaining.
                #[cfg(target_arch = "aarch64")]
                {
                    let memory = self.jit.code as *const u32;
                    let block: extern "C" fn(*mut Psx, *mut u8, *const u8, *const u32, *mut u8, *const u32) =
                        unsafe { std::mem::transmute(code) };
                    block(self, ram, pages, blocks, scratchpad, memory);
                }
            }
            Here::Waited => {}
            Here::Interpreter => return self.step_instruction(),
        }
        #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
        if self.cycles >= self.next_event {
            self.events();
        }
        #[cfg(not(any(target_arch = "arm", target_arch = "aarch64")))]
        self.step_instruction();
    }

    /// What runs at the pc: its compiled code (compiling it first), a
    /// wait's turns run at once, or the interpreter.
    #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
    fn block_here(&mut self) -> Here {
        match self.code_here() {
            Some(WAIT) => {
                let pc = self.cpu.pc;
                let wait = self.jit.waits.iter().find(|w| w.first == pc).copied();
                if wait.is_some_and(|w| self.wait_turns(&w)) { Here::Waited } else { Here::Interpreter }
            }
            Some(at) => Here::Block(unsafe { self.jit.code.add(at as usize - 1) }),
            None => Here::Interpreter,
        }
    }

    /// Where the compiled code at the pc is (compiling it first), or
    /// WAIT; None where the interpreter must run.
    #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
    fn code_here(&mut self) -> Option<u32> {
        let cpu = &self.cpu;
        let interrupt = cpu.sr & 1 != 0 && cpu.sr & cpu.cause & 0xff00 != 0;
        let pc = cpu.pc;
        let physical = pc & 0x1fff_ffff;
        if interrupt || cpu.load.0 != 0 || cpu.branched || cpu.sr & 1 << 16 != 0 || pc & 3 != 0 || physical < 0x100 {
            return None;
        }
        if physical >= 0x80_0000 && self.bios.is_empty() {
            return None;
        }
        let slot = slot(physical)?;
        if self.jit.code.is_null() {
            self.jit.code = unsafe { memory::map(CODE_BYTES) };
            if self.jit.code.is_null() {
                self.jit.enabled = false;
                return None;
            }
            self.jit.blocks = vec![0; 0x8_0000 + 0x2_0000];
        }
        let mut at = self.jit.blocks[slot];
        if at == 0 {
            at = self.compile_block(pc, slot);
        }
        (at != NONE).then_some(at)
    }

    /// Compiles the block at `pc` into the code memory; where it is.
    #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
    fn compile_block(&mut self, pc: u32, slot: usize) -> u32 {
        if let Some(w) = wait::find(self, pc) {
            self.jit.waits.retain(|x| x.first != pc);
            self.jit.waits.push(w);
            self.jit.blocks[slot] = WAIT;
            return WAIT;
        }
        #[cfg(target_arch = "arm")]
        let words = compile::block(self, pc, self.jit.code as u32);
        #[cfg(target_arch = "aarch64")]
        let words = compile_a64::block(self, pc);
        let Some(words) = words else {
            self.jit.blocks[slot] = NONE;
            return NONE;
        };
        if self.jit.used + words.len() > CODE_BYTES / 4 {
            self.jit.flush();
        }
        let at = self.jit.used;
        unsafe {
            let dst = self.jit.code.add(at);
            memory::writable(true);
            std::ptr::copy_nonoverlapping(words.as_ptr(), dst, words.len());
            memory::writable(false);
            memory::flush_icache(dst, words.len());
        }
        self.jit.used += words.len();
        // The pages the block's code came from now hold compiled code.
        let physical = pc & 0x1fff_ffff;
        if physical < 0x80_0000 {
            self.code_pages[(physical as usize & 0x1f_ffff) >> 12] = 1;
        }
        self.jit.blocks[slot] = at as u32 + 1;
        at as u32 + 1
    }

    /// The blocks compiled, for profilers: where each one's code starts,
    /// and the address of its first instruction.
    pub fn jit_blocks(&self) -> Vec<(usize, u32)> {
        if self.jit.code.is_null() {
            return Vec::new();
        }
        let compiled = self.jit.blocks.iter().enumerate().filter(|&(_, &at)| at != 0 && at < WAIT);
        compiled
            .map(|(slot, &at)| {
                let slot = slot as u32;
                let pc = if slot < 0x8_0000 { 0x8000_0000 | slot << 2 } else { 0xbfc0_0000 | (slot - 0x8_0000) << 2 };
                (self.jit.code as usize + 4 * (at as usize - 1), pc)
            })
            .collect()
    }

    /// RAM was written where compiled code came from: that code goes.
    pub(crate) fn invalidate_code(&mut self, physical: u32) {
        let page = (physical as usize & 0x1f_ffff) >> 12;
        self.code_pages[page] = 0;
        if !self.jit.blocks.is_empty() {
            self.jit.blocks[page << 10..(page + 1) << 10].iter_mut().for_each(|b| *b = 0);
        }
    }
}

/// What runs at the pc.
#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
enum Here {
    Block(*const u32),
    Waited,
    Interpreter,
}

// What compiled code calls: plain functions of the C ABI, given the
// machine first.

extern "C" fn load32(psx: *mut Psx, addr: u32) -> u32 {
    unsafe { (*psx).load32(addr) }
}

extern "C" fn load16(psx: *mut Psx, addr: u32) -> u32 {
    unsafe { u32::from((*psx).load16(addr)) }
}

extern "C" fn load16_signed(psx: *mut Psx, addr: u32) -> u32 {
    unsafe { (*psx).load16(addr) as i16 as u32 }
}

extern "C" fn load8(psx: *mut Psx, addr: u32) -> u32 {
    unsafe { u32::from((*psx).load8(addr)) }
}

extern "C" fn load8_signed(psx: *mut Psx, addr: u32) -> u32 {
    unsafe { (*psx).load8(addr) as i8 as u32 }
}

extern "C" fn store32(psx: *mut Psx, addr: u32, value: u32) {
    unsafe { (*psx).store32(addr, value) }
}

extern "C" fn store16(psx: *mut Psx, addr: u32, value: u32) {
    unsafe { (*psx).store16(addr, value as u16) }
}

extern "C" fn store8(psx: *mut Psx, addr: u32, value: u32) {
    unsafe { (*psx).store8(addr, value as u8) }
}

extern "C" fn invalidate(psx: *mut Psx, addr: u32) {
    unsafe { (*psx).invalidate_code(addr & 0x1fff_ffff) }
}

/// An exception (its code, and bit 8 if in a delay slot) at the
/// instruction at `pc`, with BadVaddr for address errors.
extern "C" fn exception(psx: *mut Psx, code: u32, pc: u32, bad_vaddr: u32) {
    let psx = unsafe { &mut *psx };
    psx.cpu.current = pc;
    psx.cpu.delay_slot = code & 0x100 != 0;
    let code = code & 0xff;
    if code == 4 || code == 5 {
        psx.cpu.bad_vaddr = bad_vaddr;
    }
    psx.exception(code);
}

/// DIV and DIVU, as the interpreter has them.
extern "C" fn div(psx: *mut Psx, n: u32, d: u32) {
    let psx = unsafe { &mut *psx };
    (psx.cpu.lo, psx.cpu.hi) = crate::cpu::divide(n as i32, d as i32);
}

extern "C" fn divu(psx: *mut Psx, n: u32, d: u32) {
    let psx = unsafe { &mut *psx };
    (psx.cpu.lo, psx.cpu.hi) = if d == 0 { (u32::MAX, n) } else { (n / d, n % d) };
}

/// COP2 is not enabled: the coprocessor-unusable exception.
extern "C" fn cop2_unusable(psx: *mut Psx, delay: u32, pc: u32) {
    let psx = unsafe { &mut *psx };
    psx.cpu.current = pc;
    psx.cpu.delay_slot = delay != 0;
    psx.cpu.cause = psx.cpu.cause & !0x3000_0000 | 2 << 28;
    psx.exception(0xb);
}

/// The GTE: a command (its cycles counted), and its registers (data 0-31,
/// control 32-63).
extern "C" fn gte_command(psx: *mut Psx, op: u32) {
    let psx = unsafe { &mut *psx };
    psx.cycles += psx.gte.command(op);
}

extern "C" fn gte_read(psx: *mut Psx, n: u32) -> u32 {
    let gte = unsafe { &(*psx).gte };
    if n < 32 { gte.read_data(n as usize) } else { gte.read_control(n as usize - 32) }
}

extern "C" fn gte_write(psx: *mut Psx, n: u32, value: u32) {
    let gte = unsafe { &mut (*psx).gte };
    if n < 32 { gte.write_data(n as usize, value) } else { gte.write_control(n as usize - 32, value) }
}

/// LWL and LWR (`left` as bit 0 of `kind`): the merged value.
extern "C" fn load_unaligned(psx: *mut Psx, addr: u32, current: u32, left: u32) -> u32 {
    let psx = unsafe { &mut *psx };
    let word = psx.load32(addr & !3);
    crate::cpu::merge_load(left != 0, addr, word, current)
}

extern "C" fn store_unaligned(psx: *mut Psx, addr: u32, value: u32, left: u32) {
    let psx = unsafe { &mut *psx };
    let word = psx.load32(addr & !3);
    psx.store32(addr & !3, crate::cpu::merge_store(left != 0, addr, word, value));
}

extern "C" fn mfc0(psx: *mut Psx, rd: u32) -> u32 {
    unsafe { (*psx).cop0_read(rd).unwrap_or(0) }
}

extern "C" fn mtc0(psx: *mut Psx, rd: u32, value: u32) {
    unsafe { (*psx).cop0_write(rd, value) }
}

extern "C" fn rfe(psx: *mut Psx) {
    let cpu = unsafe { &mut (*psx).cpu };
    cpu.sr = cpu.sr & !0x0f | cpu.sr >> 2 & 0x0f;
}
