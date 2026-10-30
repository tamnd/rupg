//! `set_subquery_pathlist`: the conditions of `WHERE` on one subquery of `FROM` that the planner moves into the subquery before it plans the subquery, as `subquery_push_qual` moves them.
//!
//! A condition that moves reads the targets of the subquery in place of its columns. It joins the `WHERE` of the subquery, or its `HAVING` when the subquery makes groups, and the query above does not test it again.

use std::collections::BTreeSet;

use rupg_analyze::{
    BoolOp, BoolTest, Expr, ExprKind, FromItem, FuncForm, JoinKind, Query, Relation, SetKind,
    SetOp, SetTree, WindowFunc,
};
use rupg_pgcatalog::builtin;
use rupg_types::oid;

use crate::{exprs, from, pullup, upper};

/// `preprocess_expression` for a condition that moves into a subquery: the subquery folds its conditions when the planner plans it.
pub type FoldQual<'a> = &'a dyn Fn(&Query, Expr) -> Expr;

/// Moves the conditions of `WHERE` that the planner can move into the subqueries of `FROM`, then does the same in each subquery, also in the subqueries of the expressions. `fold` folds a condition in the subquery that it moves into.
pub(crate) fn push_all(query: &mut Query, barrier: &dyn Fn(u32) -> bool, fold: FoldQual<'_>) {
    push(query, barrier, fold);
    for relation in &mut query.relations {
        if let Some(sub) = &mut relation.subquery {
            push_all(sub, barrier, fold);
        }
    }
    for expr in query.exprs_mut() {
        expr.each_mut(0, &mut |e, depth| {
            if depth == 0
                && let ExprKind::SubLink(link) = &mut e.kind
            {
                push_all(&mut link.query, barrier, fold);
            }
            false
        });
    }
}

/// Reduces the outer joins of `FROM` and removes the tests of `IS NULL` that an anti join makes redundant, then moves each part of `WHERE` that reads one subquery of `FROM` into the subquery, when `qual_is_pushdown_safe` lets it. The subquery is not on the side of an outer join that can give nulls, where the condition tests the rows of the join. Only a leakproof condition moves into a view with `security_barrier`, for which `barrier` is true. The planner does not move a condition into a query of `WITH` yet.
fn push(query: &mut Query, barrier: &dyn Fn(u32) -> bool, fold: FoldQual<'_>) {
    let mut above = BTreeSet::new();
    for part in query.filter.iter().chain(upper::having_split(query).0) {
        above.extend(from::nonnullable_rels(part, true));
    }
    let forced = query.filter.as_ref().map(forced_null_vars).unwrap_or_default();
    let Query { from, relations, filter, .. } = query;
    let mut anti = BTreeSet::new();
    for item in from.iter_mut() {
        reduce(item, &above, &forced, relations);
        anti.extend(strip(item));
    }
    if !anti.is_empty() {
        *filter = filter.take().and_then(|f| without_null_tests(f, &anti));
    }
    let Some(filter) = query.filter.take() else { return };
    let mut nullable = Vec::new();
    for item in &query.from {
        pullup::nullable_places(item, false, &mut nullable);
    }
    let mut parts = Vec::new();
    exprs::and_parts(&filter, &mut parts);
    let to: Vec<Option<usize>> = parts
        .iter()
        .map(|part| {
            let rels = exprs::relations(part);
            // `distribute_qual_to_rels`: a volatile condition that reads no relation is a condition of the relations of its level.
            let (r, outer) = match (rels.len(), query.from.as_slice()) {
                (1, _) => (*rels.first()?, BTreeSet::new()),
                (0, [FromItem::Relation(r)]) if exprs::volatile(part) => (*r, BTreeSet::new()),
                _ => lateral_union(query, &rels)?,
            };
            let relation = query.relations.get(r)?;
            let view = relation.oid != 0 && (!barrier(relation.oid) || !part.leaky());
            let plain =
                (relation.oid == 0 || view) && relation.cte.is_none() && !nullable.contains(&r);
            let sub = relation.subquery.as_deref().filter(|_| plain)?;
            pushable(sub, part, r, &outer).then_some(r)
        })
        .collect();
    let windows =
        query.relations.iter().any(|r| r.subquery.as_ref().is_some_and(|s| !s.windows.is_empty()));
    if to.iter().all(Option::is_none) && !windows {
        query.filter = Some(filter);
        return;
    }
    let mut owned = Vec::new();
    owned_parts(filter, &mut owned);
    let mut keep = Vec::new();
    for (part, to) in owned.into_iter().zip(to) {
        if let Some(r) = to
            && let Some(sub) = query.relations[r].subquery.as_deref_mut()
        {
            push_qual(sub, r, part, Some(fold));
        } else if !push_run_condition(query, &part, &nullable) {
            keep.push(part);
        }
    }
    query.filter = and_all(keep);
}

