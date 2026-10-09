//! `WindowAgg` and the window functions of `windowfuncs.c`. Each window that a call uses sorts the rows on its items of `PARTITION BY` and `ORDER BY`, in the order of `select_active_windows`, and a window whose items are the first keys of the order before it does not sort again. The frame of each window is the default frame: from the start of the partition to the last peer of the row.

use std::cmp::Ordering;

use rupg_analyze::{Expr, ExprKind, Query, SortGroup, WindowClause, WindowFunc};
use rupg_common::{Error, Result, SqlState};
use rupg_func::Session;
use rupg_pgcatalog::builtin;
use rupg_types::Value;

use crate::{Eval, kernel, not_yet, signature, sort};

/// The window functions that the engine has, by the name of their C function.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Method {
    RowNumber,
    Rank,
    DenseRank,
    PercentRank,
    CumeDist,
    Ntile,
    /// `lag` and `lead`, with `true` for `lead`.
    LeadLag(bool),
    FirstValue,
    LastValue,
    NthValue,
}

/// The method of a window function, or the error that the engine does not have it.
pub(crate) fn method(func: u32, location: Option<usize>) -> Result<Method> {
    let src = builtin::proc_by_oid(func).map_or("", |p| p.src);
    Ok(match src {
        "window_row_number" => Method::RowNumber,
        "window_rank" => Method::Rank,
        "window_dense_rank" => Method::DenseRank,
        "window_percent_rank" => Method::PercentRank,
        "window_cume_dist" => Method::CumeDist,
        "window_ntile" => Method::Ntile,
        "window_lag" | "window_lag_with_offset" | "window_lag_with_offset_and_default" => {
            Method::LeadLag(false)
        }
        "window_lead" | "window_lead_with_offset" | "window_lead_with_offset_and_default" => {
            Method::LeadLag(true)
        }
        "window_first_value" => Method::FirstValue,
        "window_last_value" => Method::LastValue,
        "window_nth_value" => Method::NthValue,
        _ => {
            let error = not_yet(format!("window function {}", signature(func)));
            return Err(match location {
                Some(at) => error.at(at),
                None => error,
            });
        }
    })
}

/// The window function calls of an expression, outside its subqueries.
fn collect<'q>(expr: &'q Expr, out: &mut Vec<&'q WindowFunc>) {
    if let ExprKind::Window(w) = &expr.kind {
        out.push(w);
        return;
    }
    for child in expr.children() {
        collect(child, out);
    }
}

/// The window function calls of the select list.
pub(crate) fn calls(query: &Query) -> Vec<&WindowFunc> {
    let mut out = Vec::new();
    for target in &query.targets {
        collect(&target.expr, &mut out);
    }
    out
}

/// `select_active_windows`: the windows that a call uses, with their number from 1, in the order that the plan computes them.
fn active<'q>(query: &'q Query, calls: &[&WindowFunc]) -> Result<Vec<(usize, &'q WindowClause)>> {
    let refs = sort_group_refs(query);
    let mut actives: Vec<(usize, &WindowClause, Vec<&SortGroup>)> = Vec::new();
    for (i, window) in query.windows.iter().enumerate() {
        if !calls.iter().any(|c| c.winref == i + 1) {
            continue;
        }
        let mut unique: Vec<&SortGroup> = window.partition.iter().collect();
        for item in &window.order {
            if !unique.contains(&item) {
                unique.push(item);
            }
        }
        actives.push((i + 1, window, unique));
    }
    // common_prefix_cmp: the windows that sort on the same items come together, and a window whose items start with the items of another window comes first.
    sort::qsort(&mut actives, &mut |a, b| {
        for (x, y) in a.2.iter().zip(&b.2) {
            let order = refs[y.target]
                .cmp(&refs[x.target])
                .then(y.operator.cmp(&x.operator))
                .then(y.nulls_first.cmp(&x.nulls_first));
            if order != Ordering::Equal {
                return Ok(order);
            }
        }
        Ok(b.2.len().cmp(&a.2.len()))
    })?;
    Ok(actives.into_iter().map(|(winref, window, _)| (winref, window)).collect())
}

/// `ressortgroupref` of each target, as `assignSortGroupRef` gives it: the items of `ORDER BY`, `GROUP BY` and `DISTINCT`, then the items of `ORDER BY` and `PARTITION BY` of each window. A target in no list has 0.
fn sort_group_refs(query: &Query) -> Vec<usize> {
    let mut refs = vec![0; query.targets.len()];
    let mut last = 0;
    let lists = [&query.sort, &query.group, &query.distinct]
        .into_iter()
        .chain(query.windows.iter().flat_map(|w| [&w.order, &w.partition]));
    for list in lists {
        for item in list {
            if refs[item.target] == 0 {
                last += 1;
                refs[item.target] = last;
            }
        }
    }
    refs
}

