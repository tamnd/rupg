//! `set_subquery_pathlist`: the conditions of `WHERE` on one subquery of `FROM` that the planner moves into the subquery before it plans the subquery, as `subquery_push_qual` moves them.
//!
//! A condition that moves reads the targets of the subquery in place of its columns. It joins the `WHERE` of the subquery, or its `HAVING` when the subquery makes groups, and the query above does not test it again.

use std::collections::BTreeSet;

use rupg_analyze::{BoolOp, Expr, ExprKind, FromItem, JoinKind, Query};
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

/// Reduces the outer joins of `FROM`, then moves each part of `WHERE` that reads one subquery of `FROM` into the subquery, when `qual_is_pushdown_safe` lets it. The subquery is not on the side of an outer join that can give nulls, where the condition tests the rows of the join. The planner does not move a condition into a view, a query of `WITH` or a set operation yet.
fn push(query: &mut Query) {
    let mut above = BTreeSet::new();
    for part in query.filter.iter().chain(upper::having_split(query).0) {
        above.extend(from::nonnullable_rels(part, true));
    }
    for item in &mut query.from {
        reduce(item, &above);
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
            let r = *rels.first().filter(|_| rels.len() == 1)?;
            let relation = query.relations.get(r)?;
            let plain = relation.oid == 0 && relation.cte.is_none() && !nullable.contains(&r);
            let sub = relation.subquery.as_deref().filter(|_| plain)?;
            let (unsafe_columns, volatile) = safety(sub)?;
            safe(part, r, &unsafe_columns, volatile).then_some(r)
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
    query.filter = match keep.len() {
        0 => None,
        1 => keep.pop(),
        _ => Some(Expr::new(ExprKind::Bool(BoolOp::And, keep), oid::BOOL)),
    };
}

/// `reduce_outer_joins`: an outer join is an inner join, and a full join is a left join or a right join, when a condition above it is false or null for each row with nulls for the relations of a side that can give nulls, as `from::reduce` finds it. The planner reduces the joins before it moves the conditions of `WHERE`, so that a condition that moves still makes the join an inner join.
fn reduce(item: &mut FromItem, above: &BTreeSet<usize>) {
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
    let mut local = BTreeSet::new();
    if join.kind != JoinKind::Full
        && let Some(on) = &join.on
    {
        local = from::nonnullable_rels(on, true);
    }
    let (left, right) = match join.kind {
        JoinKind::Inner => {
            local.extend(above.iter().copied());
            (local.clone(), local)
        }
        JoinKind::Left => (above.clone(), local),
        JoinKind::Right => (local, above.clone()),
        JoinKind::Full => (BTreeSet::new(), BTreeSet::new()),
    };
    reduce(&mut join.left, &left);
    reduce(&mut join.right, &right);
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
fn owned_parts(expr: Expr, out: &mut Vec<Expr>) {
    match expr.kind {
        ExprKind::Bool(BoolOp::And, args) => args.into_iter().for_each(|a| owned_parts(a, out)),
        kind => out.push(Expr { kind, ..expr }),
    }
}

/// `subquery_is_pushdown_safe` and `check_output_expressions`: `None` for a subquery that takes no condition, or for each column of the subquery true when a condition that reads it stays outside, with true when a condition that calls a volatile function stays outside. The subquery has no `LIMIT`, no `OFFSET`, no set operation and no grouping sets with `GROUP BY`. A condition stays outside when it reads a column that calls a function that gives a set or a volatile function, a column that is not an item of `DISTINCT ON`, or a column that is not an item of `PARTITION BY` of each window, which a run condition of the window could use.
fn safety(sub: &Query) -> Option<(Vec<bool>, bool)> {
    if sub.limit.is_some()
        || sub.offset.is_some()
        || sub.set_op.is_some()
        || !sub.group.is_empty() && !sub.grouping_sets.is_empty()
    {
        return None;
    }
    let srfs = sub.has_target_srfs();
    let volatile = !sub.distinct.is_empty() || !sub.windows.is_empty() || srfs;
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
    Some((columns, volatile))
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
