//! PICO-8's numbers: signed 16.16 fixed point in an `i32`, the integer part
//! from -32768 to 32767 and sixteen bits of fraction. Everything wraps
//! around, as on PICO-8 (32767 + 1 is -32768), except division, which
//! saturates. The rules follow z8lua, the Lua PICO-8 itself is built on.

pub const ONE: i32 = 1 << 16;

/// The number with this integer value, wrapping into -32768..32767.
pub const fn from_int(i: i32) -> i32 {
    i.wrapping_shl(16)
}

/// The integer part, rounded down (flr).
pub const fn floor(n: i32) -> i32 {
    n >> 16
}

pub fn from_f64(d: f64) -> i32 {
    let x = d * 65536.0;
    // Within range by an i32 cast, which 32-bit ARM does in hardware;
    // beyond it through i64 (saturating, NaN 0), wrapping as PICO-8 does.
    if x.abs() < 2_147_483_648.0 { x as i32 } else { x as i64 as i32 }
}

pub fn to_f64(n: i32) -> f64 {
    f64::from(n) / 65536.0
}

pub fn mul(a: i32, b: i32) -> i32 {
    ((i64::from(a) * i64::from(b)) >> 16) as i32
}

/// a / b: past the range, or by zero, the largest number of the result's
/// sign (0x7fff.ffff, or 0x8000.0001 as PICO-8 gives for minus infinity).
///
/// The quotient is (a << 16) / b rounded towards zero. 32-bit ARM has no
/// 64-bit division but divides doubles in hardware: the double's quotient
/// is within one of the exact one, which a multiplication then corrects.
pub fn div(a: i32, b: i32) -> i32 {
    if b == ONE {
        return a;
    }
    if b > 0 && b & (b - 1) == 0 {
        // By a power of two (x/8, x/0.5): a shift, rounded towards zero.
        let k = b.trailing_zeros();
        if k >= 16 {
            let s = k - 16;
            let bias = if a < 0 { (1i32 << s) - 1 } else { 0 };
            return (a + bias) >> s;
        }
        let s = 16 - k;
        let q = a.wrapping_shl(s);
        // -32768 itself is past the range, as for any other divisor.
        if q >> s == a && q != i32::MIN {
            return q;
        }
        return if a >= 0 { 0x7fff_ffff } else { -0x7fff_ffff };
    }
    if b != 0 {
        let n = i64::from(a) << 16;
        let qf = (f64::from(a) * 65536.0) / f64::from(b);
        if qf.abs() < 2_147_483_000.0 {
            let mut q = qf as i32;
            let r = n - i64::from(q) * i64::from(b);
            let toward = if (n < 0) == (b < 0) { 1 } else { -1 };
            if r != 0 && (r < 0) != (n < 0) {
                // One past the truncated quotient, away from zero.
                q -= toward;
            } else if r.unsigned_abs() >= u64::from(b.unsigned_abs()) {
                q += toward;
            }
            return q;
        }
        let q = n / i64::from(b);
        if q.unsigned_abs() <= 0x7fff_ffff {
            return q as i32;
        }
    }
    if (a ^ b) >= 0 { 0x7fff_ffff } else { -0x7fff_ffff }
}

/// a \ b: the quotient rounded down.
pub fn idiv(a: i32, b: i32) -> i32 {
    div(a, b) & !0xffff
}

/// a % b, never negative: PICO-8 ignores the divisor's sign.
pub fn modulo(a: i32, b: i32) -> i32 {
    match b {
        0 => 0,
        // By a power of two (x%8, x%0.5): a mask, never negative.
        _ if b > 0 && b & (b - 1) == 0 => a & (b - 1),
        // |b| = 0x8000.0000 is no i32.
        i32::MIN => i64::from(a).rem_euclid(1 << 31) as i32,
        _ => a.rem_euclid(b.abs()),
    }
}

pub fn pow(a: i32, b: i32) -> i32 {
    // Whole powers by multiplication, as a game squaring a distance
    // expects the same bits as x*x.
    if b & 0xffff == 0 && (0..=16).contains(&floor(b)) {
        let mut r = ONE;
        for _ in 0..floor(b) {
            r = mul(r, a);
        }
        return r;
    }
    from_f64(to_f64(a).powf(to_f64(b)))
}

/// The shift amount: the integer part of the number.
fn amount(b: i32) -> i32 {
    floor(b)
}

pub fn shl(a: i32, b: i32) -> i32 {
    match amount(b) {
        n if n < 0 => shr(a, from_int(-n)),
        n if n >= 32 => 0,
        n => a << n,
    }
}

