// Lifted from rudb crates/rudb-graph/src/rid.rs at cd9f9676 (2026-10-05).
// Changes: errors of malformed bytes are XX001 `Error::corrupted`.

//! The row id, and the structure that turns one into a physical position.
//!
//! A rudb native table is stripes of sixty four parts of 1024 rows, appended in order, so a row
//! already has an ordinal: its position in append order over the whole table, zero based. That
//! ordinal is the row id, and [`Places`] is the prefix sum that resolves one.
//!
//! The three properties spec/graph/02-the-data-model.md section 2.1 requires of a `rid` are worth
//! restating here because two of them are properties of this module and one is not. Stability under
//! append is free from append order. Cheap resolution to a physical position is what [`Places`] is
//! for. Invalidation rather than corruption by a rewrite is not here at all: it is the generation
//! stamp on every section in the file, and it works because an ignored section changes no answer.

use rupg_common::{Error, Result};

/// A row's position in append order over the whole table, zero based.
///
/// A `u64` in every interface, per section 2.1, and narrower than that in every stored form. The
/// width is not a type parameter because a `rid` crosses between a link column packed to
/// twenty eight bits, a key map's permutation packed to twenty four, and a gather that wants a
/// `usize`, and a newtype per width would be three conversions at every one of those boundaries.
pub type Rid = u64;

/// The `rid` value reserved to mean *no parent*.
///
/// The maximum representable value in whatever width a forward link is packed to, which at the
/// `u64` interface is this. It covers both a null child key and a child key with no matching
/// parent, and section 2.4 is explicit that the two are distinguished, where an anti join or a
/// `NOT IN` needs them to be, by consulting the child column's own validity rather than by
/// reserving a second value here.
pub const NO_PARENT: Rid = u64::MAX;

/// Rows in one part, everywhere except the last part of a load.
///
/// A power of two, which is what makes the common case of [`Places::place`] a shift and a mask
/// rather than a search. It is 1024 and not `rudb::VECTOR_SIZE` on purpose: a part is a unit of
/// storage and a vector is a unit of execution, they have been different numbers since the vector
/// went to 8192, and a module that assumed they were the same would resolve every `rid` in the file
/// to the wrong part the next time either one moved.
pub const PART_ROWS: usize = 1024;

/// The most parts one stripe holds.
pub const STRIPE_PARTS: usize = 64;

/// Where a `rid` actually is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Place {
    /// Which stripe, indexing the table's stripe list.
    pub stripe: u32,
    /// Which part within that stripe.
    pub part: u32,
    /// Which row within that part.
    pub offset: u32,
}

/// One stripe's contribution to the prefix sum.
#[derive(Debug, Clone)]
struct StripeSum {
    /// Rows in the table before this stripe begins.
    base: u64,
    /// Rows in this stripe.
    rows: u64,
    /// Cumulative rows before each part, with a final total, so `parts.len()` is the part count
    /// plus one. Held even for a uniform stripe, because 260 bytes a stripe is 400 KB at a hundred
    /// million rows and a branch that sometimes has the array and sometimes does not is how a
    /// lookup this hot grows a second code path nobody measures.
    parts: Vec<u32>,
    /// Whether every part of this stripe holds exactly [`PART_ROWS`] rows.
    ///
    /// True for every stripe but the last of a load, which is the case worth a shift and a mask.
    uniform: bool,
}

/// The per-part row count prefix sum of one table, built once at open.
///
/// Sixty four `u32` per stripe plus a `u64` per stripe, which for a hundred million rows is about
/// four hundred kilobytes. Section 4.2 of spec/graph/04-in-memory.md says it is built at open
/// rather than walked per lookup, and the reason is arithmetic rather than taste: a join gathers
/// millions of times and an open happens once.
#[derive(Debug, Clone)]
pub struct Places {
    stripes: Vec<StripeSum>,
    rows: u64,
}

