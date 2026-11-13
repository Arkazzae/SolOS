//! The NEON instructions against the scalar VU, on either ARM: these run on one (the --pi checks, or
//! natively on a unit).

use super::Vu;
use super::neon::{available, entry, execute};
use savestate::State;

fn random(seed: &mut u64) -> u64 {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 7;
    *seed ^= *seed << 17;
    *seed
}

fn populated(seed: &mut u64) -> Vu {
    let mut vu = Vu::new();
    for vector in &mut vu.regs {
        for lane in vector {
            *lane = random(seed) as u16;
        }
    }
    for lanes in [&mut vu.acc_h, &mut vu.acc_m, &mut vu.acc_l] {
        for lane in lanes {
            *lane = random(seed) as u16;
        }
    }
    vu.vco = random(seed) as u16;
    vu.vcc = random(seed) as u16;
    vu.vce = random(seed) as u8;
    vu.div_in = random(seed) as u16;
    vu.div_out = random(seed) as u16;
    vu.div_pending = random(seed) & 1 != 0;
    vu
}

fn state(vu: &Vu) -> Vec<u8> {
    let mut bytes = Vec::new();
    vu.save(&mut bytes);
    bytes
}

fn same(mut vu: Vu, word: u32, label: &str) {
    let mut reference = vu.clone();
    reference.execute_reference(word);
    assert!(execute(&mut vu, word));
    assert!(state(&vu) == state(&reference), "{label}: {word:08x}");
}

fn instruction(function: u32, element: usize, vs: usize, vt: usize, vd: usize) -> u32 {
    0x4a00_0000 | (element as u32) << 21 | (vt as u32) << 16
        | (vs as u32) << 11 | (vd as u32) << 6 | function
}

#[test]
fn linux_reports_neon_for_the_arm_conformance_target() {
    assert!(available(), "NEON conformance requires an ARM Linux CPU with NEON");
}

#[test]
fn fraction_multiply_matches_every_broadcast_and_register_alias() {
    let mut seed = 0x4a64_cafe_3276_8000;
    let edges = [0, 1, 2, 0xffff, 0xfffe, 0x8000, 0x8001, 0x7fff,
        0x7ffe, 0x4000, 0x4001, 0xc000, 0xbfff, 0x3fff];
    for function in [0, 1] {
        for element in 0..16 {
            for (vs, vt, vd) in [(1, 2, 3), (1, 2, 1), (1, 2, 2),
                (1, 1, 1), (0, 31, 0), (31, 0, 31)]
            {
                for case in 0..128 {
                    let mut vu = populated(&mut seed);
                    if case < edges.len() * 2 {
                        for n in 0..8 {
                            vu.regs[vs][n] = edges[(case + n) % edges.len()];
                            vu.regs[vt][n] = edges[(case / 2 + n * 3) % edges.len()];
                        }
                    }
                    same(vu, instruction(function, element, vs, vt, vd),
                        &format!("element {element}, aliases {vs}/{vt}/{vd}, case {case}"));
                }
            }
        }
    }
}

#[test]
fn every_signed_input_matches_at_fraction_and_saturation_boundaries() {
    let mut seed = 0x8000_7fff_0001_ffff;
    // Exhaust every 16-bit input at factors which reach both signed
    // extremes, the MIN*MIN corner, and rounding through zero.
    for function in [0, 1] {
        for factor in [0x8000, 0x8001, 0xffff, 0, 1, 0x7fff] {
            for first in (0..=u16::MAX).step_by(8) {
                let mut vu = populated(&mut seed);
                for lane in 0..8 {
                    vu.regs[1][lane] = first + lane as u16;
                }
                vu.regs[2] = [factor; 8];
                same(vu, instruction(function, 0, 1, 2, 3),
                    &format!("input {first}, factor {factor:04x}"));
            }
        }
    }
}