pub fn shr(a: i32, b: i32) -> i32 {
    match amount(b) {
        n if n < 0 => shl(a, from_int(-n)),
        n if n >= 32 => a >> 31,
        n => a >> n,
    }
}

pub fn lshr(a: i32, b: i32) -> i32 {
    match amount(b) {
        n if n < 0 => shl(a, from_int(-n)),
        n if n >= 32 => 0,
        n => ((a as u32) >> n) as i32,
    }
}

pub fn rotl(a: i32, b: i32) -> i32 {
    a.rotate_left((amount(b) & 31) as u32)
}

pub fn rotr(a: i32, b: i32) -> i32 {
    a.rotate_right((amount(b) & 31) as u32)
}

/// As PICO-8 prints a number: up to four decimals, without trailing zeros.
pub fn to_string(n: i32) -> String {
    if n & 0xffff == 0 {
        return floor(n).to_string();
    }
    let mut s = format!("{:.4}", to_f64(n));
    while s.ends_with('0') {
        s.pop();
    }
    if s.ends_with('.') {
        s.pop();
    }
    s
}

/// tostr(n, true): 0xiiii.ffff.
pub fn to_hex(n: i32) -> String {
    format!("0x{:04x}.{:04x}", (n >> 16) as u16, n as u16)
}

/// A number written in a string or the code: decimal, 0x hexadecimal or 0b
/// binary, each with a fraction, an optional sign before it. None for
/// anything else.
pub fn parse(text: &[u8]) -> Option<i32> {
    let mut s = text;
    while let [b' ' | b'\t' | b'\n' | b'\r', rest @ ..] = s {
        s = rest;
    }
    while let [rest @ .., b' ' | b'\t' | b'\n' | b'\r'] = s {
        s = rest;
    }
    let negative = s.first() == Some(&b'-');
    if negative || s.first() == Some(&b'+') {
        s = &s[1..];
    }
    let n = parse_unsigned(s)?;
    Some(if negative { n.wrapping_neg() } else { n })
}

