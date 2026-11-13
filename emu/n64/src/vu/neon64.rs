//! The vector unit's instructions in 64-bit ARM's NEON, as neon.rs has them in 32-bit ARM's: the
//! multiplies, accumulates, adds, compares, merges and logic, and the single-lane ones' broadcast.
//! The scalar VU stays the reference and handles the rest (VMULQ, VMACU, the rounding and MPEG ones,
//! VABS, the clips). Here they are intrinsics, which a 64-bit ARM always has: each function reads vs
//! and the broadcast vt, and writes the VU after all of its reads, which keeps vd aliasing either
//! source exact. Saturating narrows are used where they say what is meant: FPSR.QC, which they set,
//! is read by nothing here.

use core::arch::aarch64::*;

use super::{BROADCAST, LANE_BITS, Vu};

/// NEON is in every 64-bit ARM.
pub(super) fn available() -> bool {
    true
}

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

type V = uint16x8_t;

/// An instruction's operands: vs, the broadcast vt, and vd's number.
struct Operands {
    s: V,
    t: V,
    d: usize,
}

/// Two halves of eight 32-bit lanes.
type Wide = (uint32x4_t, uint32x4_t);

#[inline(always)]
unsafe fn load(lanes: &[u16; 8]) -> V {
    unsafe { vld1q_u16(lanes.as_ptr()) }
}

#[inline(always)]
unsafe fn store(lanes: &mut [u16; 8], v: V) {
    unsafe { vst1q_u16(lanes.as_mut_ptr(), v) }
}

#[inline(always)]
unsafe fn sign(v: V) -> V {
    unsafe { vreinterpretq_u16_s16(vshrq_n_s16::<15>(vreinterpretq_s16_u16(v))) }
}

#[inline(always)]
unsafe fn signed_product(s: V, t: V) -> Wide {
    unsafe {
        let (s, t) = (vreinterpretq_s16_u16(s), vreinterpretq_s16_u16(t));
        (
            vreinterpretq_u32_s32(vmull_s16(vget_low_s16(s), vget_low_s16(t))),
            vreinterpretq_u32_s32(vmull_high_s16(s, t)),
        )
    }
}

#[inline(always)]
unsafe fn unsigned_product(s: V, t: V) -> Wide {
    unsafe { (vmull_u16(vget_low_u16(s), vget_low_u16(t)), vmull_high_u16(s, t)) }
}

/// The low 16 bits of each 32-bit lane.
#[inline(always)]
unsafe fn low16(w: Wide) -> V {
    unsafe { vmovn_high_u32(vmovn_u32(w.0), w.1) }
}

/// The high 16 bits of each 32-bit lane.
#[inline(always)]
unsafe fn high16(w: Wide) -> V {
    unsafe { vshrn_high_n_u32::<16>(vshrn_n_u32::<16>(w.0), w.1) }
}

/// Each 32-bit lane as a signed value clamped to signed 16 bits.
#[inline(always)]
unsafe fn clamped(w: Wide) -> V {
    unsafe {
        let low = vqmovn_s32(vreinterpretq_s32_u32(w.0));
        vreinterpretq_u16_s16(vqmovn_high_s32(low, vreinterpretq_s32_u32(w.1)))
    }
}

/// `a` less `b` << 16 in each lane, 16 bits apiece.
#[inline(always)]
unsafe fn less_shifted(a: Wide, b: V) -> Wide {
    unsafe { (vsubq_u32(a.0, vshll_n_u16::<16>(vget_low_u16(b))), vsubq_u32(a.1, vshll_high_n_u16::<16>(b))) }
}

/// A signed × unsigned product's slices (low, middle, high), `a` the signed one: an unsigned reading
/// of a negative a is a + 65536, so subtracting b << 16 gives the signed product.
#[inline(always)]
unsafe fn mixed_product(a: V, b: V) -> (V, V, V) {
    unsafe {
        let product = less_shifted(unsigned_product(a, b), vandq_u16(sign(a), b));
        let middle = high16(product);
        (low16(product), middle, sign(middle))
    }
}

