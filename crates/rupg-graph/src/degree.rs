// Lifted from rudb crates/rudb-graph/src/degree.rs at cd9f9676 (2026-10-05).
// Changes: errors of malformed bytes are XX001 `Error::corrupted`.

//! What a relationship knows about its own shape: the degree distribution, the certificates and
//! the gather locality.
//!
//! spec/stats/07-graph-statistics.md sections 7.2, 7.3 and 7.4. Five facts, all exact, all computed
//! from the same slice of parent `rid`s that [`crate::Link::build`] is already walking, so the
//! marginal cost of the lot is a histogram and three running sums.
//!
//! # Why these are not in the link's header
//!
//! Section 7.1's four numbers are: the child row count, the parent row count, the unmatched count
//! and the maximum degree. Three of those are in the forward link's header because the link cannot
//! be read without them. Nothing here is like that. A planner that has these plans better and a
//! planner that does not plans the way it planned before, which is the section 3.1 invariant and is
//! the definition of a statistic in this codebase.
//!
//! So this is its own section, and a build that runs out of budget before it gets here drops it and
//! keeps the link. The other way round would be a file whose statistics describe a relationship it
//! cannot resolve.
//!
//! # What a degree histogram can and cannot answer
//!
//! Thirty two log buckets, so a bucket is a doubling and the answer to *how many parents have about
//! this many children* is exact to within a factor of two. The mean is exact, because it is the
//! linked child count over the parent count and both are counted rather than bucketed. The maximum
//! is exact, because it is tracked on its own. A percentile is neither: [`Degrees::percentile`]
//! answers with a bucket's upper bound, which is the honest shape of the answer and is what the two
//! questions section 7.2 asks of it need.
//!
//! Those questions are worth restating, because they are why the buckets are here rather than only
//! the mean. A mean of four with a maximum of four is a uniform fan out that parallelises by parent.
//! A mean of four with a ninety ninth percentile of nine hundred is a workload where one worker gets
//! the whole tail and the scheduler should partition by edge. The mean alone does not tell those
//! apart and neither does the maximum alone, because one parent with a million children moves the
//! maximum and moves nothing else.

use rupg_common::{Error, Result};

use crate::rid::{NO_PARENT, Rid};

/// The payload layout version. See the same constant in `link.rs` for why it is belt and braces.
const LAYOUT: u8 = 1;

/// Buckets in the degree histogram.
///
/// Bucket zero is the childless parents and bucket `i` is the parents whose degree is in
/// `2^(i - 1)..2^i`, so thirty two of them reach two billion children of one parent and a degree
/// past that saturates into the last one. TPC-H's widest relationship at SF100 is `part` to
/// `lineitem` at a few hundred, so the saturation is a bound this will not meet rather than a
/// rounding this will.
pub const BUCKETS: usize = 32;

/// Bytes one of these costs on disk.
///
/// Section 7.7 budgets under a kilobyte per relationship and this is 312 bytes, of which 256 is the
/// histogram. There is one per relationship and not one per anything else, so the whole of a TPC-H
/// schema's graph statistics is under two kilobytes.
pub const BYTES: usize = 8 + 32 + 16 + BUCKETS * 8;

/// A relationship's shape, as measured rather than as declared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Degrees {
    /// Parents by degree, bucket zero for the childless.
    buckets: [u64; BUCKETS],
    /// Rows in the child table, so that totality is a question this can answer on its own.
    children: u64,
    /// Children that found a parent.
    linked: u64,
    /// The most children any one parent has, exactly rather than to a bucket.
    highest: u64,
    /// How many adjacent linked children the stride below was measured over.
    strides: u64,
    /// Sum of the absolute distance between the parent `rid`s of adjacent linked children.
    ///
    /// A `u128` because it is not bounded by anything useful: six hundred million children of a
    /// hundred and fifty million parents, each landing as far from the last as it can, is past what
    /// sixty four bits holds, and a locality number that silently wrapped would read as perfect
    /// locality, which is the one wrong answer this could give that a planner would act on.
    stride: u128,
    /// Whether the parent key was verified distinct, which is section 7.3's uniqueness certificate.
    unique: bool,
}

