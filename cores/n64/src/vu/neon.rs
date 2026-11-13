//! The vector unit's instructions in Cortex-A53's eight-lane NEON: the
//! multiplies, accumulates, adds, compares, merges and logic, and the
//! single-lane ones' broadcast. The scalar VU stays the reference and
//! handles the rest (VMULQ, VMACU, the rounding and MPEG ones, VABS, the
//! clips).
//!
//! These use ordinary integer NEON operations rather than saturating
//! ones: besides the result, RSP exposes all 48 accumulator bits, and the
//! host's sticky FPSCR.QC flag must remain untouched.

#![cfg(target_arch = "arm")]

use super::{BROADCAST, LANE_BITS, Vu};

/// Linux supplies the process's usable CPU features in its auxiliary
/// vector. Rust's stable AArch32 cfg/detection APIs do not expose NEON.
pub(super) fn available() -> bool {
    #[cfg(target_os = "linux")]
    {
        static AVAILABLE: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
            unsafe extern "C" {
                fn getauxval(kind: usize) -> usize;
            }
            // Linux UAPI: include/uapi/linux/auxvec.h and
            // arch/arm/include/uapi/asm/hwcap.h.
            const AT_HWCAP: usize = 16;
            const HWCAP_NEON: usize = 1 << 12;
            unsafe { getauxval(AT_HWCAP) & HWCAP_NEON != 0 }
        });
        *AVAILABLE
    }
    #[cfg(not(target_os = "linux"))]
    false
}

/// An instruction's registers: vs, vt (broadcast by `index`) and vd.
struct Operands {
    s: *const u16,
    t: *const u16,
    index: *const u8,
    d: *mut u16,
}

/// Each instruction is one asm block that reads vs into q0 and the
/// broadcast vt into q3 (with these lines, `operands!` below), and stores
/// its results straight into the VU after all of its reads, which keeps
/// vd aliasing either source exact. Only caller-saved q0-q3 and q8-q15
/// are used, and no saturating operation (they would set FPSCR.QC). The
/// clobbers name them as their D halves: a build without NEON among its
/// target features (the armv7 targets' default, whatever -C target-cpu
/// says) drops a Q register's clobber and keeps its own values there.
/// The rest of the template and the block's own operands follow, each
/// line ending in a comma.
macro_rules! vector {
    ($o:expr, $($rest:tt)*) => {
        core::arch::asm!(
            // Stable Rust cannot attach a per-function AArch32 NEON
            // target_feature. Enable its assembler encodings locally,
            // using a superset that does not disable an A53's VFPv4/FMA.
            // The instructions remain ARMv7 NEON; available() gates use.
            ".fpu neon-fp-armv8",
            "vld1.16 {{q0}}, [{s}]",
            "vld1.16 {{q1}}, [{t}]",
            "vld1.8 {{d4, d5}}, [{index}]",
            "vtbl.8 d6, {{d2, d3}}, d4",
            "vtbl.8 d7, {{d2, d3}}, d5",
            $($rest)*
            s = in(reg) $o.s,
            t = in(reg) $o.t,
            index = in(reg) $o.index,
            d = in(reg) $o.d,
            out("d0") _, out("d1") _, out("d2") _, out("d3") _, out("d4") _, out("d5") _, out("d6") _, out("d7") _,
            out("d16") _, out("d17") _, out("d18") _, out("d19") _, out("d20") _, out("d21") _, out("d22") _,
            out("d23") _, out("d24") _, out("d25") _, out("d26") _, out("d27") _, out("d28") _, out("d29") _,
            out("d30") _, out("d31") _,
            options(nostack, preserves_flags),
        )
    };
}

/// The functions below, for the dispatch and for compiled RSP code: each
/// has an entry of its own, so a call site there always reaches the same
/// code instead of a jump through a table by function.
macro_rules! functions {
    ($($f:literal)*) => {
        /// Returns false without changing the VU for an unsupported function.
        #[inline(always)]
        pub(super) fn execute(vu: &mut Vu, word: u32) -> bool {
            match word & 63 {
                $($f => run::<$f>(vu, word),)*
                _ => false,
            }
        }

        /// The entry for an instruction's function, if it has one.
        pub(super) fn entry(word: u32) -> Option<extern "C" fn(*mut Vu, u32)> {
            match word & 63 {
                $($f => Some(enter::<$f>),)*
                _ => None,
            }
        }
    };
}

