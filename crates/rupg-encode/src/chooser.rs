//! How the encoder decides which candidate to keep.
//!
//! The encoders in [`crate::string`] and [`crate::integer`] are the work. This is the search. They
//! are separate things and until now they were the same thing, because `encode` both offered every
//! candidate and encoded every candidate it offered, and there was no way to have one without the
//! other.
//!
//! # Why it is worth separating
//!
//! `cargo xtask encode` over a million rows of ClickBench `hits` says where the encoder's seconds
//! go. String `FRONT` is 32.2 percent of them and is kept once in 142 chunks. String `FSST` is 11.2
//! percent and is kept twice in 223. Integer `DELTA` is 10.8 percent and is kept never in 607.
//! String `PLAIN` is 8.7 percent and is kept four times in 223. Those four are 62.9 percent of the
//! encoder's time and they were kept seven times out of 1,195 offers.
//!
//! That is not a bug in any encoder. It is what an exhaustive search costs, and the search is worth
//! something: the shapes it arrives at are five to one on `hits` and nobody wrote them down in
//! advance. The question is how much of the search is needed, which is a question about the data
//! and therefore a question to measure rather than argue about. F2 asks for exactly this, as "the
//! encoder chooser as a seam, with exhaustive and sampled implementations", with the ablation being
//! how much size the sampled one gives up.
//!
//! # What a chooser sees and what it does not
//!
//! A chooser is asked once per chunk per level of the cascade, never once per value. It is handed
//! the values and the candidates that apply and it returns the ones worth encoding in full. It
//! cannot invent a candidate that does not apply, so nothing it does can produce a chunk that will
//! not decode, and the worst a bad chooser can do is pick a bigger encoding than another one would
//! have. That is the property that makes this safe to swap.
//!
//! # Not a `rudb-seam` seam yet, and why
//!
//! `SeamId::StorageEncoder` exists and says "how a block of values is encoded on the way to disk",
//! and this is what belongs behind it. It cannot be registered here: `rudb-seam` is rank 2 and so is
//! this crate, so the `Strategy` supertrait every seam trait needs is not visible from here. The
//! registry goes in `rudb-storage` at rank 5, next to the write path, and there is no write path
//! yet. Until there is, this is a plain trait with two implementations and an ablation, which is
//! the part that can be measured today.

// Lifted from rudb crates/rudb-encoding/src/chooser.rs at cd9f9676 (2026-10-05).
// Changes: the error type is rupg_common::Error.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};

use crate::fsst::SymbolTable;
use crate::{integer, string};

/// Which of the candidates that apply are worth encoding in full.
///
/// Crossed once per chunk per level of the cascade. No method here sees a single value on its own,
/// which is the rule that lets the decision be indirect at all.
pub trait Chooser: std::fmt::Debug + Sync {
    /// The name that goes in a report.
    fn name(&self) -> &'static str;

    /// Which of `offered` to encode in full, for a chunk of strings at `depth`.
    ///
    /// `offered` is what applies, in the order the exhaustive chooser would try them. The return
    /// has to be a subset of it and has to be non empty, because a chunk with no candidate is a
    /// chunk that cannot be written.
    fn narrow_strings(
        &self,
        values: &[&[u8]],
        offered: &[string::Kind],
        depth: u8,
    ) -> Vec<string::Kind>;

    /// Which of `offered` to encode in full, for a chunk of integers at `depth`.
    fn narrow_integers(
        &self,
        values: &[i64],
        offered: &[integer::Kind],
        depth: u8,
    ) -> Vec<integer::Kind>;

    /// Whether `kind` can ever be in what [`Chooser::narrow_integers`] returns at `depth`.
    ///
    /// Asked before the candidates are worked out, so a kind this rules out is never tested for.
    /// That matters because the test is not free: finding out whether a dictionary or a sparse
    /// encoding applies used to sort a copy of the chunk, at every level of the cascade, for a
    /// chooser that was going to throw both away. Saying yes to a kind that is then dropped only
    /// costs the test. Saying no to a kind the narrowing would have kept changes what gets written,
    /// so the default is yes and an implementation only says no where its narrowing always would.
    fn considers_integer(&self, kind: integer::Kind, depth: u8) -> bool {
        let _ = (kind, depth);
        true
    }

