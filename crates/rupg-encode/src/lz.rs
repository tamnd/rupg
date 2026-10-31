//! A match finder, which is the one thing this crate did not have.
//!
//! # Why
//!
//! Every other encoding here is a local transform. Front coding looks at exactly one value back.
//! FSST builds 255 symbols of at most eight bytes. Run length encoding looks at the previous value.
//! None of them can say "this forty character path segment appeared six hundred values ago", and on
//! the data this engine is measured against that is where most of the redundancy is.
//!
//! Measured on one chunk of 122,880 sorted URLs out of ClickBench `hits`, in tiles of about 190 KB
//! so a lookup decompresses a tile rather than a chunk. The cascade without this got 3.16 times.
//! Front coding followed by this, with the three streams below handed to the encoders that already
//! existed, got 5.15 times. deflate at the same tile size got 5.00 and zstd at level three got
//! 5.63. Issue #575 has the full table.
//!
//! # Why there is no entropy coder
//!
//! Because it is not where the win is, which was worth measuring rather than assuming. The same
//! matcher with byte aligned tokens gets 3.57 times, barely above the 3.16 the cascade already
//! managed. Sending the same tokens through [`crate::integer`] instead gets 4.40. So what makes
//! matching pay here is the bitpacking and frame of reference this crate already has, applied to
//! the length and offset streams, and not a Huffman coder anybody has to write.
//!
//! # The three streams
//!
//! A token is a run of literal bytes followed by a copy from earlier in the output. The literal runs
//! go back through the string cascade, so FSST still gets a go at the bytes no match covered, and
//! the lengths and offsets go through the integer cascade. Splitting them matters: interleaved
//! tokens are three distributions in one stream and none of the integer encodings can see any of
//! them.
//!
//! # The segment
//!
//! Matching restarts every [`SEGMENT`] bytes. That bounds the hash chain's memory, which would
//! otherwise be one entry per byte of a 23 MB chunk, and it bounds the worst case search. Offsets
//! are still written relative to the whole output, so a decoder copies from `out.len() - offset`
//! and never has to know where a segment began.

// Lifted from rudb crates/rudb-encoding/src/lz.rs at cd9f9676 (2026-10-05).
// Changes: the error type is rupg_common::Error. rebuild is for the tests only, as only multi.rs of rudb called it. as_chunks in place of chunks_exact, for clippy.

use rupg_common::{Error, Result};

/// How many bytes are matched against before the hash chain is thrown away and started again.
///
/// 256 KiB because the measurement in #575 was taken in tiles of 1024 sorted URLs, which is about
/// 190 KB, and because the chain costs four bytes a byte of segment. Larger segments compress
/// slightly better and cost proportionally more memory. Smaller ones are what a format with finer
/// random access would want, and the number is here rather than spread through the code so that
/// trade can be made in one place.
const SEGMENT: usize = 256 * 1024;

/// The shortest copy worth emitting, in bytes.
///
/// Below four the token costs more than the bytes it saves, which is the same number deflate and
/// lz4 landed on for the same reason.
const MIN_MATCH: usize = 4;

/// How many bits of hash pick a chain.
const HASH_BITS: u32 = 16;

/// How many bytes a position is hashed over, which is the shortest copy the chains lead to.
///
/// Eight rather than [`MIN_MATCH`]. Over four bytes, the chains of a block of URLs are long with
/// positions that share `http` or `.com/` and nothing after it, and the walk spends its tries on
/// them. Over eight a chain holds positions that agree for eight bytes, so the tries go to copies
/// worth having. On the `URL`, `Title` and `Referer` columns of `hits_0` in blocks of 1024 values
/// the matcher took 25, 39 and 19 percent less time, and the blocks came out 5, 5 and 6 percent
/// smaller, because the tries it no longer spends on short copies find longer ones. A copy of four
/// to seven bytes is only found now when a hash collision leads to it, and those rarely saved much.
const HASH_LEN: usize = 8;

