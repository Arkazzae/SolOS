//! Each compiled instruction against the interpreter, on the host's ARM: blocks run for real, so
//! these run only on one (the build's own ARM, through the --pi checks or on a unit), with whichever
//! compiler it has (`native`).

use super::shared::{BRANCHED, PROGRESS, STOPPED, WROTE_CODE};
use super::{CodeMemory, Context, native};
use crate::N64;
use crate::cop0::{CU1, FR};
use crate::cpu::Cpu;
use crate::decode::decode;
use savestate::State;
use std::cell::Cell;

thread_local! {
    /// The routine that starts blocks, in the code memory last placed in.
    static ENTER_AT: Cell<usize> = const { Cell::new(0) };
}

struct Enter;

impl Enter {
    fn get(&self) -> usize {
        ENTER_AT.with(Cell::get)
    }
}

static ENTER: Enter = Enter;

/// A block placed, and how it is started noted.
fn place(memory: &mut CodeMemory, c: &native::Compiled) -> Option<*const u32> {
    let code = memory.place(c, false);
    ENTER_AT.with(|at| at.set(memory.routines[native::ENTER]));
    code
}

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

fn block(words: &[u32]) -> Option<native::Compiled> {
    native::block(words, 0x1000, super::interpret as *const () as usize)
}


fn machine(words: &[u32]) -> N64 {
    let mut rom = vec![0; 0x1000];
    rom[..4].copy_from_slice(&0x8037_1240u32.to_be_bytes());
    let mut n64 = N64::new(&rom).unwrap();
    n64.cpu.jump(0xffff_ffff_8000_1000);
    n64.cpu.cop0.status = 0;
    n64.cpu.cop0.cause = 0;
    for (i, &word) in words.iter().enumerate() {
        n64.write_u32(0x1000 + 4 * i as u32, word);
    }
    n64
}

/// Runs a block with no instruction to interpret (so no machine) and
/// no page the RDP's thread may draw to: the instructions it
/// completed, without the result's flags.
fn execute(code: *const u32, cpu: &mut Cpu, ram: *mut u8) -> u32 {
    static PAGES: [u32; crate::rdp_thread::MARKS] = [0; crate::rdp_thread::MARKS];
    execute_marked(code, cpu, ram, &PAGES, 0) & !WROTE_CODE
}

/// The pages of code for blocks run here: theirs, at 0x1000.
static CODE_PAGES: [bool; crate::rdp_thread::PAGES] = {
    let mut code = [false; crate::rdp_thread::PAGES];
    code[1] = true;
    code
};

