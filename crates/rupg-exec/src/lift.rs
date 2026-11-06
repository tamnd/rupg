//! The run of a correlated subquery once for all the rows of the query outside it, as the dependent join elimination of Neumann and Kemper (BTW 2015) unnests it (spec/15). The subquery must read the query outside it only through conditions of `WHERE` that are an `=` of an integer of the subquery and an integer column of the query just outside it. The run without these conditions gives the rows of the subquery with their keys, and each row of the query outside then reads the rows of its key from a hash table. A subquery with an aggregate and no `GROUP BY` runs with a `GROUP BY` of the keys, and a key with no group gets the row that the subquery gives for no input, so that `count(*)` gives 0 and not null.
//!
//! The subquery runs once at its first evaluation. The run for each row also reads all the rows of the tables of the subquery at the first row, into the cache of the tables, so the run once costs approximately the same for a query that reads only one row outside the subquery. The run of the subquery tests its conditions on rows that the old run did not test, so no expression of it can be volatile, and an error of the run makes the subquery run as before for each row, which then gives the same errors as before.

use std::collections::HashMap;
use std::rc::Rc;

use rupg_analyze::{BoolOp, Expr, ExprKind, Query, SortGroup, SubLinkKind, Target};
use rupg_common::Result;
use rupg_func::Session;
use rupg_types::{Value, oid};

use crate::scan::{self, Key};
use crate::{Cache, Rows, agg, correlated, free_work, run_query, volatile, window};

/// The `=` functions of two integers: `int2eq`, `int4eq`, `int8eq`, `int24eq`, `int42eq`, `int48eq`, `int84eq`, `int28eq` and `int82eq`.
const EQUALS: [u32; 9] = [63, 65, 467, 158, 159, 852, 474, 1850, 1856];

/// The state of a correlated subquery in a run, as [`Cache`] keeps it.
pub(crate) enum State {
    /// The subquery runs for each row. The queries that ran keep their addresses, which the cache uses as keys.
    Never {
        _kept: Vec<Query>,
    },
    Lifted(Rc<Lifted>),
}

/// The rows of a correlated subquery for each key.
pub(crate) struct Lifted {
    /// The value of each key in the query outside the subquery: a column of that query.
    outer: Vec<Expr>,
    rows: HashMap<Vec<Key>, Rc<Rows>>,
    /// The rows for a key that has no row.
    empty: Rc<Rows>,
    /// For `EXISTS`, the conditions that read the query outside the subquery and are not keys. They read the columns of the subquery as [`ExprKind::SubColumn`] of the rows of a key.
    test: Option<Expr>,
    /// The row of an `EXISTS` that a row satisfies.
    one: Rc<Rows>,
    /// The queries that ran, which keep their addresses, which the cache uses as keys.
    _queries: Vec<Query>,
}

/// An evaluation of an expression of the query outside a subquery, with a row of the subquery for [`ExprKind::SubColumn`].
pub(crate) type Evaluate<'a> = dyn FnMut(&Expr, Option<&[Value]>) -> Result<Value> + 'a;

/// The rows of a lifted subquery for each key.
type Buckets = HashMap<Vec<Key>, Rc<Rows>>;

/// The two queries of a lifted subquery and the value of each key outside it.
struct Split {
    /// The subquery without the conditions of the keys, with the keys as its last columns.
    all: Query,
    /// For a subquery with an aggregate, the subquery with no input row.
    empty: Option<Query>,
    outer: Vec<Expr>,
    test: Option<Expr>,
}

/// The rows of a subquery for the row of the query outside it that `eval` reads, or `None` when the subquery must run for the row. `eval` evaluates an expression of the query outside the subquery, with a row of the subquery for [`ExprKind::SubColumn`].
pub(crate) fn rows(
    query: &Query,
    kind: &SubLinkKind,
    params: &[Value],
    session: &dyn Session,
    cache: &Cache,
    eval: &mut Evaluate<'_>,
) -> Result<Option<Rc<Rows>>> {
    let key: *const Query = query;
    let found = cache.lifted.borrow().iter().position(|(q, _)| *q == key);
    let lifted = match found {
        Some(at) => match &cache.lifted.borrow()[at].1 {
            State::Never { .. } => return Ok(None),
            State::Lifted(lifted) => Rc::clone(lifted),
        },
        None => {
            let state = if correlated(query) {
                lift(query, kind, params, session, cache)
            } else {
                State::Never { _kept: Vec::new() }
            };
            let lifted = match &state {
                State::Lifted(lifted) => Some(Rc::clone(lifted)),
                State::Never { .. } => None,
            };
            cache.lifted.borrow_mut().push((key, state));
            match lifted {
                Some(lifted) => lifted,
                None => return Ok(None),
            }
        }
    };
    let mut keys = Vec::with_capacity(lifted.outer.len());
    for expr in &lifted.outer {
        let value = eval(expr, None)?;
        if value.is_null() {
            return Ok(Some(Rc::clone(&lifted.empty)));
        }
        match scan::key(value) {
            Some(k) => keys.push(k),
            None => return Ok(None),
        }
    }
    let Some(rows) = lifted.rows.get(&keys) else { return Ok(Some(Rc::clone(&lifted.empty))) };
    let Some(test) = &lifted.test else { return Ok(Some(Rc::clone(rows))) };
    for row in rows.iter() {
        if eval(test, Some(row))?.as_bool() == Some(true) {
            return Ok(Some(Rc::clone(&lifted.one)));
        }
    }
    Ok(Some(Rc::clone(&lifted.empty)))
}

