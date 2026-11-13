// Lifted from rudb crates/rudb-graph/src/bits.rs at cd9f9676 (2026-10-05).
// Changes: errors of malformed bytes are XX001 `Error::corrupted`.

//! A bitmap, a rank index over it, and select in both directions.
//!
//! The rank index was written for the dense key map of spec/graph/03-the-file-format.md section 3.3
//! and lived inside `keymap.rs` until the monotone forward link of section 3.4 needed the same
//! structure with select added. It is here rather than there because two callers with different
//! reasons is what a module is for, and because the second caller needs an operation the first does
//! not: a key map only ever asks how many keys are below this one, and a monotone link asks where
//! the nth one is.
//!
//! The rank index is unchanged by the move, including its serialized bytes, so a file written
//! before it reads the same after. [`BitVector`] is the new part: it owns a bitmap, a rank index
//! over it, and the sampling that makes select a bounded search rather than a scan.
//!
//! # What select costs and why it is not stored
//!
//! Section 3.4 budgets "a sampled select structure of one position every four thousand ninety six
//! ones plus a two-level rank index" at about thirteen percent over the bitmap. The rank index is
//! stored, because it is what a reader without anything resident would need. What a lookup reads is
//! a finer index built at open, one count per word, and select samples one every sixty four bits of
//! each value, a pass over the bitmap and then over the counts. Neither is stored: a number
//! derivable at open is a number that should not be given the chance to disagree with the array it
//! describes, and section 4.2 of spec/graph/04-the-in-memory-model.md already holds these
//! structures resident once they are first used.

use rupg_common::{Error, Result};

/// Bits in one rank superblock.
const SUPERBLOCK_BITS: usize = 4096;

/// Bits in one rank block.
const BLOCK_BITS: usize = 512;

/// Blocks in one superblock.
const BLOCKS_PER_SUPERBLOCK: usize = SUPERBLOCK_BITS / BLOCK_BITS;

/// Words in one rank block.
const BLOCK_WORDS: usize = BLOCK_BITS / 64;

/// One select sample every this many ones, and every this many zeros.
///
/// Section 3.4 budgets one every 4096, and that is what the file would pay if the samples were
/// stored. They are not, so the number is a resident cost and not a format one, and at sixty four
/// the sample for a group is usually the word the answer is in or the one before it. With 4096 a
/// select was a binary search over superblocks, a walk of blocks and a walk of words, about two
/// hundred instructions, and TPC-H q21 asked for 292,198 of them walking each line to its siblings.
/// Four bytes every sixty four ones and every sixty four zeros is half the size of the bitmap.
const SAMPLE: u64 = 64;

/// Words between two samples past which [`BitVector::select`] searches rather than walks.
const WALK_WORDS: usize = 8;

/// A two level rank index over a bitmap.
///
/// Superblocks of 4096 bits hold a `u32` cumulative count from the start of the bitmap, and blocks
/// of 512 bits hold a `u16` count from the start of their superblock. A rank is then two loads and
/// a `popcount` over at most eight words, which is section 3.3's arithmetic and is the reason the
/// block size is 512: a `u16` cannot hold a count over a wider superblock than 4096, and eight
/// words is the most a `popcount` loop should have to do.
///
/// That is the stored index, and it is what a file pays for. What a lookup reads is `fine`, one `u32`
/// per word counting the ones before it, built from the bitmap when the index is built or read and
/// never written. Two loads and one `popcount` answer a rank from it, where the stored index is
/// three loads and a loop of up to seven more. On TPC-H q21 a key map lookup spent 77 instructions
/// in the loop version, and there were 292,198 of them. It costs four bytes for every sixty four
/// bits, half the size of the bitmap, held in memory as spec/graph/04-the-in-memory-model.md section
/// 4.2 says a key map is, and the superblocks being `u32` already caps the count at what it holds.
#[derive(Debug, Clone)]
pub(crate) struct Rank {
    superblocks: Vec<u32>,
    blocks: Vec<u16>,
    /// Ones before each word, and the total after the last one. Rebuilt, never stored.
    fine: Vec<u32>,
}

