// Lifted from rudb crates/rudb-qc-rt/src/join.rs at cd9f9676 (2026-10-05).
// Changes: a module of rupg-jit, so the paths of the runtime are under `crate::rt`.

//! The join hash table, per section 10.5 of `spec/compiler/10-joins.md`: the unchained table of
//! Birler et al.
//!
//! A build pipeline appends one record per row with `jt_append`, which copies it and its hash into
//! a staging list and does nothing else. When the build has seen every row, [`JoinTable::finish`]
//! sizes the directory from the exact count, counts the entries per slot, takes the prefix sum and
//! scatters the entries so that every slot's entries lie next to each other. After that nothing
//! moves, and a probe pipeline reads the table directly from generated code:
//!
//! ```text
//! h     = hash * FOLD
//! word  = directory[h >> shift]            the first entry's address, and a Bloom tag on top
//! tag   = TAGS[h & 2047]
//! if word >> 48 & tag == tag:
//!     for e in word & ADDRESS .. directory[(h >> shift) + 1] & ADDRESS, step stride:
//!         if e.hash == hash and e.keys == keys: emit
//! ```
//!
//! An entry is the full hash and then the record the generator laid out:
//!
//! ```text
//! [hash: u64][each field: its value, then a byte that is 1 when it is valid][padding to 8]
//! ```
//!
//! The keys come first. A NULL key never enters the table, so a key's byte is always 1, and the
//! generator does not read it. A long string, key or payload, is copied into the runtime heap by
//! `jt_append`, because the record points into a morsel the build is about to drop.

#![allow(unsafe_code)]

use crate::rt::table::{KeyField, read_u128};
use crate::rt::text::{self, Heap};

/// The multiplier that spreads a hash over the high bits the slot is taken from.
pub const FOLD: u64 = 0x9e37_79b9_7f4a_7c15;

/// The bits of a directory word that hold an address.
pub const ADDRESS: u64 = (1 << 48) - 1;

/// The Bloom tags, 4 of 16 bits set in each, indexed by the low 11 bits of the folded hash.
pub static TAGS: [u16; 2048] = tags();

const fn tags() -> [u16; 2048] {
    let mut out = [0u16; 2048];
    let mut i = 0;
    while i < 2048 {
        // splitmix64 of the index, read four bits at a time until four different bits are set.
        let mut x = (i as u64).wrapping_add(0x9e37_79b9_7f4a_7c15);
        x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        x ^= x >> 31;
        let mut tag = 0u16;
        let mut set = 0;
        let mut shift = 0;
        while set < 4 {
            let bit = 1u16 << ((x >> shift) & 15);
            if tag & bit == 0 {
                tag |= bit;
                set += 1;
            }
            shift = (shift + 4) % 64;
            if shift == 0 {
                x = x.wrapping_mul(FOLD).rotate_left(17);
            }
        }
        out[i] = tag;
        i += 1;
    }
    out
}

/// The shape of a table's records.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct JoinLayout {
    /// The key columns.
    pub keys: Vec<KeyField>,
    /// The payload columns.
    pub payload: Vec<KeyField>,
    /// The bytes of a record, validity bytes included.
    pub size: u32,
    /// Whether each entry has a byte after its record that a probe sets when it matches the entry,
    /// for a semi or anti join whose left side builds. The build leaves it zero.
    pub marked: bool,
}

impl JoinLayout {
    /// The distance from one entry to the next: the hash, then the record and the mark padded
    /// to 8.
    #[must_use]
    pub fn stride(&self) -> u32 {
        8 + (self.size + u32::from(self.marked)).next_multiple_of(8)
    }

    /// Where the mark is in the record.
    #[must_use]
    pub fn mark(&self) -> u32 {
        self.size
    }
}

/// What a built table publishes to the pipelines that probe it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Published {
    /// The address of the directory, `u64[2^d + 1]`.
    pub directory: usize,
    /// `64 - d`, what the folded hash is shifted right by to give the slot.
    pub shift: u64,
    /// The address of [`TAGS`].
    pub tags: usize,
    /// How many entries the table holds.
    pub rows: usize,
}

/// A join hash table.
#[derive(Debug)]
pub struct JoinTable {
    layout: JoinLayout,
    stride: usize,
    /// The hash of every appended record, in the order they came.
    hashes: Vec<u64>,
    /// The records, back to back at `stride - 8` apart, until the table is finished.
    staged: Vec<u8>,
    /// The entries, grouped by slot, once finished. Words so that every entry is aligned.
    entries: Vec<u64>,
    directory: Vec<u64>,
    shift: u64,
    finished: bool,
}

