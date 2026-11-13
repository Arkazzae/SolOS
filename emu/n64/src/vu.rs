//! The RSP's vector unit (its COP2): 32 registers of eight 16-bit lanes,
//! an accumulator of 48 bits a lane (kept, as on the chip, as three 16-bit
//! slices: high, middle and low), the flags VCO, VCC and VCE, and the
//! state of the reciprocal unit. Its semantics follow ares
//! (ares/n64/rsp/interpreter-vpu.cpp), which is checked against the
//! console, and n64brew's "Reality Signal Processor/CPU Core".
//!
//! Lane 0 is the most significant: a register in DMEM order is lane 0's
//! high byte first. Bytes of a register are counted that way too.

use savestate::state;

#[cfg(target_arch = "arm")]
mod neon;
#[cfg(target_arch = "aarch64")]
mod neon64;
#[cfg(target_arch = "aarch64")]
use neon64 as neon;
#[cfg(all(test, any(target_arch = "arm", target_arch = "aarch64")))]
mod neon_tests;

#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
pub(crate) fn neon_available() -> bool {
    neon::available()
}

/// For compiled RSP code: a routine of the instruction's function alone,
/// taking the VU and the word, that does what `Vu::execute` does.
#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
pub(crate) fn entry(word: u32) -> Option<extern "C" fn(*mut Vu, u32)> {
    neon::entry(word)
}

/// No NEON elsewhere: compiled code calls `Vu::execute` for everything.
#[cfg(not(any(target_arch = "arm", target_arch = "aarch64")))]
#[allow(dead_code)]
pub(crate) fn entry(_word: u32) -> Option<extern "C" fn(*mut Vu, u32)> {
    None
}

#[derive(Clone)]
pub struct Vu {
    /// Host execution preferences/counters, excluded from the emulated
    /// state. ARM Linux enables the fast path only if HWCAP reports NEON.
    #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
    pub(crate) accelerated: bool,
    #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
    pub(crate) profiling: bool,
    #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
    pub(crate) native_count: u64,
    pub regs: [[u16; 8]; 32],
    acc_h: [u16; 8],
    acc_m: [u16; 8],
    acc_l: [u16; 8],
    /// VCO: carry (low bits) and not-equal (high bits); VCC: compare
    /// (low) and clip (high); VCE: compare extension. A bit per lane.
    vco: u16,
    vcc: u16,
    vce: u8,
    /// The reciprocal unit: the high half given by VRCPH or VRSQH, whether
    /// it is waiting for the low half, and the high half of the last result.
    div_in: u16,
    div_out: u16,
    div_pending: bool,
}

state!(Vu { regs, acc_h, acc_m, acc_l, vco, vcc, vce, div_in, div_out, div_pending });

/// The tables of VRCP and VRSQ: 512 entries of 16 bits, as the chip's ROM
/// has them (computed as ares computes them).
struct Tables {
    reciprocals: [u16; 512],
    inverse_roots: [u16; 512],
}

static TABLES: std::sync::LazyLock<Tables> = std::sync::LazyLock::new(|| {
    let mut reciprocals = [0u16; 512];
    reciprocals[0] = 0xffff;
    for (i, r) in reciprocals.iter_mut().enumerate().skip(1) {
        let b = (1u64 << 34) / (i as u64 + 512);
        *r = ((b + 1) >> 8) as u16;
    }
    let mut inverse_roots = [0u16; 512];
    for (i, r) in inverse_roots.iter_mut().enumerate() {
        let a = (i as u64 + 512) >> (i % 2);
        let mut b = 1u64 << 17;
        // The largest b with b / 2^22 below 1 / sqrt(a / 2^9).
        while a * (b + 1) * (b + 1) < 1 << 44 {
            b += 1;
        }
        *r = (b >> 1) as u16;
    }
    Tables { reciprocals, inverse_roots }
});

/// The lane of vt an element field gives lane `n`: all of them (0, 1),
/// pairs (2, 3: 0q, 1q), quarters (4-7: 0h-3h) or one lane for all (8-15).
const fn broadcast_lane(e: usize, n: usize) -> usize {
    match e {
        0 | 1 => n,
        2 | 3 => (n & !1) + (e & 1),
        4..=7 => (n & !3) + (e & 3),
        _ => e & 7,
    }
}