/// Lanes where both operands are the smallest signed value, whose doubled product overflows.
#[inline(always)]
unsafe fn both_smallest(s: V, t: V) -> V {
    unsafe {
        let smallest = vdupq_n_u16(0x8000);
        vandq_u16(vceqq_u16(s, smallest), vceqq_u16(t, smallest))
    }
}

/// An instruction of function F (the word's low six bits).
#[inline(always)]
fn run<const F: u32>(vu: &mut Vu, word: u32) -> bool {
    let e = (word >> 21 & 15) as usize;
    let vt = (word >> 16 & 31) as usize;
    let vs = (word >> 11 & 31) as usize;
    let vd = (word >> 6 & 31) as usize;
    // Safety: NEON is always there, the registers and tables are whole vectors of 16 bytes.
    unsafe {
        let s = load(&vu.regs[vs]);
        let raw = vld1q_u8(vu.regs[vt].as_ptr().cast::<u8>());
        let t = vreinterpretq_u16_u8(vqtbl1q_u8(raw, vld1q_u8(BROADCAST[e].as_ptr())));
        let o = Operands { s, t, d: vd };
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
                store(&mut vu.acc_l, t);
                vu.single_lane_result(word, e, vt, vs & 7, vd);
            }
            0x37 | 0x3f => {}
            _ => return false,
        }
    }
    true
}

/// VMULF and VMULU: signed fractions, doubled and rounded. Only MIN * MIN exceeds a positive signed
/// 32 bits then: its high slice is 0, and the signed result saturates. The unsigned rule: a negative
/// high slice gives 0, differing high and middle signs FFFF, else the middle slice.
#[inline(always)]
unsafe fn fraction_multiply<const UNSIGNED: bool>(vu: &mut Vu, o: &Operands) {
    unsafe {
        let smallest = both_smallest(o.s, o.t);
        let product = signed_product(o.s, o.t);
        let round = vdupq_n_u32(0x8000);
        let doubled = (vaddq_u32(vshlq_n_u32::<1>(product.0), round), vaddq_u32(vshlq_n_u32::<1>(product.1), round));
        let (low, middle) = (low16(doubled), high16(doubled));
        let high = vbicq_u16(sign(middle), smallest);
        store(&mut vu.acc_l, low);
        store(&mut vu.acc_m, middle);
        store(&mut vu.acc_h, high);
        let result = if UNSIGNED {
            vbicq_u16(vorrq_u16(sign(veorq_u16(high, middle)), middle), high)
        } else {
            vbslq_u16(smallest, vdupq_n_u16(0x7fff), middle)
        };
        store(&mut vu.regs[o.d], result);
    }
}

/// VMUDL (0), the high half of unsigned products into the low slice; VMUDM (1) and VMUDN (2), exact
/// signed × unsigned products in all 48 bits (the signed one vs, or vt).
#[inline(always)]
unsafe fn low_multiply<const KIND: u8>(vu: &mut Vu, o: &Operands) {
    unsafe {
        if KIND == 0 {
            let low = high16(unsigned_product(o.s, o.t));
            let zero = vdupq_n_u16(0);
            store(&mut vu.acc_l, low);
            store(&mut vu.acc_m, zero);
            store(&mut vu.acc_h, zero);
            store(&mut vu.regs[o.d], low);
            return;
        }
        let (low, middle, high) = if KIND == 1 { mixed_product(o.s, o.t) } else { mixed_product(o.t, o.s) };
        store(&mut vu.acc_l, low);
        store(&mut vu.acc_m, middle);
        store(&mut vu.acc_h, high);
        store(&mut vu.regs[o.d], if KIND == 1 { middle } else { low });
    }
}

/// VMUDH replaces the 32 high and middle bits with the signed product, and clears the low slice;
/// VMADH adds the product to them modulo 2^32. The result is those 32 bits clamped to 16.
#[inline(always)]
unsafe fn high_multiply<const ACCUMULATE: bool>(vu: &mut Vu, o: &Operands) {
    unsafe {
        let mut product = signed_product(o.s, o.t);
        if ACCUMULATE {
            // Each little-endian 32-bit lane is then [middle16, high16].
            let (middle, high) = (load(&vu.acc_m), load(&vu.acc_h));
            product.0 = vaddq_u32(product.0, vreinterpretq_u32_u16(vzip1q_u16(middle, high)));
            product.1 = vaddq_u32(product.1, vreinterpretq_u32_u16(vzip2q_u16(middle, high)));
        } else {
            store(&mut vu.acc_l, vdupq_n_u16(0));
        }
        store(&mut vu.acc_m, low16(product));
        store(&mut vu.acc_h, high16(product));
        store(&mut vu.regs[o.d], clamped(product));
    }
}

