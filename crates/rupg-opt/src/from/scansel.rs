//! `mergejoinscansel`: the fractions of the two sides of a merge join that the join reads. A merge join stops when one side has no more rows, and it skips the rows of a side that are before the first value of the other side.

use rupg_analyze::{Expr, ExprKind};
use rupg_plan::{STATISTIC_KIND_HISTOGRAM, STATISTIC_KIND_MCV};
use rupg_types::Value;

use super::{ScanEstimate, Stats, plain_var};
use crate::{cost, exprs};

/// The fraction of the rows of a side that the join skips before the first match, and the fraction that it reads up to its end.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct ScanSel {
    pub(super) start: f64,
    pub(super) end: f64,
}

/// The default: the join reads all the rows of the side.
pub(super) const ALL: ScanSel = ScanSel { start: 0.0, end: 1.0 };

/// `mergejoinscansel` for the clause `left = right`, the first clause of a merge join, in the order `desc` with the nulls first when `nulls_first`. The end of a side is the part of its values up to the greatest value of the other side, and its start is the part of its values below the least value of the other side, from `scalarineqsel`. Only one side can stop before its end, so the smaller end wins and the other side reads all its rows, and only one side can skip rows at its start. A default estimate is not used. The ranges come from the histogram and the most common values, as `get_variable_range`.
pub(super) fn mergejoinscansel(
    stats: &Stats,
    left: &Expr,
    right: &Expr,
    desc: bool,
    nulls_first: bool,
) -> (ScanSel, ScanSel) {
    let (Some(l), Some(r)) = (side(stats, left), side(stats, right)) else { return (ALL, ALL) };
    let (Some(lrange), Some(rrange)) = (range(&l, left, desc), range(&r, right, desc)) else {
        return (ALL, ALL);
    };
    let ineq = |est: &ScanEstimate, column: &Expr, value: &Value, other: &Expr, iseq: bool| {
        let constant = Expr::new(ExprKind::Const(value.clone()), other.ty);
        let op = cost::Inequality { isgt: desc, iseq, sorted: true };
        est.scalar_ineq(column, &constant, op).filter(|&s| s != cost::INEQ_SEL)
    };
    let (mut ls, mut rs) = (ALL, ALL);
    if let Some(s) = ineq(&l, left, &rrange.1, right, true) {
        ls.end = s;
    }
    if let Some(s) = ineq(&r, right, &lrange.1, left, true) {
        rs.end = s;
    }
    if ls.end > rs.end {
        ls.end = 1.0;
    } else if ls.end < rs.end {
        rs.end = 1.0;
    } else {
        (ls.end, rs.end) = (1.0, 1.0);
    }
    if let Some(s) = ineq(&l, left, &rrange.0, right, false) {
        ls.start = s;
    }
    if let Some(s) = ineq(&r, right, &lrange.0, left, false) {
        rs.start = s;
    }
    if ls.start < rs.start {
        ls.start = 0.0;
    } else if ls.start > rs.start {
        rs.start = 0.0;
    } else {
        (ls.start, rs.start) = (0.0, 0.0);
    }
    // The nulls come first in the order of the sort, and the join skips them too.
    if nulls_first {
        for (sel, est, expr) in [(&mut ls, &l, left), (&mut rs, &r, right)] {
            if let Some(s) = cost::Estimate::statistic(est, expr) {
                let nulls = f64::from(s.null_frac);
                sel.start = (sel.start + nulls).clamp(0.0, 1.0);
                sel.end = (sel.end + nulls).clamp(0.0, 1.0);
            }
        }
    }
    for sel in [&mut ls, &mut rs] {
        if sel.start >= sel.end {
            *sel = ALL;
        }
    }
    (ls, rs)
}

/// `examine_variable` of a side: the estimates of the relation of a column of one relation.
fn side(stats: &Stats, expr: &Expr) -> Option<ScanEstimate> {
    let [r] = exprs::relations(expr).into_iter().collect::<Vec<_>>()[..] else { return None };
    plain_var(expr).filter(|v| v.levels_up == 0 && v.relation == r)?;
    let tuples = stats.rows.get(&r)?.0;
    Some(stats.estimate(r, tuples))
}

/// `get_variable_range`: the least and the greatest value of a column, from the ends of its histogram and from its most common values, in the order of the sort. The most common values with no histogram give a range only when they have all the rows that are not null.
fn range(est: &ScanEstimate, expr: &Expr, desc: bool) -> Option<(Value, Value)> {
    let s = cost::Estimate::statistic(est, expr)?;
    let mut range: Option<(Value, Value)> = s
        .slots
        .iter()
        .find(|slot| slot.kind == STATISTIC_KIND_HISTOGRAM)
        .and_then(|slot| slot.values.as_deref())
        .and_then(|values| Some((values.first()?.clone(), values.last()?.clone())));
    if let Some(mcv) = s.slots.iter().find(|slot| slot.kind == STATISTIC_KIND_MCV) {
        let common: f64 = mcv.numbers.iter().flatten().map(|&n| f64::from(n)).sum();
        if range.is_some() || common + f64::from(s.null_frac) > 0.99999 {
            for value in mcv.values.iter().flatten() {
                range = Some(match range {
                    None => (value.clone(), value.clone()),
                    Some((min, max)) => {
                        let less = |a: &Value, b: &Value| {
                            crate::compare(expr.ty, a, expr.ty, b).is_some_and(|o| o.is_lt())
                        };
                        let min = if less(value, &min) { value.clone() } else { min };
                        let max = if less(&max, value) { value.clone() } else { max };
                        (min, max)
                    }
                });
            }
        }
    }
    range.map(|(min, max)| if desc { (max, min) } else { (min, max) })
}
