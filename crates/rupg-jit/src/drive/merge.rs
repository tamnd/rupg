// Lifted from rudb crates/rudb-qc/src/merge.rs at cd9f9676 (2026-10-05).
// Changes: the module `merge` of `crate::drive`, over the accumulators of `crate::codegen`, which has no `SumInt`, `AvgInt` or `AvgFloat`. `sets` is not lifted, as a pipeline runs on one worker yet, and a fold serves the close of an aggregate in runs. A float minimum or maximum compares as the body does, with a NaN above every other value, as PostgreSQL orders floats.

//! Folding one worker's groups into another's, the merge step of section 5.4 of
//! `spec/compiler/05-pipelines-and-state.md` for a thread-local aggregation table.
//!
//! The generated body updates a group's accumulators in place and the table does not know what
//! they are, so the fold is written here from the [`Grouping`] the generator described them with.
//! Each one does to two partial accumulators what the body does to an accumulator and a row, so a
//! query answers the same on any number of workers, with one exception that cannot be helped:
//! a float total is added up in a different order.

use std::cmp::Ordering;

use rupg_common::{Error, Result};

use crate::codegen::{AccOp, Grouping, qir_type, unsigned};
use crate::ir::Ty;
use crate::rt::table::read_u128;
use crate::rt::text;

/// How to fold one accumulator.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Fold {
    /// Where it is in the row.
    at: usize,
    how: How,
}