/// The case `PUSHDOWN_WINDOWCLAUSE_RUNCOND` of `set_subquery_pathlist`: a part of `WHERE` that cannot move into the subquery that it reads, because it reads a call of a window function, can be a run condition of the window of the call. The result is true when the run condition does all the work of the part, so that the query does not test the part again.
fn push_run_condition(query: &mut Query, part: &Expr, nullable: &[usize]) -> bool {
    let rels = exprs::relations(part);
    let [r] = rels.iter().copied().collect::<Vec<_>>()[..] else { return false };
    let Some(relation) = query.relations.get_mut(r) else { return false };
    if relation.cte.is_some() || nullable.contains(&r) {
        return false;
    }
    let Some(sub) = relation.subquery.as_deref_mut() else { return false };
    if sub.windows.is_empty() || sub.set_op.is_some() {
        return false;
    }
    // `qual_is_pushdown_safe` gives `PUSHDOWN_WINDOWCLAUSE_RUNCOND` only when a column that is not an item of `PARTITION BY` is the reason that the part stays above.
    let Some((columns, volatile)) = safety_of(sub, false) else { return false };
    if !safe(part, r, &BTreeSet::new(), &columns, volatile) {
        return false;
    }
    match run_condition(sub, part, r) {
        Some((window, condition, keep)) => {
            sub.windows[window].run.push(condition);
            !keep
        }
        None => false,
    }
}

/// `check_and_push_window_quals` and `find_window_run_conditions`: for a strict operator of two arguments, one a column of the subquery `r` that is a call of a window function and the other a value that does not change in the partition, the window of the call, the run condition, and true when the query must also test the part. The support function of the window function says that the value of the call only goes up or only goes down in a partition. A condition `<` or `<=` on a value that goes up, or `>` or `>=` on a value that goes down, is true for the first rows of the partition and then false, so it does all the work of the part. An equality on a value that goes in one direction gives a condition `<=` or `>=`, and the query still tests the equality.
fn run_condition(sub: &Query, part: &Expr, r: usize) -> Option<(usize, Expr, bool)> {
    let ExprKind::Func(op) = &part.kind else { return None };
    let FuncForm::Operator(opno) = op.form else { return None };
    if op.args.len() != 2 || !builtin::proc_by_oid(op.oid).is_some_and(|p| p.strict) {
        return None;
    }
    for (side, wfunc_left) in [(0, true), (1, false)] {
        let ExprKind::Var(v) = &op.args[side].kind else { continue };
        if v.levels_up != 0 || v.relation != r || v.attnum <= 0 {
            continue;
        }
        let target = usize::try_from(v.attnum - 1).ok().and_then(|at| sub.targets.get(at))?;
        let mut call = &target.expr;
        while let ExprKind::Relabel(inner, _) = &call.kind {
            call = inner;
        }
        let ExprKind::Window(w) = &call.kind else { continue };
        if let Some(found) = window_run_condition(sub, call, w, part, opno, wfunc_left) {
            return Some(found);
        }
    }
    None
}

/// `MONOTONICFUNC_INCREASING`.
const INCREASING: u8 = 1;
/// `MONOTONICFUNC_DECREASING`.
const DECREASING: u8 = 2;