/// For each element field, the bytes of vt that make up each lane of the
/// operand (a VTBL index): see `broadcast_lane`.
#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
static BROADCAST: [[u8; 16]; 16] = {
    let mut table = [[0; 16]; 16];
    let mut e = 0;
    while e < 16 {
        let mut n = 0;
        while n < 8 {
            let lane = broadcast_lane(e, n) as u8;
            table[e][2 * n] = 2 * lane;
            table[e][2 * n + 1] = 2 * lane + 1;
            n += 1;
        }
        e += 1;
    }
    table
};

/// A flag register's bit for each lane: VCO's carries and VCC's compares
/// (low byte), then VCO's not-equals (high byte).
#[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
static LANE_BITS: [u16; 16] = [1, 2, 4, 8, 16, 32, 64, 128, 1 << 8, 1 << 9, 1 << 10, 1 << 11, 1 << 12, 1 << 13, 1 << 14, 1 << 15];

fn broadcast(v: [u16; 8], e: usize) -> [u16; 8] {
    std::array::from_fn(|n| v[broadcast_lane(e, n)])
}

fn clamp16(value: i32) -> u16 {
    value.clamp(-32768, 32767) as i16 as u16
}

impl Vu {
    pub fn new() -> Vu {
        Vu {
            #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
            accelerated: neon::available(),
            #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
            profiling: false,
            #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
            native_count: 0,
            regs: [[0; 8]; 32],
            acc_h: [0; 8],
            acc_m: [0; 8],
            acc_l: [0; 8],
            vco: 0,
            vcc: 0,
            vce: 0,
            div_in: 0,
            div_out: 0,
            div_pending: false,
        }
    }

    pub fn byte(&self, reg: usize, i: usize) -> u8 {
        let lane = self.regs[reg][(i >> 1) & 7];
        if i & 1 == 0 { (lane >> 8) as u8 } else { lane as u8 }
    }

    pub fn set_byte(&mut self, reg: usize, i: usize, value: u8) {
        let lane = &mut self.regs[reg][(i >> 1) & 7];
        *lane = if i & 1 == 0 { *lane & 0x00ff | u16::from(value) << 8 } else { *lane & 0xff00 | u16::from(value) };
    }

    fn acc(&self, n: usize) -> u64 {
        u64::from(self.acc_h[n]) << 32 | u64::from(self.acc_m[n]) << 16 | u64::from(self.acc_l[n])
    }

    fn set_acc(&mut self, n: usize, value: u64) {
        self.acc_h[n] = (value >> 32) as u16;
        self.acc_m[n] = (value >> 16) as u16;
        self.acc_l[n] = value as u16;
    }

    /// The accumulator clamped to 16 bits: its middle slice (or low, for
    /// the unsigned forms), or `negative` or `positive` when it is out of
    /// the 32 bits of high and middle.
    fn saturate(&self, n: usize, middle: bool, negative: u16, positive: u16) -> u16 {
        let (h, m) = (self.acc_h[n], self.acc_m[n] as i16);
        if (h as i16) < 0 {
            if h != 0xffff || m >= 0 {
                return negative;
            }
        } else if h != 0 || m < 0 {
            return positive;
        }
        if middle { self.acc_m[n] } else { self.acc_l[n] }
    }

    fn lane_flag(flags: u16, n: usize) -> bool {
        flags >> n & 1 != 0
    }

    /// CFC2: VCO, VCC or VCE, sign-extended from 16 bits.
    pub fn control(&self, reg: usize) -> u32 {
        let value = match reg & 3 {
            0 => self.vco,
            1 => self.vcc,
            _ => u16::from(self.vce),
        };
        value as i16 as i32 as u32
    }

    pub fn set_control(&mut self, reg: usize, value: u32) {
        match reg & 3 {
            0 => self.vco = value as u16,
            1 => self.vcc = value as u16,
            _ => self.vce = value as u8,
        }
    }