impl Degrees {
    /// Measures a relationship from the parent `rid` of each child, in child `rid` order.
    ///
    /// `unique` is the parent side's verified distinctness, which the caller has because it looked
    /// the children up in a key map and the key map counted. It is a parameter rather than
    /// something measured here because it is a fact about the parent's key column and this only
    /// sees the children.
    ///
    /// # The counting array
    ///
    /// A `u32` per parent, which is the one allocation here that is not constant. It is bounded by
    /// the parent table rather than by the child table, and the caller is already holding a `u64`
    /// per child, so on the relationships this matters for it is a fifth of what the build is
    /// already spending: SF100 `lineitem` against `orders` is 600 MB beside 4.8 GB.
    ///
    /// Degrees saturate at `u32::MAX` rather than wrapping. A parent with four billion children is
    /// past anything this project will meet, and a histogram that wrapped would put the widest
    /// parent in the narrowest bucket, which is worse than a bound that is known to be a bound.
    #[must_use]
    pub fn of(parents_of: &[Rid], parents: u64, unique: bool) -> Self {
        let mut counts = vec![0_u32; usize::try_from(parents).unwrap_or(0)];
        let mut linked = 0_u64;
        let mut strides = 0_u64;
        let mut stride = 0_u128;
        let mut previous: Option<Rid> = None;
        // row at a time: one child at a time is what a degree is counted from, and the stride is a
        // question about the pair of adjacent children rather than about either of them. No value
        // is built here; the slice is parent `rid`s.
        for parent in parents_of {
            if *parent == NO_PARENT {
                // The run of adjacent linked children is broken rather than bridged. Bridging would
                // measure a distance the gather never makes, since an unmatched child is a gather
                // that does not happen.
                previous = None;
                continue;
            }
            if let Some(held) = counts.get_mut(usize::try_from(*parent).unwrap_or(usize::MAX)) {
                *held = held.saturating_add(1);
            }
            if let Some(before) = previous {
                stride += u128::from(before.abs_diff(*parent));
                strides += 1;
            }
            previous = Some(*parent);
            linked += 1;
        }
        let mut buckets = [0_u64; BUCKETS];
        let mut highest = 0_u64;
        for count in &counts {
            buckets[bucket(u64::from(*count))] += 1;
            highest = highest.max(u64::from(*count));
        }
        Self {
            buckets,
            children: parents_of.len() as u64,
            linked,
            highest,
            strides,
            stride,
            unique,
        }
    }

    /// Rows in the child table.
    #[must_use]
    pub fn children(&self) -> u64 {
        self.children
    }

    /// Children that found a parent.
    #[must_use]
    pub fn linked(&self) -> u64 {
        self.linked
    }

    /// Parents, which is what the histogram counts.
    #[must_use]
    pub fn parents(&self) -> u64 {
        self.buckets.iter().sum()
    }

    /// The histogram, bucket zero first.
    #[must_use]
    pub fn buckets(&self) -> &[u64; BUCKETS] {
        &self.buckets
    }

    /// Children per parent on average, exactly.
    ///
    /// Zero for a relationship with no parents, which is the answer that reads as *no expansion*
    /// rather than as a missing number.
    #[must_use]
    pub fn mean(&self) -> f64 {
        let parents = self.parents();
        if parents == 0 {
            return 0.0;
        }
        #[allow(clippy::cast_precision_loss)]
        {
            self.linked as f64 / parents as f64
        }
    }

    /// The most children any one parent has.
    #[must_use]
    pub fn highest(&self) -> u64 {
        self.highest
    }

