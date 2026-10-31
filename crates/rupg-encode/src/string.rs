//! The string column, which is offsets, bytes, and the choice between compressing the bytes and
//! not storing most of them at all.
//!
//! ClickBench `hits` is a string dataset before it is anything else. `URL`, `Referer`, `Title` and
//! the referer derived columns are most of the 20.46 GB DuckDB writes for it, so most of what
//! `spec/02-the-goal.md` promises on the resource axis has to come out of this file.
//!
//! ## The five shapes
//!
//! `CONSTANT` when every value is the same. `PLAIN`, which is lengths and raw bytes and is the
//! baseline the others have to beat. `FSST`, which is a symbol table and the same lengths over
//! compressed bytes. `DICT`, which is the distinct values and an array of codes. `FRONT`, which is
//! the length of the prefix each value shares with the one before it and the rest of the value.
//!
//! `DICT_FSST` from the section 6.2 table is not a sixth shape. A dictionary's entries are a string
//! column, and encoding them goes back through the same chooser, so a dictionary whose entries are
//! FSST compressed is what the chooser produces on its own whenever that is smaller. The same
//! recursion gives run length encoding of strings for free, because the codes are an integer chunk
//! and `crate::integer` already knows what to do with a column of long runs.
//!
//! ## Why front coding is here
//!
//! The whole file measurement in M1 says the chooser produces 11.65 GB for `hits` against Parquet's
//! 13.76 GB, and that `URL`, `Referer` and `OriginalURL` are 6.11 GB of it, and that on those three
//! the chooser loses to Parquet's Snappy. The shape it picked on all three was `DICT(FSST[255])`,
//! so the cascade was working and FSST was still losing.
//!
//! The reason is structural. FSST compresses each value on its own against a 255 symbol table, and
//! a block compressor has the previous few kilobytes of the page to point back into. Two URLs that
//! share a host and half a path are most of a back reference to each other and are nothing at all
//! to a symbol table, which can only spend eight bytes of a symbol on the part they share and has
//! to spend it again on every value. On a sorted dictionary of URLs the value before is the closest
//! thing in the column to the value in hand, and the bytes they share are the redundancy Snappy was
//! finding. Front coding is what reaches those bytes, and it composes with everything else here:
//! the suffixes it leaves behind are a string column and go back through the chooser, so
//! `DICT(FRONT(FSST))` is a shape the chooser can arrive at without anyone naming it.
//!
//! The chain has no restarts, so reading entry `n` means walking from entry zero. That is the right
//! trade while a dictionary is decoded whole, which is what `decode` does. When something wants one
//! entry out of a dictionary without materialising the rest, the answer is a restart every so many
//! entries, and it costs one full value per block.
//!
//! ## Lengths, not offsets
//!
//! The usual layout is `n + 1` offsets and Arrow does it that way because a slice of an array has
//! to be free. On disk the offsets are a monotonically increasing sequence whose differences are
//! the lengths, and the differences are what compress: URL lengths in a real column are a few dozen
//! distinct values in a narrow band, which the integer cascade turns into a handful of bits each,
//! while the offsets themselves need enough bits to address the whole chunk. The integer cascade
//! would find that by choosing DELTA, and storing lengths directly gets to the same place without
//! spending a level of the cascade on it. Offsets are a prefix sum away and that is a decode time
//! cost of one add per value.
//!
//! ## What is not here
//!
//! Nulls. A chunk here is N byte strings and an empty string is a value like any other. Validity is
//! a bitmap that belongs to the column rather than to the encoding, per `spec/05-storage.md`, and
//! `ROARING` in the section 6.2 table is what encodes it.
//!
//! Shared symbol tables and shared dictionaries across columns, which are section 6.4 and are the
//! measurement this milestone exists for. Everything here is one column on its own, which is the
//! baseline they get compared against.

// Lifted from rudb crates/rudb-encoding/src/string.rs at cd9f9676 (2026-10-05).
// Changes: the error type is rupg_common::Error. front_decode is gone, as only multi.rs of rudb called it and rupg has no shared dictionaries (spec/09 section 9.5.6). The time of the tally comes from tally::now. as_chunks in place of chunks_exact, for clippy.

use rupg_common::{Error, Result};

use crate::chooser::{Chooser, EXHAUSTIVE, Settled};
use crate::fsst::{MAX_SYMBOL_LEN, SymbolTable};
use crate::integer;
use crate::lz;
use crate::reader::Reader;
use crate::sequence::Sequence;
use crate::tally::{self, Family};

/// How deep the recursion goes. A dictionary of a dictionary is not a thing, so this only has to
/// stop the dictionary's own entries from being dictionary encoded again.
const MAX_DEPTH: u8 = 2;

/// How little sharing between neighbours is still worth offering front coding for, as one over
/// this. A twentieth of the column is around where the prefix lengths start paying for themselves,
/// and below it the candidate is an encode of the whole column that loses.
const SHARE_DIVISOR: usize = 20;

/// How few bytes is too few to bother looking for repeats in.
///
/// The matcher costs a hash table and a pass over the bytes whether it wins or not, and the chooser
/// is exhaustive, so an ungated candidate is a tax on every string column in the database. Four
/// kilobytes is about where a 32 KiB window has enough behind it to find anything.
const LZ_FLOOR: usize = 4096;

/// How many bytes of a column the symbol table is trained on.
///
/// The paper trains on about 16 KB. This is four times that, because training happens once per
/// chunk here rather than once per block, and because the cost of a symbol that is only in the
/// sample by accident is paid on every value in the chunk.
pub(crate) const SAMPLE_BYTES: usize = 64 * 1024;

/// What a string chunk is encoded as. The discriminant is the tag byte and is part of the format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// One value repeated.
    Constant = 0,
    /// Lengths and raw bytes.
    Plain = 1,
    /// Lengths, a symbol table, and FSST compressed bytes.
    Fsst = 2,
    /// The distinct values as a string chunk of their own, and codes into it as an integer chunk.
    Dict = 3,
    /// Shared prefix lengths as an integer chunk, and what is left of each value as a string chunk.
    Front = 4,
    /// Value lengths, copy lengths and copy offsets as integer chunks, and the bytes no copy
    /// covered as a string chunk. See the `lz` module for what the matcher does and why it is here.
    Lz = 5,
}

impl Kind {
    /// Every kind, in tag order.
    pub const ALL: [Self; 6] =
        [Self::Constant, Self::Plain, Self::Fsst, Self::Dict, Self::Front, Self::Lz];

    fn tag(self) -> u8 {
        self as u8
    }

    fn from_tag(tag: u8) -> Result<Self> {
        match tag {
            0 => Ok(Self::Constant),
            1 => Ok(Self::Plain),
            2 => Ok(Self::Fsst),
            3 => Ok(Self::Dict),
            4 => Ok(Self::Front),
            5 => Ok(Self::Lz),
            other => Err(Error::internal(format!("unknown string encoding tag {other}"))),
        }
    }

    /// The name that goes in a report.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Constant => "CONSTANT",
            Self::Plain => "PLAIN",
            Self::Fsst => "FSST",
            Self::Dict => "DICT",
            Self::Front => "FRONT",
            Self::Lz => "LZ",
        }
    }
}

/// Encodes a chunk of strings, choosing whatever comes out smallest.
///
/// Every candidate that applies is encoded in full and the smallest is kept, which is what this has
/// always done and is what every size this crate has reported came out of. [`encode_with`] is the
/// same thing with the search made swappable.
///
/// # Errors
///
/// If the chunk is longer than `u32::MAX` values, or if an encoding produces something its own
/// decoder would not accept.
pub fn encode(values: &[&[u8]]) -> Result<Vec<u8>> {
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
pub fn encode_with(values: &[&[u8]], chooser: &dyn Chooser) -> Result<Vec<u8>> {
    encode_at(values, 0, chooser)
}

/// A decoded chunk as one buffer with the values laid end to end, and where each one ends in it.
///
/// This is what the decoder builds and [`decode`] is a copy out of it. The cascade is why: a nest
/// like `FRONT(LZ(FSST))` decodes three levels to produce one, and a level that hands its caller a
/// `Vec<Vec<u8>>` has allocated once per value and copied every byte it holds. Three levels of that
/// on a chunk of a thousand URLs is three thousand allocations to produce a thousand strings that
/// the caller almost always wants back to back anyway.
///
/// It also makes the levels cheaper on their own terms. `PLAIN` is one `memcpy` of the whole
/// payload because the values are already end to end in the file. `FRONT` copies a shared prefix
/// out of the buffer it is writing into, so the previous value never has to be somewhere else.
/// `LZ` replays straight into the buffer, which is what its copy offsets meant in the first place.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Flat {
    bytes: Vec<u8>,
    /// Where each value ends, so a value starts where the one before it ended and the last entry
    /// is the length of `bytes`. Ends rather than offsets because a value is appended and its end
    /// is what is known at that moment.
    ends: Vec<usize>,
}

impl Flat {
    fn with_capacity(count: usize, bytes: usize) -> Self {
        Self { bytes: Vec::with_capacity(bytes), ends: Vec::with_capacity(count) }
    }

    fn push(&mut self, value: &[u8]) {
        self.bytes.extend_from_slice(value);
        self.ends.push(self.bytes.len());
    }

    /// Where the value at `index` starts, which is where the one before it ended.
    fn start(&self, index: usize) -> usize {
        if index == 0 { 0 } else { self.ends[index - 1] }
    }