#[derive(Clone, Copy, Debug)]
enum How {
    /// An `i64` count.
    Count,
    /// An `i128` total, an `i64` part and a seen byte.
    SumSplit,
    /// An `i128` total, an `i64` part and an `i64` count.
    AvgSplit,
    /// An `i64` total and a seen byte.
    SumNarrow,
    /// An `i64` total and an `i64` count.
    AvgNarrow,
    /// An `f64` total and a seen byte.
    SumFloat,
    /// A value of `width` bytes and a seen byte.
    Extreme { width: usize, keep: Keep, order: Order },
    /// A `str16` and a seen byte.
    Text { least: bool },
    /// Kept outside the row.
    Outside,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Keep {
    Least,
    Greatest,
    Any,
}

#[derive(Clone, Copy, Debug)]
enum Order {
    F32,
    F64,
    Unsigned,
    Signed,
}

/// The folds of every accumulator of `g`, in the order they sit in the row.
pub(crate) fn folds(g: &Grouping) -> Result<Vec<Fold>> {
    let mut folds = Vec::with_capacity(g.accs.len());
    for acc in &g.accs {
        let how = match acc.op {
            AccOp::CountStar | AccOp::Count => How::Count,
            AccOp::SumSplit => How::SumSplit,
            AccOp::AvgSplit => How::AvgSplit,
            AccOp::SumNarrow => How::SumNarrow,
            AccOp::AvgNarrow => How::AvgNarrow,
            AccOp::SumFloat => How::SumFloat,
            AccOp::Min | AccOp::Max | AccOp::AnyValue => {
                let ty = qir_type(acc.arg).map_err(|r| Error::internal(r.to_string()))?;
                let order = match ty {
                    Ty::F32 => Order::F32,
                    Ty::F64 => Order::F64,
                    _ if unsigned(acc.arg) => Order::Unsigned,
                    _ => Order::Signed,
                };
                let keep = match acc.op {
                    AccOp::Min => Keep::Least,
                    AccOp::Max => Keep::Greatest,
                    _ => Keep::Any,
                };
                How::Extreme { width: ty.bytes() as usize, keep, order }
            }
            AccOp::MinStr => How::Text { least: true },
            AccOp::MaxStr => How::Text { least: false },
            AccOp::Distinct(..) => How::Outside,
        };
        folds.push(Fold { at: (g.acc_offset + acc.offset) as usize, how });
    }
    Ok(folds)
}

/// Folds the accumulators of the row `src` into those of the row `dst`.
pub(crate) fn fold(folds: &[Fold], dst: &mut [u8], src: &[u8]) {
    for f in folds {
        let (d, s) = (&mut dst[f.at..], &src[f.at..]);
        match f.how {
            How::Count => put_i64(d, 0, i64_at(d, 0).wrapping_add(i64_at(s, 0))),
            // The part of `src` goes into the total, so that adding to the part of `dst` later
            // cannot overflow on account of it.
            How::SumSplit | How::AvgSplit => {
                let s_total = i128_at(s, 0).wrapping_add(i128::from(i64_at(s, 16)));
                put_i128(d, 0, i128_at(d, 0).wrapping_add(s_total));
                if matches!(f.how, How::SumSplit) {
                    d[24] |= s[24];
                } else {
                    put_i64(d, 24, i64_at(d, 24).wrapping_add(i64_at(s, 24)));
                }
            }
            How::SumNarrow => {
                put_i64(d, 0, i64_at(d, 0).wrapping_add(i64_at(s, 0)));
                d[8] |= s[8];
            }
            How::AvgNarrow => {
                put_i64(d, 0, i64_at(d, 0).wrapping_add(i64_at(s, 0)));
                put_i64(d, 8, i64_at(d, 8).wrapping_add(i64_at(s, 8)));
            }
            // The body only ever adds to a total that starts at zero, so adding a worker's total is
            // the same sum in another order. One with nothing in it is skipped, so that a total of
            // negative zero stays one.
            How::SumFloat => {
                if s[8] != 0 {
                    put_f64(d, f64_at(d) + f64_at(s));
                    d[8] = 1;
                }
            }
            How::Extreme { width, keep, order } => {
                if s[width] == 0 {
                    continue;
                }
                let replace = d[width] == 0
                    || match keep {
                        Keep::Any => false,
                        Keep::Least => less(order, &s[..width], &d[..width]),
                        Keep::Greatest => less(order, &d[..width], &s[..width]),
                    };
                if replace {
                    d[..=width].copy_from_slice(&s[..=width]);
                }
            }
            How::Text { least } => {
                if s[16] == 0 {
                    continue;
                }
                let replace = d[16] == 0 || {
                    let (old, new) = (read_u128(&d[..16]), read_u128(&s[..16]));
                    // SAFETY: both accumulators hold strings that are inline or in a heap the
                    // query's runtime keeps, its own or a worker's it holds.
                    let ord = unsafe { text::bytes(&new).cmp(text::bytes(&old)) };
                    if least { ord.is_lt() } else { ord.is_gt() }
                };
                if replace {
                    d[..17].copy_from_slice(&s[..17]);
                }
            }
            How::Outside => {}
        }
    }
}

/// Whether `a` is less than `b`, both values of the same width, the way the body compares them.
fn less(order: Order, a: &[u8], b: &[u8]) -> bool {
    match order {
        Order::F32 => {
            float(f64::from(f32::from_le_bytes(four(a))), f64::from(f32::from_le_bytes(four(b))))
        }
        Order::F64 => float(f64_at(a), f64_at(b)),
        Order::Unsigned => wide(a) < wide(b),
        Order::Signed => {
            let shift = 128 - 8 * a.len() as u32;
            ((wide(a) << shift) as i128 >> shift) < ((wide(b) << shift) as i128 >> shift)
        }
    }
}

/// Whether `a` is less than `b` in PostgreSQL's order of floats, which puts a NaN above every
/// other value and makes two NaNs equal, the order `FcmpLt` gives the body.
fn float(a: f64, b: f64) -> bool {
    match (a.is_nan(), b.is_nan()) {
        (_, true) => !a.is_nan(),
        (true, false) => false,
        (false, false) => a.partial_cmp(&b) == Some(Ordering::Less),
    }
}

/// Up to sixteen little endian bytes as a number.
fn wide(b: &[u8]) -> u128 {
    let mut w = [0u8; 16];
    w[..b.len()].copy_from_slice(b);
    u128::from_le_bytes(w)
}

fn four(b: &[u8]) -> [u8; 4] {
    let mut w = [0u8; 4];
    w.copy_from_slice(&b[..4]);
    w
}

fn i64_at(b: &[u8], at: usize) -> i64 {
    let mut w = [0u8; 8];
    w.copy_from_slice(&b[at..at + 8]);
    i64::from_le_bytes(w)
}

fn f64_at(b: &[u8]) -> f64 {
    f64::from_bits(i64_at(b, 0) as u64)
}

fn i128_at(b: &[u8], at: usize) -> i128 {
    read_u128(&b[at..at + 16]) as i128
}

fn put_i64(b: &mut [u8], at: usize, v: i64) {
    b[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

fn put_i128(b: &mut [u8], at: usize, v: i128) {
    b[at..at + 16].copy_from_slice(&v.to_le_bytes());
}

fn put_f64(b: &mut [u8], v: f64) {
    b[..8].copy_from_slice(&v.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_signed_minimum_compares_below_zero() {
        let (a, b) = ((-3i32).to_le_bytes(), 2i32.to_le_bytes());
        assert!(less(Order::Signed, &a, &b));
        assert!(!less(Order::Unsigned, &a, &b));
        let (a, b) = ((-1i8).to_le_bytes(), 0i8.to_le_bytes());
        assert!(less(Order::Signed, &a, &b));
    }

    #[test]
    fn counts_and_totals_add_and_extremes_keep_the_better() {
        let folds = [
            Fold { at: 0, how: How::Count },
            Fold { at: 8, how: How::SumSplit },
            Fold {
                at: 40,
                how: How::Extreme { width: 8, keep: Keep::Least, order: Order::Signed },
            },
            Fold { at: 56, how: How::Extreme { width: 8, keep: Keep::Any, order: Order::Signed } },
        ];
        // A split sum is an `i128` total, an `i64` part not in it yet and a seen byte.
        let row = |count: i64, sum: Option<(i128, i64)>, min: Option<i64>, any: Option<i64>| {
            let mut r = vec![0u8; 72];
            put_i64(&mut r, 0, count);
            if let Some((total, part)) = sum {
                put_i128(&mut r, 8, total);
                put_i64(&mut r, 24, part);
                r[32] = 1;
            }
            if let Some(m) = min {
                put_i64(&mut r, 40, m);
                r[48] = 1;
            }
            if let Some(a) = any {
                put_i64(&mut r, 56, a);
                r[64] = 1;
            }
            r
        };
        let mut d = row(2, Some((5, 1)), Some(4), None);
        fold(&folds, &mut d, &row(3, Some((-7, -2)), Some(-1), Some(9)));
        assert_eq!(i64_at(&d, 0), 5);
        assert_eq!(i128_at(&d, 8) + i128::from(i64_at(&d, 24)), -3);
        assert_eq!(i64_at(&d, 40), -1);
        assert_eq!((i64_at(&d, 56), d[64]), (9, 1));
        fold(&folds, &mut d, &row(1, None, None, Some(1)));
        assert_eq!(i128_at(&d, 8) + i128::from(i64_at(&d, 24)), -3);
        assert_eq!(d[32], 1);
        assert_eq!(i64_at(&d, 56), 9);
    }

    #[test]
    fn a_float_maximum_keeps_a_nan_as_postgresql_does() {
        let folds = [Fold {
            at: 0,
            how: How::Extreme { width: 8, keep: Keep::Greatest, order: Order::F64 },
        }];
        let row = |v: f64| {
            let mut r = vec![0u8; 16];
            put_f64(&mut r, v);
            r[8] = 1;
            r
        };
        let mut d = row(f64::NAN);
        fold(&folds, &mut d, &row(5.0));
        assert!(f64_at(&d).is_nan(), "a NaN is above every number");
        let mut d = row(5.0);
        fold(&folds, &mut d, &row(f64::NAN));
        assert!(f64_at(&d).is_nan());
        assert!(float(1.0, f64::NAN) && !float(f64::NAN, 1.0) && !float(f64::NAN, f64::NAN));
    }
}