impl Places {
    /// Builds the prefix sum from the per-part row counts of every stripe, in stripe order.
    ///
    /// # Errors
    ///
    /// If a stripe holds more than [`STRIPE_PARTS`] parts, if a part is wider than [`PART_ROWS`],
    /// if a part other than the last of its stripe is short, or if the total overflows a `u64`.
    /// Every one of those is a malformed directory rather than a usage error, and the reason they
    /// are checked here rather than trusted is that this structure is what a link join indexes
    /// with: a prefix sum that is wrong by one resolves every `rid` past the fault to the wrong
    /// row, and a wrong row is a wrong answer rather than a slow one.
    pub fn build(per_stripe: &[Vec<u32>]) -> Result<Self> {
        let mut stripes = Vec::with_capacity(per_stripe.len());
        let mut base = 0_u64;
        for (at, parts) in per_stripe.iter().enumerate() {
            if parts.len() > STRIPE_PARTS {
                return Err(malformed(format!(
                    "stripe {at} has {} parts and a stripe holds at most {STRIPE_PARTS}",
                    parts.len()
                )));
            }
            let mut cumulative = Vec::with_capacity(parts.len() + 1);
            cumulative.push(0);
            let mut total = 0_u32;
            for (which, &rows) in parts.iter().enumerate() {
                if rows as usize > PART_ROWS {
                    return Err(malformed(format!(
                        "part {which} of stripe {at} holds {rows} rows and a part holds at most \
                         {PART_ROWS}"
                    )));
                }
                total = total.checked_add(rows).ok_or_else(|| {
                    malformed(format!("stripe {at} overflows a thirty two bit row count"))
                })?;
                cumulative.push(total);
            }
            // Only the last part of a stripe may be short, because every earlier one being full is
            // what licenses the shift and the mask. A stripe with a hole in the middle of it is a
            // writer bug, and finding it here rather than in a join is the difference between a
            // failed open and a wrong answer.
            let uniform = parts.iter().all(|&rows| rows as usize == PART_ROWS);
            if let Some((_, earlier)) = parts.split_last()
                && let Some(which) = earlier.iter().position(|&rows| rows as usize != PART_ROWS)
            {
                return Err(malformed(format!(
                    "part {which} of stripe {at} holds {} rows and only the last part of a \
                         stripe may be short",
                    earlier[which]
                )));
            }
            let rows = u64::from(total);
            stripes.push(StripeSum { base, rows, parts: cumulative, uniform });
            base = base
                .checked_add(rows)
                .ok_or_else(|| malformed("the table overflows a sixty four bit row count"))?;
        }
        Ok(Self { stripes, rows: base })
    }

    /// Committed rows, which is one past the largest resolvable `rid`.
    #[must_use]
    pub fn rows(&self) -> u64 {
        self.rows
    }

    /// Bytes this structure holds, for the cache budget of section 4.4.
    #[must_use]
    pub fn bytes(&self) -> usize {
        let per_stripe = size_of::<StripeSum>();
        self.stripes.iter().map(|stripe| per_stripe + stripe.parts.len() * size_of::<u32>()).sum()
    }

