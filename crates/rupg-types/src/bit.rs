//! `bit` and `bit varying`, in the text and binary formats of `src/backend/utils/adt/varbit.c`. A value is a string of `0` and `1`, one character for each bit, so that the text form is the value and the order of the strings is the order of `bitcmp`.

use rupg_common::SqlState;

use crate::binary::Recv;
use crate::error::TypeError;

/// `VARBITMAXLEN`: the most bits in a value.
pub const BIT_MAX_LEN: usize = 2_147_483_640;

/// The type name in the errors of a length.
fn bit_type(varying: bool, typmod: i32) -> String {
    if varying { format!("bit varying({typmod})") } else { format!("bit({typmod})") }
}

/// `22026` for `bit` or `22001` for `bit varying`: the length does not agree with the typmod.
fn length_error(len: usize, typmod: i32, varying: bool) -> TypeError {
    if varying {
        TypeError::new(
            SqlState::STRING_DATA_RIGHT_TRUNCATION,
            format!("bit string too long for type {}", bit_type(true, typmod)),
        )
    } else {
        TypeError::new(
            SqlState::STRING_DATA_LENGTH_MISMATCH,
            format!("bit string length {len} does not match type {}", bit_type(false, typmod)),
        )
    }
}

/// The check of a length against a typmod: `bit(n)` takes exactly `n` bits and `bit varying(n)` takes no more than `n` bits. A typmod of 0 or less has no limit.
fn check_length(len: usize, typmod: i32, varying: bool) -> Result<(), TypeError> {
    let Ok(limit) = usize::try_from(typmod) else { return Ok(()) };
    if limit == 0 || len == limit || varying && len < limit {
        return Ok(());
    }
    Err(length_error(len, typmod, varying))
}

/// The character at byte `at` of `s`, for an error.
fn char_at(s: &str, at: usize) -> &str {
    let len = s[at..].chars().next().map_or(0, char::len_utf8);
    &s[at..at + len]
}

/// `bit_in` and `varbit_in`: the bits after a `B`, the hex digits after an `X`, or the bits with no prefix. The length check comes before the check of the digits, and the length of a value with no prefix is its length in bytes.
///
/// # Errors
///
/// `22P02` for a digit that is not valid, `22026` for a `bit(n)` value that does not have `n` bits, `22001` for a `bit varying(n)` value with more than `n` bits, and `54000` for a hex value that is too long.
pub fn bit_in(s: &str, typmod: i32, varying: bool) -> Result<String, TypeError> {
    let (hex, digits) = match s.as_bytes().first() {
        Some(b'b' | b'B') => (false, &s[1..]),
        Some(b'x' | b'X') => (true, &s[1..]),
        _ => (false, s),
    };
    let start = s.len() - digits.len();
    let len = if hex {
        if digits.len() > BIT_MAX_LEN / 4 {
            return Err(TypeError::new(
                SqlState::PROGRAM_LIMIT_EXCEEDED,
                format!("bit string length exceeds the maximum allowed ({BIT_MAX_LEN})"),
            ));
        }
        digits.len() * 4
    } else {
        digits.len()
    };
    check_length(len, typmod, varying)?;
    let mut out = String::with_capacity(len);
    for (i, c) in digits.bytes().enumerate() {
        if hex {
            let Some(v) = char::from(c).to_digit(16).filter(|_| c.is_ascii()) else {
                return Err(TypeError::new(
                    SqlState::INVALID_TEXT_REPRESENTATION,
                    format!("\"{}\" is not a valid hexadecimal digit", char_at(s, start + i)),
                ));
            };
            out.push_str(&format!("{v:04b}"));
        } else if c == b'0' || c == b'1' {
            out.push(char::from(c));
        } else {
            return Err(TypeError::new(
                SqlState::INVALID_TEXT_REPRESENTATION,
                format!("\"{}\" is not a valid binary digit", char_at(s, start + i)),
            ));
        }
    }
    Ok(out)
}

/// `bit` and `varbit`, the length casts: an explicit cast pads `bit(n)` with zeros or cuts it to `n` bits and cuts `bit varying(n)` to `n` bits. Other casts give the error of the input.
///
/// # Errors
///
/// `22026` or `22001` for a value that does not fit in a cast that is not explicit.
pub fn bit_coerce(
    bits: &str,
    typmod: i32,
    varying: bool,
    explicit: bool,
) -> Result<String, TypeError> {
    if check_length(bits.len(), typmod, varying).is_ok() {
        return Ok(bits.to_string());
    }
    if !explicit {
        return Err(length_error(bits.len(), typmod, varying));
    }
    let len = usize::try_from(typmod).unwrap_or(0);
    let mut out: String = bits.chars().take(len).collect();
    while out.len() < len {
        out.push('0');
    }
    Ok(out)
}

