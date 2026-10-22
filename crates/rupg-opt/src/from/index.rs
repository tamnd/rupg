//! `create_index_paths` for one table: the scans of the table by its btree indexes, as `build_index_paths` makes them, with the costs of `btcostestimate`, `genericcostestimate` and `cost_index`. The planner has no statistics, so the order of the rows in the table has no correlation with the order of the index.

use std::borrow::Cow;
use std::collections::BTreeSet;

use rupg_analyze::{Expr, ExprKind, Var};
use rupg_pgcatalog::builtin;
use rupg_plan::{Cost, Item, Kind, Node};

use super::{OrderKey, Qual, Rel, Stats, plain_var};
use crate::{Index, Level, cost, exprs};

/// `BTREE_AM_OID`, the OID of the access method `btree`.
const BTREE_AM: u32 = 403;
/// `BTEqualStrategyNumber`.
const EQUAL: i16 = 3;
/// `INDOPTION_DESC`.
const DESC: i16 = 1;
/// `INDOPTION_NULLS_FIRST`.
const NULLS_FIRST: i16 = 2;
/// `DEFAULT_PAGE_CPU_MULTIPLIER`: the cost of a page of the descent of a btree, in units of `cpu_operator_cost`.
const PAGE_CPU_MULTIPLIER: f64 = 50.0;
/// `effective_cache_size`: 4 GB of pages of 8 kB.
const CACHE_PAGES: f64 = 524_288.0;

/// The table of a scan, with the estimates that the costs of its index scans read.
pub(super) struct Table<'a, 'q> {
    pub(super) r: usize,
    /// `pages` of the table.
    pub(super) pages: f64,
    /// `tuples` of the table.
    pub(super) tuples: f64,
    /// `rows` of the table: the rows that pass its conditions.
    pub(super) rows: f64,
    /// The conditions of the scan in the order of `baserestrictinfo`.
    pub(super) quals: &'a [Qual<'q>],
    /// The selectivity of `=` of two values, as [`cost::list_selectivity_by`] reads it.
    pub(super) eq: &'a dyn Fn(&Expr, &Expr) -> Option<f64>,
}

/// A condition that a column of an index can test, as an `IndexClause`: the condition with the column on the left, the key column of the index, and the place of the condition in the conditions of the scan.
struct Clause<'q> {
    expr: Cow<'q, Expr>,
    column: usize,
    at: usize,
    kind: ClauseKind,
}

impl Clause<'_> {
    /// The value that the column is compared with, which `index_other_operands_eval_cost` costs, or `None` for `IS NULL` and `IS NOT NULL`.
    fn other(&self) -> Option<&Expr> {
        match &self.expr.kind {
            ExprKind::ScalarArrayOp { args, .. } => args.get(1),
            ExprKind::NullTest(..) => None,
            _ => exprs::binary(&self.expr).map(|(_, _, right)| right),
        }
    }
}

#[derive(Clone, Copy)]
enum ClauseKind {
    /// An operator of the operator family of the column, with its strategy.
    Op(i16),
    /// `x op ANY (array)`, with the strategy of the operator and the items of the array as `estimate_array_length` finds them.
    Array(i16, f64),
    /// `IS NULL` when true, `IS NOT NULL` when false.
    Null(bool),
}

/// `build_index_paths` for each btree index of the table with no predicate, the newest index first: a scan in the order of the index, and a scan in the other order when its order is useful. A scan is an index only scan when the index has each column of the table that the query reads, as `check_index_only`. Each scan has the order of its rows that a node above can use, as `truncate_useless_pathkeys` finds it.
pub(super) fn paths<'q>(
    level: &Level<'q>,
    table: &Table<'_, 'q>,
    indexes: &[Index],
    stats: &Stats,
) -> Vec<Rel<'q>> {
    let mut out = Vec::new();
    let read = read_columns(table, stats);
    // `get_relation_info` puts each index first in `indexlist`, so the newest index comes first.
    for index in indexes.iter().rev() {
        if index.method != BTREE_AM || index.partial {
            continue;
        }
        let clauses = match_clauses(table, index);
        let only = read.iter().all(|a| *a > 0 && index.columns.contains(a));
        let forward = order(level, table.r, index, false, stats);
        let useful = stats.useful(&BTreeSet::from([table.r]), &forward);
        if !clauses.is_empty() || !useful.is_empty() || only {
            out.push(scan(level, table, index, &clauses, only, false, useful, stats));
        }
        let backward = order(level, table.r, index, true, stats);
        let useful = stats.useful(&BTreeSet::from([table.r]), &backward);
        if !useful.is_empty() {
            out.push(scan(level, table, index, &clauses, only, true, useful, stats));
        }
    }
    out
}

