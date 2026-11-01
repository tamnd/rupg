//! `create_tidscan_paths`: the scans of a table by the item pointers of its rows in `ctid`, with the costs of `cost_tidscan` and `cost_tidrangescan`. A `Tid Scan` reads the rows of a list of item pointers, and a `Tid Range Scan` reads the pages between two item pointers.

use std::borrow::Cow;
use std::collections::BTreeSet;

use rupg_analyze::{BoolOp, Expr, ExprKind};
use rupg_pgcatalog::builtin;
use rupg_plan::{Cost, Item, Kind, Node};
use rupg_types::oid;

use super::classes::Joined;
use super::index::{Param, Table, Tables};
use super::{Qual, Rel, Stats};
use crate::{Level, cost, exprs};

/// `SelfItemPointerAttributeNumber`: the attribute number of `ctid`.
pub(super) const CTID: i16 = -1;
/// `TIDEqualOperator`.
const TID_EQUAL: u32 = 387;
/// `TIDLessOperator`, `TIDGreaterOperator`, `TIDLessEqOperator` and `TIDGreaterEqOperator`.
const TID_RANGE: [u32; 4] = [2799, 2800, 2801, 2802];

/// `IsCTIDVar`: true for the column `ctid` of the table `r`.
fn is_ctid(expr: &Expr, r: usize) -> bool {
    matches!(expr.kind, ExprKind::Var(v) if v.attnum == CTID && v.relation == r && v.levels_up == 0)
        && expr.ty == oid::TID
}

/// True for a value that a scan of the table `r` computes once: it reads no column of the table and calls no volatile function.
fn pseudoconstant(expr: &Expr, r: usize) -> bool {
    !exprs::relations(expr).contains(&r) && !exprs::volatile(expr)
}

/// `IsBinaryTidClause`: the operator of a condition that compares `ctid` of the table `r` on either side with a value that the scan computes once.
fn binary_tid(expr: &Expr, r: usize) -> Option<u32> {
    let (op, left, right) = exprs::binary(expr)?;
    let other = if is_ctid(left, r) {
        right
    } else if is_ctid(right, r) {
        left
    } else {
        return None;
    };
    pseudoconstant(other, r).then_some(op)
}

/// `RestrictInfoIsTidQual`: `ctid = value`, `ctid = ANY (array)` and `CURRENT OF` of the table `r`.
fn tid_qual(expr: &Expr, r: usize) -> bool {
    match &expr.kind {
        ExprKind::ScalarArrayOp { func, any: true, args } => {
            let [left, array] = args.as_slice() else { return false };
            let element = builtin::type_by_oid(array.ty).map_or(0, |t| t.elem);
            is_ctid(left, r)
                && cost::array_operator(*func, left.ty, element).is_some_and(|o| o.oid == TID_EQUAL)
                && pseudoconstant(array, r)
        }
        ExprKind::CurrentOf(_) => exprs::relations(expr).contains(&r),
        _ => binary_tid(expr, r) == Some(TID_EQUAL),
    }
}

/// `TidQualFromRestrictInfoList`: the conditions of a `Tid Scan` of the table `r`, which give the rows of the scan together, and true for `CURRENT OF`. The first condition that is one is the list, and else the shortest list of an `OR` whose arms each have one or are an `AND` with one. The list is empty when no condition can be a list.
fn tid_list<'e>(quals: impl IntoIterator<Item = &'e Expr>, r: usize) -> (Vec<&'e Expr>, bool) {
    let mut first = None;
    let mut ors: Vec<&Expr> = Vec::new();
    for qual in quals {
        if let ExprKind::Bool(BoolOp::Or, arms) = &qual.kind {
            let mut list = Vec::new();
            for arm in arms {
                let found = match &arm.kind {
                    ExprKind::Bool(BoolOp::And, args) => match tid_list(args, r) {
                        (found, false) => found,
                        (_, true) => Vec::new(),
                    },
                    _ if tid_qual(arm, r) => vec![arm],
                    _ => Vec::new(),
                };
                if found.is_empty() {
                    list.clear();
                    break;
                }
                list.extend(found);
            }
            if !list.is_empty() && (ors.is_empty() || list.len() < ors.len()) {
                ors = list;
            }
        } else if tid_qual(qual, r) {
            if matches!(qual.kind, ExprKind::CurrentOf(_)) {
                return (vec![qual], true);
            }
            first.get_or_insert(qual);
        }
    }
    match first {
        Some(qual) => (vec![qual], false),
        None => (ors, false),
    }
}