functions!(0x00 0x01 0x04 0x05 0x06 0x07 0x08 0x0c 0x0d 0x0e 0x0f 0x10 0x11 0x14 0x15 0x1d 0x20 0x21 0x22 0x23
    0x27 0x28 0x29 0x2a 0x2b 0x2c 0x2d 0x30 0x31 0x32 0x33 0x34 0x35 0x36 0x37 0x3f);

/// Vu::execute for one function: the scalar VU without acceleration.
extern "C" fn enter<const F: u32>(vu: *mut Vu, word: u32) {
    // Safety: compiled RSP code passes the machine's VU, which nothing else
    // holds during the call.
    let vu = unsafe { &mut *vu };
    if vu.accelerated && run::<F>(vu, word) {
        if vu.profiling {
            vu.native_count += 1;
        }
    } else {
        vu.execute_reference(word);
    }
}

/// An instruction of function F (the word's low six bits).
#[inline(always)]
fn run<const F: u32>(vu: &mut Vu, word: u32) -> bool {
    let e = (word >> 21 & 15) as usize;
    let vt = (word >> 16 & 31) as usize;
    let vs = (word >> 11 & 31) as usize;
    let vd = (word >> 6 & 31) as usize;
    let regs = vu.regs.as_mut_ptr();
    // Safety: the fields are five bits, so each register is within regs.
    let o = unsafe {
        Operands {
            s: regs.add(vs).cast::<u16>(),
            t: regs.add(vt).cast::<u16>(),
            index: BROADCAST[e].as_ptr(),
            d: regs.add(vd).cast::<u16>(),
        }
    };
    // Safety (every block below): `o` and the accumulator pointers are to
    // complete 16-byte vectors of this VU, which is borrowed mutably here.
    unsafe {
        match F {
            0x00 => fraction_multiply::<false>(vu, &o),
            0x01 => fraction_multiply::<true>(vu, &o),
            0x04 => low_multiply::<0>(vu, &o),
            0x05 => low_multiply::<1>(vu, &o),
            0x06 => low_multiply::<2>(vu, &o),
            0x07 => high_multiply::<false>(vu, &o),
            0x0f => high_multiply::<true>(vu, &o),
            0x08 => accumulate::<2>(vu, &o),
            0x0c => accumulate::<3>(vu, &o),
            0x0d => accumulate::<0>(vu, &o),
            0x0e => accumulate::<1>(vu, &o),
            0x10 => add_subtract::<false>(vu, &o),
            0x11 => add_subtract::<true>(vu, &o),
            0x14 => vu.vco = carries::<false>(vu, &o),
            0x15 => vu.vco = carries::<true>(vu, &o),
            0x1d => {
                vu.regs[vd] = match e {
                    8 => vu.acc_h,
                    9 => vu.acc_m,
                    10 => vu.acc_l,
                    _ => [0; 8],
                }
            }
            0x20 => compare::<0>(vu, &o),
            0x21 => compare::<1>(vu, &o),
            0x22 => compare::<2>(vu, &o),
            0x23 => compare::<3>(vu, &o),
            0x27 => merge(vu, &o),
            0x28 => logic::<0, false>(vu, &o),
            0x29 => logic::<0, true>(vu, &o),
            0x2a => logic::<1, false>(vu, &o),
            0x2b => logic::<1, true>(vu, &o),
            0x2c => logic::<2, false>(vu, &o),
            0x2d => logic::<2, true>(vu, &o),
            0x30..=0x36 => {
                core::arch::asm!(
                    ".fpu neon-fp-armv8",
                    "vld1.16 {{q1}}, [{t}]",
                    "vld1.8 {{d4, d5}}, [{index}]",
                    "vtbl.8 d6, {{d2, d3}}, d4",
                    "vtbl.8 d7, {{d2, d3}}, d5",
                    "vst1.16 {{q3}}, [{low}]",
                    t = in(reg) o.t,
                    index = in(reg) o.index,
                    low = in(reg) vu.acc_l.as_mut_ptr(),
                    out("d2") _, out("d3") _, out("d4") _, out("d5") _, out("d6") _, out("d7") _,
                    options(nostack, preserves_flags),
                );
                vu.single_lane_result(word, e, vt, vs & 7, vd);
            }
            0x37 | 0x3f => {}
            _ => return false,
        }
    }
    true
}

