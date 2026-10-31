//! Counting distinct values, and deciding whether two columns are related, without holding either
//! column in memory.
//!
//! Every decision in `spec/06-compression.md` section 6.4 and 6.5 starts with a question about a
//! column that is too big to answer exactly. Is this column worth a dictionary, which is a distinct
//! count. Do these two columns come from the same universe, which is an overlap between two value
//! sets. Is this column determined by that one, which is whether the pairs have as many distinct
//! values as the left side alone. Section 6.4 also says the pair space has to be pruned, because
//! 105 columns is 5,460 pairs and testing all of them exactly is not something a load can do.
//!
//! A bottom-k sketch answers all three from one pass per column and a fixed amount of memory.
//!
//! ## What it is
//!
//! Hash every value and keep the k smallest distinct hashes. That set is a uniform random sample of
//! the column's distinct values, chosen by a rule that does not depend on the order they arrived
//! in, so two sketches built on different machines from the same values are identical.
//!
//! The distinct count comes out of where the k smallest hashes end. If the hashes are uniform over
//! the 64 bit range, then after seeing `d` distinct values the kth smallest sits at about `k / d`
//! of the way through the range, so `d` is about `k` divided by that fraction. The standard
//! correction uses `k - 1` rather than `k`, which is what makes the estimate unbiased rather than
//! merely close. Relative error is about one over the square root of k, so the default k of 4096
//! is a bit under 2 percent, and a sketch that never filled up is not an estimate at all because
//! then it holds every distinct hash there was.
//!
//! The overlap between two columns comes from merging the two sketches and asking how many of the
//! k smallest hashes of the union are in both. That is the Jaccard similarity, and the reason it
//! works on sketches is that any hash small enough to be in the union's bottom k is small enough
//! that if it were in a column at all it would be in that column's own bottom k. So a lookup in the
//! sketch is a lookup in the column.
//!
//! ## Why not HyperLogLog
//!
//! Section 6.5 says HyperLogLog for the distinct count and that is the right structure if counting
//! is all you want, because it answers in a kilobyte where this wants tens. It cannot do the other
//! two questions. A HyperLogLog register holds a leading zero count and not a value, so two
//! HyperLogLogs can be merged into a count of the union but they cannot tell you which values the
//! union kept, and the intersection they give by inclusion and exclusion is the difference of three
//! noisy numbers, which for two columns that barely overlap is noise. The sketch here keeps actual
//! hashes, so an intersection is a set intersection and the error on it is the error on the sample
//! rather than the error on the difference. 128 KB per column at the default k, for 105 columns, is
//! 13 MB for a whole table, and the pair pruning it buys is worth more than the 13 MB.
//!
//! ## The hash
//!
//! Values are hashed with a multiply and fold over 8 byte words. It has to be uniform, because
//! every estimate here assumes it is, and the tests measure that rather than asserting it.
//!
//! It is now a persisted hash. A sketch is a section of a rudb file, so replacing the function
//! changes [`HASH_IDENTITY`], which every stored sketch carries and every reader checks, and the
//! sketches written by the old one are declined and rebuilt rather than merged into the new ones.
//! That is the ordinary stale-section path and it costs a rebuild. Merging across two hashes would
//! cost an answer, which is why the identity is in the bytes rather than in a comment.

// Lifted from rudb crates/rudb-encoding/src/sketch.rs at cd9f9676 (2026-10-05).
// Changes: the error type is rupg_common::Error. as_chunks in place of chunks_exact(8), for clippy.

use std::sync::atomic::{AtomicU64, Ordering};

use rupg_common::{Error, Result};

/// The default number of hashes to keep, which puts the relative error a bit under 2 percent.
pub const DEFAULT_K: usize = 4096;

/// A bottom-k sketch of the distinct values of a column.
///
/// The sketch is a function of the set of values and not of the order they were added in. Two
/// sketches built from the same values on different machines compare equal and estimate the same.
///
/// # How it is held
///
/// The hashes live in an open addressed table rather than in a sorted run, because the add is on
/// the load path of every column of every table and a sorted run makes it cost a binary search a
/// row. Measured on server2 over fifty million values, against about 2 nanoseconds a value to hash
/// it in the first place:
///
/// | distinct values | sorted run | this table |
/// | --- | --- | --- |
/// | 10 | 9.0 ns | 6.7 ns |
/// | 1,000 | 22.7 ns | 6.8 ns |
/// | 100,000 | 5.6 ns | 6.1 ns |
/// | 50,000,000 | 2.6 ns | 3.7 ns |
///
/// The run was worst in the middle, where the search is long enough to matter and every branch in
/// it is a coin flip. At the two ends it was already cheap: a short run fits in a cache line, and a
/// column with far more distinct values than k turns nearly every row away on the threshold before
/// the search happens. The table trades a little of those two ends for the middle, which is where
/// most columns of most tables are.
///
/// The table holds more than k. Keeping exactly the k smallest at every moment would mean finding
/// and dropping the largest every time a smaller one arrives, and that is a pass over the table for
/// each of them. So it fills to half its slots, then keeps the k smallest of those in one pass and
/// sets the threshold to the largest it kept. Between two of those passes it holds a superset of
/// the bottom k and turns away anything at or above the threshold, which is all but a vanishing
/// fraction of a large column. The pass happens about `ln(d / k)` times for a column of `d`
/// distinct values, which for a hundred million is around ten.
///
/// That costs memory: 16,384 slots of 8 bytes is 128 KB a column at the default k, and a hundred
/// and five of them is 13 MB. A sorted run of k would have been 32 KB, and the four times is bought
/// with the load time, which is the scarcer of the two here.
#[derive(Debug)]
pub struct Sketch {
    k: usize,
    /// Open addressed, a power of two long, [`EMPTY`] in a free slot. Empty until the first add, so
    /// a sketch nobody used costs nothing.
    slots: Vec<u64>,
    /// How many slots are taken, which is at least k and at most half the slots once `full`.
    held: usize,
    /// Nothing at or above this can be in the bottom k, so it is turned away without a probe.
    threshold: u64,
    /// Whether k distinct hashes have been seen, which is the moment the count stops being exact.
    full: bool,
    /// The bits of the last estimate [`Sketch::distinct`] worked out, or [`UNKNOWN`] once a hash has
    /// been kept since. A table is asked for the estimate of every column on every query it plans,
    /// and working one out walks the whole table of slots, so it is worked out once for each time
    /// the hashes change.
    estimate: AtomicU64,
}