/// `SupportRequestWFuncMonotonic`: the directions in which the value of a call of a window function can go in a partition, as the support function in `prosupport` of the function gives them. The support functions of `row_number` and the other functions of the rank of a row, and `int8inc_support` of `count` and of an aggregate that `CREATE AGGREGATE` gives that support function, are known. `None` for another function.
fn monotonic(w: &WindowFunc, frame: i32, ordered: bool) -> Option<u8> {
    let support = builtin::proc_by_oid(builtin::proc_by_oid(w.oid)?.support)?;
    match support.name {
        "window_row_number_support"
        | "window_rank_support"
        | "window_dense_rank_support"
        | "window_percent_rank_support"
        | "window_cume_dist_support"
        | "window_ntile_support" => Some(INCREASING),
        "int8inc_support" => {
            // `FRAMEOPTION_EXCLUSION`: only `EXCLUDE CURRENT ROW` of `count(*)` with no filter keeps the direction.
            if frame & 0x38000 != 0
                && (frame & 0x8000 == 0 || w.oid != COUNT_STAR || w.filter.is_some())
            {
                return Some(0);
            }
            // With no `ORDER BY` in `RANGE` mode all rows are peers.
            if !ordered && frame & 0x2 != 0 {
                return Some(INCREASING | DECREASING);
            }
            let mut out = 0;
            if frame & 0x20 != 0 {
                out |= INCREASING;
            }
            if frame & 0x100 != 0 {
                out |= DECREASING;
            }
            Some(out)
        }
        _ => None,
    }
}

/// `find_window_run_conditions` for the call `call` of the window function `w`: the window of the call, the run condition, and true when the query must also test the part.
fn window_run_condition(
    sub: &Query,
    call: &Expr,
    w: &WindowFunc,
    part: &Expr,
    opno: u32,
    wfunc_left: bool,
) -> Option<(usize, Expr, bool)> {
    let ExprKind::Func(op) = &part.kind else { return None };
    // `contain_subplans`: a link that came from a subquery of the subquery is still a column there.
    let mut link = |e: &Expr, _| matches!(&e.kind, ExprKind::SubLink(l) if !l.pulled).then_some(());
    if call.find(0, &mut link).is_some() {
        return None;
    }
    // `is_pseudo_constant_clause`: the other side reads no column and calls no volatile function.
    let other = &op.args[usize::from(wfunc_left)];
    let reads_column = other
        .find(0, &mut |e, _| {
            matches!(e.kind, ExprKind::Var(_) | ExprKind::SubLink(_)).then_some(())
        })
        .is_some();
    if reads_column || exprs::volatile(other) {
        return None;
    }
    let window = w.winref.checked_sub(1)?;
    let clause = sub.windows.get(window)?;
    let monotonic = monotonic(w, clause.frame.options, !clause.order.is_empty())?;
    if monotonic == 0 {
        return None;
    }
    // `get_op_index_interpretation`: the strategy of the operator in its first family of `btree`.
    let amop =
        builtin::amops_of_operator(opno).find(|a| a.method == BTREE_AM && a.purpose == b's')?;
    let (up, down) = (monotonic & INCREASING != 0, monotonic & DECREASING != 0);
    let (operator, keep) = match amop.strategy {
        1 | 2 if (wfunc_left && up) || (!wfunc_left && down) => (opno, false),
        4 | 5 if (wfunc_left && down) || (!wfunc_left && up) => (opno, false),
        3 if up && down => (opno, false),
        3 => {
            let strategy = if up == wfunc_left { 2 } else { 4 };
            let operator = builtin::opfamily_member(amop.family, amop.left, amop.right, strategy)?;
            (operator, true)
        }
        _ => return None,
    };
    let code = builtin::operator_by_oid(operator)?.code;
    let mut args = op.args.clone();
    args[usize::from(!wfunc_left)] = call.clone();
    let mut func = op.clone();
    func.oid = code;
    func.form = FuncForm::Operator(operator);
    func.args = args;
    Some((window, Expr::new(ExprKind::Func(func), part.ty), keep))
}