impl Rank {
    pub(crate) fn build(bits: &[u64]) -> Self {
        let blocks = bits.len().div_ceil(BLOCK_WORDS);
        let mut index = Self {
            superblocks: Vec::with_capacity(blocks.div_ceil(BLOCKS_PER_SUPERBLOCK)),
            blocks: Vec::with_capacity(blocks),
            fine: Vec::new(),
        };
        let mut total = 0_u32;
        let mut within = 0_u16;
        for block in 0..blocks {
            if block % BLOCKS_PER_SUPERBLOCK == 0 {
                index.superblocks.push(total);
                within = 0;
            }
            index.blocks.push(within);
            let words = block * BLOCK_WORDS;
            let ones: u32 = bits[words..(words + BLOCK_WORDS).min(bits.len())]
                .iter()
                .map(|word| word.count_ones())
                .sum();
            total += ones;
            // A superblock holds at most 4096 ones, so this cannot overflow a u16, and the `as` is
            // guarded by the reset above rather than by hope.
            #[expect(
                clippy::cast_possible_truncation,
                reason = "a superblock holds at most 4096 bits, which fits a u16"
            )]
            let ones = ones as u16;
            within += ones;
        }
        index.refine(bits);
        index
    }

    /// Fills `fine` from the bitmap this index is over.
    ///
    /// A pass over the words, which is what building the stored index cost too, so a reader pays
    /// for it once when the section is opened and never again. The counts wrap past `u32::MAX`
    /// ones rather than fail, and a bitmap with that many is already past what the superblocks hold.
    fn refine(&mut self, bits: &[u64]) {
        let mut fine = Vec::with_capacity(bits.len() + 1);
        let mut total = 0_u32;
        for word in bits {
            fine.push(total);
            total = total.wrapping_add(word.count_ones());
        }
        fine.push(total);
        self.fine = fine;
    }

    /// How many bits are set strictly below `at`.
    pub(crate) fn rank(&self, bits: &[u64], at: usize) -> u64 {
        let word = at / 64;
        let mask = (1_u64 << (at % 64)) - 1;
        u64::from(self.fine[word]) + u64::from((bits[word] & mask).count_ones())
    }

    /// The word holding the one of rank `nth`, counting from zero, and how many ones of that word
    /// come before it, searching from word `from` on. `None` when the bitmap has `nth` ones or
    /// fewer, or when word `from` already starts past it.
    ///
    /// The search gallops out from `from` before it halves, because the caller walks ranks that
    /// rise and the word it wants is nearly always a word or two past the last one. Halving the
    /// whole rest of the bitmap from there was seventeen dependent loads a row on the orders of
    /// TPC-H, and a tenth of q10.
    pub(crate) fn word_holding(&self, nth: u64, from: usize) -> Option<(usize, u32)> {
        let words = self.fine.len().checked_sub(1)?;
        if from >= words || u64::from(self.fine[from]) > nth || u64::from(self.fine[words]) <= nth {
            return None;
        }
        // `fine[low]` is at most `nth` and `fine[high]` is past it, or `high` is the end.
        let (mut low, mut step) = (from, 1);
        let mut high = from + 1;
        while high < words && u64::from(self.fine[high]) <= nth {
            low = high;
            step *= 2;
            high = (low + step).min(words);
        }
        let word = low + self.fine[low..high].partition_point(|&ones| u64::from(ones) <= nth) - 1;
        #[expect(
            clippy::cast_possible_truncation,
            reason = "the rank is inside the word, which holds at most sixty four ones"
        )]
        let within = (nth - u64::from(self.fine[word])) as u32;
        Some((word, within))
    }

    /// Ones strictly before a word, for any word up to one past the last.
    fn ones_before_word(&self, word: usize) -> u64 {
        u64::from(self.fine[word])
    }

    pub(crate) fn bytes(&self) -> usize {
        self.superblocks.len() * size_of::<u32>() + self.blocks.len() * size_of::<u16>()
    }

    /// How many blocks and superblocks index a bitmap of this many words.
    ///
    /// Derived rather than stored, because both counts are a function of the range the header
    /// already carries and a stored count is a count that can disagree with the array it describes.
    pub(crate) fn shape(words: usize) -> (usize, usize) {
        let blocks = words.div_ceil(BLOCK_WORDS);
        (blocks, blocks.div_ceil(BLOCKS_PER_SUPERBLOCK))
    }

    pub(crate) fn write(&self, out: &mut Vec<u8>) {
        for count in &self.superblocks {
            out.extend_from_slice(&count.to_le_bytes());
        }
        for offset in &self.blocks {
            out.extend_from_slice(&offset.to_le_bytes());
        }
    }

    /// Reads an index over `bits` from exactly the bytes it takes.
    pub(crate) fn read(bytes: &[u8], bits: &[u64]) -> Result<Self> {
        let words = bits.len();
        let (blocks, superblocks) = Self::shape(words);
        let split = superblocks * size_of::<u32>();
        if bytes.len() != split + blocks * size_of::<u16>() {
            return Err(malformed(
                "a dense key map's rank index is not the size its range implies",
            ));
        }
        let mut index = Self {
            superblocks: bytes[..split]
                .as_chunks::<4>()
                .0
                .iter()
                .map(|word| u32::from_le_bytes(*word))
                .collect(),
            blocks: bytes[split..]
                .as_chunks::<2>()
                .0
                .iter()
                .map(|word| u16::from_le_bytes(*word))
                .collect(),
            fine: Vec::new(),
        };
        index.refine(bits);
        Ok(index)
    }
}