/// The estimate a sketch has not worked out since its hashes last changed. These are the bits of a
/// NaN, which no estimate is.
const UNKNOWN: u64 = u64::MAX;

impl Clone for Sketch {
    fn clone(&self) -> Self {
        Self {
            k: self.k,
            slots: self.slots.clone(),
            held: self.held,
            threshold: self.threshold,
            full: self.full,
            estimate: AtomicU64::new(self.estimate.load(Ordering::Relaxed)),
        }
    }
}

/// The slot value that means nothing is here.
///
/// A hash of exactly this is never stored, because the threshold starts here and nothing at or
/// above the threshold is kept. So one value in 2^64 goes uncounted, which is smaller than the
/// sketch's own error by a margin nothing can measure.
const EMPTY: u64 = u64::MAX;

/// The largest k a sketch will take.
///
/// Sixteen million hashes is a 512 MB table and an error of two hundredths of a percent, which is
/// past the point where anything reading a sketch can tell the difference. The bound is here so
/// that [`capacity`] cannot overflow, not because anyone was going to ask for more.
const MAX_K: usize = 1 << 24;

/// How many slots a sketch of `k` hashes gets.
///
/// Four times k rounded up to a power of two, so the table compacts at half full and a probe that
/// misses walks two or three slots rather than twenty. [`Sketch::new`] holds `k` to [`MAX_K`], so
/// neither the multiply nor the rounding can run off the end.
fn capacity(k: usize) -> usize {
    (k * 4).next_power_of_two()
}

/// How many slots a sketch starts with, so that one over a handful of values, which is every
/// sketch a single-row insert makes, is not a 128 KiB table. It doubles from here up to
/// [`capacity`] as it fills.
const FIRST_SLOTS: usize = 16;

/// How many slots a sketch of `k` hashes holding `held` of them gets.
fn slots_for(k: usize, held: usize) -> usize {
    (held * 4).next_power_of_two().max(FIRST_SLOTS).min(capacity(k))
}

/// The most hashes [`Sketch::absorb`] adds one at a time rather than through [`Sketch::union`].
const FEW: usize = 256;

impl PartialEq for Sketch {
    /// Two sketches are equal when they would answer the same, which is the same k and the same
    /// bottom k hashes. The slots they happen to sit in are not part of that: a table filled in a
    /// different order holds the same set in different places.
    fn eq(&self, other: &Self) -> bool {
        self.k == other.k && self.bottom() == other.bottom()
    }
}

impl Eq for Sketch {}

impl Sketch {
    /// An empty sketch that will keep the `k` smallest hashes.
    ///
    /// # Errors
    ///
    /// If `k` is zero, which would make every estimate a division by nothing, or above `MAX_K`,
    /// which is not a sketch anybody meant to ask for.
    pub fn new(k: usize) -> Result<Self> {
        if k == 0 {
            return Err(Error::internal("a sketch that keeps no hashes estimates nothing"));
        }
        if k > MAX_K {
            return Err(Error::internal(format!(
                "a sketch of {k} hashes is past the {MAX_K} a sketch will keep"
            )));
        }
        Ok(Self {
            k,
            slots: Vec::new(),
            held: 0,
            threshold: EMPTY,
            full: false,
            estimate: AtomicU64::new(UNKNOWN),
        })
    }

    /// A sketch over a column, at the default k.
    #[must_use]
    pub fn of(values: &[&[u8]]) -> Self {
        let mut sketch = Self {
            k: DEFAULT_K,
            slots: Vec::new(),
            held: 0,
            threshold: EMPTY,
            full: false,
            estimate: AtomicU64::new(UNKNOWN),
        };
        for value in values {
            sketch.add(value);
        }
        sketch
    }

    /// Adds a value.
    pub fn add(&mut self, value: &[u8]) {
        self.add_hash(hash64(value));
    }

    /// A hash no sketch this one is unioned with needs from anything else, once this one holds its k.
    ///
    /// The union keeps the k smallest hashes of everything in it, and this sketch alone has k at or
    /// below this, so a hash above it can never be one the union keeps. `None` while the sketch is
    /// still exact, when it keeps everything.
    #[must_use]
    pub fn ceiling(&self) -> Option<u64> {
        self.full.then_some(self.threshold)
    }

    /// Keeps nothing at or above `ceiling` from here on.
    ///
    /// For a sketch of part of a column whose other parts will be unioned with it, with the ceiling
    /// one of those parts reported through [`Self::ceiling`]. Every hash turned away is one the union
    /// would drop, so the union comes out the same, but this sketch on its own no longer describes its
    /// part and must not be asked for an estimate.
    pub fn cap_at(&mut self, ceiling: u64) {
        self.threshold = self.threshold.min(ceiling);
        *self.estimate.get_mut() = UNKNOWN;
    }

    /// Adds a value that has already been hashed, for a caller that is hashing anyway.
    ///
    /// The first line is the whole of it for all but a few thousand values of a large column, and
    /// the rest is off the hot path on purpose.
    ///
    /// The check is inlined into the caller's row loop and the insert is not. A call a row just to
    /// turn the row away was 2% of a `lineitem` load's samples.
    #[inline]
    pub fn add_hash(&mut self, hash: u64) {
        if hash < self.threshold {
            self.insert(hash);
        }
    }

