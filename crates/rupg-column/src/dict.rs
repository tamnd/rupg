//! The global dictionary of a string column (spec/09 section 9.5).
//!
//! The strings are in one arena, in the order of their codes. A hash table of codes finds a string. Code 0 means no value and is never an entry, so the first string gets code 1 (spec/09 section 9.5.2).

use std::sync::{Mutex, OnceLock};

use rupg_common::bytes::Bytes;
use rupg_common::{Error, Result};
use rupg_kernels::hash;

/// The code of no value. The value stream holds it at a null position.
pub const NONE: u32 = 0;

/// The code that a lookup gives when the dictionary does not have the string.
pub const MISSING: u32 = u32::MAX;

/// The most strings that [`Recent`] keeps.
const RECENT: usize = 8;

/// The last strings that [`Dictionary::find`] found with no table of codes, with their codes, the last first. A scan finds the same constant in each vector, and this list finds it again with no read of all the strings.
#[derive(Debug, Default)]
struct Recent(Mutex<Vec<(Box<[u8]>, u32)>>);

impl Recent {
    /// The code of `value` in the list, which then goes first.
    fn get(&self, value: &[u8]) -> Option<u32> {
        let mut list = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let at = list.iter().position(|(s, _)| **s == *value)?;
        let found = list.remove(at);
        let code = found.1;
        list.insert(0, found);
        Some(code)
    }

    /// Puts `value` with its code `code` first, and drops the last string when the list is full.
    fn put(&self, value: &[u8], code: u32) {
        let mut list = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        list.truncate(RECENT - 1);
        list.insert(0, (value.into(), code));
    }
}

impl Clone for Recent {
    fn clone(&self) -> Recent {
        Recent::default()
    }
}