/// `BTREE_AM_OID`.
const BTREE_AM: u32 = 403;

/// `F_COUNT_`: `count(*)`.
const COUNT_STAR: u32 = 2803;

/// `set_subquery_pathlist` for the executor: each part of `WHERE` that reads only a view with `security_barrier` of the list of `FROM`, or a volatile part that reads no relation when the view is the only item of `FROM`, moves into the view when it is leakproof, as `qual_is_pushdown_safe` lets such a condition pass the barrier. `barrier` is true for a view with `security_barrier`. The other parts stay above the view.
pub(crate) fn push_barrier(query: &mut Query, barrier: &dyn Fn(u32) -> bool) {
    let Some(filter) = query.filter.take() else { return };
    let mut owned = Vec::new();
    owned_parts(filter, &mut owned);
    let mut keep = Vec::new();
    for part in owned {
        let rels = exprs::relations(&part);
        // A volatile condition that reads no relation is a condition of the one relation of `FROM`, as in [`push`].
        let only = match query.from.as_slice() {
            [FromItem::Relation(r)] if rels.is_empty() && exprs::volatile(&part) => Some(*r),
            _ => None,
        };
        let to = rels.first().copied().or(only).filter(|&r| {
            let relation = &query.relations[r];
            rels.len() <= 1
                && query.from.iter().any(|item| matches!(item, FromItem::Relation(k) if *k == r))
                && relation.cte.is_none()
                && !relation.lateral
                && relation.oid != 0
                && barrier(relation.oid)
                && !part.leaky()
                && relation
                    .subquery
                    .as_deref()
                    .is_some_and(|sub| pushable(sub, &part, r, &BTreeSet::new()))
        });
        match to.and_then(|r| query.relations[r].subquery.as_deref_mut().map(|sub| (r, sub))) {
            Some((r, sub)) => push_qual(sub, r, part, None),
            None => keep.push(part),
        }
    }
    query.filter = and_all(keep);
}

/// The `AND` of the conditions, or `None` when there is no condition.
pub(crate) fn and_all(mut parts: Vec<Expr>) -> Option<Expr> {
    match parts.len() {
        0 => None,
        1 => parts.pop(),
        _ => Some(Expr::new(ExprKind::Bool(BoolOp::And, parts), oid::BOOL)),
    }
}

/// `find_forced_null_vars`: the columns that each row that passes the condition has as null, from the parts of `AND` that are `x IS NULL` or `x IS UNKNOWN`.
fn forced_null_vars(expr: &Expr) -> BTreeSet<(usize, i16)> {
    let mut parts = Vec::new();
    exprs::and_parts(expr, &mut parts);
    parts.into_iter().filter_map(forced_null_var).collect()
}

/// `find_forced_null_var`: the column of a condition that is `x IS NULL` or `x IS UNKNOWN`, with no other test. A whole row does not count, because `IS NULL` of a row tests each field.
fn forced_null_var(expr: &Expr) -> Option<(usize, i16)> {
    let (ExprKind::NullTest(arg, true) | ExprKind::BooleanTest(arg, BoolTest::IsUnknown)) =
        &expr.kind
    else {
        return None;
    };
    let ExprKind::Var(var) = &arg.kind else { return None };
    (var.levels_up == 0 && var.attnum != 0).then_some((var.relation, var.attnum))
}

/// `remove_redundant_nullability_quals`: removes each test of `IS NULL` of a column of the inner side of an anti join from the conditions of `ON` above the join, and gives the relations of the inner sides of the anti joins of the item, for the tests of `WHERE`. The inner side of an anti join gives no row, so the test was true for each row that the join keeps.
fn strip(item: &mut FromItem) -> BTreeSet<usize> {
    let FromItem::Join(join) = item else { return BTreeSet::new() };
    let mut below = strip(&mut join.left);
    below.extend(strip(&mut join.right));
    if !below.is_empty() {
        join.on = join.on.take().and_then(|on| without_null_tests(on, &below));
    }
    if join.kind == JoinKind::Anti {
        let mut out = Vec::new();
        rels(&join.right, &mut out);
        below.extend(out);
    }
    below
}

