// Lifted from rudb crates/rudb-qc-rt/src/text.rs at cd9f9676 (2026-10-05).
// Changes: none.

//! `str16`, the string header of `spec/compiler/06-qir.md` section 6.3, and the heap that long
//! strings made at run time live in.
//!
//! A `str16` is a `u32` length and then either up to 12 bytes inline or a 4 byte prefix and an 8
//! byte address. It is the first engine's `StringView` with the arena offset replaced by an
//! absolute address, which is what lets compiled code read the bytes without knowing which column
//! they came from.

#![allow(unsafe_code)]

use std::ptr;

const _: () = assert!(cfg!(target_endian = "little"), "str16 is laid out little endian");

/// The longest string a `str16` holds inline.
pub const INLINE: usize = 12;

/// The `str16` for bytes that stay where they are for as long as the header is used.
#[must_use]
pub fn make(bytes: &[u8]) -> u128 {
    let mut b = [0u8; 16];
    let len = bytes.len() as u32;
    b[..4].copy_from_slice(&len.to_le_bytes());
    if bytes.len() <= INLINE {
        b[4..4 + bytes.len()].copy_from_slice(bytes);
    } else {
        b[4..8].copy_from_slice(&bytes[..4]);
        let at = bytes.as_ptr().expose_provenance() as u64;
        b[8..].copy_from_slice(&at.to_le_bytes());
    }
    u128::from_le_bytes(b)
}

/// The length of a `str16`.
#[must_use]
pub fn len(s: u128) -> usize {
    s as u32 as usize
}

/// The bytes a `str16` stands for.
///
/// # Safety
///
/// A header longer than [`INLINE`] must hold the address of that many live bytes, and they must
/// stay live and unchanged while the result is used. Every header compiled code passes to the runtime was made by
/// [`make`] over a column buffer the driver keeps alive, or by a [`Heap`] the runtime keeps alive,
/// so the rule holds for anything that arrives through an `rtcall`.
#[must_use]
pub unsafe fn bytes(s: &u128) -> &[u8] {
    let n = len(*s);
    if n <= INLINE {
        // SAFETY: a `u128` is sixteen initialized bytes, the target is little endian, and the
        // inline payload is bytes 4 to 16 of it.
        let all = unsafe { &*ptr::from_ref(s).cast::<[u8; 16]>() };
        &all[4..4 + n]
    } else {
        let at = ptr::with_exposed_provenance::<u8>((*s >> 64) as u64 as usize);
        // SAFETY: the caller's contract.
        unsafe { std::slice::from_raw_parts(at, n) }
    }
}

/// Strings made while the query runs: the result of `lower`, a group key copied out of a morsel,
/// the running `MIN` of a text column.
///
/// Pages never move once allocated, so a header made here stays good until the heap is dropped,
/// which is the end of the query. That is the `temporary` storage class.
#[derive(Debug, Default)]
pub struct Heap {
    pages: Vec<Box<[u8]>>,
    used: usize,
    big: Vec<Box<[u8]>>,
}

const PAGE: usize = 1 << 20;

impl Heap {
    /// An empty heap.
    #[must_use]
    pub fn new() -> Heap {
        Heap::default()
    }

    /// Copies the bytes in and returns a header for the copy.
    pub fn keep(&mut self, bytes: &[u8]) -> u128 {
        if bytes.len() <= INLINE {
            return make(bytes);
        }
        if bytes.len() > PAGE / 4 {
            let page: Box<[u8]> = bytes.into();
            let s = make(&page);
            self.big.push(page);
            return s;
        }
        if self.pages.is_empty() || self.used + bytes.len() > PAGE {
            self.pages.push(vec![0u8; PAGE].into_boxed_slice());
            self.used = 0;
        }
        let Some(page) = self.pages.last_mut() else { return make(bytes) };
        let at = self.used;
        page[at..at + bytes.len()].copy_from_slice(bytes);
        self.used += bytes.len();
        make(&page[at..at + bytes.len()])
    }

    /// How many bytes the heap holds.
    #[must_use]
    pub fn footprint(&self) -> usize {
        self.pages.iter().chain(&self.big).map(|p| p.len()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_strings_are_inline_and_long_ones_point() {
        let s = make(b"hello");
        assert_eq!(len(s), 5);
        // SAFETY: inline.
        assert_eq!(unsafe { bytes(&s) }, b"hello");
        let long = b"a string that does not fit".to_vec();
        let s = make(&long);
        assert_eq!(len(s), long.len());
        // SAFETY: `long` is alive.
        assert_eq!(unsafe { bytes(&s) }, &long[..]);
    }

    #[test]
    fn the_heap_keeps_what_it_is_given() {
        let mut heap = Heap::new();
        let mut kept = Vec::new();
        for i in 0..20_000 {
            let text = format!("row number {i} of a long enough string");
            kept.push((heap.keep(text.as_bytes()), text));
        }
        let big = heap.keep(&vec![7u8; PAGE]);
        kept.push((
            heap.keep(b"after the big one, still long"),
            "after the big one, still long".into(),
        ));
        // SAFETY: the heap is alive.
        assert!(unsafe { bytes(&big) }.iter().all(|b| *b == 7));
        for (s, text) in &kept {
            // SAFETY: the heap is alive.
            assert_eq!(unsafe { bytes(s) }, text.as_bytes());
        }
    }
}
