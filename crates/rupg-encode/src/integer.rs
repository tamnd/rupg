//! The single column integer encodings and the cascade over them.
//!
//! `spec/06-compression.md` section 6.2 lists the encoding set and section 6.3 says the ratios are
//! in the cascade rather than in any one encoding. This module is both: the seven candidate shapes
//! for an integer column, each of which encodes its own output by calling back into the chooser, so
//! that RLE over a dictionary over a bit packed code array is a thing that happens by construction
//! rather than a case somebody wrote out.
//!
//! Everything here works on `i64`. A narrower column is widened on the way in and nothing is lost
//! by it, because every encoding's size comes from the range of the values rather than from the
//! declared width of the type: a `SMALLINT` column of values 100 to 130 packs to 5 bits whether it
//! arrived as `i16` or as `i64`. The one place the widening would cost something is a raw copy, and
//! there is no raw copy, because a bit packed unit at width 64 is exactly that and the chooser
//! reaches it on its own when nothing else fits.
//!
//! ## The unit
//!
//! Bit packing is per 1024 values, per [`crate::bitpack`]. Everything else is per chunk, where a
//! chunk is however many values the caller passes in and is meant to be a row group. The two
//! granularities are the point rather than an accident. A frame of reference base that is chosen
//! per 1024 values tracks a column that drifts, which is what a timestamp column and an
//! autoincrementing key both do, and one base per row group would pay the whole range of the row
//! group on every value. A dictionary, on the other hand, is worth more the larger the unit it
//! covers, which is the argument section 6.5 takes all the way to a dictionary per table.
//!
//! ## The serialized form
//!
//! A chunk is a tag byte, a value count, and a body whose shape depends on the tag. Bodies that
//! contain another array of integers contain a whole chunk, tag and all, which is what makes the
//! decoder a fold and what makes the cascade free: nothing in `Rle` knows what its run lengths are
//! encoded as. The header is fixed width little endian rather than a varint, because 5 bytes per
//! chunk against a chunk that holds a row group is not worth the branch on the decode path.
//!
//! ## What the chooser does, and what it will have to do instead
//!
//! It encodes every candidate and keeps the smallest. That is the honest baseline for M1, which is
//! a measurement of what the format can do rather than of how fast a writer can decide, and it is
//! not what a write path can afford. Section 6.3 describes the real thing: evaluate the candidates
//! on a systematic sample, not the first N rows, because column data is frequently clustered and
//! the first 1024 rows of a sorted column look constant. Building that first would mean the numbers
//! this milestone produces are the sampler's numbers rather than the format's, and there would be
//! no way to tell how much the sampler is leaving behind.

// Lifted from rudb crates/rudb-encoding/src/integer.rs at cd9f9676 (2026-10-05).
// Changes: the error type is rupg_common::Error. The time of the tally comes from tally::now. as_chunks in place of chunks_exact, for clippy.

use std::collections::BTreeMap;

use rupg_common::{Error, Result};

use crate::chooser::{Chooser, EXHAUSTIVE};
use crate::reader::Reader;
use crate::tally::{self, Family};

use crate::bitpack::{self, VALUES};

/// How deep a cascade is allowed to go.
///
/// Three levels is what section 6.3 says captures most of what a general compressor would find:
/// dictionary, then bit packed codes, then nothing left worth doing. The limit exists because the
/// chooser is exhaustive and a cascade that could nest forever would be exponential, and because a
/// fourth level has never once been the smallest candidate in anything measured so far.
const MAX_DEPTH: u8 = 3;

/// How many values a run writes at once, whatever the run is.
///
/// A run length decode used to write a value at a time for the length of the run, which reads well
/// and is the wrong shape for the data: a clustered join key runs two or three long, so the loop
/// spent its time mispredicting its own exit and the branch cost more than the stores did. Writing
/// a fixed eight and then moving on by the run's real length has no exit to predict, and whatever
/// of the eight was surplus is overwritten by the run that follows, because every run writes at
/// least its own length. Eight because it is two vector stores on every machine this runs on and
/// longer than nearly every run in a column worth run length encoding at all.
const RUN: usize = 8;

/// The average run length from which a run length chunk is decoded into reserved room rather than
/// a zeroed one. Past it the zeroing is most of the writes, and below it the fixed width write of
/// [`RUN`] is the cheaper loop.
const LONG_RUN: usize = 64;

/// How many rows a run of an RLE chunk has to average before [`pointed`] says a few of its rows
/// are cheaper found by walking the runs than by expanding the chunk.
const RLE_WALKED: usize = 4;

/// What a chunk is encoded as. The discriminant is the tag byte in the serialized form and is part
/// of the format, so the numbers are written down rather than left to the compiler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// One value repeated. The whole chunk is the tag, the count and the value.
    Constant = 0,
    /// Frame of reference then bit packed, per 1024 values. Covers plain bit packing at base zero
    /// and a raw copy at width 64.
    Packed = 1,
    /// Differences between neighbours, zigzagged so a decreasing column is as cheap as an
    /// increasing one, then encoded as a chunk in its own right.
    Delta = 2,
    /// Run values and run lengths, each encoded as a chunk in its own right.
    Rle = 3,
    /// A dictionary of the distinct values and an array of codes into it, both encoded as chunks in
    /// their own right.
    Dict = 4,
    /// One dominant value with an exception list of positions and values.
    Sparse = 5,
    /// A base and a common step, with the number of steps to each value encoded as a chunk in its
    /// own right.
    Strided = 6,
}

impl Kind {
    /// Every kind, in tag order.
    pub const ALL: [Self; 7] = [
        Self::Constant,
        Self::Packed,
        Self::Delta,
        Self::Rle,
        Self::Dict,
        Self::Sparse,
        Self::Strided,
    ];

    fn tag(self) -> u8 {
        self as u8
    }

    fn from_tag(tag: u8) -> Result<Self> {
        match tag {
            0 => Ok(Self::Constant),
            1 => Ok(Self::Packed),
            2 => Ok(Self::Delta),
            3 => Ok(Self::Rle),
            4 => Ok(Self::Dict),
            5 => Ok(Self::Sparse),
            6 => Ok(Self::Strided),
            other => Err(Error::internal(format!("unknown encoding tag {other}"))),
        }
    }

    /// The name that goes in a report.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Constant => "CONSTANT",
            Self::Packed => "FOR+BITPACK",
            Self::Delta => "DELTA",
            Self::Rle => "RLE",
            Self::Dict => "DICT",
            Self::Sparse => "SPARSE",
            Self::Strided => "STRIDE",
        }
    }
}

/// Encodes a chunk of integers, choosing the cascade that comes out smallest.
///
/// # Errors
///
/// If the chunk is longer than `u32::MAX`, or if an encoding produces something its own decoder
/// would not accept, which is an internal inconsistency rather than a caller error.
pub fn encode(values: &[i64]) -> Result<Vec<u8>> {
    encode_with(values, &EXHAUSTIVE)
}

/// [`encode`] with somebody else deciding which candidates are worth encoding in full.
///
/// A chooser narrows the list and nothing else. It cannot offer a candidate that does not apply, so
/// whatever it picks still has to encode the whole chunk and still has to decode, and the worst a
/// bad one can do is come out bigger than [`encode`] would have.
///
/// # Errors
///
/// As [`encode`].
pub fn encode_with(values: &[i64], chooser: &dyn Chooser) -> Result<Vec<u8>> {
    encode_at(values, 0, chooser)
}

/// Decodes a chunk written by [`encode`].
///
/// # Errors
///
/// If the bytes are truncated, carry an unknown tag, or describe a chunk whose parts do not agree
/// with each other.
pub fn decode(bytes: &[u8]) -> Result<Vec<i64>> {
    let mut reader = Reader::new(bytes);
    let values = decode_chunk(&mut reader)?;
    if reader.remaining() != 0 {
        return Err(Error::internal(format!(
            "{} bytes left over after decoding a chunk",
            reader.remaining()
        )));
    }
    Ok(values)
}

/// Decodes one encoded chunk straight into the integer type a column is declared at.
///
/// The same values [`decode`] gives, without the `i64` in between. A reader that wants a `SMALLINT`
/// column used to fill eight bytes a row with zeros, write every value into them, and then copy the
/// lot into two bytes a row, which on ClickBench Q1 was a fifth of the query. Constant, packed,
/// sparse and run length chunks are written in the target type directly. The other kinds decode as
/// before and are narrowed after, since they are rare at the top of a column.
///
/// # Errors
///
/// As [`decode`], or if a value does not fit in `T`, which is a chunk that disagrees with the type
/// it was written for.
pub fn decode_as<T: Lane>(bytes: &[u8]) -> Result<Vec<T>> {
    let mut reader = Reader::new(bytes);
    let values = decode_chunk_as(&mut reader)?;
    if reader.remaining() != 0 {
        return Err(Error::internal(format!(
            "{} bytes left over after decoding a chunk",
            reader.remaining()
        )));
    }
    Ok(values)
}

/// An integer type a chunk can be decoded straight into. See [`decode_as`].
pub trait Lane: Copy + Default {
    /// The value in this type, or `None` when it does not fit.
    fn fit(value: i64) -> Option<Self>;

    /// The value in this type, for a value already known to fit.
    fn wrap(value: i64) -> Self;
}

macro_rules! lanes {
    ($($ty:ty),* $(,)?) => {$(
        impl Lane for $ty {
            fn fit(value: i64) -> Option<Self> {
                Self::try_from(value).ok()
            }

            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                clippy::unnecessary_cast,
                reason = "only called on a value the caller has checked fits"
            )]
            fn wrap(value: i64) -> Self {
                value as Self
            }
        }
    )*};
}

lanes!(i8, u8, i16, u16, i32, u32, i64, u64);

/// One value in the type the chunk is being decoded into, or the error for a value that is not.
fn lane<T: Lane>(value: i64) -> Result<T> {
    T::fit(value).ok_or_else(|| Error::internal(format!("{value} is outside the chunk's type")))
}

/// Counts values in one encoded chunk without expanding sparse or run-length chunks into rows.
///
/// The result is computed from the encoded row values when called. It is not a stored histogram.
/// Other encodings use the ordinary decoder until they have a useful count form of their own.
///
/// # Errors
///
/// As [`decode`], or if a sparse position or run length is outside the chunk.
pub fn tally(bytes: &[u8]) -> Result<(usize, Vec<(i64, u64)>)> {
    let mut counts = BTreeMap::<i64, u64>::new();
    let rows = fold(bytes, |value, count| {
        *counts.entry(value).or_default() += count;
        Ok(())
    })?;
    Ok((rows, counts.into_iter().collect()))
}

/// Visits the values of one encoded chunk with their runtime row counts. Sparse chunks written
/// with sorted exception positions need no per-chunk count map. An older or malformed chunk with
/// repeated positions keeps the decoder's last-write-wins behavior.
///
/// # Errors
///
/// As [`decode`], or if the callback rejects a count.
pub fn fold(bytes: &[u8], mut emit: impl FnMut(i64, u64) -> Result<()>) -> Result<usize> {
    let mut reader = Reader::new(bytes);
    let kind = Kind::from_tag(reader.u8()?)?;
    let count = reader.u32()? as usize;
    match kind {
        Kind::Constant => {
            let value = reader.i64()?;
            if count != 0 {
                emit(value, count as u64)?;
            }
        }
        Kind::Sparse => {
            let dominant = reader.i64()?;
            let exception_count = reader.u32()? as usize;
            let positions = decode_chunk(&mut reader)?;
            let values = decode_chunk(&mut reader)?;
            if positions.len() != exception_count || values.len() != exception_count {
                return Err(Error::internal("a sparse chunk disagrees about its exception count"));
            }
            let mut ordered = true;
            let mut previous = None;
            for &position in &positions {
                let position = usize::try_from(position)
                    .ok()
                    .filter(|&position| position < count)
                    .ok_or_else(|| Error::internal("a sparse exception is outside the chunk"))?;
                if previous.is_some_and(|last| position <= last) {
                    ordered = false;
                }
                previous = Some(position);
            }
            if ordered {
                if count != exception_count {
                    emit(dominant, (count - exception_count) as u64)?;
                }
                for value in values {
                    emit(value, 1)?;
                }
            } else {
                // The ordinary decoder lets a later exception overwrite an earlier one at the
                // same position. Keep that rule for chunks the writer would not normally produce.
                let mut exceptions = BTreeMap::<usize, i64>::new();
                for (position, value) in positions.into_iter().zip(values) {
                    exceptions.insert(position as usize, value);
                }
                if count != exceptions.len() {
                    emit(dominant, (count - exceptions.len()) as u64)?;
                }
                for value in exceptions.into_values() {
                    emit(value, 1)?;
                }
            }
        }
        Kind::Rle => {
            let values = decode_chunk(&mut reader)?;
            let lengths = decode_chunk(&mut reader)?;
            if values.len() != lengths.len() {
                return Err(Error::internal("an RLE chunk has more runs than run lengths"));
            }
            let mut rows = 0_usize;
            for (value, length) in values.into_iter().zip(lengths) {
                let length = usize::try_from(length)
                    .map_err(|_| Error::internal("a negative RLE run length"))?;
                rows = rows
                    .checked_add(length)
                    .filter(|&rows| rows <= count)
                    .ok_or_else(|| Error::internal("an RLE run ends past its chunk"))?;
                if length != 0 {
                    emit(value, length as u64)?;
                }
            }
            check_count(rows, count)?;
        }
        _ => {
            // Re-read the header through the existing decoder for the other cascade shapes.
            reader = Reader::new(bytes);
            let values = decode_chunk(&mut reader)?;
            check_count(values.len(), count)?;
            for value in values {
                emit(value, 1)?;
            }
        }
    }
    if reader.remaining() != 0 {
        return Err(Error::internal(format!(
            "{} bytes left over after counting a chunk",
            reader.remaining()
        )));
    }
    Ok(count)
}