    /// A symbol table already trained for FSST at `depth`, or `None` to train one on the chunk.
    ///
    /// Training is five passes over a sample of up to 64 KiB, and a caller encoding thousands of
    /// chunks cut out of one column trains the same table thousands of times. Such a caller trains
    /// it once over the column and hands it out here. The table travels with every chunk either
    /// way, so a chunk compressed against a table it was not trained on still reads back.
    fn symbols(&self, depth: u8) -> Option<&SymbolTable> {
        let _ = depth;
        None
    }
}

/// Encode every candidate that applies and keep the smallest.
///
/// The reference, and what `encode` has always done. It is the thing to beat rather than the thing
/// to ship: every size this crate has ever reported came out of it, so an alternative's ablation is
/// against this and a build that wants the old bytes exactly asks for this.
#[derive(Debug, Clone, Copy, Default)]
pub struct Exhaustive;

/// The one of these that does not have to be constructed, since it holds nothing.
pub const EXHAUSTIVE: Exhaustive = Exhaustive;

impl Chooser for Exhaustive {
    fn name(&self) -> &'static str {
        "exhaustive"
    }

    fn narrow_strings(
        &self,
        _values: &[&[u8]],
        offered: &[string::Kind],
        _depth: u8,
    ) -> Vec<string::Kind> {
        offered.to_vec()
    }

    fn narrow_integers(
        &self,
        _values: &[i64],
        offered: &[integer::Kind],
        _depth: u8,
    ) -> Vec<integer::Kind> {
        offered.to_vec()
    }
}

/// Encode every candidate on a sample, then encode only the winner on the whole chunk.
///
/// The bet is that a chunk of 122,880 values and a sample of 8,192 drawn from it agree about which
/// encoding suits them, which is a bet about the data and is what the ablation settles. Where it is
/// wrong the cost is size and never correctness, because the winner still has to apply to the whole
/// chunk and is still encoded over all of it.
///
/// The sample is windows of consecutive values rather than values picked one at a time, because
/// three of the candidates are about what a value has in common with the value before it. A sample
/// of scattered singletons would show `FRONT` and `RLE` nothing to find and would rule them out on
/// every column, which is the wrong answer arrived at quickly.
///
/// There are two guards on whether to sample at all and both of them are there because a measurement
/// said so. A chunk with fewer values than the sample is not sampled, because encoding every
/// candidate on something the size of the chunk and then encoding the winner on the chunk is more
/// work than the exhaustive chooser for the same answer. A chunk holding less than a page of bytes is
/// not sampled either, because the cost of the search scales with the bytes in the chunk and not
/// with how many values they are spread over, so on a narrow column there is nothing to save and a
/// sample that misses the structure gives up real size for it.
#[derive(Debug, Clone, Copy)]
pub struct Sampled {
    window: usize,
    regions: usize,
}

/// How many consecutive values one window of the sample holds.
///
/// The tile, which is what a bit packing kernel works in and is the smallest run of a column that
/// has the column's local structure in it rather than one value's worth of accident.
const WINDOW: usize = 1024;

/// How many windows the sample is drawn from.
///
/// Eight windows of a tile each is 8,192 values, a fifteenth of a chunk. Spread across the chunk
/// rather than taken off the front, because the front of a sorted column is one value repeated and
/// a chooser that saw only that would pick `CONSTANT` for everything.
const REGIONS: usize = 8;

/// How few bytes a chunk can hold before sampling it is not worth the risk.
///
/// The ablation in #559 found `Params` at a million rows encoding to 21,782 bytes exhaustively and
/// 128,455 bytes sampled, which is 490 percent for a column that is almost entirely empty strings.
/// It passed the value count guard because it has a million values, and then the sample missed what
/// little structure it had. The exhaustive search over a column that small costs almost nothing,
/// which is the same fact from the other side, so a floor on bytes takes the whole class of column
/// out of the sampler's hands and gives up nothing to do it.
///
/// 256 KiB is one page, which is the smallest unit the format moves. Below that the search is not
/// where the time is.
const FLOOR: usize = 256 * 1024;

impl Default for Sampled {
    fn default() -> Self {
        Self { window: WINDOW, regions: REGIONS }
    }
}