/// The strings of one column of one table, each with its code.
///
/// A dictionary that a load reads from a mapping of the file keeps its strings and their ends in the mapping, and makes its table of codes when it first finds a string. A dictionary that no query reads then costs no memory of the process (spec/19 section 19.5).
#[derive(Clone, Debug)]
pub struct Dictionary {
    /// The bytes of every string, in the order of the codes.
    bytes: Store,
    /// The end of each string in `bytes`, as eight bytes little endian. The string of code `c` ends at end `c - 1`.
    ends: Store,
    /// An open address table of codes, with [`NONE`] in an empty slot. Its length is a power of two. The first find makes it.
    slots: OnceLock<Vec<u32>>,
    /// The codes of the sorted base are 1 to `base`. The other codes are the tail, in the order they came (spec/09 section 9.5.3).
    base: u32,
    /// The place of each code in the order of the bytes of all the strings, which [`Dictionary::order`] makes when a query first needs it. A new string clears it.
    order: OnceLock<Box<[u32]>>,
    /// The last strings that a find with no table found. A new string clears them.
    recent: Recent,
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
            bytes: Store::default(),
            ends: Store::default(),
            slots: OnceLock::from(vec![NONE; 1024]),
            base: 0,
            order: OnceLock::new(),
            recent: Recent::default(),
        }
    }

    /// The number of strings.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ends.slice().len() / 8
    }

    /// True when the dictionary has no strings.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ends.slice().is_empty()
    }

    /// The bytes of the arena and of the table that the process owns, for the memory report. Bytes in a mapping of the file are not counted.
    #[must_use]
    pub fn footprint(&self) -> usize {
        self.bytes.size()
            + self.ends.size()
            + self.slots.get().map_or(0, |slots| slots.capacity() * 4)
    }

    /// The table of codes, which the first call makes.
    fn slots(&self) -> &[u32] {
        self.slots.get_or_init(|| self.table((self.len() * 2 + 1).next_power_of_two().max(1024)))
    }

    /// The string of `code`.
    ///
    /// # Panics
    ///
    /// When `code` is not a code of the dictionary.
    #[must_use]
    #[inline(always)]
    pub fn get(&self, code: u32) -> &[u8] {
        self.strings().get(code)
    }

    /// The strings, for many reads of [`Strings::get`]. Bytes in a mapping of the file take a call to find them, so a loop over many codes takes the strings one time.
    #[must_use]
    #[inline]
    pub fn strings(&self) -> Strings<'_> {
        Strings { bytes: self.bytes.slice(), ends: self.ends.slice().as_chunks::<8>().0 }
    }

    /// The code of `value`, or [`MISSING`]. A dictionary with a table of codes finds it in the table. A dictionary with no table, as a load reads it from the file, finds it in the last strings that it found, or else in a read of the lengths of all the strings, and makes no table: a table takes 8 bytes or more for each string for all the time that the dictionary lives, and its first find reads and hashes all the strings, but a query finds each constant one time for each vector.
    #[must_use]
    pub fn find(&self, value: &[u8]) -> u32 {
        let Some(slots) = self.slots.get() else {
            if let Some(code) = self.recent.get(value) {
                return code;
            }
            let code = self.scan(value);
            self.recent.put(value, code);
            return code;
        };
        let mask = slots.len() - 1;
        let mut at = hash::bytes(value) as usize & mask;
        loop {
            let code = slots[at];
            if code == NONE {
                return MISSING;
            }
            if self.get(code) == value {
                return code;
            }
            at = (at + 1) & mask;
        }
    }

    /// The code of `value` from a read of the end of each string, which compares the bytes of a string only when its length is the length of `value`, or [`MISSING`].
    fn scan(&self, value: &[u8]) -> u32 {
        let bytes = self.bytes.slice();
        let mut start = 0;
        for (at, end) in self.ends.slice().as_chunks::<8>().0.iter().enumerate() {
            let end = u64::from_le_bytes(*end) as usize;
            if end - start == value.len() && bytes[start..end] == *value {
                return at as u32 + 1;
            }
            start = end;
        }
        MISSING
    }

    /// The code of `value`, which the dictionary adds to the tail when it does not have it.
    ///
    /// # Panics
    ///
    /// When the dictionary already has 4,294,967,294 strings.
    pub fn intern(&mut self, value: &[u8]) -> u32 {
        let slots = self.slots();
        let mask = slots.len() - 1;
        let mut at = hash::bytes(value) as usize & mask;
        loop {
            let code = slots[at];
            if code == NONE {
                break;
            }
            if self.get(code) == value {
                return code;
            }
            at = (at + 1) & mask;
        }
        assert!(
            self.len() < (MISSING - 1) as usize,
            "a dictionary holds at most 4294967294 strings"
        );
        let bytes = self.bytes.owned_mut();
        bytes.extend_from_slice(value);
        let end = bytes.len() as u64;
        self.ends.owned_mut().extend_from_slice(&end.to_le_bytes());
        let code = self.len() as u32;
        let slots = self.slots.get_mut().map_or(0, |slots| {
            slots[at] = code;
            slots.len()
        });
        self.order = OnceLock::new();
        self.recent = Recent::default();
        if self.len() * 2 > slots {
            self.grow();
        }
        code
    }

    /// The arena, the end of each string as eight bytes little endian, and the size of the sorted base, which [`Dictionary::from_parts`] and [`Dictionary::from_shared`] take back.
    #[must_use]
    pub fn parts(&self) -> (&[u8], &[u8], u32) {
        (self.bytes.slice(), self.ends.slice(), self.base)
    }

    /// The dictionary of the parts that [`Dictionary::parts`] gave, with a new table of codes.
    ///
    /// # Errors
    ///
    /// `XX001` when an end is before the end before it or after the arena, the last end is not the end of the arena, the base has more codes than the dictionary, or the dictionary has too many strings.
    pub fn from_parts(bytes: Vec<u8>, ends: Vec<u64>, base: u32) -> Result<Dictionary> {
        let mut raw = Vec::with_capacity(ends.len() * 8);
        for end in ends {
            raw.extend_from_slice(&end.to_le_bytes());
        }
        let ends = raw;
        Dictionary::from_stores(Store::owned(bytes), Store::owned(ends), base)
    }

    /// The dictionary of an arena and of the ends of its strings as eight bytes little endian, which can be ranges of a mapping of the file. The dictionary reads them where they are, and makes its table of codes when it first finds a string.
    ///
    /// # Errors
    ///
    /// The errors of [`Dictionary::from_parts`], and `XX001` when the bytes of the ends are not a whole number of ends.
    pub fn from_shared(bytes: Bytes, ends: Bytes, base: u32) -> Result<Dictionary> {
        Dictionary::from_stores(Store::mapped(bytes), Store::mapped(ends), base)
    }

    /// The dictionary of [`Dictionary::from_parts`] and [`Dictionary::from_shared`], with no table of codes yet.
    fn from_stores(bytes: Store, ends: Store, base: u32) -> Result<Dictionary> {
        let raw = ends.slice();
        let mut last = 0;
        let ordered = raw.len().is_multiple_of(8)
            && raw.as_chunks::<8>().0.iter().all(|&end| {
                let end = u64::from_le_bytes(end);
                let after = end >= last;
                last = end;
                after
            })
            && last == bytes.slice().len() as u64;
        let len = raw.len() / 8;
        if !ordered || base as usize > len || len >= (MISSING - 1) as usize {
            return Err(Error::corrupted("a dictionary has bad string ends"));
        }
        Ok(Dictionary {
            bytes,
            ends,
            slots: OnceLock::new(),
            base,
            order: OnceLock::new(),
            recent: Recent::default(),
        })
    }

    fn grow(&mut self) {
        let size = self.slots().len() * 2;
        self.slots = OnceLock::from(self.table(size));
    }

    /// A table of codes with `size` slots, a power of two.
    fn table(&self, size: usize) -> Vec<u32> {
        let mask = size - 1;
        let mut slots = vec![NONE; size];
        let strings = self.strings();
        for code in 1..=self.len() as u32 {
            let mut at = hash::bytes(strings.get(code)) as usize & mask;
            while slots[at] != NONE {
                at = (at + 1) & mask;
            }
            slots[at] = code;
        }
        slots
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
        self.order_on(1, &|jobs| jobs.into_iter().for_each(|job| job()))
    }

    /// The places of [`Dictionary::order`], from a sort on `jobs` jobs at the same time, which `run` runs and returns when all of them ended. A sample of the strings gives the bounds of many parts, each job puts the codes of a run of the codes into their parts, and then the jobs sort the parts. The places are the places of [`Dictionary::order`]. A dictionary of few strings, or one job, sorts on the calling task.
    pub fn order_on(&self, jobs: usize, run: &Run<'_>) -> &[u32] {
        self.order.get_or_init(|| {
            let strings = self.strings();
            let codes: Vec<u32> = if jobs < 2 || self.len() < PARALLEL_ORDER {
                let mut codes: Vec<u32> = (1..=self.len() as u32).collect();
                sort_bytes(&mut codes, |c| strings.get(c));
                codes
            } else {
                self.sort_parts(jobs, run)
            };
            let mut places = vec![0; self.len() + 1];
            for (place, &code) in codes.iter().enumerate() {
                places[code as usize] = place as u32 + 1;
            }
            places.into_boxed_slice()
        })
    }

    /// The codes of all the strings in the order of their bytes, from the sort of [`Dictionary::order_on`] of more than one job.
    fn sort_parts(&self, jobs: usize, run: &Run<'_>) -> Vec<u32> {
        let n = self.len();
        let strings = self.strings();
        let parts = jobs * PARTS_OF_JOB;
        // Each part gets about `SAMPLE_OF_PART` strings of the sample, so the sizes of the parts are close.
        let step = (n / (parts * SAMPLE_OF_PART)).max(1);
        let mut sample: Vec<u32> = (1..=n as u32).step_by(step).collect();
        sort_bytes(&mut sample, |c| strings.get(c));
        let bounds: Vec<&[u8]> =
            (1..parts).map(|i| strings.get(sample[i * sample.len() / parts])).collect();
        // The part of a string is the number of bounds that are not after it.
        let chunk = n.div_ceil(jobs);
        let mut found: Vec<Vec<Vec<u32>>> = vec![Vec::new(); n.div_ceil(chunk)];
        let work: Vec<Job<'_>> = found
            .iter_mut()
            .enumerate()
            .map(|(j, out)| {
                let bounds = &bounds;
                Box::new(move || {
                    *out = vec![Vec::new(); parts];
                    let from = j * chunk + 1;
                    for code in from..(from + chunk).min(n + 1) {
                        let text = strings.get(code as u32);
                        out[bounds.partition_point(|b| *b <= text)].push(code as u32);
                    }
                }) as Job<'_>
            })
            .collect();
        run(work);
        let mut codes = Vec::with_capacity(n);
        let mut ends = Vec::with_capacity(parts);
        for part in 0..parts {
            for job in &found {
                codes.extend_from_slice(&job[part]);
            }
            ends.push(codes.len());
        }
        drop(found);
        // The jobs take the parts one at a time, so a large part does not keep the other jobs waiting.
        let mut slices = Vec::with_capacity(parts);
        let mut rest = codes.as_mut_slice();
        let mut start = 0;
        for &end in &ends {
            let (slice, after) = rest.split_at_mut(end - start);
            slices.push(slice);
            rest = after;
            start = end;
        }
        let queue = Mutex::new(slices);
        let work: Vec<Job<'_>> = (0..jobs)
            .map(|_| {
                let queue = &queue;
                Box::new(move || {
                    loop {
                        let next =
                            queue.lock().unwrap_or_else(std::sync::PoisonError::into_inner).pop();
                        let Some(slice) = next else { break };
                        sort_bytes(slice, |c| strings.get(c));
                    }
                }) as Job<'_>
            })
            .collect();
        run(work);
        codes
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

/// The strings of a [`Dictionary`], which [`Dictionary::strings`] gives.
#[derive(Clone, Copy, Debug)]
pub struct Strings<'a> {
    /// The bytes of every string, in the order of the codes.
    bytes: &'a [u8],
    /// The end of each string in `bytes`, little endian.
    ends: &'a [[u8; 8]],
}