    /// A computational instruction (COP2 with bit 25 set): vd, vs and vt,
    /// and the element field, which picks vt's lanes (or for the
    /// single-lane instructions, the lane, with vs's field giving vd's).
    #[inline]
    pub fn execute(&mut self, word: u32) {
        #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
        if self.accelerated && neon::execute(self, word) {
            if self.profiling {
                self.native_count += 1;
            }
            return;
        }
        self.execute_reference(word);
    }

    fn execute_reference(&mut self, word: u32) {
        let e = (word >> 21 & 15) as usize;
        let vt = (word >> 16 & 31) as usize;
        let vs = (word >> 11 & 31) as usize;
        let vd = (word >> 6 & 31) as usize;
        let s = self.regs[vs];
        let t = broadcast(self.regs[vt], e);
        let mut d = [0u16; 8];
        match word & 63 {
            // VMULF, VMULU: signed fractions, doubled and rounded.
            0x00 | 0x01 => {
                for n in 0..8 {
                    let product = i64::from(s[n] as i16) * i64::from(t[n] as i16) * 2 + 0x8000;
                    self.set_acc(n, product as u64);
                    d[n] = if word & 63 == 0 {
                        self.saturate(n, true, 0x8000, 0x7fff)
                    } else if (self.acc_h[n] as i16) < 0 {
                        0
                    } else if ((self.acc_h[n] ^ self.acc_m[n]) as i16) < 0 {
                        0xffff
                    } else {
                        self.acc_m[n]
                    };
                }
            }
            // VRNDP, VRNDN: add vt (shifted by 16 if vs is odd) to the
            // accumulator when it is non-negative, or negative.
            0x02 | 0x0a => {
                let positive = word & 63 == 0x02;
                for n in 0..8 {
                    let mut add = i64::from(t[n] as i16);
                    if vs & 1 != 0 {
                        add <<= 16;
                    }
                    let mut acc = ((self.acc(n) << 16) as i64) >> 16;
                    if (acc >= 0) == positive {
                        acc = ((acc + add) << 16) >> 16;
                    }
                    self.set_acc(n, acc as u64);
                    d[n] = clamp16((acc >> 16) as i32);
                }
            }
            // VMULQ: the MPEG decoder's multiply, rounded towards zero.
            0x03 => {
                for n in 0..8 {
                    let mut product = i32::from(s[n] as i16) * i32::from(t[n] as i16);
                    if product < 0 {
                        product += 31;
                    }
                    self.acc_h[n] = (product >> 16) as u16;
                    self.acc_m[n] = product as u16;
                    self.acc_l[n] = 0;
                    d[n] = clamp16(product >> 1) & !15;
                }
            }
            // VMUDL: the high half of unsigned low parts.
            0x04 => {
                for n in 0..8 {
                    let product = (u32::from(s[n]) * u32::from(t[n])) >> 16;
                    self.set_acc(n, u64::from(product as u16));
                    d[n] = self.acc_l[n];
                }
            }
            // VMUDM: signed vs times unsigned vt.
            0x05 => {
                for n in 0..8 {
                    let product = i32::from(s[n] as i16) * i32::from(t[n]);
                    self.set_acc(n, i64::from(product) as u64);
                    d[n] = self.acc_m[n];
                }
            }
            // VMUDN: unsigned vs times signed vt.
            0x06 => {
                for n in 0..8 {
                    let product = i32::from(s[n]) * i32::from(t[n] as i16);
                    self.set_acc(n, i64::from(product) as u64);
                    d[n] = self.acc_l[n];
                }
            }
            // VMUDH: signed high parts.
            0x07 => {
                for n in 0..8 {
                    let product = i64::from(i32::from(s[n] as i16) * i32::from(t[n] as i16)) << 16;
                    self.set_acc(n, product as u64);
                    d[n] = self.saturate(n, true, 0x8000, 0x7fff);
                }
            }
            // VMACF, VMACU: accumulate signed fractions.
            0x08 | 0x09 => {
                for n in 0..8 {
                    let product = i64::from(s[n] as i16) * i64::from(t[n] as i16) * 2;
                    self.set_acc(n, self.acc(n).wrapping_add(product as u64));
                    d[n] = if word & 63 == 0x08 {
                        self.saturate(n, true, 0x8000, 0x7fff)
                    } else if (self.acc_h[n] as i16) < 0 {
                        0
                    } else if self.acc_h[n] != 0 || (self.acc_m[n] as i16) < 0 {
                        0xffff
                    } else {
                        self.acc_m[n]
                    };
                }
            }
            // VMACQ: the MPEG decoder's accumulate.
            0x0b => {
                for n in 0..8 {
                    let mut product = (u32::from(self.acc_h[n]) << 16 | u32::from(self.acc_m[n])) as i32;
                    if product < 0 && product & 1 << 5 == 0 {
                        product += 32;
                    } else if product >= 32 && product & 1 << 5 == 0 {
                        product -= 32;
                    }
                    self.acc_h[n] = (product >> 16) as u16;
                    self.acc_m[n] = product as u16;
                    d[n] = clamp16(product >> 1) & !15;
                }
            }
            // VMADL: accumulate the high half of unsigned low parts.
            0x0c => {
                for n in 0..8 {
                    let product = u64::from((u32::from(s[n]) * u32::from(t[n])) >> 16);
                    self.set_acc(n, self.acc(n).wrapping_add(product));
                    d[n] = self.saturate(n, false, 0, 0xffff);
                }
            }
            // VMADM: accumulate signed vs times unsigned vt.
            0x0d => {
                for n in 0..8 {
                    let product = i64::from(i32::from(s[n] as i16) * i32::from(t[n]));
                    self.set_acc(n, self.acc(n).wrapping_add(product as u64));
                    d[n] = self.saturate(n, true, 0x8000, 0x7fff);
                }
            }
            // VMADN: accumulate unsigned vs times signed vt.
            0x0e => {
                for n in 0..8 {
                    let product = i64::from(i32::from(s[n]) * i32::from(t[n] as i16));
                    self.set_acc(n, self.acc(n).wrapping_add(product as u64));
                    d[n] = self.saturate(n, false, 0, 0xffff);
                }
            }
            // VMADH: accumulate signed high parts into high and middle.
            0x0f => {
                for n in 0..8 {
                    let high = ((self.acc(n) >> 16) as u32 as i32)
                        .wrapping_add(i32::from(s[n] as i16) * i32::from(t[n] as i16));
                    self.acc_h[n] = (high >> 16) as u16;
                    self.acc_m[n] = high as u16;
                    d[n] = self.saturate(n, true, 0x8000, 0x7fff);
                }
            }
            // VADD, VSUB: with VCO's carry, clamped.
            0x10 | 0x11 => {
                for n in 0..8 {
                    let carry = i32::from(Self::lane_flag(self.vco, n));
                    let (a, b) = (i32::from(s[n] as i16), i32::from(t[n] as i16));
                    let result = if word & 63 == 0x10 { a + b + carry } else { a - b - carry };
                    self.acc_l[n] = result as u16;
                    d[n] = clamp16(result);
                }
                self.vco = 0;
            }
            // VABS: vt with the sign of vs (0 where vs is 0).
            0x13 => {
                for n in 0..8 {
                    let (a, b) = (s[n] as i16, t[n] as i16);
                    let (acc, out) = match a {
                        0 => (0, 0),
                        1.. => (b, b),
                        _ if b == i16::MIN => (i16::MIN, i16::MAX),
                        _ => (-b, -b),
                    };
                    self.acc_l[n] = acc as u16;
                    d[n] = out as u16;
                }
            }
            // VADDC: unsigned add, the carries to VCO.
            0x14 => {
                self.vco = 0;
                for n in 0..8 {
                    let result = u32::from(s[n]) + u32::from(t[n]);
                    self.acc_l[n] = result as u16;
                    self.vco |= u16::from(result >> 16 != 0) << n;
                }
                d = self.acc_l;
            }
            // VSUBC: unsigned subtract, borrows and differences to VCO.
            0x15 => {
                self.vco = 0;
                for n in 0..8 {
                    let result = u32::from(s[n]).wrapping_sub(u32::from(t[n]));
                    self.acc_l[n] = result as u16;
                    self.vco |= u16::from(result >> 16 != 0) << n | u16::from(result != 0) << (n + 8);
                }
                d = self.acc_l;
            }
            // VSAR: read a slice of the accumulator (e 8, 9, 10).
            0x1d => {
                d = match e {
                    8 => self.acc_h,
                    9 => self.acc_m,
                    10 => self.acc_l,
                    _ => [0; 8],
                };
            }
            // Compares: VLT, VEQ, VNE, VGE, into VCC, picking vs or vt.
            0x20..=0x23 => {
                let mut vcc = 0;
                for n in 0..8 {
                    let (a, b) = (s[n] as i16, t[n] as i16);
                    let (carry, ne) = (Self::lane_flag(self.vco, n), Self::lane_flag(self.vco, n + 8));
                    let pick = match word & 63 {
                        0x20 => a < b || a == b && carry && ne,
                        0x21 => !ne && a == b,
                        0x22 => a != b || ne,
                        _ => a > b || a == b && (!carry || !ne),
                    };
                    vcc |= u16::from(pick) << n;
                    self.acc_l[n] = if pick { s[n] } else { t[n] };
                }
                self.vcc = vcc;
                self.vco = 0;
                d = self.acc_l;
            }
            0x24 => d = self.clip_low(s, t),
            0x25 => d = self.clip_high(s, t),
            0x26 => d = self.clip_one(s, t),
            // VMRG: vs where VCC says, vt elsewhere.
            0x27 => {
                for n in 0..8 {
                    self.acc_l[n] = if Self::lane_flag(self.vcc, n) { s[n] } else { t[n] };
                }
                self.vco = 0;
                d = self.acc_l;
            }
            // VAND, VNAND, VOR, VNOR, VXOR, VNXOR.
            0x28..=0x2d => {
                for n in 0..8 {
                    self.acc_l[n] = match word & 63 {
                        0x28 => s[n] & t[n],
                        0x29 => !(s[n] & t[n]),
                        0x2a => s[n] | t[n],
                        0x2b => !(s[n] | t[n]),
                        0x2c => s[n] ^ t[n],
                        _ => !(s[n] ^ t[n]),
                    };
                }
                d = self.acc_l;
            }
            // The single-lane ones write one lane of vd and leave the rest.
            0x30..=0x36 => {
                self.single_lane(word, e, vt, vs & 7, vd);
                return;
            }
            // VNOP, VNULL.
            0x37 | 0x3f => return,
            // What the chip does not have: vs + vt into the accumulator's
            // low slice, and 0 to vd.
            _ => {
                for n in 0..8 {
                    self.acc_l[n] = (s[n] as i16 as i32 + t[n] as i16 as i32) as u16;
                }
            }
        }
        self.regs[vd] = d;
    }

