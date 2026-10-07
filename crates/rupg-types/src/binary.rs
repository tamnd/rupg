//! The binary format of the types in this crate.
//!
//! The send side needs no code: each value is its big-endian bytes, `bool` and `"char"` are one byte, and `uuid`, `bytea` and the string types are their bytes. The receive side reads a `Bind` parameter as the receive functions of PostgreSQL read their `StringInfo`, with the same errors when the value is too short. `exec_bind_message` in `src/backend/tcop/postgres.c` then checks that the function read the whole value, which is [`Recv::finish`].
//!
//! Lifted from `crates/rudb-pgtypes/src/binary.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

use rupg_common::SqlState;

use crate::error::TypeError;
use crate::scalar::NAME_MAX_BYTES;

/// A binary parameter value and how much of it a receive function has read.
#[derive(Debug, Clone)]
pub struct Recv<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Recv<'a> {
    pub fn new(data: &'a [u8]) -> Recv<'a> {
        Recv { data, at: 0 }
    }

    /// `pq_copymsgbytes` and `pq_getmsgbytes`.
    pub fn bytes(&mut self, len: usize) -> Result<&'a [u8], TypeError> {
        let Some(bytes) = self.data.get(self.at..self.at.saturating_add(len)) else {
            return Err(TypeError::new(
                SqlState::PROTOCOL_VIOLATION,
                "insufficient data left in message".to_string(),
            ));
        };
        self.at += len;
        Ok(bytes)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], TypeError> {
        let mut out = [0; N];
        out.copy_from_slice(self.bytes(N)?);
        Ok(out)
    }

    /// The number of bytes that are not read.
    pub fn remaining(&self) -> usize {
        self.data.len() - self.at
    }

    /// `pq_getmsgbyte`, which has its own text for an empty value.
    pub fn byte(&mut self) -> Result<u8, TypeError> {
        let Some(&byte) = self.data.get(self.at) else {
            return Err(TypeError::new(
                SqlState::PROTOCOL_VIOLATION,
                "no data left in message".to_string(),
            ));
        };
        self.at += 1;
        Ok(byte)
    }

    /// The rest of the value, as `textrecv` and `bytearecv` read it.
    pub fn rest(&mut self) -> &'a [u8] {
        let rest = &self.data[self.at..];
        self.at = self.data.len();
        rest
    }

    /// The rest of the value as a string in the server encoding, as `pq_getmsgtext` reads it for `textrecv`, `varcharrecv`, `json_recv` and the other string types. The client encoding is UTF-8, so the check is the check of `pg_verify_mbstr`: a zero byte or a byte sequence that is not UTF-8 is an error that shows the bytes of the bad character.
    pub fn text(&mut self) -> Result<&'a str, TypeError> {
        let rest = self.rest();
        let valid = match std::str::from_utf8(rest) {
            Ok(text) if !text.contains('\0') => return Ok(text),
            Ok(text) => text.len(),
            Err(error) => error.valid_up_to(),
        };
        let bad = rest[..valid].iter().position(|&b| b == 0).unwrap_or(valid);
        // pg_encoding_mblen_or_incomplete gives the length from the first byte.
        let len = match rest[bad] {
            b if b & 0x80 == 0 => 1,
            b if b & 0xe0 == 0xc0 => 2,
            b if b & 0xf0 == 0xe0 => 3,
            b if b & 0xf8 == 0xf0 => 4,
            _ => 1,
        };
        let bytes: Vec<String> =
            rest[bad..].iter().take(len).map(|b| format!("0x{b:02x}")).collect();
        Err(TypeError::new(
            SqlState::CHARACTER_NOT_IN_REPERTOIRE,
            format!("invalid byte sequence for encoding \"UTF8\": {}", bytes.join(" ")),
        ))
    }

    /// `boolrecv`: any byte other than 0 is true.
    pub fn bool(&mut self) -> Result<bool, TypeError> {
        Ok(self.byte()? != 0)
    }

    pub fn i16(&mut self) -> Result<i16, TypeError> {
        self.array().map(i16::from_be_bytes)
    }

    pub fn i32(&mut self) -> Result<i32, TypeError> {
        self.array().map(i32::from_be_bytes)
    }

    pub fn i64(&mut self) -> Result<i64, TypeError> {
        self.array().map(i64::from_be_bytes)
    }

    /// `oidrecv`.
    pub fn u32(&mut self) -> Result<u32, TypeError> {
        self.array().map(u32::from_be_bytes)
    }

    pub fn f32(&mut self) -> Result<f32, TypeError> {
        self.array().map(f32::from_be_bytes)
    }

    pub fn f64(&mut self) -> Result<f64, TypeError> {
        self.array().map(f64::from_be_bytes)
    }

    pub fn uuid(&mut self) -> Result<[u8; 16], TypeError> {
        self.array()
    }

    /// The check after the receive function: parameter `number`, from 1, must have no bytes left.
    pub fn finish(self, number: usize) -> Result<(), TypeError> {
        if self.at == self.data.len() {
            return Ok(());
        }
        Err(TypeError::new(
            SqlState::INVALID_BINARY_REPRESENTATION,
            format!("incorrect binary data format in bind parameter {number}"),
        ))
    }
}