/// VMULF and VMULU: signed fractions, doubled and rounded. Only MIN * MIN
/// exceeds a positive signed 32 bits then: its high slice is 0, and the
/// signed result saturates. The unsigned rule: a negative high slice gives
/// 0, differing high and middle signs FFFF, else the middle slice.
#[inline(always)]
unsafe fn fraction_multiply<const UNSIGNED: bool>(vu: &mut Vu, o: &Operands) {
    unsafe {
        vector!(o,
            "vmull.s16 q10, d0, d6",
            "vmull.s16 q11, d1, d7",
            "vmov.i16 q9, #0x8000",
            "vceq.i16 q12, q0, q9",
            "vceq.i16 q13, q3, q9",
            "vand q12, q12, q13",
            "vshl.i32 q10, q10, #1",
            "vshl.i32 q11, q11, #1",
            "vmov.i32 q9, #0x8000",
            "vadd.i32 q10, q10, q9",
            "vadd.i32 q11, q11, q9",
            "vmovn.i32 d2, q10",
            "vmovn.i32 d3, q11",
            "vshrn.i32 d4, q10, #16",
            "vshrn.i32 d5, q11, #16",
            "vshr.s16 q8, q2, #15",
            "vbic q8, q8, q12",
            "vst1.16 {{q1}}, [{low}]",
            "vst1.16 {{q2}}, [{middle}]",
            "vst1.16 {{q8}}, [{high}]",
            ".if {unsigned}",
            "veor q9, q8, q2",
            "vshr.s16 q9, q9, #15",
            "vorr q9, q9, q2",
            "vbic q9, q9, q8",
            "vst1.16 {{q9}}, [{d}]",
            ".else",
            "vmvn.i16 q9, #0x8000",
            "vbsl q12, q9, q2",
            "vst1.16 {{q12}}, [{d}]",
            ".endif",
            unsigned = const UNSIGNED as u8,
            low = in(reg) vu.acc_l.as_mut_ptr(),
            middle = in(reg) vu.acc_m.as_mut_ptr(),
            high = in(reg) vu.acc_h.as_mut_ptr(),
        );
    }
}

/// VMUDL (0), the high half of unsigned products into the low slice;
/// VMUDM (1) and VMUDN (2), exact signed × unsigned products in all 48
/// bits (the signed one vs, or vt). An unsigned reading of a negative
/// signed a is a + 65536: subtracting b << 16 gives the signed product.
#[inline(always)]
unsafe fn low_multiply<const KIND: u8>(vu: &mut Vu, o: &Operands) {
    unsafe {
        vector!(o,
            "vmull.u16 q10, d0, d6",
            "vmull.u16 q11, d1, d7",
            ".if {kind} == 0",
            "vshrn.i32 d2, q10, #16",
            "vshrn.i32 d3, q11, #16",
            "vmov.i16 q2, #0",
            "vst1.16 {{q1}}, [{low}]",
            "vst1.16 {{q2}}, [{middle}]",
            "vst1.16 {{q2}}, [{high}]",
            "vst1.16 {{q1}}, [{d}]",
            ".else",
            ".if {kind} == 1",
            "vshr.s16 q8, q0, #15",
            "vand q8, q8, q3",
            ".else",
            "vshr.s16 q8, q3, #15",
            "vand q8, q8, q0",
            ".endif",
            "vshll.u16 q9, d16, #16",
            "vsub.i32 q10, q10, q9",
            "vshll.u16 q9, d17, #16",
            "vsub.i32 q11, q11, q9",
            "vmovn.i32 d2, q10",
            "vmovn.i32 d3, q11",
            "vshrn.i32 d4, q10, #16",
            "vshrn.i32 d5, q11, #16",
            "vshr.s16 q8, q2, #15",
            "vst1.16 {{q1}}, [{low}]",
            "vst1.16 {{q2}}, [{middle}]",
            "vst1.16 {{q8}}, [{high}]",
            ".if {kind} == 1",
            "vst1.16 {{q2}}, [{d}]",
            ".else",
            "vst1.16 {{q1}}, [{d}]",
            ".endif",
            ".endif",
            kind = const KIND,
            low = in(reg) vu.acc_l.as_mut_ptr(),
            middle = in(reg) vu.acc_m.as_mut_ptr(),
            high = in(reg) vu.acc_h.as_mut_ptr(),
        );
    }
}