    /// VCL: the low half of a double-precision clip, using VCO, VCC and
    /// VCE as VCH left them.
    fn clip_low(&mut self, s: [u16; 8], t: [u16; 8]) -> [u16; 8] {
        let (mut vcc, vco, vce) = (self.vcc, self.vco, self.vce);
        for n in 0..8 {
            let (carry, ne) = (Self::lane_flag(vco, n), Self::lane_flag(vco, n + 8));
            let (mut le, mut ge) = (Self::lane_flag(vcc, n), Self::lane_flag(vcc, n + 8));
            self.acc_l[n] = if carry {
                if !ne {
                    let sum = u32::from(s[n]) + u32::from(t[n]);
                    let (zero, overflow) = (sum & 0xffff == 0, sum > 0xffff);
                    le = if vce >> n & 1 != 0 { zero || !overflow } else { zero && !overflow };
                }
                if le { t[n].wrapping_neg() } else { s[n] }
            } else {
                if !ne {
                    ge = i32::from(s[n]) - i32::from(t[n]) >= 0;
                }
                if ge { t[n] } else { s[n] }
            };
            vcc = vcc & !(1 << n | 1 << (n + 8)) | u16::from(le) << n | u16::from(ge) << (n + 8);
        }
        self.vcc = vcc;
        self.vco = 0;
        self.vce = 0;
        self.acc_l
    }

