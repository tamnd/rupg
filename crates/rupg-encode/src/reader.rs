//! A checked cursor over encoded bytes.
//!
//! Every decoder in this crate reads a chunk that came off a disk, and a disk has been there longer
//! than the process has. A length field in it can say anything, so every read is bounds checked and
//! a read that would run off the end is an error rather than a panic and never a read of whatever
//! happened to be next in memory.

// Lifted from rudb crates/rudb-encoding/src/reader.rs at cd9f9676 (2026-10-05).
// Changes: the error type is rupg_common::Error.

use rupg_common::{Error, Result};

/// A cursor over a chunk.
#[derive(Debug)]
pub(crate) struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    /// How many bytes are left, which is what a top level decoder checks is zero.
    pub(crate) fn remaining(&self) -> usize {
        self.bytes.len() - self.at
    }

    /// How many bytes have been read, which is how a nested chunk reports its own length to the
    /// decoder that holds it.
    pub(crate) fn used(&self) -> usize {
        self.at
    }

    /// Everything not yet read, for handing to a nested decoder that will say afterwards how much
    /// of it it took.
    pub(crate) fn rest(&self) -> &'a [u8] {
        &self.bytes[self.at..]
    }

    pub(crate) fn skip(&mut self, len: usize) -> Result<()> {
        self.bytes(len).map(|_| ())
    }

    pub(crate) fn take<const N: usize>(&mut self) -> Result<[u8; N]> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.bytes(N)?);
        Ok(out)
    }

    pub(crate) fn bytes(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self.at.checked_add(len).ok_or_else(|| self.short(len))?;
        if end > self.bytes.len() {
            return Err(self.short(len));
        }
        let out = &self.bytes[self.at..end];
        self.at = end;
        Ok(out)
    }

    pub(crate) fn u8(&mut self) -> Result<u8> {
        Ok(self.take::<1>()?[0])
    }

    pub(crate) fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take::<4>()?))
    }

    pub(crate) fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take::<8>()?))
    }

    pub(crate) fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_le_bytes(self.take::<8>()?))
    }

    fn short(&self, len: usize) -> Error {
        Error::internal(format!(
            "a chunk of {} bytes ended at {} with {len} more wanted",
            self.bytes.len(),
            self.at
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reading_walks_forward_and_reports_where_it_is() {
        let bytes = [1u8, 0, 0, 0, 2, 3, 4, 5, 6, 7, 8, 9];
        let mut reader = Reader::new(&bytes);
        assert_eq!(reader.u32().unwrap(), 1);
        assert_eq!(reader.used(), 4);
        assert_eq!(reader.remaining(), 8);
        assert_eq!(reader.rest(), &bytes[4..]);
        assert_eq!(reader.bytes(2).unwrap(), &[2, 3]);
        reader.skip(6).unwrap();
        assert_eq!(reader.remaining(), 0);
    }

    #[test]
    fn a_read_past_the_end_is_an_error_and_leaves_the_cursor_alone() {
        let bytes = [1u8, 2, 3];
        let mut reader = Reader::new(&bytes);
        assert!(reader.u32().is_err());
        assert_eq!(reader.used(), 0);
        assert_eq!(reader.u8().unwrap(), 1);
        assert!(reader.bytes(3).is_err());
        assert!(reader.skip(usize::MAX).is_err());
        assert_eq!(reader.used(), 1);
    }

    #[test]
    fn the_widths_read_little_endian() {
        let bytes = [0xffu8, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f];
        assert_eq!(Reader::new(&bytes).u64().unwrap(), u64::MAX >> 1);
        assert_eq!(Reader::new(&bytes).i64().unwrap(), i64::MAX);
    }
}