/// How far back along one hash chain the search goes before it settles for what it has.
///
/// A greedy matcher with a bounded chain, which is what deflate calls a compression level. Raising
/// this buys tenths of a ratio point for a proportional amount of time.
const MAX_TRIES: usize = 32;

/// What a match finder produced, as three streams rather than one.
pub(crate) struct Tokens<'a> {
    /// The bytes no copy covered, one run a token, empty where a copy followed a copy.
    pub literals: Vec<&'a [u8]>,
    /// How many bytes each token copies, and zero for the last token when it is literals only.
    pub lengths: Vec<i64>,
    /// How far back each token copies from, counted from the end of the output so far.
    pub offsets: Vec<i64>,
}

/// What one segment's matching produced, before the literal ranges become slices.
#[derive(Default)]
struct Raw {
    /// Where each token's literal run sits in the input, as a half open range.
    runs: Vec<(usize, usize)>,
    lengths: Vec<i64>,
    offsets: Vec<i64>,
}

/// Splits `input` into literal runs and back references.
pub(crate) fn tokens_of(input: &[u8]) -> Tokens<'_> {
    let mut raw = Raw::default();
    let mut head = vec![u32::MAX; 1 << HASH_BITS];
    let span = SEGMENT.min(input.len()).max(1);
    let mut prev = vec![u32::MAX; span];

    let mut start = 0;
    while start < input.len() {
        let end = (start + SEGMENT).min(input.len());
        head.fill(u32::MAX);
        matches_in(input, start, end, &mut head, &mut prev, &mut raw);
        start = end;
    }
    Tokens {
        literals: raw.runs.iter().map(|(from, to)| &input[*from..*to]).collect(),
        lengths: raw.lengths,
        offsets: raw.offsets,
    }
}

/// One segment's worth of matching, appending to `tokens`.
fn matches_in(
    input: &[u8],
    start: usize,
    end: usize,
    head: &mut [u32],
    prev: &mut [u32],
    raw: &mut Raw,
) {
    let mut literal_start = start;
    let mut at = start;
    let (mut key, mut first) = chain_at(input, at, end, head);
    while at + HASH_LEN <= end {
        // The chain for the next position is looked up before this one is walked. The walk is a
        // run of branches on bytes nobody can predict, and a load issued after it waits for all
        // of them, where one issued before it is in flight while they resolve. Most positions are
        // not the start of a copy, so the next one is usually the one after this. On a load of a
        // million rows of `hits` the head lookup was about a ninth of the matcher's cycles.
        let (next_key, next_first) = chain_at(input, at + 1, end, head);
        let found = longest(input, at, end, first, prev, start);
        let slot = at - start;
        prev[slot] = first;
        head[key] = slot as u32;
        match found {
            Some((length, offset)) => {
                push(raw, (literal_start, at), length, offset);
                for step in 1..length {
                    insert(input, at + step, end, head, prev, start);
                }
                at += length;
                literal_start = at;
                (key, first) = chain_at(input, at, end, head);
            }
            None => {
                at += 1;
                // The lookup went ahead of the insert above, which changed it when both
                // positions hash the same.
                first = if next_key == key { slot as u32 } else { next_first };
                key = next_key;
            }
        }
    }
    if literal_start < end {
        push(raw, (literal_start, end), 0, 0);
    }
}

/// Walks one hash chain and keeps the longest copy it finds.
fn longest(
    input: &[u8],
    at: usize,
    end: usize,
    mut candidate: u32,
    prev: &[u32],
    start: usize,
) -> Option<(usize, usize)> {
    let mut best: Option<(usize, usize)> = None;
    let mut tries = 0;
    while candidate != u32::MAX && tries < MAX_TRIES {
        let position = start + candidate as usize;
        if position >= at {
            break;
        }
        // Only a longer copy replaces the one in hand, and a copy longer than `had` has to agree at
        // byte `had`. Most candidates on a long chain do not, so one byte turns them away before
        // the full compare. Nothing can be longer than what is left of the segment, so a copy that
        // reaches the end stops the walk.
        if let Some((had, _)) = best {
            if at + had >= end {
                break;
            }
            if input[position + had] != input[at + had] {
                candidate = prev[candidate as usize];
                tries += 1;
                continue;
            }
        }
        let length = shared(&input[position..end], &input[at..end]);
        if length >= MIN_MATCH && best.is_none_or(|(had, _)| length > had) {
            best = Some((length, at - position));
        }
        candidate = prev[candidate as usize];
        tries += 1;
    }
    best
}

