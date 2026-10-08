//! `float4` and `float8` in the text format.
//!
//! The output follows `float4out` and `float8out_internal` in `src/backend/utils/adt/float.c`. When `extra_float_digits` is more than 0, which is the default, the output is the shortest text that reads back to the same bits. PostgreSQL uses Ryu for the digits and its own layout in `to_chars` of `src/common/d2s.c` and `f2s.c`: fixed notation when the decimal exponent is from -4 to 14 for `float8` and from -4 to 5 for `float4`, else a mantissa, `e`, a sign and at least two digits. The standard library gives the same shortest digits with `{:e}`, with one difference. The standard library takes a text that is exactly halfway to the next value when the halfway text reads back to the value, but Ryu in PostgreSQL never takes a halfway text. So `1e23::float8` is `9.999999999999999e+22`. This module finds those texts with exact integer arithmetic and then takes the shortest text that is not halfway. When the value is exactly halfway between two shortest texts, Ryu takes the text with the even last digit and the standard library takes the text above, so this module takes the even text too. It puts the digits in the layout of PostgreSQL. When `extra_float_digits` is 0 or less, the output is `%.*g` with `DBL_DIG` or `FLT_DIG` plus `extra_float_digits` digits, as `pg_strfromd` makes it.
//!
//! The input follows `float8in_internal` and `float4in_internal`, which call `strtod` and `strtof`. So the input takes what the C library takes: decimal and hexadecimal numbers, `inf`, `infinity` and `nan` in any case, and `nan(...)`. A number that is too large, or a number that is not zero but reads as zero, is out of range.
//!
//! Lifted from `crates/rudb-pgtypes/src/float.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

use std::fmt::Write as _;

use rupg_common::SqlState;

use crate::error::TypeError;
use crate::number::is_space;

/// A small buffer on the stack for the standard formatter, so that the output of one value does not allocate.
struct Stack {
    buf: [u8; 64],
    len: usize,
}

impl Stack {
    fn new() -> Stack {
        Stack { buf: [0; 64], len: 0 }
    }

    fn bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

impl std::fmt::Write for Stack {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        let end = self.len + s.len();
        self.buf.get_mut(self.len..end).ok_or(std::fmt::Error)?.copy_from_slice(s.as_bytes());
        self.len = end;
        Ok(())
    }
}

/// Splits the output of `{:e}` into the digits with no point and the decimal exponent of the first digit.
fn split_exp(text: &[u8]) -> (&[u8], i32) {
    let e = text.iter().position(|&b| b == b'e').unwrap_or(text.len());
    let exp = std::str::from_utf8(&text[e + 1..]).ok().and_then(|t| t.parse().ok()).unwrap_or(0);
    (&text[..e], exp)
}

/// Writes `e`, the sign and at least two digits of the exponent.
fn exponent(exp: i32, out: &mut Vec<u8>) {
    out.push(b'e');
    out.push(if exp < 0 { b'-' } else { b'+' });
    let exp = exp.unsigned_abs();
    if exp < 10 {
        out.push(b'0');
    }
    crate::number::u64_out(u64::from(exp), out);
}

/// Writes the digits `d1 d2 ... dn` times 10 to the power `exp` in fixed notation, with no trailing zeros after the point and no point when there is no fraction.
fn fixed(digits: &[u8], exp: i32, out: &mut Vec<u8>) {
    let n = digits.len() as i32;
    if exp < 0 {
        out.extend_from_slice(b"0.");
        out.extend(std::iter::repeat_n(b'0', (-exp - 1) as usize));
        out.extend_from_slice(digits);
    } else if exp + 1 >= n {
        out.extend_from_slice(digits);
        out.extend(std::iter::repeat_n(b'0', (exp + 1 - n) as usize));
    } else {
        let point = (exp + 1) as usize;
        out.extend_from_slice(&digits[..point]);
        out.push(b'.');
        out.extend_from_slice(&digits[point..]);
    }
}

