//! `bool`, `"char"`, `name`, `bytea` and `uuid` in the text format.
//!
//! Each function is a port of the input or output function of the type in `src/backend/utils/adt`: `boolin` and `parse_bool_with_len` in `bool.c`, `charin` and `charout` in `char.c`, `namein` in `name.c`, `byteain` and `byteaout` in `bytea.c` with `hex_decode` in `encode.c`, and `string_to_uuid` and `uuid_out` in `uuid.c`. The string types with a length and `json` are in their own modules.
//!
//! Lifted from `crates/rudb-pgtypes/src/scalar.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

use rupg_common::SqlState;

use crate::error::TypeError;
use crate::number::is_space;

/// The longest `name` in bytes, which is `NAMEDATALEN` less the terminator.
pub const NAME_MAX_BYTES: usize = 63;

/// The setting `bytea_output`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ByteaOutput {
    /// `\x` and two lowercase hex digits for each byte. The default.
    #[default]
    Hex,
    /// Printable ASCII as itself, a backslash as two, and any other byte as a backslash and three octal digits.
    Escape,
}

/// The text input of `bool`. It takes `true`, `yes`, `on` and `1` and the matching words for false, in any case, with white space around them, and any prefix of a word that is not ambiguous. `o` is ambiguous, so `on` and `off` need two letters.
pub fn bool_in(s: &str) -> Result<bool, TypeError> {
    let b = s.as_bytes();
    let start = b.iter().position(|&c| !is_space(c)).unwrap_or(b.len());
    let end = b.iter().rposition(|&c| !is_space(c)).map_or(start, |i| i + 1);
    let word = &b[start..end];
    let prefix = |of: &[u8], min: usize| {
        word.len() >= min && word.len() <= of.len() && word.eq_ignore_ascii_case(&of[..word.len()])
    };
    let value = match word.first() {
        Some(b't' | b'T') if prefix(b"true", 1) => Some(true),
        Some(b'f' | b'F') if prefix(b"false", 1) => Some(false),
        Some(b'y' | b'Y') if prefix(b"yes", 1) => Some(true),
        Some(b'n' | b'N') if prefix(b"no", 1) => Some(false),
        Some(b'o' | b'O') if prefix(b"on", 2) => Some(true),
        Some(b'o' | b'O') if prefix(b"off", 2) => Some(false),
        Some(b'1') if word.len() == 1 => Some(true),
        Some(b'0') if word.len() == 1 => Some(false),
        _ => None,
    };
    value.ok_or_else(|| TypeError::syntax("boolean", s))
}

/// The text output of `bool`.
pub fn bool_out(v: bool, out: &mut Vec<u8>) {
    out.push(if v { b't' } else { b'f' });
}

/// The text input of `"char"`: a backslash and three octal digits is that byte, else the first byte, and an empty string is 0. A character of more than one byte gives its first byte.
pub fn char_in(s: &str) -> u8 {
    match s.as_bytes() {
        [b'\\', a @ b'0'..=b'7', b @ b'0'..=b'7', c @ b'0'..=b'7'] => {
            ((a - b'0') << 6).wrapping_add((b - b'0') << 3).wrapping_add(c - b'0')
        }
        [first, ..] => *first,
        [] => 0,
    }
}

/// The text output of `"char"`: a byte above 127 is a backslash and three octal digits, and 0 is the empty string.
pub fn char_out(v: u8, out: &mut Vec<u8>) {
    if v >= 0x80 {
        out.extend_from_slice(&[b'\\', b'0' + (v >> 6), b'0' + ((v >> 3) & 7), b'0' + (v & 7)]);
    } else if v != 0 {
        out.push(v);
    }
}

