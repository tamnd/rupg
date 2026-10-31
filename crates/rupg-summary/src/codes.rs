//! The code set of a segment column: the dictionary codes present in its non-null values (spec/09 section 9.7.3).
//!
//! The set has one container for each range of 65,536 codes, in the layout of Roaring bitmaps (Chambi, Lemire, Kaser and Godin, 2016): a sorted array when the range has at most 4,096 codes, else a bitmap of 8 KiB, and runs when runs are smaller than both.

use rupg_common::{Error, Result};

/// The most codes in an array container. Above it, a bitmap of 8 KiB is smaller.
const ARRAY_MAX: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq)]
enum Container {
    /// The low 16 bits of each code, sorted, with no repeat.
    Array(Vec<u16>),
    /// One bit for each of the 65,536 low values.
    Bitmap(Box<[u64; 1024]>),
    /// Runs of contiguous low values, each as the first value and the length minus one.
    Runs(Vec<(u16, u16)>),
}

impl Container {
    fn len(&self) -> usize {
        match self {
            Container::Array(values) => values.len(),
            Container::Bitmap(words) => words.iter().map(|w| w.count_ones() as usize).sum(),
            Container::Runs(runs) => runs.iter().map(|&(_, n)| usize::from(n) + 1).sum(),
        }
    }

    fn contains(&self, low: u16) -> bool {
        match self {
            Container::Array(values) => values.binary_search(&low).is_ok(),
            Container::Bitmap(words) => words[usize::from(low >> 6)] >> (low & 63) & 1 == 1,
            Container::Runs(runs) => {
                let at = runs.partition_point(|&(start, _)| start <= low);
                at > 0 && {
                    let (start, n) = runs[at - 1];
                    u32::from(low) <= u32::from(start) + u32::from(n)
                }
            }
        }
    }

    fn for_each(&self, mut emit: impl FnMut(u16)) {
        match self {
            Container::Array(values) => values.iter().for_each(|&v| emit(v)),
            Container::Bitmap(words) => {
                for (index, &word) in words.iter().enumerate() {
                    let mut word = word;
                    while word != 0 {
                        emit((index as u16) << 6 | word.trailing_zeros() as u16);
                        word &= word - 1;
                    }
                }
            }
            Container::Runs(runs) => {
                for &(start, n) in runs {
                    for v in 0..=n {
                        emit(start + v);
                    }
                }
            }
        }
    }

    /// The container of the sorted low values, in the smallest form.
    fn from_sorted(values: Vec<u16>) -> Container {
        let mut runs: Vec<(u16, u16)> = Vec::new();
        for &v in &values {
            match runs.last_mut() {
                Some((start, n)) if u32::from(*start) + u32::from(*n) + 1 == u32::from(v) => {
                    *n += 1
                }
                _ => runs.push((v, 0)),
            }
        }
        let run_bytes = 4 * runs.len();
        let array_bytes = 2 * values.len();
        if run_bytes < array_bytes.min(8192) {
            Container::Runs(runs)
        } else if values.len() <= ARRAY_MAX {
            Container::Array(values)
        } else {
            let mut words = Box::new([0u64; 1024]);
            for v in values {
                words[usize::from(v >> 6)] |= 1 << (v & 63);
            }
            Container::Bitmap(words)
        }
    }

    fn encoded_len(&self) -> usize {
        match self {
            Container::Array(values) => 2 * values.len(),
            Container::Bitmap(_) => 8192,
            Container::Runs(runs) => 4 * runs.len(),
        }
    }
}

/// A set of 32-bit dictionary codes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CodeSet {
    /// The containers, by the high 16 bits of their codes, in order.
    containers: Vec<(u16, Container)>,
}

impl CodeSet {
    /// The set of `codes`, which may be in any order and repeat.
    #[must_use]
    pub fn from_codes(codes: &[u32]) -> CodeSet {
        let mut sorted = codes.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        CodeSet::from_sorted(&sorted)
    }

    /// The set of `codes`, sorted with no repeat.
    #[must_use]
    pub fn from_sorted(codes: &[u32]) -> CodeSet {
        let mut containers = Vec::new();
        let mut at = 0;
        while at < codes.len() {
            let high = (codes[at] >> 16) as u16;
            let end = at + codes[at..].partition_point(|&c| (c >> 16) as u16 == high);
            let low: Vec<u16> = codes[at..end].iter().map(|&c| c as u16).collect();
            containers.push((high, Container::from_sorted(low)));
            at = end;
        }
        CodeSet { containers }
    }