    /// The part of [`Self::add_hash`] a hash below the threshold gets to.
    #[inline(never)]
    fn insert(&mut self, hash: u64) {
        *self.estimate.get_mut() = UNKNOWN;
        if self.slots.is_empty() {
            self.slots = vec![EMPTY; slots_for(self.k, 0)];
        }
        let mask = self.slots.len() - 1;
        let mut at = (hash as usize) & mask;
        loop {
            let slot = self.slots[at];
            if slot == hash {
                return;
            }
            if slot == EMPTY {
                self.slots[at] = hash;
                self.held += 1;
                break;
            }
            at = (at + 1) & mask;
        }
        if !self.full && self.held >= self.k {
            // The count stops being exact here and the threshold starts doing its work. Everything
            // held is still kept, because k of it is the bottom k.
            self.full = true;
            self.threshold =
                self.slots.iter().filter(|slot| **slot != EMPTY).copied().max().unwrap_or(EMPTY);
        } else if self.held >= self.slots.len() / 2 {
            if self.slots.len() < capacity(self.k) {
                self.grow();
            } else {
                self.compact();
            }
        }
    }

    /// Doubles the table, for a sketch that has not reached its full size yet.
    fn grow(&mut self) {
        let doubled = vec![EMPTY; self.slots.len() * 2];
        let old = std::mem::replace(&mut self.slots, doubled);
        let mask = self.slots.len() - 1;
        for hash in old.into_iter().filter(|slot| *slot != EMPTY) {
            let mut at = (hash as usize) & mask;
            while self.slots[at] != EMPTY {
                at = (at + 1) & mask;
            }
            self.slots[at] = hash;
        }
    }

    /// Throws away everything above the k smallest and tightens the threshold onto what is left.
    ///
    /// One pass to collect, one selection, one pass to refill. Amortised over the k slots that were
    /// filled since the last one, which is why the table is bigger than k in the first place.
    fn compact(&mut self) {
        let mut kept: Vec<u64> = self.slots.iter().copied().filter(|slot| *slot != EMPTY).collect();
        if kept.len() <= self.k {
            return;
        }
        kept.select_nth_unstable(self.k - 1);
        kept.truncate(self.k);
        let threshold = kept.iter().copied().max().unwrap_or(EMPTY);
        let mask = self.slots.len() - 1;
        self.slots.fill(EMPTY);
        for hash in &kept {
            let mut at = (*hash as usize) & mask;
            while self.slots[at] != EMPTY {
                at = (at + 1) & mask;
            }
            self.slots[at] = *hash;
        }
        self.held = kept.len();
        self.threshold = threshold;
    }

    /// The bottom k hashes, sorted, which is what every reader below is really asking for.
    ///
    /// Off the add path, so it is allowed to walk the table and sort. Between two compactions the
    /// table holds a superset of the bottom k, and this is where that superset is cut back down.
    fn bottom(&self) -> Vec<u64> {
        let mut kept: Vec<u64> = self.slots.iter().copied().filter(|slot| *slot != EMPTY).collect();
        kept.sort_unstable();
        kept.truncate(self.k);
        kept
    }

    /// How many hashes the sketch is holding.
    #[must_use]
    pub fn len(&self) -> usize {
        self.held.min(self.k)
    }

    /// How many hashes it keeps, which is the `k` it was built with.
    #[must_use]
    pub fn k(&self) -> usize {
        self.k
    }

    /// The bottom k hashes, smallest first.
    ///
    /// This is the whole of what a sketch knows. Everything else on it, the distinct estimate, the
    /// exactness flag, the Jaccard, is computed from this list and the `k` beside it, which is why
    /// writing one down is writing this list down and nothing more.
    #[must_use]
    pub fn hashes(&self) -> Vec<u64> {
        self.bottom()
    }

    /// The sketch that holds exactly these hashes.
    ///
    /// The inverse of [`Sketch::hashes`], and the two of them are what a stored sketch is read back
    /// through. `full`, and so [`Sketch::is_exact`], is recovered from the count rather than stored
    /// beside it: a sketch holding fewer than `k` hashes saw fewer than `k` distinct values, which
    /// is the same thing the add path means by not being full. Storing the flag as well would be
    /// storing something derivable, and the two could then disagree.
    ///
    /// # Errors
    ///
    /// If `k` is not one a sketch can be built with, or if more than `k` hashes are handed over,
    /// which is not a bottom-k set of that `k` and would make every estimate off it wrong.
    pub fn from_hashes(k: usize, hashes: &[u64]) -> Result<Self> {
        if hashes.len() > k {
            return Err(Error::internal(format!(
                "{} hashes are more than the {k} a sketch of that size keeps",
                hashes.len()
            )));
        }
        let mut sketch = Self::new(k)?;
        for hash in hashes {
            sketch.add_hash(*hash);
        }
        // The add path sets `full` the moment the kth distinct hash arrives, and a stored sketch of
        // exactly k hashes is one that was full when it was written. Anything short of k was not,
        // and the adds above have already left it that way.
        Ok(sketch)
    }