/// The digits of a `{:e}` mantissa with the point removed.
fn plain(mantissa: &[u8]) -> Vec<u8> {
    mantissa.iter().copied().filter(|&b| b != b'.').collect()
}

/// Writes the shortest text of a finite value that is not zero, in the layout of `to_chars`. `fixed_limit` is the first exponent that is not in fixed notation.
fn shortest(sci: &[u8], fixed_limit: i32, out: &mut Vec<u8>) {
    let (digits, exp) = split_exp(sci);
    if (-4..fixed_limit).contains(&exp) {
        fixed(&plain(digits), exp, out);
    } else {
        // The mantissa of `{:e}` is already `d.ddd`, or `d` with one digit.
        out.extend_from_slice(digits);
        exponent(exp, out);
    }
}

/// The facts of the binary format that the shortest text needs.
trait Shortest: Copy + PartialEq + std::fmt::LowerExp + std::str::FromStr {
    /// The most significant digits that a value needs to read back.
    const DIGITS: usize;
    /// The value is `m` times 2 to the power `e`. The third part is true when the next value below is nearer than the next value above, which is so at a power of 2 that is not the smallest normal value.
    fn parts(self) -> (u64, i32, bool);
}

impl Shortest for f64 {
    const DIGITS: usize = 17;
    fn parts(self) -> (u64, i32, bool) {
        let bits = self.to_bits();
        let (exp, frac) = ((bits >> 52) & 0x7ff, bits & ((1 << 52) - 1));
        match exp {
            0 => (frac, -1074, false),
            _ => (frac | 1 << 52, exp as i32 - 1075, frac == 0 && exp > 1),
        }
    }
}

impl Shortest for f32 {
    const DIGITS: usize = 9;
    fn parts(self) -> (u64, i32, bool) {
        let bits = self.to_bits();
        let (exp, frac) = ((bits >> 23) & 0xff, bits & ((1 << 23) - 1));
        match exp {
            0 => (u64::from(frac), -149, false),
            _ => (u64::from(frac | 1 << 23), exp as i32 - 150, frac == 0 && exp > 1),
        }
    }
}

/// True when `d` times 10 to the power `e10` is `c` times 2 to the power `e2`. The decimal is a sum of powers of 2 only when 5 to the power `-e10` divides `d`, and it has the factor 5 to the power `e10` that must divide `c`. Both `d` and `c` are less than 5 to the power 25, so the two are not equal when `e10` is outside -24 to 24, and in that range the products fit in `u128`.
fn same_value(d: u64, e10: i32, c: u64, e2: i32) -> bool {
    if !(-24..=24).contains(&e10) {
        return false;
    }
    let left = u128::from(d) * 5u128.pow(e10.max(0).unsigned_abs());
    let right = u128::from(c) * 5u128.pow(e10.min(0).unsigned_abs());
    // left * 2^(e10 - e2) == right
    let shift = e10 - e2;
    let (small, large, shift) =
        if shift >= 0 { (left, right, shift) } else { (right, left, -shift) };
    shift < 128 && large.trailing_zeros() >= shift.unsigned_abs() && large >> shift == small
}

/// True when `d` times 10 to the power `e10` is exactly halfway between `v` and the next value above or below.
fn halfway<F: Shortest>(v: F, d: u64, e10: i32) -> bool {
    let (m, e, nearer_below) = v.parts();
    let below = if nearer_below { (4 * m - 1, e - 2) } else { (2 * m - 1, e - 1) };
    same_value(d, e10, 2 * m + 1, e - 1) || same_value(d, e10, below.0, below.1)
}

/// The digits of a `{:e}` text as an integer, and the decimal exponent of the last digit.
fn decimal(sci: &[u8]) -> (u64, i32) {
    let (digits, exp) = split_exp(sci);
    let digits = plain(digits);
    let d = digits.iter().fold(0u64, |d, &b| d * 10 + u64::from(b - b'0'));
    (d, exp + 1 - digits.len() as i32)
}