thread_local! {
    /// Where the last block run here stored into code, if it did.
    static WRITTEN: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

fn execute_marked(code: *const u32, cpu: &mut Cpu, ram: *mut u8, pages: &[u32], known: u32) -> u32 {
    // No machine: a link is never taken (the generation's 0, links
    // start odd), as that reads its cycles.
    let mut context = Context {
        marks: pages.as_ptr(),
        known,
        code: CODE_PAGES.as_ptr(),
        linking: 0,
        counted: 0,
        linked: 0,
        links: std::ptr::null(),
        machine: std::ptr::null_mut(),
        left: 0,
        missed: std::ptr::null_mut(),
        written: 0,
        interpret: super::interpret,
    };
    let result = unsafe { native::enter(ENTER.get(), code, cpu, ram, std::ptr::null_mut(), &raw mut context) };
    WRITTEN.with(|written| written.set(context.written));
    assert_eq!(result & (PROGRESS | STOPPED), 0, "nothing to interpret");
    result & !BRANCHED
}

fn compare(code: *const u32, reference: &mut N64, count: usize, label: &str) {
    let before = reference.cpu.clone();
    let cycles = reference.cycles;
    let mut compiled = before.clone();
    assert_eq!(execute(code, &mut compiled, reference.rdram.as_mut_ptr()), count as u32, "{label}");
    for _ in 0..count {
        reference.step_cpu();
    }
    assert_eq!(compiled.gpr, reference.cpu.gpr, "{label}");
    assert_eq!((compiled.hi, compiled.lo), (reference.cpu.hi, reference.cpu.lo), "{label}");
    assert_eq!(compiled.fpu.regs, reference.cpu.fpu.regs, "{label}");
    assert_eq!(compiled.fpu.fcr31, reference.cpu.fpu.fcr31, "FCR31: {label}");
    // The ABI promises to leave the rest of Cpu untouched.
    compiled.gpr = before.gpr;
    compiled.hi = before.hi;
    compiled.lo = before.lo;
    compiled.fpu.regs = before.fpu.regs;
    compiled.fpu.fcr31 = before.fpu.fcr31;
    let (mut a, mut b) = (Vec::new(), Vec::new());
    compiled.save(&mut a);
    before.save(&mut b);
    assert_eq!(a, b, "metadata changed: {label}");
    reference.cpu = before;
    reference.cycles = cycles;
}

/// Stores need independent memory for the native and reference
/// machines: running both on one RAM image could conceal a missing
/// or incorrect write. Reuse the allocation, but copy and compare
/// the complete image on every case, including guarded exits.
fn compare_store(code: *const u32, reference: &mut N64, native_ram: &mut [u8], count: usize, label: &str) {
    native_ram.copy_from_slice(&reference.rdram);
    let before = reference.cpu.clone();
    let cycles = reference.cycles;
    let mut compiled = before.clone();
    assert_eq!(execute(code, &mut compiled, native_ram.as_mut_ptr()), count as u32, "{label}");
    for _ in 0..count {
        reference.step_cpu();
    }
    assert_eq!(compiled.gpr, reference.cpu.gpr, "{label}");
    assert_eq!((compiled.hi, compiled.lo), (reference.cpu.hi, reference.cpu.lo), "{label}");
    if &*native_ram != reference.rdram.as_slice() {
        let at = native_ram.iter().zip(&reference.rdram).position(|(a, b)| a != b).unwrap();
        panic!("{label}: RAM {at:08x}: native {:02x}, reference {:02x}", native_ram[at], reference.rdram[at]);
    }
    assert_eq!(compiled.fpu.regs, reference.cpu.fpu.regs, "{label}");
    assert_eq!(compiled.fpu.fcr31, reference.cpu.fpu.fcr31, "FCR31: {label}");
    compiled.gpr = before.gpr;
    compiled.hi = before.hi;
    compiled.lo = before.lo;
    compiled.fpu.regs = before.fpu.regs;
    compiled.fpu.fcr31 = before.fpu.fcr31;
    let (mut a, mut b) = (Vec::new(), Vec::new());
    compiled.save(&mut a);
    before.save(&mut b);
    assert_eq!(a, b, "metadata changed: {label}");
    reference.cpu = before;
    reference.cycles = cycles;
}

fn compare_branch(code: *const u32, reference: &mut N64, count: usize, label: &str) {
    let before = reference.cpu.clone();
    let cycles = reference.cycles;
    let mut compiled = before.clone();
    assert_eq!(execute(code, &mut compiled, reference.rdram.as_mut_ptr()), count as u32, "{label}");
    for _ in 0..count {
        reference.step_cpu();
    }
    // Unlike ordinary blocks, branches own every control field.
    // Compare the complete CPU, not just visible register values.
    let (mut a, mut b) = (Vec::new(), Vec::new());
    compiled.save(&mut a);
    reference.cpu.save(&mut b);
    assert_eq!(a, b, "branch CPU differs: {label}");
    reference.cpu = before;
    reference.cycles = cycles;
}

/// A one-instruction block that may exit before it: compared with
/// the interpreter run for as many as it completed, which it returns.
fn compare_either(code: *const u32, reference: &mut N64, label: &str) -> u32 {
    let mut cpu = reference.cpu.clone();
    let completed = execute(code, &mut cpu, reference.rdram.as_mut_ptr());
    assert!(completed <= 1, "{label}");
    compare(code, reference, completed as usize, label);
    completed
}

/// A branch block, whose last instruction may be a likely branch's
/// slot that is not counted when not taken: compared with the
/// interpreter run for as many as it completed, which it returns.
fn compare_branch_either(code: *const u32, reference: &mut N64, instructions: usize, label: &str) -> u32 {
    let mut cpu = reference.cpu.clone();
    let completed = execute(code, &mut cpu, reference.rdram.as_mut_ptr());
    assert!(completed as usize == instructions || completed as usize + 1 == instructions, "{label}");
    compare_branch(code, reference, completed as usize, label);
    completed
}

/// The register an fs operand comes from: with Status.FR clear an odd fs names its pair's even one.
/// Only that one gets the case's value: the other keeps a random one, so that reading it shows.
fn fs_read(fs: usize, status: u32) -> usize {
    if status & FR != 0 { fs } else { fs & !1 }
}

fn random(seed: &mut u64) -> u64 {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 7;
    *seed ^= *seed << 17;
    *seed
}

#[test]
fn integer_ops_match_interpreter_with_arbitrary_64_bit_inputs() {
    // SPECIAL functions; no overflow-trapping, branches or memory.
    let funcs = [0x00, 0x02, 0x03, 0x04, 0x06, 0x07, 0x0f, 0x10, 0x11, 0x12, 0x13,
        0x14, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f, 0x21, 0x23, 0x24, 0x25,
        0x26, 0x27, 0x2a, 0x2b, 0x2d, 0x2f, 0x38, 0x3a, 0x3b, 0x3c, 0x3e, 0x3f];
    let immediates = [0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f, 0x19];
    let mut memory = CodeMemory::new().expect("code memory");
    let mut seed = 0x64a3_5f09_abcd_1234;
    let mut start = machine(&[0]);
    let values = [0, 1, 31, 32, 33, 63, 64, 127, u64::MAX,
        0x7fff_ffff, 0x8000_0000, 0x1234_5678_8000_0000,
        0x7fff_ffff_ffff_ffff, 0x8000_0000_0000_0000];
    // Every source/destination overlap, including the zero register.
    let aliases = [(1, 2, 3), (1, 2, 1), (1, 2, 2), (1, 1, 1), (0, 2, 3), (1, 0, 3), (1, 2, 0)];
    for &(rs, rt, rd) in &aliases {
        let mut words = Vec::new();
        for &f in &funcs {
            // DMULT to DDIVU are native on A64 alone: compile.rs leaves them to the interpreter, and
            // a block of one interpreted word does not compile.
            if !cfg!(target_arch = "aarch64") && (0x1c..=0x1f).contains(&f) {
                continue;
            }
            for sa in [0, 1, 15, 31] {
                words.push(rs << 21 | rt << 16 | rd << 11 | sa << 6 | f);
            }
        }
        for op in immediates {
            for imm in [0, 1, 0x7fff, 0x8000, 0xffff] {
                words.push(op << 26 | rs << 21 | rt << 16 | imm);
            }
        }
        for word in words {
            let c = block(&[word]).unwrap_or_else(|| panic!("unsupported {word:08x}"));
            let code = place(&mut memory, &c).expect("code space");
            start.write_u32(0x1000, word);
            for case in 0..80 {
                for r in 1..32 {
                    start.cpu.gpr[r] = random(&mut seed);
                }
                if case < values.len() {
                    start.cpu.gpr[rs as usize] = values[case];
                    start.cpu.gpr[rt as usize] = values[(case + 1) % values.len()];
                } else if case < 2 * values.len() {
                    // Shift-count boundaries with random input words.
                    start.cpu.gpr[rs as usize] = values[case - values.len()];
                }
                start.cpu.gpr[0] = 0;
                start.cpu.hi = random(&mut seed);
                start.cpu.lo = random(&mut seed);
                compare(code, &mut start, 1, &format!("{word:08x}, case {case}"));
            }
        }
    }
}

#[test]
fn trapping_arithmetic_matches_the_interpreter_or_exits_before() {
    let mut memory = CodeMemory::new().expect("code memory");
    let mut seed = 0x9876_5432_10fe_dcba;
    let mut start = machine(&[0]);
    let values_32 = [
        0u64, 1, 2, 0x7fff_fffe, 0x7fff_ffff, 0x8000_0000, 0x8000_0001,
        0xffff_ffff, 0x1234_5678, 0x8765_4321_0000_0001, 0x1234_5678_7fff_ffff,
        0xabcd_ef01_8000_0000,
    ];
    let values_64 = [
        0u64, 1, 2, 0x7fff_ffff_ffff_fffe, 0x7fff_ffff_ffff_ffff,
        0x8000_0000_0000_0000, 0x8000_0000_0000_0001, u64::MAX,
        0x1234_5678_9abc_def0,
    ];
    let aliases = [(1, 2, 3), (1, 2, 1), (1, 2, 2), (1, 1, 1), (0, 2, 3), (1, 0, 3), (1, 2, 0)];
    let mut native = 0;
    let mut exits = 0;

    for (op, funct, is_64) in [
        (0u32, 0x20u32, false), // ADD
        (0, 0x22, false),       // SUB
        (0x08, 0, false),       // ADDI
        (0, 0x2c, true),        // DADD
        (0, 0x2e, true),        // DSUB
        (0x18, 0, true),        // DADDI
    ] {
        for &(rs, rt, rd) in &aliases {
            let mut words = Vec::new();
            if op == 0 {
                words.push(rs << 21 | rt << 16 | rd << 11 | funct);
            } else {
                for imm in [0, 1, 2, 0x7fff, 0x8000, 0xffff] {
                    words.push(op << 26 | rs << 21 | rt << 16 | imm);
                }
            }
            for word in words {
                let c = block(&[word]).unwrap_or_else(|| panic!("unsupported {word:08x}"));
                let code = place(&mut memory, &c).expect("code space");
                start.write_u32(0x1000, word);
                let values = if is_64 { &values_64[..] } else { &values_32[..] };
                for &v1 in values {
                    for &v2 in values {
                        for r in 1..32 {
                            start.cpu.gpr[r] = random(&mut seed);
                        }
                        start.cpu.gpr[rs as usize] = v1;
                        start.cpu.gpr[rt as usize] = v2;
                        start.cpu.gpr[0] = 0;
                        let label = format!("{word:08x} {v1:016x} {v2:016x}");
                        let completed = compare_either(code, &mut start, &label);
                        if completed == 0 {
                            exits += 1;
                            let mut check = start.clone();
                            check.step_cpu();
                            assert_eq!(
                                (check.cpu.cop0.cause >> 2) & 0x1f,
                                12,
                                "interpreter did not trap on overflow: {label}"
                            );
                        } else {
                            native += 1;
                        }
                    }
                }
            }
        }
    }
    assert!(native > 1000, "native: {native}");
    assert!(exits > 100, "exits: {exits}");
}

#[test]
fn dependent_block_keeps_zero_and_hilo_between_instructions() {
    let words = [
        0x6400_ffff, // daddiu zero, zero, -1 (discard)
        0x6401_ffff, // daddiu at, zero, -1
        0x0020_0011, // mthi at
        0x0020_0013, // mtlo at
        0x0000_1010, // mfhi v0
        0x0000_1812, // mflo v1
        0x0022_082d, // daddu at, at, v0
        0x2421_0001, // addiu at, at, 1
        0x0023_202b, // sltu a0, at, v1
        0x0020_0807, // srav at, zero, at
    ];
    let c = block(&words).unwrap();
    assert_eq!(c.instructions, words.len());
    let mut memory = CodeMemory::new().expect("code memory");
    let code = place(&mut memory, &c).expect("code space");
    compare(code, &mut machine(&words), words.len(), "dependent block");
}

#[test]
fn status_moves_match_the_interpreter() {
    use crate::cop0::{CU0, CU1, ERL, EXL, IE, IP7};
    let mut memory = CodeMemory::new().expect("code memory");
    let mut start = machine(&[0]);
    // MFC0 t0, Status and MTC0 t1, Status; then ANDI v0, t0, 1, which
    // an interrupt let through must not reach.
    let statuses = [0, IE, IE | 0xff00, 0xff00, 0x10, 0x10 | CU0, 0x10 | EXL, ERL | 0x8401, CU1 | 0xff01,
        0x3400_ff01, 0x0008_0000];
    let values: [u64; 15] = [0, 1, 0xff01, 0x8000, 0x8001, 0x0401, 0x10, 0x11, 2, 4, u64::from(CU1 | 1),
        0xffff_ffff_ffff_ffff, 0x2000_ff01, 0x3400_ff01, 0x0058_0000];
    for word in [0x4008_6000, 0x4089_6000] {
        let words = [word, 0x3102_0001];
        let c = block(&words).unwrap();
        let code = place(&mut memory, &c).expect("code space");
        start.write_u32(0x1000, words[0]);
        start.write_u32(0x1004, words[1]);
        for (k, &status) in statuses.iter().enumerate() {
            for &value in &values {
                for cause in [0, IP7, 0x0400] {
                    start.cpu.cop0.status = status;
                    start.cpu.cop0.cause = cause;
                    if start.cpu.cop0.interrupt_pending() {
                        // The dispatcher enters no block then.
                        continue;
                    }
                    start.cpu.gpr[8] = 0x1234_5678 + k as u64;
                    start.cpu.gpr[9] = value;
                    let label = format!("{word:08x}, status {status:08x}, value {value:x}, cause {cause:x}");
                    let mut cpu = start.cpu.clone();
                    let completed = execute(code, &mut cpu, start.rdram.as_mut_ptr());
                    // Without fetching: user mode cannot fetch from kseg0.
                    let mut reference = start.clone();
                    for &word in &words[..completed as usize] {
                        reference.step_cached(decode(word));
                    }
                    if completed > 0 {
                        cpu.finish_linear_block(completed as usize);
                    }
                    let (mut a, mut b) = (Vec::new(), Vec::new());
                    cpu.save(&mut a);
                    reference.cpu.save(&mut b);
                    assert_eq!(a, b, "{label}: {completed}");
                    // Unusable COP0, and a change of anything but IE and
                    // the mask, are the interpreter's; an interrupt let
                    // through stops after the move.
                    let usable = status & 0x18 == 0 || status & (EXL | ERL | CU0) != 0;
                    let new = value as u32 & 0xff57_ffff;
                    let expected = if !usable {
                        0
                    } else if word == 0x4008_6000 {
                        2
                    } else if (new ^ status) & !0xff01 != 0 {
                        0
                    } else if cause & new & 0xff00 != 0 && new & 7 == IE {
                        1
                    } else {
                        2
                    };
                    assert_eq!(completed, expected, "{label}");
                }
            }
        }
    }
}

/// MFC0 and DMFC0 of what the compiler reads natively. A block ends before them if COP0 is not
/// usable; Status's top bit shows that MFC0 extends its sign and DMFC0 does not.
#[cfg(any(target_arch = "aarch64", target_arch = "arm"))]
#[test]
fn cop0_reads_match_the_interpreter() {
    use crate::cop0::{CU0, ERL, EXL};
    let mut memory = CodeMemory::new().expect("code memory");
    let mut start = machine(&[0]);
    let statuses = [0, 0x10, 0x10 | CU0, 0x10 | EXL, 0x10 | ERL, 0x8000_ff00, 0xf400_0000];
    for op in [0x4000_0000u32, 0x4020_0000] {
        // Status, PRId, CacheErr and TagHi, into t0 and into the zero register.
        for rd in [12u32, 15, 27, 29] {
            if cfg!(target_arch = "arm") && op == 0x4020_0000 && rd == 12 {
                continue;
            }
            for rt in [8u32, 0] {
                let word = op | rt << 16 | rd << 11;
                let c = block(&[word]).unwrap();
                let code = place(&mut memory, &c).expect("code space");
                start.write_u32(0x1000, word);
                for status in statuses {
                    start.cpu.cop0.status = status;
                    start.cpu.gpr[8] = 0x1234_5678;
                    let label = format!("{word:08x}, status {status:08x}");
                    let mut cpu = start.cpu.clone();
                    let completed = execute(code, &mut cpu, start.rdram.as_mut_ptr());
                    let usable = status & 0x18 == 0 || status & (EXL | ERL | CU0) != 0;
                    assert_eq!(completed, u32::from(usable), "{label}");
                    // Without fetching: user mode cannot fetch from kseg0.
                    let mut reference = start.clone();
                    if completed > 0 {
                        reference.step_cached(decode(word));
                        cpu.finish_linear_block(completed as usize);
                    }
                    let (mut a, mut b) = (Vec::new(), Vec::new());
                    cpu.save(&mut a);
                    reference.cpu.save(&mut b);
                    assert_eq!(a, b, "{label}");
                }
            }
        }
    }
}

#[test]
fn fpu_moves_loads_and_stores_match_the_interpreter() {
        let mut memory = CodeMemory::new().expect("code memory");
    let mut start = machine(&[0]);
    let mut native_ram = vec![0; crate::bus::RDRAM_SIZE];
    let mut seed = 0x46f0_0d06_1357_9bdf;
    // (word, access size): LWC1, LDC1, SWC1, SDC1 at -8(at), even and
    // odd registers; MFC1, DMFC1, MTC1, DMTC1; MOV.S and MOV.D.
    let mut words = Vec::new();
    for (op, size) in [(0x31, 4), (0x35, 8), (0x39, 4), (0x3d, 8)] {
        for ft in [0, 1, 2, 31] {
            words.push((op << 26 | 1 << 21 | ft << 16 | 0xfff8, size));
        }
    }
    for move_op in [0x00, 0x01, 0x04, 0x05] {
        for (rt, fs) in [(2, 0), (2, 1), (0, 3), (2, 30), (3, 31)] {
            words.push((0x11 << 26 | move_op << 21 | rt << 16 | fs << 11, 0));
        }
    }
    for fmt in [16, 17] {
        for (fs, fd) in [(0, 2), (1, 2), (2, 1), (3, 3), (31, 0)] {
            words.push((0x11 << 26 | fmt << 21 | fs << 11 | fd << 6 | 6, 0));
        }
    }
    for (word, size) in words {
        let c = block(&[word]).unwrap_or_else(|| panic!("{word:08x} not compiled"));
        let code = place(&mut memory, &c).expect("code space");
        start.write_u32(0x1000, word);
        let store = matches!(word >> 26, 0x39 | 0x3d);
        // RDRAM through both segments and at its end; misaligned, a
        // device's register, TLB-mapped: exits before.
        let addresses: Vec<(u64, bool)> = if size == 0 {
            vec![(0, true)]
        } else {
            vec![(0xffff_ffff_8000_4000, true), (0xffff_ffff_a000_4008, true),
                (0xffff_ffff_8000_0000 + (crate::bus::RDRAM_SIZE as u64 - size), true),
                (0xffff_ffff_8000_4000 + size / 2, false), (0xffff_ffff_a404_0010, false), (0x4000, false)]
        };
        for status in [0, FR, CU1, CU1 | FR] {
            for &(address, rdram) in &addresses {
                for sample in 0..if store { 1 } else { 4 } {
                    for r in 1..32 {
                        start.cpu.gpr[r] = random(&mut seed);
                    }
                    for r in 0..32 {
                        start.cpu.fpu.regs[r] = random(&mut seed);
                    }
                    start.cpu.gpr[1] = address.wrapping_add(8);
                    if rdram {
                        let at = (address & 0x7f_fff8) as usize;
                        start.rdram[at..at + 8].copy_from_slice(&random(&mut seed).to_be_bytes());
                    }
                    start.cpu.cop0.status = status;
                    let count = usize::from(status & CU1 != 0 && rdram);
                    let label = format!("{word:08x}, status {status:08x}, address {address:x}, sample {sample}");
                    if store {
                        compare_store(code, &mut start, &mut native_ram, count, &label);
                    } else {
                        compare(code, &mut start, count, &label);
                    }
                }
            }
        }
    }
}

#[test]
fn fpu_block_checks_cu1_once_and_pairs_registers_by_fr() {
        let words = [
        0x24c6_0001, // addiu a2,a2,1
        0x4481_0800, // mtc1 at,f1
        0xc4a2_0000, // lwc1 f2,0(a1)
        0x4600_10c6, // mov.s f3,f2
        0xe4a1_0004, // swc1 f1,4(a1)
        0xd4a4_0008, // ldc1 f4,8(a1)
        0x4422_2000, // dmfc1 v0,f4
        0x4403_1800, // mfc1 v1,f3
        0x44a2_2800, // dmtc1 v0,f5
        0xf4a5_0010, // sdc1 f5,16(a1)
        0x4620_2986, // mov.d f6,f5
    ];
    let c = block(&words).unwrap();
    assert_eq!(c.instructions, words.len());
    let mut memory = CodeMemory::new().expect("code memory");
    let code = place(&mut memory, &c).expect("code space");
    let mut start = machine(&words);
    let mut native_ram = vec![0; crate::bus::RDRAM_SIZE];
    let mut seed = 0x1f2e_3d4c_5b6a_7988;
    for status in [0, FR, CU1, CU1 | FR] {
        for sample in 0..3 {
            for r in 1..32 {
                start.cpu.gpr[r] = random(&mut seed);
            }
            for r in 0..32 {
                start.cpu.fpu.regs[r] = random(&mut seed);
            }
            start.cpu.gpr[5] = 0xffff_ffff_8000_4000;
            for at in (0x4000..0x4020).step_by(8) {
                start.rdram[at..at + 8].copy_from_slice(&random(&mut seed).to_be_bytes());
            }
            start.cpu.cop0.status = status;
            let count = if status & CU1 != 0 { words.len() } else { 1 };
            compare_store(code, &mut start, &mut native_ram, count, &format!("status {status:08x}, sample {sample}"));
        }
    }
}

#[test]
fn fpu_conversions_to_words_match_the_interpreter_or_exit_before() {
        let mut memory = CodeMemory::new().expect("code memory");
    let mut start = machine(&[0]);
    let mut seed = 0x7a0c_c0a7_2026_1030;
    // Zeros, ties, fractions, the edges of 2^23, 2^24 and 2^31,
    // denormals, infinities, NaNs.
    let specials: [u32; 24] = [0, 0x8000_0000, 0x3f00_0000, 0xbf00_0000, 0x4020_0000, 0x4060_0000, 0xc020_0000,
        0x3fc0_0000, 0x3f80_0001, 0x4b00_0000, 0x4b00_0001, 0x4b7f_ffff, 0x4b80_0000, 0x4eff_ffff, 0xceff_ffff,
        0x4f00_0000, 0xcf00_0000, 0x4f00_0001, 0x0000_0001, 0x807f_ffff, 0x7f80_0000, 0xff80_0000, 0x7fc0_0000,
        0x7fbf_ffff];
    let (mut native, mut cases) = (0, 0);
    let functs: &[u32] = if cfg!(target_arch = "aarch64") {
        &[8, 9, 10, 11, 12, 13, 14, 15, 36, 37]
    } else {
        &[12, 13, 14, 15, 36]
    };
    for &funct in functs {
        for (fs, fd) in [(2u32, 4u32), (2, 2), (1, 3), (3, 1), (0, 31)] {
            let word = 0x4600_0000 | fs << 11 | fd << 6 | funct;
            let c = block(&[word]).unwrap();
            let code = place(&mut memory, &c).expect("code space");
            start.write_u32(0x1000, word);
            for status in [CU1, CU1 | FR, 0] {
                for fcr31 in [0, 0x0003_f07c, 1, 2, 3, 0x80, 0x0100_0800, 0x0100_0801, 0xf00] {
                    for case in 0..64 {
                        for r in 0..32 {
                            start.cpu.fpu.regs[r] = random(&mut seed);
                        }
                        let a = match case {
                            0..24 => specials[case],
                            24..48 => {
                                let bits = random(&mut seed) as u32;
                                bits & 0x807f_ffff | (110 + (bits >> 8) % 50) << 23
                            }
                            _ => random(&mut seed) as u32,
                        };
                        let fs = fs as usize;
                        let r = fs_read(fs, status);
                        start.cpu.fpu.regs[r] = start.cpu.fpu.regs[r] & !0xffff_ffff | u64::from(a);
                        start.cpu.cop0.status = status;
                        start.cpu.fpu.fcr31 = fcr31;
                        native += compare_either(code, &mut start,
                            &format!("{word:08x} status {status:08x} fcr31 {fcr31:08x} {a:08x}"));
                        cases += 1;
                    }
                }
            }
        }
    }
    assert!(native * 4 > cases, "{native} of {cases}");
}

#[cfg(target_arch = "aarch64")]
#[test]
fn fpu_double_conversions_to_words_match_the_interpreter_or_exit_before() {
    let mut memory = CodeMemory::new().expect("code memory");
    let mut start = machine(&[0]);
    let mut seed = 0x7a0c_c0a7_2026_1031;
    // Zeros, ties, fractions, 2^31 boundaries, denormals, infinities, NaNs.
    let specials: [u64; 24] = [
        0,
        1 << 63,
        0x3fe0_0000_0000_0000, // 0.5
        0xbfe0_0000_0000_0000, // -0.5
        0x3ff8_0000_0000_0000, // 1.5
        0x4004_0000_0000_0000, // 2.5
        0xc004_0000_0000_0000, // -2.5
        0x41df_ffff_ffc0_0000, // 2147483647.0
        0x41df_ffff_ffe0_0000, // 2147483647.5
        0x41df_ffff_fff0_0000, // 2147483647.75
        0x41df_ffff_ffff_ffff, // 2147483647.999...
        0x41e0_0000_0000_0000, // 2147483648.0 (2^31)
        0xc1df_ffff_ffc0_0000, // -2147483647.0
        0xc1df_ffff_ffe0_0000, // -2147483647.5
        0xc1df_ffff_fff0_0000, // -2147483647.75
        0xc1e0_0000_0000_0000, // -2147483648.0 (-2^31)
        0xc1e0_0000_0000_0001, // -2147483648.000...
        0x0000_0000_0000_0001, // denormal
        0x7ff0_0000_0000_0000, // +inf
        0xfff0_0000_0000_0000, // -inf
        0x7ff8_0000_0000_0000, // quiet NaN
        0x7ff7_ffff_ffff_ffff, // signaling NaN
        0x4190_0000_0000_0000, // 16777216.0
        0x4059_0000_0000_0000, // 100.0
    ];
    let (mut native, mut cases) = (0, 0);
    let functs: &[u32] = if cfg!(target_arch = "aarch64") {
        &[8, 9, 10, 11, 12, 13, 14, 15, 36, 37]
    } else {
        &[12, 13, 14, 15, 36]
    };
    for &funct in functs {
        for (fs, fd) in [(2u32, 4u32), (2, 2), (1, 3), (3, 1), (0, 31)] {
            let word = 0x4620_0000 | fs << 11 | fd << 6 | funct;
            let c = block(&[word]).unwrap();
            let code = place(&mut memory, &c).expect("code space");
            start.write_u32(0x1000, word);
            for status in [CU1, CU1 | FR, 0] {
                for fcr31 in [0, 0x0003_f07c, 1, 2, 3, 0x80, 0x0100_0800, 0x0100_0801, 0xf00] {
                    for case in 0..46 {
                        for r in 0..32 {
                            start.cpu.fpu.regs[r] = random(&mut seed);
                        }
                        let a = match case {
                            0..24 => specials[case],
                            24..38 => {
                                // Magnitude within [0.5, 2^30]
                                let bits = random(&mut seed);
                                bits & 0x800f_ffff_ffff_ffff | (1000 + (bits >> 32) % 50) << 52
                            }
                            _ => random(&mut seed),
                        };
                        let fs = fs as usize;
                        start.cpu.fpu.regs[fs_read(fs, status)] = a;
                        start.cpu.cop0.status = status;
                        start.cpu.fpu.fcr31 = fcr31;
                        native += compare_either(code, &mut start,
                            &format!("{word:08x} status {status:08x} fcr31 {fcr31:08x} {a:016x}"));
                        cases += 1;
                    }
                }
            }
        }
    }
    assert!(native * 4 > cases, "{native} of {cases}");
}

#[test]
fn fpu_double_signs_match_the_interpreter_or_exit_before() {
    let mut memory = CodeMemory::new().expect("code memory");
    let mut start = machine(&[0]);
    let mut seed = 0x51a6_d0b1_e202_6110;
    // Zeros, ones, the largest and smallest normals, denormals, infinities, NaNs.
    let specials: [u64; 12] = [0, 1 << 63, 0x3ff0_0000_0000_0000, 0xbff0_0000_0000_0000, 0x7fef_ffff_ffff_ffff,
        0x0010_0000_0000_0000, 1, 0x800f_ffff_ffff_ffff, 0x7ff0_0000_0000_0000, 0xfff0_0000_0000_0000,
        0x7ff8_0000_0000_0000, 0x7ff7_ffff_ffff_ffff];
    let (mut native, mut cases) = (0, 0);
    for funct in [5u32, 7] {
        for (fs, fd) in [(2u32, 4u32), (2, 2), (1, 3), (3, 1), (0, 31)] {
            let word = 0x4620_0000 | fs << 11 | fd << 6 | funct;
            let c = block(&[word]).unwrap();
            let code = place(&mut memory, &c).expect("code space");
            start.write_u32(0x1000, word);
            for status in [CU1, CU1 | FR, 0] {
                for fcr31 in [0, 0x0003_f07c, 0x0100_0000, 0xf00] {
                    for case in 0..24 {
                        for r in 0..32 {
                            start.cpu.fpu.regs[r] = random(&mut seed);
                        }
                        let a = if case < 12 { specials[case] } else { random(&mut seed) };
                        start.cpu.fpu.regs[fs_read(fs as usize, status)] = a;
                        start.cpu.cop0.status = status;
                        start.cpu.fpu.fcr31 = fcr31;
                        native += compare_either(code, &mut start,
                            &format!("{word:08x} status {status:08x} fcr31 {fcr31:08x} {a:016x}"));
                        cases += 1;
                    }
                }
            }
        }
    }
    assert!(native * 4 > cases, "{native} of {cases}");
}

#[test]
fn fpu_compares_and_word_conversions_match_the_interpreter_or_exit_before() {
        let mut memory = CodeMemory::new().expect("code memory");
    let mut start = machine(&[0]);
    let mut seed = 0xc0c0_7e57_0bad_cafe;
    let specials: [u32; 18] = [0, 0x8000_0000, 0x3f80_0000, 0xbf80_0000, 0x7f7f_ffff, 0x0080_0000,
        0x0000_0001, 0x807f_ffff, 0x7f80_0000, 0xff80_0000, 0x7fc0_0000, 0x7fbf_ffff, 0xffff_ffff,
        0x7fff_ffff, 0x0100_0001, 0x00ff_ffff, 0x4b80_0000, 0x3eaa_aaab];
    let (mut native, mut cases) = (0, 0);
    // Every C.cond.S, then CVT.S.W, NEG.S, ABS.S, CVT.D.S and CVT.D.W (and CVT.S.L / CVT.D.L on A64).
    let words = (48..64u32).map(|funct| 0x4600_0000 | funct)
        .chain([0x4680_0020, 0x4600_0007, 0x4600_0005, 0x4600_0021, 0x4680_0021])
        .chain(if cfg!(target_arch = "aarch64") { &[0x46a0_0020, 0x46a0_0021][..] } else { &[][..] }.iter().copied());
    for base in words {
        for (fs, ft, fd) in [(2u32, 4u32, 6u32), (2, 2, 2), (1, 4, 3), (3, 3, 1), (0, 31, 31)] {
            let word = base | ft << 16 | fs << 11 | fd << 6;
            let c = block(&[word]).unwrap();
            let code = place(&mut memory, &c).expect("code space");
            start.write_u32(0x1000, word);
            for status in [CU1, CU1 | FR, 0] {
                for fcr31 in [0, 0x0003_f07c, 1, 3, 0x80, 0x0100_0000, 0x0080_0000, 0x0080_f000, 0xf00] {
                    for case in 0..48 {
                        for r in 0..32 {
                            start.cpu.fpu.regs[r] = random(&mut seed);
                        }
                        let (a, b) = match case {
                            0..36 => (specials[case % 18], specials[(case * 7 + case / 18) % 18]),
                            36..42 => {
                                let a = random(&mut seed) as u32;
                                (a, a)
                            }
                            _ => (random(&mut seed) as u32, random(&mut seed) as u32),
                        };
                        let (fs, ft) = (fs as usize, ft as usize);
                        let r = fs_read(fs, status);
                        start.cpu.fpu.regs[r] = start.cpu.fpu.regs[r] & !0xffff_ffff | u64::from(a);
                        start.cpu.fpu.regs[ft] = start.cpu.fpu.regs[ft] & !0xffff_ffff | u64::from(b);
                        start.cpu.cop0.status = status;
                        start.cpu.fpu.fcr31 = fcr31;
                        native += compare_either(code, &mut start,
                            &format!("{word:08x} status {status:08x} fcr31 {fcr31:08x} {a:08x} {b:08x}"));
                        cases += 1;
                    }
                }
            }
        }
    }
    assert!(native * 3 > cases, "{native} of {cases}");
    // Every C.cond.D, on doubles.
    let specials: [u64; 14] = [0, 1 << 63, 0x3ff0_0000_0000_0000, 0xbff0_0000_0000_0000, 0x7fef_ffff_ffff_ffff,
        0x0010_0000_0000_0000, 1, 0x7ff0_0000_0000_0000, 0xfff0_0000_0000_0000, 0x7ff8_0000_0000_0000,
        0x7ff0_0000_0000_0001, 0x7ff7_ffff_ffff_ffff, 0x3ff0_0000_0000_0001, 0xbfd5_5555_5555_5555];
    let (mut native, mut cases) = (0, 0);
    for funct in 48..64u32 {
        for (fs, ft) in [(2u32, 4u32), (2, 2), (1, 4), (3, 3), (0, 31)] {
            let word = 0x4620_0000 | ft << 16 | fs << 11 | funct;
            let c = block(&[word]).unwrap();
            let code = place(&mut memory, &c).expect("code space");
            start.write_u32(0x1000, word);
            for status in [CU1, CU1 | FR, 0] {
                for fcr31 in [0, 0x0003_f07c, 0x0080_f000] {
                    for case in 0..40 {
                        for r in 0..32 {
                            start.cpu.fpu.regs[r] = random(&mut seed);
                        }
                        let (a, b) = match case {
                            0..28 => (specials[case % 14], specials[(case * 5 + case / 14) % 14]),
                            28..32 => {
                                let a = random(&mut seed);
                                (a, a)
                            }
                            _ => (random(&mut seed), random(&mut seed)),
                        };
                        let (fs, ft) = (fs as usize, ft as usize);
                        start.cpu.fpu.regs[fs_read(fs, status)] = a;
                        start.cpu.fpu.regs[ft] = b;
                        start.cpu.cop0.status = status;
                        start.cpu.fpu.fcr31 = fcr31;
                        native += compare_either(code, &mut start,
                            &format!("{word:08x} status {status:08x} fcr31 {fcr31:08x} {a:016x} {b:016x}"));
                        cases += 1;
                    }
                }
            }
        }
    }
    assert!(native * 3 > cases, "{native} of {cases}");
}

#[test]
fn fpu_from_long_edge_cases_match_the_interpreter_or_exit_before() {
    if !cfg!(target_arch = "aarch64") {
        return;
    }
    let mut memory = CodeMemory::new().expect("code memory");
    let mut start = machine(&[0]);
    let mut seed = 0x1234_5678_9abc_def0;
    let values: [i64; 16] = [
        0, 1, -1,
        (1 << 53) - 1, -(1 << 53) + 1,
        1 << 53, -(1 << 53),
        (1 << 53) + 1, -(1 << 53) - 1,
        (1 << 54) + (1 << 30) + 1, -(1 << 54) - (1 << 30) - 1,
        (1 << 55) - 1, -(1 << 55),
        1 << 55, -(1 << 55) - 1,
        i64::MIN,
    ];
    let (mut native, mut cases) = (0, 0);
    // CVT.S.L (0x46a0_0020) and CVT.D.L (0x46a0_0021)
    for funct in [32u32, 33] {
        for (fs, fd) in [(2u32, 4u32), (2, 2), (1, 3), (3, 1), (0, 31)] {
            let word = 0x46a0_0000 | fs << 11 | fd << 6 | funct;
            let c = block(&[word]).unwrap();
            let code = place(&mut memory, &c).expect("code space");
            start.write_u32(0x1000, word);
            for status in [CU1, CU1 | FR, 0] {
                for fcr31 in [0, 0x0003_f07c, 1, 2, 3, 0x80, 0x0100_0800, 0xf00] {
                    for &val in &values {
                        for r in 0..32 {
                            start.cpu.fpu.regs[r] = random(&mut seed);
                        }
                        let fs = fs as usize;
                        start.cpu.fpu.regs[fs_read(fs, status)] = val as u64;
                        start.cpu.cop0.status = status;
                        start.cpu.fpu.fcr31 = fcr31;
                        native += compare_either(code, &mut start,
                            &format!("{word:08x} status {status:08x} fcr31 {fcr31:08x} {val:#x}"));
                        cases += 1;
                    }
                }
            }
        }
    }
    assert!(native * 4 > cases, "{native} of {cases}");
}

#[test]
fn fpu_double_arithmetic_matches_the_interpreter_or_exits_before() {
        let mut memory = CodeMemory::new().expect("code memory");
    let mut start = machine(&[0]);
    let mut seed = 0xd0b1_e0a7_1234_5678;
    // Zeros, ones, the largest and smallest normals, denormals,
    // infinities, NaNs, thirds, values whose products or quotients
    // leave the range or come near 2^-967, and singles' edges.
    let specials: [u64; 20] = [0, 1 << 63, 0x3ff0_0000_0000_0000, 0xbff0_0000_0000_0000,
        0x7fef_ffff_ffff_ffff, 0x0010_0000_0000_0000, 0x0000_0000_0000_0001, 0x7ff0_0000_0000_0000,
        0x7ff8_0000_0000_0000, 0x7ff0_0000_0000_0001, 0x3fd5_5555_5555_5555, 0x4008_0000_0000_0000,
        0x2000_0000_0000_0000, 0x5ff0_0000_0000_0000, 0x0380_0000_0000_0000, 0x0370_0000_0000_0001,
        0x3ff0_0000_0000_0001, 0x47ef_ffff_e000_0000, 0x47ef_ffff_f000_0000, 0x3810_0000_0000_0000];
    // Doubles just below the smallest normal single that the VFP
    // rounds up to it: 2^-126 (1 - 0.75·2^-24), tiny for the VR4300,
    // and 2^-126 (1 - 2^-26), not.
    let least: [u64; 4] = [0x380f_ffff_e800_0000, 0xb80f_ffff_e800_0000, 0x380f_ffff_f800_0000,
        0xb80f_ffff_f800_0000];
    let (mut native, mut cases) = (0, 0);
    // ADD.D, SUB.D, MUL.D, DIV.D, SQRT.D, then CVT.S.D.
    for &funct in &[0u32, 1, 2, 3, 4, 32] {
        for (fs, ft, fd) in [(2u32, 4u32, 6u32), (2, 2, 2), (1, 4, 3), (3, 3, 1), (0, 31, 31)] {
            let word = 0x4620_0000 | ft << 16 | fs << 11 | fd << 6 | funct;
            let c = block(&[word]).unwrap();
            let code = place(&mut memory, &c).expect("code space");
            start.write_u32(0x1000, word);
            for status in [CU1, CU1 | FR, 0] {
                for fcr31 in [0, 0x0003_f07c, 1, 3, 0x80, 0x0100_0000, 0x0080_0000, 0x0100_0800, 0xf00] {
                    for case in 0..68 {
                        for r in 0..32 {
                            start.cpu.fpu.regs[r] = random(&mut seed);
                        }
                        let normal = |seed: &mut u64| {
                            let bits = random(seed);
                            bits & 0x800f_ffff_ffff_ffff | (900 + (bits >> 32) % 250) << 52
                        };
                        let (a, b) = match case {
                            0..40 => (specials[case % 20], specials[(case * 7 + case / 20) % 20]),
                            40..58 => (normal(&mut seed), normal(&mut seed)),
                            64.. => (least[case - 64], 0x3ff0_0000_0000_0000),
                            _ => (random(&mut seed), random(&mut seed)),
                        };
                        let (fs, ft) = (fs as usize, ft as usize);
                        start.cpu.fpu.regs[fs_read(fs, status)] = a;
                        start.cpu.fpu.regs[ft] = b;
                        start.cpu.cop0.status = status;
                        start.cpu.fpu.fcr31 = fcr31;
                        native += compare_either(code, &mut start,
                            &format!("{word:08x} status {status:08x} fcr31 {fcr31:08x} {a:016x} {b:016x}"));
                        cases += 1;
                    }
                }
            }
        }
    }
    assert!(native * 4 > cases, "{native} of {cases}");
}

#[test]
fn fpu_arithmetic_matches_the_interpreter_or_exits_before() {
        let mut memory = CodeMemory::new().expect("code memory");
    let mut start = machine(&[0]);
    let mut seed = 0xf0a0_ad05_0123_4567;
    // Zeros, ones, the largest and smallest normals, denormals,
    // infinities, NaNs (a MIPS one signals with the top fraction
    // bit set), thirds, and values whose products leave the range.
    let specials: [u32; 22] = [0, 0x8000_0000, 0x3f80_0000, 0xbf80_0000, 0x7f7f_ffff, 0xff7f_ffff,
        0x0080_0000, 0x8080_0000, 0x0080_0001, 0x0000_0001, 0x807f_ffff, 0x7f80_0000, 0xff80_0000,
        0x7fc0_0000, 0x7fbf_ffff, 0x4000_0000, 0x3eaa_aaab, 0x1f80_0000, 0x5f80_0000, 0x0100_0000,
        0x3f80_0001, 0x4b80_0000];
    // Products just below the smallest normal that the VFP rounds up
    // to it: 2^-126 (1 - 0.61·2^-24), tiny for the VR4300, and
    // 2^-126 (1 - 2^-46), not.
    let least: [(u32, u32); 4] = [(0x3f7f_f380, 0x0080_0640), (0x8080_0640, 0x3f7f_f380),
        (0x3f7f_fffe, 0x0080_0001), (0x0080_0001, 0xbf7f_fffe)];
    let (mut native, mut cases) = (0, 0);
    for funct in 0..5u32 {
        for (fs, ft, fd) in [(2u32, 4u32, 6u32), (2, 2, 2), (1, 4, 3), (3, 3, 1), (0, 31, 31)] {
            let word = 0x4600_0000 | ft << 16 | fs << 11 | fd << 6 | funct;
            let c = block(&[word]).unwrap();
            let code = place(&mut memory, &c).expect("code space");
            start.write_u32(0x1000, word);
            for status in [CU1, CU1 | FR, 0] {
                // libultra's FS and EV, each enable but inexact's.
                for fcr31 in [0, 0x0003_f07c, 1, 3, 0x80, 0x0100_0000, 0x0080_0000, 0x0100_0800, 0xf00] {
                    for case in 0..100 {
                        for r in 0..32 {
                            start.cpu.fpu.regs[r] = random(&mut seed);
                        }
                        let normal = |seed: &mut u64| {
                            let bits = random(seed) as u32;
                            bits & 0x807f_ffff | (90 + (bits >> 8) % 76) << 23
                        };
                        let (a, b) = match case {
                            0..44 => (specials[case % 22], specials[(case * 7 + case / 22) % 22]),
                            44..80 => (normal(&mut seed), normal(&mut seed)),
                            96.. => least[case - 96],
                            _ => (random(&mut seed) as u32, random(&mut seed) as u32),
                        };
                        let (fs, ft) = (fs as usize, ft as usize);
                        let r = fs_read(fs, status);
                        start.cpu.fpu.regs[r] = start.cpu.fpu.regs[r] & !0xffff_ffff | u64::from(a);
                        start.cpu.fpu.regs[ft] = start.cpu.fpu.regs[ft] & !0xffff_ffff | u64::from(b);
                        start.cpu.cop0.status = status;
                        start.cpu.fpu.fcr31 = fcr31;
                        native += compare_either(code, &mut start,
                            &format!("{word:08x} status {status:08x} fcr31 {fcr31:08x} {a:08x} {b:08x}"));
                        cases += 1;
                    }
                }
            }
        }
    }
    // Most of the ordinary cases ran natively.
    assert!(native * 4 > cases, "{native} of {cases}");
}

#[test]
fn fpu_control_and_cache_match_the_interpreter_or_exit_before() {
    use crate::cop0::{CU0, CU1, ERL, EXL};
    let mut memory = CodeMemory::new().expect("code memory");
    let mut start = machine(&[0]);
    let mut seed = 0xc7c1_cfc1_bc00_0000;
    // CFC1 v0/zero from FCR31, FCR0, another; CTC1 v0/zero to FCR31,
    // another; CACHE.
    let words = [0x4442_f800, 0x4440_f800, 0x4442_0000, 0x4442_2800, 0x44c2_f800, 0x44c0_f800, 0x44c2_2800,
        0xbc22_0000];
    // To FCR31: nothing, every writable bit, inexact's cause alone
    // and enabled, E's cause, everything.
    let values = [0u64, 0x0183_ffff, 0x1000, 0x1080, 0x2_0000, u64::MAX, 0x0100_0003];
    for word in words {
        let c = block(&[word]).unwrap();
        let code = place(&mut memory, &c).expect("code space");
        start.write_u32(0x1000, word);
        for status in [0, CU1, CU0, EXL | 0x10, ERL | 0x08, EXL | 0x10 | CU1] {
            for (n, &value) in values.iter().enumerate() {
                start.cpu.gpr[2] = value;
                start.cpu.fpu.fcr31 = random(&mut seed) as u32 & 0x0183_ffff & !0x0003_f000;
                start.cpu.cop0.status = status;
                compare_either(code, &mut start, &format!("{word:08x} status {status:08x} value {n}"));
            }
        }
        // Outside kernel mode CACHE exits unless CU0 is set (the
        // interpreter could not fetch there from kseg0 to compare).
        if word >> 26 == 0x2f {
            for (status, completed) in [(0x10, 0), (0x08, 0), (0x10 | CU0, 1)] {
                let mut cpu = start.cpu.clone();
                cpu.cop0.status = status;
                assert_eq!(execute(code, &mut cpu, start.rdram.as_mut_ptr()), completed, "status {status:x}");
            }
        }
    }
}

/// Whether a base is one the guards take: on 32-bit ARM a sign-extended word,
/// as one just below kseg0 (0xffff_ffff_7fff_xxxx), which an offset carries
/// into kseg0, leaves the access to the interpreter; on 64-bit ARM any,
/// whose whole sum they check.
fn word_base(base: u64) -> bool {
    cfg!(target_arch = "aarch64") || base == base as i32 as u64
}

#[test]
fn ordinary_rdram_loads_match_big_endian_interpreter() {
    let mut memory = CodeMemory::new().expect("code memory");
    let mut start = machine(&[0]);
    let mut seed = 0x807f_fff8_0400_1234;
    for (op, size) in [(0x20, 1), (0x24, 1), (0x21, 2), (0x25, 2), (0x23, 4), (0x27, 4), (0x37, 8)] {
        for (rs, rt) in [(1, 2), (1, 1), (1, 0)] {
            for immediate in [0i16, 1, -1, 32767, -32768] {
                let word = op << 26 | rs << 21 | rt << 16 | immediate as u16 as u32;
                let c = block(&[word]).unwrap();
                assert_eq!(c.instructions, 1);
                let code = place(&mut memory, &c).expect("code space");
                start.write_u32(0x1000, word);
                for segment in [0xffff_ffff_8000_0000u64, 0xffff_ffff_a000_0000] {
                    for physical in [0, 0x4000, 0x4000 + size, crate::bus::RDRAM_SIZE as u32 - size] {
                        for sample in 0..12 {
                            let value: u64 = match sample {
                                0 => 0,
                                1 => u64::MAX,
                                2 => 0x8000_0000_0000_0000,
                                3 => 0x7fff_ffff_ffff_ffff,
                                4 => 0x1234_5678_9abc_def0,
                                _ => random(&mut seed),
                            };
                            let aligned = (physical & !7) as usize;
                            start.rdram[aligned..aligned + 8].copy_from_slice(&value.to_be_bytes());
                            start.cpu.gpr[1] = (segment | u64::from(physical)).wrapping_sub(immediate as i64 as u64);
                            start.cpu.gpr[2] = 0x1234_5678_9abc_def0;
                            let count = usize::from(word_base(start.cpu.gpr[1]));
                            compare(code, &mut start, count,
                                &format!("load {word:08x}, segment {segment:x}, physical {physical:x}, sample {sample}"));
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn load_guards_exit_before_faults_devices_and_following_effects() {
    let mut memory = CodeMemory::new().expect("code memory");
    let mut start = machine(&[0; 5]);
    for (op, size) in [(0x20, 1), (0x24, 1), (0x21, 2), (0x25, 2), (0x23, 4), (0x27, 4), (0x37, 8)] {
        for (base, immediate) in [
            (0x0000_0000_8000_4000u64, 0i16), // unextended kseg0
            (0x8000_0000_0000_4000, 0), // xkphys is left to the interpreter
            (0x0000_0000_0000_4000, 0), // TLB-mapped
            (0xffff_ffff_c000_4000, 0), // kseg2
            (0xffff_ffff_a404_001c, 0), // SP semaphore (read has a side effect)
            (0xffff_ffff_8080_0000, 0), // first byte outside RDRAM
            (0xffff_ffff_ffff_fffe, 2), // wrapping 64-bit address
            (0x0000_0000_7fff_ffff, 1), // cannot truncate the high word
            (0xffff_ffff_bfff_ffff, 1), // crosses the direct-mapping boundary
            (0x0000_0000_0000_0010, -32),
        ] {
            for rt in [0, 3] {
                for prefix in [0, 2] {
                    let load = op << 26 | 1 << 21 | rt << 16 | immediate as u16 as u32;
                    let mut words = vec![0x6402_0005, 0x0040_0011]; // daddiu v0,zero,5; mthi v0
                    words.truncate(prefix);
                    words.extend([load, 0x0040_0013, 0x6403_0063]); // load; mtlo v0; daddiu v1,zero,99
                    let c = block(&words).unwrap();
                    let code = place(&mut memory, &c).expect("code space");
                    for (index, &word) in words.iter().enumerate() {
                        start.write_u32(0x1000 + 4 * index as u32, word);
                    }
                    start.cpu.gpr[1] = base;
                    start.cpu.gpr[2] = 0x7654_3210_ffff_ffff;
                    start.cpu.gpr[3] = 0xdead_beef_5555_aaaa;
                    start.cpu.hi = 0x1234_5678_8765_4321;
                    start.cpu.lo = 0xfedc_ba98_0011_2233;
                    compare(code, &mut start, prefix,
                        &format!("guard {load:08x}, base {base:x}, prefix {prefix}"));
                }
            }
        }
        if size > 1 {
            let word = op << 26 | 1 << 21 | 3 << 16;
            start.write_u32(0x1000, word);
            start.cpu.gpr[1] = 0xffff_ffff_8000_4001;
            let c = block(&[word]).unwrap();
            let code = place(&mut memory, &c).expect("code space");
            compare(code, &mut start, 0, "misaligned RDRAM load");
        }
        // Source register zero still takes the guard, even when the
        // destination is zero too; the interpreter must raise TLBL.
        let word = op << 26;
        start.write_u32(0x1000, word);
        let c = block(&[word]).unwrap();
        let code = place(&mut memory, &c).expect("code space");
        compare(code, &mut start, 0, "zero base and destination");
    }
}

#[test]
fn loads_chain_through_changed_bases_and_overlapping_destinations() {
    let words = [
        0x8c84_0000, // lw a0,0(a0): loads the next pointer into its base
        0x8c82_0000, // lw v0,0(a0)
        0x6484_0008, // daddiu a0,a0,8
        0xdc83_0000, // ld v1,0(a0)
        0x0043_2825, // or a1,v0,v1
    ];
    let mut start = machine(&words);
    start.cpu.gpr[4] = 0xffff_ffff_a000_4000;
    start.write_u32(0x4000, 0x8000_4008);
    start.write_u32(0x4008, 0x89ab_cdef);
    start.write_u64(0x4010, 0x1234_5678_9abc_def0);
    let c = block(&words).unwrap();
    assert_eq!(c.instructions, words.len());
    let mut memory = CodeMemory::new().expect("code memory");
    let code = place(&mut memory, &c).expect("code space");
    compare(code, &mut start, words.len(), "dependent loads");
}

#[test]
fn accesses_to_pages_the_rdp_may_still_draw_exit_before_them() {
    let words = [
        0x2421_0001, // addiu at,at,1
        0x8c82_0000, // lw v0,0(a0)
        0xac82_0004, // sw v0,4(a0)
        0x2421_0001, // addiu at,at,1
    ];
    let c = block(&words).unwrap();
    assert_eq!(c.instructions, words.len());
    let mut memory = CodeMemory::new().expect("code memory");
    let code = place(&mut memory, &c).expect("code space");
    let mut start = machine(&words);
    start.write_u32(0x4000, 0x1234_5678);
    start.cpu.gpr[4] = 0xffff_ffff_8000_4000;
    // The marks are by piece (`rdp_thread::MARK`); these mark whole
    // 4 KB pages.
    let mut pages = vec![0u32; crate::rdp_thread::MARKS];
    let per_page = 12 - crate::rdp_thread::MARK;
    let pieces = |page: usize| page << per_page..(page + 1) << per_page;
    // Batch 7 may reach page 4; the loads and stores elsewhere, and
    // there once batch 7 is known drawn, go ahead.
    pages[pieces(4)].fill(7);
    pages[pieces(3)].fill(9);
    pages[pieces(5)].fill(9);
    for (known, completed) in [(6, 1), (7, 4), (8, 4)] {
        let mut cpu = start.cpu.clone();
        let ram = start.rdram.as_mut_ptr();
        assert_eq!(execute_marked(code, &mut cpu, ram, &pages, known), completed, "known {known}");
        assert_eq!(cpu.gpr[1], start.cpu.gpr[1] + if completed == 1 { 1 } else { 2 });
    }
    // Only the store's page marked: the load runs, the store waits.
    pages[pieces(4)].fill(0);
    start.cpu.gpr[4] = 0xffff_ffff_8000_4ffc;
    start.write_u32(0x4ffc, 0x1234_5678);
    pages[pieces(5)].fill(3);
    let mut cpu = start.cpu.clone();
    assert_eq!(execute_marked(code, &mut cpu, start.rdram.as_mut_ptr(), &pages, 2), 2);
    assert_eq!(cpu.gpr[2], 0x1234_5678);
    assert_eq!(start.read_u32(0x5000), 0);
}

#[test]
fn terminal_stores_match_independent_big_endian_ram() {
    let mut memory = CodeMemory::new().expect("code memory");
    let mut start = machine(&[0]);
    let mut native_ram = vec![0; crate::bus::RDRAM_SIZE];
    let mut seed = 0xa07f_fff8_1234_5678;
    let mut sample = 0;
    for (op, size) in [(0x28, 1), (0x29, 2), (0x2b, 4), (0x3f, 8)] {
        for (rs, rt) in [(1, 2), (1, 1), (1, 0)] {
            for immediate in [0i16, 1, -1, 32767, -32768] {
                let word = op << 26 | rs << 21 | rt << 16 | immediate as u16 as u32;
                let c = block(&[word]).unwrap();
                assert_eq!(c.instructions, 1);
                let code = place(&mut memory, &c).expect("code space");
                start.write_u32(0x1000, word);
                for segment in [0xffff_ffff_8000_0000u64, 0xffff_ffff_a000_0000] {
                    for physical in [0, 0x4000 + size, crate::bus::RDRAM_SIZE as u32 - size] {
                        // Cover all data patterns without multiplying
                        // every case by another complete RAM copy.
                        let value = match sample % 7 {
                            0 => 0,
                            1 => u64::MAX,
                            2 => 0x8000_0000_0000_0000,
                            3 => 0x7fff_ffff_ffff_ffff,
                            4 => 0x1234_5678_9abc_def0,
                            _ => random(&mut seed),
                        };
                        sample += 1;
                        let aligned = (physical & !7) as usize;
                        start.rdram[aligned..aligned + 8].fill(0x5a);
                        start.cpu.gpr[1] = (segment | u64::from(physical)).wrapping_sub(immediate as i64 as u64);
                        if rt != 1 && rt != 0 {
                            start.cpu.gpr[rt as usize] = value;
                        }
                        let count = usize::from(word_base(start.cpu.gpr[1]));
                        compare_store(code, &mut start, &mut native_ram, count,
                            &format!("store {word:08x}, segment {segment:x}, physical {physical:x}, value {value:x}"));
                    }
                }
            }
        }
    }
}

#[test]
fn guarded_stores_do_not_change_ram_or_execute_the_tail() {
    let mut memory = CodeMemory::new().expect("code memory");
    let mut start = machine(&[0; 5]);
    let mut native_ram = vec![0; crate::bus::RDRAM_SIZE];
    for (op, size) in [(0x28, 1), (0x29, 2), (0x2b, 4), (0x3f, 8)] {
        for (base, immediate) in [
            (0x0000_0000_8000_4000u64, 0i16),
            (0x8000_0000_0000_4000, 0),
            (0x0000_0000_0000_4000, 0),
            (0xffff_ffff_c000_4000, 0),
            (0xffff_ffff_a430_000c, 0), // MI interrupt mask, not RAM
            (0xffff_ffff_8080_0000, 0),
            (0xffff_ffff_ffff_fffe, 2),
            (0x0000_0000_7fff_ffff, 1),
            (0xffff_ffff_bfff_ffff, 1),
            (0x0000_0000_0000_0010, -32),
        ] {
            for rt in [0, 3] {
                for prefix in [0, 2] {
                    let store = op << 26 | 1 << 21 | rt << 16 | immediate as u16 as u32;
                    let mut words = vec![0x6402_0005, 0x0040_0011];
                    words.truncate(prefix);
                    words.extend([store, 0x0040_0013, 0x6403_0063]);
                    let c = block(&words).unwrap();
                    assert_eq!(c.instructions, prefix + 3);
                    let code = place(&mut memory, &c).expect("code space");
                    for (index, &word) in words.iter().enumerate() {
                        start.write_u32(0x1000 + 4 * index as u32, word);
                    }
                    start.cpu.gpr[1] = base;
                    start.cpu.gpr[2] = 0x7654_3210_ffff_ffff;
                    start.cpu.gpr[3] = 0xdead_beef_5555_aaaa;
                    start.cpu.hi = 0x1234_5678_8765_4321;
                    start.cpu.lo = 0xfedc_ba98_0011_2233;
                    compare_store(code, &mut start, &mut native_ram, prefix,
                        &format!("store guard {store:08x}, base {base:x}, prefix {prefix}"));
                }
            }
        }
        if size > 1 {
            let word = op << 26 | 1 << 21 | 3 << 16;
            start.write_u32(0x1000, word);
            start.cpu.gpr[1] = 0xffff_ffff_8000_4001;
            let c = block(&[word]).unwrap();
            let code = place(&mut memory, &c).expect("code space");
            compare_store(code, &mut start, &mut native_ram, 0, "misaligned RDRAM store");
        }
        let word = op << 26;
        start.write_u32(0x1000, word);
        let c = block(&[word]).unwrap();
        let code = place(&mut memory, &c).expect("code space");
        compare_store(code, &mut start, &mut native_ram, 0, "zero base and value");
    }
}

#[test]
fn stores_to_source_code_stop_before_the_changed_instruction() {
    let mut memory = CodeMemory::new().expect("code memory");
    let mut native_ram = vec![0; crate::bus::RDRAM_SIZE];
    for (op, size) in [(0x28, 1), (0x29, 2), (0x2b, 4), (0x3f, 8)] {
        let words = [op << 26 | 1 << 21 | 2 << 16, 0x2423_0001, 0x2424_0001];
        let c = block(&words).unwrap();
        assert_eq!(c.instructions, 3, "a store goes on, unless into the block's own source");
        let code = place(&mut memory, &c).expect("code space");
        for physical in [0x1000u32, if size == 8 { 0x1008 } else { 0x1004 }] {
            let mut start = machine(&words);
            start.cpu.gpr[1] = 0xffff_ffff_8000_0000 | u64::from(physical);
            start.cpu.gpr[2] = 0x3c03_abcd_3423_ef01;
            start.cpu.gpr[3] = 0;
            start.cpu.gpr[4] = 0;
            compare_store(code, &mut start, &mut native_ram, 1, "self-modifying store");
            assert_eq!(start.cpu.gpr[3], 0);
            assert_eq!(start.cpu.gpr[4], 0);
            // The exit says code was written.
            let no_marks = [0; crate::rdp_thread::PAGES];
            let mut cpu = start.cpu.clone();
            let result = execute_marked(code, &mut cpu, native_ram.as_mut_ptr(), &no_marks, 0);
            assert_eq!(result, 1 | WROTE_CODE);
            // And where, for the dispatcher (on 32-bit ARM an SD's second word).
            let at = physical + if size == 8 && cfg!(target_arch = "arm") { 4 } else { 0 };
            assert_eq!(WRITTEN.with(|written| written.get()), at);
        }
    }
}

#[test]
fn all_integer_branches_match_full_cpu_and_live_virtual_pc() {
    let mut memory = CodeMemory::new().expect("code memory");
    let mut start = machine(&[0; 4]);
    let mut seed = 0xb4a0_cafe_6420_1234;
    let mut all = Vec::new();
    for immediate in [0, 1, 0x7fff, 0x8000, 0xffff] {
        all.extend(branches(immediate));
    }
    all.extend([
        0x0800_0000, 0x0c00_0000, 0x0bff_ffff, 0x0fff_ffff,
        0x0000_0008, // jr zero
        0x03e0_0008, // jr ra
        0x0020_0809, // jalr at,at (capture target before link)
        0x03e0_f809, // jalr ra,ra
        0x03e0_0009, // jalr zero,ra
        0x0000_0009, // jalr zero,zero
    ]);
    for rt in [0x10, 0x11, 0x12, 0x13] {
        // The condition must see old ra, before its link is stored.
        all.push(0x0400_0000 | 31 << 21 | rt << 16 | 0xffff);
    }
    for op in [0x04, 0x05, 0x14, 0x15] {
        all.push(op << 26 | 1 << 21 | 1 << 16 | 0xffff); // rs == rt
        all.push(op << 26 | 0xffff); // zero == zero
    }
    let values = [
        (0, 0), (1, 1), (1, 2), (0, 1u64 << 32),
        (0x0000_0000_8000_0000, 0), (0x1234_5678_8000_0000, 0),
        (u64::MAX, 0), (u64::MAX, u64::MAX),
        (0x7fff_ffff_ffff_ffff, u64::MAX), (0x8000_0000_0000_0000, 1),
    ];
    // A load slot (which these addresses mostly leave to the
    // interpreter, the branch done), and plain arithmetic, both run
    // in the block unless the branch jumps to itself: one that
    // changes the branch's rs, and one that reads the link.
    let slots = [0x8c23_0000, 0x2423_0001, 0x2421_0001, 0x27e3_0004];
    for (word, slot) in all.iter().flat_map(|&w| slots.map(|slot| (w, slot))) {
        for prefix in [0, 2] {
            let mut source = vec![0x6421_0001, 0x0020_0011]; // daddiu at,at,1; mthi at
            source.truncate(prefix);
            source.extend([word, slot]);
            let c = block(&source).unwrap();
            assert!(c.ends_in_branch);
            assert!(c.instructions == prefix + 1 || c.instructions == prefix + 2);
            let code = place(&mut memory, &c).expect("code space");
            for (index, &op) in source.iter().enumerate() {
                start.write_u32(0x1000 + 4 * index as u32, op);
            }
            // Reuse the SAME generated code at both virtual aliases.
            for pc in [0xffff_ffff_8000_1000u64, 0xffff_ffff_a000_1000] {
                for case in 0..24 {
                    for r in 1..32 {
                        start.cpu.gpr[r] = random(&mut seed);
                    }
                    let (left, right) = values.get(case).copied()
                        .unwrap_or_else(|| (random(&mut seed), random(&mut seed)));
                    start.cpu.gpr[1] = left;
                    start.cpu.gpr[2] = right;
                    start.cpu.gpr[31] = left;
                    start.cpu.hi = random(&mut seed);
                    start.cpu.lo = random(&mut seed);
                    start.cpu.jump(pc);
                    start.cpu.current_pc = 0x1234_5678_9abc_def0;
                    start.cpu.current_delay = true;
                    compare_branch_either(code, &mut start, c.instructions,
                        &format!("branch {word:08x}, slot {slot:08x}, prefix {prefix}, pc {pc:x}, input {left:x}/{right:x}"));
                }
            }
        }
    }
}

#[test]
fn fpu_branches_match_full_cpu_and_clear_the_cause() {
    use crate::cop0::CU1;
    let mut memory = CodeMemory::new().expect("code memory");
    let mut start = machine(&[0; 4]);
    let mut seed = 0xbc1f_bc1e_0000_0001u64;
    // BC1F, BC1T, BC1FL, BC1TL, ahead and to themselves.
    let words: Vec<u32> = [0u32, 1, 2, 3].iter()
        .flat_map(|&nd| [0x4500_0000 | nd << 16 | 3, 0x4500_0000 | nd << 16 | 0xffff]).collect();
    for word in words {
        for (slot, prefix) in [(0x2423_0001, 0), (0x2423_0001, 2), (0x8c23_0000, 0)] {
            let mut source = vec![0x6421_0001, 0x0020_0011];
            source.truncate(prefix);
            source.extend([word, slot]);
            let c = block(&source).unwrap();
            assert!(c.ends_in_branch, "{word:08x}");
            let code = place(&mut memory, &c).expect("code space");
            for (index, &op) in source.iter().enumerate() {
                start.write_u32(0x1000 + 4 * index as u32, op);
            }
            for pc in [0xffff_ffff_8000_1000u64, 0xffff_ffff_a000_1000] {
                for case in 0..16 {
                    for r in 1..32 {
                        start.cpu.gpr[r] = random(&mut seed);
                    }
                    start.cpu.gpr[1] = 0xffff_ffff_8000_4000;
                    start.cpu.fpu.fcr31 = random(&mut seed) as u32 & 0x0183_ffff;
                    start.cpu.cop0.status = if case % 5 == 0 { 0 } else { CU1 };
                    start.cpu.jump(pc);
                    let instructions = if start.cpu.cop0.status & CU1 == 0 { prefix } else { c.instructions };
                    let label = format!("{word:08x}, slot {slot:08x}, prefix {prefix}, pc {pc:x}, case {case}");
                    if instructions == prefix {
                        // Unusable: an exit before the branch, nothing changed.
                        compare(code, &mut start, prefix, &label);
                    } else {
                        compare_branch_either(code, &mut start, instructions, &label);
                    }
                }
            }
        }
    }
}

#[test]
fn load_results_feed_link_conditions_before_branch_metadata_changes() {
    let mut memory = CodeMemory::new().expect("code memory");
    let mut start = machine(&[0; 3]);
    for rt in [0x10, 0x11, 0x12, 0x13] {
        let branch = 0x0400_0000 | 31 << 21 | rt << 16 | 3;
        let words = [0x8c9f_0000, branch, 0x2422_0001]; // lw ra,0(a0); link branch on ra; slot
        let c = block(&words).unwrap();
        assert!(c.ends_in_branch);
        assert_eq!(c.instructions, 3);
        let code = place(&mut memory, &c).expect("code space");
        for (index, &word) in words.iter().enumerate() {
            start.write_u32(0x1000 + 4 * index as u32, word);
        }
        for value in [0, 1, 0x7fff_ffff, 0x8000_0000, u32::MAX] {
            start.write_u32(0x4000, value);
            start.cpu.gpr[4] = 0xffff_ffff_8000_4000;
            compare_branch_either(code, &mut start, c.instructions, "loaded ra branch condition");
        }
    }
}

#[test]
fn memory_slots_match_full_cpu_and_ram_or_leave_the_slot() {
    use crate::cop0::CU1;
    let mut memory = CodeMemory::new().expect("code memory");
    let mut start = machine(&[0; 4]);
    let mut native_ram = vec![0; start.rdram.len()];
    // beq at,v0; beql at,v0; bne at,v0; jr ra; jal 0x2000.
    let branches = [0x1022_0010, 0x5022_0010, 0x1422_0010, 0x03e0_0008, 0x0c00_0800];
    // lw v1, lbu v1, lh v1, lw ra, sw v0, sb v0, sd v0, sw ra (the
    // new link after JAL), swc1 f2, lwc1 f2, ldc1 f2, sdc1 f2: all
    // at 8(a0).
    // And the FPU's: add.s f2,f2,f4, mov.s f6,f2, c.lt.s f2,f4,
    // cvt.d.s f6,f2, mfc1 v1,f2, mtc1 v0,f2, mul.d f6,f2,f4.
    let slots = [0x8c83_0008, 0x9083_0009, 0x8483_000a, 0x8c9f_0008, 0xac82_0008, 0xa082_000b, 0xfc82_0008,
        0xac9f_0008, 0xe482_0008, 0xc482_0008, 0xd482_0008, 0xf482_0008, 0x4604_1080, 0x4600_1186, 0x4604_103c,
        0x4600_11a1, 0x4403_1000, 0x4482_1000, 0x4624_1182];
    let mut seed = 0x51a7_0d00_cafe_0001u64;
    for &branch in &branches {
        for &slot in &slots {
            let source = [branch, slot];
            let c = block(&source).unwrap();
            assert_eq!(c.instructions, 2, "{branch:08x} {slot:08x}");
            let code = place(&mut memory, &c).expect("code space");
            for (index, &op) in source.iter().enumerate() {
                start.write_u32(0x1000 + 4 * index as u32, op);
            }
            // RDRAM through kseg0 and kseg1 (one in the block's own
            // page), then a misaligned, a device's and a kuseg one.
            for address in [0xffff_ffff_8000_2000u64, 0xffff_ffff_a000_2ff0, 0xffff_ffff_8000_1ff0,
                0xffff_ffff_8000_2001, 0xffff_ffff_a404_0000, 0x2000]
            {
                for (left, right, status) in [(1, 1, 0), (1, 2, CU1), (3, 3, CU1)] {
                    for r in 1..32 {
                        start.cpu.gpr[r] = random(&mut seed);
                    }
                    start.cpu.gpr[1] = left;
                    start.cpu.gpr[2] = right;
                    start.cpu.gpr[4] = address;
                    start.cpu.gpr[31] = 0xffff_ffff_8000_3000;
                    start.cpu.fpu.regs[2] = random(&mut seed);
                    start.cpu.cop0.status = status;
                    for i in 0..4 {
                        start.write_u32(0x2000 + 4 * i, random(&mut seed) as u32);
                        start.write_u32(0x2ff8 + 4 * i, random(&mut seed) as u32);
                    }
                    start.cpu.jump(0xffff_ffff_8000_1000);
                    start.cpu.current_pc = 0x1234_5678_9abc_def0;
                    start.cpu.current_delay = true;
                    let label = format!("branch {branch:08x}, slot {slot:08x}, at {address:x}, {left}/{right}, status {status:x}");
                    let result = compare_slot(code, &mut start, &mut native_ram, &label);
                    let store = matches!(slot >> 26, 0x28 | 0x2b | 0x3f | 0x39 | 0x3d);
                    let ran = (result & 0xffff) as usize;
                    if store && ran == 2 && address & 0xfff_f000 == 0x1000 {
                        assert!(result & WROTE_CODE != 0, "a store into code: {label}");
                    }
                }
            }
            start.cpu.cop0.status = 0;
        }
    }
}

/// A branch with a memory slot, on RAM of its own: the whole CPU and
/// RAM compared with the interpreter run for as many as the block
/// completed (the branch alone where a guard leaves the slot to it);
/// the block's result without BRANCHED.
fn compare_slot(code: *const u32, reference: &mut N64, native_ram: &mut [u8], label: &str) -> u32 {
    static PAGES: [u32; crate::rdp_thread::MARKS] = [0; crate::rdp_thread::MARKS];
    native_ram.copy_from_slice(&reference.rdram);
    let before = reference.cpu.clone();
    let cycles = reference.cycles;
    let mut compiled = before.clone();
    let result = execute_marked(code, &mut compiled, native_ram.as_mut_ptr(), &PAGES, 0);
    for _ in 0..result & 0xffff {
        reference.step_cpu();
    }
    let (mut a, mut b) = (Vec::new(), Vec::new());
    compiled.save(&mut a);
    reference.cpu.save(&mut b);
    assert_eq!(a, b, "CPU differs: {label}");
    if &*native_ram != reference.rdram.as_slice() {
        let at = native_ram.iter().zip(&reference.rdram).position(|(a, b)| a != b).unwrap();
        panic!("{label}: RAM {at:08x}: native {:02x}, reference {:02x}", native_ram[at], reference.rdram[at]);
    }
    reference.cpu = before;
    reference.cycles = cycles;
    result
}

#[test]
fn guarded_load_before_branch_keeps_all_control_metadata_untouched() {
    let words = [0x6402_0005, 0x8c83_0000, 0x1022_ffff];
    let c = block(&words).unwrap();
    assert!(c.ends_in_branch);
    assert_eq!(c.instructions, 3);
    let mut memory = CodeMemory::new().expect("code memory");
    let code = place(&mut memory, &c).expect("code space");
    let mut start = machine(&words);
    for address in [0xffff_ffff_a404_001cu64, 0xffff_ffff_8000_4001, 0x4000, 0x8000_4000] {
        start.cpu.gpr[4] = address;
        start.cpu.current_pc = 0x1234_5678_9abc_def0;
        start.cpu.current_delay = true;
        compare(code, &mut start, 1, "guard before terminal branch");
    }
}

/// A value for a guest register: any 64 bits, a sign-extended word, a
/// small number or an address of RDRAM through kseg0.
fn value(seed: &mut u64) -> u64 {
    let bits = random(seed);
    match bits % 4 {
        0 => random(seed),
        1 => random(seed) as i32 as u64,
        2 => random(seed) % 64,
        _ => 0xffff_ffff_8000_4000 + (random(seed) & 0x3ff8),
    }
}

/// Random blocks of 32- and 64-bit arithmetic, loads and stores over
/// more guest registers than the pool holds (so held ones are let go
/// and read again), with 64-bit results over held words, and bases
/// that are no RDRAM address or not a sign-extended word (an exit
/// with registers changed before it): each compared with the
/// interpreter for as many instructions as it completed, its RAM in
/// the window the accesses reach after each run and whole after
/// each block.
#[test]
fn held_registers_match_the_interpreter_through_exits_and_evictions() {
    let mut memory = CodeMemory::new().expect("code memory");
    let mut seed = 0x5eed_0fb1_0c4e_1d00;
    // SLL, SRL, SRA, SLLV, SRLV, SRAV, MFHI, MTHI, MFLO, MTLO, DSLLV,
    // MULT, MULTU, ADDU, SUBU, AND, OR, XOR, NOR, SLT, SLTU, DADDU,
    // DSUBU, DSLL, DSLL32, DSRA32.
    let specials = [0x00, 0x02, 0x03, 0x04, 0x06, 0x07, 0x10, 0x11, 0x12, 0x13, 0x14, 0x18, 0x19, 0x21,
        0x23, 0x24, 0x25, 0x26, 0x27, 0x2a, 0x2b, 0x2d, 0x2f, 0x38, 0x3c, 0x3f];
    // ADDIU, SLTI, SLTIU, ANDI, ORI, XORI, LUI, DADDIU.
    let immediates = [0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f, 0x19];
    // LB, LBU, LH, LHU, LW, LWU, LD, SB, SH, SW, SD with their sizes.
    let accesses = [(0x20, 1), (0x24, 1), (0x21, 2), (0x25, 2), (0x23, 4), (0x27, 4), (0x37, 8), (0x28, 1),
        (0x29, 2), (0x2b, 4), (0x3f, 8)];
    let window = 0..0x1_0000;
    for number in 0..160 {
        let register = |seed: &mut u64| [0u32, 1, 2, 3, 4, 5, 6, 7, 2, 3][random(seed) as usize % 10];
        let words: Vec<u32> = (0..4 + random(&mut seed) as usize % 28).map(|_| {
            let (rs, rt, rd) = (register(&mut seed), register(&mut seed), register(&mut seed));
            match random(&mut seed) % 10 {
                0..4 => {
                    let funct = specials[random(&mut seed) as usize % specials.len()];
                    rs << 21 | rt << 16 | rd << 11 | (random(&mut seed) as u32 % 32) << 6 | funct
                }
                4..7 => {
                    let op = immediates[random(&mut seed) as usize % immediates.len()];
                    let immediate = if random(&mut seed) % 2 == 0 { random(&mut seed) as u32 % 64 } else {
                        random(&mut seed) as u32 & 0xffff
                    };
                    op << 26 | rs << 21 | rt << 16 | immediate
                }
                _ => {
                    let (op, size) = accesses[random(&mut seed) as usize % accesses.len()];
                    let base = [8, 8, 9, rs][random(&mut seed) as usize % 4];
                    let mut offset = (random(&mut seed) as u32 % 64) * size;
                    if random(&mut seed) % 8 == 0 {
                        offset += 1;
                    }
                    op << 26 | base << 21 | rt << 16 | offset.wrapping_sub(0x80) & 0xffff
                }
            }
        }).collect();
        let c = block(&words).unwrap();
        assert_eq!(c.instructions, words.len(), "{words:08x?}");
        let code = place(&mut memory, &c).expect("code space");
        let mut start = machine(&words);
        let mut native_ram = start.rdram.clone();
        for case in 0..6 {
            for r in 1..32 {
                start.cpu.gpr[r] = value(&mut seed);
            }
            start.cpu.gpr[8] = 0xffff_ffff_8000_4100 + (random(&mut seed) & 0x7f8);
            start.cpu.gpr[9] = match case % 3 {
                0 => 0xffff_ffff_a000_6100,
                1 => 0x0000_0000_8000_6100,
                _ => 0xffff_ffff_a404_0100,
            };
            start.cpu.hi = value(&mut seed);
            start.cpu.lo = value(&mut seed);
            for at in (0x4000..0x8000).step_by(8) {
                start.rdram[at..at + 8].copy_from_slice(&random(&mut seed).to_be_bytes());
            }
            native_ram[window.clone()].copy_from_slice(&start.rdram[window.clone()]);
            let label = format!("block {number}, case {case}: {words:08x?}");
            let before = start.cpu.clone();
            let mut compiled = before.clone();
            let count = execute(code, &mut compiled, native_ram.as_mut_ptr());
            for _ in 0..count {
                start.step_cpu();
            }
            assert_eq!(compiled.gpr, start.cpu.gpr, "after {count}: {label}");
            assert_eq!((compiled.hi, compiled.lo), (start.cpu.hi, start.cpu.lo), "after {count}: {label}");
            assert!(native_ram[window.clone()] == start.rdram[window.clone()], "RAM after {count}: {label}");
            compiled.gpr = before.gpr;
            compiled.hi = before.hi;
            compiled.lo = before.lo;
            let (mut a, mut b) = (Vec::new(), Vec::new());
            compiled.save(&mut a);
            before.save(&mut b);
            assert_eq!(a, b, "metadata changed: {label}");
            start.cpu = before;
        }
        assert!(native_ram == start.rdram, "RAM outside the window: block {number}");
    }
}

/// Words held changed before a branch that writes a link (also to a
/// register its condition or target read, or its slot reads), and
/// likely branches whose slot runs on one way only.
#[test]
fn held_registers_meet_links_and_likely_slots() {
    let blocks: [&[u32]; 10] = [
        // addiu ra,zero,5; addu v0,ra,ra; jal 0x400; addiu v1,ra,1
        &[0x241f_0005, 0x03ff_1021, 0x0c00_0100, 0x27e3_0001],
        // addiu a0,zero,0x40; addiu ra,ra,3; jalr a0; addu v0,ra,a0
        &[0x2404_0040, 0x27ff_0003, 0x0080_f809, 0x03e4_1021],
        // addiu ra,ra,3; bgezal ra,+4; addiu v0,ra,0
        &[0x27ff_0003, 0x07f1_0004, 0x27e2_0000],
        // lui ra,0x8000; ori ra,ra,0x2000; jalr ra,ra; addiu v0,ra,0
        &[0x3c1f_8000, 0x37ff_2000, 0x03e0_f809, 0x27e2_0000],
        // addiu v0,v0,1; beql v0,v1,+4; addiu v1,v1,7
        &[0x2442_0001, 0x5043_0004, 0x2463_0007],
        // addiu v0,v0,1; bnel v0,v1,+4; sw v0,0(a1)
        &[0x2442_0001, 0x5443_0004, 0xaca2_0000],
        // addiu v0,v0,1; bne v0,v1,+4; lw v1,0(a1)
        &[0x2442_0001, 0x1443_0004, 0x8ca3_0000],
        // addiu v0,v0,-1; bgtzl v0,+4; addiu v1,v0,0
        &[0x2442_ffff, 0x5c40_0004, 0x2443_0000],
        // sll v0,v0,0; bltzall v0,+4; addu v1,ra,v0
        &[0x0002_1000, 0x0452_0004, 0x03e2_1821],
        // lw v0,0(a1); addiu ra,v0,1; jr ra; lw ra,4(a1)
        &[0x8ca2_0000, 0x245f_0001, 0x03e0_0008, 0x8cbf_0004],
    ];
    let mut memory = CodeMemory::new().expect("code memory");
    let mut seed = 0x11a7_e5f0_4b1d_0042;
    for words in blocks {
        let c = block(words).unwrap();
        assert_eq!(c.instructions, words.len(), "{words:08x?}");
        let code = place(&mut memory, &c).expect("code space");
        let mut start = machine(words);
        let mut native_ram = vec![0; start.rdram.len()];
        for case in 0..24 {
            for r in 1..32 {
                start.cpu.gpr[r] = value(&mut seed);
            }
            if case % 2 == 0 {
                start.cpu.gpr[3] = start.cpu.gpr[2].wrapping_add(1);
            }
            if case % 3 == 0 {
                start.cpu.gpr[2] = random(&mut seed) % 3;
            }
            start.cpu.gpr[5] = if case % 4 == 3 { 0xffff_ffff_a404_0000 } else { 0xffff_ffff_8000_4008 };
            start.rdram[0x4008..0x4010].copy_from_slice(&random(&mut seed).to_be_bytes());
            compare_slot(code, &mut start, &mut native_ram, &format!("{words:08x?}, case {case}"));
        }
    }
}