    /// Whether nothing has been added.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.held == 0
    }

    /// The same column sketched at a smaller k, which is the bottom k of the hashes this holds.
    ///
    /// Exact, and that is worth the sentence because the opposite direction is not. A bottom-k
    /// sketch holds the k smallest hashes of everything it saw, so the 256 smallest of the 4096
    /// smallest are the 256 smallest, full stop. Nothing is approximated and the result is
    /// indistinguishable from a sketch of 256 built over the same column from the start. Going the
    /// other way, pouring small sketches into a larger one, gives a sketch that never fills and so
    /// claims to be exact about a column it only saw a slice of, which is the failure
    /// `rudb_stats::Sketches` is arranged around.
    ///
    /// This is how a column that is sketched once at the default k gets written down per stripe at
    /// the smaller k a stripe sketch keeps, without the column being hashed a second time.
    ///
    /// # Errors
    ///
    /// If `k` is not one a sketch can be built with, or if it is larger than this sketch's own,
    /// which is the direction above that does not work.
    pub fn narrowed(&self, k: usize) -> Result<Self> {
        if k > self.k {
            return Err(Error::internal(format!(
                "a sketch of {} hashes cannot be widened to {k}",
                self.k
            )));
        }
        // For the bounds check on k, whose answer is thrown away because `holding` below builds the
        // sketch from a list rather than by adding to this one.
        Self::new(k)?;
        let bottom = self.bottom();
        Ok(Self::holding(k, &bottom[..bottom.len().min(k)]))
    }

    /// Whether the sketch saw fewer than k distinct values, in which case it holds all of them and
    /// every count it gives is exact rather than estimated.
    #[must_use]
    pub fn is_exact(&self) -> bool {
        !self.full
    }

    /// The estimated number of distinct values, which is the exact number when [`Sketch::is_exact`]
    /// holds.
    #[must_use]
    pub fn distinct(&self) -> f64 {
        if self.is_exact() {
            return self.held as f64;
        }
        let known = self.estimate.load(Ordering::Relaxed);
        if known != UNKNOWN {
            return f64::from_bits(known);
        }
        let estimate = self.estimate_from_slots();
        self.estimate.store(estimate.to_bits(), Ordering::Relaxed);
        estimate
    }

    /// [`Self::distinct`] for a full sketch, worked out from the slots. Only the largest of the bottom
    /// k is read, so it is selected rather than sorted for.
    fn estimate_from_slots(&self) -> f64 {
        let mut kept: Vec<u64> = self.slots.iter().copied().filter(|slot| *slot != EMPTY).collect();
        let held = kept.len().min(self.k);
        let Some(at) = held.checked_sub(1) else {
            return 0.0;
        };
        let (_, largest, _) = kept.select_nth_unstable(at);
        let largest = *largest as f64 / u64::MAX as f64;
        if largest <= 0.0 {
            return held as f64;
        }
        (self.k as f64 - 1.0) / largest
    }

    /// The union of two sketches, which is the sketch the union of the two columns would have
    /// produced.
    ///
    /// # Errors
    ///
    /// If the two sketches keep a different number of hashes, because then neither one's threshold
    /// applies to the other and no estimate over the pair means anything.
    pub fn union(&self, other: &Self) -> Result<Self> {
        if self.k != other.k {
            return Err(Error::internal(format!(
                "sketches of {} and {} hashes cannot be combined",
                self.k, other.k
            )));
        }
        let (left, right) = (self.bottom(), other.bottom());
        let mut merged: Vec<u64> = Vec::with_capacity(self.k);
        let (mut a, mut b) = (0, 0);
        while merged.len() < self.k && (a < left.len() || b < right.len()) {
            let next = match (left.get(a), right.get(b)) {
                (Some(one), Some(two)) if one <= two => {
                    a += 1;
                    *one
                }
                (Some(_), Some(two)) => {
                    b += 1;
                    *two
                }
                (Some(one), None) => {
                    a += 1;
                    *one
                }
                (None, Some(two)) => {
                    b += 1;
                    *two
                }
                (None, None) => break,
            };
            if merged.last() != Some(&next) {
                merged.push(next);
            }
        }
        Ok(Self::holding(self.k, &merged))
    }

    /// Makes this the union of itself and `other`, which is [`Sketch::union`] without building a
    /// third sketch when `other` holds only a few hashes. Those are added one at a time, which
    /// leaves the same bottom k: every hash `other` holds is a hash of one of its values, and the
    /// ones of its bottom k are among them.
    ///
    /// # Errors
    ///
    /// As [`Sketch::union`].
    pub fn absorb(&mut self, other: &Self) -> Result<()> {
        if self.k != other.k || other.held > FEW {
            *self = self.union(other)?;
            return Ok(());
        }
        for &hash in &other.slots {
            if hash != EMPTY {
                self.add_hash(hash);
            }
        }
        Ok(())
    }

    /// A sketch holding exactly these hashes, which have to be sorted and deduplicated.
    ///
    /// For [`Sketch::union`], which works out its answer as a list and then needs it back as a
    /// sketch. A list of k is a sketch that has filled, and anything shorter has not.
    fn holding(k: usize, hashes: &[u64]) -> Self {
        let mut sketch = Self {
            k,
            slots: Vec::new(),
            held: 0,
            threshold: EMPTY,
            full: false,
            estimate: AtomicU64::new(UNKNOWN),
        };
        if hashes.is_empty() {
            return sketch;
        }
        sketch.slots = vec![EMPTY; slots_for(k, hashes.len())];
        let mask = sketch.slots.len() - 1;
        for hash in hashes {
            let mut at = (*hash as usize) & mask;
            while sketch.slots[at] != EMPTY {
                at = (at + 1) & mask;
            }
            sketch.slots[at] = *hash;
        }
        sketch.held = hashes.len();
        if sketch.held >= k {
            sketch.full = true;
            sketch.threshold = hashes[hashes.len() - 1];
        }
        sketch
    }

    /// The estimated Jaccard similarity, which is the size of the intersection of the two value
    /// sets over the size of their union.
    ///
    /// Section 6.4 wants this to decide whether two columns are drawn from the same universe and
    /// should share a dictionary. It is not a decision on its own, because two columns can overlap
    /// heavily and still be better off apart if one of them is tiny, but it is what prunes 5,460
    /// pairs down to the handful worth measuring properly.
    ///
    /// # Errors
    ///
    /// As [`Sketch::union`].
    pub fn jaccard(&self, other: &Self) -> Result<f64> {
        let union = self.union(other)?.bottom();
        if union.is_empty() {
            return Ok(0.0);
        }
        let (left, right) = (self.bottom(), other.bottom());
        let both =
            union.iter().filter(|hash| holds(&left, **hash) && holds(&right, **hash)).count();
        Ok(both as f64 / union.len() as f64)
    }
}

/// Whether a hash is one of the bottom k a sketch kept.
///
/// Takes the list and not the sketch because the table a sketch adds into holds a superset of its
/// bottom k between two compactions, and a hash sitting in that slack is one the sketch would not
/// have kept if it had been asked for an answer. [`Sketch::bottom`] is where the slack comes off,
/// so the caller does that once and probes the result.
///
/// Only meaningful for a hash small enough to have been kept if it were there at all, which is
/// what [`Sketch::jaccard`] guarantees by taking its candidates from the union.
fn holds(bottom: &[u64], hash: u64) -> bool {
    bottom.binary_search(&hash).is_ok()
}

