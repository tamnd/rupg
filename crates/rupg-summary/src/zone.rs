//! The zone of a vector or a segment, and the test of a filter against it (spec/12 section 12.6.1).

use std::sync::Arc;

use crate::key;

/// The bounds of the non-null values of a vector or a segment, as keys of [`crate::key`], with the counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Zone {
    /// The key of the lowest non-null value. Meaningless when every row is null.
    pub min: u64,
    /// The key of the highest non-null value. Meaningless when every row is null.
    pub max: u64,
    /// The number of rows.
    pub rows: u32,
    /// The number of null rows.
    pub nulls: u32,
    /// True when the key map keeps every value apart, so that equal keys mean equal values.
    pub exact: bool,
    /// For a string column with a dictionary: the vector holds codes of the dictionary tail, which have no rank in the bounds (spec/09 section 9.7.1). A range test cannot prune such a zone.
    pub tail: bool,
}

/// The result of a test, with the three cases of spec/12 section 12.6.1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// No row can pass. The scan skips the rows.
    None,
    /// Every row passes. The scan does not evaluate the filter.
    All,
    /// Some rows can pass. The scan reads the rows and evaluates the filter.
    Some,
}

impl Verdict {
    /// The verdict of `a AND b`.
    #[must_use]
    pub fn and(self, other: Verdict) -> Verdict {
        match (self, other) {
            (Verdict::None, _) | (_, Verdict::None) => Verdict::None,
            (Verdict::All, Verdict::All) => Verdict::All,
            _ => Verdict::Some,
        }
    }

    /// The verdict of `a OR b`.
    #[must_use]
    pub fn or(self, other: Verdict) -> Verdict {
        match (self, other) {
            (Verdict::All, _) | (_, Verdict::All) => Verdict::All,
            (Verdict::None, Verdict::None) => Verdict::None,
            _ => Verdict::Some,
        }
    }
}

/// A filter on one column, with its constants as keys. A comparison is false for a null value, as in SQL, where a filter that is not true removes the row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Test {
    /// `x = c`.
    Eq(u64),
    /// `x <> c`.
    Ne(u64),
    /// `x < c`.
    Lt(u64),
    /// `x <= c`.
    Le(u64),
    /// `x > c`.
    Gt(u64),
    /// `x >= c`.
    Ge(u64),
    /// `x IN (c, ...)`, with the keys in ascending order and no key two times.
    In(Vec<u64>),
    /// `x IN (c, ...)` of integers that span few values: the keys as [`Test::In`] has them, the least value, and the bitmap of the values from the least value, which [`in_span`] tests. [`Test::int_in`] makes it.
    Span(Vec<u64>, i64, Arc<[u64]>),
    /// `x IS NULL`.
    IsNull,
    /// `x IS NOT NULL`.
    IsNotNull,
    /// The code of `x` is in the set: [`has_code`] of the bits and the code is true. A test of a text that the zones do not read, for example `LIKE`, gives the set of the codes of the dictionary that pass it.
    Codes(Arc<[u64]>),
}

/// True when the code `code` is in the set of codes `bits`: bit `code % 64` of the word `code / 64` is set.
#[must_use]
#[inline]
pub fn has_code(bits: &[u64], code: u64) -> bool {
    usize::try_from(code / 64)
        .ok()
        .and_then(|w| bits.get(w))
        .is_some_and(|w| w >> (code % 64) & 1 == 1)
}

/// The most values that a bitmap of [`Test::Span`] holds for each key, the most values that it holds for any keys, and the most values that it holds.
const SPAN_PER_KEY: u64 = 256;
const SPAN_FLOOR: u64 = 1 << 18;
const SPAN_MOST: u64 = 1 << 24;

/// True when the integer `value` is in the bitmap `bits` of the values from `least`. A value below `least` wraps to a large offset, which is out of the bitmap.
#[must_use]
#[inline]
pub fn in_span(least: i64, bits: &[u64], value: i64) -> bool {
    has_code(bits, value.wrapping_sub(least) as u64)
}