    /// How many values the chunk holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ends.len()
    }

    /// Whether the chunk holds no values at all, which is not the same as holding empty ones.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }

    /// The values laid end to end. A caller that already knows the boundaries, which is what a
    /// global dictionary's offsets are, needs nothing else.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The value at `index`, or `None` past the end.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<&[u8]> {
        let end = *self.ends.get(index)?;
        self.bytes.get(self.start(index)..end)
    }

    /// Every value in order.
    pub fn iter(&self) -> impl Iterator<Item = &[u8]> {
        let mut at = 0;
        self.ends.iter().map(move |end| {
            let value = self.bytes.get(at..*end).unwrap_or_default();
            at = *end;
            value
        })
    }

    /// The buffer on its own, for a caller that wanted the bytes rather than the values.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// The buffer and the ends that divide it, for a caller building its own layout over them.
    ///
    /// [`into_bytes`](Self::into_bytes) is enough for a caller that already knows where the values
    /// end, which is what a global dictionary's stored offsets are. A caller that does not know has
    /// only [`iter`](Self::iter), and walking that to build a run of boundaries copies out numbers
    /// the chunk already holds. This hands both halves over and keeps the one allocation each.
    #[must_use]
    pub fn into_parts(self) -> (Vec<u8>, Vec<usize>) {
        (self.bytes, self.ends)
    }

    fn into_values(self) -> Vec<Vec<u8>> {
        let mut values = Vec::with_capacity(self.len());
        let mut at = 0;
        for end in &self.ends {
            values.push(self.bytes[at..*end].to_vec());
            at = *end;
        }
        values
    }
}

/// Decodes a chunk written by [`encode`] without taking it apart into a value each.
///
/// # Errors
///
/// As [`decode`].
pub fn decode_flat(bytes: &[u8]) -> Result<Flat> {
    let mut reader = Reader::new(bytes);
    let flat = decode_chunk(&mut reader)?;
    if reader.remaining() != 0 {
        return Err(Error::internal(format!(
            "{} bytes left over after decoding a string chunk",
            reader.remaining()
        )));
    }
    Ok(flat)
}

/// Whether each value of a chunk written by [`encode`] holds `sequence`'s pieces in order, answered
/// without decompressing it, or `None` for a chunk that is not compressed.
///
/// A compressed chunk is walked a code at a time, see [`Sequence`], and nothing is written out. Any
/// other shape is cheap to decode already and the caller reads it the usual way. A null is stored as
/// an empty value here, so the caller still has to take the nulls out.
///
/// # Errors
///
/// As [`decode`].
pub fn holds_in(bytes: &[u8], sequence: &Sequence) -> Result<Option<Vec<bool>>> {
    holds_in_where(bytes, sequence, |_| true)
}

/// [`holds_in`], walking only the values `maybe` does not rule out and answering no for the rest.
///
/// For a caller that holds a sketch of each value, see [`crate::sequence::grams`]. A value ruled out
/// is stepped over by its length and never walked.
///
/// # Errors
///
/// As [`decode`].
pub fn holds_in_where(
    bytes: &[u8],
    sequence: &Sequence,
    mut maybe: impl FnMut(usize) -> bool,
) -> Result<Option<Vec<bool>>> {
    if bytes.first() != Some(&Kind::Fsst.tag()) {
        return Ok(None);
    }
    let mut reader = Reader::new(bytes);
    reader.u8()?;
    let count = reader.u32()? as usize;
    let runs = read_compressed(&mut reader, count)?;
    let mut coded = sequence.over(&runs.table);
    let mut held = Vec::with_capacity(count);
    let mut payload = runs.payload;
    for &run in &runs.lengths {
        let Some((codes, rest)) = payload.split_at_checked(run) else {
            return Err(Error::internal("a compressed run is past the end of its chunk"));
        };
        payload = rest;
        let row = held.len();
        held.push(maybe(row) && coded.holds(codes)?);
    }
    if reader.remaining() != 0 {
        return Err(Error::internal(format!(
            "{} bytes left over after decoding a string chunk",
            reader.remaining()
        )));
    }
    Ok(Some(held))
}

/// Which values of a compressed chunk are one of `literals`, compared on the codes, or `None` for
/// a chunk of another kind.
///
/// Compressing is a function of the table and the bytes, so a value is a literal exactly when its
/// run is the literal compressed against the chunk's table. Each literal is compressed once and
/// every run is a length check and, for the few of the right length, a compare of a few bytes.
/// Nothing is decompressed. On JOB this is `movie_info.info IN ('Sweden', 'Norway', ...)` over the
/// stripes written after the column's dictionary stopped taking values, where decompressing six
/// million values to compare them was most of the scan.
///
/// # Errors
///
/// As [`decode`].
pub fn equals_in(bytes: &[u8], literals: &[&[u8]]) -> Result<Option<Vec<bool>>> {
    if bytes.first() != Some(&Kind::Fsst.tag()) {
        return Ok(None);
    }
    let mut reader = Reader::new(bytes);
    reader.u8()?;
    let count = reader.u32()? as usize;
    let runs = read_compressed(&mut reader, count)?;
    let mut wanted: Vec<Vec<u8>> = literals
        .iter()
        .map(|literal| {
            let mut codes = Vec::with_capacity(literal.len() * 2);
            runs.table.compress(literal, &mut codes);
            codes
        })
        .collect();
    wanted.sort_unstable();
    wanted.dedup();
    let mut held = Vec::with_capacity(count);
    let mut payload = runs.payload;
    for &run in &runs.lengths {
        let Some((codes, rest)) = payload.split_at_checked(run) else {
            return Err(Error::internal("a compressed run is past the end of its chunk"));
        };
        payload = rest;
        held.push(wanted.iter().any(|one| one.as_slice() == codes));
    }
    if reader.remaining() != 0 {
        return Err(Error::internal(format!(
            "{} bytes left over after decoding a string chunk",
            reader.remaining()
        )));
    }
    Ok(Some(held))
}

/// Whether [`decode_flat_at`] reads a few values of this chunk for less than decoding all of it,
/// which only a compressed chunk does. Every other kind is decoded whole and picked from.
#[must_use]
pub fn pointed(bytes: &[u8]) -> bool {
    bytes.first() == Some(&Kind::Fsst.tag())
}

/// Decodes only the values at `positions` of a chunk written by [`encode`], in that order.
///
/// A compressed chunk keeps every run's length, so the runs that are not wanted are stepped over
/// by adding their lengths and never decompressed. That is what a scan wants when a join has
/// already said which rows it keeps: in TPC-H q10 the customer scan keeps a quarter of its rows,
/// and decompressing the other three quarters of four string columns was most of what it did. The
/// other shapes are decoded whole and picked from, which costs what reading them always did.
///
/// # Errors
///
/// As [`decode`], and if the positions do not rise or one is past the end of the chunk.
pub fn decode_flat_at(bytes: &[u8], positions: &[u32]) -> Result<Flat> {
    if positions.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(Error::internal("the positions to decode do not rise"));
    }
    let mut reader = Reader::new(bytes);
    let flat = if bytes.first() == Some(&Kind::Fsst.tag()) {
        reader.u8()?;
        let count = reader.u32()? as usize;
        let runs = read_compressed(&mut reader, count)?;
        let mut flat = Flat::with_capacity(positions.len(), runs.payload.len());
        let mut at = 0;
        let mut next = 0;
        for &position in positions {
            let position = position as usize;
            if position >= count {
                return Err(Error::internal(format!("value {position} is not in the chunk")));
            }
            at += runs.lengths[next..position].iter().sum::<usize>();
            runs.run_into(position, &mut at, &mut flat.bytes)?;
            flat.ends.push(flat.bytes.len());
            next = position + 1;
        }
        flat
    } else {
        let whole = decode_chunk(&mut reader)?;
        let mut flat = Flat::with_capacity(positions.len(), 0);
        for &position in positions {
            let value = whole
                .get(position as usize)
                .ok_or_else(|| Error::internal(format!("value {position} is not in the chunk")))?;
            flat.push(value);
        }
        flat
    };
    if reader.remaining() != 0 {
        return Err(Error::internal(format!(
            "{} bytes left over after decoding a string chunk",
            reader.remaining()
        )));
    }
    Ok(flat)
}

