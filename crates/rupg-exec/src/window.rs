//! `WindowAgg` and the window functions of `windowfuncs.c`. Each window that a call uses sorts the rows on its items of `PARTITION BY` and `ORDER BY`, in the order of `select_active_windows`, and a window whose items are the first keys of the order before it does not sort again. The frame of a row has the rows from its start to its end in `ROWS`, `RANGE` or `GROUPS` mode, without the rows of the clause `EXCLUDE`. An aggregate function with `OVER`, `first_value`, `last_value` and `nth_value` use the rows of the frame.

use std::cmp::Ordering;

use rupg_analyze::{
    Expr, ExprKind, Frame, FrameBound, FrameExclusion, FrameMode, NullTreatment, Query, SortGroup,
    WindowClause, WindowFunc,
};
use rupg_common::{Error, Result, SqlState};
use rupg_func::{Call, Kernel, Session};
use rupg_pgcatalog::builtin;
use rupg_types::{Value, oid};

use crate::{Eval, Group, agg, kernel, not_yet, signature, sort};

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

/// The window function calls of an expression, outside its subqueries, with the types of their results.
fn collect<'q>(expr: &'q Expr, out: &mut Vec<(&'q WindowFunc, u32)>) {
    if let ExprKind::Window(w) = &expr.kind {
        out.push((w, expr.ty));
        return;
    }
    for child in expr.children() {
        collect(child, out);
    }
}

/// The window function calls of the select list.
pub(crate) fn calls(query: &Query) -> Vec<(&WindowFunc, u32)> {
    let mut out = Vec::new();
    for target in &query.targets {
        collect(&target.expr, &mut out);
    }
    out
}

/// `select_active_windows`: the windows that a call uses, with their number from 1, in the order that the plan computes them.
fn active<'q>(
    query: &'q Query,
    calls: &[(&WindowFunc, u32)],
) -> Result<Vec<(usize, &'q WindowClause)>> {
    let refs = sort_group_refs(query);
    let mut actives: Vec<(usize, &WindowClause, Vec<&SortGroup>)> = Vec::new();
    for (i, window) in query.windows.iter().enumerate() {
        if !calls.iter().any(|(c, _)| c.winref == i + 1) {
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
        let frame = &window.frame;
        frame.start.iter().chain(&frame.end).try_for_each(crate::check)?;
        for func in [frame.start_in_range, frame.end_in_range] {
            if func != 0 {
                kernel(func, None)?;
            }
        }
    }
    Ok(())
}

/// The rows that the windows read: a tuple of the scan for each row and, in a query with groups, the group of each row.
#[derive(Clone, Copy)]
pub(crate) struct Input<'t> {
    pub(crate) tuples: &'t [&'t [usize]],
    pub(crate) groups: &'t [Group],
}

impl<'t> Input<'t> {
    /// The state that evaluates an expression on a row.
    pub(crate) fn at<'b>(&self, eval: &'b Eval<'_>, row: usize) -> Eval<'b>
    where
        't: 'b,
    {
        let mut e = eval.at(self.tuples[row]);
        if let Some(group) = self.groups.get(row) {
            e.group.clone_from(group);
        }
        e
    }
}

/// The value of each window function call for each row, by the address of the call, and the order of the rows after the last sort of the windows.
pub(crate) type Values = Vec<Vec<(*const WindowFunc, Value)>>;

