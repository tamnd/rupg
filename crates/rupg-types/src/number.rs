//! Integers and OIDs in the text format.
//!
//! The input functions are ports of `pg_strtoint16_safe`, `pg_strtoint32_safe` and `pg_strtoint64_safe` in `src/backend/utils/adt/numutils.c`, and of `uint32in_subr` for `oid`. The rules are not the same for the two: an integer takes `0x`, `0o` and `0b` and underscores between digits, and an `oid` takes what `strtoul` with base 0 takes, which reads a leading `0` as octal and takes a minus sign.
//!
//! Lifted from `crates/rudb-pgtypes/src/number.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

use crate::error::TypeError;

/// `isspace` in the C locale.
pub(crate) fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// The value of a digit in `base`, if it is one.
pub(crate) fn digit(b: u8, base: u64) -> Option<u64> {
    let value = match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        b'A'..=b'F' => b - b'A' + 10,
        _ => return None,
    };
    (u64::from(value) < base).then_some(u64::from(value))
}

/// Reads an integer whose minimum is `-limit`. PostgreSQL first tries plain decimal digits with an optional `-`, and an overflow there is an error at once, even when other bytes follow. Any other input goes to the slow path, which reads white space, a sign, a base prefix and underscores.
fn parse_int(s: &str, limit: u64, type_name: &str) -> Result<i128, TypeError> {
    let b = s.as_bytes();
    let at = |i: usize| b.get(i).copied().unwrap_or(0);
    let range = || TypeError::range(s, type_name);
    let syntax = || TypeError::syntax(type_name, s);
    let finish = |tmp: u64, neg: bool| match neg {
        true if tmp > limit => Err(range()),
        true => Ok(-i128::from(tmp)),
        false if tmp >= limit => Err(range()),
        false => Ok(i128::from(tmp)),
    };

    let mut i = usize::from(at(0) == b'-');
    if at(i).is_ascii_digit() {
        let mut tmp = 0u64;
        while at(i).is_ascii_digit() {
            if tmp > limit / 10 {
                return Err(range());
            }
            tmp = tmp * 10 + u64::from(at(i) - b'0');
            i += 1;
        }
        if i == b.len() {
            return finish(tmp, at(0) == b'-');
        }
    }

    i = 0;
    while is_space(at(i)) {
        i += 1;
    }
    let neg = at(i) == b'-';
    if neg || at(i) == b'+' {
        i += 1;
    }
    let base = match (at(i), at(i + 1)) {
        (b'0', b'x' | b'X') => 16,
        (b'0', b'o' | b'O') => 8,
        (b'0', b'b' | b'B') => 2,
        _ => 10,
    };
    if base != 10 {
        i += 2;
    }
    let first = i;
    let mut tmp = 0u64;
    loop {
        if let Some(d) = digit(at(i), base) {
            if tmp > limit / base {
                return Err(range());
            }
            tmp = tmp * base + d;
            i += 1;
        } else if at(i) == b'_' {
            // Only a decimal number cannot start with an underscore. An underscore must have a digit after it.
            if base == 10 && i == first {
                return Err(syntax());
            }
            i += 1;
            if digit(at(i), base).is_none() {
                return Err(syntax());
            }
        } else {
            break;
        }
    }
    if i == first {
        return Err(syntax());
    }
    while is_space(at(i)) {
        i += 1;
    }
    if i != b.len() {
        return Err(syntax());
    }
    finish(tmp, neg)
}

/// The text input of `int2`.
pub fn int2_in(s: &str) -> Result<i16, TypeError> {
    parse_int(s, 1 << 15, "smallint").map(|v| v as i16)
}

/// The text input of `int4`.
pub fn int4_in(s: &str) -> Result<i32, TypeError> {
    parse_int(s, 1 << 31, "integer").map(|v| v as i32)
}