    /// VCH: the high half of a clip, setting VCO, VCC and VCE for VCL.
    fn clip_high(&mut self, s: [u16; 8], t: [u16; 8]) -> [u16; 8] {
        let (mut vco, mut vcc, mut vce) = (0u16, 0u16, 0u8);
        for n in 0..8 {
            let (a, b) = (s[n] as i16, t[n] as i16);
            let ne_value = s[n] != t[n] ^ 0xffff;
            if (a ^ b) < 0 {
                let result = a.wrapping_add(b);
                self.acc_l[n] = if result <= 0 { b.wrapping_neg() as u16 } else { s[n] };
                vcc |= u16::from(result <= 0) << n | u16::from(b < 0) << (n + 8);
                vco |= 1 << n | u16::from(result != 0 && ne_value) << (n + 8);
                vce |= u8::from(result == -1) << n;
            } else {
                let result = a.wrapping_sub(b);
                self.acc_l[n] = if result >= 0 { t[n] } else { s[n] };
                vcc |= u16::from(b < 0) << n | u16::from(result >= 0) << (n + 8);
                vco |= u16::from(result != 0 && ne_value) << (n + 8);
            }
        }
        (self.vco, self.vcc, self.vce) = (vco, vcc, vce);
        self.acc_l
    }

    /// VCR: a clip in one's complement.
    fn clip_one(&mut self, s: [u16; 8], t: [u16; 8]) -> [u16; 8] {
        let mut vcc = 0u16;
        for n in 0..8 {
            let (a, b) = (s[n] as i16, t[n] as i16);
            if (a ^ b) < 0 {
                let le = i32::from(a) + i32::from(b) + 1 <= 0;
                vcc |= u16::from(le) << n | u16::from(b < 0) << (n + 8);
                self.acc_l[n] = if le { !t[n] } else { s[n] };
            } else {
                let ge = i32::from(a) - i32::from(b) >= 0;
                vcc |= u16::from(b < 0) << n | u16::from(ge) << (n + 8);
                self.acc_l[n] = if ge { t[n] } else { s[n] };
            }
        }
        self.vcc = vcc;
        self.vco = 0;
        self.vce = 0;
        self.acc_l
    }