impl<'a> Strings<'a> {
    /// The string of `code`.
    ///
    /// # Panics
    ///
    /// When `code` is not a code of the dictionary.
    #[must_use]
    #[inline(always)]
    pub fn get(self, code: u32) -> &'a [u8] {
        let at = code as usize - 1;
        let start = if at == 0 { 0 } else { u64::from_le_bytes(self.ends[at - 1]) as usize };
        &self.bytes[start..u64::from_le_bytes(self.ends[at]) as usize]
    }

    /// The bytes of every string, in the order of the codes, so that one search can read many strings.
    #[must_use]
    pub fn bytes(self) -> &'a [u8] {
        self.bytes
    }

    /// The place in [`Strings::bytes`] where the string of `code` ends, which is where the string of the next code starts. The code 0 ends at 0.
    ///
    /// # Panics
    ///
    /// When `code` is not 0 and not a code of the dictionary.
    #[must_use]
    #[inline(always)]
    pub fn end(self, code: u32) -> usize {
        match code {
            0 => 0,
            _ => u64::from_le_bytes(self.ends[code as usize - 1]) as usize,
        }
    }
}

/// A job of [`Dictionary::order_on`], which can borrow the data of the caller.
pub type Job<'a> = Box<dyn FnOnce() + Send + 'a>;

