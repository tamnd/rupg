//! A cursor over the body of one message.
//!
//! Each method fails the way the `pq_getmsg*` function of the same name in `src/backend/libpq/pqformat.c` fails, with the same text. All of them are [`Level::Error`]: the frame was read in full, so the server is still in sync with the client.
//!
//! [`Level::Error`]: crate::Level::Error
//!
//! Lifted from `crates/rudb-pgwire/src/reader.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

use crate::error::{Level, ProtocolError};

/// SQLSTATE `22021`, `character_not_in_repertoire`.
const CHARACTER_NOT_IN_REPERTOIRE: &str = "22021";

#[derive(Debug, Clone)]
pub(crate) struct Reader<'a> {
    data: &'a [u8],
    at: usize,
    utf8: bool,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Reader<'a> {
        Reader { data, at: 0, utf8: false }
    }

    /// A reader whose [`string`](Reader::string) checks each string with [`verify_utf8`].
    pub(crate) fn utf8(data: &'a [u8]) -> Reader<'a> {
        Reader { data, at: 0, utf8: true }
    }

    pub(crate) fn remaining(&self) -> usize {
        self.data.len() - self.at
    }

    /// `pq_getmsgbyte`.
    pub(crate) fn byte(&mut self) -> Result<u8, ProtocolError> {
        let Some(&byte) = self.data.get(self.at) else {
            return Err(ProtocolError::error("no data left in message"));
        };
        self.at += 1;
        Ok(byte)
    }

    /// `pq_copymsgbytes`, which is under `pq_getmsgint`.
    fn array<const N: usize>(&mut self) -> Result<[u8; N], ProtocolError> {
        let bytes = self.take(N)?;
        let mut array = [0; N];
        array.copy_from_slice(bytes);
        Ok(array)
    }

    /// `pq_getmsgint(msg, 2)`. PostgreSQL reads the value as unsigned.
    pub(crate) fn u16(&mut self) -> Result<u16, ProtocolError> {
        self.array().map(u16::from_be_bytes)
    }

    pub(crate) fn i16(&mut self) -> Result<i16, ProtocolError> {
        self.array().map(i16::from_be_bytes)
    }

    /// `pq_getmsgint(msg, 4)`.
    pub(crate) fn u32(&mut self) -> Result<u32, ProtocolError> {
        self.array().map(u32::from_be_bytes)
    }

    pub(crate) fn i32(&mut self) -> Result<i32, ProtocolError> {
        self.array().map(i32::from_be_bytes)
    }

    /// `pq_getmsgbytes`. A length below zero fails the same way as a length past the end.
    pub(crate) fn bytes(&mut self, len: i32) -> Result<&'a [u8], ProtocolError> {
        match usize::try_from(len) {
            Ok(len) => self.take(len),
            Err(_) => Err(ProtocolError::error("insufficient data left in message")),
        }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], ProtocolError> {
        if len > self.remaining() {
            return Err(ProtocolError::error("insufficient data left in message"));
        }
        let bytes = &self.data[self.at..self.at + len];
        self.at += len;
        Ok(bytes)
    }

    /// `pq_getmsgstring`. The string does not include its terminating zero byte.
    ///
    /// The conversion from the client encoding is not the job of the codec. But when the client encoding is `UTF8` or `SQL_ASCII`, PostgreSQL only checks the bytes, and it does that before it reads the next field. A reader made with [`Reader::utf8`] does the same check at the same place, so a message with two faults gets the error of the first.
    pub(crate) fn string(&mut self) -> Result<&'a [u8], ProtocolError> {
        let rest = &self.data[self.at..];
        let Some(len) = rest.iter().position(|&b| b == 0) else {
            return Err(ProtocolError::error("invalid string in message"));
        };
        if self.utf8 {
            verify_utf8(&rest[..len])?;
        }
        self.at += len + 1;
        Ok(&rest[..len])
    }

    /// The bytes that are left, which is the whole of the rest of the message.
    pub(crate) fn rest(&mut self) -> &'a [u8] {
        let rest = &self.data[self.at..];
        self.at = self.data.len();
        rest
    }

    /// `pq_getmsgend`.
    pub(crate) fn end(&self) -> Result<(), ProtocolError> {
        if self.remaining() == 0 {
            Ok(())
        } else {
            Err(ProtocolError::error("invalid message format"))
        }
    }
}

/// Checks that `text` is UTF-8, with the error of `pg_verify_mbstr` in PostgreSQL.
///
/// The server calls this for each string that PostgreSQL converts from the client encoding when that encoding is `UTF8` or `SQL_ASCII`, for example a parameter value of a `Bind` in text format. The error shows the bytes of the first bad character: as many as its first byte says the character has, but not past the end of `text` and not more than 8.
///
/// # Errors
///
/// SQLSTATE `22021` with the text `invalid byte sequence for encoding "UTF8": 0x..`.
pub fn verify_utf8(text: &[u8]) -> Result<&str, ProtocolError> {
    std::str::from_utf8(text).map_err(|e| {
        let bad = &text[e.valid_up_to()..];
        let len = match bad[0] {
            b if b & 0xe0 == 0xc0 => 2,
            b if b & 0xf0 == 0xe0 => 3,
            b if b & 0xf8 == 0xf0 => 4,
            _ => 1,
        };
        let bytes: Vec<String> = bad.iter().take(len).map(|b| format!("0x{b:02x}")).collect();
        ProtocolError::new(
            Level::Error,
            CHARACTER_NOT_IN_REPERTOIRE,
            format!("invalid byte sequence for encoding \"UTF8\": {}", bytes.join(" ")),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bad_character_is_shown_as_postgres_shows_it() {
        let message = |text: &[u8]| verify_utf8(text).unwrap_err().message;
        assert_eq!(message(b"select '\xff'"), "invalid byte sequence for encoding \"UTF8\": 0xff");
        // A lead byte of three bytes, then a byte that does not continue it.
        assert_eq!(
            message(b"a\xe2\x82x"),
            "invalid byte sequence for encoding \"UTF8\": 0xe2 0x82 0x78"
        );
        // A character that the end of the string cuts.
        assert_eq!(message(b"a\xf0\x9f"), "invalid byte sequence for encoding \"UTF8\": 0xf0 0x9f");
        // A surrogate is not UTF-8 in PostgreSQL either.
        assert_eq!(
            message(b"\xed\xa0\x80"),
            "invalid byte sequence for encoding \"UTF8\": 0xed 0xa0 0x80"
        );
        assert_eq!(verify_utf8("\u{e9}t\u{e9}".as_bytes()), Ok("\u{e9}t\u{e9}"));
    }

    #[test]
    fn a_checked_string_fails_before_the_end_of_the_message() {
        let mut r = Reader::utf8(b"\xff\0junk");
        assert_eq!(r.string().unwrap_err().sqlstate, CHARACTER_NOT_IN_REPERTOIRE);
        let mut r = Reader::new(b"\xff\0junk");
        assert_eq!(r.string(), Ok(&b"\xff"[..]));
        assert_eq!(r.end().unwrap_err().message, "invalid message format");
    }
}