#[test]
fn high_multiply_matches_every_broadcast_alias_and_accumulator_edge() {
    let mut seed = 0x0f07_cafe_3276_8000;
    let edges = [0, 1, 2, 0xffff, 0xfffe, 0x8000, 0x8001, 0x7fff,
        0x7ffe, 0x4000, 0x4001, 0xc000, 0xbfff, 0x3fff];
    let accumulators = [0u32, 1, 0x7fff, 0x8000, 0xffff, 0xffff_7fff,
        0xffff_8000, 0xffff_8001, 0xffff_ffff, 0x7fff_ffff, 0x8000_0000,
        0x7fff_0000, 0x8000_ffff, 0x4000_0000, 0xc000_0000,
        0x0000_8001, 0xffff_0000, 0x0001_0000];
    for function in [0x07, 0x0f] {
        for element in 0..16 {
            for (vs, vt, vd) in [(1, 2, 3), (1, 2, 1), (1, 2, 2),
                (1, 1, 1), (0, 31, 0), (31, 0, 31)]
            {
                for case in 0..256 {
                    let mut vu = populated(&mut seed);
                    if case < 128 {
                        for n in 0..8 {
                            vu.regs[vs][n] = edges[(case + n) % edges.len()];
                            vu.regs[vt][n] = edges[(case / 2 + n * 3) % edges.len()];
                            let acc = accumulators[(case + n) % accumulators.len()];
                            vu.acc_h[n] = (acc >> 16) as u16;
                            vu.acc_m[n] = acc as u16;
                        }
                    }
                    same(vu, instruction(function, element, vs, vt, vd),
                        &format!("high function {function}, element {element}, aliases {vs}/{vt}/{vd}, case {case}"));
                }
            }
        }
    }
}

#[test]
fn high_accumulate_wraps_and_clamps_at_each_signed_boundary() {
    let mut seed = 0x7fff_ffff_8000_0000;
    let inputs = [0u16, 1, 0xffff, 0x8000, 0x8001, 0x7fff, 0x4000, 0xc000];
    let results = [i32::MIN, i32::MAX, -32769, -32768, -32767,
        -1, 0, 1, 32766, 32767, 32768];
    for s in inputs {
        for t in inputs {
            for target in results {
                let mut vu = populated(&mut seed);
                vu.regs[1] = [s; 8];
                vu.regs[2] = [t; 8];
                let product = i32::from(s as i16) * i32::from(t as i16);
                let acc = target.wrapping_sub(product) as u32;
                vu.acc_h = [(acc >> 16) as u16; 8];
                vu.acc_m = [acc as u16; 8];
                same(vu, instruction(0x0f, 0, 1, 2, 3),
                    &format!("product {s:04x}*{t:04x}, wrapped target {target}"));
            }
        }
    }
}

#[test]
fn every_signed_high_input_matches_at_accumulator_boundaries() {
    let mut seed = 0x0f07_7fff_0001_ffff;
    let accumulators = [0u32, 0x7fff, 0x8000, 0xffff_7fff,
        0xffff_8000, 0xffff_ffff, 0x7fff_ffff, 0x8000_0000];
    for function in [0x07, 0x0f] {
        for factor in [0x8000, 0x8001, 0xffff, 0, 1, 0x7fff] {
            for first in (0..=u16::MAX).step_by(8) {
                let mut vu = populated(&mut seed);
                for lane in 0..8 {
                    vu.regs[1][lane] = first + lane as u16;
                    let acc = accumulators[(first as usize / 8 + lane) & 7];
                    vu.acc_h[lane] = (acc >> 16) as u16;
                    vu.acc_m[lane] = acc as u16;
                }
                vu.regs[2] = [factor; 8];
                same(vu, instruction(function, 0, 1, 2, 3),
                    &format!("high function {function}, input {first}, factor {factor:04x}"));
            }
        }
    }
}

#[test]
fn low_multiply_matches_all_broadcasts_aliases_and_replaced_accumulators() {
    let mut seed = 0x0405_0664_8000_ffff;
    let inputs = [0, 1, 2, 0x7fff, 0x8000, 0xffff, 0xfffe, 0x8001,
        0x7ffe, 0x4000, 0xc000, 0x3fff, 0xbfff];
    for function in [0x04, 0x05, 0x06] {
        for element in 0..16 {
            for (vs, vt, vd) in [(1, 2, 3), (1, 2, 1), (1, 2, 2),
                (1, 1, 1), (0, 31, 0), (31, 0, 31)]
            {
                for case in 0..256 {
                    let mut vu = populated(&mut seed);
                    if case < 128 {
                        for n in 0..8 {
                            vu.regs[vs][n] = inputs[(case + n) % inputs.len()];
                            vu.regs[vt][n] = inputs[(case / inputs.len() + n * 3) % inputs.len()];
                            vu.set_acc(n, ACC_EDGES[(case + n) % ACC_EDGES.len()]);
                        }
                    }
                    same(vu, instruction(function, element, vs, vt, vd),
                        &format!("low f{function:02x} e{element} {vs}/{vt}/{vd} case{case}"));
                }
            }
        }
    }
}

