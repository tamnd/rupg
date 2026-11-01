//! The global dictionary of a string column (spec/09 section 9.5).
//!
//! The strings are in one arena, in the order of their codes. A hash table of codes finds a string. Code 0 means no value and is never an entry, so the first string gets code 1 (spec/09 section 9.5.2).

use rupg_common::{Error, Result};
use rupg_kernels::hash;

/// The code of no value. The value stream holds it at a null position.
pub const NONE: u32 = 0;

/// The code that a lookup gives when the dictionary does not have the string.
pub const MISSING: u32 = u32::MAX;

/// The strings of one column of one table, each with its code.
#[derive(Clone, Debug)]
pub struct Dictionary {
    /// The bytes of every string, in the order of the codes.
    bytes: Vec<u8>,
    /// The end of each string in `bytes`. The string of code `c` ends at `ends[c - 1]`.
    ends: Vec<u64>,
    /// An open address table of codes, with [`NONE`] in an empty slot. Its length is a power of two.
    slots: Vec<u32>,
    /// The codes of the sorted base are 1 to `base`. The other codes are the tail, in the order they came (spec/09 section 9.5.3).
    base: u32,
}

impl Default for Dictionary {
    fn default() -> Dictionary {
        Dictionary::new()
    }
}

impl Dictionary {
    /// A dictionary with no strings.
    #[must_use]
    pub fn new() -> Dictionary {
        Dictionary { bytes: Vec::new(), ends: Vec::new(), slots: vec![NONE; 1024], base: 0 }
    }

    /// The number of strings.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ends.len()
    }

    /// True when the dictionary has no strings.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }

    /// The bytes of the arena and of the table, for the memory report.
    #[must_use]
    pub fn footprint(&self) -> usize {
        self.bytes.capacity() + self.ends.capacity() * 8 + self.slots.capacity() * 4
    }

    /// The string of `code`.
    ///
    /// # Panics
    ///
    /// When `code` is not a code of the dictionary.
    #[must_use]
    #[inline]
    pub fn get(&self, code: u32) -> &[u8] {
        let at = code as usize - 1;
        let start = if at == 0 { 0 } else { self.ends[at - 1] as usize };
        &self.bytes[start..self.ends[at] as usize]
    }

    /// The code of `value`, or [`MISSING`].
    #[must_use]
    pub fn find(&self, value: &[u8]) -> u32 {
        let mask = self.slots.len() - 1;
        let mut at = hash::bytes(value) as usize & mask;
        loop {
            let code = self.slots[at];
            if code == NONE {
                return MISSING;
            }
            if self.get(code) == value {
                return code;
            }
            at = (at + 1) & mask;
        }
    }

    /// The code of `value`, which the dictionary adds to the tail when it does not have it.
    ///
    /// # Panics
    ///
    /// When the dictionary already has 4,294,967,294 strings.
    pub fn intern(&mut self, value: &[u8]) -> u32 {
        let mask = self.slots.len() - 1;
        let mut at = hash::bytes(value) as usize & mask;
        loop {
            let code = self.slots[at];
            if code == NONE {
                break;
            }
            if self.get(code) == value {
                return code;
            }
            at = (at + 1) & mask;
        }
        assert!(
            self.ends.len() < (MISSING - 1) as usize,
            "a dictionary holds at most 4294967294 strings"
        );
        self.bytes.extend_from_slice(value);
        self.ends.push(self.bytes.len() as u64);
        let code = self.ends.len() as u32;
        self.slots[at] = code;
        if self.ends.len() * 2 > self.slots.len() {
            self.grow();
        }
        code
    }

    /// The arena, the end of each string and the size of the sorted base, which [`Dictionary::from_parts`] takes back.
    #[must_use]
    pub fn parts(&self) -> (&[u8], &[u64], u32) {
        (&self.bytes, &self.ends, self.base)
    }

    /// The dictionary of the parts that [`Dictionary::parts`] gave, with a new table of codes.
    ///
    /// # Errors
    ///
    /// `XX001` when an end is before the end before it or after the arena, the last end is not the end of the arena, the base has more codes than the dictionary, or the dictionary has too many strings.
    pub fn from_parts(bytes: Vec<u8>, ends: Vec<u64>, base: u32) -> Result<Dictionary> {
        let ordered = ends.is_sorted() && ends.last().map_or(0, |&end| end) == bytes.len() as u64;
        if !ordered || base as usize > ends.len() || ends.len() >= (MISSING - 1) as usize {
            return Err(Error::corrupted("a dictionary has bad string ends"));
        }
        let size = (ends.len() * 2 + 1).next_power_of_two().max(1024);
        let mut dict = Dictionary { bytes, ends, slots: Vec::new(), base };
        dict.fill(size);
        Ok(dict)
    }

    fn grow(&mut self) {
        self.fill(self.slots.len() * 2);
    }

    /// Makes the table of codes again with `size` slots, a power of two.
    fn fill(&mut self, size: usize) {
        let mask = size - 1;
        let mut slots = vec![NONE; size];
        for code in 1..=self.ends.len() as u32 {
            let mut at = hash::bytes(self.get(code)) as usize & mask;
            while slots[at] != NONE {
                at = (at + 1) & mask;
            }
            slots[at] = code;
        }
        self.slots = slots;
    }

    /// The rank of `code` in the sorted base, or `None` for a code of the tail.
    #[must_use]
    #[inline]
    pub fn rank(&self, code: u32) -> Option<u32> {
        (code <= self.base).then_some(code)
    }

    /// The number of codes of the sorted base.
    #[must_use]
    pub fn base(&self) -> u32 {
        self.base
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_stable_and_start_at_one() {
        let mut dict = Dictionary::new();
        let words: Vec<String> =
            (0..5000).map(|i| format!("http://example.com/{}", i % 3000)).collect();
        let codes: Vec<u32> = words.iter().map(|w| dict.intern(w.as_bytes())).collect();
        assert_eq!(dict.len(), 3000);
        assert_eq!(codes[0], 1);
        assert_eq!(codes[3000], 1);
        assert_eq!(dict.intern(b""), 3001);
        for (w, &c) in words.iter().zip(&codes) {
            assert_eq!(dict.get(c), w.as_bytes());
            assert_eq!(dict.find(w.as_bytes()), c);
        }
        assert_eq!(dict.find(b"nope"), MISSING);
        assert_eq!(dict.find(b""), 3001);
        assert_eq!(dict.rank(5), None);
    }
}