/// The text input of `int8`.
pub fn int8_in(s: &str) -> Result<i64, TypeError> {
    parse_int(s, 1 << 63, "bigint").map(|v| v as i64)
}

/// What `strtoul(s, &end, 0)` gives with a 64-bit `unsigned long`: the value, the end of the number, which is 0 when there is no number, and whether the value overflowed.
fn strtoul(b: &[u8]) -> (u64, usize, bool) {
    let at = |i: usize| b.get(i).copied().unwrap_or(0);
    let mut i = 0;
    while is_space(at(i)) {
        i += 1;
    }
    let neg = at(i) == b'-';
    if neg || at(i) == b'+' {
        i += 1;
    }
    // `0x` is a prefix only when a hex digit follows it. Else the `0` is an octal number.
    let base = match (at(i), at(i + 1)) {
        (b'0', b'x' | b'X') if digit(at(i + 2), 16).is_some() => {
            i += 2;
            16
        }
        (b'0', _) => 8,
        _ => 10,
    };
    let first = i;
    let mut value = 0u64;
    let mut overflow = false;
    while let Some(d) = digit(at(i), base) {
        match value.checked_mul(base).and_then(|v| v.checked_add(d)) {
            Some(v) => value = v,
            None => overflow = true,
        }
        i += 1;
    }
    if i == first {
        return (0, 0, false);
    }
    match (overflow, neg) {
        (true, _) => (u64::MAX, i, true),
        (false, true) => (value.wrapping_neg(), i, false),
        (false, false) => (value, i, false),
    }
}

/// The text input of `oid`. A negative number from -2147483648 to -1 is the OID with the same bits, so `-1` is 4294967295.
pub fn oid_in(s: &str) -> Result<u32, TypeError> {
    uint32_in(s, "oid")
}

/// `uint32in_subr` for all of `s`: the text input of `oid`, `xid` and `cid`. The errors show `type_name`.
pub fn uint32_in(s: &str, type_name: &str) -> Result<u32, TypeError> {
    let b = s.as_bytes();
    let (value, mut end, overflow) = strtoul(b);
    if end == 0 {
        return Err(TypeError::syntax(type_name, s));
    }
    if overflow {
        return Err(TypeError::range(s, type_name));
    }
    while end < b.len() && is_space(b[end]) {
        end += 1;
    }
    if end != b.len() {
        return Err(TypeError::syntax(type_name, s));
    }
    let result = value as u32;
    if value != u64::from(result) && value != i64::from(result as i32) as u64 {
        return Err(TypeError::range(s, type_name));
    }
    Ok(result)
}

/// `uint32in_subr` with an end pointer: a number as `oid_in` reads it, at the start of `s`, and the length of its text. The caller reads what comes after it. An error shows all of `s`.
pub(crate) fn uint32_in_subr(s: &str, type_name: &str) -> Result<(u32, usize), TypeError> {
    let (value, end, overflow) = strtoul(s.as_bytes());
    if end == 0 {
        return Err(TypeError::syntax(type_name, s));
    }
    let result = value as u32;
    if overflow || (value != u64::from(result) && value != i64::from(result as i32) as u64) {
        return Err(TypeError::range(s, type_name));
    }
    Ok((result, end))
}

const PAIRS: &[u8; 200] = b"00010203040506070809101112131415161718192021222324252627282930313233343536373839404142434445464748495051525354555657585960616263646566676869707172737475767778798081828384858687888990919293949596979899";

/// Writes the decimal digits of `n`.
pub fn u64_out(mut n: u64, out: &mut Vec<u8>) {
    let mut buf = [0u8; 20];
    let mut at = buf.len();
    while n >= 100 {
        let pair = (n % 100) as usize * 2;
        n /= 100;
        at -= 2;
        buf[at..at + 2].copy_from_slice(&PAIRS[pair..pair + 2]);
    }
    if n >= 10 {
        let pair = n as usize * 2;
        at -= 2;
        buf[at..at + 2].copy_from_slice(&PAIRS[pair..pair + 2]);
    } else {
        at -= 1;
        buf[at] = b'0' + n as u8;
    }
    out.extend_from_slice(&buf[at..]);
}