/// A bitmap with rank and select, in both directions.
///
/// The monotone forward link of section 3.4 asks all four questions of one vector: `rank0` and
/// `select1` answer `forward`, and `select0` answers `backward`. That is the whole reason the
/// monotone form replaces both a forward link and a backward adjacency rather than only the
/// forward one.
#[derive(Debug, Clone)]
pub struct BitVector {
    words: Vec<u64>,
    /// Bits that mean something. The tail of the last word is zero and is not one of them.
    len: usize,
    ones: u64,
    rank: Rank,
    /// Word holding the `SAMPLE * k`th one, for each `k`. Rebuilt at read, never stored.
    ones_sample: Vec<u32>,
    /// The same for zeros.
    zeros_sample: Vec<u32>,
}

impl BitVector {
    /// Builds a vector over `len` bits held in `words`, least significant bit of word zero first.
    ///
    /// # Errors
    ///
    /// If `words` is not exactly the number of words `len` bits take, or if a bit is set in the
    /// tail past `len`. The second one matters: a set tail bit is counted by `rank` and found by
    /// `select`, so accepting it would mean a vector that answers questions about bits nobody
    /// wrote.
    pub fn new(words: Vec<u64>, len: usize) -> Result<Self> {
        if words.len() != len.div_ceil(64) {
            return Err(malformed("a bit vector's words do not match its length"));
        }
        let tail = len % 64;
        if tail != 0 && words[len / 64] >> tail != 0 {
            return Err(malformed("a bit vector has bits set past its length"));
        }
        let rank = Rank::build(&words);
        let ones = words.iter().map(|word| u64::from(word.count_ones())).sum();
        let mut vector =
            Self { words, len, ones, rank, ones_sample: Vec::new(), zeros_sample: Vec::new() };
        vector.sample();
        Ok(vector)
    }

    /// Bits in the vector.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the vector has no bits at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Bits set.
    #[must_use]
    pub fn ones(&self) -> u64 {
        self.ones
    }

    /// Bits clear, within the length.
    #[must_use]
    pub fn zeros(&self) -> u64 {
        bits(self.len) - self.ones
    }