/// Decodes a chunk that sits at the front of a longer buffer, and says how many bytes it took.
///
/// A column group holds one of these per column, and the decoder on that side cannot know where
/// one ends until it has been read.
///
/// # Errors
///
/// As [`decode`], except that trailing bytes are what the caller asked about rather than an error.
pub fn decode_prefix(bytes: &[u8]) -> Result<(Vec<Vec<u8>>, usize)> {
    let mut reader = Reader::new(bytes);
    let values = decode_chunk(&mut reader)?;
    Ok((values.into_values(), reader.used()))
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

/// Decodes a chunk written by [`encode`].
///
/// # Errors
///
/// If the bytes are truncated, carry an unknown tag, or describe a chunk whose parts disagree.
pub fn decode(bytes: &[u8]) -> Result<Vec<Vec<u8>>> {
    Ok(decode_flat(bytes)?.into_values())
}

/// The size of every candidate that applies, for a report that wants to say what was chosen over
/// what.
///
/// # Errors
///
/// As [`encode`].
pub fn candidate_sizes(values: &[&[u8]]) -> Result<Vec<(Kind, usize)>> {
    let mut sizes = Vec::new();
    for kind in candidates(values, 0) {
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
pub fn offered(values: &[&[u8]]) -> Vec<Kind> {
    candidates(values, 0)
}

/// One candidate on its own, which is what the chooser calls once per entry in [`offered`].
///
/// `None` when the encoding does not apply, which is what the chooser treats as a candidate that
/// did not run rather than as a failure. This is here so that the time the chooser spends can be
/// attributed to the candidate that spent it, which is the measurement F2 wants before anybody
/// replaces the exhaustive search with a sampled one. It is not how a writer encodes a chunk:
/// [`encode`] is, and picking a kind by hand gives up the only thing the chooser is for.
///
/// # Errors
///
/// As [`encode`].
pub fn encode_only(kind: Kind, values: &[&[u8]]) -> Result<Option<Vec<u8>>> {
    encode_as(kind, values, 0, &EXHAUSTIVE)
}

/// A shape that is FSST alone, against one table trained on a sample of `values`.
///
/// For a writer that compresses page after page of one column with FSST and nothing else. Training
/// is most of what FSST costs on a page of a thousand short values, and a page of `l_comment` trained
/// a table of its own, which on a TPC-H `lineitem` load from CSV was six percent of every cycle. The
/// table a page trains is much the same as the one the page before it trained, so the writer trains
/// one here, hands it to [`encode_fsst`] for the pages after, and trains again when it stops paying.
///
/// The time is counted as choosing, the way the rest of the time spent deciding is.
#[must_use]
pub fn fsst_shape(values: &[&[u8]]) -> Settled {
    let started = tally::now();
    let table = SymbolTable::train(&sample_of(values));
    tally::chose(Family::String, started);
    Settled::new(vec![Kind::Fsst], Vec::new()).with_symbols(0, table)
}

/// `values` as one FSST chunk against the table in `shape`, which [`fsst_shape`] made.
///
/// `None` when the table is empty, which is what a sample with nothing worth a symbol trains. This
/// is counted as an offer of FSST and, when it comes out, as kept, so that the pages a writer
/// compresses this way show up in `rudb_codec_metrics()` with the rest.
///
/// # Errors
///
/// As [`encode`].
pub fn encode_fsst(values: &[&[u8]], shape: &Settled) -> Result<Option<Vec<u8>>> {
    let out =
        tally::offer(Family::String, Kind::Fsst.tag(), || encode_as(Kind::Fsst, values, 0, shape))?;
    if out.is_some() {
        tally::kept(Family::String, Kind::Fsst.tag());
    }
    Ok(out)
}

/// How big one candidate comes out, which is all a sampling chooser needs from it.
///
/// The bytes are thrown away, so this says nothing [`encode_only`] does not. It is `pub(crate)` and
/// separate so that the sampler in [`crate::chooser`] is not handing back buffers it will not read.
pub(crate) fn size_as(kind: Kind, values: &[&[u8]], depth: u8) -> Result<Option<usize>> {
    Ok(encode_as(kind, values, depth, &EXHAUSTIVE)?.map(|bytes| bytes.len()))
}

/// The shape a chunk was encoded as, as a line of text like `DICT(FSST, RLE(...))`.
///
/// # Errors
///
/// As [`decode`].
pub fn describe(bytes: &[u8]) -> Result<String> {
    let mut reader = Reader::new(bytes);
    describe_chunk(&mut reader)
}

/// `shape` with one symbol table for the whole column, trained on what reaches FSST in `blocks`.
///
/// A settled shape is used for thousands of blocks of one column, and every block that tries FSST
/// trains its own table. On ClickBench `hits` that was 35 seconds of a 150 second load, most of it
/// on the literals `FRONT` then `LZ` leaves behind in `URL` and `Referer`, where the table comes
/// out much the same block after block. So the blocks the shape was settled on are taken down the
/// shape's levels here, the values that arrive at the FSST level are sampled together, and the
/// table trained on them is handed to every block through [`Chooser::symbols`].
///
/// A shape that ends in `PLAIN` before any FSST level comes back as it was. So does one whose
/// table comes out empty, which leaves each block to train its own as before.
#[must_use]
pub fn with_symbols(shape: Settled, blocks: &[Vec<&[u8]>]) -> Settled {
    let kinds = shape.strings();
    let Some(depth) =
        (0..=kinds.len()).find(|&at| matches!(kinds.get(at), Some(Kind::Fsst) | None))
    else {
        return shape;
    };
    let leads =
        kinds[..depth].iter().all(|kind| matches!(kind, Kind::Front | Kind::Lz | Kind::Dict));
    if !leads || depth > usize::from(MAX_DEPTH) {
        return shape;
    }
    let mut reached: Vec<Vec<u8>> = Vec::new();
    for block in blocks {
        let mut values: Vec<Vec<u8>> = block.iter().map(|value| value.to_vec()).collect();
        for kind in &kinds[..depth] {
            let refs: Vec<&[u8]> = values.iter().map(Vec::as_slice).collect();
            values = match kind {
                Kind::Front => front_code(&refs).1.into_iter().map(<[u8]>::to_vec).collect(),
                Kind::Dict => dictionary_of(&refs).0.into_iter().map(<[u8]>::to_vec).collect(),
                _ => {
                    let joined = refs.concat();
                    lz::tokens_of(&joined).literals.into_iter().map(<[u8]>::to_vec).collect()
                }
            };
        }
        reached.extend(values);
    }
    let refs: Vec<&[u8]> = reached.iter().map(Vec::as_slice).collect();
    let table = SymbolTable::train(&sample_of(&refs));
    if table.is_empty() {
        return shape;
    }
    shape.with_symbols(depth as u8, table)
}

fn encode_at(values: &[&[u8]], depth: u8, chooser: &dyn Chooser) -> Result<Vec<u8>> {
    let started = tally::now();
    let offered = candidates(values, depth);
    let narrowed = chooser.narrow_strings(values, &offered, depth);
    // Only the top level is counted, so that a cascade's time is counted once. See `tally`.
    let counted = depth == 0;
    if counted {
        tally::chose(Family::String, started);
    }
    let mut best: Option<(Kind, Vec<u8>)> = None;
    for kind in narrowed {
        let encoded = if counted {
            tally::offer(Family::String, kind.tag(), || encode_as(kind, values, depth, chooser))?
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
    let (kind, bytes) =
        best.ok_or_else(|| Error::internal("no string encoding applied to the chunk"))?;
    if counted {
        tally::kept(Family::String, kind.tag());
    }
    Ok(bytes)
}

fn candidates(values: &[&[u8]], depth: u8) -> Vec<Kind> {
    let mut kinds = vec![Kind::Plain];
    if values.is_empty() {
        return kinds;
    }
    if values.iter().all(|value| *value == values[0]) {
        return vec![Kind::Constant];
    }
    kinds.push(Kind::Fsst);
    if depth < MAX_DEPTH && has_duplicates(values) {
        kinds.push(Kind::Dict);
    }
    if depth < MAX_DEPTH && sharing_of(values) >= total_len(values) / SHARE_DIVISOR {
        kinds.push(Kind::Front);
    }
    if depth < MAX_DEPTH && total_len(values) >= LZ_FLOOR {
        kinds.push(Kind::Lz);
    }
    kinds
}

/// How many bytes each value shares with the value before it, added up.
///
/// This is a full pass over the column, and it is here rather than on a sample because it is byte
/// comparisons that stop at the first difference, which on a column with nothing to share stops
/// immediately. Against training a symbol table and compressing the whole column, which is what
/// offering the candidate would cost, it is not worth sampling.
fn sharing_of(values: &[&[u8]]) -> usize {
    let mut shared = 0;
    for pair in values.windows(2) {
        shared += shared_prefix(pair[0], pair[1]);
    }
    shared
}

/// Every value split into the bytes it shares with the value before it and the bytes it does not.
///
/// The suffixes point into the values, so this costs the prefix lengths and nothing else. It is
/// shared with [`crate::multi`], which front codes a column before compressing it against a symbol
/// table that belongs to the whole group.
pub(crate) fn front_code<'a>(values: &[&'a [u8]]) -> (Vec<i64>, Vec<&'a [u8]>) {
    let mut prefixes = Vec::with_capacity(values.len());
    let mut suffixes: Vec<&'a [u8]> = Vec::with_capacity(values.len());
    let mut previous: &[u8] = b"";
    for value in values {
        let value: &'a [u8] = value;
        let shared = shared_prefix(previous, value);
        prefixes.push(shared as i64);
        suffixes.push(&value[shared..]);
        previous = value;
    }
    (prefixes, suffixes)
}

fn shared_prefix(previous: &[u8], value: &[u8]) -> usize {
    let limit = previous.len().min(value.len());
    let mut shared = 0;
    while shared < limit && previous[shared] == value[shared] {
        shared += 1;
    }
    shared
}

fn total_len(values: &[&[u8]]) -> usize {
    values.iter().map(|value| value.len()).sum()
}

fn encode_as(
    kind: Kind,
    values: &[&[u8]],
    depth: u8,
    chooser: &dyn Chooser,
) -> Result<Option<Vec<u8>>> {
    let mut out = vec![kind.tag()];
    put_u32(&mut out, u32::try_from(values.len()).map_err(|_| too_long(values.len()))?);
    match kind {
        Kind::Constant => {
            let Some(first) = values.first() else {
                return Ok(None);
            };
            if values.iter().any(|value| value != first) {
                return Ok(None);
            }
            put_u32(&mut out, u32::try_from(first.len()).map_err(|_| too_long(first.len()))?);
            out.extend_from_slice(first);
        }
        Kind::Plain => {
            out.extend_from_slice(&encode_lengths(values, chooser)?);
            for value in values {
                out.extend_from_slice(value);
            }
        }
        Kind::Fsst => {
            let trained;
            let table = match chooser.symbols(depth) {
                Some(table) => table,
                None => {
                    trained = SymbolTable::train(&sample_of(values));
                    &trained
                }
            };
            if table.is_empty() {
                return Ok(None);
            }
            let mut compressed = Vec::new();
            let mut lengths = Vec::with_capacity(values.len());
            for value in values {
                let before = compressed.len();
                table.compress(value, &mut compressed);
                lengths.push((compressed.len() - before) as i64);
            }
            table.serialize(&mut out);
            out.extend_from_slice(&integer::encode_with(&lengths, chooser)?);
            out.extend_from_slice(&compressed);
        }
        Kind::Dict => {
            let (entries, codes) = dictionary_of(values);
            if entries.is_empty() {
                return Ok(None);
            }
            out.extend_from_slice(&encode_at(&entries, depth + 1, chooser)?);
            out.extend_from_slice(&integer::encode_with(&codes, chooser)?);
        }
        Kind::Front => {
            let (prefixes, suffixes) = front_code(values);
            out.extend_from_slice(&integer::encode_with(&prefixes, chooser)?);
            out.extend_from_slice(&encode_at(&suffixes, depth + 1, chooser)?);
        }
        Kind::Lz => {
            let mut joined = Vec::with_capacity(total_len(values));
            let mut sizes = Vec::with_capacity(values.len());
            for value in values {
                joined.extend_from_slice(value);
                sizes.push(value.len() as i64);
            }
            let tokens = lz::tokens_of(&joined);
            out.extend_from_slice(&integer::encode_with(&sizes, chooser)?);
            out.extend_from_slice(&integer::encode_with(&tokens.lengths, chooser)?);
            out.extend_from_slice(&integer::encode_with(&tokens.offsets, chooser)?);
            out.extend_from_slice(&encode_at(&tokens.literals, depth + 1, chooser)?);
        }
    }
    Ok(Some(out))
}

fn decode_chunk(reader: &mut Reader<'_>) -> Result<Flat> {
    let kind = Kind::from_tag(reader.u8()?)?;
    let count = reader.u32()? as usize;
    match kind {
        Kind::Constant => {
            let len = reader.u32()? as usize;
            let value = reader.bytes(len)?;
            let mut flat = Flat::with_capacity(count, len.saturating_mul(count));
            for _ in 0..count {
                flat.push(value);
            }
            Ok(flat)
        }
        Kind::Plain => {
            let lengths = decode_lengths(reader, count)?;
            // One copy of the whole payload rather than one a value, which the file already laid
            // out end to end and which is the layout wanted back.
            let total = sum_of(&lengths)?;
            let payload = reader.bytes(total)?;
            let mut flat = Flat::with_capacity(count, total);
            flat.bytes.extend_from_slice(payload);
            let mut at = 0;
            for length in lengths {
                at += length;
                flat.ends.push(at);
            }
            Ok(flat)
        }
        Kind::Fsst => {
            let runs = read_compressed(reader, count)?;
            let mut flat = Flat::with_capacity(count, 0);
            runs.all_into(&mut flat)?;
            Ok(flat)
        }
        Kind::Dict => {
            let dictionary = decode_chunk(reader)?;
            let codes = decode_integers(reader)?;
            if codes.len() != count {
                return Err(Error::internal(format!(
                    "a dictionary chunk says it holds {count} values and has {} codes",
                    codes.len()
                )));
            }
            let mut flat = Flat::with_capacity(count, dictionary.bytes.len());
            for code in codes {
                let entry =
                    usize::try_from(code).ok().and_then(|index| dictionary.get(index)).ok_or_else(
                        || Error::internal(format!("code {code} is not in the dictionary")),
                    )?;
                flat.push(entry);
            }
            Ok(flat)
        }
        Kind::Front => {
            let prefixes = decode_integers(reader)?;
            let suffixes = decode_chunk(reader)?;
            if prefixes.len() != count || suffixes.len() != count {
                return Err(Error::internal(format!(
                    "a front coded chunk says it holds {count} values and has {} prefixes and {} suffixes",
                    prefixes.len(),
                    suffixes.len()
                )));
            }
            // The shared prefix is copied out of the buffer being written into, so a value never
            // has to exist anywhere but where it belongs. The buffer is sized for the prefixes as
            // well as the suffixes, since sized for the suffixes alone a sorted block of URLs,
            // whose values share most of their bytes, doubled its way up and copied itself each
            // time. Walking the lengths first also checks every prefix against the value before
            // it, so a corrupt one is refused before anything is allocated for it.
            let mut room = 0usize;
            let mut previous = 0usize;
            for (index, prefix) in prefixes.iter().enumerate() {
                let shared = usize::try_from(*prefix)
                    .map_err(|_| Error::internal("a negative shared prefix length"))?;
                if shared > previous {
                    return Err(Error::internal(format!(
                        "a value shares {shared} bytes with a value {previous} bytes long"
                    )));
                }
                previous = shared + suffixes.get(index).map_or(0, <[u8]>::len);
                room = room
                    .checked_add(previous)
                    .ok_or_else(|| Error::internal("a string chunk longer than memory"))?;
            }
            let mut flat = Flat::with_capacity(count, room);
            for (index, &prefix) in prefixes.iter().enumerate() {
                let shared = prefix as usize;
                let from = if index == 0 { 0 } else { flat.start(index - 1) };
                flat.bytes.extend_from_within(from..from + shared);
                flat.bytes.extend_from_slice(suffixes.get(index).expect("in range"));
                flat.ends.push(flat.bytes.len());
            }
            Ok(flat)
        }
        Kind::Lz => {
            let sizes = decode_integers(reader)?;
            let lengths = decode_integers(reader)?;
            let offsets = decode_integers(reader)?;
            if sizes.len() != count {
                return Err(Error::internal(format!(
                    "a matched chunk says it holds {count} values and has {} lengths",
                    sizes.len()
                )));
            }
            let mut total = 0usize;
            let mut widths = Vec::with_capacity(count);
            for size in sizes {
                let width = usize::try_from(size)
                    .map_err(|_| Error::internal("a negative string length"))?;
                total = total
                    .checked_add(width)
                    .ok_or_else(|| Error::internal("a string chunk longer than memory"))?;
                widths.push(width);
            }
            // The copies point back into the bytes already replayed, which is the buffer the values
            // are going into, so the replay is the decode and there is nothing to cut up after it.
            // The room past the end is what the replay's wide stores want, and leaving it out had
            // the replay grow the buffer, which copied every block once more into fresh pages.
            let mut flat = Flat::with_capacity(count, total.saturating_add(REPLAY_SLACK));
            replay_literals(reader, &lengths, &offsets, total, &mut flat.bytes)?;
            if flat.bytes.len() != total {
                return Err(Error::internal(format!(
                    "a matched chunk rebuilt {} bytes where its lengths add up to {total}",
                    flat.bytes.len()
                )));
            }
            let mut at = 0;
            for width in widths {
                at += width;
                flat.ends.push(at);
            }
            Ok(flat)
        }
    }
}

/// A compressed chunk's symbol table and its runs, left where the file put them.
///
/// Reading a compressed chunk into this rather than straight into a buffer is what lets a run be
/// decompressed where the run belongs. The payload is one slice, the run boundaries come from the
/// length array, and so asking for a run is a decompress of a subslice and nothing else.
struct Compressed<'a> {
    /// The table the runs were compressed against.
    table: SymbolTable,
    /// How many compressed bytes each run holds, in order.
    lengths: Vec<usize>,
    /// Every run's compressed bytes, end to end.
    payload: &'a [u8],
}

impl Compressed<'_> {
    /// Decompresses run `index` onto the end of `out`, with `at` saying where the run starts.
    ///
    /// The caller carries the offset because the runs are asked for in order, and adding a length
    /// per run is cheaper than the prefix sum the alternative wants.
    ///
    /// # Errors
    ///
    /// If there is no such run, if it runs off the end of the payload, or if it does not decompress.
    fn run_into(&self, index: usize, at: &mut usize, out: &mut Vec<u8>) -> Result<()> {
        self.table.decompress(self.run(index, at)?, out)
    }

    /// Decompresses every run in order onto the end of `flat`.
    ///
    /// Each symbol is one eight byte store into room made ahead of it, which is what
    /// [`SymbolTable::decompress_at`] is for. A run of `n` codes writes at most `n` symbols of at
    /// most [`MAX_SYMBOL_LEN`] bytes, the last store included, so that much room past where the run
    /// starts is all it needs. The room is made by doubling, so the zeroes written to make it add up
    /// to at most twice what the chunk decompresses to. Growing a vector a symbol at a time and
    /// cutting it back was a fifth of TPC-H q13, all of it the order comment.
    ///
    /// # Errors
    ///
    /// If a run is past the end of the chunk or does not decompress.
    fn all_into(&self, flat: &mut Flat) -> Result<()> {
        let out = &mut flat.bytes;
        let mut at = out.len();
        let mut payload = self.payload;
        for &run in &self.lengths {
            let Some((codes, rest)) = payload.split_at_checked(run) else {
                return Err(Error::internal("a compressed run is past the end of its chunk"));
            };
            payload = rest;
            let need = run
                .checked_mul(MAX_SYMBOL_LEN)
                .and_then(|room| room.checked_add(at))
                .ok_or_else(|| Error::internal("a compressed chunk longer than memory"))?;
            if out.len() < need {
                out.resize(need.max(out.len() * 2), 0);
            }
            at = self.table.decompress_at(codes, out, at)?;
            flat.ends.push(at);
        }
        out.truncate(at);
        Ok(())
    }

    /// The compressed bytes of run `index`, with `at` saying where the run starts and left where
    /// the next one does.
    fn run(&self, index: usize, at: &mut usize) -> Result<&[u8]> {
        let length = *self
            .lengths
            .get(index)
            .ok_or_else(|| Error::internal(format!("run {index} is not in the chunk")))?;
        let end = at
            .checked_add(length)
            .ok_or_else(|| Error::internal("a compressed chunk longer than memory"))?;
        let run = self
            .payload
            .get(*at..end)
            .ok_or_else(|| Error::internal("a compressed run is past the end of its chunk"))?;
        *at = end;
        Ok(run)
    }
}

/// Reads a compressed chunk's table, run lengths and payload without decompressing any of it.
///
/// The tag and the count have already been read.
///
/// # Errors
///
/// If the table does not deserialize, if the length array is not `count` long, or if the lengths
/// add up to more than the chunk has left.
fn read_compressed<'a>(reader: &mut Reader<'a>, count: usize) -> Result<Compressed<'a>> {
    let (table, used) = SymbolTable::deserialize(reader.rest())?;
    reader.skip(used)?;
    let lengths = decode_lengths(reader, count)?;
    // The compressed total is what the payload holds and it is also the only sane guess at the
    // decompressed one, so it is checked before it is believed.
    let compressed_len = sum_of(&lengths)?;
    if compressed_len > reader.remaining() {
        return Err(Error::internal(format!(
            "a compressed chunk says it holds {compressed_len} bytes and has {}",
            reader.remaining()
        )));
    }
    let payload = reader.bytes(compressed_len)?;
    Ok(Compressed { table, lengths, payload })
}