/// The text input of `name`: the string cut to 63 bytes at the end of a character, with no error.
pub fn name_in(s: &str) -> &str {
    if s.len() <= NAME_MAX_BYTES {
        return s;
    }
    let mut end = NAME_MAX_BYTES;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// The text input of `bytea`. A string that starts with `\x` is hex, with white space allowed between the pairs of digits. Any other string is the escape format.
pub fn bytea_in(s: &str) -> Result<Vec<u8>, TypeError> {
    let b = s.as_bytes();
    if let Some(hex) = b.strip_prefix(b"\\x") {
        let mut out = Vec::with_capacity(hex.len() / 2);
        let digit = |i: usize| {
            (hex[i] as char).to_digit(16).map(|d| d as u8).ok_or_else(|| {
                // The text has the whole character at the bad byte.
                let rest = &s[s.len() - (hex.len() - i)..];
                let c = rest.chars().next().unwrap_or_default();
                TypeError::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    format!("invalid hexadecimal digit: \"{c}\""),
                )
            })
        };
        let mut i = 0;
        while i < hex.len() {
            if matches!(hex[i], b' ' | b'\n' | b'\t' | b'\r') {
                i += 1;
                continue;
            }
            let high = digit(i)?;
            if i + 1 >= hex.len() {
                return Err(TypeError::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    "invalid hexadecimal data: odd number of digits".to_string(),
                ));
            }
            out.push(high << 4 | digit(i + 1)?);
            i += 2;
        }
        return Ok(out);
    }

    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i..] {
            [b'\\', a @ b'0'..=b'3', c @ b'0'..=b'7', d @ b'0'..=b'7', ..] => {
                out.push((a - b'0') << 6 | (c - b'0') << 3 | (d - b'0'));
                i += 4;
            }
            [b'\\', b'\\', ..] => {
                out.push(b'\\');
                i += 2;
            }
            [b'\\', ..] => {
                return Err(TypeError::new(
                    SqlState::INVALID_TEXT_REPRESENTATION,
                    "invalid input syntax for type bytea".to_string(),
                ));
            }
            [c, ..] => {
                out.push(c);
                i += 1;
            }
            [] => break,
        }
    }
    Ok(out)
}

const HEX: &[u8; 16] = b"0123456789abcdef";

/// The text output of `bytea`.
pub fn bytea_out(v: &[u8], style: ByteaOutput, out: &mut Vec<u8>) {
    match style {
        ByteaOutput::Hex => {
            out.reserve(2 + v.len() * 2);
            out.extend_from_slice(b"\\x");
            for &byte in v {
                out.extend_from_slice(&[HEX[usize::from(byte >> 4)], HEX[usize::from(byte & 15)]]);
            }
        }
        ByteaOutput::Escape => {
            for &byte in v {
                match byte {
                    b'\\' => out.extend_from_slice(b"\\\\"),
                    0x20..=0x7e => out.push(byte),
                    _ => out.extend_from_slice(&[
                        b'\\',
                        b'0' + (byte >> 6),
                        b'0' + ((byte >> 3) & 7),
                        b'0' + (byte & 7),
                    ]),
                }
            }
        }
    }
}

/// The text input of `uuid`: 32 hex digits in any case, with an optional pair of braces around them and an optional hyphen after any group of four digits.
pub fn uuid_in(s: &str) -> Result<[u8; 16], TypeError> {
    let syntax = || TypeError::syntax("uuid", s);
    let b = s.as_bytes();
    let braces = b.first() == Some(&b'{');
    let mut i = usize::from(braces);
    let mut uuid = [0u8; 16];
    for (n, byte) in uuid.iter_mut().enumerate() {
        let pair = b.get(i..i + 2).ok_or_else(syntax)?;
        let high = (pair[0] as char).to_digit(16).ok_or_else(syntax)?;
        let low = (pair[1] as char).to_digit(16).ok_or_else(syntax)?;
        *byte = (high << 4 | low) as u8;
        i += 2;
        if b.get(i) == Some(&b'-') && n % 2 == 1 && n < 15 {
            i += 1;
        }
    }
    if braces {
        if b.get(i) != Some(&b'}') {
            return Err(syntax());
        }
        i += 1;
    }
    if i != b.len() {
        return Err(syntax());
    }
    Ok(uuid)
}