/// A product added to the 48-bit accumulator, wrapping: VMADM (0) and VMADN (1), signed × unsigned
/// (the signed one vs, or vt); VMACF (2), doubled signed fractions (MIN * MIN's high slice 0, as
/// above); VMADL (3), the high half of unsigned products. The product is three slices, added slice by
/// slice with the carries. VMADM and VMACF clamp the high 32 bits to signed 16 (the middle slice when
/// the high one is its sign); VMADN and VMADL give the low slice then, else 0 below and FFFF above.
#[inline(always)]
unsafe fn accumulate<const KIND: u8>(vu: &mut Vu, o: &Operands) {
    unsafe {
        let (low, middle, high) = match KIND {
            0 => mixed_product(o.s, o.t),
            1 => mixed_product(o.t, o.s),
            2 => {
                let product = signed_product(o.s, o.t);
                let doubled = (vshlq_n_u32::<1>(product.0), vshlq_n_u32::<1>(product.1));
                let middle = high16(doubled);
                (low16(doubled), middle, vbicq_u16(sign(middle), both_smallest(o.s, o.t)))
            }
            _ => (high16(unsigned_product(o.s, o.t)), vdupq_n_u16(0), vdupq_n_u16(0)),
        };
        // The low slices, then the middle ones with the low carries.
        let acc = load(&vu.acc_l);
        let sum = (vaddl_u16(vget_low_u16(acc), vget_low_u16(low)), vaddl_high_u16(acc, low));
        let (low, carry) = (low16(sum), high16(sum));
        let acc = load(&vu.acc_m);
        let sum = (
            vaddw_u16(vaddl_u16(vget_low_u16(acc), vget_low_u16(middle)), vget_low_u16(carry)),
            vaddw_high_u16(vaddl_high_u16(acc, middle), carry),
        );
        let (middle, carry) = (low16(sum), high16(sum));
        // The high slices wrap at 16 bits.
        let high = vaddq_u16(vaddq_u16(load(&vu.acc_h), high), carry);
        store(&mut vu.acc_l, low);
        store(&mut vu.acc_m, middle);
        store(&mut vu.acc_h, high);
        // In range where the high slice is the middle one's sign.
        let in_range = vceqq_u16(high, sign(middle));
        let above = sign(high);
        let result = if KIND == 0 || KIND == 2 {
            vbslq_u16(in_range, middle, veorq_u16(vdupq_n_u16(0x7fff), above))
        } else {
            vbslq_u16(in_range, low, vmvnq_u16(above))
        };
        store(&mut vu.regs[o.d], result);
    }
}

/// VADD and VSUB: signed, with VCO's carry for each lane, the 17-bit sum wrapped into the low slice
/// and clamped into vd; VCO is cleared.
#[inline(always)]
unsafe fn add_subtract<const SUBTRACT: bool>(vu: &mut Vu, o: &Operands) {
    unsafe {
        // All ones where the lane's carry is set: subtracting it adds 1.
        let carry = vreinterpretq_s16_u16(vtstq_u16(vdupq_n_u16(vu.vco), vld1q_u16(LANE_BITS.as_ptr())));
        let (s, t) = (vreinterpretq_s16_u16(o.s), vreinterpretq_s16_u16(o.t));
        let sum = if SUBTRACT {
            (
                vaddw_s16(vsubl_s16(vget_low_s16(s), vget_low_s16(t)), vget_low_s16(carry)),
                vaddw_high_s16(vsubl_high_s16(s, t), carry),
            )
        } else {
            (
                vsubw_s16(vaddl_s16(vget_low_s16(s), vget_low_s16(t)), vget_low_s16(carry)),
                vsubw_high_s16(vaddl_high_s16(s, t), carry),
            )
        };
        let wide = (vreinterpretq_u32_s32(sum.0), vreinterpretq_u32_s32(sum.1));
        store(&mut vu.acc_l, low16(wide));
        store(&mut vu.regs[o.d], clamped(wide));
    }
    vu.vco = 0;
}