    /// A degree below which this share of parents falls, as a bucket's upper bound.
    ///
    /// `share` is a fraction, so the ninety ninth percentile is `0.99`. The answer is a power of
    /// two and is an upper bound rather than the value itself, because that is all a log bucketed
    /// histogram holds. It is never above [`Self::highest`], so a uniform relationship answers with
    /// its actual maximum rather than with the bucket the maximum happens to sit in.
    #[must_use]
    pub fn percentile(&self, share: f64) -> u64 {
        let parents = self.parents();
        if parents == 0 {
            return 0;
        }
        #[allow(
            clippy::cast_precision_loss,
            clippy::cast_sign_loss,
            clippy::cast_possible_truncation
        )]
        let want = (parents as f64 * share).ceil().max(1.0) as u64;
        let mut seen = 0_u64;
        for (at, count) in self.buckets.iter().enumerate() {
            seen += count;
            if seen >= want {
                return bound(at).min(self.highest);
            }
        }
        self.highest
    }

    /// Whether every child row found a parent, which is section 7.3's totality certificate.
    #[must_use]
    pub fn total(&self) -> bool {
        self.linked == self.children
    }

    /// Whether the parent key was verified distinct, which is section 7.3's uniqueness certificate.
    #[must_use]
    pub fn unique(&self) -> bool {
        self.unique
    }

    /// The average distance between the parent `rid`s of adjacent linked children.
    ///
    /// Section 7.4's gather locality: the number that says whether a link join's gathers hit cache,
    /// which section 6.4 of the graph specification otherwise has to approximate at plan time.
    /// `None` when there are not two adjacent linked children to measure, which is a relationship
    /// with nothing to gather rather than one with perfect locality.
    ///
    /// A monotone link's answer is the mean degree's reciprocal and is not interesting, which is why
    /// section 7.4 asks for this of the non-monotone ones. It is measured for both anyway, because
    /// the measurement is the same loop and a number that exists for one form and not the other is a
    /// number every reader has to ask the form about first.
    #[must_use]
    pub fn locality(&self) -> Option<f64> {
        if self.strides == 0 {
            return None;
        }
        #[allow(clippy::cast_precision_loss)]
        Some(self.stride as f64 / self.strides as f64)
    }

    /// Appends the payload.
    pub fn write(&self, out: &mut Vec<u8>) {
        let start = out.len();
        out.push(LAYOUT);
        out.push(u8::from(self.unique));
        out.extend_from_slice(&[0; 6]);
        for number in [self.children, self.linked, self.highest, self.strides] {
            out.extend_from_slice(&number.to_le_bytes());
        }
        out.extend_from_slice(&self.stride.to_le_bytes());
        for count in &self.buckets {
            out.extend_from_slice(&count.to_le_bytes());
        }
        debug_assert_eq!(out.len() - start, BYTES, "a degree payload is a fixed {BYTES} bytes");
    }

    /// Reads a payload back.
    ///
    /// # Errors
    ///
    /// If the payload is not the size this layout gives it, or names a layout this build does not
    /// know. Both are a section to drop rather than a query to fail, by section 3.1.
    ///
    /// # Panics
    ///
    /// It does not. Every slice below is taken from a payload whose length has already been checked
    /// against the one this layout gives it, which is why the reads spell that out with `expect`
    /// rather than threading an error through arithmetic that cannot go wrong.
    pub fn read(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != BYTES {
            return Err(malformed("a degree payload is not the size its layout gives it"));
        }
        if bytes[0] != LAYOUT {
            return Err(malformed(format!(
                "degree layout {} is not one this build knows",
                bytes[0]
            )));
        }
        let at = |from: usize| -> u64 {
            u64::from_le_bytes(bytes[from..from + 8].try_into().expect("eight bytes"))
        };
        let mut buckets = [0_u64; BUCKETS];
        for (bucket, held) in buckets.iter_mut().enumerate() {
            *held = at(56 + bucket * 8);
        }
        Ok(Self {
            buckets,
            children: at(8),
            linked: at(16),
            highest: at(24),
            strides: at(32),
            stride: u128::from_le_bytes(bytes[40..56].try_into().expect("sixteen bytes")),
            unique: bytes[1] != 0,
        })
    }
}

/// Which bucket a degree falls in: zero for the childless, and a doubling after that.
fn bucket(degree: u64) -> usize {
    if degree == 0 {
        return 0;
    }
    // `64 - leading_zeros` is one more than the highest set bit, so a degree of one lands in bucket
    // one, two and three in bucket two, four through seven in bucket three, and so on.
    (64 - degree.leading_zeros() as usize).min(BUCKETS - 1)
}

/// The highest degree a bucket holds.
fn bound(at: usize) -> u64 {
    match at {
        0 => 0,
        // The last bucket is where every degree past its start saturates, so its bound is the
        // largest degree there is rather than the one its width implies.
        at if at >= BUCKETS - 1 => u64::MAX,
        at => (1 << at) - 1,
    }
}