    /// The number of codes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.containers.iter().map(|(_, c)| c.len()).sum()
    }

    /// True when the set has no code.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.containers.is_empty()
    }

    /// True when the set has `code`.
    #[must_use]
    pub fn contains(&self, code: u32) -> bool {
        let high = (code >> 16) as u16;
        self.containers
            .binary_search_by_key(&high, |(h, _)| *h)
            .is_ok_and(|at| self.containers[at].1.contains(code as u16))
    }

    /// Calls `emit` for each code in ascending order.
    pub fn for_each(&self, mut emit: impl FnMut(u32)) {
        for (high, container) in &self.containers {
            let base = u32::from(*high) << 16;
            container.for_each(|low| emit(base | u32::from(low)));
        }
    }

    /// The codes in ascending order.
    #[must_use]
    pub fn to_vec(&self) -> Vec<u32> {
        let mut out = Vec::with_capacity(self.len());
        self.for_each(|c| out.push(c));
        out
    }

    /// The bytes of [`CodeSet::write`], without writing them. The column writer stores a set only when this is at most one eighth of the encoded code stream (spec/09 section 9.7.3).
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        4 + self.containers.iter().map(|(_, c)| 5 + c.encoded_len()).sum::<usize>()
    }

    /// Writes the set: the container count, then for each container its high bits, its form and its length, then its body, all little endian.
    pub fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&(self.containers.len() as u32).to_le_bytes());
        for (high, container) in &self.containers {
            out.extend_from_slice(&high.to_le_bytes());
            match container {
                Container::Array(values) => {
                    out.push(0);
                    out.extend_from_slice(&(values.len() as u16).wrapping_sub(1).to_le_bytes());
                    values.iter().for_each(|v| out.extend_from_slice(&v.to_le_bytes()));
                }
                Container::Bitmap(words) => {
                    out.push(1);
                    out.extend_from_slice(&0u16.to_le_bytes());
                    words.iter().for_each(|w| out.extend_from_slice(&w.to_le_bytes()));
                }
                Container::Runs(runs) => {
                    out.push(2);
                    out.extend_from_slice(&(runs.len() as u16).wrapping_sub(1).to_le_bytes());
                    for (start, n) in runs {
                        out.extend_from_slice(&start.to_le_bytes());
                        out.extend_from_slice(&n.to_le_bytes());
                    }
                }
            }
        }
    }

    /// Reads a set that [`CodeSet::write`] wrote, from the start of `bytes`, and returns it with the bytes it used.
    ///
    /// # Errors
    ///
    /// `XX001` when the bytes are not a set.
    pub fn read(bytes: &[u8]) -> Result<(CodeSet, usize)> {
        let mut at = 0;
        let take = |at: &mut usize, n: usize| -> Result<&[u8]> {
            let slice = bytes
                .get(*at..*at + n)
                .ok_or_else(|| Error::corrupted("a code set is shorter than its header says"))?;
            *at += n;
            Ok(slice)
        };
        let u16_at = |s: &[u8], i: usize| u16::from_le_bytes([s[2 * i], s[2 * i + 1]]);
        let head = take(&mut at, 4)?;
        let count = u32::from_le_bytes([head[0], head[1], head[2], head[3]]) as usize;
        if count > 65_536 {
            return Err(Error::corrupted("a code set has more than 65536 containers"));
        }
        let mut containers: Vec<(u16, Container)> = Vec::with_capacity(count);
        for _ in 0..count {
            let head = take(&mut at, 5)?;
            let high = u16::from_le_bytes([head[0], head[1]]);
            let len = usize::from(u16::from_le_bytes([head[3], head[4]])) + 1;
            if containers.last().is_some_and(|(h, _)| *h >= high) {
                return Err(Error::corrupted("the containers of a code set are not in order"));
            }
            let container = match head[2] {
                0 => {
                    let body = take(&mut at, 2 * len)?;
                    let values: Vec<u16> = (0..len).map(|i| u16_at(body, i)).collect();
                    if values.windows(2).any(|w| w[0] >= w[1]) {
                        return Err(Error::corrupted(
                            "an array container of a code set is not sorted",
                        ));
                    }
                    Container::Array(values)
                }
                1 => {
                    let body = take(&mut at, 8192)?;
                    let mut words = Box::new([0u64; 1024]);
                    for (w, chunk) in words.iter_mut().zip(body.as_chunks::<8>().0) {
                        *w = u64::from_le_bytes(*chunk);
                    }
                    Container::Bitmap(words)
                }
                2 => {
                    let body = take(&mut at, 4 * len)?;
                    let runs: Vec<(u16, u16)> =
                        (0..len).map(|i| (u16_at(body, 2 * i), u16_at(body, 2 * i + 1))).collect();
                    let mut end = -1i64;
                    for &(start, n) in &runs {
                        if i64::from(start) <= end || u32::from(start) + u32::from(n) > 65_535 {
                            return Err(Error::corrupted(
                                "the runs of a code set overlap or pass the range",
                            ));
                        }
                        end = i64::from(start) + i64::from(n);
                    }
                    Container::Runs(runs)
                }
                form => {
                    return Err(Error::corrupted(format!(
                        "a code set container has the unknown form {form}"
                    )));
                }
            };
            containers.push((high, container));
        }
        Ok((CodeSet { containers }, at))
    }
}