/// The state of a subquery at its first evaluation: its rows for each key, or [`State::Never`] when it cannot run once or when the run fails.
fn lift(
    query: &Query,
    kind: &SubLinkKind,
    params: &[Value],
    session: &dyn Session,
    cache: &Cache,
) -> State {
    let Some(split) = split(query, kind) else { return State::Never { _kept: Vec::new() } };
    let Split { all, empty, outer, test } = split;
    let mut queries = vec![all];
    queries.extend(empty);
    let width = outer.len();
    // With a test, `EXISTS` keeps the rows of a key, which the test reads.
    let collapse = *kind == SubLinkKind::Exists && test.is_none();
    let one = Rc::new(vec![Vec::new()]);
    let run = || -> Result<(Buckets, Rc<Rows>)> {
        let empty_rows = match queries.get(1) {
            Some(q) => Rc::new(run_query(q, params, session, cache, &[], false)?),
            None => Rc::new(Vec::new()),
        };
        let found = run_query(&queries[0], params, session, cache, &[], false)?;
        let mut grouped: HashMap<Vec<Key>, Rows> = HashMap::new();
        'row: for mut row in found {
            let Some(columns) = row.len().checked_sub(width) else {
                return Err(rupg_common::Error::internal("a lifted row has no key"));
            };
            let mut keys = Vec::with_capacity(width);
            for value in row.drain(columns..) {
                if value.is_null() {
                    continue 'row;
                }
                match scan::key(value) {
                    Some(k) => keys.push(k),
                    None => {
                        return Err(rupg_common::Error::internal("a lifted key is not an integer"));
                    }
                }
            }
            let rows = grouped.entry(keys).or_default();
            if !collapse {
                rows.push(row);
            }
        }
        let rows = grouped
            .into_iter()
            .map(|(k, rows)| (k, if collapse { Rc::clone(&one) } else { Rc::new(rows) }))
            .collect();
        Ok((rows, empty_rows))
    };
    let result = run();
    match result {
        Ok((rows, empty)) => {
            State::Lifted(Rc::new(Lifted { outer, rows, empty, test, one, _queries: queries }))
        }
        Err(_) => State::Never { _kept: queries },
    }
}

/// True when an expression reads a column of a query outside the query at `depth`.
fn reads_outer(expr: &Expr, depth: usize) -> bool {
    expr.find(depth, &mut |e, depth| match e.kind {
        ExprKind::Var(var) if var.levels_up > depth => Some(()),
        _ => None,
    })
    .is_some()
}

/// The column of the query just outside a subquery that an expression of the subquery is, with no other type of the same form.
fn outer_column(expr: &Expr) -> Option<Expr> {
    match &expr.kind {
        ExprKind::Var(var) if var.levels_up == 1 => {
            let mut var = *var;
            var.levels_up = 0;
            Some(Expr { kind: ExprKind::Var(var), ..expr.clone() })
        }
        ExprKind::Relabel(arg, _) => outer_column(arg),
        _ => None,
    }
}

/// The `<` operator, the `<` function and the `=` function of an integer type.
fn int_operators(ty: u32) -> Option<(u32, u32, u32)> {
    match ty {
        oid::INT2 => Some((95, 64, 63)),
        oid::INT4 => Some((97, 66, 65)),
        oid::INT8 => Some((412, 469, 467)),
        _ => None,
    }
}