fn malformed(message: impl Into<String>) -> Error {
    Error::corrupted(format!("invalid graph statistics: {}", message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One child per entry, naming its parent, which is what the link build produces.
    fn of(parents_of: &[Rid], parents: u64) -> Degrees {
        Degrees::of(parents_of, parents, true)
    }

    #[test]
    fn a_uniform_fan_out_has_a_mean_a_maximum_and_a_percentile_that_agree() {
        // Four children each for a thousand parents, which is the shape section 7.2 says
        // parallelises by parent.
        let parents_of = (0..4000_u64).map(|child| child / 4).collect::<Vec<_>>();
        let held = of(&parents_of, 1000);
        assert_eq!(held.parents(), 1000, "every parent is in a bucket");
        assert_eq!(held.linked(), 4000);
        assert!((held.mean() - 4.0).abs() < 1e-9, "four children each");
        assert_eq!(held.highest(), 4, "and no parent has a fifth");
        assert_eq!(held.percentile(0.99), 4, "so the tail is the mean");
        assert!(held.total(), "every child found a parent");
        assert!(held.unique());
    }

    #[test]
    fn a_skewed_fan_out_has_the_same_mean_and_a_percentile_that_says_otherwise() {
        // The same four thousand children and the same thousand parents, with one parent holding
        // three thousand of them. Section 7.2's point: the mean does not tell these apart.
        let mut parents_of = vec![0_u64; 3001];
        parents_of.extend(1..1000_u64);
        let held = of(&parents_of, 1000);
        assert_eq!(held.parents(), 1000);
        assert!((held.mean() - 4.0).abs() < 1e-9, "the same mean as the uniform case");
        assert_eq!(held.highest(), 3001, "and one parent holds nearly all of it");
        assert_eq!(held.percentile(0.5), 1, "half the parents have one child");
        assert!(held.percentile(1.0) >= 3001, "and the last percentile reaches the tail");
    }

    #[test]
    fn a_childless_parent_is_bucket_zero_and_not_a_missing_row() {
        let held = of(&[0, 0, 2], 4);
        assert_eq!(held.parents(), 4, "all four are counted");
        assert_eq!(held.buckets()[0], 2, "two of them have no children");
        assert_eq!(held.buckets()[1], 1, "one has a single child");
        assert_eq!(held.buckets()[2], 1, "and one has two");
        assert_eq!(held.percentile(0.5), 0, "half the parents are childless");
    }

    #[test]
    fn an_unmatched_child_costs_the_totality_certificate_and_not_the_uniqueness_one() {
        let held = of(&[0, NO_PARENT, 1], 2);
        assert_eq!(held.children(), 3, "three children");
        assert_eq!(held.linked(), 2, "two of which found a parent");
        assert!(!held.total(), "so the relationship is not total");
        assert!(held.unique(), "which says nothing about the parent's key");
        assert!(!Degrees::of(&[0, 1], 2, false).unique(), "and that is the caller's fact");
    }

    #[test]
    fn a_clustered_child_gathers_near_and_a_scattered_one_gathers_far() {
        // Clustered: eight children walking four parents in order, so half the steps stay put and
        // half move by one.
        let near = of(&(0..8_u64).map(|child| child / 2).collect::<Vec<_>>(), 4);
        assert!((near.locality().expect("seven steps") - 3.0 / 7.0).abs() < 1e-9);
        // Scattered: the same eight children over the same four parents, alternating ends.
        let far = of(&[0, 3, 0, 3, 0, 3, 0, 3], 4);
        assert!((far.locality().expect("seven steps") - 3.0).abs() < 1e-9);
        assert!(far.locality() > near.locality(), "which is the number section 6.4 wanted");
    }

    #[test]
    fn an_unmatched_child_breaks_the_stride_rather_than_bridging_it() {
        // The gather from parent zero to parent nine never happens, because the child between them
        // has no parent to gather.
        let held = of(&[0, NO_PARENT, 9], 10);
        assert_eq!(held.locality(), None, "no two adjacent children are both linked");
    }

    #[test]
    fn a_relationship_with_no_children_has_no_locality_rather_than_perfect_locality() {
        let held = of(&[], 4);
        assert_eq!(held.locality(), None);
        assert_eq!(held.mean(), 0.0);
        assert!(held.total(), "no child went unmatched, because there were none");
    }

    #[test]
    fn what_is_written_is_what_is_read() {
        let parents_of = (0..4000_u64).map(|child| (child * 7) % 1000).collect::<Vec<_>>();
        let mut held = Degrees::of(&parents_of, 1000, false);
        held.children += 1;
        let mut out = Vec::new();
        held.write(&mut out);
        assert_eq!(out.len(), BYTES, "the payload is a fixed size");
        assert_eq!(Degrees::read(&out).expect("reads back"), held);
    }

    #[test]
    fn a_payload_of_the_wrong_size_or_the_wrong_layout_is_refused() {
        let held = of(&[0, 1], 2);
        let mut out = Vec::new();
        held.write(&mut out);
        assert!(Degrees::read(&out[..BYTES - 1]).is_err(), "short");
        out.push(0);
        assert!(Degrees::read(&out).is_err(), "long");
        out.pop();
        out[0] = LAYOUT + 1;
        assert!(Degrees::read(&out).is_err(), "from a build that came after this one");
    }

    #[test]
    fn the_buckets_double_and_the_last_one_holds_everything_past_it() {
        assert_eq!(bucket(0), 0);
        assert_eq!(bucket(1), 1);
        assert_eq!(bucket(2), 2);
        assert_eq!(bucket(3), 2);
        assert_eq!(bucket(4), 3);
        assert_eq!(bucket(7), 3);
        assert_eq!(bucket(8), 4);
        assert_eq!(bucket(u64::MAX), BUCKETS - 1, "and nothing falls off the end");
        assert_eq!(bound(0), 0);
        assert_eq!(bound(1), 1);
        assert_eq!(bound(2), 3);
        assert_eq!(bound(3), 7);
        assert_eq!(bound(BUCKETS - 1), u64::MAX);
    }
}
