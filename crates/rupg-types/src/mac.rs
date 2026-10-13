//! `macaddr` and `macaddr8`, in the text formats of `src/backend/utils/adt/mac.c` and `mac8.c`. A value is the bytes of the address in a `u64`, with the first byte as the most significant byte, so that the order of the numbers is the order of the addresses.

use rupg_common::SqlState;

use crate::binary::Recv;
use crate::error::TypeError;
use crate::number::is_space;

/// A part of a format of `sscanf` in `macaddr_in`.
#[derive(Clone, Copy)]
enum Part {
    /// `%x` with a width, or with no width for 0.
    Hex(usize),
    /// A character that must be next in the text.
    Char(u8),
}

use Part::{Char, Hex};

/// The formats that `macaddr_in` tries, in its order.
const FORMATS: [[Part; 11]; 7] = [
    [
        Hex(0),
        Char(b':'),
        Hex(0),
        Char(b':'),
        Hex(0),
        Char(b':'),
        Hex(0),
        Char(b':'),
        Hex(0),
        Char(b':'),
        Hex(0),
    ],
    [
        Hex(0),
        Char(b'-'),
        Hex(0),
        Char(b'-'),
        Hex(0),
        Char(b'-'),
        Hex(0),
        Char(b'-'),
        Hex(0),
        Char(b'-'),
        Hex(0),
    ],
    [
        Hex(2),
        Hex(2),
        Hex(2),
        Char(b':'),
        Hex(2),
        Hex(2),
        Hex(2),
        Char(0),
        Char(0),
        Char(0),
        Char(0),
    ],
    [
        Hex(2),
        Hex(2),
        Hex(2),
        Char(b'-'),
        Hex(2),
        Hex(2),
        Hex(2),
        Char(0),
        Char(0),
        Char(0),
        Char(0),
    ],
    [
        Hex(2),
        Hex(2),
        Char(b'.'),
        Hex(2),
        Hex(2),
        Char(b'.'),
        Hex(2),
        Hex(2),
        Char(0),
        Char(0),
        Char(0),
    ],
    [
        Hex(2),
        Hex(2),
        Char(b'-'),
        Hex(2),
        Hex(2),
        Char(b'-'),
        Hex(2),
        Hex(2),
        Char(0),
        Char(0),
        Char(0),
    ],
    [Hex(2), Hex(2), Hex(2), Hex(2), Hex(2), Hex(2), Char(0), Char(0), Char(0), Char(0), Char(0)],
];

/// `%x` of glibc `sscanf` at `b[*i..]`: white space, a sign, a `0x` prefix and hex digits, with no more than `width` characters after the white space when `width` is not 0. The result is the bits of the `unsigned int` that `%x` stores, as the `int` of `macaddr_in`, or `None` when there is no digit.
fn scan_hex(b: &[u8], i: &mut usize, width: usize) -> Option<i32> {
    let at = |i: usize| b.get(i).copied().unwrap_or(0);
    while is_space(at(*i)) {
        *i += 1;
    }
    let mut left = if width == 0 { usize::MAX } else { width };
    let negative = at(*i) == b'-';
    if negative || at(*i) == b'+' {
        *i += 1;
        left -= 1;
    }
    let mut digits = 0;
    let mut value = 0u64;
    let mut overflow = false;
    if left != 0 && at(*i) == b'0' {
        *i += 1;
        left -= 1;
        digits += 1;
        if left != 0 && matches!(at(*i), b'x' | b'X') {
            *i += 1;
            left -= 1;
        }
    }
    while left != 0 && at(*i).is_ascii_hexdigit() {
        let digit = u64::from(char::from(at(*i)).to_digit(16).unwrap_or(0));
        match value.checked_mul(16).and_then(|v| v.checked_add(digit)) {
            Some(v) => value = v,
            None => overflow = true,
        }
        *i += 1;
        left -= 1;
        digits += 1;
    }
    if digits == 0 {
        return None;
    }
    // `strtoul` gives `ULONG_MAX` for an overflow and negates the value for a minus sign. `%x` keeps the low 32 bits.
    let value = if overflow {
        u64::MAX
    } else if negative {
        value.wrapping_neg()
    } else {
        value
    };
    u32::try_from(value & u64::from(u32::MAX)).ok().map(u32::cast_signed)
}

/// The six numbers of `s` in `format`, or `None` when `s` does not match it. White space can follow the last number.
fn scan(s: &[u8], format: &[Part]) -> Option<[i32; 6]> {
    let mut out = [0; 6];
    let mut count = 0;
    let mut i = 0;
    for part in format {
        match *part {
            Hex(width) => {
                out[count] = scan_hex(s, &mut i, width)?;
                count += 1;
            }
            Char(0) => break,
            Char(c) => {
                if s.get(i) != Some(&c) {
                    return None;
                }
                i += 1;
            }
        }
    }
    while i < s.len() && is_space(s[i]) {
        i += 1;
    }
    (i == s.len()).then_some(out)
}