impl Sampled {
    /// The default sample, which is eight windows of 1,024 values.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A sample of a size somebody else picked, which is what the ablation sweeps.
    #[must_use]
    pub fn over(window: usize, regions: usize) -> Self {
        Self { window: window.max(1), regions: regions.max(1) }
    }

    /// How many values the sample holds, which is one of the two things that decide whether
    /// sampling is worth doing.
    #[must_use]
    pub fn size(self) -> usize {
        self.window * self.regions
    }

    /// Whether a chunk of `count` values holding `bytes` bytes is worth sampling.
    fn worth_it(self, count: usize, bytes: usize) -> bool {
        count > self.size() && bytes >= FLOOR
    }
}

impl Chooser for Sampled {
    fn name(&self) -> &'static str {
        "sampled"
    }

    fn narrow_strings(
        &self,
        values: &[&[u8]],
        offered: &[string::Kind],
        depth: u8,
    ) -> Vec<string::Kind> {
        let bytes = values.iter().map(|value| value.len()).sum();
        if offered.len() < 2 || !self.worth_it(values.len(), bytes) {
            return offered.to_vec();
        }
        let sample = sample(values, self.window, self.regions);
        let mut best: Option<(string::Kind, usize)> = None;
        for &kind in offered {
            let Ok(Some(size)) = string::size_as(kind, &sample, depth) else {
                continue;
            };
            if best.is_none_or(|(_, smallest)| size < smallest) {
                best = Some((kind, size));
            }
        }
        // Nothing applied to the sample, which should not happen and is not worth a wrong answer
        // if it does. Hand back everything and let the exhaustive path sort it out.
        best.map_or_else(|| offered.to_vec(), |(kind, _)| vec![kind])
    }

    fn narrow_integers(
        &self,
        values: &[i64],
        offered: &[integer::Kind],
        depth: u8,
    ) -> Vec<integer::Kind> {
        if offered.len() < 2 || !self.worth_it(values.len(), values.len() * 8) {
            return offered.to_vec();
        }
        let sample = sample(values, self.window, self.regions);
        let mut best: Option<(integer::Kind, usize)> = None;
        for &kind in offered {
            let Ok(Some(size)) = integer::size_as(kind, &sample, depth) else {
                continue;
            };
            if best.is_none_or(|(_, smallest)| size < smallest) {
                best = Some((kind, size));
            }
        }
        best.map_or_else(|| offered.to_vec(), |(kind, _)| vec![kind])
    }
}

/// Encode one shape that somebody else settled on, and do not search at all.
///
/// [`Sampled`] decides per chunk, which is right when a chunk is big enough to pay for the sample
/// and when neighbouring chunks are different from each other. Neither holds for a caller that has
/// thousands of small chunks cut out of one column, because the sample would cost as much as the
/// encode and because the answer would come out the same thousands of times. Such a caller decides
/// once, over as much of the column as it likes, and hands the answer here.
///
/// A shape is one kind per level of the cascade, which is a simplification of a real one: `FRONT`
/// produces an integer chunk of prefixes and a string chunk of suffixes at the next level, and both
/// are narrowed to the same entry. That is enough on real data because the tree is narrow and
/// because the levels below the second are small. Any level the shape does not reach is searched
/// exhaustively, which is what makes the shape a hint about the expensive part rather than a
/// decision about all of it.
///
/// An entry that does not apply to a chunk is ignored and the chunk is searched instead. The kinds
/// that apply are a property of the values, and this is a chooser rather than a way round the
/// filter, so a shape can never produce something that will not decode.
#[derive(Debug, Clone)]
pub struct Settled {
    strings: Vec<string::Kind>,
    integers: Vec<integer::Kind>,
    /// The level FSST runs at and the table it compresses against there, when one was trained for
    /// the whole column. See [`string::with_symbols`].
    symbols: Option<(u8, Arc<SymbolTable>)>,
}

impl Settled {
    /// A shape, outermost level first, for the string levels and the integer levels.
    #[must_use]
    pub fn new(strings: Vec<string::Kind>, integers: Vec<integer::Kind>) -> Self {
        Self { strings, integers, symbols: None }
    }

    /// The same shape, compressing against `table` wherever FSST is tried at `depth`.
    #[must_use]
    pub fn with_symbols(mut self, depth: u8, table: SymbolTable) -> Self {
        self.symbols = Some((depth, Arc::new(table)));
        self
    }