/// How few of a packed unit's values a selected decode has to want before it finds each one on its
/// own rather than unpacking the unit, as one in this many.
///
/// Unpacking a unit is about three thousand instructions and finding one value in it is about
/// twenty five, so the two cost the same at around a hundred and twenty values of the thousand. At
/// one in thirty two, TPC-H q09 unpacked every unit of the `lineitem` columns it reads to keep the
/// fifty or so rows the parts it wants have in each.
const SPARSE: usize = 8;

/// Decodes selected row positions from a chunk written by [`encode`].
///
/// Positions must be sorted and unique. Packed chunks read only the words holding those positions,
/// and run length chunks walk their run boundaries without expanding the output. Other cascade
/// shapes use the full decoder and select afterward until they have a point form of their own.
///
/// # Errors
///
/// As [`decode`], or if a position is outside the chunk or the positions are not strictly
/// increasing.
pub fn decode_selected(bytes: &[u8], positions: &[usize]) -> Result<Vec<i64>> {
    if positions.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(Error::internal("selected integer positions are not sorted and unique"));
    }
    let mut reader = Reader::new(bytes);
    let values = decode_selected_chunk(&mut reader, positions)?;
    if reader.remaining() != 0 {
        return Err(Error::internal(format!(
            "{} bytes left over after decoding selected values",
            reader.remaining()
        )));
    }
    Ok(values)
}

/// Whether [`decode_selected`] reads a few rows of this chunk for less than decoding all of it.
///
/// True for the kinds whose rows can be found without the rows before them: a constant, packed
/// units, and strides or dictionary codes over packed units. A run length chunk walks every run to
/// find where a row is, which is cheaper than expanding it only when its runs are long, and a delta
/// chunk adds up every delta before it, so for a delta chunk or one of short runs a caller that
/// wants a few rows does better decoding the chunk the usual way and picking them out.
#[must_use]
pub fn pointed(bytes: &[u8]) -> bool {
    let simple = |bytes: &[u8]| {
        bytes
            .first()
            .and_then(|&tag| Kind::from_tag(tag).ok())
            .is_some_and(|kind| matches!(kind, Kind::Constant | Kind::Packed))
    };
    match bytes.first().and_then(|&tag| Kind::from_tag(tag).ok()) {
        Some(Kind::Constant | Kind::Packed) => true,
        // The tag, the count, the base and the stride come before the steps.
        Some(Kind::Strided) => bytes.get(1 + 4 + 8 + 8..).is_some_and(simple),
        // The dictionary is read whole whatever the rows, so only the codes need a point form.
        Some(Kind::Dict) => {
            let mut reader = Reader::new(bytes.get(1 + 4..).unwrap_or_default());
            skip_chunk(&mut reader).is_ok() && simple(reader.rest())
        }
        // Walking the runs costs a run length a run and expanding them a write a row, so a chunk
        // whose runs are several rows long on average is cheaper walked. A key column stored in its
        // own order, the way `cast_info.movie_id` is, has runs of about ten, and a scan narrowed by
        // a join was expanding every part of it to keep a few rows. The tag and the row count come
        // first, then the run values' tag and their count, which is the number of runs.
        Some(Kind::Rle) => {
            let word = |at: usize| {
                bytes
                    .get(at..at + 4)
                    .and_then(|four| <[u8; 4]>::try_from(four).ok())
                    .map(|four| u32::from_le_bytes(four) as usize)
            };
            match (word(1), word(1 + 4 + 1)) {
                (Some(rows), Some(runs)) => runs.saturating_mul(RLE_WALKED) <= rows,
                _ => false,
            }
        }
        _ => false,
    }
}

/// Whether this is a run length chunk, which [`decode_selected`] reads without expanding its runs.
///
/// It still decodes every run length, and the run values when they are deltas, so it is not
/// [`pointed`]. What it skips is writing out every row and picking a few of them afterwards, which
/// for a key kept as runs, such as the order key of TPC-H lineitem, was most of the cost.
#[must_use]
pub fn run_length(bytes: &[u8]) -> bool {
    bytes.first().and_then(|&tag| Kind::from_tag(tag).ok()) == Some(Kind::Rle)
}

/// Decodes a chunk that sits at the front of a longer buffer, and says how many bytes it took.
///
/// A string column holds integer chunks inside its own body, and the reader on that side cannot
/// know where the nested chunk ends until it has been read. A chunk is self delimiting, so this is
/// the same work [`decode`] does without the check that nothing follows.
///
/// # Errors
///
/// As [`decode`], except that trailing bytes are what the caller asked about rather than an error.
pub fn decode_prefix(bytes: &[u8]) -> Result<(Vec<i64>, usize)> {
    let mut reader = Reader::new(bytes);
    let values = decode_chunk(&mut reader)?;
    Ok((values, reader.used()))
}

/// Whether the chunk at the front of `bytes` is a stride, values that are a base and a multiple of
/// one step apart, the way a decimal quantity of whole units is.
///
/// A reader that holds what it decodes asks this to choose the form it holds it in. A stride is a
/// narrow range of codes that decodes to a wide range of values, so held flat it is eight bytes a
/// row where packed over the range it is a few bits, while a delta chunk, the way a sorted key is
/// written, is read by runs that are faster over flat values.
#[must_use]
pub fn is_strided(bytes: &[u8]) -> bool {
    bytes.first().and_then(|&tag| Kind::from_tag(tag).ok()) == Some(Kind::Strided)
}

/// [`describe`] over a chunk at the front of a longer buffer, and how many bytes it took.
///
/// # Errors
///
/// As [`decode_prefix`].
pub fn describe_prefix(bytes: &[u8]) -> Result<(String, usize)> {
    let mut reader = Reader::new(bytes);
    let text = describe_chunk(&mut reader)?;
    Ok((text, reader.used()))
}

/// The size in bytes of every candidate, for a report that wants to say what the cascade was
/// chosen over rather than only what it chose. A candidate that does not apply is absent.
///
/// # Errors
///
/// As [`encode`].
pub fn candidate_sizes(values: &[i64]) -> Result<Vec<(Kind, usize)>> {
    let mut sizes = Vec::new();
    for kind in candidates(values, 0, &EXHAUSTIVE) {
        if let Some(bytes) = encode_as(kind, values, 0, &EXHAUSTIVE)? {
            sizes.push((kind, bytes.len()));
        }
    }
    Ok(sizes)
}

/// Which candidates [`encode`] would try on this chunk, in the order it tries them.
///
/// The chooser is exhaustive, so this is also the list of encodes it pays for to return one of
/// them. A caller measuring where the encode time goes needs the list separately from the sizes,
/// because a candidate that is offered and turns out not to apply still costs whatever it spent
/// finding that out.
#[must_use]
pub fn offered(values: &[i64]) -> Vec<Kind> {
    candidates(values, 0, &EXHAUSTIVE)
}

/// One candidate on its own, which is what the chooser calls once per entry in [`offered`].
///
/// `None` when the encoding does not apply. This is here so that the time the chooser spends can be
/// attributed to the candidate that spent it, which is the measurement F2 wants before anybody
/// replaces the exhaustive search with a sampled one. It is not how a writer encodes a chunk:
/// [`encode`] is, and picking a kind by hand gives up the only thing the chooser is for.
///
/// # Errors
///
/// As [`encode`].
pub fn encode_only(kind: Kind, values: &[i64]) -> Result<Option<Vec<u8>>> {
    encode_as(kind, values, 0, &EXHAUSTIVE)
}

/// How big one candidate comes out, which is all a sampling chooser needs from it.
///
/// The bytes are thrown away, so this says nothing [`encode_only`] does not. It is `pub(crate)` and
/// separate so that the sampler in [`crate::chooser`] is not handing back buffers it will not read.
pub(crate) fn size_as(kind: Kind, values: &[i64], depth: u8) -> Result<Option<usize>> {
    Ok(encode_as(kind, values, depth, &EXHAUSTIVE)?.map(|bytes| bytes.len()))
}

/// The kind at every level of an encoded chunk, in the order the encoder chose them.
///
/// The order is the one [`encode_with`] asks its chooser in: a level, then everything under its
/// first inner chunk, then everything under its second. So a chooser that hands these back one per
/// question gets the same cascade on a chunk that offers the same kinds, without searching any of
/// it. That is what a writer with many small parts of one column wants, because the search is
/// most of what the encode costs and neighbouring parts nearly always come out the same shape.
///
/// # Errors
///
/// As [`decode`].
pub fn shape(bytes: &[u8]) -> Result<Vec<Kind>> {
    let mut reader = Reader::new(bytes);
    let mut kinds = Vec::new();
    shape_chunk(&mut reader, &mut kinds)?;
    Ok(kinds)
}

/// The cascade a chunk was encoded as, as a line of text like `DICT(PACKED, PACKED)`.
///
/// # Errors
///
/// As [`decode`].
pub fn describe(bytes: &[u8]) -> Result<String> {
    let mut reader = Reader::new(bytes);
    describe_chunk(&mut reader)
}

fn encode_at(values: &[i64], depth: u8, chooser: &dyn Chooser) -> Result<Vec<u8>> {
    let started = tally::now();
    let offered = candidates(values, depth, chooser);
    let narrowed = chooser.narrow_integers(values, &offered, depth);
    // Only the top level is counted, so that a cascade's time is counted once. See `tally`.
    let counted = depth == 0;
    if counted {
        tally::chose(Family::Integer, started);
    }
    let mut best: Option<(Kind, Vec<u8>)> = None;
    for kind in narrowed {
        let encoded = if counted {
            tally::offer(Family::Integer, kind.tag(), || encode_as(kind, values, depth, chooser))?
        } else {
            encode_as(kind, values, depth, chooser)?
        };
        let Some(bytes) = encoded else {
            continue;
        };
        if best.as_ref().is_none_or(|(_, current)| bytes.len() < current.len()) {
            best = Some((kind, bytes));
        }
    }
    // `Packed` applies to every input including the empty one, so the chooser always has at least
    // one candidate and this cannot be reached without a bug in `candidates`.
    let (kind, bytes) = best.ok_or_else(|| Error::internal("no encoding applied to the chunk"))?;
    if counted {
        tally::kept(Family::Integer, kind.tag());
    }
    Ok(bytes)
}

/// Which candidates are worth encoding for this input.
///
/// The filters here are not the cost model. They are the cases where the encoding cannot be
/// expressed at all, or is provably larger than `Packed` on the same data, so that the exhaustive
/// chooser does not spend a dictionary build on a column of 100,000 distinct values to discover
/// what its distinct count already said.
///
/// A kind the chooser says it will never keep is not tested for at all. The test for a dictionary
/// sorts a copy of the chunk, and this runs at every level of the cascade, so a chooser that never
/// keeps a dictionary was paying for a sort per level to find out something it would ignore.
fn candidates(values: &[i64], depth: u8, chooser: &dyn Chooser) -> Vec<Kind> {
    let mut kinds = vec![Kind::Packed];
    if depth >= MAX_DEPTH {
        return kinds;
    }
    let Some(profile) = Profile::of(values) else {
        return kinds;
    };
    if profile.runs == 1 {
        // Nothing else can beat 13 bytes, so this is the whole answer rather than a candidate.
        return vec![Kind::Constant];
    }
    let considered = |kind| chooser.considers_integer(kind, depth);
    // Every neighbouring difference is no wider than the whole range, so a range that fits in an
    // `i64` answers for all of them and only a chunk holding both ends of the type walks the pairs.
    let fits = profile.max.checked_sub(profile.min).is_some();
    if considered(Kind::Delta)
        && (if fits { profile.deltas_pay(values) } else { deltas_fit(values) })
    {
        kinds.push(Kind::Delta);
    }
    if considered(Kind::Rle) && profile.runs * 4 <= values.len() * 3 {
        kinds.push(Kind::Rle);
    }
    // A dictionary's codes are as wide as its distinct count, so one whose codes are no narrower
    // than the values has only added a dictionary. On TPC-H SF1 it was offered 1,124 times, kept 16
    // times and cost 30% of the integer cascade's time before this.
    if considered(Kind::Dict) && profile.width() > 1 {
        let distinct = spread_of(values).0;
        if distinct * 2 <= values.len() && width_of(distinct as u64 - 1) < profile.width() {
            kinds.push(Kind::Dict);
        }
    }
    // A value in four rows out of five leaves a fifth for everything else, and each of those rows
    // starts at most two runs, so a chunk with more runs than that has no such value and the vote
    // is not taken.
    if considered(Kind::Sparse)
        && (profile.runs - 1) * 5 <= values.len() * 2
        && majority(values).is_some_and(|(_, count)| count * 10 >= values.len() * 8)
    {
        kinds.push(Kind::Sparse);
    }
    if considered(Kind::Strided) && stride_from(values, profile.min).is_some() {
        kinds.push(Kind::Strided);
    }
    kinds
}