/// `macaddr_in`: six numbers in hex with one of the formats of `sscanf` in `mac.c`, such as `08:00:2b:01:02:03`, `08002b-010203` or `0800.2b01.0203`.
///
/// # Errors
///
/// `22P02` for text in no format, and `22003` for a number that is not in the range 0 to 255.
pub fn macaddr_in(s: &str) -> Result<u64, TypeError> {
    let numbers = FORMATS
        .iter()
        .find_map(|format| scan(s.as_bytes(), format))
        .ok_or_else(|| TypeError::syntax("macaddr", s))?;
    let mut value = 0u64;
    for n in numbers {
        let byte = u8::try_from(n).map_err(|_| {
            TypeError::new(
                SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
                format!("invalid octet value in \"macaddr\" value: \"{s}\""),
            )
        })?;
        value = value << 8 | u64::from(byte);
    }
    Ok(value)
}

/// The bytes of an address, from the first.
fn bytes(value: u64, count: usize) -> impl Iterator<Item = u8> {
    value.to_be_bytes().into_iter().skip(8 - count)
}

/// The bytes in hex with a `:` between them.
fn hex_out(value: u64, count: usize, out: &mut Vec<u8>) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for (i, byte) in bytes(value, count).enumerate() {
        if i > 0 {
            out.push(b':');
        }
        out.push(HEX[usize::from(byte >> 4)]);
        out.push(HEX[usize::from(byte & 15)]);
    }
}

/// `macaddr_out`: `08:00:2b:01:02:03`.
pub fn macaddr_out(value: u64, out: &mut Vec<u8>) {
    hex_out(value, 6, out);
}

/// `macaddr8_out`: `08:00:2b:01:02:03:04:05`.
pub fn macaddr8_out(value: u64, out: &mut Vec<u8>) {
    hex_out(value, 8, out);
}

/// `macaddr8_in`: 6 or 8 bytes of 2 hex digits, with the same `:`, `-` or `.` between them or with nothing between them. White space can come before and after the address. A 6-byte address gets `ff:fe` as the 4th and 5th bytes.
///
/// # Errors
///
/// `22P02` for text that is not an address.
pub fn macaddr8_in(s: &str) -> Result<u64, TypeError> {
    let fail = || TypeError::syntax("macaddr8", s);
    let b = s.as_bytes();
    let at = |i: usize| b.get(i).copied().unwrap_or(0);
    let mut i = 0;
    while is_space(at(i)) {
        i += 1;
    }
    let mut bytes = [0u8; 8];
    let mut count = 0;
    let mut spacer = 0;
    while at(i) != 0 && at(i + 1) != 0 {
        if count == 8 {
            return Err(fail());
        }
        let digit = |c: u8| char::from(c).to_digit(16).and_then(|d| u8::try_from(d).ok());
        let (Some(high), Some(low)) = (digit(at(i)), digit(at(i + 1))) else { return Err(fail()) };
        bytes[count] = high << 4 | low;
        count += 1;
        i += 2;
        if matches!(at(i), b':' | b'-' | b'.') {
            if spacer == 0 {
                spacer = at(i);
            } else if spacer != at(i) {
                return Err(fail());
            }
            i += 1;
        }
        if (count == 6 || count == 8) && is_space(at(i)) {
            while is_space(at(i)) {
                i += 1;
            }
            if at(i) != 0 {
                return Err(fail());
            }
        }
    }
    let bytes = match count {
        6 => [bytes[0], bytes[1], bytes[2], 0xff, 0xfe, bytes[3], bytes[4], bytes[5]],
        8 => bytes,
        _ => return Err(fail()),
    };
    Ok(u64::from_be_bytes(bytes))
}

/// `macaddr_recv`: the 6 bytes.
///
/// # Errors
///
/// `08P01` when the value has less than 6 bytes.
pub fn macaddr_recv(recv: &mut Recv<'_>) -> Result<u64, TypeError> {
    let mut value = 0;
    for _ in 0..6 {
        value = value << 8 | u64::from(recv.byte()?);
    }
    Ok(value)
}