    /// The string kinds of the shape, outermost first, which is what a report prints.
    #[must_use]
    pub fn strings(&self) -> &[string::Kind] {
        &self.strings
    }
}

impl Chooser for Settled {
    fn name(&self) -> &'static str {
        "settled"
    }

    fn narrow_strings(
        &self,
        _values: &[&[u8]],
        offered: &[string::Kind],
        depth: u8,
    ) -> Vec<string::Kind> {
        match self.strings.get(depth as usize) {
            Some(kind) if offered.contains(kind) => vec![*kind],
            _ => offered.to_vec(),
        }
    }

    fn narrow_integers(
        &self,
        _values: &[i64],
        offered: &[integer::Kind],
        depth: u8,
    ) -> Vec<integer::Kind> {
        match self.integers.get(depth as usize) {
            Some(kind) if offered.contains(kind) => vec![*kind],
            _ => offered.to_vec(),
        }
    }

    fn considers_integer(&self, kind: integer::Kind, depth: u8) -> bool {
        // `Packed` applies to every chunk the search offers anything else on, so a level settled on
        // it keeps nothing else and the tests for the other kinds are thrown away unread. A global
        // dictionary block settles its integers on `Packed`, and its lengths, prefixes and token
        // arrays each ran those tests, a sorted copy for the dictionary one among them, on every
        // block. Any other settled kind may not be offered, and then the level is searched, so it
        // has to say yes to everything.
        match self.integers.get(depth as usize) {
            Some(integer::Kind::Packed) => kind == integer::Kind::Packed,
            _ => true,
        }
    }

    fn symbols(&self, depth: u8) -> Option<&SymbolTable> {
        self.symbols.as_ref().filter(|(at, _)| *at == depth).map(|(_, table)| &**table)
    }
}

/// Encode an integer chunk the way an earlier one came out, and search only where it stops fitting.
///
/// [`Settled`] holds one kind per level, which is too coarse for a cascade that branches: an `RLE`
/// wants its run values packed and its run lengths constant, and a shape of one kind per level
/// cannot say both. This holds every level's kind in the order the encoder asks for them, which is
/// what [`integer::shape`] reads back out of an encoded chunk, and hands them back one per question.
///
/// The first question whose answer is not among the kinds offered ends the replay, and from there
/// on every question goes to `fallback`. A chunk only offers kinds that apply to it, so a shape
/// that stops fitting costs a search and never a chunk that will not decode. The order of the
/// questions is the order of the kinds only while every answer is a single kind, which is why the
/// replay does not pick back up after a search.
///
/// A shape that still fits can still be the wrong one. Bit packing applies to everything, so a
/// shape settled on a stretch of noise replays happily over a column that has since become one
/// value with exceptions, at forty times the size. What does change when the column does is the set
/// of kinds the top level offers, so a replay can be told the set its shape was searched under with
/// [`Replay::expecting`], and searches from the top when the chunk offers anything else. The set is
/// worked out for the chunk whatever the chooser, so the check costs nothing.
///
/// One of these is for one chunk. The position is kept in atomics because a chooser is shared
/// between threads by contract, not because a chunk's encode is ever split between them.
#[derive(Debug)]
pub struct Replay<'a> {
    kinds: &'a [integer::Kind],
    next: AtomicUsize,
    lost: AtomicBool,
    /// The kinds the top level offered, one bit per tag, once it has been asked.
    first: AtomicU8,
    /// The set the top level has to offer for the replay to go ahead, when there is one.
    expected: Option<u8>,
    fallback: &'a dyn Chooser,
}

impl<'a> Replay<'a> {
    /// A replay of `kinds`, with `fallback` answering once they stop fitting.
    #[must_use]
    pub fn new(kinds: &'a [integer::Kind], fallback: &'a dyn Chooser) -> Self {
        Self {
            kinds,
            next: AtomicUsize::new(0),
            lost: AtomicBool::new(false),
            first: AtomicU8::new(0),
            expected: None,
            fallback,
        }
    }

    /// The same replay, going ahead only on a chunk whose top level offers exactly `offered`.
    #[must_use]
    pub fn expecting(mut self, offered: &[integer::Kind]) -> Self {
        self.expected = Some(bits(offered));
        self
    }