fn parse_unsigned(s: &[u8]) -> Option<i32> {
    let radix: u32 = match s {
        [b'0', b'x' | b'X', ..] => 16,
        [b'0', b'b' | b'B', ..] => 2,
        _ => 10,
    };
    if radix != 10 {
        let digits = &s[2..];
        let (int, frac) = match digits.iter().position(|&c| c == b'.') {
            Some(p) => (&digits[..p], &digits[p + 1..]),
            None => (digits, &digits[..0]),
        };
        if int.is_empty() && frac.is_empty() {
            return None;
        }
        let mut whole: u32 = 0;
        for &c in int {
            whole = whole.wrapping_mul(radix).wrapping_add((c as char).to_digit(radix)?);
        }
        let bits_per_digit = if radix == 16 { 4 } else { 1 };
        let mut fraction: u32 = 0;
        let mut shift = 16i32;
        for &c in frac {
            let d = (c as char).to_digit(radix)?;
            shift -= bits_per_digit;
            if shift >= 0 {
                fraction |= d << shift;
            } else if shift > -(bits_per_digit) {
                fraction |= d >> -shift;
            }
        }
        return Some((whole << 16 | fraction) as i32);
    }
    // Decimal: digits, a point, digits; PICO-8 has no exponents.
    let mut end = 0;
    let mut digits = 0;
    while end < s.len() && s[end].is_ascii_digit() {
        end += 1;
        digits += 1;
    }
    if end < s.len() && s[end] == b'.' {
        end += 1;
        while end < s.len() && s[end].is_ascii_digit() {
            end += 1;
            digits += 1;
        }
    }
    if digits == 0 {
        return None;
    }
    if end != s.len() {
        return None;
    }
    let d: f64 = std::str::from_utf8(s).ok()?.parse().ok()?;
    Some(from_f64(d))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(d: f64) -> i32 {
        from_f64(d)
    }

    #[test]
    fn arithmetic_wraps_and_division_saturates() {
        assert_eq!(from_int(32767).wrapping_add(ONE), from_int(-32768));
        assert_eq!(mul(n(1.5), n(-2.0)), n(-3.0));
        assert_eq!(div(ONE, 0), 0x7fff_ffff);
        assert_eq!(div(-ONE, 0), -0x7fff_ffff);
        assert_eq!(div(n(7.0), n(2.0)), n(3.5));
        assert_eq!(idiv(n(-7.0), n(2.0)), n(-4.0));
        assert_eq!(modulo(n(-5.0), n(3.0)), n(1.0));
        assert_eq!(modulo(n(5.0), n(-3.0)), n(2.0));
        assert_eq!(modulo(n(5.5), n(2.0)), n(1.5));
        assert_eq!(pow(n(3.0), n(2.0)), n(9.0));
    }

    /// The reference: 64-bit integer division, as before.
    fn div_ref(a: i32, b: i32) -> i32 {
        if b == ONE {
            return a;
        }
        if b != 0 {
            let q = (i64::from(a) << 16) / i64::from(b);
            if q.unsigned_abs() <= 0x7fff_ffff {
                return q as i32;
            }
        }
        if (a ^ b) >= 0 { 0x7fff_ffff } else { -0x7fff_ffff }
    }

    #[test]
    fn division_and_modulo_match_64_bit_integer_arithmetic() {
        let mut s = 0x2545_f491u32;
        let mut next = || {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            s as i32
        };
        let edges = [0, 1, -1, 2, -2, ONE, -ONE, 3, 7, 0x7fff_ffff, i32::MIN, i32::MIN + 1, 0x8000, 0x10000 * 3, 0x1999];
        for &a in &edges {
            for &b in &edges {
                assert_eq!(div(a, b), div_ref(a, b), "{a:#x} / {b:#x}");
                if b != 0 {
                    assert_eq!(modulo(a, b), i64::from(a).rem_euclid(i64::from(b).abs()) as i32, "{a:#x} % {b:#x}");
                }
            }
        }
        for i in 0..3_000_000 {
            let (a, b) = (next(), next());
            // Small divisors and dividends as often as large ones.
            let a = if i % 3 == 0 { a >> (i % 24) } else { a };
            let b = if i % 2 == 0 { b >> (i % 30) } else { b };
            assert_eq!(div(a, b), div_ref(a, b), "{a:#x} / {b:#x}");
            if b != 0 {
                assert_eq!(modulo(a, b), i64::from(a).rem_euclid(i64::from(b).abs()) as i32, "{a:#x} % {b:#x}");
            }
        }
        // Powers of two, both ways.
        for k in 0..31 {
            let b = 1i32 << k;
            for &a in &edges {
                for d in [-3, -1, 0, 1, 5] {
                    let a = a.wrapping_add(d);
                    assert_eq!(div(a, b), div_ref(a, b), "{a:#x} / {b:#x}");
                    assert_eq!(div(a, -b), div_ref(a, -b), "{a:#x} / -{b:#x}");
                    assert_eq!(modulo(a, b), i64::from(a).rem_euclid(i64::from(b)) as i32, "{a:#x} % {b:#x}");
                }
            }
            for _ in 0..20_000 {
                let a = next();
                assert_eq!(div(a, b), div_ref(a, b), "{a:#x} / {b:#x}");
                assert_eq!(modulo(a, b), i64::from(a).rem_euclid(i64::from(b)) as i32, "{a:#x} % {b:#x}");
            }
        }
        // Quotients that are whole numbers, and one short of them.
        for q in [1, 7, 100, 32767, -32768, -5] {
            for b in [3, 7, 0x1999, 0x10001, 12345] {
                let a = ((i64::from(q) * i64::from(b)) >> 16) as i32;
                for d in -2..=2 {
                    assert_eq!(div(a + d, b), div_ref(a + d, b));
                }
            }
        }
    }

    #[test]
    fn shifts_follow_the_integer_part() {
        assert_eq!(shl(ONE, from_int(4)), from_int(16));
        assert_eq!(shr(from_int(-16), from_int(2)), from_int(-4));
        assert_eq!(lshr(-1, from_int(28)), 0xf);
        assert_eq!(shr(from_int(-1), from_int(40)), -1);
        assert_eq!(rotl(0x8000_0000u32 as i32, ONE), 1);
    }

    #[test]
    fn printing_and_reading_numbers() {
        assert_eq!(to_string(n(1.5)), "1.5");
        assert_eq!(to_string(from_int(-3)), "-3");
        assert_eq!(to_string(div(ONE, from_int(3))), "0.3333");
        assert_eq!(to_hex(n(-1.5)), "0xfffe.8000");
        assert_eq!(parse(b"0x1.8"), Some(n(1.5)));
        assert_eq!(parse(b"0b101.1"), Some(n(5.5)));
        assert_eq!(parse(b" -12 "), Some(from_int(-12)));
        assert_eq!(parse(b"0.1"), Some(0x1999));
        assert_eq!(parse(b"32768"), Some(from_int(-32768)));
        assert_eq!(parse(b"0x.0001"), Some(1));
        assert_eq!(parse(b"abc"), None);
        assert_eq!(parse(b""), None);
        assert_eq!(parse(b"1e2"), None);
    }
}