/// The `{:e}` text of `d` times 10 to the power `e10`, with no trailing zeros.
fn sci_text(mut d: u64, mut e10: i32) -> Stack {
    while d.is_multiple_of(10) && d != 0 {
        d /= 10;
        e10 += 1;
    }
    let mut digits = Stack::new();
    let _ = write!(digits, "{d}");
    let digits = digits.bytes();
    let mut sci = Stack::new();
    let _ = sci.write_str(std::str::from_utf8(&digits[..1]).unwrap_or("0"));
    if digits.len() > 1 {
        let _ = sci.write_str(".");
        let _ = sci.write_str(std::str::from_utf8(&digits[1..]).unwrap_or(""));
    }
    let _ = write!(sci, "e{}", e10 + digits.len() as i32 - 1);
    sci
}

/// True when the text `d` times 10 to the power `e10` reads back to `v` and is not halfway.
fn inside<F: Shortest>(v: F, d: u64, e10: i32) -> bool {
    let mut text = Stack::new();
    let _ = write!(text, "{d}e{e10}");
    let reads_back = std::str::from_utf8(text.bytes()).ok().and_then(|t| t.parse::<F>().ok());
    reads_back == Some(v) && !halfway(v, d, e10)
}

/// The `{:e}` text of the shortest digits of a positive finite value, as Ryu in PostgreSQL gives them.
fn shortest_sci<F: Shortest>(v: F) -> Stack {
    let mut sci = Stack::new();
    let _ = write!(sci, "{v:e}");
    let (d, e10) = decimal(sci.bytes());
    if !halfway(v, d, e10) {
        return even_tie(v, d, e10).map_or(sci, |d| sci_text(d, e10));
    }
    let len = split_exp(sci.bytes()).0.iter().filter(|&&b| b != b'.').count();
    for len in len + 1..=F::DIGITS {
        let mut near = Stack::new();
        let _ = write!(near, "{:.*e}", len - 1, v);
        let (d, e10) = decimal(near.bytes());
        // The nearest text of this length can be halfway, and then the text on the other side of the value can be inside.
        for d in [d, d + 1, d - 1] {
            if inside(v, d, e10) {
                return sci_text(even_tie(v, d, e10).unwrap_or(d), e10);
            }
        }
    }
    sci
}

/// When the value is exactly halfway between the odd digits `d` and the next digits, Ryu takes the even digits, but the standard library takes the digits above. Gives the even digits when they read back to the value.
fn even_tie<F: Shortest>(v: F, d: u64, e10: i32) -> Option<u64> {
    if d.is_multiple_of(2) {
        return None;
    }
    let (m, e, _) = v.parts();
    [(d - 1, 2 * d - 1), (d + 1, 2 * d + 1)]
        .into_iter()
        .find(|&(even, twice)| same_value(twice * 5, e10 - 1, m, e) && inside(v, even, e10))
        .map(|(even, _)| even)
}

/// Writes `%.*g` of a finite value that is not zero, with `precision` significant digits.
fn general(sci: &[u8], precision: i32, out: &mut Vec<u8>) {
    let (digits, exp) = split_exp(sci);
    let mut digits = plain(digits);
    while digits.len() > 1 && digits.last() == Some(&b'0') {
        digits.pop();
    }
    if exp < -4 || exp >= precision {
        out.push(digits[0]);
        if digits.len() > 1 {
            out.push(b'.');
            out.extend_from_slice(&digits[1..]);
        }
        exponent(exp, out);
    } else {
        fixed(&digits, exp, out);
    }
}

/// Writes the sign and the special values. Returns `true` when the value is done.
fn special(nan: bool, inf: bool, zero: bool, negative: bool, out: &mut Vec<u8>) -> bool {
    if nan {
        out.extend_from_slice(b"NaN");
        return true;
    }
    if negative {
        out.push(b'-');
    }
    if inf {
        out.extend_from_slice(b"Infinity");
    } else if zero {
        out.push(b'0');
    }
    inf || zero
}

