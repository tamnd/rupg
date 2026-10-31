//! What each codec cost the process and how often it was kept, for `rudb_codec_metrics()`.
//!
//! W1 asks the page builder for no codec that uses over 5% of encode time while being kept in
//! under 1% of offers. The number that started that rule, four codecs at 62.9% of encode time kept
//! 7 times in 1,195 offers, came from `cargo xtask encode`, which runs the exhaustive chooser over a
//! fixture. The writer does not run that chooser. It settles a shape on a sample and replays it, so
//! what the writer offers is much less than the fixture says, and the rule can only be checked by
//! counting in the writer.
//!
//! # What is counted
//!
//! The top level of every cascade and nothing under it. An offer is a candidate the chooser asked
//! for, kept is the one that came out smallest, and the time is the whole of that candidate's
//! encode with everything it cascaded into. So the times of one family add up to its encode time
//! and the share a codec has of it is the share of the writer's encode time its offers cost. A
//! dictionary's codes going through the integer cascade are inside the dictionary's time rather
//! than counted again as integers.
//!
//! # What it costs
//!
//! Two clock readings and three relaxed atomic adds per candidate per chunk, and a chunk is
//! thousands of values. The counters are process wide and never reset, so a reading is the
//! difference between two queries, or a fresh process with one load in it.

// Lifted from rudb crates/rudb-encoding/src/tally.rs at cd9f9676 (2026-10-05).
// Changes: the error type is rupg_common::Error. The time comes from a clock that the process sets with set_clock, not from Instant::now.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::{integer, string};

/// The counters of one codec.
struct Counters {
    offers: AtomicU64,
    kept: AtomicU64,
    nanos: AtomicU64,
}

impl Counters {
    const fn zero() -> Self {
        Self { offers: AtomicU64::new(0), kept: AtomicU64::new(0), nanos: AtomicU64::new(0) }
    }
}

/// One slot per codec in tag order, and one after them for choosing.
static INTEGERS: [Counters; 8] = [const { Counters::zero() }; 8];
static STRINGS: [Counters; 7] = [const { Counters::zero() }; 7];

/// The name of the row that holds the time spent choosing rather than encoding.
///
/// That is the questions a family asks of a chunk before it offers anything, like the one pass in
/// the integer cascade that finds its smallest value, its largest and its runs. It is encode time
/// no codec spent, so it gets a row of its own rather than going missing from the total.
pub const CHOOSING: &str = "(choosing)";

/// What one codec has cost the process so far.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Codec {
    /// `integer` or `string`.
    pub family: &'static str,
    /// The name a report gives the codec, or [`CHOOSING`].
    pub name: &'static str,
    /// Top level candidates the chooser asked this codec to encode. Zero for [`CHOOSING`].
    pub offers: u64,
    /// Of those, how many came out smallest and were written. Zero for [`CHOOSING`].
    pub kept: u64,
    /// The time those offers took, with everything they cascaded into.
    pub nanos: u64,
}

/// Every codec of both families, integers first, whether it was offered or not.
#[must_use]
pub fn codecs() -> Vec<Codec> {
    let integers = integer::Kind::ALL.iter().map(|kind| kind.name()).chain([CHOOSING]);
    let strings = string::Kind::ALL.iter().map(|kind| kind.name()).chain([CHOOSING]);
    let integers = integers.zip(&INTEGERS).map(|(name, counters)| ("integer", name, counters));
    let strings = strings.zip(&STRINGS).map(|(name, counters)| ("string", name, counters));
    integers
        .chain(strings)
        .map(|(family, name, counters)| Codec {
            family,
            name,
            offers: counters.offers.load(Ordering::Relaxed),
            kept: counters.kept.load(Ordering::Relaxed),
            nanos: counters.nanos.load(Ordering::Relaxed),
        })
        .collect()
}

/// Which family a chunk is being counted into.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Family {
    Integer,
    String,
}

impl Family {
    fn slots(self) -> &'static [Counters] {
        match self {
            Self::Integer => &INTEGERS,
            Self::String => &STRINGS,
        }
    }
}

/// The clock of the counters: a monotonic reading in nanoseconds. The crate may not read the clock itself (spec/22 section 22.1), so the process gives it with [`set_clock`]. With no clock the readings are 0 and the counters count offers without time.
static CLOCK: OnceLock<fn() -> u64> = OnceLock::new();

/// Sets the clock of the counters, once for the process. A later call does nothing.
pub fn set_clock(clock: fn() -> u64) {
    let _ = CLOCK.set(clock);
}

/// A reading of the clock, in nanoseconds.
pub(crate) fn now() -> u64 {
    CLOCK.get().map_or(0, |clock| clock())
}

/// The time between two readings, in nanoseconds.
fn since(started: u64) -> u64 {
    now().saturating_sub(started)
}

/// Charges the time since `started` to choosing, for a chunk that is about to offer candidates.
pub(crate) fn chose(family: Family, started: u64) {
    if let Some(counters) = family.slots().last() {
        counters.nanos.fetch_add(since(started), Ordering::Relaxed);
    }
}

/// Runs `encode` as one offer of the codec with this tag and charges it the time.
pub(crate) fn offer<T>(family: Family, tag: u8, encode: impl FnOnce() -> T) -> T {
    let started = now();
    let out = encode();
    if let Some(counters) = family.slots().get(usize::from(tag)) {
        counters.offers.fetch_add(1, Ordering::Relaxed);
        counters.nanos.fetch_add(since(started), Ordering::Relaxed);
    }
    out
}

/// Counts the codec with this tag as the one the chunk was written in.
pub(crate) fn kept(family: Family, tag: u8) {
    if let Some(counters) = family.slots().get(usize::from(tag)) {
        counters.kept.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn of(family: &str, name: &str) -> Codec {
        codecs().into_iter().find(|codec| codec.family == family && codec.name == name).unwrap()
    }

    #[test]
    fn a_top_level_chunk_counts_its_offers_and_the_one_it_kept() {
        let kept = |family: &str| -> u64 {
            codecs().iter().filter(|codec| codec.family == family).map(|codec| codec.kept).sum()
        };
        let before = (of("integer", "FOR+BITPACK"), of("integer", "RLE"), of("string", "PLAIN"));
        let kept_before = (kept("integer"), kept("string"));
        let runs: Vec<i64> = (0..4096).map(|at| at / 512).collect();
        integer::encode(&runs).unwrap();
        string::encode(&[b"a".as_slice(), b"b", b"c"]).unwrap();
        let after = (of("integer", "FOR+BITPACK"), of("integer", "RLE"), of("string", "PLAIN"));
        // Other tests encode on other threads, so this can only say the counts went up.
        assert!(after.0.offers > before.0.offers);
        assert!(after.1.offers > before.1.offers);
        assert!(after.2.offers > before.2.offers);
        assert!(kept("integer") > kept_before.0);
        assert!(kept("string") > kept_before.1);
    }

    #[test]
    fn every_codec_of_both_families_has_a_row() {
        let rows = codecs();
        assert_eq!(rows.len(), integer::Kind::ALL.len() + string::Kind::ALL.len() + 2);
        assert!(rows.iter().any(|codec| codec.family == "string" && codec.name == "LZ"));
    }
}