/// What one pass over a chunk says about it, which is most of what the candidate tests ask.
///
/// The tests used to walk the chunk once each: once to see whether it was one value, once for the
/// deltas, once to count runs, twice for the vote and once more for the smallest value under the
/// stride. That is six passes on every chunk at every level of the cascade, and on ClickBench `hits`
/// they were most of the tenth of the load's CPU that `encode_at` came to, since a replayed part
/// still asks every question the fallback would. This is one pass, and the vote is only taken where
/// the run count leaves room for it.
///
/// The same pass looks at the differences too, since it has both neighbours in hand. `DELTA` was
/// offered on every chunk whose range fit, and on the `hits_0` load it was 3,325 offers, 20.6% of
/// the integer cascade's time and never kept once. What it stores is the zigzagged differences, so
/// their spread says how wide they pack and their runs say whether they would run-length code.
struct Profile {
    min: i64,
    max: i64,
    /// Runs of equal neighbours, which is one for a chunk of a single value.
    runs: usize,
    /// The smallest and largest zigzagged difference between neighbours. They wrap where the range
    /// does not fit in an `i64`, and nothing reads them then.
    delta_low: u64,
    delta_high: u64,
    /// Runs of equal differences, which is one for a chunk of fewer than three values.
    delta_runs: usize,
}

impl Profile {
    fn of(values: &[i64]) -> Option<Self> {
        let first = *values.first()?;
        let (mut min, mut max, mut breaks) = (first, first, 0usize);
        let mut last = values.get(1).map_or(0, |second| second.wrapping_sub(first));
        let (mut delta_low, mut delta_high, mut turns) = (u64::MAX, 0u64, 0usize);
        for (before, after) in values.iter().zip(&values[1..]) {
            min = min.min(*after);
            max = max.max(*after);
            breaks += usize::from(before != after);
            let delta = after.wrapping_sub(*before);
            let zigzagged = zigzag(delta);
            delta_low = delta_low.min(zigzagged);
            delta_high = delta_high.max(zigzagged);
            turns += usize::from(delta != last);
            last = delta;
        }
        Some(Self { min, max, runs: breaks + 1, delta_low, delta_high, delta_runs: turns + 1 })
    }

    /// How many bits `Packed` needs for a value of this chunk at most, from the whole range.
    fn width(&self) -> u32 {
        width_of(self.max.wrapping_sub(self.min) as u64)
    }

    /// Whether the differences are worth encoding, for a chunk whose range fits in an `i64`.
    ///
    /// They are when they pack narrower than the values, which is a sorted key or a slowly moving
    /// counter, or when they run-length code and the values do not, which is a column that climbs
    /// in steps, or when the first few take only a handful of values, which is a column that walks
    /// a cycle and whose differences make a dictionary of a few entries. Anything else comes out no
    /// smaller than `Packed` or `Rle` on the values.
    fn deltas_pay(&self, values: &[i64]) -> bool {
        let len = values.len();
        let narrower = width_of(self.delta_high.wrapping_sub(self.delta_low)) < self.width();
        // A chunk that run-length codes has differences that are nearly all zero, so they repeat
        // and there are few of them, and `Rle` on the values still beats them.
        let unruly = self.runs * 4 > len * 3;
        let repeat = self.delta_runs * 4 <= len.saturating_sub(1) * 3;
        narrower || (unruly && (repeat || few_deltas(values)))
    }
}

/// Whether the first [`FEW_DELTAS_SEEN`] differences take no more than [`FEW_DELTAS`] values.
///
/// It stops at the first difference past that many, which on a chunk with nothing cyclic in it is
/// a handful of pairs in.
fn few_deltas(values: &[i64]) -> bool {
    let mut seen = [0i64; FEW_DELTAS];
    let mut count = 0;
    for pair in values.windows(2).take(FEW_DELTAS_SEEN) {
        let delta = pair[1].wrapping_sub(pair[0]);
        if seen[..count].contains(&delta) {
            continue;
        }
        if count == FEW_DELTAS {
            return false;
        }
        seen[count] = delta;
        count += 1;
    }
    true
}

/// How many distinct differences [`few_deltas`] allows.
const FEW_DELTAS: usize = 4;

/// How many differences [`few_deltas`] looks at.
const FEW_DELTAS_SEEN: usize = 64;

/// The bits a value up to `range` takes, which is zero for a range of zero.
fn width_of(range: u64) -> u32 {
    u64::BITS - range.leading_zeros()
}

/// `None` when the encoding does not apply to this input, which the caller treats as a candidate
/// that did not run rather than as a failure.
fn encode_as(
    kind: Kind,
    values: &[i64],
    depth: u8,
    chooser: &dyn Chooser,
) -> Result<Option<Vec<u8>>> {
    let mut out = Vec::new();
    put_u8(&mut out, kind.tag());
    put_u32(&mut out, u32::try_from(values.len()).map_err(|_| too_long(values.len()))?);
    match kind {
        Kind::Constant => {
            let Some(first) = values.first() else {
                return Ok(None);
            };
            if values.iter().any(|value| value != first) {
                return Ok(None);
            }
            put_i64(&mut out, *first);
        }
        Kind::Packed => encode_packed(values, &mut out)?,
        Kind::Delta => {
            // An empty chunk has no first value to hang the differences off. The search never asks
            // for one because `candidates` rules it out, but `encode_only` goes straight past that
            // and used to index into the chunk anyway.
            let (Some(first), Some(deltas)) = (values.first(), deltas(values)) else {
                return Ok(None);
            };
            put_i64(&mut out, *first);
            out.extend_from_slice(&encode_at(&deltas, depth + 1, chooser)?);
        }
        Kind::Rle => {
            let (run_values, run_lengths) = runs(values);
            if run_values.is_empty() {
                return Ok(None);
            }
            out.extend_from_slice(&encode_at(&run_values, depth + 1, chooser)?);
            out.extend_from_slice(&encode_at(&run_lengths, depth + 1, chooser)?);
        }
        Kind::Dict => {
            let dictionary = distinct_values(values);
            if dictionary.is_empty() {
                return Ok(None);
            }
            let codes = codes_over(values, &dictionary);
            out.extend_from_slice(&encode_at(&dictionary, depth + 1, chooser)?);
            out.extend_from_slice(&encode_at(&codes, depth + 1, chooser)?);
        }
        Kind::Sparse => {
            // The majority is the most frequent value whenever there is one, and a chunk the search
            // offers this for always has one. `encode_only` can ask about any chunk, so the sort is
            // still there for a chunk with no majority.
            let Some((value, _)) = majority(values).or_else(|| spread_of(values).1) else {
                return Ok(None);
            };
            let mut positions = Vec::new();
            let mut exceptions = Vec::new();
            for (index, other) in values.iter().enumerate() {
                if *other != value {
                    positions.push(index as i64);
                    exceptions.push(*other);
                }
            }
            put_i64(&mut out, value);
            put_u32(
                &mut out,
                u32::try_from(positions.len()).map_err(|_| too_long(positions.len()))?,
            );
            out.extend_from_slice(&encode_at(&positions, depth + 1, chooser)?);
            out.extend_from_slice(&encode_at(&exceptions, depth + 1, chooser)?);
        }
        Kind::Strided => {
            let (Some(base), Some(stride)) = (values.iter().min().copied(), stride_of(values))
            else {
                return Ok(None);
            };
            let mut steps = Vec::with_capacity(values.len());
            for value in values {
                let step = offset_from(*value, base) / stride;
                // A step count the recursion cannot hold. An offset is at most 65 bits because both
                // ends came from an `i64`, and only a stride of one leaves it that wide, which is a
                // stride this never offers. Refused rather than wrapped, because a candidate that
                // does not apply is one the chooser skips.
                let Ok(step) = i64::try_from(step) else {
                    return Ok(None);
                };
                steps.push(step);
            }
            put_i64(&mut out, base);
            put_u64(&mut out, stride);
            out.extend_from_slice(&encode_at(&steps, depth + 1, chooser)?);
        }
    }
    Ok(Some(out))
}

/// Frame of reference and bit packing, one base and one width per 1024 values.
///
/// A base per unit rather than per chunk is most of what makes this work on real columns. A
/// timestamp column over a day drifts across a range that needs 47 bits, and the same column inside
/// any one unit spans a few seconds and needs 12. One base per row group would pay the 47 on every
/// value.
///
/// A unit shorter than 1024 values, which is the last one of any chunk whose length is not a
/// multiple of the unit and is the only one of every short array in a cascade, goes through
/// [`bitpack::pack_tail`] instead. The transposed layout has no partial form and would charge a
/// five entry dictionary for 1024 entries.
fn encode_packed(values: &[i64], out: &mut Vec<u8>) -> Result<()> {
    // The same three buffers for every unit, because the chooser encodes every candidate it is
    // offered before it picks one and this loop runs once per candidate per unit.
    let mut offsets: Vec<u64> = Vec::with_capacity(VALUES);
    // Held at the width 64 length, which is the largest a unit can be, so a narrower unit writes the
    // front of it and there is no resize per unit.
    let mut packed: Vec<u64> = vec![0; bitpack::packed_len::<u64>(64)];
    let mut transposed = bitpack::Scratch::<u64>::new();
    for unit in values.chunks(VALUES) {
        let base = unit.iter().copied().min().unwrap_or(0);
        offsets.clear();
        offsets.extend(unit.iter().map(|value| offset_from(*value, base)));
        let width = bitpack::required_width(&offsets);
        put_i64(out, base);
        put_u8(out, u8::try_from(width).map_err(|_| Error::internal("impossible width"))?);
        if unit.len() == VALUES {
            let words = bitpack::packed_len::<u64>(width);
            bitpack::pack_with(&offsets, width, &mut packed[..words], &mut transposed)?;
            for word in &packed[..words] {
                put_u64(out, *word);
            }
        } else {
            bitpack::pack_tail(&offsets, width, out)?;
        }
    }
    Ok(())
}

fn decode_chunk(reader: &mut Reader<'_>) -> Result<Vec<i64>> {
    let kind = Kind::from_tag(reader.u8()?)?;
    let count = reader.u32()? as usize;
    match kind {
        Kind::Constant => Ok(vec![reader.i64()?; count]),
        Kind::Packed => {
            // One buffer for the chunk, and every value written into it once. Both unpackers take
            // the frame of reference base and put the value it belongs to where it goes, so there
            // is no unit of raw offsets in between and no second pass to fold the base back in, and
            // both read the packed bytes where the chunk put them rather than through a copy.
            let mut values = vec![0i64; count];
            let mut done = 0;
            while done < count {
                let base = reader.i64()?;
                let width = reader.u8()? as usize;
                let wanted = (count - done).min(VALUES);
                let into = &mut values[done..done + wanted];
                if wanted == VALUES {
                    let unit = reader.bytes(bitpack::unit_len(width))?;
                    bitpack::unpack_unit_into(unit, width, into, |offset| {
                        value_from(offset, base)
                    })?;
                } else {
                    let bytes = reader.bytes(bitpack::tail_len(wanted, width))?;
                    bitpack::unpack_tail_into(bytes, width, into, |offset| {
                        value_from(offset, base)
                    })?;
                }
                done += wanted;
            }
            Ok(values)
        }
        Kind::Delta => {
            let first = reader.i64()?;
            let mut values = decode_chunk(reader)?;
            check_count(values.len() + 1, count)?;
            // Each value is written over the difference that follows it, so the sums go into the
            // vector the differences came in and only the last one is pushed on the end. Pushing
            // every value into a second vector asked it for room once a value.
            let mut current = first;
            for value in &mut values {
                let delta = unzigzag(*value as u64);
                *value = current;
                current = current.wrapping_add(delta);
            }
            values.push(current);
            Ok(values)
        }
        Kind::Rle => {
            let run_values = decode_chunk(reader)?;
            let run_lengths = decode_chunk(reader)?;
            expanded(&run_values, &run_lengths, count)
        }
        Kind::Dict => {
            let dictionary = decode_chunk(reader)?;
            let codes = decode_chunk(reader)?;
            let mut values = Vec::with_capacity(count);
            for code in codes {
                let index =
                    usize::try_from(code).ok().and_then(|index| dictionary.get(index)).ok_or_else(
                        || Error::internal(format!("code {code} is not in the dictionary")),
                    )?;
                values.push(*index);
            }
            check_count(values.len(), count)?;
            Ok(values)
        }
        Kind::Sparse => {
            let value = reader.i64()?;
            let exception_count = reader.u32()? as usize;
            let positions = decode_chunk(reader)?;
            let exceptions = decode_chunk(reader)?;
            if positions.len() != exception_count || exceptions.len() != exception_count {
                return Err(Error::internal("a sparse chunk disagrees about its exception count"));
            }
            let mut values = vec![value; count];
            for (position, exception) in positions.into_iter().zip(exceptions) {
                let position = usize::try_from(position)
                    .ok()
                    .filter(|position| *position < count)
                    .ok_or_else(|| {
                        Error::internal(format!("exception at {position} is outside the chunk"))
                    })?;
                values[position] = exception;
            }
            Ok(values)
        }
        Kind::Strided => {
            let base = reader.i64()?;
            let stride = reader.u64()?;
            let steps = decode_chunk(reader)?;
            check_count(steps.len(), count)?;
            strided(steps, stride, base)
        }
    }
}