/// True when `count` integers from `least` to `most` make a bitmap of [`Test::Span`]: they span fewer than 256 values for each integer, or fewer than 2^18 values, and fewer than 2^24 values. A bitmap of 2^18 values has 32 KiB, which stays in the cache of level 2.
#[must_use]
pub fn spans(least: i64, most: i64, count: usize) -> bool {
    let room = (count as u64).saturating_mul(SPAN_PER_KEY).clamp(SPAN_FLOOR, SPAN_MOST);
    most.checked_sub(least)
        .and_then(|span| u64::try_from(span).ok())
        .is_some_and(|span| span < room)
}

impl Test {
    /// `x IN (c, ...)` of the keys `keys` of integers, in ascending order and with no key two times. When the keys make a bitmap, as [`spans`] tells, the test is [`Test::Span`] with a bitmap, so that the test of a value reads one bit and does no binary search.
    #[must_use]
    pub fn int_in(keys: Vec<u64>) -> Test {
        let (Some(&first), Some(&last)) = (keys.first(), keys.last()) else {
            return Test::In(keys);
        };
        let (least, most) = (key::int_of(first), key::int_of(last));
        if !spans(least, most, keys.len()) {
            return Test::In(keys);
        }
        let span = most.wrapping_sub(least) as u64;
        let mut bits = vec![0u64; (span / 64 + 1) as usize];
        for &k in &keys {
            let at = key::int_of(k).wrapping_sub(least) as u64;
            bits[(at / 64) as usize] |= 1 << (at % 64);
        }
        Test::Span(keys, least, Arc::from(bits))
    }
}

impl Zone {
    /// The zone of no rows.
    pub const EMPTY: Zone =
        Zone { min: u64::MAX, max: 0, rows: 0, nulls: 0, exact: true, tail: false };

    /// The zone of `rows` integer values from `min` to `max`, with `nulls` nulls.
    #[must_use]
    pub fn ints(min: i64, max: i64, rows: u32, nulls: u32) -> Zone {
        Zone { min: key::int(min), max: key::int(max), rows, nulls, exact: true, tail: false }
    }

    /// True when every row is null.
    #[must_use]
    pub fn all_null(&self) -> bool {
        self.nulls == self.rows
    }

    /// Adds the zone of other rows, for the zone of a segment from the zones of its vectors.
    pub fn merge(&mut self, other: &Zone) {
        if !other.all_null() {
            if self.all_null() {
                self.min = other.min;
                self.max = other.max;
            } else {
                self.min = self.min.min(other.min);
                self.max = self.max.max(other.max);
            }
        }
        self.rows += other.rows;
        self.nulls += other.nulls;
        self.exact &= other.exact;
        self.tail |= other.tail;
    }

    /// The verdict of `test` on the rows of the zone.
    #[must_use]
    pub fn test(&self, test: &Test) -> Verdict {
        if self.rows == 0 {
            return Verdict::None;
        }
        match test {
            Test::IsNull => {
                return if self.nulls == 0 {
                    Verdict::None
                } else if self.all_null() {
                    Verdict::All
                } else {
                    Verdict::Some
                };
            }
            Test::IsNotNull => {
                return if self.all_null() {
                    Verdict::None
                } else if self.nulls == 0 {
                    Verdict::All
                } else {
                    Verdict::Some
                };
            }
            _ => {}
        }
        if self.all_null() {
            return Verdict::None;
        }
        // The verdict on the non-null values. A null fails every comparison, so All becomes Some when the zone has a null.
        let verdict = if self.tail { Verdict::Some } else { self.test_values(test) };
        if verdict == Verdict::All && self.nulls > 0 { Verdict::Some } else { verdict }
    }