/// Replays a matched chunk's tokens, reading the literal runs out of the nested chunk holding them.
///
/// The nested chunk is decoded into a buffer and copied out of, the way anything nested is, unless
/// it is compressed. On the ClickBench `URL` column it always is, and there a block of a thousand
/// values holds about eight thousand seven hundred literal runs, so that buffer is the whole
/// block's bytes and copying the runs out of it writes every one of them a second time.
/// Decompressing a run straight to where it belongs skips the buffer, the length array that would
/// cut it up, and that second pass over the bytes.
///
/// # Errors
///
/// Whatever reading the literals or replaying the tokens reports.
fn replay_literals(
    reader: &mut Reader<'_>,
    lengths: &[i64],
    offsets: &[i64],
    total: usize,
    out: &mut Vec<u8>,
) -> Result<()> {
    if reader.rest().first() == Some(&Kind::Fsst.tag()) {
        reader.u8()?;
        let runs = reader.u32()? as usize;
        let compressed = read_compressed(reader, runs)?;
        return replay_in_place(&compressed, lengths, offsets, total, out);
    }
    let literals = decode_chunk(reader)?;
    lz::rebuild_into(&literals, lengths, offsets, out)
}

/// Room past the end of a replay, for the stores that write whole words past where a value ends.
///
/// A symbol is stored as eight bytes and a copy as sixteen at a time, and each is followed by a
/// step of the cursor to where the bytes it meant end. What lands past that is written over by
/// whatever comes next, or cut off at the end.
const REPLAY_SLACK: usize = 16;