#[test]
fn low_multiply_exhausts_each16bit_operand_at_signed_and_unsigned_extremes() {
    let mut seed = 0x0605_0464_ffff_8000;
    for function in [0x04, 0x05, 0x06] {
        // Unsigned VMUDL is symmetric. For both mixed operations,
        // exhaust the signed and unsigned roles independently.
        for varying in 0..=usize::from(function != 0x04) {
            for factor in [0, 1, 0x7fff, 0x8000, 0xfffe, 0xffff] {
                for first in (0..=u16::MAX).step_by(8) {
                    let mut vu = populated(&mut seed);
                    vu.regs[1] = [factor; 8];
                    vu.regs[2] = [factor; 8];
                    for lane in 0..8 {
                        vu.regs[1 + varying][lane] = first + lane as u16;
                    }
                    same(vu, instruction(function, 0, 1, 2, 3),
                        &format!("low f{function:02x} role{varying} input{first} factor{factor:04x}"));
                }
            }
        }
    }
}

#[test]
fn low_multiply_public_dispatch_counts_and_matches_dependent_full_state() {
    let mut seed = 0x0405_06aa_ffff_0000;
    let mut actual = populated(&mut seed);
    actual.accelerated = true;
    actual.profiling = true;
    let mut reference = actual.clone();
    for n in 0..16_384 {
        let vs = (random(&mut seed) & 31) as usize;
        let vt = (random(&mut seed) & 31) as usize;
        let vd = (random(&mut seed) & 31) as usize;
        let input = random(&mut seed) as u16;
        actual.regs[vs][n as usize & 7] = input;
        reference.regs[vs][n as usize & 7] = input;
        let word = instruction(0x04 + n % 3, (n as usize >> 1) & 15, vs, vt, vd);
        reference.execute_reference(word);
        actual.execute(word);
        assert!(state(&actual) == state(&reference), "low sequence instruction{n}");
        assert_eq!(actual.native_count, u64::from(n) + 1);
    }
}

/// Compare complete public dispatch with acceleration enabled/disabled.
#[test]
#[ignore]
fn low_instruction_cost() {
    let mode = std::env::var("N64_VU_MODE").unwrap_or_else(|_| "neon".into());
    let function: u32 = std::env::var("N64_VU_FUNCTION").unwrap_or_else(|_| "4".into()).parse().unwrap();
    let count: u32 = std::env::var("N64_VU_COUNT").unwrap_or_else(|_| "200000".into()).parse().unwrap();
    assert!(matches!(function, 0x04..=0x06));
    let mut seed = 0xabc0_6423_44f1_8000;
    let mut vu = populated(&mut seed);
    vu.accelerated = mode == "neon";
    for n in 0..count {
        vu.regs[1][n as usize & 7] = (n as u16).wrapping_mul(31337) ^ (n >> 8) as u16;
        let word = std::hint::black_box(instruction(function, n as usize & 15, 1, 2, 1));
        vu.execute(word);
        std::hint::black_box(&vu);
    }
    let hash = state(&vu).into_iter().fold(0xcbf29ce484222325u64,
        |h, b| (h ^ u64::from(b)).wrapping_mul(0x100000001b3));
    eprintln!("low f{function:02x} {mode}: {count}, state hash {hash:016x}");
}

/// The functions `execute` has.
const HANDLED: [u32; 36] = [0x00, 0x01, 0x04, 0x05, 0x06, 0x07, 0x08, 0x0c, 0x0d, 0x0e, 0x0f, 0x10, 0x11,
    0x14, 0x15, 0x1d, 0x20, 0x21, 0x22, 0x23, 0x27, 0x28, 0x29, 0x2a, 0x2b, 0x2c, 0x2d,
    0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x3f];