/// `strip_redundant_nullability_quals`: the condition without the parts that test `IS NULL` of a column of the relations `anti`.
fn without_null_tests(expr: Expr, anti: &BTreeSet<usize>) -> Option<Expr> {
    let mut parts = Vec::new();
    owned_parts(expr, &mut parts);
    parts.retain(|p| forced_null_var(p).is_none_or(|(r, _)| !anti.contains(&r)));
    and_all(parts)
}

/// `reduce_outer_joins`: an outer join is an inner join, and a full join is a left join or a right join, when a condition above it is false or null for each row with nulls for the relations of a side that can give nulls, as `from::reduce` finds it. The planner reduces the joins before it moves the conditions of `WHERE`, so that a condition that moves still makes the join an inner join.
///
/// A left join is an anti join when a condition above it tests `IS NULL` of a column of the nullable side, `forced`, that cannot be null in a row that the join matches. The condition of `ON` is false or null when the column is null, or the column is a column of a table with `NOT NULL` and no outer join below gives nulls for it. Then only the rows with no match pass the test. A right join that becomes an anti join has its sides in the other order.
fn reduce(
    item: &mut FromItem,
    above: &BTreeSet<usize>,
    forced: &BTreeSet<(usize, i16)>,
    relations: &[Relation],
) {
    let FromItem::Join(join) = item else { return };
    let join = &mut **join;
    let overlaps = |side: &FromItem| {
        let mut out = Vec::new();
        rels(side, &mut out);
        out.iter().any(|r| above.contains(r))
    };
    let (left, right) = (overlaps(&join.left), overlaps(&join.right));
    join.kind = match join.kind {
        JoinKind::Left if right => JoinKind::Inner,
        JoinKind::Right if left => JoinKind::Inner,
        JoinKind::Full if left && right => JoinKind::Inner,
        JoinKind::Full if left => JoinKind::Left,
        JoinKind::Full if right => JoinKind::Right,
        kind => kind,
    };
    if matches!(join.kind, JoinKind::Left | JoinKind::Right) && !forced.is_empty() {
        let inner = if join.kind == JoinKind::Left { &join.right } else { &join.left };
        let mut inside = Vec::new();
        rels(inner, &mut inside);
        let mut nullable = Vec::new();
        pullup::nullable_places(inner, false, &mut nullable);
        let strict = join.on.as_ref().map(|on| pullup::nonnullable_vars(on, true));
        let anti = forced.iter().any(|&(r, attnum)| {
            inside.contains(&r)
                && (strict.as_ref().is_some_and(|s| s.contains(&(r, attnum)))
                    || !nullable.contains(&r) && pullup::column_not_null(relations, r, attnum))
        });
        if anti {
            if join.kind == JoinKind::Right {
                std::mem::swap(&mut join.left, &mut join.right);
            }
            join.kind = JoinKind::Anti;
        }
    }
    let mut local = BTreeSet::new();
    let mut local_forced = BTreeSet::new();
    if join.kind != JoinKind::Full
        && let Some(on) = &join.on
    {
        local = from::nonnullable_rels(on, true);
        local_forced = forced_null_vars(on);
    }
    let (left, right) = match join.kind {
        JoinKind::Inner | JoinKind::Semi | JoinKind::RightSemi => {
            local.extend(above.iter().copied());
            local_forced.extend(forced.iter().copied());
            ((local.clone(), local_forced.clone()), (local, local_forced))
        }
        JoinKind::Left | JoinKind::Anti => ((above.clone(), forced.clone()), (local, local_forced)),
        JoinKind::Right | JoinKind::RightAnti => {
            ((local, local_forced), (above.clone(), forced.clone()))
        }
        JoinKind::Full => Default::default(),
    };
    reduce(&mut join.left, &left.0, &left.1, relations);
    reduce(&mut join.right, &right.0, &right.1, relations);
}