/// VADDC and VSUBC: unsigned, the wrapped result into the low slice and vd; the new VCO, of carries
/// (or borrows, and for VSUBC differences). A lane's bits are summed across the vector.
#[inline(always)]
unsafe fn carries<const SUBTRACT: bool>(vu: &mut Vu, o: &Operands) -> u16 {
    unsafe {
        let (carry_bits, equal_bits) = (vld1q_u16(LANE_BITS.as_ptr()), vld1q_u16(LANE_BITS.as_ptr().add(8)));
        let (result, flags) = if SUBTRACT {
            let borrow = vandq_u16(vcgtq_u16(o.t, o.s), carry_bits);
            let different = vbicq_u16(equal_bits, vceqq_u16(o.s, o.t));
            (vsubq_u16(o.s, o.t), vorrq_u16(borrow, different))
        } else {
            let sum = (vaddl_u16(vget_low_u16(o.s), vget_low_u16(o.t)), vaddl_high_u16(o.s, o.t));
            let carry = high16(sum);
            (low16(sum), vandq_u16(vtstq_u16(carry, carry), carry_bits))
        };
        store(&mut vu.acc_l, result);
        store(&mut vu.regs[o.d], result);
        vaddvq_u16(flags)
    }
}

/// VLT (0), VEQ (1), VNE (2), VGE (3): signed compares with VCO's carry and not-equal bits, as
/// `execute_reference` has them; the lanes where they hold pick vs (else vt) for the low slice and vd,
/// and are VCC. VCO is cleared.
#[inline(always)]
unsafe fn compare<const KIND: u8>(vu: &mut Vu, o: &Operands) {
    unsafe {
        let (carry_bits, equal_bits) = (vld1q_u16(LANE_BITS.as_ptr()), vld1q_u16(LANE_BITS.as_ptr().add(8)));
        let vco = vdupq_n_u16(vu.vco);
        let (carry, unequal) = (vtstq_u16(vco, carry_bits), vtstq_u16(vco, equal_bits));
        let equal = vceqq_u16(o.s, o.t);
        let (s, t) = (vreinterpretq_s16_u16(o.s), vreinterpretq_s16_u16(o.t));
        let holds = match KIND {
            0 => vorrq_u16(vcgtq_s16(t, s), vandq_u16(vandq_u16(carry, unequal), equal)),
            1 => vbicq_u16(equal, unequal),
            2 => vornq_u16(unequal, equal),
            _ => vorrq_u16(vcgtq_s16(s, t), vbicq_u16(equal, vandq_u16(carry, unequal))),
        };
        let result = vbslq_u16(holds, o.s, o.t);
        store(&mut vu.acc_l, result);
        store(&mut vu.regs[o.d], result);
        vu.vcc = vaddvq_u16(vandq_u16(holds, carry_bits));
    }
    vu.vco = 0;
}

/// VMRG: vs where VCC's lane is set, else vt; VCO is cleared.
#[inline(always)]
unsafe fn merge(vu: &mut Vu, o: &Operands) {
    unsafe {
        let set = vtstq_u16(vdupq_n_u16(vu.vcc), vld1q_u16(LANE_BITS.as_ptr()));
        let result = vbslq_u16(set, o.s, o.t);
        store(&mut vu.acc_l, result);
        store(&mut vu.regs[o.d], result);
    }
    vu.vco = 0;
}

/// VAND, VOR, VXOR (0, 1, 2) and their inversions, into the low slice and vd.
#[inline(always)]
unsafe fn logic<const KIND: u8, const INVERT: bool>(vu: &mut Vu, o: &Operands) {
    unsafe {
        let result = match KIND {
            0 => vandq_u16(o.s, o.t),
            1 => vorrq_u16(o.s, o.t),
            _ => veorq_u16(o.s, o.t),
        };
        let result = if INVERT { vmvnq_u16(result) } else { result };
        store(&mut vu.acc_l, result);
        store(&mut vu.regs[o.d], result);
    }
}