/// `make_pathkeys_for_window`: the items of `PARTITION BY` and then of `ORDER BY`, with no second item for the same target. The planner gives an error for an item of `PARTITION BY` with no order.
fn keys(window: &WindowClause) -> Result<Vec<SortGroup>> {
    if window.partition.iter().any(|g| g.sort == 0) {
        return Err(Error::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "could not implement window PARTITION BY",
        )
        .with_detail("Window partitioning columns must be of sortable datatypes."));
    }
    if window.order.iter().any(|g| g.sort == 0) {
        return Err(Error::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "could not implement window ORDER BY",
        )
        .with_detail("Window ordering columns must be of sortable datatypes."));
    }
    let mut keys: Vec<SortGroup> = Vec::new();
    for item in window.partition.iter().chain(&window.order) {
        if !keys.iter().any(|k| k.target == item.target) {
            keys.push(item.clone());
        }
    }
    Ok(keys)
}

/// Checks the windows that the calls of a query use and the functions of their items.
pub(crate) fn check(query: &Query) -> Result<()> {
    let calls = calls(query);
    if calls.is_empty() {
        return Ok(());
    }
    for (_, window) in active(query, &calls)? {
        for item in keys(window)? {
            kernel(item.sort, None)?;
        }
        for item in window.partition.iter().chain(&window.order) {
            kernel(item.equal, None)?;
        }
    }
    Ok(())
}

/// The value of each window function call for each tuple, by the address of the call, and the order of the tuples after the last sort of the windows.
pub(crate) type Values = Vec<Vec<(*const WindowFunc, Value)>>;

/// Computes the window function calls of a query for its tuples, as the `WindowAgg` nodes do. The result has the order of the tuples after the windows, and the values of the calls for each tuple.
pub(crate) fn compute(
    eval: &Eval<'_>,
    query: &Query,
    calls: &[&WindowFunc],
    tuples: &[Vec<usize>],
    session: &dyn Session,
) -> Result<(Vec<usize>, Values)> {
    let actives = active(query, calls)?;
    // The values of the targets that the windows sort on, for each tuple.
    let mut used = vec![false; query.targets.len()];
    for (_, window) in &actives {
        for item in window.partition.iter().chain(&window.order) {
            used[item.target] = true;
        }
    }
    let mut rows = Vec::with_capacity(tuples.len());
    for tuple in tuples {
        let mut e = eval.at(tuple);
        let row = query
            .targets
            .iter()
            .zip(&used)
            .map(|(t, used)| if *used { e.eval(&t.expr) } else { Ok(Value::Null) })
            .collect::<Result<Vec<_>>>()?;
        rows.push(row);
    }
    let types: Vec<u32> = query.targets.iter().map(|t| t.expr.ty).collect();
    let mut order: Vec<usize> = (0..tuples.len()).collect();
    let mut current: Vec<SortGroup> = Vec::new();
    let mut values: Values = vec![Vec::new(); tuples.len()];
    for (winref, window) in actives {
        let wanted = keys(window)?;
        // pathkeys_count_contained_in: the rows need no sort when the keys of the window start the keys of the order.
        let same = |a: &SortGroup, b: &SortGroup| {
            a.target == b.target && a.sort == b.sort && a.nulls_first == b.nulls_first
        };
        let sorted =
            wanted.len() <= current.len() && wanted.iter().zip(&current).all(|(a, b)| same(a, b));
        if !sorted {
            let keys = sort::Keys::new(&wanted, &types, session)?;
            sort::qsort(&mut order, &mut |a, b| keys.compare(&rows[*a], &rows[*b]))?;
            current = wanted;
        }
        let partition = sort::Keys::new(&window.partition, &types, session)?;
        let peers = sort::Keys::new(&window.order, &types, session)?;
        let mine: Vec<&WindowFunc> = calls.iter().copied().filter(|c| c.winref == winref).collect();
        let mut start = 0;
        while start < order.len() {
            let mut end = start + 1;
            while end < order.len() && partition.equal(&rows[order[start]], &rows[order[end]])? {
                end += 1;
            }
            let part = &order[start..end];
            // The peer group of each row: the rows equal on the items of `ORDER BY`. With no item, every row of the partition is a peer.
            let mut groups: Vec<(usize, usize)> = Vec::with_capacity(part.len());
            let mut first = 0;
            for i in 1..=part.len() {
                if i == part.len() || !peers.equal(&rows[part[first]], &rows[part[i]])? {
                    groups.extend(std::iter::repeat_n((first, i), i - first));
                    first = i;
                }
            }
            for call in &mine {
                let method = method(call.oid, None)?;
                let results = partition_values(eval, call, method, part, &groups, tuples)?;
                for (&tuple, value) in part.iter().zip(results) {
                    values[tuple].push((std::ptr::from_ref(*call), value));
                }
            }
            start = end;
        }
    }
    Ok((order, values))
}

/// A count of rows as a value of type `bigint`.
fn int8(n: usize) -> Value {
    Value::Int8(i64::try_from(n).unwrap_or(i64::MAX))
}

/// The value of an argument of a call on a tuple.
fn arg(eval: &Eval<'_>, call: &WindowFunc, n: usize, tuple: &[usize]) -> Result<Value> {
    let expr =
        call.args.get(n).ok_or_else(|| Error::internal("a window function with no argument"))?;
    eval.at(tuple).eval(expr)
}