/// [`lz::replay`] over compressed literal runs, into a buffer made the length of the output first.
///
/// The output length is known before a byte is decoded, because the chunk stores the length of
/// every value. So the buffer is sized once and written through a cursor, and a symbol or a copy is
/// a fixed width store rather than a push that checks capacity and moves a length. The copies were
/// the reason: on ClickBench `URL` a block of a thousand values replays about eight thousand seven
/// hundred of them, most of them a few tens of bytes, and each one was a call into `memmove`.
///
/// # Errors
///
/// As [`lz::replay`], and if the tokens build more than `total` bytes.
fn replay_in_place(
    compressed: &Compressed<'_>,
    lengths: &[i64],
    offsets: &[i64],
    total: usize,
    out: &mut Vec<u8>,
) -> Result<()> {
    let runs = compressed.lengths.len();
    if runs != lengths.len() || lengths.len() != offsets.len() {
        return Err(Error::internal(format!(
            "a matched chunk has {runs} literal runs, {} lengths and {} offsets",
            lengths.len(),
            offsets.len()
        )));
    }
    let base = out.len();
    let room = total
        .checked_add(REPLAY_SLACK)
        .ok_or_else(|| Error::internal("a string chunk longer than memory"))?;
    out.resize(base + room, 0);
    let mut payload = compressed.payload;
    let mut at = base;
    for ((&run, &length), &offset) in compressed.lengths.iter().zip(lengths).zip(offsets) {
        let Some((codes, rest)) = payload.split_at_checked(run) else {
            return Err(Error::internal("a compressed run is past the end of its chunk"));
        };
        payload = rest;
        at = compressed.table.decompress_at(codes, out, at)?;
        let length =
            usize::try_from(length).map_err(|_| Error::internal("a negative copy length"))?;
        if length == 0 {
            continue;
        }
        let offset =
            usize::try_from(offset).map_err(|_| Error::internal("a negative copy offset"))?;
        at = copy_back(out, base, at, offset, length)?;
    }
    if at > base + total {
        return Err(Error::internal(format!(
            "a matched chunk rebuilt {} bytes where its lengths add up to {total}",
            at - base
        )));
    }
    out.truncate(at);
    Ok(())
}

/// Copies `length` bytes from `offset` back to `at`, handing back where the copy ends.
///
/// Sixteen bytes at a time where the copy starts at least sixteen bytes back, since then no store
/// reads a byte it has not been given yet, and eight at a time where it starts eight back. Nearer
/// than that the copy is repeating a short run and goes a byte at a time, the way it always did.
/// The whole width stores need room past the end of the copy, and a copy near the end of the buffer
/// that does not have it goes a byte at a time too.
fn copy_back(
    out: &mut [u8],
    base: usize,
    at: usize,
    offset: usize,
    length: usize,
) -> Result<usize> {
    if offset == 0 || offset > at - base {
        return Err(Error::internal(format!(
            "a copy reaches {offset} bytes back into {} bytes of output",
            at - base
        )));
    }
    let end = at
        .checked_add(length)
        .filter(|&end| end <= out.len())
        .ok_or_else(|| Error::internal("a matched chunk rebuilds more than its lengths say"))?;
    let from = at - offset;
    let wide = end + REPLAY_SLACK <= out.len();
    if wide && offset >= 16 {
        let mut step = 0;
        while step < length {
            out.copy_within(from + step..from + step + 16, at + step);
            step += 16;
        }
    } else if wide && offset >= 8 {
        let mut step = 0;
        while step < length {
            out.copy_within(from + step..from + step + 8, at + step);
            step += 8;
        }
    } else {
        for step in 0..length {
            out[at + step] = out[from + step];
        }
    }
    Ok(end)
}

fn describe_chunk(reader: &mut Reader<'_>) -> Result<String> {
    let kind = Kind::from_tag(reader.u8()?)?;
    let count = reader.u32()? as usize;
    Ok(match kind {
        Kind::Constant => {
            let len = reader.u32()? as usize;
            reader.bytes(len)?;
            "CONSTANT".to_string()
        }
        Kind::Plain => {
            let (shape, lengths) = describe_lengths(reader, count)?;
            reader.skip(lengths.iter().sum())?;
            format!("PLAIN({shape})")
        }
        Kind::Fsst => {
            let (table, used) = SymbolTable::deserialize(reader.rest())?;
            reader.skip(used)?;
            let (shape, lengths) = describe_lengths(reader, count)?;
            reader.skip(lengths.iter().sum())?;
            format!("FSST[{}]({shape})", table.len())
        }
        Kind::Dict => {
            let entries = describe_chunk(reader)?;
            let codes = describe_integers(reader)?;
            format!("DICT({entries}, {codes})")
        }
        Kind::Front => {
            let prefixes = describe_integers(reader)?;
            let suffixes = describe_chunk(reader)?;
            format!("FRONT({prefixes}, {suffixes})")
        }
        Kind::Lz => {
            let sizes = describe_integers(reader)?;
            let lengths = describe_integers(reader)?;
            let offsets = describe_integers(reader)?;
            let literals = describe_chunk(reader)?;
            format!("LZ({sizes}, {lengths}, {offsets}, {literals})")
        }
    })
}

/// The shape of the length array and the lengths themselves, because a describe has to walk past
/// the payload to leave the reader where the next chunk starts and the payload size is the sum of
/// the lengths.
fn describe_lengths(reader: &mut Reader<'_>, count: usize) -> Result<(String, Vec<usize>)> {
    let (shape, _) = integer::describe_prefix(reader.rest())?;
    let lengths = decode_lengths(reader, count)?;
    Ok((shape, lengths))
}

fn encode_lengths(values: &[&[u8]], chooser: &dyn Chooser) -> Result<Vec<u8>> {
    let lengths: Vec<i64> = values.iter().map(|value| value.len() as i64).collect();
    integer::encode_with(&lengths, chooser)
}

fn decode_lengths(reader: &mut Reader<'_>, count: usize) -> Result<Vec<usize>> {
    let lengths = decode_integers(reader)?;
    if lengths.len() != count {
        return Err(Error::internal(format!(
            "a string chunk says it holds {count} values and has {} lengths",
            lengths.len()
        )));
    }
    lengths
        .into_iter()
        .map(|length| {
            usize::try_from(length).map_err(|_| Error::internal("a negative string length"))
        })
        .collect()
}

/// How long the values add up to, refusing a length array that adds up to more than memory.
///
/// A truncated chunk used to be caught by the read of the value that ran off the end. Reading the
/// payload in one go means the total has to be trusted before the read rather than after it, and a
/// corrupt length array is the only thing that could overflow it.
fn sum_of(lengths: &[usize]) -> Result<usize> {
    lengths
        .iter()
        .try_fold(0usize, |total, length| total.checked_add(*length))
        .ok_or_else(|| Error::internal("a string chunk longer than memory"))
}

/// Reads one nested integer chunk. The integer decoder wants a slice of exactly its own chunk and
/// the reader does not know how long that is, so it decodes from the rest of the buffer and is told
/// afterwards how much it used.
fn decode_integers(reader: &mut Reader<'_>) -> Result<Vec<i64>> {
    let (values, used) = integer::decode_prefix(reader.rest())?;
    reader.skip(used)?;
    Ok(values)
}

fn describe_integers(reader: &mut Reader<'_>) -> Result<String> {
    let (text, used) = integer::describe_prefix(reader.rest())?;
    reader.skip(used)?;
    Ok(text)
}

/// A sample of the column spread across the whole of it, taken at random skips rather than at a
/// fixed stride.
///
/// Section 6.3 makes the point about choosing an encoding from a sample and it applies at least as
/// much to training a symbol table. Column data is frequently sorted or clustered, so the first
/// 64 KB of a URL column is the hosts that sort first and a table trained on it escapes most of the
/// rest of the column.
///
/// The skips are random rather than fixed because a fixed stride aliases. Column data is also
/// frequently periodic, and a stride that shares a factor with the period samples one phase of it
/// and never sees the others. That is not a hypothetical: the first version of this took every
/// `n`th value, and on a test column whose values cycle with a period that the stride happened to
/// divide, the table it trained was 3.4 times worse than one trained on the whole column, because
/// it learned eight byte symbols that only line up with the phase it saw and had no shorter symbols
/// left to fall back on.
///
/// The generator is a fixed seed xorshift, so the sample is a function of the column and encoding
/// the same values twice produces the same bytes.
pub(crate) fn sample_of<'a>(values: &[&'a [u8]]) -> Vec<&'a [u8]> {
    sample_bytes_of(values, SAMPLE_BYTES)
}

/// [`sample_of`] with the byte budget spelled out, for a caller training one table over several
/// columns that has to split the budget between them.
pub(crate) fn sample_bytes_of<'a>(values: &[&'a [u8]], budget: usize) -> Vec<&'a [u8]> {
    let budget = budget.max(1);
    let total: usize = values.iter().map(|value| value.len()).sum();
    if total <= budget {
        return values.to_vec();
    }
    let stride = total.div_ceil(budget).max(1);
    let span = (stride * 2 - 1).max(1) as u64;
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut sample = Vec::with_capacity(values.len() / stride + 1);
    let mut at = 0usize;
    while at < values.len() {
        sample.push(values[at]);
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        at += 1 + (state % span) as usize;
    }
    sample
}