/// The values of a strided chunk, `base` plus each step times `stride`, written over the steps.
///
/// Over the steps rather than into a run of their own, so a chunk is one allocation rather than
/// two, and with the one check a chunk needs taken over the whole run first, so the loop that makes
/// the values has nothing in it but a multiply and an add and the compiler does it four lanes at a
/// time. A value pushed at a time with the check inside was about thirteen instructions a value,
/// and a decimal column of whole numbers, which TPC-H's `l_quantity` is, is stored this way.
fn strided(mut steps: Vec<i64>, stride: u64, base: i64) -> Result<Vec<i64>> {
    // Every bit of every step or'ed together has its sign bit set exactly when some step is negative.
    if steps.iter().fold(0, |held, &step| held | step) < 0 {
        return Err(Error::internal("a negative number of strides"));
    }
    for step in &mut steps {
        *step = base.wrapping_add((*step as u64).wrapping_mul(stride) as i64);
    }
    Ok(steps)
}

/// The runs of an RLE chunk laid out one after another, in the type the chunk is decoded into.
///
/// Every check a run could fail is made once over all of them before anything is written: that no
/// length is negative, that the lengths add up to the chunk, and that the lowest and highest value
/// fit `T`. Made a run at a time they were a conversion, a checked add and a lane check between
/// every two writes, about twenty five instructions a run against the four stores of the run
/// itself, and `l_orderkey` is a million and a half runs. After the checks every run lands inside
/// the chunk, so the loop is the write and the step.
///
/// A chunk whose runs are long on average, the way a sorted column's are thousands of rows each,
/// has every run appended into reserved room, so that each value is written once by the run it
/// belongs to. Zeroing the chunk first was a second write of all of it. Short runs are cheaper the
/// other way, with room for one run past the end so that the write never has to ask how much of
/// its fixed width landed inside the chunk, and appending those cost a few percent more on
/// ClickBench 15, 17 and 31.
fn expanded<T: Lane>(run_values: &[i64], run_lengths: &[i64], count: usize) -> Result<Vec<T>> {
    if run_values.len() != run_lengths.len() {
        return Err(Error::internal("an RLE chunk has more runs than run lengths"));
    }
    let (signs, longest, total) =
        run_lengths.iter().fold((0, 0, 0u128), |(signs, longest, total), &length| {
            (signs | length, longest.max(length), total + u128::from(length as u64))
        });
    if signs < 0 {
        return Err(Error::internal("a negative RLE run length"));
    }
    if total > count as u128 {
        return Err(Error::internal("an RLE run ends past its chunk"));
    }
    check_count(total as usize, count)?;
    // A type that holds all of `i64` needs no look at the values, and the test folds away for it.
    let wide = T::fit(i64::MIN).is_some() && T::fit(i64::MAX).is_some();
    if !wide {
        let (low, high) = run_values
            .iter()
            .fold((i64::MAX, i64::MIN), |(low, high), &value| (low.min(value), high.max(value)));
        if !run_values.is_empty() {
            lane::<T>(low)?;
            lane::<T>(high)?;
        }
    }
    let runs = run_values.iter().zip(run_lengths);
    if run_values.len().saturating_mul(LONG_RUN) <= count {
        let mut values = Vec::with_capacity(count);
        for (&value, &length) in runs {
            values.resize(values.len() + length as usize, T::wrap(value));
        }
        return Ok(values);
    }
    let mut values = vec![T::default(); count + RUN];
    let mut at = 0;
    if longest as usize <= RUN {
        for (&value, &length) in runs {
            values[at..at + RUN].fill(T::wrap(value));
            at += length as usize;
        }
    } else {
        // Whole blocks of [`RUN`] rather than a fill of the run's own length, since a fill of a
        // length the compiler cannot see picks its store width at run time, and on ClickBench 23 that
        // choice was more of the loop than the stores. A run is rounded up to blocks, which the room
        // past the end of the chunk and the run after it both absorb.
        for (&value, &length) in runs {
            let length = length as usize;
            let room = length.next_multiple_of(RUN).max(RUN);
            for block in values[at..at + room].as_chunks_mut::<RUN>().0 {
                block.fill(T::wrap(value));
            }
            at += length;
        }
    }
    values.truncate(count);
    Ok(values)
}

/// [`decode_chunk`] into `T`. See [`decode_as`].
fn decode_chunk_as<T: Lane>(reader: &mut Reader<'_>) -> Result<Vec<T>> {
    let Some(&tag) = reader.rest().first() else {
        return Err(Error::internal("a chunk ended before its encoding tag"));
    };
    match Kind::from_tag(tag)? {
        Kind::Constant | Kind::Packed | Kind::Sparse | Kind::Rle => {}
        // Every other kind is made wide as [`decode`] makes it and narrowed after, with the range of
        // the chunk taken once so that a chunk whose ends fit is narrowed with no check a value.
        //
        // The check a value is worth taking out twice over. It is the check itself, and it is that a
        // narrowing that cannot fail is a `Vec<i64>` walked into a `Vec<T>` of the same length, which
        // the standard library does in the allocation the wide values arrived in when the two widths
        // match. A `BIGINT` column is the case where they always match, so the whole narrowing is a
        // walk over a vector that stays where it is, where the checked form allocated a second
        // vector and copied every row into it.
        _ => {
            let values = decode_chunk(reader)?;
            let (low, high) = values.iter().fold((i64::MAX, i64::MIN), |(low, high), &value| {
                (low.min(value), high.max(value))
            });
            if values.is_empty() || T::fit(low).is_some() && T::fit(high).is_some() {
                return Ok(values.into_iter().map(T::wrap).collect());
            }
            return values.into_iter().map(lane).collect();
        }
    }
    let kind = Kind::from_tag(reader.u8()?)?;
    let count = reader.u32()? as usize;
    match kind {
        Kind::Constant => Ok(vec![lane(reader.i64()?)?; count]),
        Kind::Packed => {
            let mut values = vec![T::default(); count];
            let mut wide = [0i64; VALUES];
            let mut done = 0;
            while done < count {
                let base = reader.i64()?;
                let width = reader.u8()? as usize;
                let wanted = (count - done).min(VALUES);
                let into = &mut values[done..done + wanted];
                let whole = wanted == VALUES;
                let bytes = if whole {
                    reader.bytes(bitpack::unit_len(width))?
                } else {
                    reader.bytes(bitpack::tail_len(wanted, width))?
                };
                // Every value of a block is between its base and the base plus the widest offset its
                // width holds, so when both ends fit the whole block does and the unpack writes the
                // target type with no check a value. A block that could hold more than the type,
                // which a width rounded up past the range can, is unpacked wide and checked.
                let mask = if width >= 64 { u64::MAX } else { (1u64 << width) - 1 };
                let top = i64::try_from(i128::from(base) + i128::from(mask)).ok();
                if T::fit(base).is_some() && top.and_then(T::fit).is_some() {
                    let map = |offset| T::wrap(value_from(offset, base));
                    if whole {
                        bitpack::unpack_unit_into(bytes, width, into, map)?;
                    } else {
                        bitpack::unpack_tail_into(bytes, width, into, map)?;
                    }
                } else {
                    let wide = &mut wide[..wanted];
                    let map = |offset| value_from(offset, base);
                    if whole {
                        bitpack::unpack_unit_into(bytes, width, wide, map)?;
                    } else {
                        bitpack::unpack_tail_into(bytes, width, wide, map)?;
                    }
                    for (value, &held) in into.iter_mut().zip(wide.iter()) {
                        *value = lane(held)?;
                    }
                }
                done += wanted;
            }
            Ok(values)
        }
        Kind::Rle => {
            let run_values = decode_chunk(reader)?;
            let run_lengths = decode_chunk(reader)?;
            expanded(&run_values, &run_lengths, count)
        }
        Kind::Sparse => {
            let value = lane::<T>(reader.i64()?)?;
            let exception_count = reader.u32()? as usize;
            let positions = decode_chunk(reader)?;
            let exceptions = decode_chunk(reader)?;
            if positions.len() != exception_count || exceptions.len() != exception_count {
                return Err(Error::internal("a sparse chunk disagrees about its exception count"));
            }
            let mut values = vec![value; count];
            for (position, exception) in positions.into_iter().zip(exceptions) {
                let position = usize::try_from(position)
                    .ok()
                    .filter(|position| *position < count)
                    .ok_or_else(|| {
                        Error::internal(format!("exception at {position} is outside the chunk"))
                    })?;
                values[position] = lane(exception)?;
            }
            Ok(values)
        }
        Kind::Delta | Kind::Dict | Kind::Strided => {
            Err(Error::internal("a chunk kind that decodes wide reached the narrow decoder"))
        }
    }
}

fn decode_selected_chunk(reader: &mut Reader<'_>, positions: &[usize]) -> Result<Vec<i64>> {
    let Some(&tag) = reader.rest().first() else {
        return Err(Error::internal("a chunk ended before its encoding tag"));
    };
    let kind = Kind::from_tag(tag)?;
    if !matches!(kind, Kind::Constant | Kind::Packed | Kind::Rle | Kind::Strided | Kind::Dict) {
        let values = decode_chunk(reader)?;
        return positions
            .iter()
            .map(|&position| {
                values.get(position).copied().ok_or_else(|| {
                    Error::internal(format!(
                        "selected integer position {position} is outside {} values",
                        values.len()
                    ))
                })
            })
            .collect();
    }

    let decoded = Kind::from_tag(reader.u8()?)?;
    debug_assert_eq!(decoded, kind);
    let count = reader.u32()? as usize;
    if positions.last().is_some_and(|&position| position >= count) {
        return Err(Error::internal(format!(
            "selected integer position {} is outside {count} values",
            positions.last().expect("a last position exists")
        )));
    }
    match kind {
        Kind::Constant => {
            let value = reader.i64()?;
            Ok(vec![value; positions.len()])
        }
        Kind::Packed => {
            let mut out = Vec::with_capacity(positions.len());
            let mut from = 0;
            let mut done = 0;
            while done < count {
                let base = reader.i64()?;
                let width = reader.u8()? as usize;
                let wanted = (count - done).min(VALUES);
                let upto = positions.partition_point(|&position| position < done + wanted);
                if wanted == VALUES && (upto - from) * SPARSE > VALUES {
                    // Enough of the unit is wanted that unpacking all of it is cheaper than
                    // finding each value on its own.
                    let bytes = reader.bytes(bitpack::unit_len(width))?;
                    let mut unit = [0_i64; VALUES];
                    bitpack::unpack_unit_into(bytes, width, &mut unit, |offset| {
                        value_from(offset, base)
                    })?;
                    out.extend(positions[from..upto].iter().map(|&position| unit[position - done]));
                } else if wanted == VALUES {
                    let bytes = reader.bytes(bitpack::unit_len(width))?;
                    bitpack::unpack_u64_each(
                        bytes,
                        width,
                        positions[from..upto].iter().map(|&position| position - done),
                        |offset| out.push(value_from(offset, base)),
                    )?;
                } else {
                    let bytes = reader.bytes(bitpack::tail_len(wanted, width))?;
                    for &position in &positions[from..upto] {
                        let offset = bitpack::tail_at(bytes, width, position - done)?;
                        out.push(value_from(offset, base));
                    }
                }
                from = upto;
                done += wanted;
            }
            Ok(out)
        }
        Kind::Rle => {
            let run_value_bytes = reader.rest();
            let mut run_value_reader = Reader::new(run_value_bytes);
            let run_value_count = skip_chunk(&mut run_value_reader)?;
            let run_value_len = run_value_reader.used();
            reader.skip(run_value_len)?;
            let mut ends = decode_chunk(reader)?;
            if run_value_count != ends.len() {
                return Err(Error::internal("an RLE chunk has more runs than run lengths"));
            }
            // The lengths summed in place into where each run ends, with their signs gathered on
            // the way so that one test after the loop stands for a test a run. A sum that would
            // pass the top of `i64` stops there and then disagrees with the row count.
            let mut signs = 0i64;
            let mut end = 0i64;
            for length in &mut ends {
                signs |= *length;
                end = end.saturating_add(*length);
                *length = end;
            }
            if signs < 0 {
                return Err(Error::internal("a negative RLE run length"));
            }
            check_count(usize::try_from(end).unwrap_or(usize::MAX), count)?;
            // The last run ends at the row count, which is past every position, so the walk
            // below always stops at a run. A run of no rows ends where the one before it did and
            // is walked past.
            let run_values = &run_value_bytes[..run_value_len];
            if !pointed(run_values) {
                // Run values that cannot be read at a few runs, which for the order key of TPC-H
                // lineitem are deltas, are decoded once and read straight out by run.
                let values = decode_chunk(&mut Reader::new(run_values))?;
                check_count(values.len(), run_value_count)?;
                let mut out = Vec::with_capacity(positions.len());
                let mut run = 0;
                for &position in positions {
                    let position = position as i64;
                    while ends[run] <= position {
                        run += 1;
                    }
                    out.push(values[run]);
                }
                return Ok(out);
            }
            // Each position's run, as its place among the runs wanted. The values are then read
            // once per run and put back per position.
            let mut wanted_runs = Vec::new();
            let mut places = Vec::with_capacity(positions.len());
            let mut run = 0;
            for &position in positions {
                let position = position as i64;
                while ends[run] <= position {
                    run += 1;
                }
                if wanted_runs.last() != Some(&run) {
                    wanted_runs.push(run);
                }
                // Under the number of positions, which is under a part's rows.
                places.push((wanted_runs.len() - 1) as u32);
            }
            let run_values = decode_selected(run_values, &wanted_runs)?;
            Ok(places.into_iter().map(|place| run_values[place as usize]).collect())
        }
        // The steps and the codes are chunks of their own, read at the same rows, and the dictionary
        // is read whole since a code can point anywhere in it.
        Kind::Strided => {
            let base = reader.i64()?;
            let stride = reader.u64()?;
            let steps = decode_selected_chunk(reader, positions)?;
            steps
                .into_iter()
                .map(|step| {
                    let step = u64::try_from(step)
                        .map_err(|_| Error::internal("a negative number of strides"))?;
                    Ok(value_from(step.wrapping_mul(stride), base))
                })
                .collect()
        }
        Kind::Dict => {
            let dictionary = decode_chunk(reader)?;
            let codes = decode_selected_chunk(reader, positions)?;
            codes
                .into_iter()
                .map(|code| {
                    usize::try_from(code)
                        .ok()
                        .and_then(|index| dictionary.get(index))
                        .copied()
                        .ok_or_else(|| {
                            Error::internal(format!("code {code} is not in the dictionary"))
                        })
                })
                .collect()
        }
        _ => unreachable!("unsupported kinds used the full decoder"),
    }
}