/// `create_tidscan_paths` for the conditions of the table: a `Tid Scan` of the conditions of [`tid_list`], and a `Tid Range Scan` of the conditions that compare `ctid` with `<`, `<=`, `>` or `>=`. `enable_tidscan` off makes no scan, but for `CURRENT OF`, for which the scan is the only plan of the table and the result is true.
pub(super) fn paths<'q>(
    level: &Level<'q>,
    table: &Table<'_, 'q>,
    stats: &Stats,
) -> (Vec<Rel<'q>>, bool) {
    let enabled = crate::enable().tidscan;
    let r = table.r;
    let quals: Vec<&Expr> = table.quals.iter().map(|q| &*q.expr).collect();
    let mut out = Vec::new();
    let (list, current) = tid_list(quals.iter().copied(), r);
    if !list.is_empty() && (enabled || current) {
        out.push(tid_scan(level, table, &list, stats));
        if current {
            return (out, true);
        }
    }
    if !enabled {
        return (out, false);
    }
    let in_range = |q: &&Expr| binary_tid(q, r).is_some_and(|op| TID_RANGE.contains(&op));
    let range: Vec<&Expr> = quals.iter().copied().filter(in_range).collect();
    if !range.is_empty() {
        out.push(range_scan(level, table, &range, stats));
    }
    (out, false)
}

/// The scans of the table for the conditions `ctid = value` of the joins, as `BuildParameterizedTidPaths`: first the conditions that the equivalence classes make for `ctid` with the members of other relations, but not of the relations of `prohibited`, then the conditions of the joins in `joins`. Each scan reads the rows of one condition, and the values of the relations of the condition.
pub(super) fn param_paths<'q>(
    level: &Level<'q>,
    table: &Table<'_, 'q>,
    joins: &[Qual<'q>],
    classes: &[Joined<'q>],
    prohibited: &BTreeSet<usize>,
    ppi: &Tables<'_, '_, 'q>,
    stats: &Stats,
) -> Vec<Param<'q>> {
    if !crate::enable().tidscan {
        return Vec::new();
    }
    let r = table.r;
    let base = table.quals.len();
    let implied = classes.iter().flat_map(|c| c.column(r, |m, _| is_ctid(m, r), prohibited));
    let mut out = Vec::new();
    for qual in implied.chain(joins.iter().cloned()) {
        if binary_tid(&qual.expr, r) != Some(TID_EQUAL) {
            continue;
        }
        let mut outer = exprs::relations(&qual.expr);
        outer.remove(&r);
        let t = ppi(&outer);
        let rel = tid_scan(level, &t, &[&qual.expr], stats);
        out.push(Param { rel, outer, enforced: t.quals[base..].to_vec(), indexed: false });
    }
    out
}