    /// Bits set strictly below `at`.
    #[must_use]
    pub fn rank1(&self, at: usize) -> u64 {
        if at >= self.len {
            return self.ones;
        }
        self.rank.rank(&self.words, at)
    }

    /// Bits clear strictly below `at`.
    #[must_use]
    pub fn rank0(&self, at: usize) -> u64 {
        let at = at.min(self.len);
        bits(at) - self.rank1(at)
    }

    /// Where the `nth` set bit is, counting from zero, or `None` if there are not that many.
    #[must_use]
    pub fn select1(&self, nth: u64) -> Option<usize> {
        if nth >= self.ones {
            return None;
        }
        Some(self.select(nth, true))
    }

    /// Where the `nth` clear bit is, counting from zero, or `None` if there are not that many.
    #[must_use]
    pub fn select0(&self, nth: u64) -> Option<usize> {
        if nth >= self.zeros() {
            return None;
        }
        Some(self.select(nth, false))
    }

    /// The bitmap itself, least significant bit of word zero first.
    ///
    /// For a caller that walks the bits in order, which is one load per sixty four of them, where
    /// asking `select` for each would be a search per bit.
    pub(crate) fn words(&self) -> &[u64] {
        &self.words
    }

    /// Bytes this costs on disk, which is the bitmap and the rank index and not the samples.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.words.len() * size_of::<u64>() + self.rank.bytes()
    }

    /// Bytes [`BitVector::write`] produces for a vector of `len` bits, whatever its bits are.
    ///
    /// For a payload that puts something after a vector and has to find where it starts.
    #[must_use]
    pub fn bytes_for(len: usize) -> usize {
        let words = len.div_ceil(64);
        let (blocks, superblocks) = Rank::shape(words);
        words * size_of::<u64>() + superblocks * size_of::<u32>() + blocks * size_of::<u16>()
    }

    /// Appends the bitmap and its rank index.
    pub fn write(&self, out: &mut Vec<u8>) {
        for word in &self.words {
            out.extend_from_slice(&word.to_le_bytes());
        }
        self.rank.write(out);
    }

    /// Reads a vector of `len` bits from exactly the bytes [`BitVector::write`] produced.
    ///
    /// The rank index is read rather than rebuilt, and then it is not checked against the bitmap.
    /// Checking would be the pass that reading it was meant to avoid. A torn index is a wrong
    /// answer from a structure section 3.1 says can be deleted without changing any answer, so the
    /// protection that matters is the section checksum above this layer, not a recount here.
    ///
    /// # Errors
    ///
    /// If the bytes are not the length `len` implies, or if the tail past `len` is not zero.
    ///
    /// # Panics
    ///
    /// It does not. `chunks_exact` hands out eight bytes and the conversion to an eight byte array
    /// is the one the compiler cannot see through.
    pub fn read(bytes: &[u8], len: usize) -> Result<Self> {
        let words = len.div_ceil(64);
        let bitmap = words * size_of::<u64>();
        if bytes.len() < bitmap {
            return Err(malformed("a bit vector is shorter than its length implies"));
        }
        let held = bytes[..bitmap]
            .as_chunks::<8>()
            .0
            .iter()
            .map(|word| u64::from_le_bytes(*word))
            .collect::<Vec<u64>>();
        let rank = Rank::read(&bytes[bitmap..], &held)?;
        let tail = len % 64;
        if tail != 0 && held[len / 64] >> tail != 0 {
            return Err(malformed("a bit vector has bits set past its length"));
        }
        let ones = held.iter().map(|word| u64::from(word.count_ones())).sum();
        let mut vector = Self {
            words: held,
            len,
            ones,
            rank,
            ones_sample: Vec::new(),
            zeros_sample: Vec::new(),
        };
        vector.sample();
        Ok(vector)
    }

    /// One word index per `SAMPLE` ones, and one per `SAMPLE` zeros.
    ///
    /// A pass over the per word counts, which is why this is cheap enough to do at read rather
    /// than store. The sample for group `k` is the word a search for the `k * SAMPLE`th bit would
    /// land in, so the answer for any `nth` in that group lies between the sample for the group and
    /// the sample for the next one, and both counts being non-decreasing in the word index is what
    /// makes one forward walk enough to fill either array.
    fn sample(&mut self) {
        self.ones_sample = self.samples(true, self.ones);
        self.zeros_sample = self.samples(false, self.zeros());
    }

    fn samples(&self, set: bool, total: u64) -> Vec<u32> {
        let words = self.words.len();
        let mut sample = Vec::with_capacity(usize::try_from(total.div_ceil(SAMPLE)).unwrap_or(0));
        let mut at = 0_usize;
        for group in 0..total.div_ceil(SAMPLE) {
            let target = group * SAMPLE;
            while at + 1 < words && self.before(set, at + 1) <= target {
                at += 1;
            }
            sample.push(u32::try_from(at).unwrap_or(u32::MAX));
        }
        sample
    }

    /// Bits of the given value strictly before a word's first bit.
    fn before(&self, set: bool, word: usize) -> u64 {
        let ones = self.rank.ones_before_word(word);
        if set { ones } else { bits(word * 64) - ones }
    }

    /// Where the `nth` bit of the given value is.
    ///
    /// The sample for `nth`'s group and the one after it bound the word it is in. On a vector as
    /// dense in the value asked for as the monotone link's ones or a key map's bits, those are a
    /// word or two apart and the walk between them is a load and a compare each. A sparse one can
    /// put many words between two samples, and there a binary search over the per word counts
    /// finds the word instead. The counts for zeros are the counts for ones subtracted from the
    /// position, which is why one routine answers both.
    fn select(&self, nth: u64, set: bool) -> usize {
        let words = self.words.len();
        if words == 0 {
            return self.len;
        }
        let samples = if set { &self.ones_sample } else { &self.zeros_sample };
        let group = usize::try_from(nth / SAMPLE).unwrap_or(usize::MAX);
        let from = samples.get(group).map_or(0, |at| *at as usize);
        let to = samples.get(group + 1).map_or(words - 1, |at| *at as usize);
        let mut word = from;
        if to - from > WALK_WORDS {
            // The last word whose count of bits before it is at most `nth`.
            let (mut low, mut high) = (from, to);
            while low < high {
                let middle = low + (high - low).div_ceil(2);
                if self.before(set, middle) <= nth {
                    low = middle;
                } else {
                    high = middle - 1;
                }
            }
            word = low;
        } else {
            while word < to && self.before(set, word + 1) <= nth {
                word += 1;
            }
        }
        let held = if set { self.words[word] } else { !self.words[word] };
        let before = self.before(set, word);
        if nth < before || nth - before >= u64::from(held.count_ones()) {
            // Unreachable for an `nth` the callers checked against `ones` or `zeros`, and a
            // saturating answer rather than a panic if it ever is reached, because section 3.1 says
            // this layer is allowed to be slow and is not allowed to take the process down.
            return self.len;
        }
        #[expect(
            clippy::cast_possible_truncation,
            reason = "under the count of one word, so under sixty four"
        )]
        let offset = (nth - before) as u32;
        word * 64 + nth_set(held, offset) as usize
    }
}