    /// What the top level of the chunk offered, in tag order, or nothing before it was asked.
    #[must_use]
    pub fn first_offered(&self) -> Vec<integer::Kind> {
        let first = self.first.load(Ordering::Relaxed);
        integer::Kind::ALL.into_iter().filter(|kind| first & (1 << *kind as u8) != 0).collect()
    }

    /// Whether every question was answered from the shape, which is whether the chunk came out
    /// the shape it was given.
    #[must_use]
    pub fn held(&self) -> bool {
        !self.lost.load(Ordering::Relaxed) && self.next.load(Ordering::Relaxed) == self.kinds.len()
    }
}

impl Chooser for Replay<'_> {
    fn name(&self) -> &'static str {
        "replay"
    }

    fn narrow_strings(
        &self,
        values: &[&[u8]],
        offered: &[string::Kind],
        depth: u8,
    ) -> Vec<string::Kind> {
        self.fallback.narrow_strings(values, offered, depth)
    }

    fn narrow_integers(
        &self,
        values: &[i64],
        offered: &[integer::Kind],
        depth: u8,
    ) -> Vec<integer::Kind> {
        if depth == 0 {
            self.first.store(bits(offered), Ordering::Relaxed);
            if self.expected.is_some_and(|expected| expected != bits(offered)) {
                self.lost.store(true, Ordering::Relaxed);
            }
        }
        if !self.lost.load(Ordering::Relaxed) {
            let at = self.next.fetch_add(1, Ordering::Relaxed);
            match self.kinds.get(at) {
                Some(kind) if offered.contains(kind) => return vec![*kind],
                _ => self.lost.store(true, Ordering::Relaxed),
            }
        }
        self.fallback.narrow_integers(values, offered, depth)
    }

    fn considers_integer(&self, kind: integer::Kind, depth: u8) -> bool {
        // Asked before the question the replay answers, so it has to say yes to the kind the shape
        // is about to hand back as well as to anything the fallback might keep.
        self.kinds.contains(&kind) || self.fallback.considers_integer(kind, depth)
    }
}

/// A set of integer kinds as one bit per tag.
fn bits(kinds: &[integer::Kind]) -> u8 {
    kinds.iter().fold(0, |set, kind| set | 1 << *kind as u8)
}