/// Computes the window function calls of a query for its rows, as the `WindowAgg` nodes do. The result has the order of the rows after the windows, and the values of the calls for each row.
pub(crate) fn compute(
    eval: &Eval<'_>,
    query: &Query,
    calls: &[(&WindowFunc, u32)],
    input: Input<'_>,
    session: &dyn Session,
) -> Result<(Vec<usize>, Values)> {
    let count = input.tuples.len();
    let actives = active(query, calls)?;
    // Each `WindowAgg` node computes the offsets of its frame before it reads a row, and the node of the last window is at the top of the plan.
    let mut offsets = vec![[Value::Null, Value::Null]; actives.len()];
    for (slot, (_, window)) in offsets.iter_mut().zip(&actives).rev() {
        *slot = frame_offsets(eval, &window.frame)?;
    }
    // The values of the targets that the windows sort on, for each row.
    let mut used = vec![false; query.targets.len()];
    for (_, window) in &actives {
        for item in window.partition.iter().chain(&window.order) {
            used[item.target] = true;
        }
    }
    let mut rows = Vec::with_capacity(count);
    for row in 0..count {
        let mut e = input.at(eval, row);
        let row = query
            .targets
            .iter()
            .zip(&used)
            .map(|(t, used)| if *used { e.eval(&t.expr) } else { Ok(Value::Null) })
            .collect::<Result<Vec<_>>>()?;
        rows.push(row);
    }
    let types: Vec<u32> = query.targets.iter().map(|t| t.expr.ty).collect();
    let mut order: Vec<usize> = (0..count).collect();
    let mut current: Vec<SortGroup> = Vec::new();
    let mut values: Values = vec![Vec::new(); count];
    for ((winref, window), offsets) in actives.into_iter().zip(&offsets) {
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
        let mine: Vec<(&WindowFunc, u32)> =
            calls.iter().copied().filter(|(c, _)| c.winref == winref).collect();
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
            // The frame of each row, when a call uses it.
            let framed = |(call, _): &&(&WindowFunc, u32)| {
                call.agg
                    || matches!(
                        method(call.oid, None),
                        Ok(Method::FirstValue | Method::LastValue | Method::NthValue)
                    )
            };
            let bounds = if mine.iter().any(|c| framed(&c)) {
                let ranged = Ranged::new(query, window, session)?;
                let keys: Vec<&Value> = match &ranged {
                    Some(ranged) => part.iter().map(|&row| &rows[row][ranged.target]).collect(),
                    None => Vec::new(),
                };
                frames(&window.frame, offsets, &groups, ranged.as_ref(), &keys)?
            } else {
                Vec::new()
            };
            for &(call, ty) in &mine {
                let results = if call.agg {
                    agg::window_values(eval, call, ty, part, &bounds, input)?
                } else {
                    let method = method(call.oid, None)?;
                    partition_values(eval, call, method, part, &groups, &bounds, input)?
                };
                for (&row, value) in part.iter().zip(results) {
                    values[row].push((std::ptr::from_ref(call), value));
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

/// The value of an argument of a call on a row.
fn arg(
    eval: &Eval<'_>,
    call: &WindowFunc,
    n: usize,
    input: Input<'_>,
    row: usize,
) -> Result<Value> {
    let expr =
        call.args.get(n).ok_or_else(|| Error::internal("a window function with no argument"))?;
    input.at(eval, row).eval(expr)
}

/// The value of an argument of type `integer` on a row, or `None` for a null.
fn int4_arg(
    eval: &Eval<'_>,
    call: &WindowFunc,
    n: usize,
    input: Input<'_>,
    row: usize,
) -> Result<Option<i32>> {
    match arg(eval, call, n, input, row)? {
        Value::Null => Ok(None),
        Value::Int4(v) => Ok(Some(v)),
        _ => Err(Error::internal("an argument of a window function that is not integer")),
    }
}

/// The values of a call for the rows of one partition. `groups` has the start and the end of the peer group of each row, and `frames` the frame of each row for `first_value`, `last_value` and `nth_value`.
fn partition_values(
    eval: &Eval<'_>,
    call: &WindowFunc,
    method: Method,
    part: &[usize],
    groups: &[(usize, usize)],
    frames: &[Bounds],
    input: Input<'_>,
) -> Result<Vec<Value>> {
    let n = part.len();
    let mut out = Vec::with_capacity(n);
    // `WinCheckAndInitializeNullTreatment`: only these functions take the clause, and the check runs when the function runs.
    let takes_nulls = matches!(
        method,
        Method::LeadLag(_) | Method::FirstValue | Method::LastValue | Method::NthValue
    );
    if call.nulls != NullTreatment::None && !takes_nulls {
        let name = builtin::proc_by_oid(call.oid).map_or("", |p| p.name);
        return Err(Error::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            format!("function {name} does not allow RESPECT/IGNORE NULLS"),
        ));
    }
    // With `IGNORE NULLS`, the value of the argument on each row of the partition, which the search for the rows that are not null reads.
    let values = if call.nulls == NullTreatment::Ignore {
        part.iter().map(|&row| arg(eval, call, 0, input, row)).collect::<Result<Vec<_>>>()?
    } else {
        Vec::new()
    };
    let ignore = !values.is_empty();
    // The `count`-th row of `rows` where the argument is not null, from 1, as `ignorenulls_getfuncarginframe`.
    let nth_not_null = |rows: &mut dyn Iterator<Item = usize>, count: usize| {
        rows.filter(|&at| !values[at].is_null()).nth(count - 1).map(|at| values[at].clone())
    };
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
            for &row in part {
                if state.is_none() {
                    let Some(buckets) = int4_arg(eval, call, 0, input, row)? else {
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
                    match int4_arg(eval, call, 1, input, part[i])? {
                        Some(offset) => i64::from(offset),
                        None => {
                            out.push(Value::Null);
                            continue;
                        }
                    }
                } else {
                    1
                };
                if ignore && offset != 0 {
                    // `WinGetFuncArgInPartition`: the row that is the `offset`-th row with a value that is not null, from the current row.
                    let ahead = forward == (offset > 0);
                    let count = usize::try_from(offset.unsigned_abs()).unwrap_or(usize::MAX);
                    let found = if ahead {
                        nth_not_null(&mut (i + 1..n), count)
                    } else {
                        nth_not_null(&mut (0..i).rev(), count)
                    };
                    out.push(match found {
                        Some(value) => value,
                        None if call.args.len() > 2 => arg(eval, call, 2, input, part[i])?,
                        None => Value::Null,
                    });
                    continue;
                }
                let at = i64::try_from(i).unwrap_or(i64::MAX);
                let target = if forward { at + offset } else { at - offset };
                let value = match usize::try_from(target).ok().filter(|t| *t < n) {
                    Some(t) => arg(eval, call, 0, input, part[t])?,
                    None if call.args.len() > 2 => arg(eval, call, 2, input, part[i])?,
                    None => Value::Null,
                };
                out.push(value);
            }
        }
        Method::FirstValue if ignore => {
            for bounds in frames {
                out.push(nth_not_null(&mut bounds.rows(), 1).unwrap_or(Value::Null));
            }
        }
        Method::LastValue if ignore => {
            for bounds in frames {
                let rows: Vec<usize> = bounds.rows().collect();
                out.push(nth_not_null(&mut rows.into_iter().rev(), 1).unwrap_or(Value::Null));
            }
        }
        Method::FirstValue => {
            for bounds in frames {
                out.push(match bounds.rows().next() {
                    Some(at) => arg(eval, call, 0, input, part[at])?,
                    None => Value::Null,
                });
            }
        }
        Method::LastValue => {
            for bounds in frames {
                out.push(match bounds.rows().last() {
                    Some(at) => arg(eval, call, 0, input, part[at])?,
                    None => Value::Null,
                });
            }
        }
        Method::NthValue => {
            for (i, bounds) in frames.iter().enumerate() {
                let Some(nth) = int4_arg(eval, call, 1, input, part[i])? else {
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
                if ignore {
                    out.push(nth_not_null(&mut bounds.rows(), at + 1).unwrap_or(Value::Null));
                    continue;
                }
                out.push(match bounds.rows().nth(at) {
                    Some(at) => arg(eval, call, 0, input, part[at])?,
                    None => Value::Null,
                });
            }
        }
    }
    Ok(out)
}

/// The rows of the frame of a row, as indexes in the partition: the rows from `head` to `tail` that the clause `EXCLUDE` does not remove.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Bounds {
    pub(crate) head: usize,
    pub(crate) tail: usize,
    /// The first row and the end of the rows that the exclusion removes.
    skip: (usize, usize),
    /// The current row with `EXCLUDE TIES`, which stays in the frame.
    keep: Option<usize>,
}

impl Bounds {
    /// True when the exclusion removes rows.
    pub(crate) fn excludes(&self) -> bool {
        self.skip.0 < self.skip.1
    }

    /// The rows of the frame in their order.
    pub(crate) fn rows(&self) -> impl Iterator<Item = usize> {
        let b = *self;
        (b.head..b.tail).filter(move |&at| at < b.skip.0 || at >= b.skip.1 || b.keep == Some(at))
    }
}

/// `calculate_frame_offsets`: the offsets of the start and the end of a frame, or nulls for the bounds with no offset.
fn frame_offsets(eval: &Eval<'_>, frame: &Frame) -> Result<[Value; 2]> {
    let mut out = [Value::Null, Value::Null];
    for (slot, (expr, which)) in
        out.iter_mut().zip([(&frame.start, "starting"), (&frame.end, "ending")])
    {
        let Some(expr) = expr else { continue };
        let value = eval.at(&[]).eval(expr)?;
        if value == Value::Null {
            return Err(Error::new(
                SqlState::NULL_VALUE_NOT_ALLOWED,
                format!("frame {which} offset must not be null"),
            ));
        }
        if frame.mode() != FrameMode::Range && value.as_i64().is_some_and(|v| v < 0) {
            return Err(Error::new(
                SqlState::INVALID_PRECEDING_OR_FOLLOWING_SIZE,
                format!("frame {which} offset must not be negative"),
            ));
        }
        *slot = value;
    }
    Ok(out)
}

/// The `in_range` function of a bound of a frame in `RANGE` mode with an offset, and the type of the offset.
type InRange = Option<(Kernel, u32)>;

/// The `in_range` functions of a frame in `RANGE` mode with an offset, and the column of `ORDER BY` that they compare.
struct Ranged<'s> {
    target: usize,
    ty: u32,
    start: InRange,
    end: InRange,
    asc: bool,
    nulls_first: bool,
    session: &'s dyn Session,
}

impl<'s> Ranged<'s> {
    fn new(query: &Query, window: &WindowClause, session: &'s dyn Session) -> Result<Option<Self>> {
        let frame = &window.frame;
        if frame.mode() != FrameMode::Range || (frame.start.is_none() && frame.end.is_none()) {
            return Ok(None);
        }
        let Some(item) = window.order.first() else {
            return Err(Error::internal("a frame in RANGE mode with no column to order"));
        };
        let func = |f: u32, offset: &Option<Expr>| -> Result<InRange> {
            match offset {
                Some(offset) if f != 0 => Ok(Some((kernel(f, None)?, offset.ty))),
                _ => Ok(None),
            }
        };
        Ok(Some(Ranged {
            target: item.target,
            ty: query.targets[item.target].expr.ty,
            start: func(frame.start_in_range, &frame.start)?,
            end: func(frame.end_in_range, &frame.end)?,
            asc: frame.in_range_asc,
            nulls_first: frame.in_range_nulls_first,
            session,
        }))
    }

    /// The call of an `in_range` function on the value of a row, the value of the current row and the offset.
    fn test(
        &self,
        func: InRange,
        val: &Value,
        base: &Value,
        offset: &Value,
        sub: bool,
        less: bool,
    ) -> Result<bool> {
        let (func, ty) =
            func.ok_or_else(|| Error::internal("a frame with no in_range function"))?;
        let types = [self.ty, self.ty, ty, oid::BOOL, oid::BOOL];
        let call = Call {
            session: self.session,
            args: &types,
            ret: oid::BOOL,
            variadic: false,
            want: None,
        };
        let args = [val.clone(), base.clone(), offset.clone(), Value::Bool(sub), Value::Bool(less)];
        Ok(func(&call, &args)? == Value::Bool(true))
    }
}

/// The frame of each row of a partition, as `update_frameheadpos`, `update_frametailpos` and `row_is_in_frame` find it. `groups` has the peer group of each row, and `keys` the value of the column of `ORDER BY` of each row in `RANGE` mode with an offset.
fn frames(
    frame: &Frame,
    offsets: &[Value; 2],
    groups: &[(usize, usize)],
    ranged: Option<&Ranged<'_>>,
    keys: &[&Value],
) -> Result<Vec<Bounds>> {
    let n = groups.len();
    // The number of the peer group of each row, and the first row of each group.
    let mut starts: Vec<usize> = Vec::new();
    let mut number: Vec<usize> = Vec::with_capacity(n);
    for (i, g) in groups.iter().enumerate() {
        if g.0 == i {
            starts.push(i);
        }
        number.push(starts.len() - 1);
    }
    let int8 = |v: &Value| i128::from(v.as_i64().unwrap_or(0));
    let clamp = |v: i128| usize::try_from(v.clamp(0, n as i128)).unwrap_or(n);
    // The first row of the first group after the group `g`, or of the group `g`, as a number of a group.
    let group_start = |g: i128| match usize::try_from(g) {
        Ok(g) => starts.get(g).copied().unwrap_or(n),
        Err(_) => 0,
    };
    let mode = frame.mode();
    let (mut head, mut tail) = (0, 0);
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let current = i as i128;
        let start = frame.start_bound();
        let first = match (start, mode) {
            (FrameBound::UnboundedPreceding, _) => 0,
            (FrameBound::UnboundedFollowing, _) => n,
            (FrameBound::CurrentRow, FrameMode::Rows) => i,
            (FrameBound::CurrentRow, _) => groups[i].0,
            (_, FrameMode::Rows) => {
                let offset = int8(&offsets[0]);
                clamp(if start == FrameBound::Preceding {
                    current - offset
                } else {
                    current + offset
                })
            }
            (_, FrameMode::Groups) => {
                let (group, offset) = (number[i] as i128, int8(&offsets[0]));
                group_start(if start == FrameBound::Preceding {
                    group - offset
                } else {
                    group + offset
                })
            }
            (_, FrameMode::Range) => {
                let ranged =
                    ranged.ok_or_else(|| Error::internal("a frame with no in_range function"))?;
                let (mut sub, mut less) = (start == FrameBound::Preceding, false);
                if !ranged.asc {
                    (sub, less) = (!sub, true);
                }
                while head < n {
                    let (value, base) = (keys[head], keys[i]);
                    let (null, base_null) = (*value == Value::Null, *base == Value::Null);
                    if null || base_null {
                        let stop = if ranged.nulls_first {
                            !null || base_null
                        } else {
                            null || !base_null
                        };
                        if stop {
                            break;
                        }
                    } else if ranged.test(ranged.start, value, base, &offsets[0], sub, less)? {
                        break;
                    }
                    head += 1;
                }
                head
            }
        };
        let end = frame.end_bound();
        let last = match (end, mode) {
            (FrameBound::UnboundedFollowing, _) => n,
            (FrameBound::UnboundedPreceding, _) => 0,
            (FrameBound::CurrentRow, FrameMode::Rows) => i + 1,
            (FrameBound::CurrentRow, _) => groups[i].1,
            (_, FrameMode::Rows) => {
                let offset = int8(&offsets[1]);
                clamp(if end == FrameBound::Preceding {
                    current - offset + 1
                } else {
                    current + offset + 1
                })
            }
            (_, FrameMode::Groups) => {
                let (group, offset) = (number[i] as i128, int8(&offsets[1]));
                group_start(if end == FrameBound::Preceding {
                    group - offset + 1
                } else {
                    group + offset + 1
                })
            }
            (_, FrameMode::Range) => {
                let ranged =
                    ranged.ok_or_else(|| Error::internal("a frame with no in_range function"))?;
                let (mut sub, mut less) = (end == FrameBound::Preceding, true);
                if !ranged.asc {
                    (sub, less) = (!sub, false);
                }
                while tail < n {
                    let (value, base) = (keys[tail], keys[i]);
                    let (null, base_null) = (*value == Value::Null, *base == Value::Null);
                    if null || base_null {
                        let stop = if ranged.nulls_first { !null } else { !base_null };
                        if stop {
                            break;
                        }
                    } else if !ranged.test(ranged.end, value, base, &offsets[1], sub, less)? {
                        break;
                    }
                    tail += 1;
                }
                tail
            }
        };
        let (skip, keep) = match frame.exclusion() {
            FrameExclusion::NoOthers => ((0, 0), None),
            FrameExclusion::CurrentRow => ((i, i + 1), None),
            FrameExclusion::Group => (groups[i], None),
            FrameExclusion::Ties => (groups[i], Some(i)),
        };
        out.push(Bounds { head: first, tail: last, skip, keep });
    }
    Ok(out)
}