/// VMUDH replaces the 32 high and middle bits with the signed product, and
/// clears the low slice; VMADH adds the product to them modulo 2^32. The
/// result is those 32 bits clamped to 16, with min and max (not the
/// saturating forms).
#[inline(always)]
unsafe fn high_multiply<const ACCUMULATE: bool>(vu: &mut Vu, o: &Operands) {
    unsafe {
        vector!(o,
            "vmull.s16 q10, d0, d6",
            "vmull.s16 q11, d1, d7",
            ".if {accumulate}",
            "vld1.16 {{q8}}, [{middle}]",
            "vld1.16 {{q9}}, [{high}]",
            // Each little-endian 32-bit lane is then [middle16, high16].
            "vzip.16 q8, q9",
            "vadd.i32 q10, q10, q8",
            "vadd.i32 q11, q11, q9",
            ".else",
            "vmov.i16 q8, #0",
            "vst1.16 {{q8}}, [{low}]",
            ".endif",
            "vmovn.i32 d2, q10",
            "vmovn.i32 d3, q11",
            "vshrn.i32 d4, q10, #16",
            "vshrn.i32 d5, q11, #16",
            "vst1.16 {{q1}}, [{middle}]",
            "vst1.16 {{q2}}, [{high}]",
            "vmov.i32 q8, #0x8000",
            "vneg.s32 q8, q8",
            "vmvn q9, q8",
            "vmax.s32 q10, q10, q8",
            "vmax.s32 q11, q11, q8",
            "vmin.s32 q10, q10, q9",
            "vmin.s32 q11, q11, q9",
            "vmovn.i32 d2, q10",
            "vmovn.i32 d3, q11",
            "vst1.16 {{q1}}, [{d}]",
            accumulate = const ACCUMULATE as u8,
            low = in(reg) vu.acc_l.as_mut_ptr(),
            middle = in(reg) vu.acc_m.as_mut_ptr(),
            high = in(reg) vu.acc_h.as_mut_ptr(),
        );
    }
}

/// A product added to the 48-bit accumulator, wrapping: VMADM (0) and
/// VMADN (1), signed × unsigned (the signed one vs, or vt); VMACF (2),
/// doubled signed fractions (MIN * MIN's high slice 0, as above); VMADL
/// (3), the high half of unsigned products. The product is three slices
/// (q1 low, q2 middle, q8 high), added slice by slice with the carries.
/// VMADM and VMACF clamp the high 32 bits to signed 16 (the middle slice
/// when the high one is its sign); VMADN and VMADL give the low slice then,
/// else 0 below and FFFF above.
#[inline(always)]
unsafe fn accumulate<const KIND: u8>(vu: &mut Vu, o: &Operands) {
    unsafe {
        vector!(o,
            ".if {kind} <= 1",
            ".if {kind} == 0",
            "vmov q1, q0",
            "vmov q2, q3",
            ".else",
            "vmov q1, q3",
            "vmov q2, q0",
            ".endif",
            "vmull.u16 q10, d2, d4",
            "vmull.u16 q11, d3, d5",
            "vshr.s16 q8, q1, #15",
            "vand q8, q8, q2",
            "vshll.u16 q9, d16, #16",
            "vsub.i32 q10, q10, q9",
            "vshll.u16 q9, d17, #16",
            "vsub.i32 q11, q11, q9",
            "vmovn.i32 d2, q10",
            "vmovn.i32 d3, q11",
            "vshrn.i32 d4, q10, #16",
            "vshrn.i32 d5, q11, #16",
            "vshr.s16 q8, q2, #15",
            ".endif",
            ".if {kind} == 2",
            "vmull.s16 q10, d0, d6",
            "vmull.s16 q11, d1, d7",
            "vmov.i16 q9, #0x8000",
            "vceq.i16 q12, q0, q9",
            "vceq.i16 q13, q3, q9",
            "vand q12, q12, q13",
            "vshl.i32 q10, q10, #1",
            "vshl.i32 q11, q11, #1",
            "vmovn.i32 d2, q10",
            "vmovn.i32 d3, q11",
            "vshrn.i32 d4, q10, #16",
            "vshrn.i32 d5, q11, #16",
            "vshr.s16 q8, q2, #15",
            "vbic q8, q8, q12",
            ".endif",
            ".if {kind} == 3",
            "vmull.u16 q10, d0, d6",
            "vmull.u16 q11, d1, d7",
            "vshrn.i32 d2, q10, #16",
            "vshrn.i32 d3, q11, #16",
            "vmov.i16 q2, #0",
            "vmov.i16 q8, #0",
            ".endif",
            // The low slices, their carries kept in q9.
            "vld1.16 {{q9}}, [{low}]",
            "vaddl.u16 q10, d18, d2",
            "vaddl.u16 q11, d19, d3",
            "vshrn.i32 d18, q10, #16",
            "vshrn.i32 d19, q11, #16",
            "vmovn.i32 d2, q10",
            "vmovn.i32 d3, q11",
            // The middle slices and the low carries.
            "vld1.16 {{q12}}, [{middle}]",
            "vaddl.u16 q10, d24, d4",
            "vaddl.u16 q11, d25, d5",
            "vaddw.u16 q10, q10, d18",
            "vaddw.u16 q11, q11, d19",
            "vshrn.i32 d18, q10, #16",
            "vshrn.i32 d19, q11, #16",
            "vmovn.i32 d4, q10",
            "vmovn.i32 d5, q11",
            // The high slices wrap at 16 bits.
            "vld1.16 {{q12}}, [{high}]",
            "vadd.i16 q12, q12, q8",
            "vadd.i16 q12, q12, q9",
            "vst1.16 {{q1}}, [{low}]",
            "vst1.16 {{q2}}, [{middle}]",
            "vst1.16 {{q12}}, [{high}]",
            // In range where the high slice is the middle one's sign.
            "vshr.s16 q8, q2, #15",
            "vceq.i16 q8, q12, q8",
            "vshr.s16 q9, q12, #15",
            ".if {signed}",
            "vmvn.i16 q10, #0x8000",
            "veor q10, q10, q9",
            "vbsl q8, q2, q10",
            ".else",
            "vmvn q10, q9",
            "vbsl q8, q1, q10",
            ".endif",
            "vst1.16 {{q8}}, [{d}]",
            kind = const KIND,
            signed = const (KIND == 0 || KIND == 2) as u8,
            low = in(reg) vu.acc_l.as_mut_ptr(),
            middle = in(reg) vu.acc_m.as_mut_ptr(),
            high = in(reg) vu.acc_h.as_mut_ptr(),
        );
    }
}