/// The value of an argument of type `integer` on a tuple, or `None` for a null.
fn int4_arg(eval: &Eval<'_>, call: &WindowFunc, n: usize, tuple: &[usize]) -> Result<Option<i32>> {
    match arg(eval, call, n, tuple)? {
        Value::Null => Ok(None),
        Value::Int4(v) => Ok(Some(v)),
        _ => Err(Error::internal("an argument of a window function that is not integer")),
    }
}

/// The values of a call for the rows of one partition. `groups` has the start and the end of the peer group of each row.
fn partition_values(
    eval: &Eval<'_>,
    call: &WindowFunc,
    method: Method,
    part: &[usize],
    groups: &[(usize, usize)],
    tuples: &[Vec<usize>],
) -> Result<Vec<Value>> {
    let n = part.len();
    let tuple = |i: usize| tuples[part[i]].as_slice();
    let mut out = Vec::with_capacity(n);
    match method {
        Method::RowNumber => out.extend((1..=n).map(int8)),
        Method::Rank => out.extend(groups.iter().map(|g| int8(g.0 + 1))),
        Method::DenseRank => {
            let mut rank = 0;
            for (i, g) in groups.iter().enumerate() {
                if g.0 == i {
                    rank += 1;
                }
                out.push(int8(rank));
            }
        }
        #[allow(clippy::cast_precision_loss)]
        Method::PercentRank => out.extend(
            groups
                .iter()
                .map(|g| Value::Float8(if n == 1 { 0.0 } else { g.0 as f64 / (n - 1) as f64 })),
        ),
        #[allow(clippy::cast_precision_loss)]
        Method::CumeDist => out.extend(groups.iter().map(|g| Value::Float8(g.1 as f64 / n as f64))),
        Method::Ntile => {
            // window_ntile: the first row that has a value of the argument sets the size of the buckets, and the first buckets take one more row when the rows do not divide.
            let total = i64::try_from(n).unwrap_or(i64::MAX);
            let mut state: Option<(i32, i64, i64, i64)> = None;
            for i in 0..n {
                if state.is_none() {
                    let Some(buckets) = int4_arg(eval, call, 0, tuple(i))? else {
                        out.push(Value::Null);
                        continue;
                    };
                    if buckets <= 0 {
                        return Err(Error::new(
                            SqlState::INVALID_ARGUMENT_FOR_NTILE,
                            "argument of ntile must be greater than zero",
                        ));
                    }
                    let buckets = i64::from(buckets);
                    let mut boundary = total / buckets;
                    let mut remainder = 0;
                    if boundary <= 0 {
                        boundary = 1;
                    } else {
                        remainder = total % buckets;
                        if remainder != 0 {
                            boundary += 1;
                        }
                    }
                    state = Some((1, 0, boundary, remainder));
                }
                let Some((ntile, rows, boundary, remainder)) = state.as_mut() else {
                    return Err(Error::internal("ntile with no state"));
                };
                *rows += 1;
                if *boundary < *rows {
                    if *remainder != 0 && i64::from(*ntile) == *remainder {
                        *remainder = 0;
                        *boundary -= 1;
                    }
                    *ntile += 1;
                    *rows = 1;
                }
                out.push(Value::Int4(*ntile));
            }
        }
        Method::LeadLag(forward) => {
            for i in 0..n {
                let offset = if call.args.len() > 1 {
                    match int4_arg(eval, call, 1, tuple(i))? {
                        Some(offset) => i64::from(offset),
                        None => {
                            out.push(Value::Null);
                            continue;
                        }
                    }
                } else {
                    1
                };
                let at = i64::try_from(i).unwrap_or(i64::MAX);
                let target = if forward { at + offset } else { at - offset };
                let value = match usize::try_from(target).ok().filter(|t| *t < n) {
                    Some(t) => arg(eval, call, 0, tuple(t))?,
                    None if call.args.len() > 2 => arg(eval, call, 2, tuple(i))?,
                    None => Value::Null,
                };
                out.push(value);
            }
        }
        Method::FirstValue => {
            for _ in 0..n {
                out.push(arg(eval, call, 0, tuple(0))?);
            }
        }
        Method::LastValue => {
            for g in groups {
                out.push(arg(eval, call, 0, tuple(g.1 - 1))?);
            }
        }
        Method::NthValue => {
            for (i, g) in groups.iter().enumerate() {
                let Some(nth) = int4_arg(eval, call, 1, tuple(i))? else {
                    out.push(Value::Null);
                    continue;
                };
                if nth <= 0 {
                    return Err(Error::new(
                        SqlState::INVALID_ARGUMENT_FOR_NTH_VALUE,
                        "argument of nth_value must be greater than zero",
                    ));
                }
                let at = usize::try_from(nth - 1).unwrap_or(usize::MAX);
                out.push(if at < g.1 { arg(eval, call, 0, tuple(at))? } else { Value::Null });
            }
        }
    }
    Ok(out)
}