/// A bit count as a `u64`, which is what every count in here is compared against.
///
/// A position is a `usize` because it indexes a bitmap and a count is a `u64` because it is written
/// to a file, and the conversion between them is one place rather than nine.
fn bits(at: usize) -> u64 {
    u64::try_from(at).unwrap_or(u64::MAX)
}

/// Where the `nth` set bit of a word is, counting from zero.
///
/// A byte at a time to the byte that holds it and then the lowest set bit cleared until it is
/// reached, which is at most eight steps and seven, where clearing bits across the whole word was
/// up to sixty three. `pdep` does this in one instruction on x86 and there is no portable way to
/// say so yet. A word with `nth` ones or fewer answers 64, as the loop over the word did.
pub(crate) fn nth_set(word: u64, nth: u32) -> u32 {
    let mut left = nth;
    for (at, byte) in (0_u32..).zip(word.to_le_bytes()) {
        let ones = byte.count_ones();
        if left < ones {
            let mut byte = byte;
            for _ in 0..left {
                byte &= byte - 1;
            }
            return at * 8 + byte.trailing_zeros();
        }
        left -= ones;
    }
    64
}

fn malformed(message: impl Into<String>) -> Error {
    Error::corrupted(format!("invalid bit vector: {}", message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_nth_set_bit_is_where_clearing_the_lower_ones_puts_it() {
        let mut word = 0x9e37_79b9_7f4a_7c15_u64;
        for _ in 0..200 {
            word = word.rotate_left(7) ^ word.wrapping_mul(0x2545_f491_4f6c_dd1d);
            for nth in 0..=64 {
                let mut cleared = word;
                for _ in 0..nth {
                    cleared &= cleared.wrapping_sub(1);
                }
                assert_eq!(nth_set(word, nth), cleared.trailing_zeros(), "{word:#x} {nth}");
            }
        }
    }

    /// Builds a vector from a list of bits, the slow and obviously correct way.
    fn vector(bits: &[bool]) -> BitVector {
        let mut words = vec![0_u64; bits.len().div_ceil(64)];
        for (at, bit) in bits.iter().enumerate() {
            if *bit {
                words[at / 64] |= 1 << (at % 64);
            }
        }
        BitVector::new(words, bits.len()).expect("build")
    }

    /// Checks rank and select against counting by hand, at every position.
    fn agrees(bits: &[bool]) {
        let built = vector(bits);
        let (mut ones, mut zeros) = (Vec::new(), Vec::new());
        for (at, bit) in bits.iter().enumerate() {
            assert_eq!(built.rank1(at), ones.len() as u64, "rank1 at {at}");
            assert_eq!(built.rank0(at), zeros.len() as u64, "rank0 at {at}");
            if *bit { ones.push(at) } else { zeros.push(at) }
        }
        assert_eq!(built.ones(), ones.len() as u64);
        assert_eq!(built.zeros(), zeros.len() as u64);
        for (nth, at) in ones.iter().enumerate() {
            assert_eq!(built.select1(nth as u64), Some(*at), "select1 of {nth}");
        }
        for (nth, at) in zeros.iter().enumerate() {
            assert_eq!(built.select0(nth as u64), Some(*at), "select0 of {nth}");
        }
        assert_eq!(built.select1(ones.len() as u64), None, "there is no one past the last");
        assert_eq!(built.select0(zeros.len() as u64), None, "there is no zero past the last");
    }

    #[test]
    fn an_empty_vector_answers_nothing_rather_than_panicking() {
        let built = vector(&[]);
        assert!(built.is_empty());
        assert_eq!(built.ones(), 0);
        assert_eq!(built.zeros(), 0);
        assert_eq!(built.select1(0), None);
        assert_eq!(built.select0(0), None);
        assert_eq!(built.rank1(0), 0);
        assert_eq!(built.rank0(9), 0);
    }

    #[test]
    fn a_vector_of_one_bit_each_way_agrees_with_counting() {
        agrees(&[true]);
        agrees(&[false]);
    }

    #[test]
    fn alternating_bits_agree_with_counting_across_a_word_boundary() {
        agrees(&(0..200).map(|at| at % 2 == 0).collect::<Vec<bool>>());
    }

    #[test]
    fn a_vector_longer_than_a_superblock_agrees_with_counting() {
        // 4096 bits is one superblock and eight blocks, so this is the first size where every level
        // of the index is exercised and the sample has more than one entry to choose between.
        agrees(&(0..10_000).map(|at| at % 7 == 0).collect::<Vec<bool>>());
    }

    #[test]
    fn a_vector_that_is_almost_all_ones_agrees_with_counting() {
        // The sparse direction of each search. With one zero every thousand bits, a select0 sample
        // spans many superblocks, which is the case the binary search inside the window exists for.
        agrees(&(0..20_000).map(|at| at % 1000 != 0).collect::<Vec<bool>>());
    }

    #[test]
    fn a_vector_that_is_almost_all_zeros_agrees_with_counting() {
        agrees(&(0..20_000).map(|at| at % 1000 == 0).collect::<Vec<bool>>());
    }

    #[test]
    fn a_vector_of_all_ones_and_one_of_all_zeros_both_agree() {
        agrees(&vec![true; 5000]);
        agrees(&vec![false; 5000]);
    }

    #[test]
    fn a_run_of_ones_longer_than_the_select_sample_is_found() {
        // Section 3.4's monotone link puts one run of ones per parent, and a parent with more than
        // four thousand ninety six children puts a whole sample group inside one run.
        let mut bits = vec![false; 3];
        bits.extend(std::iter::repeat_n(true, 9000));
        bits.push(false);
        agrees(&bits);
    }

    #[test]
    fn rank_past_the_end_saturates_rather_than_reading_past_it() {
        let built = vector(&[true, false, true]);
        assert_eq!(built.rank1(3), 2);
        assert_eq!(built.rank1(9999), 2);
        assert_eq!(built.rank0(9999), 1);
    }

    #[test]
    fn a_vector_survives_being_written_and_read_back() {
        let bits = (0..5000).map(|at| at % 3 == 0).collect::<Vec<bool>>();
        let built = vector(&bits);
        let mut bytes = Vec::new();
        built.write(&mut bytes);
        assert_eq!(bytes.len(), built.bytes(), "bytes() is what write() writes");
        let read = BitVector::read(&bytes, bits.len()).expect("read");
        assert_eq!(read.ones(), built.ones());
        for nth in 0..read.ones() {
            assert_eq!(read.select1(nth), built.select1(nth));
        }
        for nth in 0..read.zeros() {
            assert_eq!(read.select0(nth), built.select0(nth));
        }
    }

    #[test]
    fn a_word_count_that_does_not_match_the_length_is_refused() {
        assert!(BitVector::new(vec![0; 2], 64).is_err());
        assert!(BitVector::new(vec![0; 1], 65).is_err());
    }

    #[test]
    fn a_bit_set_past_the_length_is_refused_rather_than_counted() {
        // It would be counted by rank and found by select, so a vector that accepted it would give
        // answers about a bit nobody wrote.
        assert!(BitVector::new(vec![1 << 40], 8).is_err());
        let mut bytes = Vec::new();
        vector(&[true, false, true]).write(&mut bytes);
        bytes[0] |= 1 << 4;
        assert!(BitVector::read(&bytes, 3).is_err());
    }

    #[test]
    fn a_truncated_vector_is_refused_rather_than_read_past() {
        let mut bytes = Vec::new();
        vector(&(0..5000).map(|at| at % 3 == 0).collect::<Vec<bool>>()).write(&mut bytes);
        assert!(BitVector::read(&bytes[..bytes.len() - 1], 5000).is_err());
        assert!(BitVector::read(&bytes[..4], 5000).is_err());
    }

    #[test]
    fn the_rank_index_costs_about_an_eighth_of_the_bitmap() {
        // Section 3.4 budgets the whole select structure at about thirteen percent. The samples are
        // not stored, so what a file pays is this.
        let built = vector(&(0..1_000_000).map(|at| at % 5 == 0).collect::<Vec<bool>>());
        let bitmap = 1_000_000 / 8;
        assert!(built.bytes() > bitmap, "{} is not more than {bitmap}", built.bytes());
        assert!(
            built.bytes() < bitmap * 6 / 5,
            "{} is more than a fifth over {bitmap}",
            built.bytes()
        );
    }
}