/// VADD and VSUB: signed, with VCO's carry for each lane, the 17-bit sum
/// wrapped into the low slice and clamped into vd; VCO is cleared.
#[inline(always)]
unsafe fn add_subtract<const SUBTRACT: bool>(vu: &mut Vu, o: &Operands) {
    unsafe {
        vector!(o,
            "vdup.16 q8, {vco}",
            "vld1.16 {{q9}}, [{bits}]",
            // All ones where the lane's carry is set: subtracting it adds 1.
            "vtst.16 q8, q8, q9",
            ".if {subtract}",
            "vsubl.s16 q10, d0, d6",
            "vsubl.s16 q11, d1, d7",
            "vaddw.s16 q10, q10, d16",
            "vaddw.s16 q11, q11, d17",
            ".else",
            "vaddl.s16 q10, d0, d6",
            "vaddl.s16 q11, d1, d7",
            "vsubw.s16 q10, q10, d16",
            "vsubw.s16 q11, q11, d17",
            ".endif",
            "vmovn.i32 d24, q10",
            "vmovn.i32 d25, q11",
            "vst1.16 {{q12}}, [{low}]",
            "vmov.i32 q13, #0x8000",
            "vneg.s32 q13, q13",
            "vmvn q14, q13",
            "vmax.s32 q10, q10, q13",
            "vmax.s32 q11, q11, q13",
            "vmin.s32 q10, q10, q14",
            "vmin.s32 q11, q11, q14",
            "vmovn.i32 d24, q10",
            "vmovn.i32 d25, q11",
            "vst1.16 {{q12}}, [{d}]",
            subtract = const SUBTRACT as u8,
            vco = in(reg) u32::from(vu.vco),
            bits = in(reg) LANE_BITS.as_ptr(),
            low = in(reg) vu.acc_l.as_mut_ptr(),
        );
    }
    vu.vco = 0;
}

/// VADDC and VSUBC: unsigned, the wrapped result into the low slice and
/// vd; the new VCO, of carries (or borrows, and for VSUBC differences).
/// A lane's bits are summed across the vector into the register.
#[inline(always)]
unsafe fn carries<const SUBTRACT: bool>(vu: &mut Vu, o: &Operands) -> u16 {
    let vco: u32;
    unsafe {
        vector!(o,
            "vld1.16 {{d16, d17, d18, d19}}, [{bits}]",
            ".if {subtract}",
            "vsub.i16 q12, q0, q3",
            "vcgt.u16 q10, q3, q0",
            "vceq.i16 q11, q0, q3",
            "vand q10, q10, q8",
            "vbic q11, q9, q11",
            "vorr q10, q10, q11",
            ".else",
            "vaddl.u16 q10, d0, d6",
            "vaddl.u16 q11, d1, d7",
            "vmovn.i32 d24, q10",
            "vmovn.i32 d25, q11",
            "vshrn.i32 d20, q10, #16",
            "vshrn.i32 d21, q11, #16",
            "vtst.16 q10, q10, q10",
            "vand q10, q10, q8",
            ".endif",
            "vst1.16 {{q12}}, [{low}]",
            "vst1.16 {{q12}}, [{d}]",
            "vpadd.i16 d20, d20, d21",
            "vpadd.i16 d20, d20, d20",
            "vpadd.i16 d20, d20, d20",
            "vmov.u16 {vco}, d20[0]",
            subtract = const SUBTRACT as u8,
            vco = out(reg) vco,
            bits = in(reg) LANE_BITS.as_ptr(),
            low = in(reg) vu.acc_l.as_mut_ptr(),
        );
    }
    vco as u16
}