/// `regions` windows of `window` consecutive values each, spread evenly across the input.
///
/// The starts are spread over the whole range a window can start at, so the first window begins at
/// the first value and the last one ends at the last value. A chunk of 122,880 values sampled at
/// eight windows of 1,024 gives windows starting at 0, 17,408, 34,816 and so on up to 121,856, which
/// crosses every part of the chunk including both ends of it.
///
/// Spreading to the end rather than striding by `len / regions` matters on the columns this is for.
/// A stride would leave the last stride minus one window of the chunk unsampled, and the tail of a
/// chunk is exactly where a column that is sorted or clustered stops looking like its front.
pub(crate) fn sample<T: Copy>(values: &[T], window: usize, regions: usize) -> Vec<T> {
    let wanted = window * regions;
    if values.len() <= wanted {
        return values.to_vec();
    }
    let last = values.len() - window;
    let mut out = Vec::with_capacity(wanted);
    for region in 0..regions {
        let from = if regions == 1 { 0 } else { region * last / (regions - 1) };
        out.extend_from_slice(&values[from..from + window]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{Chooser, EXHAUSTIVE, Replay, Sampled, Settled, sample};
    use crate::{integer, string};

    /// A settled shape that tests for every kind, which is what [`Settled`] did before it said
    /// which kinds it could never keep.
    #[derive(Debug)]
    struct TestsEverything<'a>(&'a Settled);

    impl Chooser for TestsEverything<'_> {
        fn name(&self) -> &'static str {
            "tests everything"
        }

        fn narrow_strings(
            &self,
            values: &[&[u8]],
            offered: &[string::Kind],
            depth: u8,
        ) -> Vec<string::Kind> {
            self.0.narrow_strings(values, offered, depth)
        }

        fn narrow_integers(
            &self,
            values: &[i64],
            offered: &[integer::Kind],
            depth: u8,
        ) -> Vec<integer::Kind> {
            self.0.narrow_integers(values, offered, depth)
        }
    }

    /// A shape settled on `Packed` writes the same bytes whether or not the other kinds are tested
    /// for, over integers of every shape and over strings whose lengths, prefixes and tokens are
    /// integer arrays under it.
    #[test]
    fn a_shape_settled_on_packed_writes_what_testing_everything_wrote() {
        for values in shaped_columns() {
            let settled = Settled::new(Vec::new(), vec![integer::Kind::Packed]);
            let quick = integer::encode_with(&values, &settled).unwrap();
            let full = integer::encode_with(&values, &TestsEverything(&settled)).unwrap();
            assert_eq!(quick, full, "{}", integer::describe(&full).unwrap());
        }
        let texts = (0..2048)
            .map(|row| match row % 5 {
                0 => format!("https://example.com/item/{row}"),
                1 => format!("https://example.com/item/{}", row / 7),
                2 => String::new(),
                3 => "same".repeat(row % 11),
                _ => format!("{:x}", row * 2_654_435_761_usize),
            })
            .collect::<Vec<_>>();
        let values = texts.iter().map(|text| text.as_bytes()).collect::<Vec<_>>();
        for strings in [
            vec![string::Kind::Front, string::Kind::Lz],
            vec![string::Kind::Lz, string::Kind::Fsst],
            vec![string::Kind::Lz, string::Kind::Plain],
            vec![string::Kind::Fsst],
            vec![string::Kind::Plain],
        ] {
            let settled = Settled::new(strings, vec![integer::Kind::Packed]);
            let quick = string::encode_with(&values, &settled).unwrap();
            let full = string::encode_with(&values, &TestsEverything(&settled)).unwrap();
            assert_eq!(quick, full, "{:?}", settled.strings());
        }
    }

    /// Columns of the shapes a writer meets: a climbing timestamp, runs, one value with exceptions,
    /// a stride, noise, and a short tail.
    fn shaped_columns() -> Vec<Vec<i64>> {
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mut noise = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % 1_000_000) as i64
        };
        vec![
            (0..2048).map(|row| 1_600_000_000_000_000 + row * 1_000_000 + row % 7).collect(),
            (0..2048).map(|row| row / 300).collect(),
            (0..2048).map(|row| if row % 97 == 0 { row } else { 42 }).collect(),
            (0..2048).map(|row| 5 + row * 1_000_000).collect(),
            (0..2048).map(|_| noise()).collect(),
            (0..37).map(|row| row * row).collect(),
        ]
    }

    /// Replaying the shape a chunk came out as gives the same bytes, and asks no question the shape
    /// did not answer, which is the whole of what a writer is relying on when it stops searching.
    #[test]
    fn a_chunk_replayed_through_its_own_shape_comes_out_the_same() {
        for values in shaped_columns() {
            let searched = integer::encode_with(&values, &EXHAUSTIVE).unwrap();
            let kinds = integer::shape(&searched).unwrap();
            let replay = Replay::new(&kinds, &EXHAUSTIVE);
            let replayed = integer::encode_with(&values, &replay).unwrap();
            assert_eq!(replayed, searched, "{}", integer::describe(&searched).unwrap());
            assert!(replay.held(), "{}", integer::describe(&searched).unwrap());
        }
    }

    /// A shape that fits but was searched under a different offer is not replayed. Bit packing
    /// fits everything, so without the check a shape settled on noise would pack a column of one
    /// value with exceptions, which the search writes in a fraction of the bytes.
    #[test]
    fn a_shape_searched_under_another_offer_searches_again() {
        let columns = shaped_columns();
        let (noise, sparse) = (&columns[4], &columns[2]);
        let first = Replay::new(&[], &EXHAUSTIVE);
        let searched = integer::encode_with(noise, &first).unwrap();
        let kinds = integer::shape(&searched).unwrap();
        let offered = first.first_offered();
        assert_eq!(offered, integer::offered(noise));

        let blind = Replay::new(&kinds, &EXHAUSTIVE);
        let packed = integer::encode_with(sparse, &blind).unwrap();
        assert!(blind.held(), "packing fits any column, which is the trouble");

        let checked = Replay::new(&kinds, &EXHAUSTIVE).expecting(&offered);
        let written = integer::encode_with(sparse, &checked).unwrap();
        assert!(!checked.held());
        assert_eq!(written, integer::encode_with(sparse, &EXHAUSTIVE).unwrap());
        assert!(written.len() * 4 < packed.len(), "{} against {}", written.len(), packed.len());
    }

    /// A shape from one column on another column it does not fit still writes that column, because
    /// the replay stops at the first kind that is not offered and searches from there.
    #[test]
    fn a_shape_that_does_not_fit_still_writes_values_that_read_back() {
        let columns = shaped_columns();
        for from in &columns {
            let kinds = integer::shape(&integer::encode_with(from, &EXHAUSTIVE).unwrap()).unwrap();
            for values in &columns {
                let replay = Replay::new(&kinds, &EXHAUSTIVE);
                let bytes = integer::encode_with(values, &replay).unwrap();
                assert_eq!(&integer::decode(&bytes).unwrap(), values);
            }
        }
    }

    #[test]
    fn a_sample_covers_the_whole_input_and_not_one_end_of_it() {
        let values: Vec<i64> = (0..8000).collect();
        let taken = sample(&values, 10, 4);
        assert_eq!(taken.len(), 40);
        assert_eq!(taken[0], 0);
        assert_eq!(taken[10], 2663);
        assert_eq!(taken[20], 5326);
        assert_eq!(taken[30], 7990);
        assert_eq!(taken[39], 7999);
    }

    #[test]
    fn an_input_no_bigger_than_the_sample_is_the_sample() {
        let values: Vec<i64> = (0..30).collect();
        assert_eq!(sample(&values, 10, 4), values);
    }

    #[test]
    fn the_last_window_does_not_run_off_the_end() {
        // Two windows of 40 over 100 values puts the second one at 60, which is the last start that
        // fits. Windows that overlap because there are more of them than the input has room for is
        // fine and double counts a few values. Reading past the end is not.
        let values: Vec<i64> = (0..100).collect();
        let taken = sample(&values, 40, 2);
        assert_eq!(taken.len(), 80);
        assert_eq!(*taken.last().expect("the sample is not empty"), 99);
    }

    #[test]
    fn the_exhaustive_chooser_hands_back_exactly_what_it_was_offered() {
        let offered = [string::Kind::Plain, string::Kind::Fsst, string::Kind::Dict];
        assert_eq!(EXHAUSTIVE.narrow_strings(&[b"a".as_slice()], &offered, 0), offered);
        let offered = [integer::Kind::Packed, integer::Kind::Delta];
        assert_eq!(EXHAUSTIVE.narrow_integers(&[1, 2], &offered, 0), offered);
    }

    #[test]
    fn a_chunk_no_bigger_than_the_sample_is_not_narrowed_at_all() {
        // Sampling a chunk that is smaller than the sample would encode every candidate on
        // something the size of the chunk and then encode the winner on the chunk, which is more
        // work than the exhaustive chooser for the same answer.
        let sampled = Sampled::over(4, 2);
        let values: Vec<i64> = (0..8).collect();
        let offered = [integer::Kind::Packed, integer::Kind::Delta];
        assert_eq!(sampled.narrow_integers(&values, &offered, 0), offered);
    }

    #[test]
    fn a_sampled_chooser_returns_one_of_what_it_was_offered() {
        let sampled = Sampled::over(16, 2);
        let values: Vec<i64> = (0..40_000).map(|index| index / 200).collect();
        let offered = [integer::Kind::Packed, integer::Kind::Rle, integer::Kind::Dict];
        let narrowed = sampled.narrow_integers(&values, &offered, 0);
        assert_eq!(narrowed.len(), 1);
        assert!(offered.contains(&narrowed[0]), "{narrowed:?}");
    }

    #[test]
    fn a_chunk_with_plenty_of_values_and_hardly_any_bytes_is_not_sampled() {
        // ClickBench Params at a million rows: a value per row and almost all of them empty. It
        // passes the value count guard and the exhaustive chooser encodes it in 21,782 bytes while
        // the sampler took 128,455, so the byte floor is what keeps it out of the sampler's hands.
        let sampled = Sampled::over(16, 2);
        let empty = Vec::new();
        let values: Vec<&[u8]> = vec![empty.as_slice(); 40_000];
        let offered = [string::Kind::Plain, string::Kind::Fsst, string::Kind::Dict];
        assert_eq!(sampled.narrow_strings(&values, &offered, 0), offered);
    }
}