#[test]
fn unhandled_functions_leave_every_byte_unchanged() {
    let mut seed = 0x1234_5678_9abc_def0;
    let mut vu = populated(&mut seed);
    let before = state(&vu);
    for function in (0..64).filter(|function| !HANDLED.contains(function)) {
        assert!(!execute(&mut vu, instruction(function, 0, 1, 2, 3)));
        assert!(state(&vu) == before);
    }
}

/// Every function at every element field and register alias, from
/// random state (flags, accumulator, reciprocal unit) and edge values.
#[test]
fn every_function_matches_every_broadcast_alias_and_flag() {
    let mut seed = 0x0a11_f00d_2026_1030;
    let inputs = [0, 1, 2, 0x7fff, 0x8000, 0xffff, 0xfffe, 0x8001,
        0x7ffe, 0x4000, 0xc000, 0x3fff, 0xbfff];
    for function in HANDLED {
        for element in 0..16 {
            for (vs, vt, vd) in [(1, 2, 3), (1, 2, 1), (1, 2, 2),
                (1, 1, 1), (0, 31, 0), (31, 0, 31), (9, 2, 3)]
            {
                for case in 0..96 {
                    let mut vu = populated(&mut seed);
                    if case < 48 {
                        for n in 0..8 {
                            vu.regs[vs][n] = inputs[(case + n) % inputs.len()];
                            vu.regs[vt][n] = inputs[(case / 3 + n * 3) % inputs.len()];
                            vu.set_acc(n, ACC_EDGES[(case + n) % ACC_EDGES.len()]);
                        }
                        if case & 1 == 0 {
                            vu.regs[vt] = vu.regs[vs];
                        }
                    }
                    same(vu, instruction(function, element, vs, vt, vd),
                        &format!("f{function:02x} e{element} {vs}/{vt}/{vd} case{case}"));
                }
            }
        }
    }
}

/// Exhausts each 16-bit operand against values at the clamps, the
/// carries and the sign, for the adds, compares and accumulates.
#[test]
fn every_input_matches_for_adds_compares_and_accumulates() {
    let mut seed = 0x10_11_14_15_20_23_08_0c;
    for function in [0x08, 0x0c, 0x10, 0x11, 0x14, 0x15, 0x20, 0x21, 0x22, 0x23] {
        for factor in [0, 1, 0x7fff, 0x8000, 0xfffe, 0xffff] {
            for first in (0..=u16::MAX).step_by(8) {
                let mut vu = populated(&mut seed);
                for lane in 0..8 {
                    vu.regs[1][lane] = first + lane as u16;
                    vu.set_acc(lane, ACC_EDGES[(first as usize / 8 + lane) % ACC_EDGES.len()]);
                }
                vu.regs[2] = [factor; 8];
                same(vu.clone(), instruction(function, 0, 1, 2, 3),
                    &format!("f{function:02x} input{first} factor{factor:04x}"));
                same(vu, instruction(function, 0, 2, 1, 3),
                    &format!("f{function:02x} swapped input{first} factor{factor:04x}"));
            }
        }
    }
}

/// Each function's entry for compiled code against the reference, with
/// acceleration on (then it counts) and off; none for the others.
#[test]
fn entries_match_the_reference_and_count_like_execute() {
    let mut seed = 0xe7e7_0e17_2026_1030;
    for function in 0..64 {
        let word = instruction(function, 5, 1, 2, 3);
        let Some(enter) = entry(word) else {
            assert!(!HANDLED.contains(&function), "f{function:02x} has no entry");
            continue;
        };
        assert!(HANDLED.contains(&function));
        for accelerated in [true, false] {
            for case in 0..64 {
                let mut actual = populated(&mut seed);
                actual.accelerated = accelerated;
                actual.profiling = true;
                let mut reference = actual.clone();
                let word = instruction(function, case % 16, (case * 7) % 32, (case * 3 + 1) % 32, case % 32);
                reference.execute_reference(word);
                enter(&mut actual, word);
                assert!(state(&actual) == state(&reference), "f{function:02x} case {case}");
                assert_eq!(actual.native_count, u64::from(accelerated), "f{function:02x}");
            }
        }
    }
}