/// The relations of an item of `FROM`.
pub(crate) fn rels(item: &FromItem, out: &mut Vec<usize>) {
    match item {
        FromItem::Relation(r) => out.push(*r),
        FromItem::Join(join) => {
            rels(&join.left, out);
            rels(&join.right, out);
        }
    }
}

/// The parts of a condition that `AND` joins, as [`exprs::and_parts`] finds them.
pub(crate) fn owned_parts(expr: Expr, out: &mut Vec<Expr>) {
    match expr.kind {
        ExprKind::Bool(BoolOp::And, args) => args.into_iter().for_each(|a| owned_parts(a, out)),
        kind => out.push(Expr { kind, ..expr }),
    }
}

/// True when the condition `part`, which reads the subquery `r`, can move into the subquery. A `UNION ALL` that `pull_up_simple_union_all` makes an append relation gives the condition to each member, as `set_append_rel_size`: a member that `pull_up_simple_subquery` merges takes it as it is, and another member takes it when `set_subquery_pathlist` lets it. The planner does not give the condition to only some members.
fn pushable(sub: &Query, part: &Expr, r: usize, outer: &BTreeSet<usize>) -> bool {
    let fits = |q: &Query| {
        safety(q).is_some_and(|(columns, volatile)| safe(part, r, outer, &columns, volatile))
            && !grouping_conflict(q, part, r)
    };
    let Some(op) = sub.set_op.as_ref().filter(|_| from::simple_union_all(sub)) else {
        return fits(sub);
    };
    let mut all = true;
    let mut each = |at: usize| {
        let Some(leaf) = sub.relations.get(at).and_then(|l| l.subquery.as_deref()) else {
            all = false;
            return;
        };
        all &= if pullup::member(leaf) {
            safe(part, r, outer, &vec![false; leaf.targets.len()], false)
        } else {
            fits(leaf)
        };
    };
    op.left.each_leaf(&mut each);
    op.right.each_leaf(&mut each);
    all
}

/// `get_baserel_parampathinfo` and `set_append_rel_size`: for a part of `WHERE` that reads more than one relation, the `UNION ALL` subquery with `LATERAL` that the part reads, with the other relations that the part reads, when the subquery reads all of them. The planner joins such a subquery as the inner side of a nested loop, and each member of its append relation tests the part with the columns of the outer side as parameters.
fn lateral_union(query: &Query, rels: &BTreeSet<usize>) -> Option<(usize, BTreeSet<usize>)> {
    let mut found = rels.iter().filter_map(|&r| {
        let relation = query.relations.get(r)?;
        let sub = relation
            .subquery
            .as_deref()
            .filter(|s| relation.lateral && from::simple_union_all(s))?;
        let mut reads = BTreeSet::new();
        for (expr, depth) in sub.all_exprs(1) {
            reads.extend(exprs::relations_at(expr, depth));
        }
        let mut outer = rels.clone();
        outer.remove(&r);
        outer.is_subset(&reads).then_some((r, outer))
    });
    let one = found.next()?;
    found.next().is_none().then_some(one)
}

/// `subquery_is_pushdown_safe` and `check_output_expressions`: `None` for a subquery that takes no condition, or for each column of the subquery true when a condition that reads it stays outside, with true when a condition that calls a volatile function stays outside. The subquery has no `LIMIT`, no `OFFSET` and no grouping sets with `GROUP BY`. For a set operation, each `SELECT` must take the condition. A condition stays outside when it reads a column that calls a function that gives a set or a volatile function, a column that is not an item of `DISTINCT ON`, or a column that is not an item of `PARTITION BY` of each window, which a run condition of the window could use.
fn safety(sub: &Query) -> Option<(Vec<bool>, bool)> {
    safety_of(sub, true)
}