/// The distinct values in sorted order and the code of every value, in one pass over one sort.
///
/// The dictionary is sorted for the same reason the integer one is: an ordered dictionary turns a
/// range predicate into a code range rather than a code set, and front coding over the entries needs
/// them sorted anyway.
///
/// It sorts a permutation of indices rather than the values, which is the whole point. Sorting the
/// values means copying every one of them onto the heap first, and the codes then have to be found
/// by searching the dictionary back for each value, which is a binary search of string comparisons
/// per row. Walking the permutation gives the codes away for free, because the position a value
/// sorted to is the position its code was assigned at.
fn dictionary_of<'a>(values: &[&'a [u8]]) -> (Vec<&'a [u8]>, Vec<i64>) {
    let mut order: Vec<u32> = (0..values.len() as u32).collect();
    order.sort_unstable_by(|left, right| values[*left as usize].cmp(values[*right as usize]));
    let mut entries: Vec<&'a [u8]> = Vec::new();
    let mut codes = vec![0i64; values.len()];
    for &index in &order {
        let value = values[index as usize];
        if entries.last() != Some(&value) {
            entries.push(value);
        }
        codes[index as usize] = (entries.len() - 1) as i64;
    }
    (entries, codes)
}

/// Whether any value appears twice, which is the only thing the candidate list wants to know.
///
/// This used to build the whole sorted dictionary and compare its length against the input, which
/// is a copy of the chunk and a sort of it paid on every chunk at every level whether the dictionary
/// was ever encoded or not. It is a linear probe over hashes instead: expected O(n), no allocation
/// per value, and it stops at the first duplicate it finds, which on a column with any repetition at
/// all is immediately.
///
/// A hash collision is resolved by comparing the bytes, so the answer is exact rather than probable.
fn has_duplicates(values: &[&[u8]]) -> bool {
    let Some(slots) = values.len().checked_mul(2).map(usize::next_power_of_two) else {
        return false;
    };
    let mask = slots - 1;
    let mut table = vec![u32::MAX; slots];
    for (index, value) in values.iter().enumerate() {
        let mut at = hash_of(value) as usize & mask;
        loop {
            let held = table[at];
            if held == u32::MAX {
                table[at] = index as u32;
                break;
            }
            if values[held as usize] == *value {
                return true;
            }
            at = (at + 1) & mask;
        }
    }
    false
}

/// FNV-1a over the bytes, eight at a time.
///
/// Good enough for a table that verifies every hit, and it is not part of the format, so nothing
/// depends on which hash this is. Eight bytes at a time because a URL column is long values and a
/// byte at a time over a hundred bytes of every one of 122,880 rows is the loop this is here to
/// avoid.
fn hash_of(value: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    let (chunks, rest) = value.as_chunks::<8>();
    for chunk in chunks {
        hash = (hash ^ u64::from_le_bytes(*chunk)).wrapping_mul(0x1_0000_01b3);
    }
    for byte in rest {
        hash = (hash ^ u64::from(*byte)).wrapping_mul(0x1_0000_01b3);
    }
    (hash ^ (value.len() as u64)).wrapping_mul(0x1_0000_01b3)
}