/// How close a column is to being determined by another one, from a sketch of the left column and
/// a sketch of the two of them paired.
///
/// A functional dependency from A to B means every A value goes with exactly one B value, so the
/// pairs have exactly as many distinct values as A does. On ClickBench `hits` this is `URLHash`
/// against `URL` and `RefererHash` against `Referer`, which section 6.6 says is 1.6 GB of `BIGINT`
/// carrying nothing that is not already in two string columns.
///
/// The result is 1.0 for a dependency that holds and drops towards the ratio of the two counts as
/// it stops holding. It is an estimate over two estimates, so a value near 1.0 is a candidate to be
/// verified exactly and never a conclusion. Section 6.6 is explicit that a rule is applied only
/// after a full verification pass, and this is what decides which pairs are worth that pass.
///
/// # Errors
///
/// As [`Sketch::union`], and if the pairs somehow have fewer distinct values than the left column,
/// which cannot happen and is a bug in the caller's pairing if it does.
pub fn dependence(left: &Sketch, pairs: &Sketch) -> Result<f64> {
    if left.k != pairs.k {
        return Err(Error::internal("a column and its pairs need sketches of the same size"));
    }
    let alone = left.distinct();
    let together = pairs.distinct();
    if alone <= 0.0 {
        return Ok(1.0);
    }
    Ok((alone / together.max(alone)).min(1.0))
}

/// The hash of two values as a pair, for [`dependence`].
///
/// The left hash is mixed before the two are combined, so that the pair of `ab` and `c` does not
/// hash the same as the pair of `a` and `bc`.
#[must_use]
pub fn pair_hash(left: &[u8], right: &[u8]) -> u64 {
    pair_of(hash64(left), hash64(right))
}

/// The same pair hash for two values whose hashes are already known.
///
/// Testing every pair of a 105 column table is 5,460 pairs, and hashing the two values again for
/// each of them would hash every value of every column 104 times over. Hashing each column once a
/// row and combining the results here is the same answer for a hundredth of the work, and it is the
/// only way a pass over `hits` that tests all the pairs finishes in an afternoon.
#[must_use]
pub fn pair_of(left: u64, right: u64) -> u64 {
    mix(left ^ SEEDS[3], right.wrapping_add(SEEDS[2]))
}

/// The constants are odd 64 bit values with about half their bits set, which is what a multiply
/// based mixer needs to move low bits into high ones.
const SEEDS: [u64; 4] =
    [0xa076_1d64_78bd_642f, 0xe703_7ed1_a0b4_28db, 0x8ebc_6af0_9c88_c6e3, 0x5899_65cc_7537_4cc3];

/// A 64 bit multiply of two values, folded to 64 bits by xoring the halves.
///
/// This is the whole strength of the hash. A 64 by 64 multiply moves every input bit into the high
/// half of the product, and xoring the halves together brings them back down, so one of these turns
/// a one bit change anywhere into a change in about half the output bits.
fn mix(left: u64, right: u64) -> u64 {
    let wide = u128::from(left).wrapping_mul(u128::from(right));
    (wide as u64) ^ ((wide >> 64) as u64)
}

/// The string [`HASH_IDENTITY`] is the hash of.
///
/// Public so that the identity is checkable rather than asserted: anyone holding this crate can
/// compute `hash64(HASH_PROBE)` and get the number a file carries.
pub const HASH_PROBE: &[u8] = b"rudb sketch hash 1";

/// Which hash a stored sketch was built with.
///
/// `spec/stats/03-the-file-format.md` section 3.4 asks for this to be written into every persisted
/// sketch, and the reason is that a sketch built by one hash and merged with a sketch built by
/// another is silently wrong. Not an error, not a worse estimate: a union of two bottom-k sets
/// drawn from two different orderings of the same values, which answers confidently and wrongly.
/// So a reader compares this against what the file says and declines the sketch when they differ,
/// which is the ordinary missing-statistic case rather than a failure.
///
/// The value is [`hash64`] of [`HASH_PROBE`] rather than a number somebody picked, and a test holds it
/// there. That is what makes it an identity and not a comment: changing the hash without changing
/// this is the mistake it exists to catch, and a constant that had to be remembered would not
/// catch it.
pub const HASH_IDENTITY: u64 = 0x565d_3caf_6ae8_c2b5;

/// The hash used by every sketch here.
///
/// It has to be uniform, because every estimate in this module assumes the hashes are spread evenly
/// over the range.
///
/// It used to say that nothing on disk depended on this so it could be replaced freely. That stopped
/// being true when sketches became a section of the file. It can still be replaced, but the
/// replacement changes [`HASH_IDENTITY`], and every sketch written by the old one stops being
/// readable and gets rebuilt at the next checkpoint, which is the ordinary path for a stale section
/// and costs a rebuild rather than an answer.
#[must_use]
pub fn hash64(value: &[u8]) -> u64 {
    let mut state = SEEDS[0] ^ mix(value.len() as u64, SEEDS[1]);
    let (chunks, rest) = value.as_chunks::<8>();
    for chunk in chunks {
        state = mix(state ^ u64::from_le_bytes(*chunk), SEEDS[2]);
    }
    if !rest.is_empty() {
        state = mix(state ^ tail(value, rest), SEEDS[3]);
    }
    mix(state, SEEDS[1])
}

/// The last one to seven bytes of `value`, which are `rest`, as the little endian word they make
/// padded with zeroes above.
///
/// Copying them into a zeroed word is the plain way, and it was a call to `memcpy` a string and a
/// load that waited on the copy's stores, about a fifth of what counting the distinct values of a
/// `lineitem` load spends. So the word is read with loads of a fixed width instead. A value of eight
/// bytes or more has a whole word ending where it ends, and shifting that right leaves the tail. A
/// shorter one is two loads that may overlap, and bytes that overlap are the same byte in the same
/// place, so or-ing them together changes nothing.
fn tail(value: &[u8], rest: &[u8]) -> u64 {
    let len = rest.len();
    if let Some(word) = value.len().checked_sub(8).and_then(|at| value.get(at..)) {
        return word_at(word, 0) >> (8 * (8 - len));
    }
    let at = |index: usize| u64::from(rest.get(index).copied().unwrap_or(0));
    if len >= 4 {
        u64::from(half_at(rest, 0)) | (u64::from(half_at(rest, len - 4)) << (8 * (len - 4)))
    } else {
        at(0) | (at(len / 2) << (8 * (len / 2))) | (at(len - 1) << (8 * (len - 1)))
    }
}