/// The text output of `float8`.
pub fn float8_out(v: f64, extra_float_digits: i32, out: &mut Vec<u8>) {
    if special(v.is_nan(), v.is_infinite(), v == 0.0, v.is_sign_negative(), out) {
        return;
    }
    if extra_float_digits > 0 {
        shortest(shortest_sci(v.abs()).bytes(), 15, out);
        return;
    }
    let precision = (15 + extra_float_digits).clamp(1, 32);
    let mut sci = Stack::new();
    let _ = write!(sci, "{:.*e}", precision as usize - 1, v.abs());
    general(sci.bytes(), precision, out);
}

/// The text output of `float4`.
pub fn float4_out(v: f32, extra_float_digits: i32, out: &mut Vec<u8>) {
    if special(v.is_nan(), v.is_infinite(), v == 0.0, v.is_sign_negative(), out) {
        return;
    }
    if extra_float_digits > 0 {
        shortest(shortest_sci(v.abs()).bytes(), 6, out);
        return;
    }
    // `float4out` gives the value to `pg_strfromd` as a double.
    let precision = (6 + extra_float_digits).clamp(1, 32);
    let mut sci = Stack::new();
    let _ = write!(sci, "{:.*e}", precision as usize - 1, f64::from(v.abs()));
    general(sci.bytes(), precision, out);
}

/// The two float types for the input, with the facts of the IEEE format that a hexadecimal number needs.
pub(crate) trait Float: Copy + std::str::FromStr {
    /// The significant bits, with the hidden bit.
    const BITS: u32;
    /// The exponent of the last bit of the smallest subnormal value.
    const MIN_EXP: i64;
    /// The largest exponent of the first bit.
    const MAX_EXP: i64;
    const INFINITY: Self;
    const NAN: Self;
    fn from_parts(mantissa: u64, exp: i64) -> Self;
    fn negate(self) -> Self;
    fn is_zero(self) -> bool;
    fn is_infinite(self) -> bool;
}

impl Float for f64 {
    const BITS: u32 = 53;
    const MIN_EXP: i64 = -1074;
    const MAX_EXP: i64 = 1023;
    const INFINITY: f64 = f64::INFINITY;
    const NAN: f64 = f64::NAN;
    fn from_parts(mantissa: u64, exp: i64) -> f64 {
        if mantissa >> 52 == 0 {
            return f64::from_bits(mantissa);
        }
        f64::from_bits(((exp + 52 + 1023) as u64) << 52 | (mantissa & ((1 << 52) - 1)))
    }
    fn negate(self) -> f64 {
        -self
    }
    fn is_zero(self) -> bool {
        self == 0.0
    }
    fn is_infinite(self) -> bool {
        f64::is_infinite(self)
    }
}

impl Float for f32 {
    const BITS: u32 = 24;
    const MIN_EXP: i64 = -149;
    const MAX_EXP: i64 = 127;
    const INFINITY: f32 = f32::INFINITY;
    const NAN: f32 = f32::NAN;
    fn from_parts(mantissa: u64, exp: i64) -> f32 {
        if mantissa >> 23 == 0 {
            return f32::from_bits(mantissa as u32);
        }
        f32::from_bits(((exp + 23 + 127) as u32) << 23 | (mantissa as u32 & ((1 << 23) - 1)))
    }
    fn negate(self) -> f32 {
        -self
    }
    fn is_zero(self) -> bool {
        self == 0.0
    }
    fn is_infinite(self) -> bool {
        f32::is_infinite(self)
    }
}

