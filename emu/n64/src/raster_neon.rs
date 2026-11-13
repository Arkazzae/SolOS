//! NEON for the drawing threads on 32-bit and 64-bit ARM (Cortex-A53 / AArch64): a pixel's color channels
//! r, g, b, a as the four 16-bit lanes of a D register (in uint16x4_t on AArch64). Each operation is a small
//! asm block or NEON intrinsic on lanes that the compiler keeps in vector registers, so the spans in
//! raster.rs keep their shape and their scalar steps; `pixel()` stays the reference. Every operation is exact
//! for the ranges the spans give it, as noted at each: color channels and texels are 0-255, filter weights
//! at most 32.
//!
//! Texels come to the lanes packed as four bytes (r, g, b, a from the low byte up), decoded by small tables
//! and a few integer operations, then widened here.
//!
//! As in the RSP's vector unit, no saturating operation is used (they would set FPSCR.QC), only caller-saved
//! registers are named on 32-bit ARM (q8-q15), clobbered as their D halves, and `available()` gates every use.

#![cfg(any(target_arch = "arm", target_arch = "aarch64"))]

#[cfg(target_arch = "arm")]
use core::arch::asm;

/// Whether this CPU has NEON (Linux's HWCAP, as for the RSP's vector unit).
pub(crate) fn available() -> bool {
    crate::vu::neon_available()
}

/// Four 16-bit lanes of a NEON D register: a color's r, g, b and a, or other values a lane each.
#[cfg(target_arch = "arm")]
#[derive(Clone, Copy)]
pub(crate) struct Lanes(pub(crate) f64);

#[cfg(target_arch = "arm")]
impl Lanes {
    /// Lanes 0 and 1 from the low and high halves of `low`, lanes 2 and 3 from those of `high`.
    #[inline(always)]
    pub(crate) fn pair(low: u32, high: u32) -> Lanes {
        Lanes(f64::from_bits(u64::from(high) << 32 | u64::from(low)))
    }

    /// The same value in all four lanes.
    #[inline(always)]
    pub(crate) fn splat(value: u16) -> Lanes {
        let half = u32::from(value) * 0x1_0001;
        Lanes::pair(half, half)
    }

    /// Lane 3 (a color's alpha).
    #[inline(always)]
    pub(crate) fn lane3(self) -> u32 {
        (self.0.to_bits() >> 48) as u32
    }

    /// Lanes 0 and 1, then lanes 2 and 3, as `pair` takes them.
    #[inline(always)]
    pub(crate) fn halves(self) -> (u32, u32) {
        let bits = self.0.to_bits();
        (bits as u32, (bits >> 32) as u32)
    }
}

/// Four 16-bit lanes of a NEON D register on 64-bit ARM.
#[cfg(target_arch = "aarch64")]
#[derive(Clone, Copy)]
pub(crate) struct Lanes(pub(crate) core::arch::aarch64::uint16x4_t);

#[cfg(target_arch = "aarch64")]
impl Lanes {
    /// Lanes 0 and 1 from the low and high halves of `low`, lanes 2 and 3 from those of `high`.
    #[inline(always)]
    pub(crate) fn pair(low: u32, high: u32) -> Lanes {
        let bits = u64::from(high) << 32 | u64::from(low);
        Lanes(unsafe { core::arch::aarch64::vcreate_u16(bits) })
    }

    /// The same value in all four lanes.
    #[inline(always)]
    pub(crate) fn splat(value: u16) -> Lanes {
        Lanes(unsafe { core::arch::aarch64::vdup_n_u16(value) })
    }

    /// Lane 3 (a color's alpha).
    #[inline(always)]
    pub(crate) fn lane3(self) -> u32 {
        unsafe { core::arch::aarch64::vget_lane_u16::<3>(self.0) as u32 }
    }

    /// Lanes 0 and 1, then lanes 2 and 3, as `pair` takes them.
    #[inline(always)]
    pub(crate) fn halves(self) -> (u32, u32) {
        let bits = unsafe { core::arch::aarch64::vget_lane_u64::<0>(core::arch::aarch64::vreinterpret_u64_u16(self.0)) };
        (bits as u32, (bits >> 32) as u32)
    }
}