/// Eight bytes from `at` as a little endian word, or zero past the end, which no caller reaches.
fn word_at(bytes: &[u8], at: usize) -> u64 {
    bytes.get(at..at + 8).and_then(|word| word.try_into().ok()).map_or(0, u64::from_le_bytes)
}

/// Four bytes from `at` as a little endian word, the same way.
fn half_at(bytes: &[u8], at: usize) -> u32 {
    bytes.get(at..at + 4).and_then(|word| word.try_into().ok()).map_or(0, u32::from_le_bytes)
}

/// The hash of a 16 byte integer, which is what every fixed width number is widened to before it
/// gets here.
///
/// The same value as `hash64(&value.to_le_bytes())` and not an approximation of it, which the test
/// at the bottom of this file holds to. It exists because the loop and the remainder in [`hash64`]
/// are dead weight for a length the caller already knows, and this is called once a row of every
/// integer column of every table on the load path.
#[must_use]
pub fn hash128(value: u128) -> u64 {
    let mut state = SEEDS[0] ^ mix(16, SEEDS[1]);
    state = mix(state ^ (value as u64), SEEDS[2]);
    state = mix(state ^ ((value >> 64) as u64), SEEDS[2]);
    mix(state, SEEDS[1])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_kept_estimate_is_dropped_when_a_hash_arrives() {
        let mut sketch = Sketch::new(64).expect("sketch");
        for value in 0..1_000_u64 {
            sketch.add(&value.to_le_bytes());
        }
        let first = sketch.distinct();
        assert_eq!(first.to_bits(), sketch.clone().distinct().to_bits());
        for value in 1_000..100_000_u64 {
            sketch.add(&value.to_le_bytes());
        }
        let fresh = Sketch::from_hashes(64, &sketch.hashes()).expect("sketch");
        assert!(sketch.distinct() > first * 10.0);
        assert_eq!(sketch.distinct().to_bits(), fresh.distinct().to_bits());
    }

    fn values(count: usize, prefix: &str) -> Vec<Vec<u8>> {
        (0..count).map(|index| format!("{prefix}{index}").into_bytes()).collect()
    }

    fn borrow(values: &[Vec<u8>]) -> Vec<&[u8]> {
        values.iter().map(Vec::as_slice).collect()
    }

    fn within(estimate: f64, actual: f64, tolerance: f64) -> bool {
        (estimate - actual).abs() / actual <= tolerance
    }

    #[test]
    fn a_sketch_that_never_filled_up_is_exact() {
        let column = values(1000, "value-");
        let sketch = Sketch::of(&borrow(&column));
        assert!(sketch.is_exact());
        assert_eq!(sketch.distinct(), 1000.0);
    }

    #[test]
    fn what_the_sketch_kept_is_the_k_smallest_hashes_and_nothing_more() {
        let column = values(100_000, "value-");
        let sketch = Sketch::of(&borrow(&column));
        let mut every: Vec<u64> = column.iter().map(|value| hash64(value)).collect();
        every.sort_unstable();
        every.dedup();
        every.truncate(DEFAULT_K);
        assert_eq!(sketch.bottom(), every);
        assert_eq!(sketch.len(), DEFAULT_K);
    }

    #[test]
    fn a_sketch_holding_its_k_th_value_is_no_longer_exact() {
        let column = values(DEFAULT_K, "value-");
        let mut sketch = Sketch::new(DEFAULT_K).unwrap();
        for value in &column[..DEFAULT_K - 1] {
            sketch.add(value);
        }
        assert!(sketch.is_exact());
        assert_eq!(sketch.distinct(), (DEFAULT_K - 1) as f64);
        sketch.add(&column[DEFAULT_K - 1]);
        assert!(!sketch.is_exact());
        assert_eq!(sketch.len(), DEFAULT_K);
    }

    #[test]
    fn duplicates_do_not_count() {
        let mut sketch = Sketch::new(64).unwrap();
        for _ in 0..1000 {
            sketch.add(b"the same value");
        }
        assert_eq!(sketch.distinct(), 1.0);
    }

    #[test]
    fn the_distinct_count_is_within_two_percent_at_the_default_k() {
        for count in [50_000usize, 250_000, 1_000_000] {
            let mut sketch = Sketch::new(DEFAULT_K).unwrap();
            for index in 0..count {
                sketch.add(format!("http://example.com/page/{index}").as_bytes());
            }
            assert!(!sketch.is_exact());
            let estimate = sketch.distinct();
            assert!(
                within(estimate, count as f64, 0.02),
                "{estimate:.0} against {count} distinct values"
            );
        }
    }

    #[test]
    fn the_sketch_does_not_depend_on_the_order_values_arrived_in() {
        let column = values(100_000, "value-");
        let forwards = Sketch::of(&borrow(&column));
        let mut backwards = Sketch::new(DEFAULT_K).unwrap();
        for value in column.iter().rev() {
            backwards.add(value);
        }
        assert_eq!(forwards, backwards);
    }

    #[test]
    fn two_columns_with_the_same_values_overlap_completely() {
        let column = values(200_000, "http://example.com/");
        let left = Sketch::of(&borrow(&column));
        let right = Sketch::of(&borrow(&column));
        assert_eq!(left.jaccard(&right).unwrap(), 1.0);
    }

    #[test]
    fn two_columns_with_nothing_in_common_do_not_overlap() {
        let left = Sketch::of(&borrow(&values(200_000, "left-")));
        let right = Sketch::of(&borrow(&values(200_000, "right-")));
        assert_eq!(left.jaccard(&right).unwrap(), 0.0);
    }

    #[test]
    fn a_half_overlap_measures_as_a_third() {
        // Two columns of 100,000 values sharing 50,000 of them. The intersection is 50,000 and the
        // union is 150,000, so the Jaccard similarity is a third and not a half, which is the
        // number that catches people out about this measure.
        let shared = values(50_000, "shared-");
        let mut left = shared.clone();
        left.extend(values(50_000, "left-"));
        let mut right = shared;
        right.extend(values(50_000, "right-"));
        let overlap = Sketch::of(&borrow(&left)).jaccard(&Sketch::of(&borrow(&right))).unwrap();
        assert!(within(overlap, 1.0 / 3.0, 0.05), "{overlap:.4}");
    }

    #[test]
    fn the_union_of_two_sketches_counts_the_union_of_the_columns() {
        let left = values(300_000, "left-");
        let right = values(300_000, "right-");
        let union = Sketch::of(&borrow(&left)).union(&Sketch::of(&borrow(&right))).unwrap();
        assert!(within(union.distinct(), 600_000.0, 0.03), "{:.0}", union.distinct());
    }

    #[test]
    fn narrowing_a_sketch_gives_what_sketching_the_column_at_that_k_would_have() {
        // The property the per stripe writer leans on, and it is an equality rather than a
        // tolerance: a bottom-k of a bottom-k is a bottom-k, so the narrowed sketch holds the same
        // hashes as one built over the same column from the start, and both estimate the same
        // number off them.
        let column = values(100_000, "value-");
        let wide = Sketch::of(&borrow(&column));
        let narrow = wide.narrowed(256).unwrap();
        let direct = {
            let mut sketch = Sketch::new(256).unwrap();
            for value in &column {
                sketch.add(value);
            }
            sketch
        };
        assert_eq!(narrow.bottom(), direct.bottom());
        assert_eq!(narrow.k(), 256);
        assert!(!narrow.is_exact(), "a hundred thousand values fill a sketch of 256");
        assert_eq!(narrow.distinct(), direct.distinct());
    }

    #[test]
    fn narrowing_a_sketch_that_never_filled_keeps_it_exact() {
        // A column under the smaller k is held entire either way, so narrowing cannot turn an exact
        // count into an estimate. The other direction is the one that lies, and it is refused.
        let column = values(100, "value-");
        let sketch = Sketch::of(&borrow(&column));
        let narrow = sketch.narrowed(256).unwrap();
        assert!(narrow.is_exact());
        assert_eq!(narrow.distinct(), 100.0);
        assert!(narrow.narrowed(DEFAULT_K).is_err(), "narrowing does not widen");
        assert!(sketch.narrowed(0).is_err());
    }

    #[test]
    fn sketches_of_different_sizes_do_not_combine() {
        let small = Sketch::new(16).unwrap();
        let large = Sketch::new(32).unwrap();
        assert!(small.union(&large).is_err());
        assert!(small.jaccard(&large).is_err());
    }

    #[test]
    fn a_sketch_that_keeps_nothing_is_rejected() {
        assert!(Sketch::new(0).is_err());
    }

    #[test]
    fn a_functional_dependency_shows_up_as_a_dependence_of_one() {
        // The `URL` and `URLHash` case from section 6.6. The hash is determined by the URL, so
        // pairing them adds no distinct values.
        let urls = values(200_000, "http://example.com/page/");
        let mut left = Sketch::new(DEFAULT_K).unwrap();
        let mut pairs = Sketch::new(DEFAULT_K).unwrap();
        for url in &urls {
            let derived = hash64(url).to_le_bytes();
            left.add(url);
            pairs.add_hash(pair_hash(url, &derived));
        }
        let score = dependence(&left, &pairs).unwrap();
        assert!(score > 0.97, "{score:.4}");
    }

    #[test]
    fn two_independent_columns_do_not_look_like_a_dependency() {
        let left = values(1000, "left-");
        let right = values(1000, "right-");
        let mut alone = Sketch::new(DEFAULT_K).unwrap();
        let mut pairs = Sketch::new(DEFAULT_K).unwrap();
        for left_value in &left {
            alone.add(left_value);
            for right_value in &right {
                pairs.add_hash(pair_hash(left_value, right_value));
            }
        }
        let score = dependence(&alone, &pairs).unwrap();
        assert!(score < 0.01, "{score:.4}");
    }

    #[test]
    fn the_pair_hash_does_not_ignore_where_the_boundary_is() {
        assert_ne!(pair_hash(b"ab", b"c"), pair_hash(b"a", b"bc"));
        assert_ne!(pair_hash(b"a", b"b"), pair_hash(b"b", b"a"));
    }

    #[test]
    fn combining_two_hashes_is_the_same_as_hashing_the_pair() {
        // The lab hashes each column once a row and combines, and that has to be the same answer as
        // hashing the two values together, or a dependency measured the fast way is not the
        // dependency the slow way would have found.
        for left in ["", "a", "http://example.com/one"] {
            for right in ["", "b", "http://example.com/two"] {
                assert_eq!(
                    pair_hash(left.as_bytes(), right.as_bytes()),
                    pair_of(hash64(left.as_bytes()), hash64(right.as_bytes()))
                );
            }
        }
    }

    #[test]
    fn the_hash_spreads_one_bit_changes_across_the_output() {
        // Every estimate here assumes the hashes are uniform, so this measures the property rather
        // than asserting it. Flipping one bit of the input has to change about half the output
        // bits, and a hash that failed this would make every count above it wrong in a way that
        // looks like the sketch is broken.
        let mut total = 0u32;
        let mut trials = 0u32;
        for index in 0..2000u32 {
            let value = index.to_le_bytes();
            let base = hash64(&value);
            for bit in 0..32 {
                let mut flipped = value;
                flipped[bit / 8] ^= 1 << (bit % 8);
                total += (base ^ hash64(&flipped)).count_ones();
                trials += 1;
            }
        }
        let average = f64::from(total) / f64::from(trials);
        assert!((average - 32.0).abs() < 1.0, "{average:.3} bits changed on average");
    }

    #[test]
    fn the_hash_does_not_collide_on_values_that_differ_by_one_byte() {
        // The shape of a real column: a million near identical URLs. A hash that collided here
        // would make the distinct count an undercount and the overlap an overcount at the same
        // time.
        let mut hashes: Vec<u64> = (0..200_000u32)
            .map(|index| hash64(format!("http://a/{index:09}").as_bytes()))
            .collect();
        hashes.sort_unstable();
        let before = hashes.len();
        hashes.dedup();
        assert_eq!(hashes.len(), before);
    }

    #[test]
    fn a_long_value_and_its_prefix_hash_differently() {
        assert_ne!(hash64(b""), hash64(b"\0"));
        assert_ne!(hash64(b"abcdefgh"), hash64(b"abcdefgh\0"));
        assert_ne!(hash64(&[0u8; 16]), hash64(&[0u8; 24]));
    }

    #[test]
    fn the_short_way_round_a_sixteen_byte_hash_gives_the_long_way_round_answer() {
        let values = [
            0u128,
            1,
            2,
            255,
            256,
            u64::MAX as u128,
            u64::MAX as u128 + 1,
            u128::MAX,
            (-1i128) as u128,
            (i64::MIN as i128) as u128,
        ];
        for value in values {
            assert_eq!(hash128(value), hash64(&value.to_le_bytes()), "for {value}");
        }
        for step in 0..1000u128 {
            let value = step.wrapping_mul(0x9e37_79b9_7f4a_7c15_1234_5678_9abc_def1);
            assert_eq!(hash128(value), hash64(&value.to_le_bytes()), "for {value}");
        }
    }

    /// The tail read with fixed loads is the zero padded copy it replaced, for every length a tail
    /// can have and every offset a string can start at inside a longer one.
    #[test]
    fn hash64_matches_the_padded_tail() {
        fn plain(value: &[u8]) -> u64 {
            let mut state = SEEDS[0] ^ mix(value.len() as u64, SEEDS[1]);
            let (chunks, rest) = value.as_chunks::<8>();
            for chunk in chunks {
                state = mix(state ^ u64::from_le_bytes(*chunk), SEEDS[2]);
            }
            if !rest.is_empty() {
                let mut last = [0u8; 8];
                last[..rest.len()].copy_from_slice(rest);
                state = mix(state ^ u64::from_le_bytes(last), SEEDS[3]);
            }
            mix(state, SEEDS[1])
        }
        let bytes: Vec<u8> = (0..64_u32).map(|at| (at.wrapping_mul(0x9E) ^ 0xA5) as u8).collect();
        for start in 0..8 {
            for len in 0..40 {
                let value = &bytes[start..start + len];
                assert_eq!(hash64(value), plain(value), "for {len} bytes at {start}");
            }
        }
        assert_eq!(hash64(&[0xFF; 7]), plain(&[0xFF; 7]));
    }

    #[test]
    fn the_stored_identity_is_what_this_hash_actually_answers() {
        // The one test that makes `HASH_IDENTITY` an identity rather than a note. Changing the hash
        // and forgetting the constant is the mistake that merges two incompatible sketches without
        // saying anything, and this is where it stops: the constant has to move with the function,
        // and moving it is what makes every stored sketch declined and rebuilt.
        assert_eq!(hash64(HASH_PROBE), HASH_IDENTITY);
    }

    #[test]
    fn a_sketch_survives_being_taken_apart_and_put_back_together() {
        // What a stored sketch is: the bottom k and the k beside it, and nothing else. If this
        // round trip lost anything then persisting one would lose it too.
        for count in [0usize, 1, 10, DEFAULT_K - 1, DEFAULT_K, DEFAULT_K * 10] {
            let values = values(count, "row");
            let sketch = Sketch::of(&borrow(&values));
            let back = Sketch::from_hashes(sketch.k(), &sketch.hashes()).expect("rebuild");
            assert_eq!(back, sketch, "at {count} values");
            assert_eq!(back.k(), sketch.k());
            assert_eq!(back.len(), sketch.len());
            assert_eq!(back.is_exact(), sketch.is_exact(), "at {count} values");
            assert!((back.distinct() - sketch.distinct()).abs() < 1e-6, "at {count} values");
        }
    }

    #[test]
    fn the_hashes_come_back_smallest_first_and_there_are_never_more_than_k() {
        let values = values(100_000, "row");
        let sketch = Sketch::of(&borrow(&values));
        let hashes = sketch.hashes();
        assert_eq!(hashes.len(), DEFAULT_K, "a column past k holds exactly k");
        assert!(hashes.windows(2).all(|pair| pair[0] < pair[1]), "sorted and distinct");
    }

    #[test]
    fn more_hashes_than_k_is_refused_rather_than_truncated() {
        // Truncating would build a sketch that looks like a bottom-k set of a smaller k and is not
        // one, and every estimate off it would be wrong by the amount that was dropped.
        assert!(Sketch::from_hashes(4, &[1, 2, 3, 4, 5]).is_err());
        assert!(Sketch::from_hashes(0, &[]).is_err(), "a sketch of no hashes estimates nothing");
    }

    #[test]
    fn absorbing_one_row_at_a_time_is_the_union() {
        let column = values(20_000, "row-");
        let mut whole = Sketch::of(&borrow(&column));
        let mut fed = Sketch::new(DEFAULT_K).expect("a sketch");
        for value in &column {
            fed.absorb(&Sketch::of(&[value.as_slice()])).expect("same k");
        }
        assert_eq!(fed, whole, "row by row");
        let mut halves = Sketch::of(&borrow(&column[..10_000]));
        halves.absorb(&Sketch::of(&borrow(&column[10_000..]))).expect("same k");
        assert_eq!(halves, whole, "two large halves go through the union");
        whole.absorb(&Sketch::new(DEFAULT_K).expect("a sketch")).expect("same k");
        assert_eq!(whole.distinct(), halves.distinct(), "an empty one changes nothing");
        let small = Sketch::of(&borrow(&column[..10]));
        assert_eq!(small.slots.len(), FIRST_SLOTS * 2, "ten values in a small table");
        assert!(Sketch::new(8).expect("a sketch").absorb(&small).is_err(), "another k");
    }
}