    fn test_values(&self, test: &Test) -> Verdict {
        let (lo, hi) = (self.min, self.max);
        let single = self.exact && lo == hi;
        match *test {
            Test::Eq(c) => {
                if c < lo || c > hi {
                    Verdict::None
                } else if single {
                    Verdict::All
                } else {
                    Verdict::Some
                }
            }
            Test::Ne(c) => {
                if c < lo || c > hi {
                    Verdict::All
                } else if single {
                    Verdict::None
                } else {
                    Verdict::Some
                }
            }
            Test::Lt(c) => self.below(c, false),
            Test::Le(c) => self.below(c, true),
            Test::Gt(c) => self.above(c, false),
            Test::Ge(c) => self.above(c, true),
            // A key out of the bounds of the zone is in no row of it. A zone with two values or more passes a key in its bounds as some rows, and a zone with one value has at most one key in its bounds, so the first key in the bounds gives the verdict.
            Test::In(ref list) | Test::Span(ref list, ..) => {
                let from = list.partition_point(|&c| c < lo);
                match list.get(from) {
                    Some(&c) if c <= hi => self.test_values(&Test::Eq(c)),
                    _ => Verdict::None,
                }
            }
            // The zones of codes hold ranks, and a set of codes has no bounds to test them with.
            Test::Codes(_) => Verdict::Some,
            Test::IsNull | Test::IsNotNull => unreachable!("handled by test"),
        }
    }

    /// `x < c`, or `x <= c` when `equal`.
    fn below(&self, c: u64, equal: bool) -> Verdict {
        let (lo, hi) = (self.min, self.max);
        // A key below the constant's key means a value below the constant, whether or not the map is exact.
        if hi < c || (equal && self.exact && hi == c) {
            Verdict::All
        } else if lo > c || (!equal && self.exact && lo == c) {
            Verdict::None
        } else {
            Verdict::Some
        }
    }