/// `cost_tidscan` and `create_tidscan_plan`: the `Tid Scan` of the table with the conditions of the list, which give its rows together. Each item pointer costs a random page and a row, and `= ANY` has an item pointer for each element of its array. The conditions of the list cost once, and the other conditions cost for each row. The filter has the other conditions: the conditions that are not the one condition of the list and are not from its equivalence class, or the conditions that are not the `OR` of the list.
fn tid_scan<'q>(
    level: &Level<'q>,
    table: &Table<'_, 'q>,
    list: &[&Expr],
    stats: &Stats,
) -> Rel<'q> {
    let ntuples: f64 = list
        .iter()
        .map(|q| match &q.kind {
            ExprKind::ScalarArrayOp { args, .. } => args.get(1).map_or(1.0, cost::array_length),
            _ => 1.0,
        })
        .sum();
    let (_, tid_cost) = stats.qual_costs(list.iter().copied());
    let (qual_startup, qual_cost) = stats.qual_costs(table.quals.iter().map(|q| &*q.expr));
    let startup = qual_startup + tid_cost;
    let per_tuple = cost::CPU_TUPLE + qual_cost - tid_cost;
    let run = cost::random_page() * ntuples + per_tuple * ntuples;
    let cond = match list {
        [one] => (*one).clone(),
        _ => Expr::new(
            ExprKind::Bool(BoolOp::Or, list.iter().map(|q| (*q).clone()).collect()),
            oid::BOOL,
        ),
    };
    let class = match list {
        [one] => table.quals.iter().find(|q| q.expr.same(one)).and_then(|q| q.class),
        _ => None,
    };
    let mut filter: Vec<Qual<'q>> = table
        .quals
        .iter()
        .filter(|q| !q.expr.same(&cond) && !(class.is_some() && q.class == class))
        .cloned()
        .collect();
    stats.sort_quals(&mut filter);
    let kind = Kind::TidScan { relation: table.r, range: false, cond: vec![item(cond)] };
    scan(level, table, kind, filter, (startup, startup + run), stats)
}

/// `cost_tidrangescan` and `create_tidrangescan_plan`: the `Tid Range Scan` of the table with the conditions of the range. The pages of the range cost a random page and then sequential pages, and its rows cost a row each. The conditions of the range cost once, and the other conditions, which are the filter, cost for each row.
fn range_scan<'q>(
    level: &Level<'q>,
    table: &Table<'_, 'q>,
    range: &[&Expr],
    stats: &Stats,
) -> Rel<'q> {
    let selectivity = cost::list_selectivity_by(range.iter().copied(), table.eq);
    let pages = (selectivity * table.pages).ceil();
    let pages = if pages <= 0.0 { 1.0 } else { pages };
    let ntuples = selectivity * table.tuples;
    let disk = cost::random_page() + cost::SEQ_PAGE * (pages - 1.0);
    let (_, tid_cost) = stats.qual_costs(range.iter().copied());
    let (qual_startup, qual_cost) = stats.qual_costs(table.quals.iter().map(|q| &*q.expr));
    let startup = qual_startup + tid_cost;
    let cpu = (cost::CPU_TUPLE + qual_cost - tid_cost) * ntuples;
    let in_range = |q: &&Qual<'q>| range.iter().any(|e| std::ptr::eq(*e, &*q.expr));
    let mut filter: Vec<Qual<'q>> = table.quals.iter().filter(|q| !in_range(q)).cloned().collect();
    stats.sort_quals(&mut filter);
    let cond = range.iter().map(|e| item((*e).clone())).collect();
    let kind = Kind::TidScan { relation: table.r, range: true, cond };
    scan(level, table, kind, filter, (startup, startup + cpu + disk), stats)
}

/// A condition of a scan as an expression of the node.
fn item<'q>(expr: Expr) -> Item<'q> {
    Item { expr: Cow::Owned(expr), below: false }
}

/// The node of a scan of the table with its filter and its start cost and total cost.
fn scan<'q>(
    level: &Level<'q>,
    table: &Table<'_, 'q>,
    kind: Kind<'q>,
    filter: Vec<Qual<'q>>,
    (startup, total): (f64, f64),
    stats: &Stats,
) -> Rel<'q> {
    let mut node = Node::new(kind, level.scope, Vec::new());
    node.filter = filter.iter().map(Qual::item).collect();
    let width = stats.width(&BTreeSet::from([table.r]));
    node.cost = Cost { startup, total, rows: table.rows, width };
    Rel { node, rels: BTreeSet::from([table.r]), order: Vec::new() }
}