/// The columns of the table that the query reads above the scan and in the conditions of the scan, as `check_index_only` finds them with `pull_varattnos`.
fn read_columns(table: &Table<'_, '_>, stats: &Stats) -> BTreeSet<i16> {
    let mut out: BTreeSet<i16> = stats
        .needed
        .iter()
        .filter(|(v, _, _)| v.relation == table.r && v.levels_up == 0)
        .map(|(v, _, _)| v.attnum)
        .collect();
    for qual in table.quals {
        let mut columns = Vec::new();
        exprs::columns_at(&qual.expr, 0, &mut columns);
        for column in columns {
            if let ExprKind::Var(v) = column.kind
                && v.relation == table.r
            {
                out.insert(v.attnum);
            }
        }
    }
    out
}

/// `match_restriction_clauses_to_index`: the conditions of the scan that a key column of the index can test, each with the first column that can, as `match_clause_to_index`. The list is in the order of the columns, then in the order of the conditions, as `build_index_paths` makes `indexclauses`.
fn match_clauses<'q>(table: &Table<'_, 'q>, index: &Index) -> Vec<Clause<'q>> {
    let mut out: Vec<Clause<'q>> = Vec::new();
    for (at, qual) in table.quals.iter().enumerate() {
        let found =
            (0..index.keys.len()).find_map(|column| match_clause(table.r, index, column, qual, at));
        out.extend(found);
    }
    out.sort_by_key(|c| c.column);
    out
}

/// `match_clause_to_indexcol`: the condition as a clause of the key column of the index, when the column can test it. An operator of the operator family of the column must have the column on one side and on the other side a value that reads no column of the table and calls no volatile function. With the column on the right, the commutator of the operator must be in the family, and the clause is the condition with the commutator, as `commute_op_clause`. `x op ANY (array)` and `IS NULL` and `IS NOT NULL` of the column are clauses too.
fn match_clause<'q>(
    r: usize,
    index: &Index,
    column: usize,
    qual: &Qual<'q>,
    at: usize,
) -> Option<Clause<'q>> {
    let expr = &*qual.expr;
    let attnum = *index.keys.get(column)?;
    let family = *index.families.get(column)?;
    if attnum <= 0 || family == 0 || !matches!(index.collations.get(column), Some(0 | 100) | None) {
        return None;
    }
    let is_column = |e: &Expr| {
        plain_var(e).is_some_and(|v| v.relation == r && v.attnum == attnum && v.levels_up == 0)
    };
    let other_ok = |e: &Expr| !exprs::relations(e).contains(&r) && !exprs::volatile(e);
    let clause = |expr: Cow<'q, Expr>, kind| Clause { expr, column, at, kind };
    match &expr.kind {
        ExprKind::ScalarArrayOp { func, any: true, args } => {
            let [left, array] = args.as_slice() else { return None };
            let element = builtin::type_by_oid(array.ty).map_or(0, |t| t.elem);
            let op = cost::array_operator(*func, left.ty, element)?;
            if !is_column(left) || !other_ok(array) {
                return None;
            }
            let s = strategy(op.oid, family)?;
            let kind = ClauseKind::Array(s, cost::array_length(array));
            Some(clause(qual.expr.clone(), kind))
        }
        ExprKind::NullTest(arg, is_null) => {
            is_column(arg).then(|| clause(qual.expr.clone(), ClauseKind::Null(*is_null)))
        }
        _ => {
            let (op, left, right) = exprs::binary(expr)?;
            if is_column(left) && other_ok(right) {
                let s = strategy(op, family)?;
                return Some(clause(qual.expr.clone(), ClauseKind::Op(s)));
            }
            if is_column(right) && other_ok(left) {
                let commutator = builtin::operator_by_oid(op)?.commutator;
                let s = strategy(commutator, family)?;
                let commuted = exprs::commute(expr)?;
                return Some(clause(Cow::Owned(commuted), ClauseKind::Op(s)));
            }
            None
        }
    }
}