    /// VRCP, VRCPL, VRCPH, VMOV, VRSQ, VRSQL, VRSQH: one lane of vt (by
    /// the element field) in, one lane of vd (by vs's field) out.
    fn single_lane(&mut self, word: u32, e: usize, vt: usize, de: usize, vd: usize) {
        self.acc_l = broadcast(self.regs[vt], e);
        self.single_lane_result(word, e, vt, de, vd);
    }

    /// The single-lane instructions' lane of vd and reciprocal unit, after
    /// the accumulator's low slice (which vt is not) took broadcast vt.
    fn single_lane_result(&mut self, word: u32, e: usize, vt: usize, de: usize, vd: usize) {
        let source = self.regs[vt];
        let lane = source[e & 7];
        match word & 63 {
            0x33 => {
                // VMOV.
                self.regs[vd][de] = source[broadcast_lane(e, de)];
            }
            0x32 | 0x36 => {
                // VRCPH, VRSQH: the high half of the next input, and of the
                // last result.
                self.div_pending = true;
                self.div_in = lane;
                self.regs[vd][de] = self.div_out;
            }
            op => {
                let long = matches!(op, 0x31 | 0x35);
                let root = op >= 0x34;
                let input = if long && self.div_pending {
                    (u32::from(self.div_in) << 16 | u32::from(lane)) as i32
                } else {
                    i32::from(lane as i16)
                };
                let result = reciprocal(input, root);
                self.div_pending = false;
                self.div_out = (result >> 16) as u16;
                self.regs[vd][de] = result as u16;
            }
        }
    }
}