/// The three texels of the bilinear filter, packed, weighed (lanes 0-2 of `weights`: the corner's
/// 32 - fs - ft, then fs and ft) as `(corner · w0 + along_s · w1 + along_t · w2 + 16) >> 5` in each
/// channel. This is the scalar filter's
/// `corner + ((fs · (along_s - corner) + ft · (along_t - corner) + 16) >> 5)`: adding 32 · corner
/// inside the shift is adding corner outside it. Channels of 0-255 and weights summing to 32 stay
/// below 2^13.
#[cfg(target_arch = "arm")]
#[inline(always)]
pub(crate) fn filter3(corner: u32, along_s: u32, along_t: u32, weights: Lanes) -> Lanes {
    let out: f64;
    // Safety: register-only NEON arithmetic; q14 and q15 are scratch.
    unsafe {
        asm!(
            ".fpu neon-fp-armv8",
            "vmov d30, {a}, {b}",
            "vmov d31, {c}, {c}",
            "vmovl.u8 q14, d30",
            "vmovl.u8 q15, d31",
            "vmul.i16 {out}, d28, {w}[0]",
            "vmla.i16 {out}, d29, {w}[1]",
            "vmla.i16 {out}, d30, {w}[2]",
            "vrshr.u16 {out}, {out}, #5",
            a = in(reg) corner, b = in(reg) along_s, c = in(reg) along_t, w = in(dreg_low8) weights.0,
            out = out(dreg) out,
            out("d28") _, out("d29") _, out("d30") _, out("d31") _,
            options(pure, nomem, nostack, preserves_flags),
        );
    }
    Lanes(out)
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
pub(crate) fn filter3(corner: u32, along_s: u32, along_t: u32, weights: Lanes) -> Lanes {
    use core::arch::aarch64::*;
    unsafe {
        let c_u16 = vget_low_u16(vmovl_u8(vcreate_u8(u64::from(corner))));
        let s_u16 = vget_low_u16(vmovl_u8(vcreate_u8(u64::from(along_s))));
        let t_u16 = vget_low_u16(vmovl_u8(vcreate_u8(u64::from(along_t))));
        let mut acc = vmul_lane_u16::<0>(c_u16, weights.0);
        acc = vmla_lane_u16::<1>(acc, s_u16, weights.0);
        acc = vmla_lane_u16::<2>(acc, t_u16, weights.0);
        Lanes(vrshr_n_u16::<5>(acc))
    }
}

/// A packed texel (or color) as lanes.
#[cfg(target_arch = "arm")]
#[inline(always)]
pub(crate) fn widen(packed: u32) -> Lanes {
    let out: f64;
    // Safety: register-only NEON arithmetic; q15 is scratch.
    unsafe {
        asm!(
            ".fpu neon-fp-armv8",
            "vmov d31, {x}, {x}",
            "vmovl.u8 q15, d31",
            "vmov {out}, d30",
            x = in(reg) packed, out = lateout(dreg) out,
            out("d30") _, out("d31") _,
            options(pure, nomem, nostack, preserves_flags),
        );
    }
    Lanes(out)
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
pub(crate) fn widen(packed: u32) -> Lanes {
    use core::arch::aarch64::*;
    unsafe {
        Lanes(vget_low_u16(vmovl_u8(vcreate_u8(u64::from(packed)))))
    }
}

/// The shade color along a span: its four attributes (s15.16, each with 128 added to its integer
/// part, see `color`) and their steps, two to a D register.
#[cfg(target_arch = "arm")]
#[derive(Clone, Copy)]
pub(crate) struct Shade {
    rg: Lanes,
    ba: Lanes,
    step_rg: Lanes,
    step_ba: Lanes,
    low: Lanes,
    high: Lanes,
    zero: Lanes,
}

#[cfg(target_arch = "arm")]
impl Shade {
    /// From the attributes r, g, b and a at the span's first pixel and their steps.
    #[inline(always)]
    pub(crate) fn new(start: [i32; 4], step: [i32; 4]) -> Shade {
        let [r, g, b, a] = start.map(|v| v.wrapping_add(128 << 16) as u32);
        let [dr, dg, db, da] = step.map(|v| v as u32);
        let pair = |low: u32, high: u32| Lanes(f64::from_bits(u64::from(high) << 32 | u64::from(low)));
        Shade {
            rg: pair(r, g), ba: pair(b, a), step_rg: pair(dr, dg), step_ba: pair(db, da),
            low: Lanes::splat(128), high: Lanes::splat(255), zero: Lanes::splat(0),
        }
    }

    /// A pixel on.
    #[inline(always)]
    pub(crate) fn step(&mut self) {
        // Safety: register-only NEON arithmetic.
        unsafe {
            asm!(
                ".fpu neon-fp-armv8",
                "vadd.i32 {rg}, {rg}, {drg}",
                "vadd.i32 {ba}, {ba}, {dba}",
                rg = inout(dreg) self.rg.0, ba = inout(dreg) self.ba.0, drg = in(dreg) self.step_rg.0,
                dba = in(dreg) self.step_ba.0,
                options(pure, nomem, nostack, preserves_flags),
            );
        }
    }

    /// The shade here, raster.rs's clamp9 of each attribute's integer part. The integer parts' low 16
    /// bits (VSHRN) are those plus 128, whose low nine bits w give clamp9 = clamp(w - 128, 0, 255).
    #[inline(always)]
    pub(crate) fn color(&self) -> Lanes {
        let out: f64;
        // Safety: register-only NEON arithmetic; q15 is scratch.
        unsafe {
            asm!(
                ".fpu neon-fp-armv8",
                "vmov d30, {rg}",
                "vmov d31, {ba}",
                "vshrn.i32 {out}, q15, #16",
                "vbic.i16 {out}, #0xfe00",
                "vsub.i16 {out}, {out}, {low}",
                "vmax.s16 {out}, {out}, {zero}",
                "vmin.s16 {out}, {out}, {high}",
                rg = in(dreg) self.rg.0, ba = in(dreg) self.ba.0, low = in(dreg) self.low.0,
                high = in(dreg) self.high.0, zero = in(dreg) self.zero.0, out = out(dreg) out,
                out("d30") _, out("d31") _,
                options(pure, nomem, nostack, preserves_flags),
            );
        }
        Lanes(out)
    }
}

/// The shade color along a span on 64-bit ARM.
#[cfg(target_arch = "aarch64")]
#[derive(Clone, Copy)]
pub(crate) struct Shade {
    rgba: core::arch::aarch64::uint32x4_t,
    step_rgba: core::arch::aarch64::uint32x4_t,
}

#[cfg(target_arch = "aarch64")]
impl Shade {
    /// From the attributes r, g, b and a at the span's first pixel and their steps.
    #[inline(always)]
    pub(crate) fn new(start: [i32; 4], step: [i32; 4]) -> Shade {
        use core::arch::aarch64::*;
        let [r, g, b, a] = start.map(|v| v.wrapping_add(128 << 16) as u32);
        let [dr, dg, db, da] = step.map(|v| v as u32);
        unsafe {
            Shade {
                rgba: vld1q_u32([r, g, b, a].as_ptr()),
                step_rgba: vld1q_u32([dr, dg, db, da].as_ptr()),
            }
        }
    }

    /// A pixel on.
    #[inline(always)]
    pub(crate) fn step(&mut self) {
        use core::arch::aarch64::*;
        unsafe {
            self.rgba = vaddq_u32(self.rgba, self.step_rgba);
        }
    }

    /// The shade here, raster.rs's clamp9 of each attribute's integer part.
    #[inline(always)]
    pub(crate) fn color(&self) -> Lanes {
        use core::arch::aarch64::*;
        unsafe {
            let shifted = vshrn_n_u32::<16>(self.rgba);
            let masked = vand_u16(shifted, vdup_n_u16(0x1ff));
            let sub = vsub_s16(vreinterpret_s16_u16(masked), vdup_n_s16(128));
            let clamped = vmin_s16(vmax_s16(sub, vdup_n_s16(0)), vdup_n_s16(255));
            Lanes(vreinterpret_u16_s16(clamped))
        }
    }
}

/// `(x · y + 128) >> 8` in each lane: a channel times a channel (0-255 each) as the combiner rounds it,
/// at most 254, so its clamp9 changes nothing.
#[cfg(target_arch = "arm")]
#[inline(always)]
pub(crate) fn mul_round(x: Lanes, y: Lanes) -> Lanes {
    let out: f64;
    // Safety: register-only NEON arithmetic.
    unsafe {
        asm!(
            ".fpu neon-fp-armv8",
            "vmul.i16 {out}, {x}, {y}",
            "vrshr.u16 {out}, {out}, #8",
            x = in(dreg) x.0, y = in(dreg) y.0, out = out(dreg) out,
            options(pure, nomem, nostack, preserves_flags),
        );
    }
    Lanes(out)
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
pub(crate) fn mul_round(x: Lanes, y: Lanes) -> Lanes {
    use core::arch::aarch64::*;
    unsafe {
        Lanes(vrshr_n_u16::<8>(vmul_u16(x.0, y.0)))
    }
}

/// `((p - s) · t + (s << 8) + 128) >> 8` in each lane, the combiner's lerp from s to p by t (all 0-255),
/// which lies between s and p, so its clamp9 changes nothing. As `s + (((p - s) · t + 128) >> 8)` in
/// 32 bits.
#[cfg(target_arch = "arm")]
#[inline(always)]
pub(crate) fn lerp(p: Lanes, s: Lanes, t: Lanes) -> Lanes {
    let out: f64;
    // Safety: register-only NEON arithmetic; q13 and q14 are scratch.
    unsafe {
        asm!(
            ".fpu neon-fp-armv8",
            "vsub.i16 d26, {p}, {s}",
            "vmull.s16 q14, d26, {t}",
            "vrshrn.i32 {out}, q14, #8",
            "vadd.i16 {out}, {out}, {s}",
            p = in(dreg) p.0, s = in(dreg) s.0, t = in(dreg) t.0, out = out(dreg) out,
            out("d26") _, out("d27") _, out("d28") _, out("d29") _,
            options(pure, nomem, nostack, preserves_flags),
        );
    }
    Lanes(out)
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
pub(crate) fn lerp(p: Lanes, s: Lanes, t: Lanes) -> Lanes {
    use core::arch::aarch64::*;
    unsafe {
        let diff = vsub_s16(vreinterpret_s16_u16(p.0), vreinterpret_s16_u16(s.0));
        let prod = vmull_s16(diff, vreinterpret_s16_u16(t.0));
        let rounded = vrshrn_n_s32::<8>(prod);
        let out = vadd_u16(vreinterpret_u16_s16(rounded), s.0);
        Lanes(out)
    }
}

/// The blender's mix `(p · a5 + m · b5) >> 5` clamped to 255 in each lane, with a5 and b5 in lanes 0
/// and 1 of `factors` (p and m 0-255, a5 below 32, b5 at most 32: no lane passes 2^14).
#[cfg(target_arch = "arm")]
#[inline(always)]
pub(crate) fn mix(p: Lanes, m: Lanes, factors: Lanes, top: Lanes) -> Lanes {
    let out: f64;
    // Safety: register-only NEON arithmetic.
    unsafe {
        asm!(
            ".fpu neon-fp-armv8",
            "vmul.i16 {out}, {p}, {f}[0]",
            "vmla.i16 {out}, {m}, {f}[1]",
            "vshr.u16 {out}, {out}, #5",
            "vmin.u16 {out}, {out}, {top}",
            p = in(dreg) p.0, m = in(dreg) m.0, f = in(dreg_low8) factors.0, top = in(dreg) top.0,
            out = out(dreg) out,
            options(pure, nomem, nostack, preserves_flags),
        );
    }
    Lanes(out)
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
pub(crate) fn mix(p: Lanes, m: Lanes, factors: Lanes, top: Lanes) -> Lanes {
    use core::arch::aarch64::*;
    unsafe {
        let prod = vmul_lane_u16::<0>(p.0, factors.0);
        let acc = vmla_lane_u16::<1>(prod, m.0, factors.0);
        let shifted = vshr_n_u16::<5>(acc);
        let out = vmin_u16(shifted, top.0);
        Lanes(out)
    }
}

/// The shifts that put r, g and b's top five bits where RGBA5551 has them (and drop lane 3).
#[inline(always)]
pub(crate) fn rgba5551_shifts() -> Lanes {
    Lanes::pair(8 | 3 << 16, (-2i16 as u16 as u32) | (-16i16 as u16 as u32) << 16)
}

/// Stores the color's r, g and b (0-255) as an RGBA5551 pixel with its coverage bit set, big-endian at
/// `at`: (r >> 3) << 11 | (g >> 3) << 6 | (b >> 3) << 1 | 1, the three fields summed across lanes.
///
/// Safety: `at` and the byte after it must be writable.
#[cfg(target_arch = "arm")]
#[inline(always)]
pub(crate) unsafe fn store_rgba5551(color: Lanes, shifts: Lanes, at: *mut u8) {
    let x = color.0;
    // Safety: the caller's.
    unsafe {
        asm!(
            ".fpu neon-fp-armv8",
            "vbic.i16 {x}, #7",
            "vshl.u16 {x}, {x}, {shifts}",
            "vpadd.i16 {x}, {x}, {x}",
            "vpadd.i16 {x}, {x}, {x}",
            "vorr.i16 {x}, #1",
            "vrev16.8 {x}, {x}",
            "vst1.16 {{{x}[0]}}, [{at}]",
            x = inout(dreg) x => _, shifts = in(dreg) shifts.0, at = in(reg) at,
            options(nostack, preserves_flags),
        );
    }
}

/// Stores the color's r, g and b (0-255) as an RGBA5551 pixel with its coverage bit set, big-endian at
/// `at`: (r >> 3) << 11 | (g >> 3) << 6 | (b >> 3) << 1 | 1, the three fields summed across lanes.
///
/// Safety: `at` and the byte after it must be writable.
#[cfg(target_arch = "aarch64")]
#[inline(always)]
pub(crate) unsafe fn store_rgba5551(color: Lanes, shifts: Lanes, at: *mut u8) {
    use core::arch::aarch64::*;
    unsafe {
        let masked = vbic_u16(color.0, vdup_n_u16(7));
        let shifted = vshl_s16(vreinterpret_s16_u16(masked), vreinterpret_s16_u16(shifts.0));
        let sum = vaddv_u16(vreinterpret_u16_s16(shifted));
        let pixel = (sum | 1).to_be();
        std::ptr::copy_nonoverlapping(&pixel as *const u16 as *const u8, at, 2);
    }
}

/// An RGBA5551 texel, palette entry or image pixel (its two bytes, big-endian) packed as r, g, b and a
/// bytes: `RGBA16_HIGH[first] + RGBA16_LOW[second]`. The green field straddles the bytes; its
/// expansion `g · 33 >> 2` is `(g >> 2) · 33 + ((g & 3) · 33 >> 2)`, so the two parts add without a
/// carry. Alpha is 255 for the coverage bit, as texels have it.
pub(crate) static RGBA16_HIGH: [u32; 256] = {
    let mut table = [0; 256];
    let mut h = 0;
    while h < 256 {
        let r = (h >> 3) as u32;
        table[h] = (r << 3 | r >> 2) | ((h as u32 & 7) * 33) << 8;
        h += 1;
    }
    table
};

pub(crate) static RGBA16_LOW: [u32; 256] = {
    let mut table = [0; 256];
    let mut l = 0;
    while l < 256 {
        let b = (l as u32 >> 1) & 31;
        let a = if l & 1 != 0 { 255 } else { 0 };
        table[l] = (((l as u32 >> 6) * 33) >> 2) << 8 | (b << 3 | b >> 2) << 16 | a << 24;
        l += 1;
    }
    table
};

/// An RGBA5551 value (two bytes, big-endian) packed.
#[inline(always)]
pub(crate) fn rgba16(first: u8, second: u8) -> u32 {
    RGBA16_HIGH[usize::from(first)] + RGBA16_LOW[usize::from(second)]
}

/// An IA16 value: intensity in r, g and b, then alpha.
#[inline(always)]
pub(crate) fn ia16(intensity: u8, alpha: u8) -> u32 {
    (u32::from(intensity) * 0x01_0101) | (u32::from(alpha) << 24)
}

/// An IA8 texel: four bits of intensity and four of alpha, each times 17.
pub(crate) static IA8: [u32; 256] = {
    let mut table = [0; 256];
    let mut v = 0;
    while v < 256 {
        let (i, a) = ((v as u32 >> 4) * 17, (v as u32 & 15) * 17);
        table[v] = (i * 0x01_0101) | (a << 24);
        v += 1;
    }
    table
};

/// An IA4 texel: three bits of intensity (spread over eight) and one of alpha.
pub(crate) static IA4: [u32; 16] = {
    let mut table = [0; 16];
    let mut v = 0;
    while v < 16 {
        let i = v as u32 >> 1;
        let i = i << 5 | i << 2 | i >> 1;
        table[v] = (i * 0x01_0101) | ((if v & 1 != 0 { 255 } else { 0 }) << 24);
        v += 1;
    }
    table
};

/// A combiner cycle's inputs a, b, c and d as lanes, fixed for a span: each lane is either a lane of the
/// pixel's colors [combined, texel 0, texel 1, shade] (a VTBL index of its two bytes in those four
/// D registers, 255 for none) or fixed, as `combine_selected` picks the inputs (a color's alpha may go
/// to all lanes; the alpha selectors pick lane 3). And whether every lane is a product (see
/// `combine_cycle`).
#[repr(C, align(8))]
#[derive(Clone, Copy)]
pub(crate) struct CycleLanes {
    index: [u64; 4],
    fixed: [u64; 4],
    product: u64,
}

impl CycleLanes {
    /// Each input's lanes: `Ok((color, channel))` from the pixel's colors, or `Err` with the fixed value.
    pub(crate) fn new(inputs: [[Result<(u8, u8), i16>; 4]; 4], product: bool) -> CycleLanes {
        let mut lanes = CycleLanes { index: [0; 4], fixed: [0; 4], product: u64::from(product) };
        for (n, input) in inputs.iter().enumerate() {
            for (lane, source) in input.iter().enumerate() {
                let (index, fixed) = match *source {
                    Ok((color, channel)) => {
                        let byte = u64::from(8 * color + 2 * channel);
                        (byte | (byte + 1) << 8, 0)
                    }
                    Err(value) => (0xffff, u64::from(value as u16)),
                };
                lanes.index[n] |= index << (16 * lane);
                lanes.fixed[n] |= fixed << (16 * lane);
            }
        }
        lanes
    }
}

/// One combiner cycle on the pixel's colors in d24-d27 (q12, q13), its `CycleLanes` at `{p}` (moved
/// on past them), into d16: `(a - b) · c + d`, rounded and clamped as `combine_selected` has it. Inputs
/// are -256 to 480, so a - b fits 16 bits and the product 32; `vrshrn` gives `(product + 128) >> 8`,
/// at most ±1380 here, and adding d inside or outside the shift is the same. Then clamp9 as
/// `Shade::color`'s: the low nine bits of the value plus 128, less 128, clamped to 0-255.
///
/// A cycle whose every lane is a product (raster.rs's `PickedInputs::is_product`: b nothing, a and c
/// 0-255, the sum with d at most 255) branches to `(a · c + 128) >> 8 + d` in 16 bits, which clamp9
/// leaves as it is. `$general` and `$done` are the cycle's own local labels.
#[cfg(target_arch = "arm")]
macro_rules! combine_cycle {
    ($general:literal, $done:literal) => {
        concat!(
            "vld1.64 {{d16-d19}}, [{p}:64]!\n",
            "vld1.64 {{d20-d23}}, [{p}:64]!\n",
            "ldr {product}, [{p}], #8\n",
            "vtbl.8 d16, {{d24-d27}}, d16\n",
            "vtbl.8 d18, {{d24-d27}}, d18\n",
            "vtbl.8 d19, {{d24-d27}}, d19\n",
            "vorr d16, d16, d20\n",
            "vorr d18, d18, d22\n",
            "vorr d19, d19, d23\n",
            "cmp {product}, #0\n",
            "beq ", $general, "f\n",
            "vmul.i16 d16, d16, d18\n",
            "vrshr.u16 d16, d16, #8\n",
            "vadd.i16 d16, d16, d19\n",
            "b ", $done, "f\n",
            $general, ":\n",
            "vtbl.8 d17, {{d24-d27}}, d17\n",
            "vorr d17, d17, d21\n",
            "vsub.i16 d16, d16, d17\n",
            "vmull.s16 q14, d16, d18\n",
            "vmov.i16 d17, #128\n",
            "vmov.i16 d18, #255\n",
            "vrshrn.i32 d16, q14, #8\n",
            "vmov.i16 d20, #0\n",
            "vadd.i16 d16, d16, d19\n",
            "vadd.i16 d16, d16, d17\n",
            "vbic.i16 d16, #0xfe00\n",
            "vsub.i16 d16, d16, d17\n",
            "vmax.s16 d16, d16, d20\n",
            "vmin.s16 d16, d16, d18\n",
            $done, ":\n",
        )
    };
}

/// One combiner cycle with nothing for the combined color: one-cycle mode's (the second), or two-cycle
/// mode's first.
#[cfg(target_arch = "arm")]
#[inline(always)]
pub(crate) fn combine_one(cycle: &CycleLanes, texel0: Lanes, texel1: Lanes, shade: Lanes) -> Lanes {
    let out: f64;
    // Safety: reads `cycle`; q8-q14 are scratch.
    unsafe {
        asm!(
            ".fpu neon-fp-armv8",
            "vmov.i16 d24, #0",
            "vmov d25, {t0}",
            "vmov d26, {t1}",
            "vmov d27, {sh}",
            combine_cycle!("2", "3"),
            "vmov {out}, d16",
            p = inout(reg) cycle as *const CycleLanes => _, product = out(reg) _,
            t0 = in(dreg) texel0.0, t1 = in(dreg) texel1.0, sh = in(dreg) shade.0, out = lateout(dreg) out,
            out("d16") _, out("d17") _, out("d18") _, out("d19") _, out("d20") _, out("d21") _, out("d22") _,
            out("d23") _, out("d24") _, out("d25") _, out("d26") _, out("d27") _, out("d28") _, out("d29") _,
            options(pure, readonly, nostack),
        );
    }
    Lanes(out)
}

/// Both combiner cycles: the first's result is the second's combined color.
#[cfg(target_arch = "arm")]
#[inline(always)]
pub(crate) fn combine_two(cycles: &[CycleLanes; 2], texel0: Lanes, texel1: Lanes, shade: Lanes) -> Lanes {
    let out: f64;
    // Safety: reads `cycles`; q8-q14 are scratch.
    unsafe {
        asm!(
            ".fpu neon-fp-armv8",
            "vmov.i16 d24, #0",
            "vmov d25, {t0}",
            "vmov d26, {t1}",
            "vmov d27, {sh}",
            combine_cycle!("2", "3"),
            "vmov d24, d16",
            combine_cycle!("4", "5"),
            "vmov {out}, d16",
            p = inout(reg) cycles.as_ptr() => _, product = out(reg) _,
            t0 = in(dreg) texel0.0, t1 = in(dreg) texel1.0, sh = in(dreg) shade.0, out = lateout(dreg) out,
            out("d16") _, out("d17") _, out("d18") _, out("d19") _, out("d20") _, out("d21") _, out("d22") _,
            out("d23") _, out("d24") _, out("d25") _, out("d26") _, out("d27") _, out("d28") _, out("d29") _,
            options(pure, readonly, nostack),
        );
    }
    Lanes(out)
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn combine_cycle(
    cycle: &CycleLanes,
    combined: core::arch::aarch64::uint16x4_t,
    texel0: core::arch::aarch64::uint16x4_t,
    texel1: core::arch::aarch64::uint16x4_t,
    shade: core::arch::aarch64::uint16x4_t,
) -> core::arch::aarch64::uint16x4_t {
    use core::arch::aarch64::*;
    unsafe {
        let table = uint8x8x4_t(
            vreinterpret_u8_u16(combined),
            vreinterpret_u8_u16(texel0),
            vreinterpret_u8_u16(texel1),
            vreinterpret_u8_u16(shade),
        );
        let a_table = vtbl4_u8(table, vreinterpret_u8_u64(vcreate_u64(cycle.index[0])));
        let a = vorr_u16(vreinterpret_u16_u8(a_table), vreinterpret_u16_u64(vcreate_u64(cycle.fixed[0])));
        let c_table = vtbl4_u8(table, vreinterpret_u8_u64(vcreate_u64(cycle.index[2])));
        let c = vorr_u16(vreinterpret_u16_u8(c_table), vreinterpret_u16_u64(vcreate_u64(cycle.fixed[2])));
        let d_table = vtbl4_u8(table, vreinterpret_u8_u64(vcreate_u64(cycle.index[3])));
        let d = vorr_u16(vreinterpret_u16_u8(d_table), vreinterpret_u16_u64(vcreate_u64(cycle.fixed[3])));

        if cycle.product != 0 {
            let prod = vmul_u16(a, c);
            let rounded = vrshr_n_u16::<8>(prod);
            vadd_u16(rounded, d)
        } else {
            let b_table = vtbl4_u8(table, vreinterpret_u8_u64(vcreate_u64(cycle.index[1])));
            let b = vorr_u16(vreinterpret_u16_u8(b_table), vreinterpret_u16_u64(vcreate_u64(cycle.fixed[1])));
            let diff = vsub_s16(vreinterpret_s16_u16(a), vreinterpret_s16_u16(b));
            let prod = vmull_s16(diff, vreinterpret_s16_u16(c));
            let rounded = vrshrn_n_s32::<8>(prod);
            let sum = vadd_s16(rounded, vreinterpret_s16_u16(d));
            let plus128 = vadd_s16(sum, vdup_n_s16(128));
            let masked = vand_u16(vreinterpret_u16_s16(plus128), vdup_n_u16(0x1ff));
            let minus128 = vsub_s16(vreinterpret_s16_u16(masked), vdup_n_s16(128));
            let clamped = vmin_s16(vmax_s16(minus128, vdup_n_s16(0)), vdup_n_s16(255));
            vreinterpret_u16_s16(clamped)
        }
    }
}

/// One combiner cycle with nothing for the combined color: one-cycle mode's (the second), or two-cycle
/// mode's first.
#[cfg(target_arch = "aarch64")]
#[inline(always)]
pub(crate) fn combine_one(cycle: &CycleLanes, texel0: Lanes, texel1: Lanes, shade: Lanes) -> Lanes {
    use core::arch::aarch64::*;
    unsafe {
        let out = combine_cycle(cycle, vdup_n_u16(0), texel0.0, texel1.0, shade.0);
        Lanes(out)
    }
}

/// Both combiner cycles: the first's result is the second's combined color.
#[cfg(target_arch = "aarch64")]
#[inline(always)]
pub(crate) fn combine_two(cycles: &[CycleLanes; 2], texel0: Lanes, texel1: Lanes, shade: Lanes) -> Lanes {
    use core::arch::aarch64::*;
    unsafe {
        let cycle0 = combine_cycle(&cycles[0], vdup_n_u16(0), texel0.0, texel1.0, shade.0);
        let cycle1 = combine_cycle(&cycles[1], cycle0, texel0.0, texel1.0, shade.0);
        Lanes(cycle1)
    }
}

/// A span's first attributes: `start + de · lines` (32 bits, wrapping) along the major edge, then plus
/// `(dx · offset) >> 16` with the products in 64 bits, as the scalar triangle setup has them; the
/// narrowing keeps the shifted products' low 32 bits, which is all the 32-bit sum takes.
#[cfg(target_arch = "arm")]
#[inline(always)]
pub(crate) fn attributes_at(start: &[i32; 8], de: &[i32; 8], dx: &[i32; 8], lines: i32, offset: i32) -> [i32; 8] {
    let mut out = [0; 8];
    // Safety: reads three arrays of eight words and writes one; q8-q15 are scratch.
    unsafe {
        asm!(
            ".fpu neon-fp-armv8",
            "vld1.32 {{d16-d19}}, [{start}]",
            "vld1.32 {{d20-d23}}, [{de}]",
            "vdup.32 q12, {lines}",
            "vmla.i32 q8, q10, q12",
            "vmla.i32 q9, q11, q12",
            "vld1.32 {{d20-d23}}, [{dx}]",
            "vdup.32 d24, {offset}",
            "vmull.s32 q13, d20, d24",
            "vmull.s32 q14, d21, d24",
            "vmull.s32 q15, d22, d24",
            "vmull.s32 q10, d23, d24",
            "vshrn.i64 d22, q13, #16",
            "vshrn.i64 d23, q14, #16",
            "vshrn.i64 d26, q15, #16",
            "vshrn.i64 d27, q10, #16",
            "vadd.i32 q8, q8, q11",
            "vadd.i32 q9, q9, q13",
            "vst1.32 {{d16-d19}}, [{out}]",
            start = in(reg) start.as_ptr(), de = in(reg) de.as_ptr(), dx = in(reg) dx.as_ptr(),
            lines = in(reg) lines, offset = in(reg) offset, out = in(reg) out.as_mut_ptr(),
            out("d16") _, out("d17") _, out("d18") _, out("d19") _, out("d20") _, out("d21") _, out("d22") _,
            out("d23") _, out("d24") _, out("d25") _, out("d26") _, out("d27") _, out("d28") _, out("d29") _,
            out("d30") _, out("d31") _,
            options(nostack, preserves_flags),
        );
    }
    out
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
pub(crate) fn attributes_at(start: &[i32; 8], de: &[i32; 8], dx: &[i32; 8], lines: i32, offset: i32) -> [i32; 8] {
    use core::arch::aarch64::*;
    let mut out = [0i32; 8];
    unsafe {
        let start0 = vld1q_s32(start.as_ptr());
        let start1 = vld1q_s32(start.as_ptr().add(4));
        let de0 = vld1q_s32(de.as_ptr());
        let de1 = vld1q_s32(de.as_ptr().add(4));
        let dx0 = vld1q_s32(dx.as_ptr());
        let dx1 = vld1q_s32(dx.as_ptr().add(4));
        let along0 = vmlaq_n_s32(start0, de0, lines);
        let along1 = vmlaq_n_s32(start1, de1, lines);
        let p0 = vmull_n_s32(vget_low_s32(dx0), offset);
        let p1 = vmull_n_s32(vget_high_s32(dx0), offset);
        let s0 = vshrn_n_s64::<16>(p0);
        let s1 = vshrn_n_s64::<16>(p1);
        let off0 = vcombine_s32(s0, s1);
        let p2 = vmull_n_s32(vget_low_s32(dx1), offset);
        let p3 = vmull_n_s32(vget_high_s32(dx1), offset);
        let s2 = vshrn_n_s64::<16>(p2);
        let s3 = vshrn_n_s64::<16>(p3);
        let off1 = vcombine_s32(s2, s3);
        let res0 = vaddq_s32(along0, off0);
        let res1 = vaddq_s32(along1, off1);
        vst1q_s32(out.as_mut_ptr(), res0);
        vst1q_s32(out.as_mut_ptr().add(4), res1);
    }
    out
}

/// The values as they are, all of them computed here: an empty asm block that takes and gives them back.
/// The compiler keeps the loads of a texel after one another (each waiting for the one before on the
/// in-order A53); with the loads of three or six texels finished at such a point, their waits overlap.
#[inline(always)]
pub(crate) fn together3([mut a, mut b, mut c]: [u32; 3]) -> [u32; 3] {
    #[cfg(target_arch = "arm")]
    unsafe {
        asm!("@ {0} {1} {2}", inout(reg) a, inout(reg) b, inout(reg) c,
            options(pure, nomem, nostack, preserves_flags));
    }
    #[cfg(target_arch = "aarch64")]
    unsafe {
        core::arch::asm!("// {0:w} {1:w} {2:w}", inout(reg) a, inout(reg) b, inout(reg) c,
            options(pure, nomem, nostack, preserves_flags));
    }
    [a, b, c]
}

/// `together3` for six values.
#[inline(always)]
pub(crate) fn together6([mut a, mut b, mut c, mut d, mut e, mut f]: [u32; 6]) -> [u32; 6] {
    #[cfg(target_arch = "arm")]
    unsafe {
        asm!("@ {0} {1} {2} {3} {4} {5}", inout(reg) a, inout(reg) b, inout(reg) c, inout(reg) d, inout(reg) e,
            inout(reg) f, options(pure, nomem, nostack, preserves_flags));
    }
    #[cfg(target_arch = "aarch64")]
    unsafe {
        core::arch::asm!("// {0:w} {1:w} {2:w} {3:w} {4:w} {5:w}", inout(reg) a, inout(reg) b, inout(reg) c, inout(reg) d, inout(reg) e,
            inout(reg) f, options(pure, nomem, nostack, preserves_flags));
    }
    [a, b, c, d, e, f]
}

/// `together3` or `together6` by the length.
#[inline(always)]
pub(crate) fn together<const N: usize>(values: [u32; N]) -> [u32; N] {
    let mut out = values;
    if N == 3 {
        out.copy_from_slice(&together3([values[0], values[1], values[2]]));
    } else if N == 6 {
        out.copy_from_slice(&together6([values[0], values[1], values[2], values[3], values[4], values[5]]));
    }
    out
}

/// RGBA5551 values packed (as `rgba16`) from their two bytes loaded as little-endian halfwords: both
/// tables' parts of all of them looked up before any is added.
#[inline(always)]
pub(crate) fn rgba16_halves<const N: usize>(halves: [u32; N]) -> [u32; N] {
    let high = together(halves.map(|half| RGBA16_HIGH[(half & 0xff) as usize]));
    let low = together(halves.map(|half| RGBA16_LOW[(half >> 8 & 0xff) as usize]));
    std::array::from_fn(|k| high[k] + low[k])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widen_unpacks_bytes_into_lanes() {
        let val: u32 = 0x12_34_56_78;
        let lanes = widen(val);
        let (low, high) = lanes.halves();
        assert_eq!(low, 0x0056_0078);
        assert_eq!(high, 0x0012_0034);
        assert_eq!(lanes.lane3(), 0x12);
    }

    #[test]
    fn filter3_matches_scalar_bilinear() {
        let corner = 0x10_20_30_40;
        let along_s = 0x50_60_70_80;
        let along_t = 0x90_a0_b0_c0;
        for fs in [0, 8, 16, 24, 32] {
            for ft in [0, 8, 16, 24, 32] {
                if fs + ft > 32 {
                    continue;
                }
                let w0 = (32 - fs - ft) as u32;
                let weights = Lanes::pair(w0 | ((fs as u32) << 16), ft as u32);
                let neon_filtered = filter3(corner, along_s, along_t, weights);
                let (low, high) = neon_filtered.halves();
                let actual = [
                    (low & 0xffff) as u8,
                    (low >> 16) as u8,
                    (high & 0xffff) as u8,
                    (high >> 16) as u8,
                ];
                let c_bytes = corner.to_le_bytes();
                let s_bytes = along_s.to_le_bytes();
                let t_bytes = along_t.to_le_bytes();
                for i in 0..4 {
                    let expected = ((u32::from(c_bytes[i]) * w0
                        + u32::from(s_bytes[i]) * fs as u32
                        + u32::from(t_bytes[i]) * ft as u32
                        + 16)
                        >> 5) as u8;
                    assert_eq!(actual[i], expected, "mismatch at ch {i} for fs={fs}, ft={ft}");
                }
            }
        }
    }

    #[test]
    fn mul_round_and_lerp_match_formulas() {
        let p = Lanes::pair(0x0010_0020, 0x0030_0040);
        let s = Lanes::pair(0x0005_000a, 0x000f_0014);
        let t = Lanes::pair(0x0080_0080, 0x0080_0080);
        let l = lerp(p, s, t);
        let (low, high) = l.halves();
        let actual = [(low & 0xffff) as i32, (low >> 16) as i32, (high & 0xffff) as i32, (high >> 16) as i32];
        let p_lanes = [0x20, 0x10, 0x40, 0x30];
        let s_lanes = [0x0a, 0x05, 0x14, 0x0f];
        for i in 0..4 {
            let exp = s_lanes[i] + (((p_lanes[i] - s_lanes[i]) * 128 + 128) >> 8);
            assert_eq!(actual[i], exp);
        }

        let m = mul_round(p, s);
        let (m_low, m_high) = m.halves();
        let m_actual = [(m_low & 0xffff) as i32, (m_low >> 16) as i32, (m_high & 0xffff) as i32, (m_high >> 16) as i32];
        for i in 0..4 {
            let exp = (p_lanes[i] * s_lanes[i] + 128) >> 8;
            assert_eq!(m_actual[i], exp);
        }
    }

    #[test]
    fn shade_stepping_and_color_clamping() {
        let start = [100 << 16, 200 << 16, 50 << 16, 250 << 16];
        let step = [1 << 16, -2 << 16, 10 << 16, -5 << 16];
        let mut shade = Shade::new(start, step);
        let c0 = shade.color();
        let (l0, h0) = c0.halves();
        assert_eq!([l0 & 0xffff, l0 >> 16, h0 & 0xffff, h0 >> 16], [100, 200, 50, 250]);
        shade.step();
        let c1 = shade.color();
        let (l1, h1) = c1.halves();
        assert_eq!([l1 & 0xffff, l1 >> 16, h1 & 0xffff, h1 >> 16], [101, 198, 60, 245]);
    }

    #[test]
    fn attributes_at_matches_step_product() {
        let start = [10, 20, 30, 40, 50, 60, 70, 80];
        let de = [1, 2, 3, 4, -1, -2, -3, -4];
        let dx = [100, 200, -300, -400, 500, 600, -700, -800];
        let lines = 5;
        let offset = 0x1_8000;
        let actual = attributes_at(&start, &de, &dx, lines, offset);
        for i in 0..8 {
            let along = start[i].wrapping_add(de[i].wrapping_mul(lines));
            let expected = along.wrapping_add(((i64::from(dx[i]) * i64::from(offset)) >> 16) as i32);
            assert_eq!(actual[i], expected, "mismatch at attr {i}");
        }
    }

    #[test]
    fn store_rgba5551_encodes_be_pixels() {
        let color = Lanes::pair(0x00f8_00f8, 0x00f8_00ff); // r=248, g=248, b=248, a=255
        let shifts = rgba5551_shifts();
        let mut buf = [0u8; 4];
        unsafe {
            store_rgba5551(color, shifts, buf.as_mut_ptr());
        }
        let pixel = u16::from_be_bytes([buf[0], buf[1]]);
        assert_eq!(pixel, 0xffff); // full white + coverage 1
    }
}