    /// Resolves a `rid` to its physical position, or `None` when it is past the end of the table.
    ///
    /// `None` rather than an error because a link may legitimately point past the end: a forward
    /// link built against one generation and read against another is stale, and section 3.1 of
    /// spec/graph/03-the-file-format.md wants a stale section ignored rather than raised. The
    /// generation check is what catches that case first; this is the second line.
    #[must_use]
    pub fn place(&self, rid: Rid) -> Option<Place> {
        if rid >= self.rows {
            return None;
        }
        // Stripes are in ascending base order, so this is a binary search for the last stripe whose
        // base is at or below the rid. `partition_point` is the branchless form of that and it is
        // the whole search for a table of one stripe, which is every table under 65,536 rows.
        let at = self.stripes.partition_point(|stripe| stripe.base <= rid) - 1;
        let stripe = &self.stripes[at];
        let within = rid - stripe.base;
        #[expect(
            clippy::cast_possible_truncation,
            reason = "a stripe holds at most 65,536 rows, so `within` fits a u32"
        )]
        let within = within as u32;
        let (part, offset) = if stripe.uniform {
            // The common case, and the reason PART_ROWS is a power of two.
            (within >> PART_SHIFT, within & PART_MASK)
        } else {
            let part = stripe.parts.partition_point(|&before| before <= within) - 1;
            #[expect(
                clippy::cast_possible_truncation,
                reason = "a stripe holds at most sixty four parts"
            )]
            let part = part as u32;
            (part, within - stripe.parts[part as usize])
        };
        #[expect(
            clippy::cast_possible_truncation,
            reason = "a table holds at most 4,294,967,295 stripes and the build checked the count"
        )]
        let stripe = at as u32;
        Some(Place { stripe, part, offset })
    }

    /// The `rid` of a physical position, which is [`Places::place`] backwards.
    ///
    /// The forward link build needs this: it walks the parent table in physical order and has to
    /// record what each row's `rid` is.
    ///
    /// `None` when the position is not in the table.
    #[must_use]
    pub fn rid(&self, place: Place) -> Option<Rid> {
        let stripe = self.stripes.get(place.stripe as usize)?;
        let before = *stripe.parts.get(place.part as usize)?;
        let rows = stripe.parts.get(place.part as usize + 1)? - before;
        if place.offset >= rows {
            return None;
        }
        Some(stripe.base + u64::from(before + place.offset))
    }

    /// Rows in one stripe, or `None` when there is no such stripe.
    #[must_use]
    pub fn stripe_rows(&self, stripe: u32) -> Option<u64> {
        self.stripes.get(stripe as usize).map(|held| held.rows)
    }

    /// Stripes in the table.
    #[must_use]
    pub fn stripes(&self) -> usize {
        self.stripes.len()
    }
}

/// `log2(PART_ROWS)`, for the shift in the uniform case.
const PART_SHIFT: u32 = PART_ROWS.trailing_zeros();

/// `PART_ROWS - 1`, for the mask in the uniform case.
#[expect(
    clippy::cast_possible_truncation,
    reason = "PART_ROWS is 1024, so the mask fits a u32 with room to spare"
)]
const PART_MASK: u32 = (PART_ROWS - 1) as u32;