/// The text output of `uuid`: lowercase, in groups of 8, 4, 4, 4 and 12 digits.
pub fn uuid_out(v: &[u8; 16], out: &mut Vec<u8>) {
    for (i, &byte) in v.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            out.push(b'-');
        }
        out.extend_from_slice(&[HEX[usize::from(byte >> 4)], HEX[usize::from(byte & 15)]]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(f: impl Fn(&mut Vec<u8>)) -> Vec<u8> {
        let mut out = Vec::new();
        f(&mut out);
        out
    }

    #[test]
    fn a_boolean_takes_a_prefix_that_is_not_ambiguous() {
        for input in ["t", "TRUE", " tr ", "y", "yes", "on", "ON", "1", "\ttrue\n"] {
            assert_eq!(bool_in(input), Ok(true), "{input:?}");
        }
        for input in ["f", "fAlSe", "n", "no", "of", "off", "0"] {
            assert_eq!(bool_in(input), Ok(false), "{input:?}");
        }
        for input in ["", " ", "o", "truex", "yess", "onn", "01", "2", "t r"] {
            assert_eq!(
                bool_in(input).unwrap_err().message,
                format!("invalid input syntax for type boolean: \"{input}\""),
                "{input:?}"
            );
        }
        assert_eq!(text(|out| bool_out(true, out)), b"t");
    }

    #[test]
    fn a_char_is_one_byte_or_an_octal_escape() {
        assert_eq!(char_in("a"), b'a');
        assert_eq!(char_in("abc"), b'a');
        assert_eq!(char_in(""), 0);
        assert_eq!(char_in("\\101"), b'A');
        assert_eq!(char_in("\\377"), 0xff);
        assert_eq!(char_in("\\777"), 0xff);
        assert_eq!(char_in("\\18"), b'\\');
        assert_eq!(char_in("é"), 0xc3);
        assert_eq!(text(|out| char_out(0xc3, out)), b"\\303");
        assert_eq!(text(|out| char_out(b'r', out)), b"r");
        assert_eq!(text(|out| char_out(0, out)), b"");
    }

    #[test]
    fn a_name_is_cut_at_a_character() {
        let long = "a".repeat(70);
        assert_eq!(name_in(&long).len(), 63);
        let wide = format!("{}é", "a".repeat(62));
        assert_eq!(name_in(&wide), "a".repeat(62));
        assert_eq!(name_in("pg_class"), "pg_class");
    }

    #[test]
    fn bytea_reads_and_writes_both_formats() {
        assert_eq!(bytea_in("\\x0aFF"), Ok(vec![0x0a, 0xff]));
        assert_eq!(bytea_in("\\x 0a\n ff "), Ok(vec![0x0a, 0xff]));
        assert_eq!(bytea_in("\\x"), Ok(vec![]));
        assert_eq!(bytea_in("ab\\\\c\\001\\377"), Ok(b"ab\\c\x01\xff".to_vec()));
        // Only a lowercase `x` starts the hex format, so this is the escape format.
        assert!(bytea_in("\\X0a").is_err());
        let odd = bytea_in("\\x0").unwrap_err();
        assert_eq!(
            (odd.sqlstate.as_str(), odd.message.as_str()),
            ("22023", "invalid hexadecimal data: odd number of digits")
        );
        assert_eq!(bytea_in("\\x0g").unwrap_err().message, "invalid hexadecimal digit: \"g\"");
        assert_eq!(bytea_in("\\xé0").unwrap_err().message, "invalid hexadecimal digit: \"é\"");
        assert_eq!(bytea_in("\\x0 1").unwrap_err().message, "invalid hexadecimal digit: \" \"");
        for input in ["\\", "a\\4", "\\400", "\\12"] {
            let error = bytea_in(input).unwrap_err();
            assert_eq!(
                (error.sqlstate.as_str(), error.message.as_str()),
                ("22P02", "invalid input syntax for type bytea"),
                "{input:?}"
            );
        }
        let value = b"a\\\x00\x7f\xff ~";
        assert_eq!(text(|out| bytea_out(value, ByteaOutput::Hex, out)), b"\\x615c007fff207e");
        assert_eq!(
            text(|out| bytea_out(value, ByteaOutput::Escape, out)),
            b"a\\\\\\000\\177\\377 ~"
        );
    }

    #[test]
    fn a_uuid_takes_braces_and_hyphens_after_groups_of_four() {
        let uuid = [
            0xa0, 0xee, 0xbc, 0x99, 0x9c, 0x0b, 0x4e, 0xf8, 0xbb, 0x6d, 0x6b, 0xb9, 0xbd, 0x38,
            0x0a, 0x11,
        ];
        for input in [
            "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11",
            "A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11",
            "{a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11}",
            "a0eebc999c0b4ef8bb6d6bb9bd380a11",
            "a0ee-bc99-9c0b-4ef8-bb6d-6bb9-bd38-0a11",
            "{a0eebc99-9c0b4ef8-bb6d6bb9-bd380a11}",
        ] {
            assert_eq!(uuid_in(input), Ok(uuid), "{input:?}");
        }
        for input in [
            "",
            "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a1",
            "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a111",
            "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11-",
            "{a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11",
            "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11}",
            "a0e-ebc99-9c0b-4ef8-bb6d-6bb9bd380a11",
            "a0eebc99--9c0b-4ef8-bb6d-6bb9bd380a11",
            " a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11",
        ] {
            assert_eq!(
                uuid_in(input).unwrap_err().message,
                format!("invalid input syntax for type uuid: \"{input}\""),
                "{input:?}"
            );
        }
        assert_eq!(text(|out| uuid_out(&uuid, out)), b"a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11");
    }
}