/// The union of code sets, for `count(DISTINCT x)` over many segments (spec/09 section 9.7.3).
#[derive(Clone, Debug, Default)]
pub struct Union {
    /// One bit for each code, in words of 64 codes. A dictionary of `URL` with 27 million codes needs 3.4 MB.
    words: Vec<u64>,
    count: u64,
}

impl Union {
    /// An empty union.
    #[must_use]
    pub fn new() -> Union {
        Union::default()
    }

    /// Adds one code.
    pub fn insert(&mut self, code: u32) {
        let index = (code >> 6) as usize;
        if index >= self.words.len() {
            self.words.resize(index + 1, 0);
        }
        let bit = 1u64 << (code & 63);
        if self.words[index] & bit == 0 {
            self.words[index] |= bit;
            self.count += 1;
        }
    }

    /// Adds every code of `set`.
    pub fn add(&mut self, set: &CodeSet) {
        set.for_each(|code| self.insert(code));
    }

    /// The number of codes.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.count
    }

    /// True when the union has no code.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(codes: &[u32]) -> CodeSet {
        let set = CodeSet::from_codes(codes);
        let mut bytes = Vec::new();
        set.write(&mut bytes);
        assert_eq!(bytes.len(), set.encoded_len());
        let (back, used) = CodeSet::read(&bytes).unwrap();
        assert_eq!(used, bytes.len());
        assert_eq!(back, set);
        back
    }

    #[test]
    fn every_form_round_trips() {
        let sparse: Vec<u32> = (0..100).map(|i| i * 977 + 3).collect();
        let dense: Vec<u32> = (0..60_000).filter(|i| i % 3 != 0).collect();
        let runs: Vec<u32> = (70_000..80_000).chain(200_000..200_010).collect();
        for codes in [&sparse[..], &dense[..], &runs[..], &[][..], &[u32::MAX - 1][..]] {
            let set = round_trip(codes);
            let mut expected = codes.to_vec();
            expected.sort_unstable();
            assert_eq!(set.to_vec(), expected);
            assert_eq!(set.len(), expected.len());
            for &c in codes {
                assert!(set.contains(c));
            }
            assert!(!set.contains(0));
        }
        let set = CodeSet::from_codes(&dense);
        assert!(matches!(set.containers[0].1, Container::Bitmap(_)));
        let set = CodeSet::from_codes(&runs);
        assert!(set.containers.iter().all(|(_, c)| matches!(c, Container::Runs(_))));
    }

    #[test]
    fn a_union_counts_each_code_once() {
        let mut union = Union::new();
        union.add(&CodeSet::from_codes(&[1, 2, 3, 100_000]));
        union.add(&CodeSet::from_codes(&[3, 4, 100_000]));
        assert_eq!(union.len(), 5);
    }

    #[test]
    fn bad_bytes_are_an_error() {
        let mut bytes = Vec::new();
        CodeSet::from_codes(&[1, 2, 9]).write(&mut bytes);
        for len in 0..bytes.len() {
            assert!(CodeSet::read(&bytes[..len]).is_err());
        }
        bytes[6] = 7;
        assert!(CodeSet::read(&bytes).is_err());
    }
}