/// `macaddr8_recv`: the 8 bytes, or 6 bytes that get `ff:fe` as the 4th and 5th bytes.
///
/// # Errors
///
/// `08P01` when the value has less than 6 bytes, or 7 bytes.
pub fn macaddr8_recv(recv: &mut Recv<'_>) -> Result<u64, TypeError> {
    if recv.remaining() == 6 {
        return macaddr_recv(recv).map(macaddr_to_macaddr8);
    }
    let mut value = 0;
    for _ in 0..8 {
        value = value << 8 | u64::from(recv.byte()?);
    }
    Ok(value)
}

/// `macaddrtomacaddr8`: `ff:fe` as the 4th and 5th bytes.
#[must_use]
pub fn macaddr_to_macaddr8(value: u64) -> u64 {
    (value >> 24) << 40 | 0xfffe << 24 | value & 0xff_ffff
}

/// `macaddr8tomacaddr`: the address without the 4th and 5th bytes, or `None` when they are not `ff:fe`.
#[must_use]
pub fn macaddr8_to_macaddr(value: u64) -> Option<u64> {
    (value >> 24 & 0xffff == 0xfffe).then_some((value >> 40) << 24 | value & 0xff_ffff)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(f: fn(u64, &mut Vec<u8>), value: u64) -> String {
        let mut out = Vec::new();
        f(value, &mut out);
        String::from_utf8(out).unwrap()
    }

    /// The values and the errors of PostgreSQL 19 for the same text.
    #[test]
    fn text_forms() {
        for input in [
            "08:00:2b:01:02:03",
            "08-00-2b-01-02-03",
            "08002b:010203",
            "08002b-010203",
            "0800.2b01.0203",
            "0800-2b01-0203",
            "08002b010203",
            " 08:00:2b:01:02:03 ",
            "8:0:2b:1:2:3",
        ] {
            assert_eq!(
                text(macaddr_out, macaddr_in(input).unwrap()),
                "08:00:2b:01:02:03",
                "{input}"
            );
        }
        assert_eq!(
            text(macaddr_out, macaddr_in("AB:CD:EF:01:02:03").unwrap()),
            "ab:cd:ef:01:02:03"
        );
        for input in
            ["08:00:2b:01:02", "08:00:2b:01:02:03:04", "08:00:2b:01:02:0g", "08:00-2b:01:02:03", ""]
        {
            let error = macaddr_in(input).unwrap_err();
            assert_eq!(error.sqlstate.as_str(), "22P02", "{input}");
        }
        let error = macaddr_in("08:00:2b:01:02:100").unwrap_err();
        assert_eq!(error.sqlstate.as_str(), "22003");
        assert_eq!(
            error.message,
            "invalid octet value in \"macaddr\" value: \"08:00:2b:01:02:100\""
        );

        let mac8 = |s: &str| text(macaddr8_out, macaddr8_in(s).unwrap());
        assert_eq!(mac8("08:00:2b:01:02:03"), "08:00:2b:ff:fe:01:02:03");
        for input in [
            "08:00:2b:01:02:03:04:05",
            "08-00-2b-01-02-03-04-05",
            "08002b0102030405",
            "0800.2b01.0203.0405",
            "08002b:0102030405",
            "08:00:2b:01:02:03:04:05 ",
            "0800:2b01:0203:0405",
        ] {
            assert_eq!(mac8(input), "08:00:2b:01:02:03:04:05", "{input}");
        }
        for input in ["08:00:2b:01:02:03:04", "08:00:2b:01:02:03:04:05:06", "x", "08:00:2b:01:02:3"]
        {
            assert_eq!(
                macaddr8_in(input).unwrap_err().message,
                format!("invalid input syntax for type macaddr8: \"{input}\"")
            );
        }
    }

    #[test]
    fn binary_forms() {
        let six = [8, 0, 0x2b, 1, 2, 3];
        assert_eq!(macaddr_recv(&mut Recv::new(&six)).unwrap(), 0x0800_2b01_0203);
        assert_eq!(macaddr8_recv(&mut Recv::new(&six)).unwrap(), 0x0800_2bff_fe01_0203);
        let eight = [8, 0, 0x2b, 1, 2, 3, 4, 5];
        assert_eq!(macaddr8_recv(&mut Recv::new(&eight)).unwrap(), 0x0800_2b01_0203_0405);
        assert!(macaddr_recv(&mut Recv::new(&six[..5])).is_err());
    }

    #[test]
    fn casts() {
        let six = macaddr_in("08:00:2b:01:02:03").unwrap();
        assert_eq!(text(macaddr8_out, macaddr_to_macaddr8(six)), "08:00:2b:ff:fe:01:02:03");
        assert_eq!(macaddr8_to_macaddr(macaddr_to_macaddr8(six)), Some(six));
        assert_eq!(macaddr8_to_macaddr(macaddr8_in("08:00:2b:01:02:03:04:05").unwrap()), None);
    }
}