/// Runs the jobs of [`Dictionary::order_on`], and returns when all of them ended.
pub type Run<'r> = dyn for<'a> Fn(Vec<Job<'a>>) + 'r;

/// The fewest strings of a dictionary whose [`Dictionary::order_on`] sorts on many jobs.
const PARALLEL_ORDER: usize = 1 << 16;

/// The parts of [`Dictionary::order_on`] for each job.
const PARTS_OF_JOB: usize = 8;

/// The strings of the sample of [`Dictionary::order_on`] for each part.
const SAMPLE_OF_PART: usize = 32;

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
            // Eight bytes come in one read. A copy of fewer bytes, of a length that the compiler does not know, would call `memmove`.
            let mut digit = [0_u8; 8];
            match rest.first_chunk::<8>() {
                Some(first) => digit = *first,
                None => digit.iter_mut().zip(rest).for_each(|(to, &b)| *to = b),
            }
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

    /// The sort of the places on many jobs gives the places of a sort of the bytes, for texts with long common starts, texts that are the start of others, zero bytes and many equal parts of the sample.
    #[test]
    #[allow(clippy::disallowed_methods)]
    fn a_sort_on_many_jobs_is_the_sort_of_the_bytes() {
        let mut dict = Dictionary::new();
        let mut seed = 7_u64;
        for i in 0..(PARALLEL_ORDER as u32 * 2) {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            let mut text =
                format!("http://www.example.com/{}/{}", seed % 3, seed >> 44).into_bytes();
            text.truncate(12 + (seed >> 20) as usize % 40);
            if i % 5 == 0 {
                text.push(0);
            }
            dict.intern(&text);
        }
        let mut sorted: Vec<u32> = (1..=dict.len() as u32).collect();
        sorted.sort_by(|&a, &b| dict.get(a).cmp(dict.get(b)));
        let order = dict.order_on(4, &|jobs| {
            std::thread::scope(|s| {
                for job in jobs {
                    s.spawn(job);
                }
            });
        });
        assert!(dict.len() > PARALLEL_ORDER);
        for (place, &code) in sorted.iter().enumerate() {
            assert_eq!(order[code as usize], place as u32 + 1, "{:?}", dict.get(code));
        }
    }

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

    /// A dictionary from its parts finds each string, the empty string, a string that is the start of another and a string that it does not have, with no table of codes, also when the finds repeat more times than the list of the last finds holds. A new string after a find that did not find it is then found.
    #[test]
    fn a_dictionary_from_its_parts_finds_with_no_table() {
        let mut built = Dictionary::new();
        let words: Vec<Vec<u8>> = (0..3000)
            .map(|i| format!("http://example.com/{}", i * 7).into_bytes())
            .chain([b"".to_vec(), b"http".to_vec(), b"http://example.com/7x".to_vec()])
            .collect();
        for w in &words {
            built.intern(w);
        }
        let (bytes, ends, base) = built.parts();
        let ends: Vec<u64> =
            ends.as_chunks::<8>().0.iter().map(|e| u64::from_le_bytes(*e)).collect();
        let mut dict = Dictionary::from_parts(bytes.to_vec(), ends, base).unwrap();
        for round in 0..3 {
            for (c, w) in words.iter().enumerate().step_by(97 - round * 40) {
                assert_eq!(dict.find(w), c as u32 + 1, "{w:?}");
                assert_eq!(dict.find(w), c as u32 + 1, "{w:?}");
            }
        }
        assert_eq!(dict.find(b"http://example.com/"), MISSING);
        assert_eq!(dict.find(b"nope"), MISSING);
        assert!(dict.slots.get().is_none());
        assert_eq!(dict.intern(b"nope"), 3004);
        assert_eq!(dict.find(b"nope"), 3004);
        assert_eq!(dict.find(b"http"), 3002);
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

/// Bytes that a dictionary owns and can grow, or a range of a mapping of the file, which a new string first copies.
#[derive(Clone, Debug, Default)]
struct Store {
    /// The bytes in a mapping of the file, if the bytes are there.
    mapped: Option<Bytes>,
    /// The bytes that the dictionary owns, when it has no mapped bytes.
    owned: Vec<u8>,
}

impl Store {
    /// Bytes that the dictionary owns.
    fn owned(bytes: Vec<u8>) -> Store {
        Store { mapped: None, owned: bytes }
    }

    /// Bytes in a mapping of the file.
    fn mapped(bytes: Bytes) -> Store {
        Store { mapped: Some(bytes), owned: Vec::new() }
    }

    /// All the bytes.
    #[inline(always)]
    fn slice(&self) -> &[u8] {
        self.mapped.as_deref().unwrap_or(&self.owned)
    }

    /// The bytes that the process owns, which are not the bytes of a mapping of the file.
    fn size(&self) -> usize {
        let copied = self.mapped.as_ref().filter(|bytes| !bytes.is_mapped());
        self.owned.capacity() + copied.map_or(0, |bytes| bytes.len())
    }

    /// The bytes to grow, which are a copy of the mapped bytes the first time.
    fn owned_mut(&mut self) -> &mut Vec<u8> {
        if let Some(bytes) = self.mapped.take() {
            self.owned = bytes.to_vec();
        }
        &mut self.owned
    }
}