/// [`safety`], where a column that is not an item of `PARTITION BY` of each window keeps a condition outside only when `windows` is true.
fn safety_of(sub: &Query, windows: bool) -> Option<(Vec<bool>, bool)> {
    if sub.limit.is_some()
        || sub.offset.is_some()
        || !sub.group.is_empty() && !sub.grouping_sets.is_empty()
    {
        return None;
    }
    let srfs = sub.has_target_srfs();
    let volatile = !sub.distinct.is_empty() || !sub.windows.is_empty() || srfs;
    let Some(op) = &sub.set_op else {
        let columns = sub
            .targets
            .iter()
            .enumerate()
            .map(|(at, t)| {
                t.junk
                    || srfs && t.expr.first_set_call().is_some()
                    || exprs::volatile(&t.expr)
                    || sub.distinct_on && !sub.distinct.iter().any(|d| d.target == at)
                    || windows
                        && sub.windows.iter().any(|w| !w.partition.iter().any(|p| p.target == at))
            })
            .collect();
        return Some((columns, volatile));
    };
    // `recurse_pushdown_safe`: each `SELECT` of the operation takes the condition, and no part is an `EXCEPT`.
    let mut columns = vec![false; sub.targets.len()];
    let mut volatile = volatile;
    let mut ok = !has_except(op);
    let mut each = |at: usize| {
        let leaf = sub.relations.get(at).and_then(|r| r.subquery.as_deref());
        let Some((leaf, (unsafe_columns, leaf_volatile))) =
            leaf.filter(|l| l.set_op.is_none()).and_then(|l| Some((l, safety(l)?)))
        else {
            ok = false;
            return;
        };
        volatile |= leaf_volatile;
        // `compare_tlist_datatypes`: a column of a type that is not the type of the column of the operation does not take a condition.
        let mut types = op.columns.iter();
        for (at, (t, bad)) in leaf.targets.iter().zip(unsafe_columns).enumerate() {
            let mismatch =
                !t.junk && types.next().is_some_and(|(ty, _)| *ty != t.expr.strip_implicit().ty);
            if at >= columns.len() {
                columns.push(false);
            }
            columns[at] |= bad || mismatch;
        }
    };
    op.left.each_leaf(&mut each);
    op.right.each_leaf(&mut each);
    ok.then_some((columns, volatile))
}

/// Point 6 of `qual_is_pushdown_safe`: true when the subquery has `DISTINCT`, a window or a set operation that makes groups, and the condition compares a column of the grouping with an operator whose equality is not the equality of the grouping, as `expression_has_grouping_conflict` finds it.
fn grouping_conflict(sub: &Query, part: &Expr, r: usize) -> bool {
    let groups = sub.set_op.as_ref().is_some_and(|op| setop_equal(op, 0).is_some());
    if sub.windows.is_empty() && sub.distinct.is_empty() && !groups {
        return false;
    }
    let equal = |e: &Expr| match &e.kind {
        ExprKind::Var(v) if v.levels_up == 0 && v.relation == r => {
            usize::try_from(v.attnum - 1).ok().and_then(|at| column_equal(sub, at))
        }
        _ => None,
    };
    upper::conflict_walk(&equal, None, part)
}

/// `subquery_column_grouping_eqop`: the function of the `=` operator that the subquery groups the column with: from `DISTINCT` or `DISTINCT ON`, from `PARTITION BY` when each window has the column, or from the set operation.
fn column_equal(sub: &Query, at: usize) -> Option<u32> {
    if let Some(d) = sub.distinct.iter().find(|d| d.target == at) {
        return Some(d.equal);
    }
    if !sub.windows.is_empty() {
        let mut equal = None;
        for w in &sub.windows {
            equal = Some(w.partition.iter().find(|p| p.target == at)?.equal);
        }
        return equal;
    }
    sub.set_op.as_ref().and_then(|op| setop_equal(op, at))
}

/// `setop_column_grouping_eqop`: the function of the `=` operator of the column of the first operation of the tree that makes groups, or `None` when no operation makes groups.
fn setop_equal(op: &SetOp, at: usize) -> Option<u32> {
    if let Some(g) = op.groups.get(at) {
        return Some(g.equal);
    }
    [&op.left, &op.right].into_iter().find_map(|t| match t {
        SetTree::Op(o) => setop_equal(o, at),
        SetTree::Leaf(_) => None,
    })
}