/// `get_op_opfamily_strategy`: the strategy of a search operator in the operator family, or `None` when the family has not the operator.
fn strategy(op: u32, family: u32) -> Option<i16> {
    builtin::amops_of_operator(op)
        .find(|a| a.family == family && a.purpose == b's')
        .map(|a| a.strategy)
}

/// `build_index_pathkeys`: the order of the rows of a scan of the index, forward or backward. A key that adds nothing after the keys before it, such as a column that a condition makes a constant, does not count, as `pathkey_is_redundant`. The order stops at a column that is an expression or whose operator class is not the default class of its type.
fn order(
    level: &Level<'_>,
    r: usize,
    index: &Index,
    backward: bool,
    stats: &Stats,
) -> Vec<OrderKey> {
    let relation = &level.query.relations[r];
    let mut out: Vec<OrderKey> = Vec::new();
    for (i, &attnum) in index.keys.iter().enumerate() {
        let column = usize::try_from(attnum - 1).ok().and_then(|c| relation.columns.get(c));
        let (Some(column), Some(true)) = (column, index.default_class.get(i).copied()) else {
            break;
        };
        let mut expr =
            Expr::new(ExprKind::Var(Var { relation: r, attnum, levels_up: 0 }), column.ty);
        expr.typmod = column.typmod;
        let before: Vec<&Expr> = out.iter().map(|k| &k.exprs[0]).collect();
        if stats.redundant(&expr, &before) {
            continue;
        }
        let option = index.options.get(i).copied().unwrap_or(0);
        let desc = (option & DESC != 0) != backward;
        let nulls_first = (option & NULLS_FIRST != 0) != backward;
        let mut exprs = vec![expr.clone()];
        if let Some((members, _)) =
            stats.equal.iter().find(|(m, _)| m.iter().any(|e| e.same(&expr)))
        {
            exprs.extend(members.iter().filter(|m| !m.same(&expr)).cloned());
        }
        out.push(OrderKey { exprs, desc, nulls_first });
    }
    out
}

/// The scan of the table by the index with the clauses, as `create_index_path` and `create_indexscan_plan` make it. The conditions of the scan that are not clauses of the index are the filter, in the order of `order_qual_clauses`.
#[allow(clippy::too_many_arguments)]
fn scan<'q>(
    level: &Level<'q>,
    table: &Table<'_, 'q>,
    index: &Index,
    clauses: &[Clause<'q>],
    only: bool,
    backward: bool,
    order: Vec<OrderKey>,
    stats: &Stats,
) -> Rel<'q> {
    let mut filter: Vec<Qual<'q>> = table
        .quals
        .iter()
        .enumerate()
        .filter(|(at, _)| !clauses.iter().any(|c| c.at == *at))
        .map(|(_, q)| q.clone())
        .collect();
    stats.sort_quals(&mut filter);
    let (index_startup, index_total, selectivity) = btree_cost(table, index, clauses, stats);
    // `cost_index`: with no correlation, each row costs a random page of the table, as `index_pages_fetched` finds the pages.
    let mut startup = index_startup;
    let mut run = index_total - index_startup;
    let fetched = cost::clamp_rows(selectivity * table.tuples);
    let pages = pages_fetched(fetched, table.pages, index.pages, table.pages);
    run += pages * cost::RANDOM_PAGE;
    let (qual_startup, qual_cost) = stats.qual_costs(filter.iter().map(|q| &*q.expr));
    startup += qual_startup;
    run += (cost::CPU_TUPLE + qual_cost) * fetched;
    let cond = clauses.iter().map(|c| Item { expr: c.expr.clone(), below: false }).collect();
    let kind = Kind::IndexScan {
        relation: table.r,
        index: index.oid,
        only,
        backward,
        cond,
        columns: index.columns.clone(),
    };
    let mut node = Node::new(kind, level.scope, Vec::new());
    node.filter = filter.iter().map(|q| q.item()).collect();
    let width = stats.width(&BTreeSet::from([table.r]));
    node.cost = Cost { startup, total: startup + run, rows: table.rows, width };
    Rel { node, rels: BTreeSet::from([table.r]), order }
}