/// The reciprocal (or inverse square root) unit: a 32-bit fixed point
/// result from the table, as ares computes it.
pub(crate) fn reciprocal(input: i32, root: bool) -> u32 {
    let mask = input >> 31;
    let mut data = input ^ mask;
    if input > -32768 {
        data -= mask;
    }
    if data == 0 {
        return 0x7fff_ffff;
    }
    if input == -32768 {
        return 0xffff_0000;
    }
    let shift = (data as u32).leading_zeros();
    let index = (((data as u64) << shift) & 0x7fc0_0000) as u32 >> 22;
    let tables = &*TABLES;
    let result = if root {
        let entry = i32::from(tables.inverse_roots[(index & 0x1fe | shift & 1) as usize]);
        ((0x10000 | entry) << 14) >> ((31 - shift) >> 1)
    } else {
        let entry = i32::from(tables.reciprocals[index as usize]);
        ((0x10000 | entry) << 14) >> (31 - shift)
    };
    (result ^ mask) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reciprocals() {
        // 1 / x in S1.30 from the table's 16 bits: 1/1 and 1/2 just below
        // 1.0 and 0.5; 0 gives the largest value, -32768 its own.
        assert_eq!(reciprocal(0, false), 0x7fff_ffff);
        assert_eq!(reciprocal(1, false), 0x7fff_c000);
        assert_eq!(reciprocal(2, false), 0x3fff_e000);
        assert_eq!(reciprocal(-1, false), !0x7fff_c000);
        assert_eq!(reciprocal(-32768, false), 0xffff_0000);
        assert_eq!(TABLES.reciprocals[1], 0xff00);
    }

    #[test]
    fn broadcasts() {
        let v = [0, 1, 2, 3, 4, 5, 6, 7];
        assert_eq!(broadcast(v, 3), [1, 1, 3, 3, 5, 5, 7, 7]);
        assert_eq!(broadcast(v, 6), [2, 2, 2, 2, 6, 6, 6, 6]);
        assert_eq!(broadcast(v, 13), [5; 8]);
    }

    #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
    #[test]
    fn rsp_acceleration_preferences_and_counters_are_not_save_state() {
        use crate::N64;
        assert!(neon_available());
        let mut rom = vec![0; 0x1000];
        rom[..4].copy_from_slice(&0x8037_1240u32.to_be_bytes());
        let mut fast = N64::new(&rom).unwrap();
        fast.set_rsp_vector_acceleration(false);
        assert!(!fast.rsp.vu.accelerated);
        assert!(fast.rsp.fast_memory);
        fast.set_rsp_vector_acceleration(true);
        assert!(fast.rsp.vu.accelerated);
        fast.rsp.vu.regs[1] = [0x8000; 8];
        fast.rsp.vu.regs[2] = [0x8000; 8];
        fast.set_rsp_profiling(true);
        let initial = fast.save_state();
        let mut reference = fast.clone();
        reference.set_rsp_acceleration(false);
        for n64 in [&mut fast, &mut reference] {
            n64.rsp.vu.execute(0x4a02_08c0); // VMULF v3,v1,v2
        }
        assert_eq!(fast.rsp_stats().native_vector_instructions, 1);
        assert_eq!(reference.rsp_stats().native_vector_instructions, 0);
        assert_eq!(fast.rsp.vu.regs[3], [0x7fff; 8]);
        assert!(fast.save_state() == reference.save_state());

        fast.set_rsp_acceleration(false);
        fast.load_state(&initial).unwrap();
        assert!(!fast.rsp.vu.accelerated);
        assert_eq!(fast.rsp_stats().native_vector_instructions, 0);
        fast.rsp.vu.execute(0x4a02_08c0);
        assert_eq!(fast.rsp_stats().native_vector_instructions, 0);
        assert!(fast.save_state() == reference.save_state());

        fast.set_rsp_acceleration(true);
        fast.load_state(&initial).unwrap();
        reference.load_state(&initial).unwrap();
        assert!(fast.rsp.vu.accelerated);
        assert!(!reference.rsp.vu.accelerated);
        for n64 in [&mut fast, &mut reference] {
            n64.rsp.vu.execute(0x4a02_08c1); // VMULU v3,v1,v2
        }
        assert_eq!(fast.rsp_stats().native_vector_instructions, 1);
        assert!(fast.save_state() == reference.save_state());
        let state = fast.save_state();
        assert!(fast.load_state(&state[..state.len() - 1]).is_err());
        assert_eq!(fast.rsp_stats().native_vector_instructions, 1);
        assert!(fast.save_state() == state);
    }
}