/// Advances over one encoded chunk without materializing its values and returns its row count.
fn skip_chunk(reader: &mut Reader<'_>) -> Result<usize> {
    let kind = Kind::from_tag(reader.u8()?)?;
    let count = reader.u32()? as usize;
    match kind {
        Kind::Constant => reader.skip(8)?,
        Kind::Packed => skip_packed(reader, count)?,
        Kind::Delta => {
            reader.skip(8)?;
            skip_chunk(reader)?;
        }
        Kind::Rle | Kind::Dict => {
            skip_chunk(reader)?;
            skip_chunk(reader)?;
        }
        Kind::Sparse => {
            reader.skip(12)?;
            skip_chunk(reader)?;
            skip_chunk(reader)?;
        }
        Kind::Strided => {
            reader.skip(16)?;
            skip_chunk(reader)?;
        }
    }
    Ok(count)
}

/// Advances over the units of a `Packed` body of `count` values.
fn skip_packed(reader: &mut Reader<'_>, count: usize) -> Result<()> {
    let mut done = 0;
    while done < count {
        reader.skip(8)?;
        let width = reader.u8()? as usize;
        if width > 64 {
            return Err(Error::internal(format!("a packed integer width of {width} is past 64")));
        }
        let wanted = (count - done).min(VALUES);
        let bytes = if wanted == VALUES {
            bitpack::packed_len::<u64>(width)
                .checked_mul(8)
                .ok_or_else(|| Error::internal("packed integer size overflow"))?
        } else {
            bitpack::tail_len(wanted, width)
        };
        reader.skip(bytes)?;
        done += wanted;
    }
    Ok(())
}

/// [`shape`] for one chunk and everything inside it.
fn shape_chunk(reader: &mut Reader<'_>, kinds: &mut Vec<Kind>) -> Result<()> {
    let kind = Kind::from_tag(reader.u8()?)?;
    let count = reader.u32()? as usize;
    kinds.push(kind);
    match kind {
        Kind::Constant => reader.skip(8)?,
        Kind::Packed => skip_packed(reader, count)?,
        Kind::Delta => {
            reader.skip(8)?;
            shape_chunk(reader, kinds)?;
        }
        Kind::Rle | Kind::Dict => {
            shape_chunk(reader, kinds)?;
            shape_chunk(reader, kinds)?;
        }
        Kind::Sparse => {
            reader.skip(12)?;
            shape_chunk(reader, kinds)?;
            shape_chunk(reader, kinds)?;
        }
        Kind::Strided => {
            reader.skip(16)?;
            shape_chunk(reader, kinds)?;
        }
    }
    Ok(())
}

fn describe_chunk(reader: &mut Reader<'_>) -> Result<String> {
    let kind = Kind::from_tag(reader.u8()?)?;
    let count = reader.u32()? as usize;
    Ok(match kind {
        Kind::Constant => {
            reader.i64()?;
            "CONSTANT".to_string()
        }
        Kind::Packed => {
            let mut widths = Vec::new();
            let mut seen = 0;
            while seen < count {
                reader.i64()?;
                let width = reader.u8()? as usize;
                let wanted = (count - seen).min(VALUES);
                if wanted == VALUES {
                    for _ in 0..bitpack::packed_len::<u64>(width) {
                        reader.u64()?;
                    }
                } else {
                    reader.bytes(bitpack::tail_len(wanted, width))?;
                }
                widths.push(width);
                seen += wanted;
            }
            let low = widths.iter().copied().min().unwrap_or(0);
            let high = widths.iter().copied().max().unwrap_or(0);
            // Square brackets rather than round ones, so that a reader and a test can both take a
            // parenthesis to mean one more level of cascade and nothing else.
            if low == high {
                format!("FOR+BITPACK[{low}]")
            } else {
                format!("FOR+BITPACK[{low}..{high}]")
            }
        }
        Kind::Delta => {
            reader.i64()?;
            format!("DELTA({})", describe_chunk(reader)?)
        }
        Kind::Rle => {
            let values = describe_chunk(reader)?;
            let lengths = describe_chunk(reader)?;
            format!("RLE({values}, {lengths})")
        }
        Kind::Dict => {
            let dictionary = describe_chunk(reader)?;
            let codes = describe_chunk(reader)?;
            format!("DICT({dictionary}, {codes})")
        }
        Kind::Sparse => {
            reader.i64()?;
            reader.u32()?;
            let positions = describe_chunk(reader)?;
            let exceptions = describe_chunk(reader)?;
            format!("SPARSE({positions}, {exceptions})")
        }
        Kind::Strided => {
            reader.i64()?;
            let stride = reader.u64()?;
            format!("STRIDE[{stride}]({})", describe_chunk(reader)?)
        }
    })
}

/// The step every value of the chunk is a whole number of, or `None` when there is not one worth
/// having.
///
/// This is the greatest common divisor of every value's distance from the smallest one. A timestamp
/// column loaded from a source that recorded whole seconds holds microseconds that are all multiples
/// of a million, and without this the frame of reference pays twenty bits a value to write down the
/// twenty zero bits at the bottom of every one of them.
///
/// The walk stops the moment the divisor reaches one, which is what makes this affordable to ask on
/// every chunk. Two values that share no factor are enough to answer, and on a column of arbitrary
/// numbers that is almost always the first pair.
fn stride_of(values: &[i64]) -> Option<u64> {
    stride_from(values, values.iter().min().copied()?)
}

/// [`stride_of`] for a chunk whose smallest value is already known.
fn stride_from(values: &[i64], base: i64) -> Option<u64> {
    let mut divisor = 0u64;
    for value in values {
        divisor = gcd(divisor, offset_from(*value, base));
        if divisor == 1 {
            return None;
        }
    }
    // Zero is every value being the base, which `Constant` already holds for nothing, and one is
    // the frame of reference on its own with two extra words of header.
    (divisor > 1).then_some(divisor)
}

/// Binary GCD, which is the one without a division in it.
fn gcd(mut left: u64, mut right: u64) -> u64 {
    if left == 0 {
        return right;
    }
    if right == 0 {
        return left;
    }
    let shift = (left | right).trailing_zeros();
    left >>= left.trailing_zeros();
    loop {
        right >>= right.trailing_zeros();
        if left > right {
            std::mem::swap(&mut left, &mut right);
        }
        right -= left;
        if right == 0 {
            return left << shift;
        }
    }
}

/// The distance from the frame of reference base, which is always representable in a `u64` because
/// both ends came from an `i64` and the width of the difference is at most 65 bits minus the sign.
fn offset_from(value: i64, base: i64) -> u64 {
    (i128::from(value) - i128::from(base)) as u64
}

fn value_from(offset: u64, base: i64) -> i64 {
    (i128::from(base) + i128::from(offset)) as i64
}

/// Zigzag, so that a column that counts down packs as narrowly as one that counts up. Without it a
/// delta of -1 is 64 bits of ones.
fn zigzag(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)) as u64
}

fn unzigzag(value: u64) -> i64 {
    ((value >> 1) as i64) ^ -((value & 1) as i64)
}

/// The zigzagged differences, or `None` if any difference is too wide to be one.
///
/// A column holding both `i64::MIN` and `i64::MAX` has a difference that does not fit in an `i64`,
/// and rather than widening every delta array to 128 bits for a case that does not occur in data,
/// the encoding declines to apply. `Packed` covers it.
/// Whether every neighbouring difference fits in an `i64`, which is the only thing the candidate
/// list needs to know about deltas.
///
/// The candidate list used to answer this by building the whole delta array and checking that it
/// came back, which is an allocation and a pass over the chunk thrown away on every chunk, and then
/// `Kind::Delta` built it again. This is the same pass with nothing kept.
fn deltas_fit(values: &[i64]) -> bool {
    values.windows(2).all(|pair| pair[1].checked_sub(pair[0]).is_some())
}

fn deltas(values: &[i64]) -> Option<Vec<i64>> {
    let mut deltas = Vec::with_capacity(values.len().saturating_sub(1));
    for pair in values.windows(2) {
        let difference = pair[1].checked_sub(pair[0])?;
        deltas.push(zigzag(difference) as i64);
    }
    Some(deltas)
}

/// The value and the length of every run of equal neighbours.
///
/// Each run is found by walking to its end and pushed once. This used to push the first value of a
/// run and then add one to the last length for every value after it, which kept both vectors'
/// lengths in memory across the whole loop and was the hottest loop left in `encode_at` once the
/// candidate tests became one pass.
fn runs(values: &[i64]) -> (Vec<i64>, Vec<i64>) {
    let mut run_values: Vec<i64> = Vec::new();
    let mut run_lengths: Vec<i64> = Vec::new();
    let mut start = 0;
    while let Some(&value) = values.get(start) {
        let length = values[start..].iter().take_while(|other| **other == value).count();
        run_values.push(value);
        run_lengths.push(length as i64);
        start += length;
    }
    (run_values, run_lengths)
}

/// The distinct values in sorted order.
///
/// Sorted rather than in order of first appearance, because an ordered dictionary is what lets a
/// range predicate become a code range instead of a code set, per section 6.7, and because the
/// codes of a clustered column then run in order and delta encode.
/// How many distinct values there are and which one occurs most often, from one sort.
///
/// Both questions are about the histogram of the chunk and neither needs the histogram itself, so
/// one sorted copy and one walk over it answers both. They used to be two functions that each sorted
/// their own copy and threw it away, which is a chunk sorted twice on every chunk at every level of
/// the cascade before a single candidate has been encoded.
///
/// No hash map, because the sort is what makes the walk a scan of equal runs, and a hash map would
/// pay a lookup per value to learn the same thing.
fn spread_of(values: &[i64]) -> (usize, Option<(i64, usize)>) {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let mut distinct = 0;
    let mut best: Option<(i64, usize)> = None;
    let mut index = 0;
    while index < sorted.len() {
        let value = sorted[index];
        let mut end = index;
        while end < sorted.len() && sorted[end] == value {
            end += 1;
        }
        distinct += 1;
        let count = end - index;
        if best.is_none_or(|(_, seen)| count > seen) {
            best = Some((value, count));
        }
        index = end;
    }
    (distinct, best)
}

/// The value more than half of the chunk holds, and how many times, found in two passes without
/// sorting anything.
///
/// This is the vote that keeps one candidate and a lead: a value that holds more than half the
/// chunk outlasts every other value put together, so it is the candidate left at the end, and the
/// second pass checks that the candidate really does hold more than half. When it does it is the
/// value [`spread_of`] would name as the most frequent, since a value over half the chunk has no tie.
fn majority(values: &[i64]) -> Option<(i64, usize)> {
    let mut candidate = *values.first()?;
    let mut lead = 0usize;
    for value in values {
        if lead == 0 {
            candidate = *value;
            lead = 1;
        } else if *value == candidate {
            lead += 1;
        } else {
            lead -= 1;
        }
    }
    let count = values.iter().filter(|value| **value == candidate).count();
    (count * 2 > values.len()).then_some((candidate, count))
}

/// The distinct values in sorted order, for the same reason the string dictionary is sorted: an
/// ordered dictionary turns a range predicate into a code range rather than a code set.
fn distinct_values(values: &[i64]) -> Vec<i64> {
    let mut distinct = values.to_vec();
    distinct.sort_unstable();
    distinct.dedup();
    distinct
}

/// Where each value sits in the dictionary.
///
/// The string side builds its dictionary and its codes together from one sort of a permutation,
/// because the alternative there is a copy of every value onto the heap and a `memcmp` per level of
/// a binary search per row. This side was changed to match and it measured slower, so it was changed
/// back. An integer dictionary only exists when the distinct count is at most half the row count, so
/// the search is over something small and cache resident, the comparison is one integer rather than
/// a string, and carrying the source index through the sort means sorting a padded sixteen byte pair
/// instead of an eight byte value. The search is cheaper than the wider sort.
fn codes_over(values: &[i64], dictionary: &[i64]) -> Vec<i64> {
    values
        .iter()
        .map(|value| {
            dictionary
                .binary_search(value)
                .expect("the dictionary is the distinct values of this chunk") as i64
        })
        .collect()
}