/// `bit_recv` and `varbit_recv`: the number of bits as an `int4`, then the bits in bytes, with the last byte padded with zero bits.
///
/// # Errors
///
/// `22P03` for a length that is not valid, the errors of [`bit_in`] for a length that does not agree with the typmod, and `08P01` for a value with too few bytes.
pub fn bit_recv(recv: &mut Recv<'_>, typmod: i32, varying: bool) -> Result<String, TypeError> {
    let len = recv.i32()?;
    let Some(len) = usize::try_from(len).ok().filter(|&len| len <= BIT_MAX_LEN) else {
        return Err(TypeError::new(
            SqlState::INVALID_BINARY_REPRESENTATION,
            "invalid length in external bit string".to_string(),
        ));
    };
    check_length(len, typmod, varying)?;
    let bytes = recv.bytes(len.div_ceil(8))?;
    Ok((0..len).map(|i| if (bytes[i / 8] >> (7 - i % 8)) & 1 == 1 { '1' } else { '0' }).collect())
}

/// The bits in bytes, from the high bit of the first byte, with the last byte padded with zero bits.
#[must_use]
pub fn bit_bytes(bits: &str) -> Vec<u8> {
    let mut out = vec![0u8; bits.len().div_ceil(8)];
    for (i, c) in bits.bytes().enumerate() {
        if c == b'1' {
            out[i / 8] |= 0x80 >> (i % 8);
        }
    }
    out
}

/// `bit_send` and `varbit_send`.
pub fn bit_send(bits: &str, out: &mut Vec<u8>) {
    let len = i32::try_from(bits.len()).unwrap_or(i32::MAX);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(&bit_bytes(bits));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The values and the errors of PostgreSQL 19 for the same text.
    #[test]
    fn text_forms() {
        assert_eq!(bit_in("1011", -1, false).unwrap(), "1011");
        assert_eq!(bit_in("", -1, false).unwrap(), "");
        assert_eq!(bit_in("x1f", -1, true).unwrap(), "00011111");
        assert_eq!(bit_in("X1F", 8, false).unwrap(), "00011111");
        assert_eq!(bit_in("b101", -1, true).unwrap(), "101");
        assert_eq!(bit_in("XABC", -1, false).unwrap(), "101010111100");
        assert_eq!(
            bit_in("102", -1, true).unwrap_err().message,
            "\"2\" is not a valid binary digit"
        );
        assert_eq!(
            bit_in("x1g", -1, true).unwrap_err().message,
            "\"g\" is not a valid hexadecimal digit"
        );
        assert_eq!(bit_in("é", -1, true).unwrap_err().message, "\"é\" is not a valid binary digit");
        let error = bit_in("101", 4, false).unwrap_err();
        assert_eq!(
            (error.sqlstate.as_str(), error.message.as_str()),
            ("22026", "bit string length 3 does not match type bit(4)")
        );
        let error = bit_in("1010", 3, true).unwrap_err();
        assert_eq!(
            (error.sqlstate.as_str(), error.message.as_str()),
            ("22001", "bit string too long for type bit varying(3)")
        );
        assert_eq!(bit_in("1010", 5, true).unwrap(), "1010");
    }

    #[test]
    fn length_casts() {
        assert_eq!(bit_coerce("101", 5, false, true).unwrap(), "10100");
        assert_eq!(bit_coerce("10101", 3, false, true).unwrap(), "101");
        assert_eq!(bit_coerce("101", 2, true, true).unwrap(), "10");
        assert_eq!(bit_coerce("101", 5, true, true).unwrap(), "101");
        assert!(bit_coerce("101", 5, false, false).is_err());
        assert_eq!(bit_coerce("101", -1, false, false).unwrap(), "101");
    }

    #[test]
    fn binary_forms() {
        let mut out = Vec::new();
        bit_send("1010101010", &mut out);
        assert_eq!(out, [0, 0, 0, 10, 0xaa, 0x80]);
        assert_eq!(bit_recv(&mut Recv::new(&out), -1, true).unwrap(), "1010101010");
        let mut out = Vec::new();
        bit_send("", &mut out);
        assert_eq!(out, [0, 0, 0, 0]);
        assert!(bit_recv(&mut Recv::new(&[0xff, 0xff, 0xff, 0xff]), -1, true).is_err());
    }
}