/// The queries of a subquery that can run once, or `None` when it must run for each row.
fn split(query: &Query, kind: &SubLinkKind) -> Option<Split> {
    let exists = *kind == SubLinkKind::Exists;
    let aggs = !agg::calls(query).is_empty();
    // `EXISTS` needs only a row, so a `LIMIT` that is a constant more than 0 does not change it.
    let limit_ok = match &query.limit {
        None => true,
        Some(limit) => {
            exists && !aggs && matches!(limit.kind, ExprKind::Const(Value::Int8(n)) if n > 0)
        }
    };
    if !limit_ok
        || query.set_op.is_some()
        || !window::calls(query).is_empty()
        || query.has_target_srfs()
        || !query.ctes.is_empty()
        || query.recursive
        || !query.row_marks.is_empty()
        || query.write.is_some()
        || query.offset.is_some()
        || !query.distinct.is_empty()
        || !query.group.is_empty()
        || !query.grouping_sets.is_empty()
        || query.having.is_some()
        || !query.barrier.is_empty()
        || !query.security.is_empty()
        || !agg::outer_calls(query).is_empty()
        || !free_work(query).is_empty()
        || (aggs && !matches!(kind, SubLinkKind::Expr | SubLinkKind::Exists))
    {
        return None;
    }
    let filter = query.filter.as_ref()?;
    let mut conds = Vec::new();
    scan::conjuncts(filter, &mut conds);
    let (mut kept, mut inner, mut outer, mut other) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for cond in conds {
        if !reads_outer(cond, 0) {
            kept.push(cond.clone());
        } else if let Some((side, column)) = key(cond) {
            inner.push(side);
            outer.push(column);
        } else {
            other.push(cond.clone());
        }
    }
    // Only `EXISTS` with no aggregate can test the other conditions on the rows of a key.
    if inner.is_empty() || (!other.is_empty() && !(exists && !aggs)) {
        return None;
    }
    let mut all = query.clone();
    all.filter = and(kept);
    all.minmax = None;
    let mut test = None;
    if exists && !aggs {
        all.targets.clear();
        all.sort.clear();
        all.limit = None;
        if let Some(cond) = and(other) {
            let (cond, columns) = outer_test(cond)?;
            all.targets.extend(columns.into_iter().enumerate().map(|(n, expr)| Target {
                name: format!("?test{n}?"),
                expr,
                origin: None,
                junk: false,
            }));
            test = Some(cond);
        }
    }
    if aggs {
        all.sort.clear();
    }
    // The keys come after the columns of the subquery and before its junk columns, which the run does not give, so the sort moves to the new places of the junk columns.
    let first = all.targets.iter().position(|t| t.junk).unwrap_or(all.targets.len());
    let count = inner.len();
    for (n, expr) in inner.into_iter().enumerate() {
        let target = Target { name: format!("?key{n}?"), expr, origin: None, junk: false };
        all.targets.insert(first + n, target);
    }
    for item in &mut all.sort {
        if item.target >= first {
            item.target += count;
        }
    }
    let mut empty = None;
    if aggs {
        all.grouped = true;
        for (n, target) in all.targets.iter().enumerate().skip(first).take(count) {
            let (operator, sort, equal) = int_operators(target.expr.ty)?;
            all.group.push(SortGroup {
                target: n,
                operator,
                sort,
                equal,
                nulls_first: false,
                hashable: true,
                collation: 0,
            });
        }
        let mut none = query.clone();
        none.filter = Some(Expr {
            kind: ExprKind::Const(Value::Bool(false)),
            ty: oid::BOOL,
            typmod: -1,
            location: None,
        });
        none.minmax = None;
        none.sort.clear();
        if correlated(&none) {
            return None;
        }
        empty = Some(none);
    }
    let still = correlated(&all);
    let unstable = all.all_exprs(0).iter().any(|(e, _)| volatile(e));
    if still || unstable {
        return None;
    }
    Some(Split { all, empty, outer, test })
}

/// The column of the subquery and the column of the query outside it that a condition of the subquery tests for `=`, when the condition can be a key.
fn key(cond: &Expr) -> Option<(Expr, Expr)> {
    let ExprKind::Func(f) = &cond.kind else { return None };
    let [a, b] = f.args.as_slice() else { return None };
    if !EQUALS.contains(&f.oid) {
        return None;
    }
    let (side, column) = match (outer_column(a), outer_column(b)) {
        (None, Some(column)) => (a, column),
        (Some(column), None) => (b, column),
        _ => return None,
    };
    let plain = !reads_outer(side, 0)
        && side.find(0, &mut |e, _| matches!(e.kind, ExprKind::SubLink(_)).then_some(())).is_none();
    if !plain || int_operators(side.ty).is_none() || int_operators(column.ty).is_none() {
        return None;
    }
    Some((side.clone(), column))
}

/// The `AND` of conditions, or `None` for no condition.
fn and(mut conds: Vec<Expr>) -> Option<Expr> {
    match conds.len() {
        0 => None,
        1 => conds.pop(),
        _ => Some(Expr {
            kind: ExprKind::Bool(BoolOp::And, conds),
            ty: oid::BOOL,
            typmod: -1,
            location: None,
        }),
    }
}

/// A condition of a subquery as a test in the query just outside it, and the columns of the subquery that the test reads as [`ExprKind::SubColumn`]. `None` when the condition reads more than the columns of the two queries and constants, or is volatile.
fn outer_test(mut cond: Expr) -> Option<(Expr, Vec<Expr>)> {
    let mut columns: Vec<Expr> = Vec::new();
    let mut ok = !volatile(&cond);
    cond.each_mut(0, &mut |e, _| {
        if matches!(
            e.kind,
            ExprKind::SubLink(_)
                | ExprKind::Agg(_)
                | ExprKind::PlaceHolder(_)
                | ExprKind::SubColumn(_)
        ) {
            ok = false;
            return true;
        }
        let ExprKind::Var(var) = e.kind else { return false };
        match var.levels_up {
            0 => {
                let at = columns.iter().position(|c| c == e).unwrap_or_else(|| {
                    columns.push(e.clone());
                    columns.len() - 1
                });
                e.kind = ExprKind::SubColumn(at);
            }
            1 => e.kind = ExprKind::Var(rupg_analyze::Var { levels_up: 0, ..var }),
            _ => ok = false,
        }
        true
    });
    ok.then_some((cond, columns))
}
