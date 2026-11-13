//! COP1, the VR4300's FPU: 32 registers of 64 bits, single and double
//! precision, conversions to and from 32- and 64-bit integers, and the
//! compares that set the condition its branches test.
//!
//! The VR4300 hands some cases to software rather than compute them: it
//! raises the unimplemented operation exception (E) for denormal inputs,
//! quiet NaN inputs, results that would be denormal (unless FCR31 flushes
//! them to zero, with the underflow and inexact exceptions off), and
//! conversions to integers of infinities, NaNs and values out of range.
//! These follow ares (ares/n64/cpu/interpreter-fpu.cpp), whose FPU is
//! checked against n64-systemtest.
//!
//! Rust's floating point rounds to nearest only, so the other rounding
//! modes are done here: each result is computed rounded to nearest along
//! with which side of it the exact value lies (from the rounding error,
//! which fma and the two-sum trick give exactly), and moved one step when
//! the mode rounds the other way. That side also gives the inexact flag.
//! The rounding is to the format's precision with no limit on the exponent,
//! and the result is then tiny (below the smallest normal value) or has
//! overflowed (above the largest) as MIPS defines both: after rounding, in
//! the mode. Products, quotients and roots are worked out on the operands'
//! significands, with the exponent apart, so that results far outside a
//! double's range (a double's product of two tiny ones) still round on their
//! exact value. A tiny result never reaches the register: it is left to
//! software, or flushed.
//!
//! With Status.FR clear the registers pair up into 16 of 64 bits: the
//! moves and loads reach the odd registers as the top halves of the even
//! ones; the arithmetic reads its fs operand from the even register (as the
//! chip does) and writes fd whole.

use crate::cop0::FR;
use crate::cpu::FLOATING_POINT;
use crate::decode::{Instr, Op};
use crate::N64;
use savestate::state;

// The exceptions, as bits of FCR31's flags (from bit 2), enables (from
// bit 7) and cause (from bit 12; only the cause has E).
const INEXACT: u32 = 1;
const UNDERFLOW: u32 = 2;
const OVERFLOW: u32 = 4;
const DIVISION_BY_ZERO: u32 = 8;
const INVALID: u32 = 16;
const UNIMPLEMENTED: u32 = 32;

// FCR31: the compare's condition, and flushing denormal results to zero.
const CONDITION: u32 = 1 << 23;
const FLUSH: u32 = 1 << 24;

#[derive(Clone)]
pub struct Fpu {
    pub regs: [u64; 32],
    pub fcr31: u32,
}

state!(Fpu { regs, fcr31 });

impl Fpu {
    pub fn new() -> Fpu {
        Fpu { regs: [0; 32], fcr31: 0 }
    }

    fn enables(&self) -> u32 {
        self.fcr31 >> 7 & 31
    }

