//! `encode` and `decode` from `encode.c`: the text forms `hex`, `base64`, `base64url`, `base32hex` and `escape` of a `bytea` value.

use rupg_common::{Error, Result, SqlState};
use rupg_types::Value;

use crate::{Call, Kernel, bad_value};

/// The kernel of `encode` or `decode` by its `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    Some(match src {
        "binary_encode" => binary_encode,
        "binary_decode" => binary_decode,
        _ => return None,
    })
}

/// A text form of `bytea`.
#[derive(Clone, Copy)]
enum Form {
    Hex,
    Base64 { url: bool },
    Base32Hex,
    Escape,
}

/// `pg_find_encoding`: the form by its name, which does not depend on case.
fn form(name: &str) -> Result<Form> {
    Ok(match name.to_ascii_lowercase().as_str() {
        "hex" => Form::Hex,
        "base64" => Form::Base64 { url: false },
        "base64url" => Form::Base64 { url: true },
        "base32hex" => Form::Base32Hex,
        "escape" => Form::Escape,
        _ => {
            return Err(Error::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!("unrecognized encoding: \"{name}\""),
            )
            .with_hint(
                "Valid encodings are \"base32hex\", \"base64\", \"base64url\", \"escape\", and \"hex\".",
            ));
        }
    })
}

/// `binary_encode`: the text form of a `bytea` value.
fn binary_encode(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (Some(Value::Bytea(data)), Some(Value::Text(name))) = (args.first(), args.get(1)) else {
        return Err(bad_value());
    };
    let text = match form(name)? {
        Form::Hex => hex_encode(data),
        Form::Base64 { url } => base64_encode(data, url),
        Form::Base32Hex => base32hex_encode(data),
        Form::Escape => escape_encode(data),
    };
    Ok(Value::Text(text))
}

/// `binary_decode`: the `bytea` value of a text form.
fn binary_decode(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (Some(Value::Text(data)), Some(Value::Text(name))) = (args.first(), args.get(1)) else {
        return Err(bad_value());
    };
    let bytes = match form(name)? {
        Form::Hex => hex_decode(data)?,
        Form::Base64 { url } => base64_decode(data, url)?,
        Form::Base32Hex => base32hex_decode(data)?,
        Form::Escape => escape_decode(data)?,
    };
    Ok(Value::Bytea(bytes))
}

/// The error of a value that is not in the text form.
fn invalid(message: String) -> Error {
    Error::new(SqlState::INVALID_PARAMETER_VALUE, message)
}

/// The character at a byte position of the input, for a message.
fn symbol(text: &str, at: usize) -> &str {
    let c = text[at..].chars().next().map_or(0, char::len_utf8);
    &text[at..at + c]
}

/// True for the white space that the decoders skip.
fn space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r')
}

/// `hex_encode`: two lowercase digits for each byte.
fn hex_encode(data: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(data.len() * 2);
    for &b in data {
        out.push(char::from(DIGITS[usize::from(b >> 4)]));
        out.push(char::from(DIGITS[usize::from(b & 15)]));
    }
    out
}

/// `hex_decode`: the bytes of pairs of hexadecimal digits. White space between two pairs is skipped.
fn hex_decode(text: &str) -> Result<Vec<u8>> {
    let src = text.as_bytes();
    let digit = |at: usize| {
        char::from(src[at])
            .to_digit(16)
            .ok_or_else(|| invalid(format!("invalid hexadecimal digit: \"{}\"", symbol(text, at))))
    };
    let mut out = Vec::with_capacity(src.len() / 2);
    let mut s = 0;
    while s < src.len() {
        if space(src[s]) {
            s += 1;
            continue;
        }
        let high = digit(s)?;
        s += 1;
        if s >= src.len() {
            return Err(invalid("invalid hexadecimal data: odd number of digits".into()));
        }
        let low = digit(s)?;
        s += 1;
        out.push(((high << 4) | low) as u8);
    }
    Ok(out)
}

/// `pg_base64_encode_internal`: 4 characters for each 3 bytes. The standard form has padding and a new line after each 76 characters, and the URL form has neither.
fn base64_encode(data: &[u8], url: bool) -> String {
    let alphabet: &[u8; 64] = if url {
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_"
    } else {
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
    };
    let sym = |buf: u32, shift: u32| char::from(alphabet[((buf >> shift) & 0x3f) as usize]);
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4 + data.len() / 57);
    let mut line = 76;
    let mut chunks = data.chunks_exact(3);
    for c in &mut chunks {
        let buf = (u32::from(c[0]) << 16) | (u32::from(c[1]) << 8) | u32::from(c[2]);
        for shift in [18, 12, 6, 0] {
            out.push(sym(buf, shift));
        }
        if !url && out.len() >= line {
            out.push('\n');
            line = out.len() + 76;
        }
    }
    match *chunks.remainder() {
        [a] => {
            let buf = u32::from(a) << 16;
            out.push(sym(buf, 18));
            out.push(sym(buf, 12));
            if !url {
                out.push_str("==");
            }
        }
        [a, b] => {
            let buf = (u32::from(a) << 16) | (u32::from(b) << 8);
            out.push(sym(buf, 18));
            out.push(sym(buf, 12));
            out.push(sym(buf, 6));
            if !url {
                out.push('=');
            }
        }
        _ => {}
    }
    out
}