    /// `x > c`, or `x >= c` when `equal`.
    fn above(&self, c: u64, equal: bool) -> Verdict {
        let (lo, hi) = (self.min, self.max);
        if lo > c || (equal && self.exact && lo == c) {
            Verdict::All
        } else if hi < c || (!equal && self.exact && hi == c) {
            Verdict::None
        } else {
            Verdict::Some
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval(test: &Test, value: Option<i64>) -> bool {
        let Some(v) = value else { return matches!(test, Test::IsNull) };
        let k = key::int(v);
        match test {
            Test::Eq(c) => k == *c,
            Test::Ne(c) => k != *c,
            Test::Lt(c) => k < *c,
            Test::Le(c) => k <= *c,
            Test::Gt(c) => k > *c,
            Test::Ge(c) => k >= *c,
            Test::In(list) => list.contains(&k),
            Test::Span(_, least, bits) => in_span(*least, bits, v),
            Test::Codes(bits) => has_code(bits, v as u64),
            Test::IsNull => false,
            Test::IsNotNull => true,
        }
    }

    /// Every verdict agrees with the rows, for every small zone and every test.
    #[test]
    fn verdicts_agree_with_the_rows() {
        let domain: Vec<Option<i64>> = vec![None, Some(1), Some(2), Some(3)];
        let mut rows_sets = Vec::new();
        for mask in 1u32..(1 << 6) {
            let rows: Vec<Option<i64>> =
                (0..6).filter(|i| mask & (1 << i) != 0).map(|i| domain[i % 4]).collect();
            rows_sets.push(rows);
        }
        let mut tests = vec![
            Test::IsNull,
            Test::IsNotNull,
            Test::In(vec![key::int(0), key::int(2)]),
            Test::In(vec![key::int(1), key::int(3)]),
            Test::In(vec![key::int(2)]),
            Test::In(vec![key::int(-1), key::int(0), key::int(4)]),
            Test::int_in(vec![key::int(0), key::int(2)]),
            Test::int_in(vec![key::int(-1), key::int(0), key::int(4)]),
            Test::Codes(Arc::from([0b1010_u64])),
        ];
        for c in 0..=4 {
            let c = key::int(c);
            tests.extend([
                Test::Eq(c),
                Test::Ne(c),
                Test::Lt(c),
                Test::Le(c),
                Test::Gt(c),
                Test::Ge(c),
            ]);
        }
        for rows in &rows_sets {
            let values: Vec<i64> = rows.iter().flatten().copied().collect();
            let nulls = rows.len() - values.len();
            let zone = Zone::ints(
                values.iter().copied().min().unwrap_or(0),
                values.iter().copied().max().unwrap_or(0),
                rows.len() as u32,
                nulls as u32,
            );
            for test in &tests {
                let passing = rows.iter().filter(|v| eval(test, **v)).count();
                match zone.test(test) {
                    Verdict::None => assert_eq!(passing, 0, "{rows:?} {test:?}"),
                    Verdict::All => assert_eq!(passing, rows.len(), "{rows:?} {test:?}"),
                    Verdict::Some => {}
                }
            }
        }
    }

    #[test]
    fn keys_near_each_other_make_a_bitmap() {
        let keys = |list: &[i64]| list.iter().map(|&x| key::int(x)).collect::<Vec<u64>>();
        let Test::Span(_, least, bits) = Test::int_in(keys(&[-70, 3, 4000])) else {
            panic!("keys in 2^18 values make a bitmap");
        };
        assert_eq!(least, -70);
        for v in -200..5000 {
            assert_eq!(in_span(least, &bits, v), [-70, 3, 4000].contains(&v), "{v}");
        }
        assert!(!in_span(least, &bits, i64::MIN));
        assert!(!in_span(least, &bits, i64::MAX));
        assert!(matches!(Test::int_in(keys(&[0, 300_000])), Test::In(_)));
        assert!(matches!(Test::int_in(keys(&[i64::MIN, i64::MAX])), Test::In(_)));
        assert!(matches!(Test::int_in(Vec::new()), Test::In(_)));
        let many: Vec<i64> = (0..10_000).map(|i| i * 250).collect();
        assert!(matches!(Test::int_in(keys(&many)), Test::Span(..)));
        let sparse: Vec<i64> = (0..10_000).map(|i| i * 300).collect();
        assert!(matches!(Test::int_in(keys(&sparse)), Test::In(_)));
    }

    #[test]
    fn inexact_keys_never_decide_on_equal_keys() {
        let zone = Zone {
            min: key::bytes(b"abcdefgh1"),
            max: key::bytes(b"abcdefgh9"),
            rows: 2,
            nulls: 0,
            exact: false,
            tail: false,
        };
        assert_eq!(zone.test(&Test::Eq(key::bytes(b"abcdefgh5"))), Verdict::Some);
        assert_eq!(zone.test(&Test::Ne(key::bytes(b"abcdefgh5"))), Verdict::Some);
        assert_eq!(zone.test(&Test::Lt(key::bytes(b"abcdefgh"))), Verdict::Some);
        assert_eq!(zone.test(&Test::Lt(key::bytes(b"abcdefgi"))), Verdict::All);
        assert_eq!(zone.test(&Test::Eq(key::bytes(b"b"))), Verdict::None);
    }

    #[test]
    fn tail_codes_block_pruning() {
        let mut zone = Zone::ints(3, 5, 10, 0);
        zone.tail = true;
        assert_eq!(zone.test(&Test::Eq(key::int(100))), Verdict::Some);
        assert_eq!(zone.test(&Test::IsNull), Verdict::None);
    }

    #[test]
    fn merge_skips_null_zones() {
        let mut zone = Zone::ints(0, 0, 4, 4);
        zone.merge(&Zone::ints(5, 9, 4, 1));
        zone.merge(&Zone::ints(-2, 3, 4, 0));
        assert_eq!((zone.min, zone.max, zone.rows, zone.nulls), (key::int(-2), key::int(9), 12, 5));
    }
}