    fn rounding(&self) -> Rounding {
        [Rounding::Nearest, Rounding::Zero, Rounding::Up, Rounding::Down][self.fcr31 as usize & 3]
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Rounding {
    Nearest,
    Zero,
    Up,
    Down,
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Format {
    Single,
    Double,
}

/// What an operand is, by its bits (the MIPS NaN that signals has the top
/// bit of the fraction set, the opposite of IEEE 754-2008's advice).
#[derive(Clone, Copy, PartialEq)]
enum Class {
    Number,
    Denormal,
    QuietNan,
    SignalingNan,
}

impl Format {
    fn class(self, bits: u64) -> Class {
        let (exponent, fraction, quiet_bit) = match self {
            Format::Single => (bits >> 23 & 0xff == 0xff, bits & 0x7f_ffff, 1 << 22),
            Format::Double => (bits >> 52 & 0x7ff == 0x7ff, bits & 0xf_ffff_ffff_ffff, 1 << 51),
        };
        let zero_exponent = match self {
            Format::Single => bits >> 23 & 0xff == 0,
            Format::Double => bits >> 52 & 0x7ff == 0,
        };
        if exponent && fraction != 0 {
            if fraction & quiet_bit != 0 { Class::SignalingNan } else { Class::QuietNan }
        } else if zero_exponent && fraction != 0 {
            Class::Denormal
        } else {
            Class::Number
        }
    }

    fn value(self, bits: u64) -> f64 {
        match self {
            Format::Single => f64::from(f32::from_bits(bits as u32)),
            Format::Double => f64::from_bits(bits),
        }
    }

    fn bits(self, value: f64) -> u64 {
        match self {
            Format::Single => u64::from((value as f32).to_bits()),
            Format::Double => value.to_bits(),
        }
    }

    /// The NaN results get.
    fn default_nan(self) -> u64 {
        match self {
            Format::Single => 0x7fbf_ffff,
            Format::Double => 0x7ff7_ffff_ffff_ffff,
        }
    }

    fn max(self) -> f64 {
        match self {
            Format::Single => f64::from(f32::MAX),
            Format::Double => f64::MAX,
        }
    }

    fn min_positive(self) -> f64 {
        match self {
            Format::Single => f64::from(f32::MIN_POSITIVE),
            Format::Double => f64::MIN_POSITIVE,
        }
    }

    /// The exponents of the smallest and the largest normal values.
    fn exponents(self) -> (i32, i32) {
        match self {
            Format::Single => (-126, 127),
            Format::Double => (-1022, 1023),
        }
    }

    /// The format's last bit as a double's bits: the step between two of its
    /// values (24 bits of significand to a double's 53).
    fn unit(self) -> u64 {
        match self {
            Format::Single => 1 << 29,
            Format::Double => 1,
        }
    }
}

/// x + e exactly, where e is the error of rounding a + b to x.
fn two_sum(a: f64, b: f64) -> (f64, f64) {
    let x = a + b;
    if !x.is_finite() {
        return (x, 0.0);
    }
    let bb = x - a;
    (x, (a - (x - bb)) + (b - bb))
}

/// A normal double as m · 2^k: m keeps its sign and lies in [1, 2).
fn split(v: f64) -> (f64, i32) {
    let bits = v.to_bits();
    (f64::from_bits(bits & !(0x7ff << 52) | 1023 << 52), (bits >> 52 & 0x7ff) as i32 - 1023)
}

/// A result rounded to its format in a mode: the value, and whether it is
/// inexact, overflowed, or is tiny (then the value is a zero of its sign,
/// for the flush to pick from).
struct Rounded {
    value: f64,
    inexact: bool,
    overflow: bool,
    tiny: bool,
}

/// A result that is exact as it is: a zero, an infinity from an infinite
/// operand or a division by zero, or a NaN.
fn exact(value: f64) -> Rounded {
    Rounded { value, inexact: false, overflow: false, tiny: false }
}

/// Rounds (x + e) · 2^scale to the format in a mode. x is a normal double
/// (or zero) that rounds to nearest in the format as the exact value x + e
/// does: that value rounded to a double, or for a single any double near it
/// that is not on a single's halfway point unless the value is. e need only
/// have the right sign and be smaller than a double's step at x.
///
/// The rounding is to the format's precision with no limit on the exponent
/// (on the double's bits, the exponent apart in `scale`), and only then is
/// the result checked against the format's range: above its largest
/// exponent it has overflowed, and below the smallest it is tiny, as MIPS
/// detects tininess (after rounding). So a value just below the smallest
/// normal that rounds up to it is not tiny, and one that rounds to the step
/// below it is, though rounding into the denormals would give the smallest
/// normal for both.
fn round(x: f64, e: f64, scale: i32, format: Format, mode: Rounding) -> Rounded {
    if x == 0.0 {
        return exact(x);
    }
    let unit = format.unit();
    let bits = x.to_bits();
    // To nearest, ties to even, on the bits; a carry goes into the exponent.
    let low = bits & (unit - 1);
    let mut rounded = bits - low;
    if unit > 1 && (low > unit / 2 || low == unit / 2 && rounded & unit != 0) {
        rounded += unit;
    }
    // The exact value is above the rounded one if this is positive.
    let side = (x - f64::from_bits(rounded)) + e;
    let positive = x > 0.0;
    // A step away from zero (true) or towards it.
    let step = match mode {
        Rounding::Nearest => None,
        Rounding::Zero => (side != 0.0 && (side > 0.0) != positive).then_some(false),
        Rounding::Up => (side > 0.0).then_some(positive),
        Rounding::Down => (side < 0.0).then_some(!positive),
    };
    match step {
        Some(true) => rounded += unit,
        Some(false) => rounded -= unit,
        None => {}
    }
    let inexact = side != 0.0;
    let biased = (rounded >> 52 & 0x7ff) as i32;
    let exponent = biased - 1023 + scale;
    let (least, most) = format.exponents();
    if biased == 0x7ff || exponent > most {
        let infinity = match mode {
            Rounding::Nearest => true,
            Rounding::Zero => false,
            Rounding::Up => positive,
            Rounding::Down => !positive,
        };
        let value = if infinity { f64::INFINITY } else { format.max() };
        return Rounded { value: value.copysign(x), inexact: true, overflow: true, tiny: false };
    }
    if biased == 0 || exponent < least {
        return Rounded { value: 0.0f64.copysign(x), inexact, overflow: false, tiny: true };
    }
    let value = f64::from_bits(rounded.wrapping_add((i64::from(scale) << 52) as u64));
    Rounded { value, inexact, overflow: false, tiny: false }
}

/// a + b rounded. Sums that a double cannot hold are worked out on halves.
fn sum(a: f64, b: f64, format: Format, mode: Rounding) -> Rounded {
    let (x, e) = two_sum(a, b);
    if x.is_infinite() {
        let (x, e) = two_sum(a * 0.5, b * 0.5);
        return round(x, e, 1, format, mode);
    }
    round(x, e, 0, format, mode)
}

impl N64 {
    fn status_fr(&self) -> bool {
        self.cpu.cop0.status & FR != 0
    }

    /// The fs operand of arithmetic: the even register of a pair when
    /// Status.FR is clear.
    fn fs_bits(&self, i: usize, format: Format) -> u64 {
        let reg = self.cpu.fpu.regs[if self.status_fr() { i } else { i & !1 }];
        if format == Format::Single { reg & 0xffff_ffff } else { reg }
    }

    /// The ft operand of arithmetic, taken from its register as named.
    fn ft_bits(&self, i: usize, format: Format) -> u64 {
        let reg = self.cpu.fpu.regs[i];
        if format == Format::Single { reg & 0xffff_ffff } else { reg }
    }

    /// The 32-bit moves' view of a register: with FR clear, an odd one is
    /// the top half of the even one below it.
    fn fpr32(&self, i: usize) -> u32 {
        if self.status_fr() || i & 1 == 0 {
            self.cpu.fpu.regs[i] as u32
        } else {
            (self.cpu.fpu.regs[i & !1] >> 32) as u32
        }
    }

    fn set_fpr32(&mut self, i: usize, value: u32) {
        let regs = &mut self.cpu.fpu.regs;
        if self.cpu.cop0.status & FR != 0 || i & 1 == 0 {
            regs[i] = regs[i] & 0xffff_ffff_0000_0000 | u64::from(value);
        } else {
            regs[i & !1] = regs[i & !1] & 0xffff_ffff | u64::from(value) << 32;
        }
    }

    /// The 64-bit moves' view: with FR clear, the even register of a pair.
    fn fpr64(&self, i: usize) -> u64 {
        self.cpu.fpu.regs[if self.status_fr() { i } else { i & !1 }]
    }

    fn set_fpr64(&mut self, i: usize, value: u64) {
        let i = if self.status_fr() { i } else { i & !1 };
        self.cpu.fpu.regs[i] = value;
    }

    fn floating_point_exception(&mut self) {
        self.exception(FLOATING_POINT, 0x180);
    }

    /// Sets the cause bits; raises the exception if one of them is enabled
    /// (E always is), or else adds them to the flags. Whether the result
    /// may be written.
    fn fpu_raise(&mut self, cause: u32) -> bool {
        let fpu = &mut self.cpu.fpu;
        fpu.fcr31 |= cause << 12;
        if cause & (fpu.enables() | UNIMPLEMENTED) != 0 {
            self.floating_point_exception();
            return false;
        }
        fpu.fcr31 |= (cause & 31) << 2;
        true
    }

    pub(crate) fn fpu_instruction(&mut self, i: Instr) {
        if !self.cop1_usable() {
            return;
        }
        match i.op {
            Op::Mfc1 => {
                let value = self.fpr32(i.fs());
                self.cpu.gpr[i.rt()] = value as i32 as i64 as u64;
            }
            Op::Dmfc1 => self.cpu.gpr[i.rt()] = self.fpr64(i.fs()),
            Op::Mtc1 => self.set_fpr32(i.fs(), self.cpu.gpr[i.rt()] as u32),
            Op::Dmtc1 => self.set_fpr64(i.fs(), self.cpu.gpr[i.rt()]),
            Op::Cfc1 => {
                let value = match i.fs() {
                    0 => 0x0a00,
                    31 => self.cpu.fpu.fcr31,
                    _ => 0,
                };
                self.cpu.gpr[i.rt()] = value as i32 as i64 as u64;
            }
            Op::Ctc1 => {
                if i.fs() == 31 {
                    let fpu = &mut self.cpu.fpu;
                    fpu.fcr31 = self.cpu.gpr[i.rt()] as u32 & 0x0183_ffff;
                    let cause = fpu.fcr31 >> 12 & 63;
                    if cause & (fpu.enables() | UNIMPLEMENTED) != 0 {
                        self.floating_point_exception();
                    }
                }
            }
            Op::Bc1f | Op::Bc1t | Op::Bc1fl | Op::Bc1tl => {
                self.cpu.fpu.fcr31 &= !(63 << 12);
                let condition = self.cpu.fpu.fcr31 & CONDITION != 0;
                let taken = condition == matches!(i.op, Op::Bc1t | Op::Bc1tl);
                if matches!(i.op, Op::Bc1f | Op::Bc1t) {
                    self.branch_on(taken, i);
                } else {
                    let target = self.branch_target(i);
                    self.branch_likely(taken, target);
                }
            }
            Op::Lwc1 | Op::Ldc1 | Op::Swc1 | Op::Sdc1 => self.fpu_load_store(i),
            Op::FpuOp => {
                let format = if i.fmt() == 16 { Format::Single } else { Format::Double };
                // Every operation but MOV starts with the cause cleared.
                if i.funct() != 6 {
                    self.cpu.fpu.fcr31 &= !(63 << 12);
                }
                match i.funct() {
                    0..=4 => self.arithmetic(i, format),
                    5 | 7 => self.sign(i, format),
                    6 => {
                        let value = self.fpr64(i.fs());
                        self.cpu.fpu.regs[i.fd()] = value;
                    }
                    8..=15 | 36 | 37 => self.to_integer(i, format),
                    32 | 33 => self.convert(i, format),
                    48..=63 => self.compare(i, format),
                    _ => {
                        self.fpu_raise(UNIMPLEMENTED);
                    }
                }
            }
            Op::FpuCvtInt => {
                self.cpu.fpu.fcr31 &= !(63 << 12);
                match i.funct() {
                    32 | 33 => self.from_integer(i),
                    _ => {
                        self.fpu_raise(UNIMPLEMENTED);
                    }
                }
            }
            _ => {
                self.cpu.fpu.fcr31 &= !(63 << 12);
                self.fpu_raise(UNIMPLEMENTED);
            }
        }
    }

    fn fpu_load_store(&mut self, i: Instr) {
        use crate::cop0::Access;
        let vaddr = self.cpu.gpr[i.rs()].wrapping_add(i.simm());
        let ft = i.ft();
        match i.op {
            Op::Lwc1 => {
                if let Some(phys) = self.translate(vaddr, 4, Access::Load) {
                    let value = self.read_u32(phys);
                    self.set_fpr32(ft, value);
                }
            }
            Op::Ldc1 => {
                if let Some(phys) = self.translate(vaddr, 8, Access::Load) {
                    let value = self.read_u64(phys);
                    self.set_fpr64(ft, value);
                }
            }
            Op::Swc1 => {
                if let Some(phys) = self.translate(vaddr, 4, Access::Store) {
                    let value = self.fpr32(ft);
                    self.write_u32(phys, value);
                }
            }
            _ => {
                if let Some(phys) = self.translate(vaddr, 8, Access::Store) {
                    let value = self.fpr64(ft);
                    self.write_u64(phys, value);
                }
            }
        }
    }

    /// The checks of an arithmetic operand: E for a denormal or a quiet NaN,
    /// V for a signaling NaN. Whether the operation goes on.
    fn check_inputs(&mut self, classes: &[Class]) -> bool {
        if classes.iter().any(|&c| c == Class::QuietNan || c == Class::Denormal) {
            return self.fpu_raise(UNIMPLEMENTED);
        }
        if classes.contains(&Class::SignalingNan) {
            return self.fpu_raise(INVALID);
        }
        true
    }

    /// Writes a result to fd, after the exceptions it raises: a tiny result
    /// is left to software (E) unless FCR31 flushes it, which needs the
    /// underflow and inexact exceptions off; NaNs become the default NaN.
    fn deliver(&mut self, fd: usize, result: Rounded, format: Format, invalid: bool, division_by_zero: bool) {
        let fpu = &self.cpu.fpu;
        let flush = fpu.fcr31 & FLUSH != 0 && fpu.enables() & (UNDERFLOW | INEXACT) == 0;
        if result.tiny && !flush {
            self.fpu_raise(UNIMPLEMENTED);
            return;
        }
        let mut cause = 0;
        if division_by_zero {
            cause |= DIVISION_BY_ZERO;
        }
        if result.inexact || result.tiny {
            cause |= INEXACT;
        }
        if result.tiny {
            cause |= UNDERFLOW;
        }
        if result.overflow {
            cause |= OVERFLOW;
        }
        if invalid {
            cause |= INVALID;
        }
        if cause != 0 && !self.fpu_raise(cause) {
            return;
        }
        let mut value = result.value;
        if result.tiny {
            // Flushed: to zero, or to the smallest normal value where the
            // rounding mode goes away from zero.
            let negative = value.is_sign_negative();
            value = match (self.cpu.fpu.rounding(), negative) {
                (Rounding::Up, false) => format.min_positive(),
                (Rounding::Down, true) => -format.min_positive(),
                _ => value,
            };
        }
        let bits = if value.is_nan() { format.default_nan() } else { format.bits(value) };
        self.cpu.fpu.regs[fd] = bits;
    }

    /// ADD, SUB, MUL, DIV and SQRT. With a zero or an infinity among the
    /// operands (or a negative root) the result is exact, an infinity from a
    /// division by zero, or a NaN; the rest are rounded, products, quotients
    /// and roots from the significands in [1, 2) (the root's in [1, 4), for
    /// an even exponent) and the exponent apart.
    fn arithmetic(&mut self, i: Instr, format: Format) {
        let funct = i.funct();
        let (sb, tb) = (self.fs_bits(i.fs(), format), self.ft_bits(i.ft(), format));
        // SQRT has one operand; ft is not looked at.
        let classes = [format.class(sb), if funct == 4 { Class::Number } else { format.class(tb) }];
        if !self.check_inputs(&classes) {
            return;
        }
        if classes.iter().any(|&c| c == Class::SignalingNan) {
            self.cpu.fpu.regs[i.fd()] = format.default_nan();
            return;
        }
        let (a, b) = (format.value(sb), format.value(tb));
        let mode = self.cpu.fpu.rounding();
        let special = |v: f64| v == 0.0 || v.is_infinite();
        let division_by_zero = funct == 3 && b == 0.0 && a != 0.0 && a.is_finite();
        let result = if special(a) || funct != 4 && special(b) || funct == 4 && a < 0.0 {
            exact(match funct {
                0 => a + b,
                1 => a - b,
                2 => a * b,
                3 => a / b,
                _ => a.sqrt(),
            })
        } else {
            match funct {
                0 => sum(a, b, format, mode),
                1 => sum(a, -b, format, mode),
                2 => {
                    let ((ma, ka), (mb, kb)) = (split(a), split(b));
                    let x = ma * mb;
                    round(x, ma.mul_add(mb, -x), ka + kb, format, mode)
                }
                3 => {
                    // The remainder ma - x · mb is exact; over mb it is the
                    // quotient's error.
                    let ((ma, ka), (mb, kb)) = (split(a), split(b));
                    let x = ma / mb;
                    round(x, (-x).mul_add(mb, ma) / mb, ka - kb, format, mode)
                }
                _ => {
                    // m - x² is exact; over 2x it is the root's error (its
                    // sign alone would not do for a single, whose rounding
                    // weighs it against x's distance to the nearest single).
                    let (m, k) = split(a);
                    let (m, k) = if k & 1 != 0 { (m * 2.0, k - 1) } else { (m, k) };
                    let x = m.sqrt();
                    round(x, (-x).mul_add(x, m) / (x + x), k / 2, format, mode)
                }
            }
        };
        let invalid = result.value.is_nan();
        self.deliver(i.fd(), result, format, invalid, division_by_zero);
    }

    /// ABS and NEG, which the VR4300 treats as arithmetic: NaNs and
    /// denormals raise their exceptions.
    fn sign(&mut self, i: Instr, format: Format) {
        let bits = self.fs_bits(i.fs(), format);
        let class = format.class(bits);
        if !self.check_inputs(&[class]) {
            return;
        }
        if class == Class::SignalingNan {
            self.cpu.fpu.regs[i.fd()] = format.default_nan();
            return;
        }
        let a = format.value(bits);
        let value = if i.funct() == 5 { a.abs() } else { -a };
        self.cpu.fpu.regs[i.fd()] = format.bits(value);
    }

    /// CVT.S.D and CVT.D.S (converting to the same format is left to
    /// software).
    fn convert(&mut self, i: Instr, format: Format) {
        let to = if i.funct() == 32 { Format::Single } else { Format::Double };
        if to == format {
            self.fpu_raise(UNIMPLEMENTED);
            return;
        }
        let bits = self.fs_bits(i.fs(), format);
        let class = format.class(bits);
        if !self.check_inputs(&[class]) {
            return;
        }
        if class == Class::SignalingNan {
            self.cpu.fpu.regs[i.fd()] = to.default_nan();
            return;
        }
        let a = format.value(bits);
        let result = if a == 0.0 || a.is_infinite() { exact(a) } else { round(a, 0.0, 0, to, self.cpu.fpu.rounding()) };
        self.deliver(i.fd(), result, to, false, false);
    }

    /// CVT.S and CVT.D from W and L. A long of 2^55 or more (either sign)
    /// is left to software. A long that a double does not hold is rounded
    /// to one for a single to odd (of the two doubles around it, the one
    /// whose last bit is set), which never lands on a single's halfway
    /// point unless the long does, so the single's rounding is not a second
    /// one.
    fn from_integer(&mut self, i: Instr) {
        let to = if i.funct() == 32 { Format::Single } else { Format::Double };
        let bits = self.fs_bits(i.fs(), Format::Double);
        let value = if i.fmt() == 20 {
            i64::from(bits as i32)
        } else {
            let value = bits as i64;
            if !(-(1 << 55)..1 << 55).contains(&value) {
                self.fpu_raise(UNIMPLEMENTED);
                return;
            }
            value
        };
        let mut x = value as f64;
        let bits = x.to_bits();
        if to == Format::Single && x as i64 != value && bits & 1 == 0 {
            x = f64::from_bits(if (x as i64 > value) == (x > 0.0) { bits - 1 } else { bits + 1 });
        }
        let e = (i128::from(value) - x as i128) as f64;
        let result = round(x, e, 0, to, self.cpu.fpu.rounding());
        self.deliver(i.fd(), result, to, false, false);
    }

    /// ROUND, TRUNC, CEIL and FLOOR to W and L, and CVT.W and CVT.L in the
    /// current rounding mode. Infinities, NaNs, denormals and values out of
    /// range (2^31 for W; 2^53 for L) are left to software.
    fn to_integer(&mut self, i: Instr, format: Format) {
        let funct = i.funct();
        let word = matches!(funct, 12..=15 | 36);
        let bits = self.fs_bits(i.fs(), format);
        let a = format.value(bits);
        let limit = if word { 2f64.powi(31) } else { 2f64.powi(53) };
        let out_of_range = if word { a >= limit || a < -limit } else { a >= limit || a <= -limit };
        if format.class(bits) != Class::Number || a.is_infinite() || out_of_range {
            self.fpu_raise(UNIMPLEMENTED);
            return;
        }
        let mode = match funct {
            8 | 12 => Rounding::Nearest,
            9 | 13 => Rounding::Zero,
            10 | 14 => Rounding::Up,
            11 | 15 => Rounding::Down,
            _ => self.cpu.fpu.rounding(),
        };
        let r = match mode {
            Rounding::Nearest => a.round_ties_even(),
            Rounding::Zero => a.trunc(),
            Rounding::Up => a.ceil(),
            Rounding::Down => a.floor(),
        };
        if word && !(-limit..limit).contains(&r) {
            self.fpu_raise(UNIMPLEMENTED);
            return;
        }
        if r != a && !self.fpu_raise(INEXACT) {
            return;
        }
        self.cpu.fpu.regs[i.fd()] = if word { u64::from(r as i32 as u32) } else { r as i64 as u64 };
    }

    /// C.cond: the condition is the low four bits of the function, less
    /// than (bit 2), equal (bit 1) and unordered (bit 0), and bit 3 makes
    /// any NaN invalid rather than only signaling ones.
    fn compare(&mut self, i: Instr, format: Format) {
        let condition = i.funct() & 15;
        let (sb, tb) = (self.fs_bits(i.fs(), format), self.ft_bits(i.ft(), format));
        let classes = [format.class(sb), format.class(tb)];
        let nan = |c: Class| c == Class::QuietNan || c == Class::SignalingNan;
        let result = if nan(classes[0]) || nan(classes[1]) {
            if (condition & 8 != 0 || classes.contains(&Class::SignalingNan)) && !self.fpu_raise(INVALID) {
                return;
            }
            condition & 1 != 0
        } else {
            let (a, b) = (format.value(sb), format.value(tb));
            condition & 4 != 0 && a < b || condition & 2 != 0 && a == b
        };
        let fpu = &mut self.cpu.fpu;
        fpu.fcr31 = if result { fpu.fcr31 | CONDITION } else { fpu.fcr31 & !CONDITION };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounding_modes() {
        // 1 + 2^-30 in single precision lies between 1 and the next float.
        let (x, e) = two_sum(1.0, 2f64.powi(-30));
        let up = round(x, e, 0, Format::Single, Rounding::Up);
        assert_eq!(up.value, f64::from(1.0f32.next_up()));
        assert!(up.inexact);
        assert_eq!(round(x, e, 0, Format::Single, Rounding::Nearest).value, 1.0);
        assert_eq!(round(x, e, 0, Format::Single, Rounding::Down).value, 1.0);
        let (x, e) = two_sum(-1.0, -(2f64.powi(-30)));
        assert_eq!(round(x, e, 0, Format::Single, Rounding::Zero).value, -1.0);
        assert_eq!(round(x, e, 0, Format::Single, Rounding::Down).value, f64::from((-1.0f32).next_down()));
        // An exact result is not inexact; an overflow in RZ stops at the
        // largest float.
        assert!(!round(1.5, 0.0, 0, Format::Single, Rounding::Up).inexact);
        let over = round(f64::from(f32::MAX) * 2.0, 0.0, 0, Format::Single, Rounding::Zero);
        assert!(over.overflow && over.value == f64::from(f32::MAX));
        // Just below 2^128 RZ gives the largest float without overflowing,
        // RN overflows; the exponent apart does not change that.
        let below = 2f64.powi(128) - 2f64.powi(103);
        let zero = round(below, 0.0, 0, Format::Single, Rounding::Zero);
        assert!(!zero.overflow && zero.inexact && zero.value == f64::from(f32::MAX));
        assert!(round(below, 0.0, 0, Format::Single, Rounding::Nearest).overflow);
        let scaled = round(below / 2f64.powi(200), 0.0, 200, Format::Single, Rounding::Zero);
        assert!(!scaled.overflow && scaled.value == f64::from(f32::MAX));
        // The step up from the largest double overflows.
        let max = round(f64::MAX, 1.0, 0, Format::Double, Rounding::Up);
        assert!(max.overflow && max.value == f64::INFINITY);
    }

    #[test]
    fn classes() {
        assert!(Format::Single.class(0x7fbf_ffff) == Class::QuietNan);
        assert!(Format::Single.class(0x7fc0_0000) == Class::SignalingNan);
        assert!(Format::Single.class(0x0000_0001) == Class::Denormal);
        assert!(Format::Double.class(0x7ff0_0000_0000_0000) == Class::Number);
    }

    /// A console with nothing to run, its FPU usable with Status.FR set.
    fn console() -> N64 {
        let mut rom = vec![0u8; 0x1000];
        rom[..4].copy_from_slice(&[0x80, 0x37, 0x12, 0x40]);
        let mut n64 = N64::new(&rom).unwrap();
        n64.cpu.cop0.status = crate::cop0::CU1 | FR;
        n64
    }

    /// One instruction (fd f4, fs f0, ft f2) from FCR31 `fcr31`: f4 after
    /// (0x0123_4567_89ab_cdef where nothing was written) and FCR31.
    fn run(n64: &mut N64, word: u32, fcr31: u32, a: u64, b: u64) -> (u64, u32) {
        n64.cpu.cop0.status = crate::cop0::CU1 | FR;
        n64.cpu.fpu.fcr31 = fcr31;
        n64.cpu.fpu.regs[0] = a;
        n64.cpu.fpu.regs[2] = b;
        n64.cpu.fpu.regs[4] = 0x0123_4567_89ab_cdef;
        n64.fpu_instruction(crate::decode::decode(word));
        (n64.cpu.fpu.regs[4], n64.cpu.fpu.fcr31)
    }

    /// FCR31 with exceptions as flags and cause, and the mode.
    fn raised(exceptions: u32, mode: u32) -> u32 {
        exceptions << 2 | exceptions << 12 | mode
    }

    /// n64-systemtest's cases (src/tests/cop1/mod.rs) for results below the
    /// smallest normal value with FS set, a root under directed rounding,
    /// and a conversion that overflows only rounding up: the result and
    /// FCR31 with all exceptions off; the same with FS clear or underflow
    /// enabled is the unimplemented exception, and with overflow enabled
    /// the overflow one.
    #[test]
    fn systemtest_cases() {
        let mut n64 = console();
        let (s, d) = (16, 17);
        let (nearest, zero, up, down) = (0, 1, 2, 3);
        let (min, nmin) = (0x0010_0000_0000_0000u64, 0x8010_0000_0000_0000u64);
        let (smin, snmin) = (0x0080_0000u64, 0x8080_0000u64);
        let ui = UNDERFLOW | INEXACT;
        let near_max = 3.40282348e+38_f64.to_bits();
        let cases = [
            ("MUL.D", cop1(d, 2, 2), nearest, nmin, min, 1 << 63, ui),
            ("MUL.D", cop1(d, 2, 2), up, nmin, min, 1 << 63, ui),
            ("MUL.D", cop1(d, 2, 2), down, nmin, min, nmin, ui),
            ("MUL.D", cop1(d, 2, 2), zero, nmin, min, 1 << 63, ui),
            ("MUL.D", cop1(d, 2, 2), nearest, min, min, 0, ui),
            ("MUL.D", cop1(d, 2, 2), up, min, min, min, ui),
            ("MUL.D", cop1(d, 2, 2), down, min, min, 0, ui),
            ("MUL.D", cop1(d, 2, 2), zero, min, min, 0, ui),
            // 1.0000001
            ("DIV.S", cop1(s, 3, 2), nearest, smin, 0x3f80_0001, 0, ui),
            ("DIV.S", cop1(s, 3, 2), zero, smin, 0x3f80_0001, 0, ui),
            ("DIV.S", cop1(s, 3, 2), down, smin, 0x3f80_0001, 0, ui),
            ("DIV.S", cop1(s, 3, 2), up, smin, 0x3f80_0001, smin, ui),
            ("DIV.S", cop1(s, 3, 2), nearest, snmin, 0x3f80_0001, 0x8000_0000, ui),
            ("DIV.S", cop1(s, 3, 2), down, snmin, 0x3f80_0001, snmin, ui),
            ("DIV.S", cop1(s, 3, 2), up, snmin, 0x3f80_0001, 0x8000_0000, ui),
            ("DIV.S", cop1(s, 3, 2), up, smin, 0x4000_0000, smin, ui),
            ("DIV.S", cop1(s, 3, 2), down, snmin, 0x4000_0000, snmin, ui),
            ("SQRT.S", cop1(s, 4, 0), nearest, 0x7f7f_ffff, 0, 0x5f7f_ffff, INEXACT),
            ("SQRT.S", cop1(s, 4, 0), up, 0x7f7f_ffff, 0, 0x5f80_0000, INEXACT),
            ("SQRT.S", cop1(s, 4, 0), down, 0x7f7f_ffff, 0, 0x5f7f_ffff, INEXACT),
            // 1.0673196e-23 to 3.2669859e-12
            ("SQRT.S", cop1(s, 4, 0), nearest, 0x194e_731c, 0, 0x2c65_e4c9, INEXACT),
            ("CVT.S.D", cop1(d, 32, 0), nearest, near_max, 0, 0x7f7f_ffff, INEXACT),
            ("CVT.S.D", cop1(d, 32, 0), zero, near_max, 0, 0x7f7f_ffff, INEXACT),
            ("CVT.S.D", cop1(d, 32, 0), up, near_max, 0, 0x7f80_0000, OVERFLOW | INEXACT),
            ("CVT.S.D", cop1(d, 32, 0), down, near_max, 0, 0x7f7f_ffff, INEXACT),
            ("CVT.S.D", cop1(d, 32, 0), nearest, min, 0, 0, ui),
            ("CVT.S.D", cop1(d, 32, 0), up, min, 0, smin, ui),
            ("CVT.S.D", cop1(d, 32, 0), down, min, 0, 0, ui),
            ("CVT.S.D", cop1(d, 32, 0), down, nmin, 0, snmin, ui),
            ("CVT.S.D", cop1(d, 32, 0), up, nmin, 0, 0x8000_0000, ui),
        ];
        let mut failed = Vec::new();
        for (name, word, mode, a, b, value, exceptions) in cases {
            let flush = if exceptions & UNDERFLOW != 0 { FLUSH } else { 0 };
            let got = run(&mut n64, word, flush | mode, a, b);
            if got != (value, flush | raised(exceptions, mode)) {
                failed.push(format!("{name} mode {mode} {a:#x} {b:#x}: {:#x} {:#x}", got.0, got.1));
            }
            // Where it underflows, FS clear or underflow enabled leaves it
            // to software; where it overflows, the overflow exception.
            let trap = |enables: u32| if exceptions & UNDERFLOW != 0 { UNIMPLEMENTED } else { exceptions & enables };
            let enabled = [(mode, 0), (flush | mode | UNDERFLOW << 7, UNDERFLOW), (flush | mode | 31 << 7, 31)];
            for (fcr31, enables) in enabled {
                if trap(enables) == 0 {
                    continue;
                }
                let got = run(&mut n64, word, fcr31, a, b);
                if got != (0x0123_4567_89ab_cdef, fcr31 | trap(enables) << 12) {
                    failed.push(format!("{name} {a:#x} {b:#x} fcr31 {fcr31:#x}: {:#x} {:#x}", got.0, got.1));
                }
            }
        }
        assert!(failed.is_empty(), "{failed:#?}");
    }

    /// Tininess after rounding: a product just below the smallest normal
    /// single that rounds (in 24 bits, the exponent unlimited) to the step
    /// below it is tiny, one that rounds up to it is not, though rounding
    /// into the denormals gives the smallest normal for both.
    #[test]
    fn tiny_after_rounding() {
        let mut n64 = console();
        let mul = cop1(16, 2, 2);
        // (1 - 3200·2^-24) · 2^-126 (1 + 3200·2^-24) = 2^-126 (1 - 0.61·2^-24)
        assert_eq!(run(&mut n64, mul, 0, 0x3f7f_f380, 0x0080_0640), (0x0123_4567_89ab_cdef, UNIMPLEMENTED << 12));
        assert_eq!(run(&mut n64, mul, FLUSH, 0x3f7f_f380, 0x0080_0640), (0, FLUSH | raised(UNDERFLOW | INEXACT, 0)));
        // (1 - 2^-23) · 2^-126 (1 + 2^-23) = 2^-126 (1 - 2^-46)
        assert_eq!(run(&mut n64, mul, 0, 0x3f7f_fffe, 0x0080_0001), (0x0080_0000, raised(INEXACT, 0)));
        // The double 2^-126 (1 - 0.75·2^-24) to a single.
        let cvt = cop1(17, 32, 0);
        assert_eq!(run(&mut n64, cvt, 0, 0x380f_ffff_e800_0000, 0), (0x0123_4567_89ab_cdef, UNIMPLEMENTED << 12));
        assert_eq!(run(&mut n64, cvt, 0, 0x380f_ffff_f800_0000, 0), (0x0080_0000, raised(INEXACT, 0)));
    }

    /// Longs to singles round once: as Rust's conversion to nearest (and
    /// toward zero as a truncation), also where a double's rounding would
    /// land on a single's halfway point.
    #[test]
    fn long_to_single() {
        let mut n64 = console();
        let cvt = cop1(21, 32, 0);
        let mut seed = 0x1234_5678_9abc_def0u64;
        let truncated = |v: i64| {
            let m = v.unsigned_abs();
            let drop = (64 - m.leading_zeros()).saturating_sub(24);
            let t = (m >> drop << drop) as f32;
            if v < 0 { -t } else { t }
        };
        for case in 0..20000 {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            let r = (seed as i64 >> 9) >> (seed & 31);
            let v = match case {
                0 => (1 << 54) + (1 << 30) + 1,
                1 => -(1 << 54) - (1 << 30) - 1,
                2 => (1 << 54) + (1 << 30) - 1,
                3 => (1 << 55) - 1,
                _ if case & 1 == 0 => r,
                // Near a single's halfway point: 24 bits, a one, then a few
                // low bits.
                _ => (((seed >> 40) as i64 | 1 << 23) << 31 | 1 << 30) + (r & 15) - 8,
            };
            let (bits, _) = run(&mut n64, cvt, 0, v as u64, 0);
            assert_eq!(bits as u32, (v as f32).to_bits(), "{v:#x} to nearest");
            let (bits, _) = run(&mut n64, cvt, 1, v as u64, 0);
            assert_eq!(bits as u32, truncated(v).to_bits(), "{v:#x} toward zero");
        }
    }

    /// The generators of oorandom 11.1, which n64-systemtest's randomized
    /// tests draw their operands from: PCG with 64 bits of state (32-bit
    /// output) and with 128 (64-bit output, its own output step).
    struct Pcg32 {
        state: u64,
        inc: u64,
    }

    impl Pcg32 {
        fn new(seed: u64) -> Pcg32 {
            let mut r = Pcg32 { state: 0, inc: 1_442_695_040_888_963_407u64 << 1 | 1 };
            r.next();
            r.state = r.state.wrapping_add(seed);
            r.next();
            r
        }

        fn next(&mut self) -> u32 {
            let old = self.state;
            self.state = old.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(self.inc);
            ((((old >> 18) ^ old) >> 27) as u32).rotate_right((old >> 59) as u32)
        }
    }

    struct Pcg64 {
        state: u128,
        inc: u128,
    }

    impl Pcg64 {
        fn new(seed: u128) -> Pcg64 {
            let mut r = Pcg64 { state: 0, inc: 0x2fe0_e169_ffbd_06e3_5bc3_07bd_4d2f_814f << 1 | 1 };
            r.next();
            r.state = r.state.wrapping_add(seed);
            r.next();
            r
        }

        fn next(&mut self) -> u64 {
            let old = self.state;
            self.state = old.wrapping_mul(47_026_247_687_942_121_848_144_207_491_837_523_525).wrapping_add(self.inc);
            ((((old >> 29) ^ old) >> 58) as u64).rotate_right((old >> 122) as u32)
        }
    }

    /// n64-systemtest's randomized FPU test (src/tests/cop1/randomized.rs):
    /// `iterations` times, FCR31 set to flush on odd iterations and to the
    /// rounding mode in bits 1-2 of the count, all exceptions off; fs (f0)
    /// and ft (f2) random bits; the result (f4, which keeps the last result
    /// where the instruction took the unimplemented exception: the test's
    /// handler goes on after it) and FCR31 hashed. The hashes it compares
    /// with were taken on a console.
    fn systemtest_hash(word: u32, double: bool, iterations: u32) -> u64 {
        let mut n64 = console();
        let (mut small, mut large) = (Pcg32::new(0), Pcg64::new(0));
        let instr = crate::decode::decode(word);
        let mut hash = 0u64;
        for i in 0..iterations {
            n64.cpu.cop0.status = crate::cop0::CU1 | FR;
            n64.cpu.fpu.fcr31 = (i & 1) << 24 | (i >> 1 & 3);
            let (a, b) = if double {
                (large.next(), large.next())
            } else {
                (u64::from(small.next()), u64::from(small.next()))
            };
            n64.cpu.fpu.regs[0] = a;
            n64.cpu.fpu.regs[2] = b;
            n64.fpu_instruction(instr);
            let fcr31 = n64.cpu.fpu.fcr31;
            let result = if double { n64.cpu.fpu.regs[4] } else { n64.cpu.fpu.regs[4] & 0xffff_ffff };
            hash = hash.wrapping_mul(397) ^ result;
            hash = hash.wrapping_mul(397) ^ u64::from(fcr31);
        }
        hash
    }

    /// The words of the instructions the randomized tests run: COP1 with
    /// fd f4, fs f0 and ft f2.
    fn cop1(fmt: u32, funct: u32, ft: u32) -> u32 {
        0x11 << 26 | fmt << 21 | ft << 16 | 4 << 6 | funct
    }

    /// n64-systemtest's quick randomized tests (2000 or 3000 operands), with
    /// the hashes it expects.
    #[test]
    fn systemtest_randomized() {
        let (s, d, w) = (16, 17, 20);
        let tests = [
            ("ADD.S", cop1(s, 0, 2), false, 3000, 0x341a_a5e3_310b_4ab0),
            ("ADD.D", cop1(d, 0, 2), true, 3000, 0xe888_e60d_c53b_5a03),
            ("SUB.S", cop1(s, 1, 2), false, 3000, 0xa56d_beaf_17cd_7173),
            ("SUB.D", cop1(d, 1, 2), true, 3000, 0x79d3_b74f_1bc3_ff24),
            ("MUL.S", cop1(s, 2, 2), false, 2000, 0x1707_88e9_b03e_2c0a),
            ("MUL.D", cop1(d, 2, 2), true, 2000, 0x15e6_d768_53f4_df59),
            ("DIV.S", cop1(s, 3, 2), false, 2000, 0x55c6_8396_00fd_0fa2),
            ("DIV.D", cop1(d, 3, 2), true, 2000, 0xa2ae_9ba9_a3fe_c554),
            ("SQRT.S", cop1(s, 4, 0), false, 2000, 0x0276_c42a_0e3a_11a1),
            ("SQRT.D", cop1(d, 4, 0), true, 2000, 0x6cb8_9862_81ef_ca80),
            ("CVT.S.W", cop1(w, 32, 0), false, 2000, 0x8ac0_9585_da53_7355),
            ("CVT.W.S", cop1(s, 36, 0), false, 2000, 0x41ca_0130_57f7_2659),
        ];
        check_hashes(&tests);
    }

    fn check_hashes(tests: &[(&str, u32, bool, u32, u64)]) {
        let mut failed = Vec::new();
        for &(name, word, double, iterations, expected) in tests {
            let hash = systemtest_hash(word, double, iterations);
            if hash != expected {
                failed.push(format!("{name}: {hash:#x}, expected {expected:#x}"));
            }
        }
        assert!(failed.is_empty(), "{failed:#?}");
    }

    /// n64-systemtest's stress tests: the same with 2 or 3 million operands
    /// (its default build does not run them; under a second here in a
    /// release build on a PC).
    #[test]
    #[ignore]
    fn systemtest_randomized_stress() {
        let (s, d, w) = (16, 17, 20);
        let tests = [
            ("ADD.S", cop1(s, 0, 2), false, 3_000_000, 0x464c_fdae_544a_9be0),
            ("ADD.D", cop1(d, 0, 2), true, 3_000_000, 0x25d2_61c1_4617_031e),
            ("SUB.S", cop1(s, 1, 2), false, 3_000_000, 0x5906_d66d_6ed3_5481),
            ("SUB.D", cop1(d, 1, 2), true, 3_000_000, 0xe6de_e76e_5196_222f),
            ("MUL.S", cop1(s, 2, 2), false, 2_000_000, 0xd6fc_bec0_926c_c113),
            ("MUL.D", cop1(d, 2, 2), true, 2_000_000, 0xe2c0_5aae_c9cc_1dc4),
            ("DIV.S", cop1(s, 3, 2), false, 2_000_000, 0x76ef_e398_daa1_0453),
            ("DIV.D", cop1(d, 3, 2), true, 2_000_000, 0xf64e_b203_f41f_e776),
            ("SQRT.S", cop1(s, 4, 0), false, 2_000_000, 0x4fde_4820_2eec_2625),
            ("SQRT.D", cop1(d, 4, 0), true, 2_000_000, 0x88ed_c4c0_aec1_2da4),
            ("CVT.S.W", cop1(w, 32, 0), false, 2_000_000, 0x1adc_1603_2524_5c23),
            ("CVT.W.S", cop1(s, 36, 0), false, 2_000_000, 0x1e1b_3c37_5a34_d773),
        ];
        check_hashes(&tests);
    }
}