/// `pg_base64_decode_internal`: the bytes of groups of 4 characters. White space is skipped. The URL form can end without its padding.
fn base64_decode(text: &str, url: bool) -> Result<Vec<u8>> {
    let name = if url { "base64url" } else { "base64" };
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let (mut buf, mut pos, mut end) = (0u32, 0, 0);
    for (at, mut c) in text.bytes().enumerate() {
        if space(c) {
            continue;
        }
        if url {
            c = match c {
                b'-' => b'+',
                b'_' => b'/',
                c => c,
            };
        }
        let b = if c == b'=' {
            if end == 0 {
                end = match pos {
                    2 => 1,
                    3 => 2,
                    _ => {
                        return Err(invalid(format!(
                            "unexpected \"=\" while decoding {name} sequence"
                        )));
                    }
                };
            }
            0
        } else {
            match c {
                b'A'..=b'Z' => c - b'A',
                b'a'..=b'z' => c - b'a' + 26,
                b'0'..=b'9' => c - b'0' + 52,
                b'+' => 62,
                b'/' => 63,
                _ => {
                    return Err(invalid(format!(
                        "invalid symbol \"{}\" found while decoding {name} sequence",
                        symbol(text, at)
                    )));
                }
            }
        };
        buf = (buf << 6) | u32::from(b);
        pos += 1;
        if pos == 4 {
            out.push((buf >> 16) as u8);
            if end == 0 || end > 1 {
                out.push((buf >> 8) as u8);
            }
            if end == 0 || end > 2 {
                out.push(buf as u8);
            }
            buf = 0;
            pos = 0;
        }
    }
    match pos {
        0 => {}
        2 if url => out.push((buf >> 4) as u8),
        3 if url => {
            out.push((buf >> 10) as u8);
            out.push((buf >> 2) as u8);
        }
        _ => {
            return Err(invalid(format!("invalid {name} end sequence")).with_hint(
                "Input data is missing padding, is truncated, or is otherwise corrupted.",
            ));
        }
    }
    Ok(out)
}

/// `base32hex_encode`: 8 characters of the extended hex alphabet of RFC 4648 for each 5 bytes, with padding to a multiple of 8.
fn base32hex_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHIJKLMNOPQRSTUV";
    let mut out = String::with_capacity(data.len().div_ceil(5) * 8);
    let (mut buf, mut bits) = (0u32, 0);
    for &b in data {
        buf = (buf << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(char::from(ALPHABET[((buf >> bits) & 0x1f) as usize]));
            buf &= (1 << bits) - 1;
        }
    }
    if bits > 0 {
        out.push(char::from(ALPHABET[((buf << (5 - bits)) & 0x1f) as usize]));
    }
    while out.len() % 8 != 0 {
        out.push('=');
    }
    out
}

/// `base32hex_decode`: the bytes of the characters, which do not depend on case. White space is skipped, and only padding can come after the first `=`.
fn base32hex_decode(text: &str) -> Result<Vec<u8>> {
    let bad = |at| {
        invalid(format!(
            "invalid symbol \"{}\" found while decoding base32hex sequence",
            symbol(text, at)
        ))
    };
    let mut out = Vec::with_capacity(text.len() * 5 / 8);
    let (mut buf, mut bits, mut pos, mut end) = (0u32, 0, 0, false);
    for (at, c) in text.bytes().enumerate() {
        if space(c) {
            continue;
        }
        if c == b'=' {
            if !end {
                if !matches!(pos, 2 | 4 | 5 | 7) {
                    return Err(invalid(
                        "unexpected \"=\" while decoding base32hex sequence".into(),
                    ));
                }
                end = true;
            }
            pos += 1;
            continue;
        }
        if end {
            return Err(bad(at));
        }
        let val = match c {
            b'0'..=b'9' => c - b'0',
            b'A'..=b'V' => c - b'A' + 10,
            b'a'..=b'v' => c - b'a' + 10,
            _ => return Err(bad(at)),
        };
        buf = (buf << 5) | u32::from(val);
        bits += 5;
        pos += 1;
        while bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
        if pos == 8 {
            pos = 0;
        }
    }
    Ok(out)
}

/// `esc_encode`: `\ooo` for a zero byte and for a byte with the high bit, `\\` for a backslash, and the other bytes as they are.
fn escape_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len());
    for &c in data {
        if c == 0 || c >= 0x80 {
            out.push('\\');
            for shift in [6, 3, 0] {
                out.push(char::from(b'0' + ((c >> shift) & 7)));
            }
        } else if c == b'\\' {
            out.push_str("\\\\");
        } else {
            out.push(char::from(c));
        }
    }
    out
}

/// `esc_decode`: the bytes of the text, with `\ooo` and `\\` for one byte.
fn escape_decode(text: &str) -> Result<Vec<u8>> {
    let src = text.as_bytes();
    let mut out = Vec::with_capacity(src.len());
    let mut s = 0;
    while s < src.len() {
        if src[s] != b'\\' {
            out.push(src[s]);
            s += 1;
        } else if s + 3 < src.len()
            && (b'0'..=b'3').contains(&src[s + 1])
            && (b'0'..=b'7').contains(&src[s + 2])
            && (b'0'..=b'7').contains(&src[s + 3])
        {
            out.push(((src[s + 1] - b'0') << 6) | ((src[s + 2] - b'0') << 3) | (src[s + 3] - b'0'));
            s += 4;
        } else if s + 1 < src.len() && src[s + 1] == b'\\' {
            out.push(b'\\');
            s += 2;
        } else {
            return Err(Error::new(
                SqlState::INVALID_TEXT_REPRESENTATION,
                "invalid input syntax for type bytea",
            ));
        }
    }
    Ok(out)
}