/// Random instructions of every handled function in one dependent
/// sequence through the public dispatch: flags, the accumulator and the
/// reciprocal unit carry over from one to the next.
#[test]
fn dependent_sequence_of_every_function_matches_the_reference() {
    let mut seed = 0x5e9e_0ce0_f0a1_1fff;
    let mut actual = populated(&mut seed);
    actual.accelerated = true;
    actual.profiling = true;
    let mut reference = actual.clone();
    reference.accelerated = false;
    for n in 0..65_536u64 {
        let function = HANDLED[(random(&mut seed) % HANDLED.len() as u64) as usize];
        let word = instruction(function, (random(&mut seed) & 15) as usize, (random(&mut seed) & 31) as usize,
            (random(&mut seed) & 31) as usize, (random(&mut seed) & 31) as usize);
        reference.execute(word);
        actual.execute(word);
        assert!(state(&actual) == state(&reference), "instruction {n}: {word:08x}");
        assert_eq!(actual.native_count, n + 1);
    }
}

#[cfg(target_arch = "arm")]
#[test]
fn multiply_and_high_accumulate_do_not_change_the_host_fpscr() {
    unsafe fn read() -> u32 {
        let value;
        unsafe {
            core::arch::asm!("vmrs {value}, fpscr", value = out(reg) value,
                options(nomem, nostack, preserves_flags));
        }
        value
    }
    unsafe fn write(value: u32) {
        unsafe {
            core::arch::asm!("vmsr fpscr, {value}", value = in(reg) value,
                options(nomem, nostack));
        }
    }
    let mut vu = Vu::new();
    vu.regs[1] = [0x8000; 8];
    vu.regs[2] = [0x8000; 8];
    let saved = unsafe { read() };
    for function in HANDLED {
        for qc in [0, 1 << 27] {
            vu.acc_h = [0x7fff; 8];
            vu.acc_m = [0xffff; 8];
            let expected = saved & !(1 << 27) | qc;
            unsafe { write(expected) };
            assert!(execute(&mut vu, instruction(function, 0, 1, 2, 3)));
            let observed = unsafe { read() };
            unsafe { write(saved) };
            assert_eq!(observed, expected, "function {function}");
        }
    }
}

const ACC_EDGES: [u64; 22] = [
    0, 1, 0xffff, 0x1_0000, 0xffff_0000, 0xffff_ffff,
    0x1_0000_0000, 0x7fff_ffff, 0x8000_0000, 0x7fff_0000,
    0x8000_0000_0000, 0x7fff_ffff_ffff, 0x8000_0000_0001,
    0xffff_ffff_ffff, 0xffff_ffff_0000, 0xffff_0000_0000,
    0xffff_7fff_ffff, 0xffff_8000_0000, 0xffff_8000_ffff,
    0x7fff_ffff_0000, 0x8000_0000_ffff, 0x0000_8000_ffff,
];

#[test]
fn mixed_accumulate_matches_all_broadcasts_aliases_and_random48() {
    let mut seed = 0x0dad_0e64_48ac_c001;
    let inputs = [0, 1, 2, 0x7fff, 0x8000, 0xffff, 0xfffe, 0x8001,
        0x7ffe, 0x4000, 0xc000, 0x3fff, 0xbfff];
    for function in [0x0d, 0x0e] {
        for element in 0..16 {
            for (vs, vt, vd) in [(1, 2, 3), (1, 2, 1), (1, 2, 2),
                (1, 1, 1), (0, 31, 0), (31, 0, 31)]
            {
                for case in 0..256 {
                    let mut vu = populated(&mut seed);
                    if case < 128 {
                        for n in 0..8 {
                            vu.regs[vs][n] = inputs[(case + n) % inputs.len()];
                            vu.regs[vt][n] = inputs[(case / inputs.len() + n * 3) % inputs.len()];
                            vu.set_acc(n, ACC_EDGES[(case + n) % ACC_EDGES.len()]);
                        }
                    }
                    same(vu, instruction(function, element, vs, vt, vd),
                        &format!("mixed f{function:02x} e{element} {vs}/{vt}/{vd} case{case}"));
                }
            }
        }
    }
}