fn too_long(len: usize) -> Error {
    Error::internal(format!("a string chunk of {len} is longer than the format allows"))
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn urls(count: usize) -> Vec<Vec<u8>> {
        let hosts = ["www.example.com", "shop.example.com", "news.other.example.org"];
        let paths = ["/index.html", "/catalog/item", "/search", "/user/profile/settings"];
        (0..count)
            .map(|index| {
                let host = hosts[index % hosts.len()];
                let path = paths[(index / 3) % paths.len()];
                format!("http://{host}{path}?session={}&ref=google", index * 7).into_bytes()
            })
            .collect()
    }

    /// Only the values asked for come back, in order, from a compressed chunk that steps over the
    /// rest and from every other shape, which is decoded whole and picked from.
    #[test]
    fn the_values_at_some_positions_are_the_ones_a_whole_decode_has_there() {
        let values = urls(1000);
        let refs: Vec<&[u8]> = values.iter().map(Vec::as_slice).collect();
        let positions = [0_u32, 3, 4, 500, 998, 999];
        let wanted: Vec<Vec<u8>> =
            positions.iter().map(|&at| values[at as usize].clone()).collect();
        for kind in offered(&refs) {
            let Some(encoded) = encode_only(kind, &refs).expect("encoded") else { continue };
            let flat = decode_flat_at(&encoded, &positions).expect("decoded");
            assert_eq!(flat.into_values(), wanted, "{kind:?}");
            let none = decode_flat_at(&encoded, &[]).expect("decoded");
            assert!(none.is_empty(), "{kind:?}");
            assert!(decode_flat_at(&encoded, &[4, 3]).is_err(), "{kind:?}");
            assert!(decode_flat_at(&encoded, &[1000]).is_err(), "{kind:?}");
        }
        let fsst = encode_only(Kind::Fsst, &refs).expect("encoded").expect("compressible");
        assert_eq!(decode_flat_at(&fsst, &positions).expect("decoded").into_values(), wanted);
    }

    /// An equality on the codes picks out the values a compare of the strings would, through a
    /// column table too, and a chunk of another kind is left to the caller.
    #[test]
    fn equals_in_answers_what_comparing_the_strings_answers() {
        let mut values = urls(1000);
        values[7] = b"Sweden".to_vec();
        values[500] = b"Sweden".to_vec();
        values[901] = Vec::new();
        let refs: Vec<&[u8]> = values.iter().map(Vec::as_slice).collect();
        let literals: [&[u8]; 4] = [b"Sweden", refs[3], refs[998], b"not there at all"];
        let wanted: Vec<bool> = refs.iter().map(|value| literals.contains(value)).collect();
        let fsst = encode_only(Kind::Fsst, &refs).expect("encoded").expect("compressible");
        assert_eq!(equals_in(&fsst, &literals).expect("answered"), Some(wanted.clone()));
        assert_eq!(equals_in(&fsst, &[]).expect("answered"), Some(vec![false; 1000]));
        let empty = equals_in(&fsst, &[b"".as_slice()]).expect("answered").expect("fsst");
        assert!(empty[901] && empty.iter().filter(|&&one| one).count() == 1);
        let shape = with_symbols(front_lz(), &[refs[..512].to_vec()]);
        let shared = encode_with(&refs, &shape).expect("encoded");
        if let Some(held) = equals_in(&shared, &literals).expect("answered") {
            assert_eq!(held, wanted, "against the column table");
        }
        let plain = encode_only(Kind::Plain, &refs).expect("encoded").expect("plain");
        assert_eq!(equals_in(&plain, &literals).expect("answered"), None);
    }

    fn front_lz() -> Settled {
        Settled::new(vec![Kind::Front, Kind::Lz], vec![integer::Kind::Packed])
    }

    /// The point of the table: every block of a column compresses against one table trained once,
    /// and what it writes still reads back as the values, including a block the table was not
    /// trained on.
    #[test]
    fn a_block_compressed_against_the_column_table_reads_back() {
        let values = urls(4096);
        let refs: Vec<&[u8]> = values.iter().map(Vec::as_slice).collect();
        let blocks: Vec<Vec<&[u8]>> = refs.chunks(1024).take(2).map(<[&[u8]]>::to_vec).collect();
        let shape = with_symbols(front_lz(), &blocks);
        assert!(shape.symbols(2).is_some(), "FRONT then LZ leaves FSST the third level");
        assert!(shape.symbols(1).is_none(), "and only that one");
        for block in refs.chunks(1024) {
            let encoded = encode_with(block, &shape).expect("encoded");
            assert_eq!(decode(&encoded).expect("decoded"), block.to_vec());
        }
    }

    /// A shape that settles on `PLAIN` never tries FSST, so there is nothing to train.
    #[test]
    fn a_shape_ending_in_plain_gets_no_table() {
        let values = urls(1024);
        let refs: Vec<&[u8]> = values.iter().map(Vec::as_slice).collect();
        let plain = Settled::new(vec![Kind::Lz, Kind::Plain], vec![integer::Kind::Packed]);
        let shape = with_symbols(plain, std::slice::from_ref(&refs));
        assert!((0..=MAX_DEPTH).all(|depth| shape.symbols(depth).is_none()));
        let fsst = Settled::new(vec![Kind::Fsst], vec![integer::Kind::Packed]);
        assert!(with_symbols(fsst, &[refs]).symbols(0).is_some());
    }

    /// The same values with a scrambled identifier stuck on the front of each, for the tests that
    /// need neighbouring values to have nothing in common. Shuffling the order is not enough,
    /// because two URLs picked at random still agree on a scheme and often on a host.
    fn keyed(values: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
        values
            .into_iter()
            .enumerate()
            .map(|(index, value)| {
                let key = (index as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15) % 1_000_000_007;
                let mut out = format!("{key:010}/").into_bytes();
                out.extend_from_slice(&value);
                out
            })
            .collect()
    }

    fn borrow(values: &[Vec<u8>]) -> Vec<&[u8]> {
        values.iter().map(Vec::as_slice).collect()
    }

    fn round_trip(values: &[Vec<u8>]) -> Vec<u8> {
        let borrowed = borrow(values);
        let bytes = encode(&borrowed).unwrap();
        let back = decode(&bytes).unwrap();
        assert_eq!(back, values, "{}", describe(&bytes).unwrap());
        check_flat(&bytes, values);
        bytes
    }

    /// The flat form holds the same values and lays them out the way a caller with its own offsets
    /// expects. Called from [`round_trip`], so every shape any test in here reaches is checked.
    fn check_flat(bytes: &[u8], values: &[Vec<u8>]) {
        let flat = decode_flat(bytes).unwrap();
        let shape = describe(bytes).unwrap();
        assert_eq!(flat.len(), values.len(), "{shape}");
        assert_eq!(flat.iter().collect::<Vec<_>>(), borrow(values), "{shape}");
        assert_eq!(flat.bytes(), values.concat(), "{shape}");
        assert_eq!(flat.get(values.len()), None, "{shape}");
    }

    fn kind_of(bytes: &[u8]) -> Kind {
        Kind::from_tag(bytes[0]).unwrap()
    }

    #[test]
    fn every_shape_decodes_flat_to_what_it_decodes_split() {
        // round_trip only sees the shape the chooser picked, which on any one column is one of the
        // six. This walks all of them, so PLAIN reading its payload in one go and FRONT copying a
        // prefix out of the buffer it is filling are both covered on data they apply to.
        let columns =
            [urls(600), keyed(urls(600)), vec![b"same".to_vec(); 400], vec![Vec::new(); 7]];
        for values in &columns {
            let borrowed = borrow(values);
            for kind in offered(&borrowed) {
                let Some(bytes) = encode_only(kind, &borrowed).unwrap() else {
                    continue;
                };
                assert_eq!(decode(&bytes).unwrap(), *values, "{}", kind.name());
                let flat = decode_flat(&bytes).unwrap();
                assert_eq!(flat.iter().collect::<Vec<_>>(), borrowed, "{}", kind.name());
                assert_eq!(flat.bytes(), values.concat(), "{}", kind.name());
            }
        }
    }

    #[test]
    fn a_front_coded_chunk_that_shares_more_than_it_has_is_an_error() {
        // The prefix chain is the one place the flat decoder reads back out of the buffer it is
        // filling, so a prefix longer than the value before it is what would hand back somebody
        // else's bytes rather than fail. Built by hand because no encoder produces one.
        let suffixes: [&[u8]; 2] = [b"abc", b"x"];
        let mut bytes = vec![Kind::Front.tag()];
        put_u32(&mut bytes, 2);
        bytes.extend_from_slice(&integer::encode(&[0, 9]).unwrap());
        bytes.extend_from_slice(&encode_only(Kind::Plain, &suffixes).unwrap().unwrap());
        let error = decode_flat(&bytes).expect_err("a nine byte prefix of a three byte value");
        assert_eq!(error.message(), "a value shares 9 bytes with a value 3 bytes long");
        assert_eq!(decode(&bytes).unwrap_err().message(), error.message());
    }

    #[test]
    fn the_dictionary_is_sorted_and_the_codes_point_back_at_the_values() {
        // The two things the dictionary path has to get right, and the reason it is one function
        // now rather than a sort followed by a binary search per row.
        let values = vec![
            b"pear".to_vec(),
            b"apple".to_vec(),
            b"pear".to_vec(),
            b"cherry".to_vec(),
            b"apple".to_vec(),
        ];
        let borrowed = borrow(&values);
        let (entries, codes) = dictionary_of(&borrowed);
        assert_eq!(entries, vec![b"apple".as_slice(), b"cherry".as_slice(), b"pear".as_slice()]);
        assert_eq!(codes, vec![2, 0, 2, 1, 0]);
        for (code, value) in codes.iter().zip(&borrowed) {
            assert_eq!(entries[*code as usize], *value);
        }
    }

    #[test]
    fn a_column_with_nothing_repeated_has_no_duplicates_and_one_with_anything_does() {
        let distinct: Vec<Vec<u8>> =
            (0..5000).map(|index| format!("value-{index}").into_bytes()).collect();
        assert!(!has_duplicates(&borrow(&distinct)));

        // One repeat at the far end, so a check that gave up early would miss it.
        let mut repeated = distinct.clone();
        repeated.push(b"value-0".to_vec());
        assert!(has_duplicates(&borrow(&repeated)));

        assert!(!has_duplicates(&borrow(&Vec::new())));
        assert!(!has_duplicates(&borrow(&[b"one".to_vec()])));
        assert!(has_duplicates(&borrow(&vec![b"same".to_vec(); 2])));
    }

    #[test]
    fn long_values_that_differ_only_at_the_end_are_not_confused_for_each_other() {
        // The hash is eight bytes at a time and the table verifies every hit, so this is the case
        // that says the verify is really there rather than the hash being trusted.
        let stem = "http://www.example.com/a/very/long/path/that/goes/on?session=";
        let values: Vec<Vec<u8>> =
            (0..2000).map(|index| format!("{stem}{index}").into_bytes()).collect();
        assert!(!has_duplicates(&borrow(&values)));
        let (entries, codes) = dictionary_of(&borrow(&values));
        assert_eq!(entries.len(), values.len());
        assert_eq!(codes.len(), values.len());
    }

    #[test]
    fn what_the_chooser_returns_is_the_smallest_of_what_it_was_offered() {
        // `offered` and `encode_only` are what `cargo xtask encode` splits the chooser's seconds
        // with, so they have to describe the chooser that actually runs rather than a second copy
        // of its rules that drifts. This is the assertion that keeps the two the same thing: walk
        // the list, encode each one alone, and the smallest has to be byte for byte what `encode`
        // came back with.
        for values in [urls(400), keyed(urls(400)), vec![b"same".to_vec(); 50], Vec::new()] {
            let borrowed = borrow(&values);
            let chosen = encode(&borrowed).unwrap();
            let mut smallest: Option<Vec<u8>> = None;
            for kind in offered(&borrowed) {
                let Some(bytes) = encode_only(kind, &borrowed).unwrap() else {
                    continue;
                };
                if smallest.as_ref().is_none_or(|best| bytes.len() < best.len()) {
                    smallest = Some(bytes);
                }
            }
            assert_eq!(smallest.as_deref(), Some(chosen.as_slice()), "{}", values.len());
        }
    }

    fn raw_size(values: &[Vec<u8>]) -> usize {
        values.iter().map(Vec::len).sum::<usize>() + values.len() * 4
    }

    #[test]
    fn a_matched_chunk_replays_literals_whether_or_not_they_are_compressed() {
        // The literals of a matched chunk are a chunk of their own, and when that chunk is
        // compressed the replay decompresses each run straight into the output instead of into a
        // buffer it then copies out of. Both columns here are checked value for value by
        // round_trip, so what is left is to show that one of them takes the fused path and the
        // other takes the one that decodes the literals first, and that the two agree.
        let compressed = describe(&round_trip(&keyed(urls(20_000)))).unwrap();
        assert!(compressed.starts_with("LZ(") && compressed.contains(", FSST["), "{compressed}");

        let buffered = describe(&round_trip(&keyed(urls(300)))).unwrap();
        assert!(buffered.starts_with("LZ(") && buffered.contains(", PLAIN("), "{buffered}");
    }

    #[test]
    fn a_copy_back_writes_what_a_byte_at_a_time_copy_writes_at_every_distance() {
        // The wide stores read bytes the same copy wrote a step earlier once the copy is longer
        // than its distance, so every distance either side of eight and sixteen is checked against
        // the plain loop, at lengths that end short of, on and past a whole store.
        let seed: Vec<u8> = (0..40u8).map(|byte| byte.wrapping_mul(37).wrapping_add(11)).collect();
        for offset in 1..=seed.len() {
            for length in 1..=50 {
                let mut wanted = seed.clone();
                for _ in 0..length {
                    wanted.push(wanted[wanted.len() - offset]);
                }
                let mut out = seed.clone();
                out.resize(seed.len() + length + REPLAY_SLACK, 0);
                let end = copy_back(&mut out, 0, seed.len(), offset, length).unwrap();
                assert_eq!(&out[..end], wanted.as_slice(), "offset {offset} length {length}");
            }
        }
        let mut short = vec![1, 2, 3, 0];
        assert!(copy_back(&mut short, 0, 3, 1, 2).is_err(), "past the end of the buffer");
        assert!(copy_back(&mut short, 0, 3, 4, 1).is_err(), "further back than the output");
    }

    #[test]
    fn an_empty_chunk_round_trips() {
        let bytes = round_trip(&[]);
        assert_eq!(kind_of(&bytes), Kind::Plain);
    }

    #[test]
    fn a_constant_column_costs_what_one_value_costs() {
        let values = vec![b"https://www.example.com/".to_vec(); 100_000];
        let bytes = round_trip(&values);
        assert_eq!(kind_of(&bytes), Kind::Constant);
        assert_eq!(bytes.len(), 9 + 24);
    }

    #[test]
    fn a_url_column_of_unique_values_is_matched_rather_than_only_compressed() {
        // Every value distinct, so a dictionary is the values plus an index and cannot win, and
        // every value starts with an identifier of its own, so neighbours share nothing and front
        // coding cannot win either. This used to be the case that fell back to FSST, on the
        // reasoning that a symbol table was the only thing that could reach repeated vocabulary
        // with no structure around it. That reasoning was wrong and #575 is the measurement: the
        // vocabulary repeats at a distance, and a match finder reaches distance where a 255 symbol
        // table of at most eight bytes each does not.
        let values = keyed(urls(20_000));
        let bytes = round_trip(&values);
        assert_eq!(kind_of(&bytes), Kind::Lz);

        // Against the encoding that used to win, on the same values, so the claim is a comparison
        // and not just a label.
        let borrowed: Vec<&[u8]> = values.iter().map(Vec::as_slice).collect();
        let fsst = encode_as(Kind::Fsst, &borrowed, 0, &EXHAUSTIVE).unwrap().unwrap();
        assert!(bytes.len() < fsst.len(), "{} against FSST {}", bytes.len(), fsst.len());

        // Eleven bytes of every value are the identifier and a separator and nothing compresses
        // them, so the ratio here is lower than the one FSST gets on the URLs on their own.
        let ratio = raw_size(&values) as f64 / bytes.len() as f64;
        assert!(ratio > 4.0, "{ratio:.2}x");
    }

    #[test]
    fn a_sample_of_a_periodic_column_learns_every_phase_of_it() {
        // This column is periodic and its period is what a fixed stride would have divided. The
        // sample has to see all of it, because a table trained on one phase learns eight byte
        // symbols that only line up with that phase and has nothing shorter to fall back on. The
        // measured cost of getting this wrong was 3.4 times the compressed size.
        let values = urls(20_000);
        let borrowed = borrow(&values);
        let sample = sample_of(&borrowed);
        let mut phases: Vec<&[u8]> = sample
            .iter()
            .map(|value| {
                let query =
                    value.iter().position(|byte| *byte == b'?').expect("every value has a query");
                &value[..query]
            })
            .collect();
        phases.sort_unstable();
        phases.dedup();
        // Three hosts and four paths, and the sample has to contain all twelve of the combinations.
        assert_eq!(phases.len(), 12);
        let whole = SymbolTable::train(&borrowed);
        let sampled = SymbolTable::train(&sample);
        let mut on_whole = Vec::new();
        let mut on_sample = Vec::new();
        for value in &borrowed {
            whole.compress(value, &mut on_whole);
            sampled.compress(value, &mut on_sample);
        }
        // Training on a twentieth of the column is allowed to cost something. It is not allowed to
        // cost a factor.
        assert!(
            on_sample.len() < on_whole.len() * 5 / 4,
            "{} against {}",
            on_sample.len(),
            on_whole.len()
        );
    }

    #[test]
    fn a_repeating_column_becomes_a_dictionary_of_compressed_entries() {
        // The DICT_FSST row of the section 6.2 table, which is not an encoding of its own here: it
        // is a dictionary whose entries went back through the chooser. What the entries then get
        // is whatever wins on them, and since #575 that is the match finder rather than front
        // coding with the leftovers FSST compressed. The point of the test is unchanged: nobody
        // named the shape and the chooser arrived at it.
        //
        // The rows pick their value by a hash of the row number. They used to walk the values in
        // a fixed stride, which makes the dictionary codes a cycle whose differences take a
        // quarter as many values as the codes do, and a real column's codes are not that.
        let distinct = urls(500);
        let values: Vec<Vec<u8>> = (0..50_000_u64)
            .map(|index| {
                let hashed = (index.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 32) as usize;
                distinct[hashed % distinct.len()].clone()
            })
            .collect();
        let bytes = round_trip(&values);
        assert_eq!(kind_of(&bytes), Kind::Dict);
        let shape = describe(&bytes).unwrap();
        assert!(shape.starts_with("DICT(LZ("), "{shape}");
        let ratio = raw_size(&values) as f64 / bytes.len() as f64;
        assert!(ratio > 20.0, "{ratio:.2}x, {shape}");
    }

    #[test]
    fn a_column_of_long_runs_costs_almost_nothing() {
        // A dictionary makes the codes an integer chunk, and the integer chunk knows what to do
        // with runs, so run length encoding of strings falls out of the recursion.
        let distinct = urls(50);
        let mut values = Vec::new();
        for entry in &distinct {
            values.extend(std::iter::repeat_n(entry.clone(), 1000));
        }
        let bytes = round_trip(&values);
        let shape = describe(&bytes).unwrap();
        assert!(shape.contains("RLE"), "{shape}");
        assert!(bytes.len() < 2000, "{} bytes: {shape}", bytes.len());
    }

    #[test]
    fn incompressible_strings_stay_close_to_their_own_size() {
        // The case where nothing works. It has to land on PLAIN or on an FSST that is not much
        // worse, rather than on a dictionary of every value in the column.
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let values: Vec<Vec<u8>> = (0..2000)
            .map(|_| {
                (0..32)
                    .map(|_| {
                        state ^= state << 13;
                        state ^= state >> 7;
                        state ^= state << 17;
                        state as u8
                    })
                    .collect()
            })
            .collect();
        let bytes = round_trip(&values);
        assert!(bytes.len() < 2000 * 32 + 3000, "{} bytes", bytes.len());
    }

    #[test]
    fn lengths_are_stored_rather_than_offsets() {
        // Every value is 24 bytes, so the lengths are a constant chunk and cost 13 bytes for the
        // whole column. Offsets would be 100,000 increasing integers.
        let values: Vec<Vec<u8>> =
            (0..100_000).map(|index| format!("{index:024}").into_bytes()).collect();
        let borrowed = borrow(&values);
        let bytes = encode_only(Kind::Plain, &borrowed).unwrap().unwrap();
        assert_eq!(bytes.len(), 5 + 13 + 100_000 * 24);
    }

    #[test]
    fn empty_strings_are_values_and_not_nulls() {
        let values = vec![Vec::new(), b"a".to_vec(), Vec::new(), b"bb".to_vec()];
        round_trip(&values);
    }

    #[test]
    fn a_chunk_with_one_value_round_trips() {
        round_trip(&[b"only".to_vec()]);
    }

    #[test]
    fn every_candidate_that_applies_decodes_to_the_input() {
        let values = urls(3000);
        let borrowed = borrow(&values);
        let applicable = candidates(&borrowed, 0);
        assert!(applicable.len() >= 2, "{applicable:?}");
        for kind in applicable {
            let bytes = encode_only(kind, &borrowed).unwrap().unwrap();
            assert_eq!(decode(&bytes).unwrap(), values, "{}", kind.name());
        }
    }

    #[test]
    fn the_chooser_picks_the_smallest_candidate() {
        let values = urls(2000);
        let borrowed = borrow(&values);
        let chosen = encode(&borrowed).unwrap();
        for (_, size) in candidate_sizes(&borrowed).unwrap() {
            assert!(chosen.len() <= size);
        }
    }

    #[test]
    fn a_truncated_chunk_is_an_error_and_not_a_panic() {
        let values = urls(40);
        let bytes = encode(&borrow(&values)).unwrap();
        for len in 0..bytes.len() {
            assert!(decode(&bytes[..len]).is_err(), "{len} bytes decoded");
        }
    }

    #[test]
    fn trailing_bytes_are_an_error() {
        let mut bytes = encode(&borrow(&urls(10))).unwrap();
        bytes.push(0);
        let error = decode(&bytes).unwrap_err();
        assert!(error.message().contains("left over"), "{error}");
    }

    #[test]
    fn an_unknown_tag_is_an_error() {
        let error = decode(&[99, 0, 0, 0, 0]).unwrap_err();
        assert!(error.message().contains("unknown string encoding tag"), "{error}");
    }

    #[test]
    fn a_dictionary_code_outside_the_dictionary_is_an_error() {
        let mut bytes = vec![Kind::Dict.tag()];
        put_u32(&mut bytes, 1);
        bytes.extend_from_slice(&encode(&[b"one".as_slice()]).unwrap());
        bytes.extend_from_slice(&integer::encode(&[9]).unwrap());
        let error = decode(&bytes).unwrap_err();
        assert!(error.message().contains("not in the dictionary"), "{error}");
    }

    #[test]
    fn a_sorted_column_of_urls_is_front_coded() {
        // The M1 finding, in a test. Sorted URLs share a host and most of a path with the URL next
        // to them, FSST cannot reach those bytes because it compresses each value on its own, and
        // front coding is the shape that reaches them.
        let mut values = urls(20_000);
        values.sort();
        let bytes = round_trip(&values);
        assert_eq!(kind_of(&bytes), Kind::Front);
        let shape = describe(&bytes).unwrap();
        let mut plain = Vec::new();
        let borrowed = borrow(&values);
        for (kind, size) in candidate_sizes(&borrowed).unwrap() {
            if kind == Kind::Fsst {
                plain.push(size);
            }
        }
        let fsst = plain[0];
        assert!(bytes.len() * 2 < fsst, "{} against FSST {fsst}: {shape}", bytes.len());
    }

    #[test]
    fn a_column_with_nothing_to_share_is_not_offered_front_coding() {
        // The candidate costs an encode of the whole column, so a column whose neighbours have
        // nothing in common must not be paying for it.
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let values: Vec<Vec<u8>> = (0..2000)
            .map(|_| {
                (0..24)
                    .map(|_| {
                        state ^= state << 13;
                        state ^= state >> 7;
                        state ^= state << 17;
                        (state % 251) as u8
                    })
                    .collect()
            })
            .collect();
        let borrowed = borrow(&values);
        assert!(!candidates(&borrowed, 0).contains(&Kind::Front));
    }

    #[test]
    fn a_prefix_longer_than_the_value_before_it_is_an_error() {
        let mut bytes = vec![Kind::Front.tag()];
        put_u32(&mut bytes, 2);
        bytes.extend_from_slice(&integer::encode(&[0, 9]).unwrap());
        bytes.extend_from_slice(&encode(&[b"one".as_slice(), b"two".as_slice()]).unwrap());
        let error = decode(&bytes).unwrap_err();
        assert!(error.message().contains("shares 9 bytes"), "{error}");
    }

    #[test]
    fn a_negative_prefix_is_an_error() {
        let mut bytes = vec![Kind::Front.tag()];
        put_u32(&mut bytes, 1);
        bytes.extend_from_slice(&integer::encode(&[-1]).unwrap());
        bytes.extend_from_slice(&encode(&[b"one".as_slice()]).unwrap());
        let error = decode(&bytes).unwrap_err();
        assert!(error.message().contains("negative shared prefix"), "{error}");
    }

    #[test]
    fn a_negative_length_is_an_error() {
        let mut bytes = vec![Kind::Plain.tag()];
        put_u32(&mut bytes, 1);
        bytes.extend_from_slice(&integer::encode(&[-1]).unwrap());
        let error = decode(&bytes).unwrap_err();
        assert!(error.message().contains("negative string length"), "{error}");
    }

    #[test]
    fn the_sample_is_spread_across_the_chunk_and_not_taken_from_the_front() {
        // A sorted column whose first 64 KB says nothing about the rest of it. If the sample were
        // the front, the table would learn `aaaa` and escape every `zzzz`.
        let mut values: Vec<Vec<u8>> = Vec::new();
        for index in 0..20_000 {
            let head = if index < 10_000 { "aaaaaaaaaaaaaaaa" } else { "zzzzzzzzzzzzzzzz" };
            values.push(format!("{head}/{index:08}").into_bytes());
        }
        let borrowed = borrow(&values);
        let sample = sample_of(&borrowed);
        let first_half = sample.iter().filter(|value| value.starts_with(b"aaaa")).count();
        let second_half = sample.len() - first_half;
        assert!(first_half > 0 && second_half > 0, "{first_half} and {second_half}");
        let bytes = round_trip(&values);
        let ratio = raw_size(&values) as f64 / bytes.len() as f64;
        assert!(ratio > 4.0, "{ratio:.2}x");
    }
}