/// `namerecv`: unlike the text input, the binary input refuses a name that is too long. The caller checks the encoding first, as `pq_getmsgtext` does.
pub fn name_recv(text: &str) -> Result<&str, TypeError> {
    if text.len() <= NAME_MAX_BYTES {
        return Ok(text);
    }
    let mut error = TypeError::new(SqlState::NAME_TOO_LONG, "identifier too long".to_string());
    error.detail = Some(format!("Identifier must be less than {} characters.", NAME_MAX_BYTES + 1));
    Err(error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_value_must_have_the_size_of_its_type() {
        let mut recv = Recv::new(&[0, 0, 0, 42]);
        assert_eq!(recv.i32(), Ok(42));
        assert_eq!(recv.finish(1), Ok(()));

        let mut recv = Recv::new(&[0, 0, 42]);
        let short = recv.i32().unwrap_err();
        assert_eq!(
            (short.sqlstate.as_str(), short.message.as_str()),
            ("08P01", "insufficient data left in message")
        );

        let mut recv = Recv::new(&[0, 0, 0, 0, 42]);
        assert_eq!(recv.i32(), Ok(0));
        let long = recv.finish(3).unwrap_err();
        assert_eq!(
            (long.sqlstate.as_str(), long.message.as_str()),
            ("22P03", "incorrect binary data format in bind parameter 3")
        );

        assert_eq!(Recv::new(&[]).bool().unwrap_err().message, "no data left in message");
        assert_eq!(Recv::new(&[7]).bool(), Ok(true));
        assert_eq!(Recv::new(&1.5f64.to_be_bytes()).f64(), Ok(1.5));
        assert_eq!(Recv::new(&[0xff; 4]).u32(), Ok(u32::MAX));
        let mut recv = Recv::new(b"abc");
        assert_eq!((recv.rest(), recv.finish(1)), (&b"abc"[..], Ok(())));
    }

    #[test]
    fn a_string_must_be_utf8_with_no_zero_byte() {
        assert_eq!(Recv::new("aé".as_bytes()).text(), Ok("aé"));
        let message = |bytes: &[u8]| Recv::new(bytes).text().unwrap_err().message;
        assert_eq!(message(b"a\0b"), "invalid byte sequence for encoding \"UTF8\": 0x00");
        assert_eq!(message(b"a\xc3("), "invalid byte sequence for encoding \"UTF8\": 0xc3 0x28");
        assert_eq!(message(b"\xe2\x82"), "invalid byte sequence for encoding \"UTF8\": 0xe2 0x82");
        assert_eq!(message(b"\xff"), "invalid byte sequence for encoding \"UTF8\": 0xff");
        assert_eq!(Recv::new(b"\xed\xa0\x80").text().unwrap_err().sqlstate.as_str(), "22021");
    }

    #[test]
    fn a_long_name_is_an_error_in_binary() {
        assert_eq!(name_recv(&"n".repeat(63)).map(str::len), Ok(63));
        let error = name_recv(&"n".repeat(64)).unwrap_err();
        assert_eq!(
            (error.sqlstate.as_str(), error.message.as_str()),
            ("42622", "identifier too long")
        );
        assert_eq!(error.detail.as_deref(), Some("Identifier must be less than 64 characters."));
    }
}