#[test]
fn mixed_accumulate_exhausts16bit_operand_at_carry_and_saturation_edges() {
    let mut seed = 0x0ead_0d64_8000_ffff;
    for function in [0x0d, 0x0e] {
        for factor in [0, 1, 0x7fff, 0x8000, 0xfffe, 0xffff] {
            for first in (0..=u16::MAX).step_by(8) {
                let mut vu = populated(&mut seed);
                for lane in 0..8 {
                    vu.regs[1][lane] = first + lane as u16;
                    vu.set_acc(lane, ACC_EDGES[(first as usize / 8 + lane) % ACC_EDGES.len()]);
                }
                vu.regs[2] = [factor; 8];
                // Vary either the signed or unsigned operand according
                // to the operation, covering both mixed input roles.
                same(vu, instruction(function, 0, 1, 2, 3),
                    &format!("mixed f{function:02x} input{first} factor{factor:04x}"));
            }
        }
    }
}

#[test]
fn mixed_accumulate_constructed_results_cover_exact_saturation_and_wrap() {
    let mut seed = 0x0dad_0ead_ffff_0001;
    let inputs = [0, 1, 0xffff, 0x8000, 0x8001, 0x7fff, 0xfffe, 0x4000];
    const MASK48: u64 = (1 << 48) - 1;
    for function in [0x0d, 0x0e] {
        for s in inputs {
            for t in inputs {
                let product = if function == 0x0d {
                    i64::from(s as i16) * i64::from(t)
                } else {
                    i64::from(s) * i64::from(t as i16)
                };
                for first in 0..ACC_EDGES.len() {
                    let mut actual = populated(&mut seed);
                    actual.regs[1] = [s; 8];
                    actual.regs[2] = [t; 8];
                    for lane in 0..8 {
                        let target = ACC_EDGES[(first + lane) % ACC_EDGES.len()];
                        actual.set_acc(lane, target.wrapping_sub(product as u64) & MASK48);
                    }
                    let mut reference = actual.clone();
                    let word = instruction(function, 0, 1, 2, 3);
                    reference.execute_reference(word);
                    assert!(execute(&mut actual, word));
                    for lane in 0..8 {
                        assert_eq!(actual.acc(lane), ACC_EDGES[(first + lane) % ACC_EDGES.len()]);
                    }
                    assert!(state(&actual) == state(&reference),
                        "mixed constructed f{function:02x} {s:04x}*{t:04x} result{first}");
                }
            }
        }
    }
}

#[test]
fn mixed_accumulate_dependent_sequence_preserves_every_vu_byte() {
    let mut seed = 0x048f_ffff_ffff_ffff;
    let mut actual = populated(&mut seed);
    let mut reference = actual.clone();
    for n in 0..32_768 {
        let vs = (random(&mut seed) & 31) as usize;
        let vt = (random(&mut seed) & 31) as usize;
        let vd = (random(&mut seed) & 31) as usize;
        let word = instruction(0x0d + (n & 1), (n as usize >> 1) & 15, vs, vt, vd);
        reference.execute_reference(word);
        assert!(execute(&mut actual, word));
        assert!(state(&actual) == state(&reference), "mixed sequence instruction{n}");
    }
}

#[cfg(target_arch = "arm")]
#[test]
fn mixed_accumulate_preserves_fpscr_including_sticky_qc() {
    unsafe fn read() -> u32 {
        let value;
        unsafe { core::arch::asm!("vmrs {value}, fpscr", value = out(reg) value,
            options(nomem, nostack, preserves_flags)); }
        value
    }
    unsafe fn write(value: u32) {
        unsafe { core::arch::asm!("vmsr fpscr, {value}", value = in(reg) value,
            options(nomem, nostack)); }
    }
    let mut vu = Vu::new();
    vu.regs[1] = [0xffff, 0x8000, 0, 1, 0x7fff, 0xffff, 0x8001, 0xffff];
    vu.regs[2] = [0x8000, 0xffff, 0xffff, 0xffff, 0x7fff, 1, 0x8000, 0xffff];
    let saved = unsafe { read() };
    for qc in [0, 1 << 27] {
        for function in [0x0d, 0x0e] {
            for e in 0..16 {
                for n in 0..8 { vu.set_acc(n, ACC_EDGES[(e+n) % ACC_EDGES.len()]); }
                let expected = saved & !(1 << 27) | qc;
                unsafe { write(expected) };
                assert!(execute(&mut vu, instruction(function, e, 1, 2, 3)));
                let observed = unsafe { read() };
                unsafe { write(saved) };
                assert_eq!(observed, expected, "mixed f{function:02x} e{e}");
            }
        }
    }
}