fn check_count(actual: usize, expected: usize) -> Result<()> {
    if actual == expected {
        Ok(())
    } else {
        Err(Error::internal(format!(
            "a chunk says it holds {expected} values and decoded to {actual}"
        )))
    }
}

fn too_long(len: usize) -> Error {
    Error::internal(format!("a chunk of {len} values is longer than the format allows"))
}

fn put_u8(out: &mut Vec<u8>, value: u8) {
    out.push(value);
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_i64(out: &mut Vec<u8>, value: i64) {
    out.extend_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(values: &[i64]) -> Vec<u8> {
        let bytes = encode(values).unwrap();
        assert_eq!(decode(&bytes).unwrap(), values, "{}", describe(&bytes).unwrap());
        bytes
    }

    fn kind_of(bytes: &[u8]) -> Kind {
        Kind::from_tag(bytes[0]).unwrap()
    }

    /// The same xorshift the bit packing tests use, for the same reason.
    struct Random(u64);

    impl Random {
        fn new() -> Self {
            Self(0x9e37_79b9_7f4a_7c15)
        }

        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    #[test]
    fn the_dictionary_is_sorted_and_the_codes_point_back_at_the_values() {
        let values = vec![30i64, 10, 30, 20, 10, -5];
        let dictionary = distinct_values(&values);
        let codes = codes_over(&values, &dictionary);
        assert_eq!(dictionary, vec![-5, 10, 20, 30]);
        assert_eq!(codes, vec![3, 1, 3, 2, 1, 0]);
        for (code, value) in codes.iter().zip(&values) {
            assert_eq!(dictionary[*code as usize], *value);
        }
    }

    #[test]
    fn one_sort_gives_the_distinct_count_and_the_most_frequent_value() {
        let values = vec![7i64, 7, 7, 1, 2, 2];
        assert_eq!(spread_of(&values), (3, Some((7, 3))));
        assert_eq!(spread_of(&[]), (0, None));
        assert_eq!(spread_of(&[9]), (1, Some((9, 1))));

        // A tie goes to the value that sorts first, which is arbitrary but has to be stable,
        // because Sparse writes the dominant value into the chunk and the size depends on it.
        assert_eq!(spread_of(&[4i64, 4, 8, 8]), (2, Some((4, 2))));
    }

    #[test]
    fn the_majority_is_the_most_frequent_value_whenever_there_is_one() {
        let chunks: Vec<Vec<i64>> = vec![
            vec![],
            vec![3],
            vec![1, 2],
            vec![1, 1, 2],
            vec![2, 1, 1],
            vec![4, 4, 8, 8],
            vec![7, 1, 7, 2, 7, 3, 7],
            vec![1, 2, 3, 9, 9, 9, 9],
            (0..1000).map(|index| if index % 5 == 0 { index } else { -4 }).collect(),
            (0..1000).map(|index| index % 3).collect(),
        ];
        for chunk in chunks {
            let (_, dominant) = spread_of(&chunk);
            let expected = dominant.filter(|(_, count)| count * 2 > chunk.len());
            assert_eq!(majority(&chunk), expected, "{chunk:?}");
        }
    }

    /// The one pass offers exactly what the separate tests offered, including on the chunks where
    /// the shortcuts in it are the whole answer: a range too wide for an `i64`, and a chunk with too
    /// many runs to have a value in four rows out of five.
    #[test]
    fn the_one_pass_offers_what_the_separate_tests_offered() {
        let mut random = Random::new();
        let mut chunks: Vec<Vec<i64>> = vec![
            vec![],
            vec![5],
            vec![5, 5, 5],
            vec![i64::MIN, i64::MAX],
            vec![i64::MAX, i64::MIN, i64::MAX],
            vec![i64::MIN, 0, i64::MAX],
            vec![-1, i64::MAX],
            (0..1000).map(|index| if index % 5 == 0 { index } else { -4 }).collect(),
            (0..1000).map(|index| if index % 4 == 0 { index } else { -4 }).collect(),
            (0..1000).map(|index| index / 7).collect(),
            (0..1000).map(|index| index * 1_000_000).collect(),
        ];
        for _ in 0..200 {
            let len = (random.next() % 300) as usize;
            let spread = 1 + random.next() % 8;
            let common = (random.next() % 5) as i64;
            chunks.push(
                (0..len)
                    .map(|_| {
                        let draw = random.next();
                        if draw % 10 < spread { (draw >> 8) as i64 % 50 } else { common }
                    })
                    .collect(),
            );
        }
        for chunk in chunks {
            let mut expected = vec![Kind::Packed];
            if !chunk.is_empty() {
                if chunk.iter().all(|value| *value == chunk[0]) {
                    expected = vec![Kind::Constant];
                } else {
                    let runs = 1 + chunk.windows(2).filter(|pair| pair[0] != pair[1]).count();
                    let low = *chunk.iter().min().unwrap();
                    let high = *chunk.iter().max().unwrap();
                    let bits = |range: u128| 128 - range.leading_zeros();
                    let width = bits((i128::from(high) - i128::from(low)) as u128);
                    let zigzags: Vec<u128> = chunk
                        .windows(2)
                        .map(|pair| u128::from(zigzag(pair[1].wrapping_sub(pair[0]))))
                        .collect();
                    let spread = zigzags.iter().max().unwrap() - zigzags.iter().min().unwrap();
                    let turns = 1 + zigzags.windows(2).filter(|pair| pair[0] != pair[1]).count();
                    let mut first: Vec<u128> = zigzags.iter().take(64).copied().collect();
                    first.sort_unstable();
                    first.dedup();
                    let pays = bits(spread) < width
                        || (runs * 4 > chunk.len() * 3
                            && (turns * 4 <= (chunk.len() - 1) * 3 || first.len() <= 4));
                    if deltas_fit(&chunk) && pays {
                        expected.push(Kind::Delta);
                    }
                    if runs * 4 <= chunk.len() * 3 {
                        expected.push(Kind::Rle);
                    }
                    let distinct = spread_of(&chunk).0;
                    if distinct * 2 <= chunk.len() && bits(distinct as u128 - 1) < width {
                        expected.push(Kind::Dict);
                    }
                    if majority(&chunk).is_some_and(|(_, count)| count * 10 >= chunk.len() * 8) {
                        expected.push(Kind::Sparse);
                    }
                    if stride_of(&chunk).is_some() {
                        expected.push(Kind::Strided);
                    }
                }
            }
            assert_eq!(candidates(&chunk, 0, &EXHAUSTIVE), expected, "{chunk:?}");
        }
    }

    #[test]
    fn runs_are_every_stretch_of_equal_neighbours_in_order() {
        assert_eq!(runs(&[]), (vec![], vec![]));
        assert_eq!(runs(&[4]), (vec![4], vec![1]));
        assert_eq!(runs(&[1, 1, 2, 1, 1, 1]), (vec![1, 2, 1], vec![2, 1, 3]));
    }

    #[test]
    fn deltas_that_do_not_fit_are_refused_before_they_are_built() {
        assert!(deltas_fit(&[1i64, 2, 3]));
        assert!(deltas_fit(&[i64::MAX, i64::MAX]));
        assert!(!deltas_fit(&[i64::MIN, i64::MAX]));
        assert_eq!(deltas_fit(&[i64::MIN, i64::MAX]), deltas(&[i64::MIN, i64::MAX]).is_some());
        assert_eq!(deltas_fit(&[1i64, 2, 3]), deltas(&[1i64, 2, 3]).is_some());
    }

    #[test]
    fn what_the_chooser_returns_is_the_smallest_of_what_it_was_offered() {
        // `offered` and `encode_only` are what `cargo xtask encode` splits the chooser's seconds
        // with, so they have to describe the chooser that actually runs rather than a second copy
        // of its rules that drifts. This is the assertion that keeps the two the same thing.
        let mut random = Random::new();
        let noise: Vec<i64> = (0..2000).map(|_| (random.next() % 5000) as i64).collect();
        let runs: Vec<i64> = (0..2000).map(|index: i64| index / 100).collect();
        let climbing: Vec<i64> = (0..2000).map(|index| 1_700_000_000 + index).collect();
        for values in [noise, runs, climbing, vec![7; 300], Vec::new()] {
            let chosen = encode(&values).unwrap();
            let mut smallest: Option<Vec<u8>> = None;
            for kind in offered(&values) {
                let Some(bytes) = encode_only(kind, &values).unwrap() else {
                    continue;
                };
                if smallest.as_ref().is_none_or(|best| bytes.len() < best.len()) {
                    smallest = Some(bytes);
                }
            }
            assert_eq!(smallest.as_deref(), Some(chosen.as_slice()), "{}", values.len());
        }
    }

    #[test]
    fn a_column_of_whole_seconds_in_microseconds_pays_nothing_for_the_zeroes() {
        // What three ClickBench columns are. `epoch_ms(EventTime * 1000)` on a source that recorded
        // whole seconds gives microseconds with twenty zero bits under every value, and a frame of
        // reference over a part that spans a working day needs 36 bits to write them down.
        let mut random = Random::new();
        let day = 1_374_000_000_000_000i64;
        let values: Vec<i64> =
            (0..100_000).map(|_| day + (random.next() % 68_400) as i64 * 1_000_000).collect();
        let bytes = round_trip(&values);
        assert_eq!(kind_of(&bytes), Kind::Strided);
        assert!(describe(&bytes).unwrap().starts_with("STRIDE[1000000]"), "{:?}", describe(&bytes));
        assert!(is_strided(&bytes), "a reader asks this to hold the column packed");
        // 17 bits a value for the range of seconds, against the 36 the microseconds need.
        let strided = 100_000 * 17 / 8;
        assert!(bytes.len() < strided + 2000, "{} bytes for {strided} of payload", bytes.len());

        let plain = encode_only(Kind::Packed, &values).unwrap().expect("packing always applies");
        assert!(
            bytes.len() * 2 < plain.len(),
            "{} strided against {} packed",
            bytes.len(),
            plain.len()
        );
    }

    #[test]
    fn a_stride_is_the_common_factor_of_the_distances_from_the_smallest_value() {
        assert_eq!(stride_of(&[10i64, 20, 40]), Some(10));
        // The base is the smallest value and not zero, so a column that does not start on a
        // multiple of its own step still has one.
        assert_eq!(stride_of(&[7i64, 17, 37]), Some(10));
        assert_eq!(stride_of(&[10i64, 20, 23]), None);
        // Every value the same is `Constant`'s case and this declines it rather than dividing by a
        // stride of zero.
        assert_eq!(stride_of(&[5i64; 100]), None);
        assert_eq!(stride_of(&[]), None);
        // The two ends of the type, where the distance needs 65 bits and only a `u64` holds it.
        assert_eq!(stride_of(&[i64::MIN, i64::MAX]), Some(u64::MAX));
    }

    #[test]
    fn a_stride_across_the_whole_of_the_type_round_trips() {
        // The distance is 65 bits, so the step count is one and the offset it comes back as is a
        // number no `i64` holds. This is the arithmetic the encoder has to do in `u64`.
        for values in [vec![i64::MIN, i64::MAX], vec![i64::MIN, 0, i64::MAX]] {
            let bytes = round_trip(&values);
            assert_eq!(decode(&bytes).unwrap(), values);
        }
    }

    #[test]
    fn a_column_with_no_common_factor_is_not_offered_a_stride() {
        let mut random = Random::new();
        let values: Vec<i64> = (0..2000).map(|_| (random.next() % 1_000_000) as i64).collect();
        assert!(!offered(&values).contains(&Kind::Strided));
        assert!(encode_only(Kind::Strided, &values).unwrap().is_none());
    }

    #[test]
    fn an_empty_chunk_round_trips() {
        let bytes = round_trip(&[]);
        assert_eq!(bytes.len(), 5);
    }

    #[test]
    fn a_constant_column_costs_thirteen_bytes_however_long_it_is() {
        let bytes = round_trip(&vec![42; 1_000_000]);
        assert_eq!(kind_of(&bytes), Kind::Constant);
        assert_eq!(bytes.len(), 13);
    }

    #[test]
    fn a_narrow_range_is_packed_at_the_width_of_the_range_and_not_of_the_type() {
        // 100_000 values between 1000 and 1063 is 6 bits each, plus 9 bytes of header per 1024.
        let mut random = Random::new();
        let values: Vec<i64> = (0..100_000).map(|_| 1000 + (random.next() % 64) as i64).collect();
        let bytes = round_trip(&values);
        assert_eq!(kind_of(&bytes), Kind::Packed);
        let packed = 100_000 * 6 / 8;
        assert!(bytes.len() < packed + 2000, "{} bytes for {packed} of payload", bytes.len());
        assert!(bytes.len() > packed, "{} bytes cannot hold {packed}", bytes.len());
    }

    #[test]
    fn a_counter_becomes_deltas_and_then_a_constant() {
        // The classic case and the reason DELTA exists. A million consecutive integers is a
        // difference of 1 a million times, which is a constant chunk under the delta.
        let values: Vec<i64> = (0..1_000_000).collect();
        let bytes = round_trip(&values);
        assert_eq!(kind_of(&bytes), Kind::Delta);
        assert_eq!(describe(&bytes).unwrap(), "DELTA(CONSTANT)");
        assert!(!is_strided(&bytes), "a delta is held flat");
        assert!(bytes.len() < 40, "{} bytes for a counter", bytes.len());
    }

    #[test]
    fn a_column_that_counts_down_is_as_cheap_as_one_that_counts_up() {
        // What zigzag is for. Without it every delta is -1, which is 64 bits of ones.
        let up: Vec<i64> = (0..100_000).collect();
        let down: Vec<i64> = (0..100_000).rev().collect();
        assert_eq!(round_trip(&up).len(), round_trip(&down).len());
    }

    #[test]
    fn long_runs_become_rle() {
        let mut values = Vec::new();
        for run in 0..1000 {
            values.extend(std::iter::repeat_n(run % 7, 200));
        }
        let bytes = round_trip(&values);
        assert_eq!(kind_of(&bytes), Kind::Rle);
        assert!(bytes.len() < 2000, "{} bytes for 1000 runs", bytes.len());
    }

    #[test]
    fn a_low_cardinality_column_becomes_a_dictionary() {
        // Values that are far apart so that packing them directly is 30 bits each, and only 40 of
        // them so that the codes are 6 bits each. The dictionary has to win by a factor of five.
        //
        // Drawn at random rather than laid out at a fixed interval, because a fixed interval is a
        // stride and STRIDE writes the same codes without a dictionary to point them at.
        let mut random = Random::new();
        let dictionary: Vec<i64> =
            (0..40).map(|_| 1_000_000_000 + (random.next() % (1 << 30)) as i64).collect();
        let values: Vec<i64> =
            (0..100_000).map(|_| dictionary[(random.next() % 40) as usize]).collect();
        let bytes = round_trip(&values);
        assert_eq!(kind_of(&bytes), Kind::Dict);
        assert!(bytes.len() < 100_000, "{} bytes", bytes.len());
    }

    #[test]
    fn a_nearly_constant_column_becomes_sparse() {
        let mut values = vec![0i64; 100_000];
        for index in 0..300 {
            values[index * 331] = 1 << 40;
        }
        let bytes = round_trip(&values);
        assert_eq!(kind_of(&bytes), Kind::Sparse);
        assert!(bytes.len() < 3000, "{} bytes for 300 exceptions", bytes.len());
    }

    #[test]
    fn encoded_counts_match_decoded_rows_across_integer_shapes() {
        let mut sparse = vec![0_i64; 4096];
        for (index, value) in [(7, -3), (91, 12), (1001, -3), (3000, 12)] {
            sparse[index] = value;
        }
        let mut runs = Vec::new();
        for value in [0, 7, 0, -5] {
            runs.extend(std::iter::repeat_n(value, 500));
        }
        let mut random = Random::new();
        let packed = (0..2000).map(|_| (random.next() % 251) as i64).collect::<Vec<_>>();
        for values in [vec![0_i64; 1024], sparse, runs, packed] {
            let bytes = encode(&values).unwrap();
            let (rows, counts) = tally(&bytes).unwrap();
            let mut expected = BTreeMap::<i64, u64>::new();
            for value in decode(&bytes).unwrap() {
                *expected.entry(value).or_default() += 1;
            }
            assert_eq!(rows, values.len());
            assert_eq!(counts, expected.into_iter().collect::<Vec<_>>());
        }
    }

    #[test]
    fn folded_sparse_exceptions_keep_the_last_value_at_a_repeated_position() {
        let mut bytes = vec![Kind::Sparse.tag()];
        put_u32(&mut bytes, 10);
        put_i64(&mut bytes, 0);
        put_u32(&mut bytes, 2);
        bytes.extend(encode(&[7, 7]).unwrap());
        bytes.extend(encode(&[3, 5]).unwrap());

        let mut counts = BTreeMap::<i64, u64>::new();
        assert_eq!(
            fold(&bytes, |value, count| {
                *counts.entry(value).or_default() += count;
                Ok(())
            })
            .unwrap(),
            10
        );
        assert_eq!(counts, BTreeMap::from([(0, 9), (5, 1)]));
        assert_eq!(decode(&bytes).unwrap()[7], 5);
    }

    #[test]
    fn the_cascade_goes_more_than_one_level_deep() {
        // The whole point of section 6.3. A dictionary over a clustered column produces codes that
        // run in long stretches, and the run lengths of those are themselves compressible.
        let mut values = Vec::new();
        for index in 0..2000i64 {
            values.extend(std::iter::repeat_n(1_000_000 + (index % 5) * 104_729, 100));
        }
        let bytes = round_trip(&values);
        let shape = describe(&bytes).unwrap();
        assert!(shape.contains('('), "{shape} is not a cascade");
        assert!(bytes.len() < 4000, "{} bytes: {shape}", bytes.len());
    }

    #[test]
    fn random_data_is_packed_at_full_width_and_costs_what_it_costs() {
        // The case where nothing works, which has to come out at eight bytes a value plus change
        // rather than at eight bytes a value plus a dictionary of every value in the column.
        let mut random = Random::new();
        let values: Vec<i64> = (0..10_000).map(|_| random.next() as i64).collect();
        let bytes = round_trip(&values);
        assert_eq!(kind_of(&bytes), Kind::Packed);
        assert!(bytes.len() < 10_000 * 8 + 1000, "{} bytes", bytes.len());
    }

    #[test]
    fn the_extremes_of_the_type_survive() {
        // Every offset and every delta in here overflows something if the arithmetic is done in 64
        // bits, which is why it is done in 128.
        let values = vec![i64::MIN, i64::MAX, 0, -1, i64::MIN, i64::MAX];
        round_trip(&values);
        round_trip(&[i64::MIN; 3]);
        round_trip(&[i64::MIN, i64::MIN + 1]);
    }

    #[test]
    fn a_chunk_that_is_not_a_multiple_of_the_unit_round_trips() {
        for len in [1, 2, 1023, 1024, 1025, 2047, 2049] {
            let values: Vec<i64> = (0..len).map(|index| (index * 31 % 97) as i64).collect();
            round_trip(&values);
        }
    }

    #[test]
    fn units_of_different_widths_in_one_chunk_do_not_read_each_others_leftovers() {
        // A decode reuses its buffers from one unit to the next instead of getting a zeroed one
        // each time, so a unit that wrote fewer bits than the unit before it would come back with
        // the older unit's values in the bits it did not write. Each run of 1024 here needs a
        // different width and the widths go up and down, and the last run repeats the first, which
        // is the pair that would agree by accident if the reuse were wrong in the obvious way.
        //
        // The values are random rather than written out because this has to stay one packed chunk
        // of six units to be testing anything, and the first version of it was arithmetic and got
        // cascaded into a delta of runs where every nested array was under a unit long. That was
        // caught by gating a panic on the second unit and rerunning, which this version reaches and
        // the old one did not, and the assertion on the shape below is there so it stays reached.
        let mut random = Random::new();
        let mut values = Vec::new();
        for width in [40u32, 3, 61, 1, 17, 40] {
            for _ in 0..1024 {
                values.push((random.next() & ((1u64 << width) - 1)) as i64);
            }
        }
        let bytes = encode(&values).unwrap();
        let described = describe(&bytes).unwrap();
        assert!(described.starts_with("FOR+BITPACK"), "expected one packed chunk, got {described}");
        assert_eq!(decode(&bytes).unwrap(), values, "{described}");
    }

    #[test]
    fn a_chunk_that_cascades_more_than_one_level_deep_decodes_whole() {
        // A dictionary of deltas is three nested decodes, and each level reads the packed bytes of
        // its own unit out of the chunk where they lie. A chunk long enough to cascade and wide
        // enough to bit pack at more than one level is what says the levels do not read each
        // other's bytes.
        let mut values = Vec::new();
        for index in 0..8192i64 {
            values.push(1_600_000_000 + index / 4 + (index % 7) * 1_000);
        }
        let bytes = encode(&values).unwrap();
        let described = describe(&bytes).unwrap();
        assert!(described.contains('('), "expected a cascade, got {described}");
        assert_eq!(decode(&bytes).unwrap(), values, "{described}");
    }

    #[test]
    fn selected_positions_agree_with_a_full_decode_for_every_kind_with_a_point_form() {
        let positions = [0, 1, 17, 1023, 1024, 4097, 8191];
        let packed: Vec<i64> = (0..8192).map(|index| index * 31 % 1_000_003).collect();
        let mut runs = Vec::new();
        for run in 0..160i64 {
            runs.extend(std::iter::repeat_n(run * 13, (run as usize % 71) + 2));
        }
        runs.resize(8192, -7);
        let strided: Vec<i64> = (0..8192).map(|index| 500 + index * 7 % 5003 * 100).collect();
        let coded: Vec<i64> =
            (0..8192).map(|index| [-9_000_000_000, 3, 77, 1 << 40][index % 4]).collect();

        for (kind, values) in [
            (Kind::Packed, packed),
            (Kind::Rle, runs),
            (Kind::Strided, strided),
            (Kind::Dict, coded),
        ] {
            let bytes = encode_only(kind, &values).unwrap().expect("encoding applies");
            let selected = decode_selected(&bytes, &positions).unwrap();
            let expected = positions.iter().map(|&position| values[position]).collect::<Vec<_>>();
            assert_eq!(selected, expected, "{}", kind.name());
            // Some units wanted densely enough to be unpacked and some a value at a time, and
            // several rows of the same run.
            let mixed: Vec<usize> = (0..8192)
                .filter(|at| if at < &3000 { at % 3 == 0 } else { at % 97 == 5 })
                .collect();
            let selected = decode_selected(&bytes, &mixed).unwrap();
            let expected = mixed.iter().map(|&position| values[position]).collect::<Vec<_>>();
            assert_eq!(selected, expected, "{} mixed", kind.name());
            let shape = describe(&bytes).unwrap();
            // A run length chunk on top whose runs average several rows walks its runs.
            let simple = kind == Kind::Rle || (!shape.contains("RLE") && !shape.contains("DELTA"));
            assert_eq!(pointed(&bytes), simple, "{shape}");
            assert_eq!(run_length(&bytes), kind == Kind::Rle, "{shape}");
        }
    }

    #[test]
    fn a_run_length_chunk_is_walked_only_when_its_runs_are_long() {
        let long: Vec<i64> = (0..8192).map(|index| index / 10).collect();
        let short: Vec<i64> = (0..8192).map(|index| index / 2 % 5).collect();
        let long = encode_only(Kind::Rle, &long).unwrap().expect("runs apply");
        let short = encode_only(Kind::Rle, &short).unwrap().expect("runs apply");
        assert!(pointed(&long));
        assert!(!pointed(&short));
    }

    #[test]
    fn selected_rows_of_a_run_length_chunk_match_a_full_decode_however_many_are_wanted() {
        let mut random = Random::new();
        let mut values = Vec::new();
        while values.len() < 8192 {
            let length = 1 + (random.next() % 40) as usize;
            let value = (random.next() % 1000) as i64 - 500;
            values.extend(std::iter::repeat_n(value, length));
        }
        values.truncate(8192);
        let bytes = encode_only(Kind::Rle, &values).unwrap().expect("runs apply");
        for one_in in [1, 2, 7, 60, 900, 5000] {
            let positions: Vec<usize> =
                (0..8192).filter(|_| random.next().is_multiple_of(one_in)).collect();
            let expected: Vec<i64> = positions.iter().map(|&position| values[position]).collect();
            assert_eq!(decode_selected(&bytes, &positions).unwrap(), expected, "one in {one_in}");
        }
        for position in [0, 1, 8190, 8191] {
            assert_eq!(decode_selected(&bytes, &[position]).unwrap(), [values[position]]);
        }
        assert!(decode_selected(&bytes, &[3000, 5]).is_err());
        assert!(decode_selected(&bytes, &[8192]).is_err());
    }

    #[test]
    fn selected_positions_must_be_ordered_and_inside_the_chunk() {
        let bytes = encode_only(Kind::Packed, &(0..2048).collect::<Vec<_>>())
            .unwrap()
            .expect("packed applies");
        assert!(decode_selected(&bytes, &[7, 7]).is_err());
        assert!(decode_selected(&bytes, &[8, 3]).is_err());
        assert!(decode_selected(&bytes, &[2048]).is_err());
    }

    #[test]
    fn a_partial_unit_costs_its_own_values_and_not_a_whole_unit() {
        // Three values that need 40 bits each. In the transposed layout a unit is 1024 values
        // whether it holds them or not, so this would be 5 KB, and every nested array in a cascade
        // is this short. It is 15 bytes of payload and 14 of header.
        let values = vec![1i64 << 39, (1 << 39) + 7, 1 << 38];
        let bytes = encode_only(Kind::Packed, &values).unwrap().unwrap();
        assert_eq!(bytes.len(), 5 + 9 + 15);
        assert_eq!(decode(&bytes).unwrap(), values);
    }

    #[test]
    fn the_frame_of_reference_is_per_unit_and_not_per_chunk() {
        // A column that drifts, which is what a timestamp column and a clustered key both do. Each
        // unit here spans 1023 and packs at 10 bits, and a base per chunk would pay the 22 bits the
        // whole chunk spans on every value in it.
        let values: Vec<i64> =
            (0..4096i64).map(|index| (index / 1024) * 1_000_000 + (index % 1024)).collect();
        let bytes = encode_only(Kind::Packed, &values).unwrap().unwrap();
        assert_eq!(describe(&bytes).unwrap(), "FOR+BITPACK[10]");
        assert_eq!(decode(&bytes).unwrap(), values);
    }

    #[test]
    fn every_candidate_that_applies_decodes_to_the_input() {
        // The chooser only ever hands back the smallest, so without this the other five are only
        // tested when they happen to win. Any of them being wrong is a wrong answer that appears
        // when a column's distribution shifts.
        let mut values = vec![5i64; 3000];
        for (index, value) in values.iter_mut().enumerate() {
            if index % 500 == 0 {
                *value = index as i64;
            }
        }
        let applicable = candidates(&values, 0, &EXHAUSTIVE);
        assert!(applicable.len() >= 4, "{applicable:?}");
        for kind in applicable {
            let bytes = encode_only(kind, &values).unwrap().unwrap();
            assert_eq!(decode(&bytes).unwrap(), values, "{}", kind.name());
        }
    }

    /// The test above only asks the kinds `candidates` offered, so between them the two cover the
    /// encoders on input the search would give them and nothing else. `encode_only` does not go
    /// through `candidates` at all, so every one of its callers can hand an encoder a shape the
    /// filter would have refused, and the empty chunk is the shape that used to panic.
    #[test]
    fn every_kind_that_applies_decodes_to_what_it_was_given() {
        let shapes: Vec<Vec<i64>> = vec![
            Vec::new(),
            vec![5; 1024],
            vec![i64::MIN, i64::MAX, 0, -1],
            (0..1024).map(|at| at * 7).collect(),
            (0..1024).map(|at| at % 17).collect(),
            (0..1024).map(|at| if at % 100 == 0 { at } else { 3 }).collect(),
            (0..1024).map(|at| -at * 1_000_003).collect(),
            (0..1024_i64)
                .map(|at| {
                    at.wrapping_mul(6_364_136_223_846_793_005)
                        .wrapping_add(1_442_695_040_888_963_407)
                })
                .collect(),
        ];
        let kinds =
            [Kind::Constant, Kind::Packed, Kind::Delta, Kind::Rle, Kind::Dict, Kind::Sparse];
        for values in &shapes {
            for kind in kinds {
                let Some(bytes) = encode_only(kind, values).unwrap() else {
                    continue;
                };
                assert_eq!(
                    &decode(&bytes).unwrap(),
                    values,
                    "{} over {} values",
                    kind.name(),
                    values.len()
                );
            }
        }
    }

    #[test]
    fn the_chooser_picks_the_smallest_candidate_rather_than_the_first_that_applies() {
        let mut values = vec![5i64; 3000];
        values[1500] = 9;
        let chosen = encode(&values).unwrap();
        for (_, size) in candidate_sizes(&values).unwrap() {
            assert!(chosen.len() <= size);
        }
    }

    #[test]
    fn a_truncated_chunk_is_an_error_and_not_a_panic() {
        let bytes = encode(&[1, 2, 3, 4, 5]).unwrap();
        for len in 0..bytes.len() {
            let error = decode(&bytes[..len]).unwrap_err();
            assert!(error.message().contains("chunk"), "{error}");
        }
    }

    #[test]
    fn trailing_bytes_are_an_error() {
        let mut bytes = encode(&[1, 2, 3]).unwrap();
        bytes.push(0);
        let error = decode(&bytes).unwrap_err();
        assert!(error.message().contains("left over"), "{error}");
    }

    #[test]
    fn an_unknown_tag_is_an_error() {
        let error = decode(&[99, 0, 0, 0, 0]).unwrap_err();
        assert!(error.message().contains("unknown encoding tag"), "{error}");
    }

    #[test]
    fn a_dictionary_code_outside_the_dictionary_is_an_error() {
        // A corrupted or malicious chunk must not index out of bounds, and this is the one place in
        // the decoder where a number that came off the disk is used as an index. Built by hand
        // rather than by corrupting a real chunk, because a byte offset into an encoding that the
        // chooser is free to change is a test that breaks for the wrong reason.
        let mut bytes = vec![Kind::Dict.tag()];
        put_u32(&mut bytes, 1);
        bytes.extend_from_slice(&encode(&[10]).unwrap());
        bytes.extend_from_slice(&encode(&[5]).unwrap());
        let error = decode(&bytes).unwrap_err();
        assert!(error.message().contains("not in the dictionary"), "{error}");
    }

    #[test]
    fn a_negative_run_length_is_an_error() {
        // The other number off the disk that the decoder would otherwise trust, and the one that
        // would turn into an allocation of nine quintillion values.
        let mut bytes = vec![Kind::Rle.tag()];
        put_u32(&mut bytes, 4);
        bytes.extend_from_slice(&encode(&[7]).unwrap());
        bytes.extend_from_slice(&encode(&[-4]).unwrap());
        let error = decode(&bytes).unwrap_err();
        assert!(error.message().contains("negative"), "{error}");
    }

    /// A run that ends past the chunk it is in is an error and not a write past the end.
    #[test]
    fn a_run_that_runs_past_its_chunk_is_an_error() {
        // The decode writes a fixed eight values per run and moves on by the run's own length, so
        // the buffer carries eight values of slack and a run that claims more rows than the chunk
        // holds would be the one way to reach past it. It is refused before the write rather than
        // caught by the count afterwards.
        let mut bytes = vec![Kind::Rle.tag()];
        put_u32(&mut bytes, 4);
        bytes.extend_from_slice(&encode(&[7]).unwrap());
        bytes.extend_from_slice(&encode(&[9]).unwrap());
        let error = decode(&bytes).unwrap_err();
        assert!(error.message().contains("past its chunk"), "{error}");
    }

    /// Runs of every length around the eight that a run is written in, in one chunk.
    #[test]
    fn runs_shorter_and_longer_than_the_width_they_are_written_in_all_come_back() {
        // A run of one, several shorter than eight, one of exactly eight and two longer, with the
        // shortest run last so that the surplus of the write before it has nothing after it to be
        // overwritten by. The values differ from each other, because a surplus that was left in
        // place would be invisible against a neighbour holding the same value.
        let lengths = [1, 3, 7, 8, 9, 40, 2, 1];
        let mut values = Vec::new();
        for (at, length) in lengths.iter().enumerate() {
            let value = i64::try_from(at).expect("eight runs") * 1000 - 3;
            values.extend(std::iter::repeat_n(value, *length));
        }
        let bytes = encode(&values).expect("encodes");
        assert_eq!(decode(&bytes).expect("decodes"), values, "runs around the write width");
        // And the same rows a run at a time, which is the run length encoder's worst case and the
        // shape a column with no runs in it decodes as.
        let singles: Vec<i64> = (0..300).map(|index| index * 7 % 11).collect();
        let bytes = encode(&singles).expect("encodes");
        assert_eq!(decode(&bytes).expect("decodes"), singles, "no run longer than one");
    }

    #[test]
    fn the_cascade_depth_is_bounded() {
        // Without the limit a chooser that finds a dictionary of a dictionary of a dictionary would
        // recurse until the values ran out, and the encode time of a wide column would be a
        // surprise rather than a number.
        let values: Vec<i64> = (0..50_000).map(|index| (index / 100) % 250).collect();
        let bytes = round_trip(&values);
        let shape = describe(&bytes).unwrap();
        let depth = shape.matches('(').count();
        assert!(depth <= MAX_DEPTH as usize, "{shape} is {depth} deep");
    }

    #[test]
    fn candidate_sizes_reports_what_the_chooser_looked_at() {
        let values: Vec<i64> = (0..5000).map(|index| index % 17 * 1000).collect();
        let sizes = candidate_sizes(&values).unwrap();
        assert!(sizes.iter().any(|(kind, _)| *kind == Kind::Dict));
        assert!(sizes.iter().any(|(kind, _)| *kind == Kind::Packed));
        assert!(sizes.iter().all(|(_, size)| *size > 0));
    }

    #[test]
    fn a_chunk_can_be_read_from_the_front_of_a_longer_buffer() {
        // What a string column does. It writes an integer chunk of lengths into the middle of its
        // own body and has to find the end of it again on the way back.
        let first = encode(&[1, 2, 3]).unwrap();
        let second: Vec<i64> = (0..3000).map(|index| index % 11).collect();
        let second_bytes = encode(&second).unwrap();
        let mut joined = first.clone();
        joined.extend_from_slice(&second_bytes);
        joined.extend_from_slice(b"and then something else");

        let (values, used) = decode_prefix(&joined).unwrap();
        assert_eq!(values, vec![1, 2, 3]);
        assert_eq!(used, first.len());
        let (more, used_again) = decode_prefix(&joined[used..]).unwrap();
        assert_eq!(more, second);
        assert_eq!(used_again, second_bytes.len());

        let (text, described) = describe_prefix(&joined).unwrap();
        assert_eq!(described, first.len());
        assert_eq!(text, describe(&first).unwrap());
    }

    #[test]
    fn a_truncated_chunk_is_still_an_error_when_read_as_a_prefix() {
        let bytes = encode(&(0..2000).collect::<Vec<i64>>()).unwrap();
        for len in 0..bytes.len() {
            assert!(decode_prefix(&bytes[..len]).is_err(), "{len} bytes decoded");
        }
    }

    /// Every kind decodes into a narrow type as the same values [`decode`] gives, whichever kind
    /// the chunk is at the top.
    #[test]
    fn decoding_into_a_narrow_type_agrees_with_the_wide_decoder() {
        let columns: Vec<Vec<i64>> = vec![
            vec![7; 3000],
            (0..3000).map(|i| (i * 37) % 200 - 100).collect(),
            (0..3000).map(|i| if i % 97 == 0 { i % 50 } else { 0 }).collect(),
            (0..3000).map(|i| i / 250).collect(),
            (0..3000).map(|i| i * 3 + 11).collect(),
            (0..3000).map(|i| [5, -9, 120][i as usize % 3]).collect(),
        ];
        let kinds = [
            Kind::Constant,
            Kind::Packed,
            Kind::Delta,
            Kind::Rle,
            Kind::Dict,
            Kind::Sparse,
            Kind::Strided,
        ];
        for values in &columns {
            for kind in kinds {
                let Some(bytes) = encode_only(kind, values).unwrap() else { continue };
                let wide = decode(&bytes).unwrap();
                let as_i16: Vec<i64> =
                    decode_as::<i16>(&bytes).unwrap().into_iter().map(i64::from).collect();
                let as_i32: Vec<i64> =
                    decode_as::<i32>(&bytes).unwrap().into_iter().map(i64::from).collect();
                assert_eq!(as_i16, wide, "{kind:?} as i16");
                assert_eq!(as_i32, wide, "{kind:?} as i32");
                assert_eq!(decode_as::<i64>(&bytes).unwrap(), wide, "{kind:?} as i64");
            }
            let chosen = encode(values).unwrap();
            let narrow: Vec<i64> =
                decode_as::<i16>(&chosen).unwrap().into_iter().map(i64::from).collect();
            assert_eq!(narrow, decode(&chosen).unwrap(), "the chosen cascade as i16");
        }
    }

    /// A value is refused exactly where `TryFrom` refuses it, at both edges of every type.
    #[test]
    fn decoding_into_a_narrow_type_takes_what_fits_and_refuses_what_does_not() {
        fn check<T: Lane + TryFrom<i64> + PartialEq + std::fmt::Debug>(edges: [i64; 2]) {
            for edge in edges {
                for value in [edge - 1, edge, edge + 1] {
                    for kind in
                        [Kind::Constant, Kind::Packed, Kind::Rle, Kind::Sparse, Kind::Strided]
                    {
                        let values = [value, value, edges[0].max(0).min(edges[1]), value];
                        let Some(bytes) = encode_only(kind, &values).unwrap() else { continue };
                        let fits = values.iter().all(|&value| T::try_from(value).is_ok());
                        assert_eq!(decode_as::<T>(&bytes).is_ok(), fits, "{value} {kind:?}");
                    }
                }
            }
        }
        check::<i8>([-128, 127]);
        check::<u8>([0, 255]);
        check::<i16>([-32_768, 32_767]);
        check::<u16>([0, 65_535]);
        check::<i32>([i64::from(i32::MIN), i64::from(i32::MAX)]);
        check::<u32>([0, i64::from(u32::MAX)]);
    }

    /// A packed block whose width reaches past the type while every value in it still fits, which
    /// is the block that has to be checked a value at a time rather than by its two ends.
    #[test]
    fn a_packed_block_wider_than_its_type_still_decodes_when_its_values_fit() {
        let values: Vec<i64> = (0..1500).map(|i| if i % 2 == 0 { -5 } else { 32_767 }).collect();
        let bytes = encode_only(Kind::Packed, &values).unwrap().expect("packing always applies");
        let narrow: Vec<i64> =
            decode_as::<i16>(&bytes).unwrap().into_iter().map(i64::from).collect();
        assert_eq!(narrow, values);
        let over: Vec<i64> = values.iter().map(|&value| value + 1).collect();
        let bytes = encode_only(Kind::Packed, &over).unwrap().expect("packing always applies");
        assert!(decode_as::<i16>(&bytes).is_err(), "32768 is not an i16");
    }
}