/// True when the set operation or an operation below it is an `EXCEPT`.
fn has_except(op: &SetOp) -> bool {
    op.kind == SetKind::Except
        || [&op.left, &op.right].into_iter().any(|t| matches!(t, SetTree::Op(o) if has_except(o)))
}

/// `qual_is_pushdown_safe`: true when the condition `part`, which reads the subquery `r`, can move into the subquery. It has no subquery, it calls no volatile function when `volatile` is true, and each column of the subquery that it reads is a column that `unsafe_columns` lets a condition read. It reads no whole row of the subquery. The other relations of the query that it reads are in `outer`.
fn safe(
    part: &Expr,
    r: usize,
    outer: &BTreeSet<usize>,
    unsafe_columns: &[bool],
    volatile: bool,
) -> bool {
    if volatile && exprs::volatile(part) {
        return false;
    }
    let blocked = |attnum: i16| {
        let at = usize::try_from(attnum - 1).ok();
        at.and_then(|at| unsafe_columns.get(at)).is_none_or(|u| *u)
    };
    part.find(0, &mut |e, depth| match &e.kind {
        ExprKind::SubLink(_) | ExprKind::Agg(_) | ExprKind::Grouping(_) | ExprKind::Window(_) => {
            Some(())
        }
        ExprKind::Var(v)
            if v.levels_up == depth
                && (v.relation != r && !outer.contains(&v.relation)
                    || v.relation == r && blocked(v.attnum)) =>
        {
            Some(())
        }
        _ => None,
    })
    .is_none()
}

/// `subquery_push_qual`: the condition reads the targets of the subquery in place of its columns, as `ReplaceVarsFromTargetList`, and a column of a query outside or of another relation of the query is one more level up. It joins `HAVING` when the subquery makes groups, or `WHERE`. With `fold`, the condition folds in the subquery, as `subquery_planner` folds the conditions of the subquery: a condition that is true does not join, and one that is false takes the place of the others.
fn push_qual(sub: &mut Query, r: usize, mut qual: Expr, fold: Option<FoldQual<'_>>) {
    // `recurse_push_qual`: each `SELECT` of a set operation takes the condition, one level further down.
    if let Some(op) = &sub.set_op {
        let mut leaves = Vec::new();
        op.left.each_leaf(&mut |at| leaves.push(at));
        op.right.each_leaf(&mut |at| leaves.push(at));
        qual.each_mut(0, &mut |e, depth| {
            if let ExprKind::Var(v) = &mut e.kind
                && (v.levels_up > depth || v.levels_up == depth && v.relation != r)
            {
                v.levels_up += 1;
            }
            false
        });
        for at in leaves {
            if let Some(leaf) = sub.relations.get_mut(at).and_then(|l| l.subquery.as_deref_mut()) {
                push_qual(leaf, r, qual.clone(), fold);
            }
        }
        return;
    }
    qual.each_mut(0, &mut |e, depth| {
        let ExprKind::Var(v) = &mut e.kind else { return false };
        if v.levels_up > depth {
            v.levels_up += 1;
        } else if v.levels_up == depth && v.relation == r {
            let at = usize::try_from(v.attnum - 1).ok();
            if let Some(target) = at.and_then(|at| sub.targets.get(at)) {
                let mut new = target.expr.clone();
                new.raise(depth);
                *e = new;
                return true;
            }
        } else if v.levels_up == depth {
            v.levels_up += 1;
        }
        false
    });
    if let Some(fold) = fold {
        qual = fold(sub, qual);
        if exprs::is_true(&qual) {
            return;
        }
    }
    let place = if sub.grouped || !sub.grouping_sets.is_empty() {
        &mut sub.having
    } else {
        if sub.filter.is_none() {
            sub.pushed = true;
        }
        &mut sub.filter
    };
    *place = Some(match place.take() {
        Some(_) if exprs::is_false(&qual) => qual,
        Some(old) => pullup::and(old, Some(qual)),
        None => qual,
    });
}