/// The hash of `at` and the newest position on its chain, or nothing for a position too near the
/// end to hash.
fn chain_at(input: &[u8], at: usize, end: usize, head: &[u32]) -> (usize, u32) {
    if at + HASH_LEN > end {
        return (0, u32::MAX);
    }
    let key = hash(&input[at..at + HASH_LEN]);
    (key, head[key])
}

/// Puts `at` at the head of its chain, so later positions can match against it.
fn insert(input: &[u8], at: usize, end: usize, head: &mut [u32], prev: &mut [u32], start: usize) {
    if at + HASH_LEN > end {
        return;
    }
    let key = hash(&input[at..at + HASH_LEN]);
    let slot = at - start;
    prev[slot] = head[key];
    head[key] = slot as u32;
}

fn push(raw: &mut Raw, run: (usize, usize), length: usize, offset: usize) {
    raw.runs.push(run);
    raw.lengths.push(length as i64);
    raw.offsets.push(offset as i64);
}

/// Rebuilds the bytes [`tokens_of`] took apart.
///
/// # Errors
///
/// If the three streams disagree on how many tokens there are, or if a copy reaches back further
/// than the output goes, which is what a corrupt or hand written chunk looks like from here.
#[cfg(test)]
pub(crate) fn rebuild(
    literals: &[Vec<u8>],
    lengths: &[i64],
    offsets: &[i64],
    total: usize,
) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(total);
    replay(
        literals.len(),
        |index, into: &mut Vec<u8>| {
            into.extend_from_slice(&literals[index]);
            Ok(())
        },
        lengths,
        offsets,
        &mut out,
    )?;
    Ok(out)
}

/// [`rebuild`] appending to a buffer somebody else owns, with the literal runs still packed.
///
/// The string decoder builds its output as one buffer, so the bytes this produces are the values
/// rather than something to cut the values out of afterwards. Appending rather than returning is
/// what makes that true, and a copy offset counts back from where the replay has got to rather than
/// from the start of the buffer, so anything already in it is out of reach and stays that way.
///
/// # Errors
///
/// As [`rebuild`].
pub(crate) fn rebuild_into(
    literals: &crate::string::Flat,
    lengths: &[i64],
    offsets: &[i64],
    out: &mut Vec<u8>,
) -> Result<()> {
    replay(
        literals.len(),
        |index, into: &mut Vec<u8>| {
            into.extend_from_slice(literals.get(index).expect("in range"));
            Ok(())
        },
        lengths,
        offsets,
        out,
    )
}

/// Replays the tokens, with the caller saying how a literal run reaches the output.
///
/// The run is appended by a callback rather than handed over as a slice, and that is what lets the
/// thing holding the literals be something other than a buffer of them. A matched chunk's literals
/// are themselves a chunk, and on the ClickBench `URL` column that chunk is FSST compressed, so
/// decoding it into a buffer and then copying every run out of that buffer reads and writes each
/// literal byte twice. With this the decompression writes where the byte belongs the first time.
///
/// Runs are asked for in order, from zero, exactly once each, which is what lets a caller answer
/// from a cursor rather than from an index.
///
/// # Errors
///
/// Whatever the callback reports, on top of what [`rebuild`] reports.
pub(crate) fn replay(
    runs: usize,
    mut run: impl FnMut(usize, &mut Vec<u8>) -> Result<()>,
    lengths: &[i64],
    offsets: &[i64],
    out: &mut Vec<u8>,
) -> Result<()> {
    if runs != lengths.len() || lengths.len() != offsets.len() {
        return Err(Error::internal(format!(
            "a matched chunk has {runs} literal runs, {} lengths and {} offsets",
            lengths.len(),
            offsets.len()
        )));
    }
    let base = out.len();
    for index in 0..runs {
        run(index, out)?;
        let length = usize::try_from(lengths[index])
            .map_err(|_| Error::internal("a negative copy length"))?;
        if length == 0 {
            continue;
        }
        let offset = usize::try_from(offsets[index])
            .map_err(|_| Error::internal("a negative copy offset"))?;
        if offset == 0 || offset > out.len() - base {
            return Err(Error::internal(format!(
                "a copy reaches {offset} bytes back into {} bytes of output",
                out.len() - base
            )));
        }
        let from = out.len() - offset;
        if offset >= length {
            // Nothing the copy reads is anything it writes, so it is a block move and the compiler
            // gets to use one. This is the common case by a long way: an overlapping copy is how a
            // repeating run is written and a run is a small part of real text.
            out.extend_from_within(from..from + length);
        } else {
            // Byte at a time because the copy reads what it just wrote, which is how a run of one
            // repeated byte is written as a single token.
            out.reserve(length);
            for step in 0..length {
                let byte = out[from + step];
                out.push(byte);
            }
        }
    }
    Ok(())
}