/// VLT (0), VEQ (1), VNE (2), VGE (3): signed compares with VCO's carry
/// and not-equal bits, as `execute_reference` has them; the lanes where
/// they hold pick vs (else vt) for the low slice and vd, and are VCC. VCO
/// is cleared.
#[inline(always)]
unsafe fn compare<const KIND: u8>(vu: &mut Vu, o: &Operands) {
    let vcc: u32;
    unsafe {
        vector!(o,
            "vld1.16 {{d16, d17, d18, d19}}, [{bits}]",
            "vdup.16 q10, {vco}",
            "vtst.16 q11, q10, q8",
            "vtst.16 q12, q10, q9",
            "vceq.i16 q13, q0, q3",
            ".if {kind} == 0",
            "vand q11, q11, q12",
            "vand q11, q11, q13",
            "vcgt.s16 q14, q3, q0",
            "vorr q14, q14, q11",
            ".endif",
            ".if {kind} == 1",
            "vbic q14, q13, q12",
            ".endif",
            ".if {kind} == 2",
            "vorn q14, q12, q13",
            ".endif",
            ".if {kind} == 3",
            "vand q11, q11, q12",
            "vbic q11, q13, q11",
            "vcgt.s16 q14, q0, q3",
            "vorr q14, q14, q11",
            ".endif",
            "vmov q15, q14",
            "vbsl q15, q0, q3",
            "vst1.16 {{q15}}, [{low}]",
            "vst1.16 {{q15}}, [{d}]",
            "vand q14, q14, q8",
            "vpadd.i16 d28, d28, d29",
            "vpadd.i16 d28, d28, d28",
            "vpadd.i16 d28, d28, d28",
            "vmov.u16 {vcc}, d28[0]",
            kind = const KIND,
            vco = in(reg) u32::from(vu.vco),
            vcc = lateout(reg) vcc,
            bits = in(reg) LANE_BITS.as_ptr(),
            low = in(reg) vu.acc_l.as_mut_ptr(),
        );
    }
    vu.vcc = vcc as u16;
    vu.vco = 0;
}

/// VMRG: vs where VCC's lane is set, else vt; VCO is cleared.
#[inline(always)]
unsafe fn merge(vu: &mut Vu, o: &Operands) {
    unsafe {
        vector!(o,
            "vld1.16 {{q8}}, [{bits}]",
            "vdup.16 q10, {vcc}",
            "vtst.16 q10, q10, q8",
            "vbsl q10, q0, q3",
            "vst1.16 {{q10}}, [{low}]",
            "vst1.16 {{q10}}, [{d}]",
            vcc = in(reg) u32::from(vu.vcc),
            bits = in(reg) LANE_BITS.as_ptr(),
            low = in(reg) vu.acc_l.as_mut_ptr(),
        );
    }
    vu.vco = 0;
}

/// VAND, VOR, VXOR (0, 1, 2) and their inversions, into the low slice
/// and vd.
#[inline(always)]
unsafe fn logic<const KIND: u8, const INVERT: bool>(vu: &mut Vu, o: &Operands) {
    unsafe {
        vector!(o,
            ".if {kind} == 0",
            "vand q8, q0, q3",
            ".endif",
            ".if {kind} == 1",
            "vorr q8, q0, q3",
            ".endif",
            ".if {kind} == 2",
            "veor q8, q0, q3",
            ".endif",
            ".if {invert}",
            "vmvn q8, q8",
            ".endif",
            "vst1.16 {{q8}}, [{low}]",
            "vst1.16 {{q8}}, [{d}]",
            kind = const KIND,
            invert = const INVERT as u8,
            low = in(reg) vu.acc_l.as_mut_ptr(),
        );
    }
}