/// The text output of `int2`, `int4` and `int8`: the digits with a `-` for a negative value.
pub fn int_out(n: i64, out: &mut Vec<u8>) {
    if n < 0 {
        out.push(b'-');
    }
    u64_out(n.unsigned_abs(), out);
}

/// The text output of `oid`, which is unsigned.
pub fn oid_out(n: u32, out: &mut Vec<u8>) {
    u64_out(u64::from(n), out);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(f: impl Fn(&mut Vec<u8>)) -> String {
        let mut out = Vec::new();
        f(&mut out);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn integers_take_the_forms_of_numutils() {
        for (input, value) in [
            ("0", 0),
            ("-0", 0),
            (" 12 ", 12),
            ("+7", 7),
            ("0x1F", 31),
            ("-0X1f", -31),
            ("0o17", 15),
            ("0b101", 5),
            ("1_000", 1000),
            ("0x_1F", 31),
            ("2147483647", i32::MAX),
            ("-2147483648", i32::MIN),
            ("-0x80000000", i32::MIN),
            ("\t\n42\r", 42),
        ] {
            assert_eq!(int4_in(input), Ok(value), "{input:?}");
        }
        for input in ["", " ", "1e3", "1.5", "_1", "1_", "1__0", "0x", "0b2", "- 1", "1 2", "0o8"] {
            let error = int4_in(input).unwrap_err();
            assert_eq!(error.sqlstate.as_str(), "22P02", "{input:?}");
            assert_eq!(
                error.message,
                format!("invalid input syntax for type integer: \"{input}\"")
            );
        }
        for input in ["2147483648", "-2147483649", "0x80000000", " 99999999999 ", "99999999999x"] {
            let error = int4_in(input).unwrap_err();
            assert_eq!(error.sqlstate.as_str(), "22003", "{input:?}");
            assert_eq!(
                error.message,
                format!("value \"{input}\" is out of range for type integer")
            );
        }
        assert_eq!(int2_in("-32768"), Ok(i16::MIN));
        assert_eq!(
            int2_in("32768").unwrap_err().message,
            "value \"32768\" is out of range for type smallint"
        );
        assert_eq!(int8_in("-9223372036854775808"), Ok(i64::MIN));
        assert_eq!(int8_in("0x7FFF_FFFF_FFFF_FFFF"), Ok(i64::MAX));
        assert_eq!(
            int8_in("x").unwrap_err().message,
            "invalid input syntax for type bigint: \"x\""
        );
    }

    #[test]
    fn an_oid_takes_what_strtoul_takes() {
        for (input, value) in [
            ("0", 0),
            ("16384", 16384),
            (" 010 ", 8),
            ("0x10", 16),
            ("-1", u32::MAX),
            ("-2147483648", 1 << 31),
            ("4294967295", u32::MAX),
            ("+5", 5),
        ] {
            assert_eq!(oid_in(input), Ok(value), "{input:?}");
        }
        for input in ["", "x", "0x", "08", "1_0", "1.0", "-"] {
            assert_eq!(oid_in(input).unwrap_err().sqlstate.as_str(), "22P02", "{input:?}");
        }
        for input in ["4294967296", "-2147483649", "99999999999999999999999"] {
            assert_eq!(
                oid_in(input).unwrap_err().message,
                format!("value \"{input}\" is out of range for type oid")
            );
        }
    }

    #[test]
    fn integers_print_as_digits() {
        for n in [0, 7, -7, 10, 99, 100, 12345, i64::MAX, i64::MIN, -1_000_000] {
            assert_eq!(text(|out| int_out(n, out)), n.to_string());
        }
        assert_eq!(text(|out| oid_out(u32::MAX, out)), "4294967295");
    }
}