/// Compare complete public dispatch with acceleration enabled/disabled.
#[test]
#[ignore]
fn mixed_instruction_cost() {
    let mode = std::env::var("N64_VU_MODE").unwrap_or_else(|_| "neon".into());
    let function: u32 = std::env::var("N64_VU_FUNCTION").unwrap_or_else(|_| "14".into()).parse().unwrap();
    let count: u32 = std::env::var("N64_VU_COUNT").unwrap_or_else(|_| "200000".into()).parse().unwrap();
    assert!(matches!(function, 0x0d | 0x0e));
    let mut seed = 0xabc0_6423_44f1_8000;
    let mut vu = populated(&mut seed);
    vu.accelerated = mode == "neon";
    for n in 0..count {
        vu.regs[1][n as usize & 7] = (n as u16).wrapping_mul(31337) ^ (n >> 8) as u16;
        let word = std::hint::black_box(instruction(function, n as usize & 15, 1, 2, 1));
        vu.execute(word);
        std::hint::black_box(&vu);
    }
    let hash = state(&vu).into_iter().fold(0xcbf29ce484222325u64,
        |h, b| (h ^ u64::from(b)).wrapping_mul(0x100000001b3));
    eprintln!("mixed f{function:02x} {mode}: {count}, state hash {hash:016x}");
}

/// Tenths of a nanosecond per instruction of each function through the
/// public dispatch, accelerated and not, cycling through every element
/// field (on the Pi's 1 GHz A53, tenths of a cycle).
#[test]
#[ignore]
fn function_timings() {
    let count = 200_000u32;
    let mut seed = 0x7173_0b5e_2026_1030;
    for function in [0x00, 0x04, 0x05, 0x06, 0x07, 0x08, 0x0c, 0x0d, 0x0e, 0x0f, 0x10, 0x11, 0x14, 0x15,
        0x1d, 0x20, 0x23, 0x27, 0x28, 0x2c, 0x30, 0x31, 0x32, 0x33]
    {
        let mut line = format!("{function:02x}");
        for accelerated in [false, true] {
            let mut vu = populated(&mut seed);
            vu.accelerated = accelerated;
            let start = std::time::Instant::now();
            for n in 0..count {
                let word = std::hint::black_box(instruction(function, n as usize & 15, 1 + (n as usize & 3), 2, 1));
                vu.execute(word);
            }
            std::hint::black_box(&vu);
            line += &format!(" {:6}", start.elapsed().as_nanos() * 10 / u128::from(count));
        }
        eprintln!("{line}");
    }
}

/// Run separately under libcount.so for scalar/NEON instruction cost.
#[test]
#[ignore]
fn instruction_cost() {
    let mode = std::env::var("N64_VU_MODE").unwrap_or_else(|_| "neon".into());
    let neon = mode == "neon";
    let function: Option<u32> = std::env::var("N64_VU_FUNCTION").ok().map(|x| x.parse().unwrap());
    let count: u32 = std::env::var("N64_VU_COUNT").unwrap_or_else(|_| "100000".into()).parse().unwrap();
    let mut seed = 0xabc0_6423_44f1_8000;
    let mut vu = populated(&mut seed);
    for n in 0..count {
        vu.regs[1][n as usize & 7] = (n as u16).wrapping_mul(31337) ^ (n >> 8) as u16;
        let word = std::hint::black_box(instruction(function.unwrap_or(n & 1), (n as usize >> 1) & 15, 1, 2, 1));
        if neon {
            assert!(execute(&mut vu, word));
        } else {
            vu.execute_reference(word);
        }
        std::hint::black_box(&vu);
    }
    eprintln!("{mode}: {count}, {:04x?}", vu.regs[1]);
}