fn hash(bytes: &[u8]) -> usize {
    let word = u64::from_le_bytes(bytes[..HASH_LEN].try_into().expect("eight bytes to hash"));
    (word.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> (64 - HASH_BITS)) as usize
}

/// How many bytes `a` and `b` start with in common.
///
/// Eight bytes at a time, because the copies in sorted text run to tens of bytes and this compare
/// was the hottest loop of a load: a dictionary of URLs spends most of its encode here.
fn shared(a: &[u8], b: &[u8]) -> usize {
    let cap = a.len().min(b.len());
    let mut n = 0;
    for (left, right) in a[..cap].as_chunks::<8>().0.iter().zip(b[..cap].as_chunks::<8>().0) {
        let left = u64::from_le_bytes(*left);
        let right = u64::from_le_bytes(*right);
        let differ = left ^ right;
        if differ != 0 {
            return n + (differ.trailing_zeros() / 8) as usize;
        }
        n += 8;
    }
    while n < cap && a[n] == b[n] {
        n += 1;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Times the matcher over real text and says what the blocks come to, for comparing matchers.
    ///
    /// `LZ_DATA` names a directory of files with one value a line, in the order a dictionary first
    /// saw them. They are cut into blocks of 1024 values the way a dictionary's payload is, and each
    /// block is matched as it is and after front coding, which are the two ways a settled shape
    /// reaches this.
    ///
    /// The digest of the tokens is there so a change that is meant to be only faster can show it
    /// found the same copies. `LZ_ONLY` skips the full encodes, which otherwise take most of the
    /// time and hide what the matcher did under a profiler or a cycle count.
    #[test]
    #[ignore = "a measurement over real text, run by hand in release with LZ_DATA set"]
    #[allow(clippy::disallowed_methods)]
    fn measure_on_real_text() {
        use crate::string::{Kind, front_code, size_as};
        use std::time::{Duration, Instant};
        let Ok(dir) = std::env::var("LZ_DATA") else { return };
        for name in ["URL", "Title", "Referer"] {
            let text = std::fs::read(format!("{dir}/{name}.txt")).expect("a data file");
            let values: Vec<&[u8]> =
                text.split(|byte| *byte == b'\n').filter(|value| !value.is_empty()).collect();
            let (mut raw, mut lz, mut front) = (0, 0, 0);
            let mut spent = Duration::ZERO;
            let mut digest = 0u64;
            for block in values.chunks(1024) {
                let joined = block.concat();
                let suffixes = front_code(block).1.concat();
                raw += joined.len();
                let start = Instant::now();
                let tokens = [tokens_of(&joined), tokens_of(&suffixes)];
                spent += start.elapsed();
                for token in &tokens {
                    for (length, offset) in token.lengths.iter().zip(&token.offsets) {
                        digest = (digest ^ (*length as u64) ^ ((*offset as u64) << 32))
                            .wrapping_mul(0x0100_0000_01B3);
                    }
                }
                if std::env::var_os("LZ_ONLY").is_none() {
                    lz += size_as(Kind::Lz, block, 0).expect("encodes").expect("applies");
                    front += size_as(Kind::Front, block, 0).expect("encodes").expect("applies");
                }
            }
            println!(
                "{name}: {raw} bytes, matched in {:.1} ms, LZ {lz} ({:.3}x), FRONT {front} ({:.3}x), \
                 tokens {digest:016x}",
                spent.as_secs_f64() * 1e3,
                raw as f64 / lz as f64,
                raw as f64 / front as f64,
            );
        }
    }

    fn round_trip(input: &[u8]) {
        let tokens = tokens_of(input);
        let owned: Vec<Vec<u8>> = tokens.literals.iter().map(|run| run.to_vec()).collect();
        let back = rebuild(&owned, &tokens.lengths, &tokens.offsets, input.len()).unwrap();
        assert_eq!(back, input, "{} tokens", tokens.lengths.len());
    }

    #[test]
    fn nothing_round_trips() {
        round_trip(b"");
    }

    #[test]
    fn something_with_no_repeats_round_trips() {
        round_trip(b"abcdefghijklmnop");
    }

    #[test]
    fn a_repeat_becomes_a_copy() {
        let input = b"the same sentence twice, the same sentence twice";
        let tokens = tokens_of(input);
        assert!(tokens.lengths.iter().any(|length| *length >= MIN_MATCH as i64), "no copy emitted");
        round_trip(input);
    }

    #[test]
    fn a_run_of_one_byte_is_one_overlapping_copy() {
        // The copy reads bytes this same token is still writing, which is the case the byte at a
        // time loop in rebuild exists for.
        let input = vec![b'x'; 4096];
        round_trip(&input);
        let tokens = tokens_of(&input);
        assert!(tokens.lengths.len() < 8, "{} tokens for one repeated byte", tokens.lengths.len());
    }

    #[test]
    fn a_copy_that_overlaps_by_part_of_itself_round_trips() {
        // Three byte period, so a copy of any length past the third byte reads bytes it is still
        // writing but not the one it wrote last. That is the case either branch of rebuild could
        // get wrong on its own and neither a run of one byte nor a clean repeat reaches it.
        let input: Vec<u8> = (0..8192).map(|index| b"abc"[index % 3]).collect();
        round_trip(&input);
        let tokens = tokens_of(&input);
        assert!(
            tokens
                .offsets
                .iter()
                .zip(&tokens.lengths)
                .any(|(offset, length)| *offset > 1 && *offset < *length),
            "no partly overlapping copy emitted"
        );
    }

    #[test]
    fn rebuilding_into_a_buffer_cannot_reach_what_was_already_in_it() {
        // The string decoder replays into the buffer the values are going into, and that buffer
        // holds other values by the time it gets there. A copy offset counts back from where the
        // replay started, so the bytes before it are not something a corrupt chunk can reach.
        let input = b"the same sentence twice, the same sentence twice";
        let tokens = tokens_of(input);
        let packed =
            crate::string::encode_only(crate::string::Kind::Plain, &tokens.literals).unwrap();
        let literals = crate::string::decode_flat(&packed.expect("plain applies")).unwrap();
        let mut out = b"something that was here first".to_vec();
        let base = out.len();
        rebuild_into(&literals, &tokens.lengths, &tokens.offsets, &mut out).unwrap();
        assert_eq!(&out[..base], b"something that was here first");
        assert_eq!(&out[base..], input);

        let mut offsets = tokens.offsets.clone();
        let copy = offsets.iter().position(|offset| *offset > 0).expect("a copy");
        offsets[copy] += base as i64;
        let mut out = vec![0; base];
        let error = rebuild_into(&literals, &tokens.lengths, &offsets, &mut out)
            .expect_err("a copy reaching before the base");
        assert!(error.message().starts_with("a copy reaches"), "{}", error.message());
    }

    #[test]
    fn something_longer_than_a_segment_round_trips() {
        let mut input = Vec::new();
        while input.len() < SEGMENT * 2 + 1234 {
            input.extend_from_slice(b"http://example.com/some/path?query=value&more=stuff ");
        }
        round_trip(&input);
    }

    #[test]
    fn the_word_compare_finds_the_same_copies_as_a_byte_compare() {
        // The matcher as it was before `shared` compared words and `longest` turned candidates
        // away on one byte, kept to show the tokens have not moved, since every chunk a load has
        // written was cut by it.
        fn by_bytes(input: &[u8]) -> (Vec<(usize, usize)>, Vec<i64>, Vec<i64>) {
            let mut raw = Raw::default();
            let mut head = vec![u32::MAX; 1 << HASH_BITS];
            let mut prev = vec![u32::MAX; SEGMENT.min(input.len()).max(1)];
            let mut start = 0;
            while start < input.len() {
                let end = (start + SEGMENT).min(input.len());
                head.fill(u32::MAX);
                let mut literal_start = start;
                let mut at = start;
                while at + HASH_LEN <= end {
                    let mut candidate = head[hash(&input[at..at + HASH_LEN])];
                    let mut found: Option<(usize, usize)> = None;
                    let mut tries = 0;
                    while candidate != u32::MAX && tries < MAX_TRIES {
                        let position = start + candidate as usize;
                        if position >= at {
                            break;
                        }
                        let mut length = 0;
                        while at + length < end && input[position + length] == input[at + length] {
                            length += 1;
                        }
                        if length >= MIN_MATCH && found.is_none_or(|(had, _)| length > had) {
                            found = Some((length, at - position));
                        }
                        candidate = prev[candidate as usize];
                        tries += 1;
                    }
                    insert(input, at, end, &mut head, &mut prev, start);
                    match found {
                        Some((length, offset)) => {
                            push(&mut raw, (literal_start, at), length, offset);
                            for step in 1..length {
                                insert(input, at + step, end, &mut head, &mut prev, start);
                            }
                            at += length;
                            literal_start = at;
                        }
                        None => at += 1,
                    }
                }
                if literal_start < end {
                    push(&mut raw, (literal_start, end), 0, 0);
                }
                start = end;
            }
            (raw.runs, raw.lengths, raw.offsets)
        }

        let mut urls = Vec::new();
        for n in 0..9000 {
            urls.extend_from_slice(
                format!("http://example.com/a/b/{}/{n}?q={}\n", n % 37, n * 7).as_bytes(),
            );
        }
        let mut noise = Vec::new();
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        for _ in 0..300_000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            noise.push(b"abcab"[(state % 5) as usize]);
        }
        let inputs: [&[u8]; 6] =
            [b"", b"abcabcabcabcabcabcabcx", &[b'z'; 5000], &urls, &noise, &urls[..SEGMENT + 17]];
        for input in inputs {
            let tokens = tokens_of(input);
            let (runs, lengths, offsets) = by_bytes(input);
            let literals: Vec<&[u8]> = runs.iter().map(|(from, to)| &input[*from..*to]).collect();
            assert_eq!(tokens.literals, literals);
            assert_eq!(tokens.lengths, lengths);
            assert_eq!(tokens.offsets, offsets);
        }
    }

    #[test]
    fn urls_compress() {
        let mut input = Vec::new();
        for n in 0..4000 {
            input.extend_from_slice(format!("http://example.com/page/{n}?ref=search\n").as_bytes());
        }
        let tokens = tokens_of(&input);
        let literal_bytes: usize = tokens.literals.iter().map(|run| run.len()).sum();
        assert!(literal_bytes * 4 < input.len(), "{literal_bytes} literal of {}", input.len());
        round_trip(&input);
    }

    #[test]
    fn a_copy_that_reaches_too_far_is_refused() {
        let literals = vec![b"ab".to_vec()];
        assert!(rebuild(&literals, &[4], &[99], 6).is_err());
    }

    #[test]
    fn streams_of_different_lengths_are_refused() {
        let literals = vec![b"ab".to_vec()];
        assert!(rebuild(&literals, &[0, 0], &[0, 0], 2).is_err());
    }
}