/// `mantissa` times 2 to the power `exp`, rounded to the nearest value with ties to even. `sticky` says that bits below the mantissa were not zero.
fn from_binary<F: Float>(mantissa: u64, sticky: bool, exp: i64) -> F {
    if mantissa == 0 {
        return F::from_parts(0, 0);
    }
    let bits = i64::from(64 - mantissa.leading_zeros());
    let mut e = (exp + bits - i64::from(F::BITS)).max(F::MIN_EXP);
    let shift = e - exp;
    let mut m = if shift <= 0 {
        mantissa << -shift
    } else if shift > 64 {
        0
    } else {
        let kept = if shift == 64 { 0 } else { mantissa >> shift };
        let lost = if shift == 64 { mantissa } else { mantissa & ((1 << shift) - 1) };
        let half = 1u64 << (shift - 1);
        let up = lost > half || (lost == half && (sticky || kept & 1 == 1));
        kept + u64::from(up)
    };
    if m == 1 << F::BITS {
        m >>= 1;
        e += 1;
    }
    if e + i64::from(F::BITS) - 1 > F::MAX_EXP {
        return F::INFINITY;
    }
    F::from_parts(m, e)
}

/// Reads a number as `strtod` does. Returns the value, the end of the number, and whether a mantissa that is not zero gave zero or a finite number gave infinity, which is `ERANGE` with a value that PostgreSQL refuses. Returns `None` when there is no number at the start.
pub(crate) fn strtod<F: Float>(b: &[u8]) -> Option<(F, usize, bool)> {
    let at = |i: usize| b.get(i).copied().unwrap_or(0);
    let starts = |i: usize, word: &[u8]| {
        b.get(i..i + word.len()).is_some_and(|s| s.eq_ignore_ascii_case(word))
    };
    let neg = at(0) == b'-';
    let mut i = usize::from(neg || at(0) == b'+');
    let sign = |v: F| if neg { v.negate() } else { v };

    if starts(i, b"inf") {
        let end = if starts(i, b"infinity") { i + 8 } else { i + 3 };
        return Some((sign(F::INFINITY), end, false));
    }
    if starts(i, b"nan") {
        let mut end = i + 3;
        if at(end) == b'(' {
            let mut j = end + 1;
            while at(j).is_ascii_alphanumeric() || at(j) == b'_' {
                j += 1;
            }
            if at(j) == b')' {
                end = j + 1;
            }
        }
        return Some((sign(F::NAN), end, false));
    }

    if at(i) == b'0' && matches!(at(i + 1), b'x' | b'X') {
        let start = i + 2;
        let mut j = start;
        let (mut m, mut sticky, mut exp, mut taken, mut any) = (0u64, false, 0i64, 0, false);
        let mut point = false;
        loop {
            let c = at(j);
            if c == b'.' && !point {
                point = true;
            } else if let Some(d) = (c as char).to_digit(16) {
                any = true;
                if m == 0 && d == 0 {
                    // A leading zero adds nothing, but after the point it moves the value.
                    exp -= if point { 4 } else { 0 };
                } else if taken < 16 {
                    m = m << 4 | u64::from(d);
                    taken += 1;
                    exp -= if point { 4 } else { 0 };
                } else {
                    sticky |= d != 0;
                    exp += if point { 0 } else { 4 };
                }
            } else {
                break;
            }
            j += 1;
        }
        if any {
            if matches!(at(j), b'p' | b'P') {
                let mut k = j + 1;
                let negative = at(k) == b'-';
                k += usize::from(negative || at(k) == b'+');
                if at(k).is_ascii_digit() {
                    let mut power = 0i64;
                    while at(k).is_ascii_digit() {
                        power = (power * 10 + i64::from(at(k) - b'0')).min(1 << 40);
                        k += 1;
                    }
                    exp += if negative { -power } else { power };
                    j = k;
                }
            }
            let value: F = from_binary(m, sticky, exp);
            let range = value.is_infinite() || (value.is_zero() && m != 0);
            return Some((sign(value), j, range));
        }
        // `0x` with no digit is the number 0 and the `x` is not part of it.
        return Some((sign(F::from_parts(0, 0)), i + 1, false));
    }

    let start = i;
    let mut nonzero = false;
    let mut digits = 0;
    while at(i).is_ascii_digit() {
        nonzero |= at(i) != b'0';
        digits += 1;
        i += 1;
    }
    if at(i) == b'.' {
        i += 1;
        while at(i).is_ascii_digit() {
            nonzero |= at(i) != b'0';
            digits += 1;
            i += 1;
        }
    }
    if digits == 0 {
        return None;
    }
    if matches!(at(i), b'e' | b'E') {
        let mut k = i + 1;
        k += usize::from(matches!(at(k), b'+' | b'-'));
        if at(k).is_ascii_digit() {
            while at(k).is_ascii_digit() {
                k += 1;
            }
            i = k;
        }
    }
    let text = std::str::from_utf8(&b[start..i]).ok()?;
    let value: F = text.parse().ok()?;
    let range = value.is_infinite() || (value.is_zero() && nonzero);
    Some((sign(value), i, range))
}