fn malformed(message: impl Into<String>) -> Error {
    Error::corrupted(format!("invalid row id prefix sum: {}", message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stripe list where every stripe but the last is full, which is what a load writes.
    fn ladder(stripes: usize, tail: u32) -> Vec<Vec<u32>> {
        #[expect(clippy::cast_possible_truncation, reason = "PART_ROWS is 1024")]
        let full = PART_ROWS as u32;
        let mut out = vec![vec![full; STRIPE_PARTS]; stripes.saturating_sub(1)];
        if stripes > 0 {
            let whole = (tail / full) as usize;
            let mut last = vec![full; whole];
            if !tail.is_multiple_of(full) {
                last.push(tail % full);
            }
            out.push(last);
        }
        out
    }

    #[test]
    fn a_rid_resolves_to_the_part_and_the_offset_append_order_gave_it() {
        let places = Places::build(&ladder(1, 3000)).expect("build");
        assert_eq!(places.rows(), 3000);
        assert_eq!(places.place(0), Some(Place { stripe: 0, part: 0, offset: 0 }));
        assert_eq!(places.place(1023), Some(Place { stripe: 0, part: 0, offset: 1023 }));
        assert_eq!(places.place(1024), Some(Place { stripe: 0, part: 1, offset: 0 }));
        assert_eq!(places.place(2999), Some(Place { stripe: 0, part: 2, offset: 951 }));
        assert_eq!(places.place(3000), None);
    }

    #[test]
    fn every_rid_of_a_multi_stripe_table_round_trips_through_its_place() {
        // Two full stripes and a short one, which is the shape of any load that is not an exact
        // multiple of 65,536 rows, and the shape where an off by one in the prefix sum shows.
        let places = Places::build(&ladder(3, 5000)).expect("build");
        assert_eq!(places.rows(), 65_536 * 2 + 5000);
        for rid in 0..places.rows() {
            let place = places.place(rid).expect("every rid under the row count resolves");
            assert_eq!(places.rid(place), Some(rid), "rid {rid} did not round trip");
        }
        assert_eq!(places.place(places.rows()), None);
    }

    #[test]
    fn the_uniform_path_and_the_searching_path_agree_on_the_same_stripe() {
        // The same stripe described two ways: once as sixty four full parts, which takes the shift
        // and the mask, and once with a short part on the end, which takes the search. Every rid
        // that exists in both has to resolve to the same place, because otherwise the fast path is
        // a second implementation rather than an optimization of the first.
        #[expect(clippy::cast_possible_truncation, reason = "PART_ROWS is 1024")]
        let full = PART_ROWS as u32;
        let fast = Places::build(&[vec![full; 8]]).expect("build");
        let slow = Places::build(&[{
            let mut parts = vec![full; 7];
            parts.push(full - 1);
            parts
        }])
        .expect("build");
        for rid in 0..slow.rows() {
            assert_eq!(fast.place(rid), slow.place(rid), "rid {rid}");
        }
    }

    #[test]
    fn a_hole_in_the_middle_of_a_stripe_is_refused_rather_than_resolved() {
        #[expect(clippy::cast_possible_truncation, reason = "PART_ROWS is 1024")]
        let full = PART_ROWS as u32;
        let complaint = Places::build(&[vec![full, 7, full]]).expect_err("a short middle part");
        let complaint = complaint.to_string();
        assert!(complaint.contains("part 1"), "{complaint}");
        assert!(complaint.contains("only the last part"), "{complaint}");
    }

    #[test]
    fn a_part_wider_than_a_part_is_refused() {
        #[expect(clippy::cast_possible_truncation, reason = "PART_ROWS is 1024")]
        let over = PART_ROWS as u32 + 1;
        let complaint = Places::build(&[vec![over]]).expect_err("an oversized part");
        assert!(complaint.to_string().contains("at most"), "{complaint}");
    }

    #[test]
    fn a_stripe_wider_than_a_stripe_is_refused() {
        #[expect(clippy::cast_possible_truncation, reason = "PART_ROWS is 1024")]
        let full = PART_ROWS as u32;
        let complaint =
            Places::build(&[vec![full; STRIPE_PARTS + 1]]).expect_err("an oversized stripe");
        assert!(complaint.to_string().contains("at most 64"), "{complaint}");
    }

    #[test]
    fn an_empty_table_resolves_nothing_and_says_so_rather_than_panicking() {
        let places = Places::build(&[]).expect("build");
        assert_eq!(places.rows(), 0);
        assert_eq!(places.place(0), None);
        assert_eq!(places.stripes(), 0);
    }

    #[test]
    fn the_prefix_sum_of_a_hundred_million_rows_is_about_four_hundred_kilobytes() {
        // The size claim in section 4.2, checked rather than asserted in prose. It is checked
        // because the structure is resident per open table and a factor of ten here is the
        // difference between a cache and a leak.
        let stripes = 100_000_000 / (PART_ROWS * STRIPE_PARTS) + 1;
        let places = Places::build(&ladder(stripes, 4096)).expect("build");
        let bytes = places.bytes();
        assert!(bytes < 600 * 1024, "the prefix sum took {bytes} bytes");
        assert!(bytes > 200 * 1024, "the prefix sum took {bytes} bytes, which is suspiciously few");
    }

    #[test]
    fn no_parent_is_not_a_rid_any_table_can_resolve() {
        // The reserved value has to be outside every table, not merely outside this one, which is
        // what makes it safe to mean *no parent* in a link of any width.
        let places = Places::build(&ladder(2, 1)).expect("build");
        assert_eq!(places.place(NO_PARENT), None);
    }
}
