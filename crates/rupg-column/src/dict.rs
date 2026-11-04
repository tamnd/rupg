//! The global dictionary of a string column (spec/09 section 9.5).
//!
//! The strings are in one arena, in the order of their codes. A hash table of codes finds a string. Code 0 means no value and is never an entry, so the first string gets code 1 (spec/09 section 9.5.2).

use std::sync::OnceLock;

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
    /// The place of each code in the order of the bytes of all the strings, which [`Dictionary::order`] makes when a query first needs it. A new string clears it.
    order: OnceLock<Box<[u32]>>,
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
        Dictionary {
            bytes: Vec::new(),
            ends: Vec::new(),
            slots: vec![NONE; 1024],
            base: 0,
            order: OnceLock::new(),
        }
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
        self.order = OnceLock::new();
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
        let mut dict = Dictionary { bytes, ends, slots: Vec::new(), base, order: OnceLock::new() };
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

    /// The place of each code in the order of the bytes of all the strings, at the index of the code. Code [`NONE`] has place 0, and the first string in order has place 1. The dictionary sorts its strings one time, and keeps the places until it gets a new string.
    #[must_use]
    pub fn order(&self) -> &[u32] {
        self.order.get_or_init(|| {
            let mut codes: Vec<u32> = (1..=self.ends.len() as u32).collect();
            sort_bytes(&mut codes, |c| self.get(c));
            let mut places = vec![0; self.ends.len() + 1];
            for (place, &code) in codes.iter().enumerate() {
                places[code as usize] = place as u32 + 1;
            }
            places.into_boxed_slice()
        })
    }

    /// True when the dictionary has the places of [`Dictionary::order`], so that they cost nothing.
    #[must_use]
    pub fn has_order(&self) -> bool {
        self.order.get().is_some()
    }

    /// The number of codes of the sorted base.
    #[must_use]
    pub fn base(&self) -> u32 {
        self.base
    }
}

/// A run of codes shorter than this is sorted by a compare of the bytes after the digits that it has in common.
const SMALL_RUN: usize = 32;

/// Sorts `codes` by the bytes that `get` gives for each, as a sort of the bytes does. The sort takes eight bytes at a time as one number, so that most compares are of numbers. The codes of a run with the same eight bytes are sorted by the next eight bytes. Texts with a long common start, such as URLs, need no compare of the common start for each pair.
fn sort_bytes<'a>(codes: &mut [u32], get: impl Fn(u32) -> &'a [u8]) {
    let mut runs = vec![(0, codes.len(), 0)];
    let mut keys: Vec<(u64, u8, u32)> = Vec::new();
    while let Some((start, end, depth)) = runs.pop() {
        let run = &mut codes[start..end];
        if run.len() < SMALL_RUN {
            run.sort_unstable_by(|&a, &b| get(a)[depth..].cmp(&get(b)[depth..]));
            continue;
        }
        keys.clear();
        keys.extend(run.iter().map(|&c| {
            let rest = &get(c)[depth..];
            let mut digit = [0_u8; 8];
            let n = rest.len().min(8);
            digit[..n].copy_from_slice(&rest[..n]);
            // A text that ends in the digit comes before a longer text with the same digit, and one with more bytes after the digit gets 9.
            (u64::from_be_bytes(digit), rest.len().min(9) as u8, c)
        }));
        keys.sort_unstable();
        for (to, &(_, _, c)) in run.iter_mut().zip(&keys) {
            *to = c;
        }
        let mut at = 0;
        while at < keys.len() {
            let (digit, tag, _) = keys[at];
            let same = keys[at..].iter().take_while(|k| k.0 == digit && k.1 == tag).count();
            if tag == 9 && same > 1 {
                runs.push((start + at, start + at + same, depth + 8));
            }
            at += same;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The order of the codes is the order of a sort of their bytes, for texts with long common starts, texts that are the start of others, and zero bytes.
    #[test]
    fn a_sort_by_eight_bytes_is_the_sort_of_the_bytes() {
        let mut dict = Dictionary::new();
        let mut seed = 1_u64;
        for i in 0..20_000_u32 {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            let mut text = format!("http://example.com/path/{}", seed % 977).into_bytes();
            text.truncate((seed >> 40) as usize % 40);
            if i % 7 == 0 {
                text.push(0);
            }
            if i % 11 == 0 {
                text.extend(std::iter::repeat_n(b'a', i as usize % 30));
            }
            dict.intern(&text);
        }
        let mut sorted: Vec<u32> = (1..=dict.len() as u32).collect();
        sorted.sort_by(|&a, &b| dict.get(a).cmp(dict.get(b)));
        let order = dict.order();
        for (place, &code) in sorted.iter().enumerate() {
            assert_eq!(order[code as usize], place as u32 + 1, "{:?}", dict.get(code));
        }
    }

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

    #[test]
    fn the_order_is_the_order_of_the_bytes() {
        let mut dict = Dictionary::new();
        for word in ["pear", "apple", "fig", "", "apples"] {
            dict.intern(word.as_bytes());
        }
        assert_eq!(dict.order(), [0, 5, 2, 4, 1, 3]);
        assert!(dict.has_order());
        dict.intern(b"banana");
        assert!(!dict.has_order());
        assert_eq!(dict.order()[6], 4);
    }
}
