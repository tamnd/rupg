//! `set_subquery_pathlist`: the conditions of `WHERE` on one subquery of `FROM` that the planner moves into the subquery before it plans the subquery, as `subquery_push_qual` moves them.
//!
//! A condition that moves reads the targets of the subquery in place of its columns. It joins the `WHERE` of the subquery, or its `HAVING` when the subquery makes groups, and the query above does not test it again.

use std::collections::BTreeSet;

use rupg_analyze::{
    BoolOp, BoolTest, Expr, ExprKind, FromItem, JoinKind, Query, Relation, SetKind, SetOp, SetTree,
};
use rupg_types::oid;

use crate::{exprs, from, pullup, upper};

/// Moves the conditions of `WHERE` that the planner can move into the subqueries of `FROM`, then does the same in each subquery, also in the subqueries of the expressions.
pub(crate) fn push_all(query: &mut Query) {
    push(query);
    for relation in &mut query.relations {
        if let Some(sub) = &mut relation.subquery {
            push_all(sub);
        }
    }
    for expr in query.exprs_mut() {
        expr.each_mut(0, &mut |e, depth| {
            if depth == 0
                && let ExprKind::SubLink(link) = &mut e.kind
            {
                push_all(&mut link.query);
            }
            false
        });
    }
}

/// Reduces the outer joins of `FROM` and removes the tests of `IS NULL` that an anti join makes redundant, then moves each part of `WHERE` that reads one subquery of `FROM` into the subquery, when `qual_is_pushdown_safe` lets it. The subquery is not on the side of an outer join that can give nulls, where the condition tests the rows of the join. The planner does not move a condition into a view or a query of `WITH` yet.
fn push(query: &mut Query) {
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
            let r = match (rels.len(), query.from.as_slice()) {
                (1, _) => *rels.first()?,
                (0, [FromItem::Relation(r)]) if exprs::volatile(part) => *r,
                _ => return None,
            };
            let relation = query.relations.get(r)?;
            let plain = relation.oid == 0 && relation.cte.is_none() && !nullable.contains(&r);
            let sub = relation.subquery.as_deref().filter(|_| plain)?;
            pushable(sub, part, r).then_some(r)
        })
        .collect();
    if to.iter().all(Option::is_none) {
        query.filter = Some(filter);
        return;
    }
    let mut owned = Vec::new();
    owned_parts(filter, &mut owned);
    let mut keep = Vec::new();
    for (part, to) in owned.into_iter().zip(to) {
        match to.and_then(|r| query.relations[r].subquery.as_deref_mut().map(|sub| (r, sub))) {
            Some((r, sub)) => push_qual(sub, r, part),
            None => keep.push(part),
        }
    }
    query.filter = and_all(keep);
}

/// The `AND` of the conditions, or `None` when there is no condition.
fn and_all(mut parts: Vec<Expr>) -> Option<Expr> {
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
fn rels(item: &FromItem, out: &mut Vec<usize>) {
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
fn pushable(sub: &Query, part: &Expr, r: usize) -> bool {
    let fits =
        |q: &Query| safety(q).is_some_and(|(columns, volatile)| safe(part, r, &columns, volatile));
    let Some(op) = sub.set_op.as_ref().filter(|_| from::simple_union_all(sub)) else {
        return fits(sub);
    };
    let mut all = true;
    let mut each = |at: usize| {
        let Some(leaf) = sub.relations.get(at).and_then(|l| l.subquery.as_deref()) else {
            all = false;
            return;
        };
        let member = pullup::simple(leaf)
            && leaf.filter.is_none()
            && matches!(leaf.from.as_slice(), [FromItem::Relation(_)]);
        all &= if member {
            safe(part, r, &vec![false; leaf.targets.len()], false)
        } else {
            fits(leaf)
        };
    };
    op.left.each_leaf(&mut each);
    op.right.each_leaf(&mut each);
    all
}

/// `subquery_is_pushdown_safe` and `check_output_expressions`: `None` for a subquery that takes no condition, or for each column of the subquery true when a condition that reads it stays outside, with true when a condition that calls a volatile function stays outside. The subquery has no `LIMIT`, no `OFFSET` and no grouping sets with `GROUP BY`. For a set operation, each `SELECT` must take the condition. A condition stays outside when it reads a column that calls a function that gives a set or a volatile function, a column that is not an item of `DISTINCT ON`, or a column that is not an item of `PARTITION BY` of each window, which a run condition of the window could use.
fn safety(sub: &Query) -> Option<(Vec<bool>, bool)> {
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
                    || sub.windows.iter().any(|w| !w.partition.iter().any(|p| p.target == at))
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

/// True when the set operation or an operation below it is an `EXCEPT`.
fn has_except(op: &SetOp) -> bool {
    op.kind == SetKind::Except
        || [&op.left, &op.right].into_iter().any(|t| matches!(t, SetTree::Op(o) if has_except(o)))
}

/// `qual_is_pushdown_safe`: true when the condition `part`, which reads the subquery `r`, can move into the subquery. It has no subquery, it calls no volatile function when `volatile` is true, and each column of the subquery that it reads is a column that `unsafe_columns` lets a condition read. It reads no whole row of the subquery.
fn safe(part: &Expr, r: usize, unsafe_columns: &[bool], volatile: bool) -> bool {
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
        ExprKind::Var(v) if v.levels_up == depth && (v.relation != r || blocked(v.attnum)) => {
            Some(())
        }
        _ => None,
    })
    .is_none()
}

/// `subquery_push_qual`: the condition reads the targets of the subquery in place of its columns, as `ReplaceVarsFromTargetList`, and a column of a query outside is one more level up. It joins `HAVING` when the subquery makes groups, or `WHERE`.
fn push_qual(sub: &mut Query, r: usize, mut qual: Expr) {
    // `recurse_push_qual`: each `SELECT` of a set operation takes the condition, one level further down.
    if let Some(op) = &sub.set_op {
        let mut leaves = Vec::new();
        op.left.each_leaf(&mut |at| leaves.push(at));
        op.right.each_leaf(&mut |at| leaves.push(at));
        qual.each_mut(0, &mut |e, depth| {
            if let ExprKind::Var(v) = &mut e.kind
                && v.levels_up > depth
            {
                v.levels_up += 1;
            }
            false
        });
        for at in leaves {
            if let Some(leaf) = sub.relations.get_mut(at).and_then(|l| l.subquery.as_deref_mut()) {
                push_qual(leaf, r, qual.clone());
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
        }
        false
    });
    let place = if sub.grouped || !sub.grouping_sets.is_empty() {
        &mut sub.having
    } else {
        &mut sub.filter
    };
    *place = Some(match place.take() {
        Some(old) => pullup::and(old, Some(qual)),
        None => qual,
    });
}