/// `btcostestimate` and `genericcostestimate`: the start cost and the total cost of the scan of the index, and the selectivity of its clauses. The rows of the index that the scan reads come from the clauses of the first columns: the clauses of each column up to the first column with no `=` clause, and the clauses of that column. A unique index with `=` on each key column reads one row. Each item of `ANY` is one more descent of the tree.
fn btree_cost(
    table: &Table<'_, '_>,
    index: &Index,
    clauses: &[Clause<'_>],
    stats: &Stats,
) -> (f64, f64, f64) {
    let mut column = 0;
    let mut equal_here = false;
    let mut found_array = false;
    let mut found_null = false;
    let mut scans: f64 = 1.0;
    let mut bound: Vec<&Expr> = Vec::new();
    for clause in clauses {
        if column < clause.column {
            if equal_here {
                column += 1;
            }
            equal_here = false;
            // A column with no clause would be a skip array, but a column with no statistics has the default number of distinct values, so the bound clauses stop.
            if column < clause.column {
                found_array = true;
                break;
            }
        }
        match clause.kind {
            ClauseKind::Op(s) => equal_here |= s == EQUAL,
            ClauseKind::Array(s, items) => {
                found_array = true;
                if items > 1.0 {
                    scans *= items;
                }
                equal_here |= s == EQUAL;
            }
            ClauseKind::Null(true) => {
                found_null = true;
                equal_here = true;
            }
            ClauseKind::Null(false) => {}
        }
        bound.push(&*clause.expr);
    }
    let tuples = table.tuples;
    let index_tuples = if index.unique
        && column + 1 == index.keys.len()
        && equal_here
        && !found_array
        && !found_null
    {
        1.0
    } else {
        let selectivity = cost::list_selectivity_by(bound, table.eq);
        scans = scans.min((index.pages * 0.3333333).ceil()).max(1.0);
        (selectivity * tuples / scans).round_ties_even()
    };
    // `genericcostestimate`.
    let selectivity = cost::list_selectivity_by(clauses.iter().map(|c| &*c.expr), table.eq);
    let index_tuples = if index_tuples <= 0.0 {
        (selectivity * tuples / scans).round_ties_even()
    } else {
        index_tuples
    };
    let index_tuples = index_tuples.min(tuples).max(1.0);
    let index_pages = if index.pages > 1.0 && tuples > 1.0 {
        (index_tuples * (index.pages - 1.0) / tuples).ceil()
    } else {
        1.0
    };
    let mut total = if scans > 1.0 {
        let fetched = pages_fetched(index_pages * scans, index.pages, index.pages, table.pages);
        fetched * cost::RANDOM_PAGE
    } else {
        index_pages * cost::RANDOM_PAGE
    };
    let others = clauses.iter().filter_map(Clause::other);
    let qual_arg: f64 = others.map(|o| stats.qual_costs([o])).map(|(s, p)| s + p).sum();
    let qual_op = cost::CPU_OPERATOR * clauses.len() as f64;
    let mut startup = qual_arg;
    total += qual_arg;
    total += index_tuples * scans * (cost::CPU_INDEX_TUPLE + qual_op);
    if tuples > 1.0 {
        let descent = (tuples.ln() / 2.0_f64.ln()).ceil() * cost::CPU_OPERATOR;
        startup += descent;
        total += scans * descent;
    }
    // The index has no pages below its root yet, so its tree has a height of 0.
    let descent = PAGE_CPU_MULTIPLIER * cost::CPU_OPERATOR;
    startup += descent;
    total += scans * descent;
    (startup, total, selectivity)
}

/// `index_pages_fetched`: the pages of a relation that a scan of the rows reads, with the pages in the cache that `effective_cache_size` gives the relation, as Mackert and Lohman estimate them. The pages of the table stand for `total_table_pages`, which changes the result only when the tables of the query are larger than the cache.
fn pages_fetched(tuples: f64, pages: f64, index_pages: f64, all_pages: f64) -> f64 {
    let t = if pages > 1.0 { pages } else { 1.0 };
    let total = (all_pages + index_pages).max(1.0);
    let b = CACHE_PAGES * t / total;
    let b = if b <= 1.0 { 1.0 } else { b.ceil() };
    if t <= b {
        let fetched = 2.0 * t * tuples / (2.0 * t + tuples);
        if fetched >= t { t } else { fetched.ceil() }
    } else {
        let lim = 2.0 * t * b / (2.0 * t - b);
        let fetched = if tuples <= lim {
            2.0 * t * tuples / (2.0 * t + tuples)
        } else {
            b + (tuples - lim) * (t - b) / t
        };
        fetched.ceil()
    }
}