fn float_in<F: Float>(s: &str, type_name: &str) -> Result<F, TypeError> {
    let b = s.as_bytes();
    let mut num = 0;
    while num < b.len() && is_space(b[num]) {
        num += 1;
    }
    let syntax = || TypeError::syntax(type_name, s);
    let Some((value, len, range)) = strtod::<F>(&b[num..]) else { return Err(syntax()) };
    if range {
        return Err(TypeError::new(
            SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
            format!("\"{}\" is out of range for type {type_name}", &s[num..num + len]),
        ));
    }
    let mut end = num + len;
    while end < b.len() && is_space(b[end]) {
        end += 1;
    }
    if end != b.len() {
        return Err(syntax());
    }
    Ok(value)
}

/// The text input of `float8`.
pub fn float8_in(s: &str) -> Result<f64, TypeError> {
    float_in(s, "double precision")
}

/// The text input of `float4`.
pub fn float4_in(s: &str) -> Result<f32, TypeError> {
    float_in(s, "real")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out8(v: f64, digits: i32) -> String {
        let mut out = Vec::new();
        float8_out(v, digits, &mut out);
        String::from_utf8(out).unwrap()
    }

    fn out4(v: f32, digits: i32) -> String {
        let mut out = Vec::new();
        float4_out(v, digits, &mut out);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn the_shortest_text_has_the_layout_of_ryu_in_postgres() {
        for (v, text) in [
            (1.0, "1"),
            (-1.5, "-1.5"),
            (0.1, "0.1"),
            (1e-4, "0.0001"),
            (1e-5, "1e-05"),
            (1.5e-5, "1.5e-05"),
            (1e14, "100000000000000"),
            (1e15, "1e+15"),
            (123456789012345680.0, "1.2345678901234568e+17"),
            (1e100, "1e+100"),
            (5e-324, "5e-324"),
            (f64::MAX, "1.7976931348623157e+308"),
            (0.0, "0"),
            (-0.0, "-0"),
            (f64::NAN, "NaN"),
            (f64::INFINITY, "Infinity"),
            (f64::NEG_INFINITY, "-Infinity"),
        ] {
            assert_eq!(out8(v, 1), text, "{v:e}");
        }
        for (v, text) in [
            (1.0f32, "1"),
            (0.1, "0.1"),
            (123456.0, "123456"),
            (1234567.0, "1.234567e+06"),
            (1e-5, "1e-05"),
            (f32::MAX, "3.4028235e+38"),
            (-0.0, "-0"),
        ] {
            assert_eq!(out4(v, 1), text, "{v:e}");
        }
    }

    /// A shorter text that is exactly halfway to the next value reads back to the value, but Ryu in PostgreSQL does not take it. The values are from the oracle.
    #[test]
    fn the_shortest_text_is_not_halfway() {
        for (v, text) in [
            (1e23, "9.999999999999999e+22"),
            (9.5e21, "9.500000000000001e+21"),
            (2e22, "2e+22"),
            (18014398509481992.0, "1.8014398509481992e+16"),
            (9007199254740996.0, "9.007199254740996e+15"),
        ] {
            assert_eq!(out8(v, 1), text, "{v:e}");
        }
        for (v, text) in [
            (42483247.0f32, "4.2483248e+07"),
            (84966494.0, "8.4966496e+07"),
            (16777218.0, "1.6777218e+07"),
            (33554436.0, "3.3554436e+07"),
            (16777216.0, "1.6777216e+07"),
            (33554432.0, "3.3554432e+07"),
            (1e10, "1e+10"),
            (1.1754944e-38, "1.1754944e-38"),
            (-12_890_453.0 / 4.0, "-3.2226132e+06"),
            (-4_179_433.0 / 8.0, "-522429.12"),
            (-529_305.0 / 16.0, "-33081.562"),
        ] {
            assert_eq!(out4(v, 2), text, "{v:e}");
        }
    }

    #[test]
    fn fewer_digits_use_the_g_format() {
        assert_eq!(out8(0.1, 0), "0.1");
        assert_eq!(out8(1.0 / 3.0, 0), "0.333333333333333");
        assert_eq!(out8(1e15, 0), "1e+15");
        assert_eq!(out8(123456789012345.0, 0), "123456789012345");
        assert_eq!(out8(1e-5, 0), "1e-05");
        assert_eq!(out8(2.5, -14), "2");
        assert_eq!(out8(-1234.5, -15), "-1e+03");
        assert_eq!(out8(-0.0, 0), "-0");
        assert_eq!(out4(0.1, 0), "0.1");
        assert_eq!(out4(1.0 / 3.0, 0), "0.333333");
        assert_eq!(out4(1234567.0, 0), "1.23457e+06");
    }

    #[test]
    fn the_input_takes_what_strtod_takes() {
        for (input, value) in [
            ("1", 1.0),
            (" -1.5e3 ", -1500.0),
            (".5", 0.5),
            ("5.", 5.0),
            ("1e", f64::NAN),
            ("0x1p3", 8.0),
            ("0x.8", 0.5),
            ("-0X1.8P-1", -0.75),
            ("0x1.fffffffffffff8p1023", f64::NAN),
            ("Infinity", f64::INFINITY),
            ("-inf", f64::NEG_INFINITY),
            ("+INFINITY", f64::INFINITY),
            ("1e-310", 1e-310),
            ("0e999999", 0.0),
        ] {
            let got = float8_in(input);
            if value.is_nan() {
                assert!(got.is_err(), "{input:?}");
            } else {
                assert_eq!(got, Ok(value), "{input:?}");
            }
        }
        assert!(float8_in("nan").unwrap().is_nan());
        assert!(float8_in("NaN(123)").unwrap().is_nan());
        assert_eq!(float8_in("0x1.fffffffffffffp1023"), Ok(f64::MAX));
        assert_eq!(float8_in("0x1p-1074"), Ok(5e-324));
        assert_eq!(float8_in("0x1.8p-1074"), Ok(1e-323));
        for input in ["", " ", "x", "1.5x", "infinit", "nan(", "--1", "1e5e"] {
            let error = float8_in(input).unwrap_err();
            assert_eq!(error.sqlstate.as_str(), "22P02", "{input:?}");
            assert_eq!(
                error.message,
                format!("invalid input syntax for type double precision: \"{input}\"")
            );
        }
        for (input, number) in
            [(" 1e400 ", "1e400"), ("-1e-400", "-1e-400"), ("0x1p1024", "0x1p1024")]
        {
            let error = float8_in(input).unwrap_err();
            assert_eq!(error.sqlstate.as_str(), "22003");
            assert_eq!(
                error.message,
                format!("\"{number}\" is out of range for type double precision")
            );
        }
        assert_eq!(
            float4_in("1e39").unwrap_err().message,
            "\"1e39\" is out of range for type real"
        );
        assert_eq!(float4_in("1e-40"), Ok(1e-40));
        assert_eq!(float4_in("3.4028235e38"), Ok(f32::MAX));
        assert_eq!(
            float4_in("x").unwrap_err().message,
            "invalid input syntax for type real: \"x\""
        );
    }
}
