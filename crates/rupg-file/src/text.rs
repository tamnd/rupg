//! The text form of spec/08 section 8.14.
//!
//! A text form has one field on each line, as the field name, a space and the value. The lines are sorted by name. [`TextIn`] reads the text back and refuses a missing field, an unknown field and a field that is there two times. Integers are decimal, checksums are hexadecimal with `0x`, and byte strings are hexadecimal with no prefix.

use std::collections::BTreeMap;
use std::fmt::{self, Write as _};
use std::str::FromStr;

use rupg_common::{Error, Result, SqlState};

/// Writes a text form.
#[derive(Debug, Default)]
pub struct TextOut {
    fields: Vec<(String, String)>,
}

impl TextOut {
    /// An empty text form.
    pub fn new() -> TextOut {
        TextOut::default()
    }

    /// Adds a field with the `Display` form of the value.
    pub fn field(&mut self, name: &str, value: impl fmt::Display) -> &mut TextOut {
        self.fields.push((name.to_string(), value.to_string()));
        self
    }

    /// Adds a field as `0x` and 16 hexadecimal digits.
    pub fn hex(&mut self, name: &str, value: u64) -> &mut TextOut {
        self.field(name, format_args!("{value:#018x}"))
    }

    /// Adds a field as hexadecimal bytes, or `none` if all bytes are zero.
    pub fn bytes(&mut self, name: &str, value: &[u8]) -> &mut TextOut {
        let text =
            if value.iter().all(|&b| b == 0) { "none".to_string() } else { hex_bytes(value) };
        self.fields.push((name.to_string(), text));
        self
    }

    /// The text, with the lines sorted by name.
    pub fn finish(mut self) -> String {
        self.fields.sort();
        let mut text = String::new();
        for (name, value) in &self.fields {
            let _ = writeln!(text, "{name} {value}");
        }
        text
    }
}

/// The bytes as hexadecimal digits with no prefix.
pub fn hex_bytes(value: &[u8]) -> String {
    let mut text = String::with_capacity(value.len() * 2);
    for b in value {
        let _ = write!(text, "{b:02x}");
    }
    text
}

/// Reads a text form.
#[derive(Debug)]
pub struct TextIn<'a> {
    what: &'static str,
    fields: BTreeMap<&'a str, &'a str>,
}

impl<'a> TextIn<'a> {
    /// Reads the lines of `text`. `what` names the structure in error messages. Blank lines are skipped.
    pub fn parse(what: &'static str, text: &'a str) -> Result<TextIn<'a>> {
        let mut fields = BTreeMap::new();
        for line in text.lines() {
            let line = line.trim_end();
            if line.is_empty() {
                continue;
            }
            let (name, value) = line.split_once(' ').unwrap_or((line, ""));
            if fields.insert(name, value).is_some() {
                return Err(bad(what, format!("the field \"{name}\" is there two times")));
            }
        }
        Ok(TextIn { what, fields })
    }

    /// Takes a field as text.
    pub fn take_str(&mut self, name: &str) -> Result<&'a str> {
        self.fields
            .remove(name)
            .ok_or_else(|| bad(self.what, format!("the field \"{name}\" is missing")))
    }

    /// Takes a field and parses it with `FromStr`.
    pub fn take<T: FromStr>(&mut self, name: &str) -> Result<T> {
        let value = self.take_str(name)?;
        value.parse().map_err(|_| {
            bad(self.what, format!("the field \"{name}\" has the bad value \"{value}\""))
        })
    }

    /// Takes a field that [`TextOut::hex`] wrote.
    pub fn take_hex(&mut self, name: &str) -> Result<u64> {
        let value = self.take_str(name)?;
        value.strip_prefix("0x").and_then(|digits| u64::from_str_radix(digits, 16).ok()).ok_or_else(
            || bad(self.what, format!("the field \"{name}\" has the bad value \"{value}\"")),
        )
    }

    /// Takes a field that [`TextOut::bytes`] wrote, with exactly `N` bytes.
    pub fn take_bytes<const N: usize>(&mut self, name: &str) -> Result<[u8; N]> {
        let value = self.take_str(name)?;
        let mut out = [0u8; N];
        if value == "none" {
            return Ok(out);
        }
        let error =
            || bad(self.what, format!("the field \"{name}\" must have {N} bytes in hexadecimal"));
        if value.len() != N * 2 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(error());
        }
        for (i, b) in out.iter_mut().enumerate() {
            *b = u8::from_str_radix(&value[i * 2..i * 2 + 2], 16).map_err(|_| error())?;
        }
        Ok(out)
    }

    /// Ends the read. It is an error if a field was not taken.
    pub fn finish(self) -> Result<()> {
        match self.fields.keys().next() {
            Some(name) => Err(bad(self.what, format!("the field \"{name}\" is not known"))),
            None => Ok(()),
        }
    }
}

fn bad(what: &str, message: String) -> Error {
    Error::new(
        SqlState::INVALID_TEXT_REPRESENTATION,
        format!("bad text form of the {what}: {message}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let mut out = TextOut::new();
        out.field("b", 7)
            .hex("a", 0xab)
            .bytes("c", &[1, 0xff])
            .bytes("d", &[0, 0])
            .field("e", "two words");
        let text = out.finish();
        assert_eq!(text, "a 0x00000000000000ab\nb 7\nc 01ff\nd none\ne two words\n");
        let mut back = TextIn::parse("test", &text).unwrap();
        assert_eq!(back.take_hex("a").unwrap(), 0xab);
        assert_eq!(back.take::<u32>("b").unwrap(), 7);
        assert_eq!(back.take_bytes::<2>("c").unwrap(), [1, 0xff]);
        assert_eq!(back.take_bytes::<2>("d").unwrap(), [0, 0]);
        assert_eq!(back.take_str("e").unwrap(), "two words");
        back.finish().unwrap();
    }

    #[test]
    fn errors() {
        let state = |r: Result<()>| r.unwrap_err().state();
        assert_eq!(
            state(TextIn::parse("test", "a 1\na 2\n").map(|_| ())),
            SqlState::INVALID_TEXT_REPRESENTATION
        );
        let mut t = TextIn::parse("test", "a x\nb 0x1\nc 0g\n\nz 1\n").unwrap();
        assert!(t.take::<u32>("a").is_err());
        assert!(t.take::<u32>("missing").is_err());
        assert!(t.take_bytes::<2>("b").is_err());
        assert!(t.take_bytes::<1>("c").is_err());
        let e = t.finish().unwrap_err();
        assert_eq!(e.message(), "bad text form of the test: the field \"z\" is not known");
    }
}