impl JoinTable {
    /// An empty table for records of this shape.
    #[must_use]
    pub fn new(layout: JoinLayout) -> JoinTable {
        let stride = layout.stride() as usize;
        JoinTable {
            layout,
            stride,
            hashes: Vec::new(),
            staged: Vec::new(),
            entries: Vec::new(),
            directory: Vec::new(),
            shift: 63,
            finished: false,
        }
    }

    /// The shape of the records.
    #[must_use]
    pub fn layout(&self) -> &JoinLayout {
        &self.layout
    }

    /// How many records the build appended.
    #[must_use]
    pub fn len(&self) -> usize {
        self.hashes.len()
    }

    /// Whether the build appended nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.hashes.is_empty()
    }

    /// Whether [`JoinTable::finish`] has run.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Copies the record at `record` into the table.
    ///
    /// # Safety
    ///
    /// `record` must be the address of `size` readable bytes laid out as the table's
    /// [`JoinLayout`] says, and every valid non-inline string in it must point at live bytes.
    pub unsafe fn append(&mut self, record: usize, hash: u64, heap: &mut Heap) {
        let size = self.layout.size as usize;
        // SAFETY: the caller's contract.
        let bytes = unsafe { crate::rt::mem::slice(record, size) };
        let at = self.staged.len();
        self.staged.extend_from_slice(bytes);
        self.staged.resize(at + self.stride - 8, 0);
        for f in self.layout.keys.iter().chain(&self.layout.payload) {
            let o = at + f.offset as usize;
            if f.text && self.staged[at + f.null() as usize] != 0 {
                let s = read_u128(&self.staged[o..o + 16]);
                if text::len(s) > text::INLINE {
                    // SAFETY: the caller vouches for the record's strings.
                    let kept = heap.keep(unsafe { text::bytes(&s) });
                    self.staged[o..o + 16].copy_from_slice(&kept.to_le_bytes());
                }
            }
        }
        self.hashes.push(hash);
    }

    /// The finalize step of the build: lays the entries out by slot and writes the directory.
    ///
    /// # Errors
    ///
    /// When the entries landed at an address too high for a directory word to hold.
    pub fn finish(&mut self) -> Result<(), String> {
        let n = self.hashes.len();
        let slots = (n * 100).div_ceil(65).next_power_of_two().max(2);
        let bits = u64::from(slots.trailing_zeros());
        self.shift = 64 - bits;
        let slot = |hash: u64| (hash.wrapping_mul(FOLD) >> (64 - bits)) as usize;
        let mut start = vec![0usize; slots + 1];
        for &h in &self.hashes {
            start[slot(h) + 1] += 1;
        }
        for s in 0..slots {
            start[s + 1] += start[s];
        }
        let record = self.stride - 8;
        let words = self.stride / 8;
        let mut entries = vec![0u64; n * words];
        let mut fill = start.clone();
        let mut tags = vec![0u16; slots];
        for (i, &h) in self.hashes.iter().enumerate() {
            let s = slot(h);
            let at = fill[s] * words;
            fill[s] += 1;
            entries[at] = h;
            let src = &self.staged[i * record..(i + 1) * record];
            for (k, chunk) in src.chunks(8).enumerate() {
                let mut w = [0u8; 8];
                w[..chunk.len()].copy_from_slice(chunk);
                entries[at + 1 + k] = u64::from_le_bytes(w);
            }
            tags[s] |= TAGS[(h.wrapping_mul(FOLD) & 2047) as usize];
        }
        let base = entries.as_ptr().expose_provenance();
        let end = base + n * self.stride;
        if end as u64 > ADDRESS {
            return Err(format!(
                "the join table is at {end:#x}, above what a directory word holds"
            ));
        }
        let mut directory = Vec::with_capacity(slots + 1);
        for s in 0..slots {
            directory.push((base + start[s] * self.stride) as u64 | u64::from(tags[s]) << 48);
        }
        directory.push(end as u64);
        self.entries = entries;
        self.directory = directory;
        self.staged = Vec::new();
        self.finished = true;
        Ok(())
    }

    /// The values of key column `key` over every entry, sign extended, once the table is finished.
    ///
    /// `None` for a text key, a key wider than 8 bytes, or a table not finished. The driver hands
    /// these to the scan of a pipeline that probes the table, which then reads only the rows whose
    /// key one of these is. See `spec/compiler/10-joins.md` section 10.4.
    #[must_use]
    pub fn integers(&self, key: usize) -> Option<Vec<i64>> {
        let field = self.layout.keys.get(key)?;
        let width = field.width as usize;
        if !self.finished || field.text || !matches!(width, 1 | 2 | 4 | 8) {
            return None;
        }
        let words = self.stride / 8;
        // The record starts after the hash word, and the key is at its offset in the record.
        let (word, shift) = (1 + field.offset as usize / 8, field.offset as usize % 8 * 8);
        let bits = width * 8;
        Some(
            self.entries
                .chunks_exact(words)
                .map(|entry| {
                    let mut value = entry[word] >> shift;
                    if shift + bits > 64 {
                        value |= entry[word + 1] << (64 - shift);
                    }
                    // Up to the key's width and back down again, which extends its sign.
                    ((value << (64 - bits)) as i64) >> (64 - bits)
                })
                .collect(),
        )
    }

    /// The records a semi join keeps, those some probe marked, or with `semi` false the ones an
    /// anti join keeps, those no probe marked and those with a null key, which the build keeps in
    /// the table for this. Empty for a table with no marks or not finished.
    #[must_use]
    pub fn kept(&self, semi: bool) -> Vec<&[u8]> {
        if !self.finished || !self.layout.marked {
            return Vec::new();
        }
        // SAFETY: the entries are words, which are bytes with no padding.
        let bytes = unsafe {
            std::slice::from_raw_parts(self.entries.as_ptr().cast::<u8>(), self.entries.len() * 8)
        };
        let mark = self.layout.mark() as usize;
        bytes
            .chunks_exact(self.stride)
            .map(|entry| &entry[8..])
            .filter(|record| {
                let keyed = self.layout.keys.iter().all(|f| record[f.null() as usize] != 0);
                if semi { keyed && record[mark] != 0 } else { !keyed || record[mark] == 0 }
            })
            .collect()
    }

    /// What a probe reads, once the table is finished.
    #[must_use]
    pub fn published(&self) -> Published {
        Published {
            directory: self.directory.as_ptr().expose_provenance(),
            shift: self.shift,
            tags: TAGS.as_ptr().expose_provenance(),
            rows: self.hashes.len(),
        }
    }

    /// The records whose hash is `hash` and whose keys equal those of `key`, a record laid out
    /// as the table's, the way generated code finds them. For tests and for checking the
    /// generator.
    ///
    /// # Safety
    ///
    /// Every string in `key` must point at live bytes.
    #[must_use]
    pub unsafe fn matches(&self, key: &[u8], hash: u64) -> Vec<&[u8]> {
        let mut out = Vec::new();
        if !self.finished {
            return out;
        }
        let h = hash.wrapping_mul(FOLD);
        let s = (h >> self.shift) as usize;
        let word = self.directory[s];
        let tag = u64::from(TAGS[(h & 2047) as usize]);
        if (word >> 48) & tag != tag {
            return out;
        }
        let base = self.entries.as_ptr().expose_provenance();
        let lo = ((word & ADDRESS) as usize - base) / 8;
        let hi = ((self.directory[s + 1] & ADDRESS) as usize - base) / 8;
        let words = self.stride / 8;
        // SAFETY: the entries are words, which are bytes with no padding.
        let bytes = unsafe {
            std::slice::from_raw_parts(self.entries.as_ptr().cast::<u8>(), self.entries.len() * 8)
        };
        let mut e = lo;
        while e < hi {
            let entry = &bytes[e * 8..(e + words) * 8];
            // SAFETY: the caller's contract, and the table's strings are inline or in the heap.
            if self.entries[e] == hash && unsafe { self.same(&entry[8..], key) } {
                out.push(&entry[8..]);
            }
            e += words;
        }
        out
    }

    unsafe fn same(&self, record: &[u8], key: &[u8]) -> bool {
        self.layout.keys.iter().all(|f| {
            let (o, w) = (f.offset as usize, f.width as usize);
            if f.text {
                let (a, b) = (read_u128(&record[o..o + 16]), read_u128(&key[o..o + 16]));
                // SAFETY: the caller's contract.
                a == b || unsafe { text::bytes(&a) == text::bytes(&b) }
            } else {
                record[o..o + w] == key[o..o + w]
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> JoinLayout {
        JoinLayout {
            keys: vec![KeyField { offset: 0, width: 8, text: false }],
            payload: vec![KeyField { offset: 16, width: 16, text: true }],
            size: 33,
            marked: false,
        }
    }

    fn record(k: u64, s: &[u8]) -> Vec<u8> {
        let mut r = vec![0u8; 33];
        r[..8].copy_from_slice(&k.to_le_bytes());
        r[8] = 1;
        r[16..32].copy_from_slice(&text::make(s).to_le_bytes());
        r[32] = 1;
        r
    }

    #[test]
    fn every_tag_sets_four_bits() {
        assert!(TAGS.iter().all(|t| t.count_ones() == 4));
        assert!(TAGS.iter().collect::<std::collections::HashSet<_>>().len() > 1000);
    }

    #[test]
    fn duplicates_share_a_slot_and_long_strings_outlive_the_record() {
        let mut t = JoinTable::new(layout());
        let mut heap = Heap::new();
        let long = b"a payload longer than twelve bytes".to_vec();
        for i in 0..1000u64 {
            let r = record(i % 300, &long);
            // SAFETY: the record is alive and its string points at `long`.
            unsafe {
                t.append(r.as_ptr().expose_provenance(), (i % 300).wrapping_mul(31), &mut heap)
            };
        }
        drop(long);
        t.finish().unwrap();
        assert_eq!(t.len(), 1000);
        let key = record(7, b"");
        // SAFETY: the key's string is inline.
        let found = unsafe { t.matches(&key, 7 * 31) };
        assert_eq!(found.len(), 4);
        for r in found {
            let s = read_u128(&r[16..32]);
            // SAFETY: the payload was copied into the heap.
            assert_eq!(unsafe { text::bytes(&s) }, b"a payload longer than twelve bytes");
        }
        // SAFETY: as above.
        assert!(unsafe { t.matches(&record(300, b""), 300 * 31) }.is_empty());
    }

    #[test]
    fn a_semi_join_keeps_the_marked_entries_and_an_anti_join_the_rest() {
        let mut t = JoinTable::new(JoinLayout { marked: true, ..layout() });
        let mut heap = Heap::new();
        for i in 0..4u64 {
            let mut r = record(i, b"x");
            // The last one has a null key, which only an anti join keeps.
            r[8] = u8::from(i < 3);
            // SAFETY: the record is alive and its string is inline.
            unsafe { t.append(r.as_ptr().expose_provenance(), i, &mut heap) };
        }
        t.finish().unwrap();
        assert!(t.kept(true).is_empty());
        assert_eq!(t.kept(false).len(), 4);
        // What a probe does on a match: set the byte past the record, which is after the hash.
        let at = 8 + t.layout().mark() as usize;
        let words = t.stride / 8;
        let entry = t.entries.chunks_exact_mut(words).find(|e| e[0] == 1).unwrap();
        entry[at / 8] |= 1 << (at % 8 * 8);
        let keys = |rows: Vec<&[u8]>| rows.iter().map(|r| r[0]).collect::<Vec<_>>();
        assert_eq!(keys(t.kept(true)), [1]);
        let mut rest = keys(t.kept(false));
        rest.sort_unstable();
        assert_eq!(rest, [0, 2, 3]);
    }

    #[test]
    fn integer_keys_read_back_sign_extended() {
        // A two byte key, and an eight byte key that starts three bytes in and so crosses a word.
        let layout = JoinLayout {
            keys: vec![
                KeyField { offset: 0, width: 2, text: false },
                KeyField { offset: 3, width: 8, text: false },
            ],
            payload: vec![KeyField { offset: 16, width: 16, text: true }],
            size: 33,
            marked: false,
        };
        let mut t = JoinTable::new(layout);
        let mut heap = Heap::new();
        let pairs = [(-3i16, i64::MIN), (7, -1), (i16::MAX, 1 << 40)];
        for (at, &(a, b)) in pairs.iter().enumerate() {
            let mut r = record(0, b"");
            r[..2].copy_from_slice(&a.to_le_bytes());
            r[2] = 1;
            r[3..11].copy_from_slice(&b.to_le_bytes());
            r[11] = 1;
            // SAFETY: the record is alive and its string is inline.
            unsafe { t.append(r.as_ptr().expose_provenance(), at as u64, &mut heap) };
        }
        assert_eq!(t.integers(0), None, "not before the table is finished");
        t.finish().unwrap();
        let mut small = t.integers(0).unwrap();
        let mut large = t.integers(1).unwrap();
        small.sort_unstable();
        large.sort_unstable();
        assert_eq!(small, vec![-3, 7, i64::from(i16::MAX)]);
        assert_eq!(large, vec![i64::MIN, -1, 1 << 40]);
        assert_eq!(t.integers(2), None);
    }

    #[test]
    fn an_empty_table_finds_nothing() {
        let mut t = JoinTable::new(layout());
        t.finish().unwrap();
        // SAFETY: the key's string is inline.
        assert!(unsafe { t.matches(&record(1, b""), 31) }.is_empty());
        assert_eq!(t.published().rows, 0);
    }
}
