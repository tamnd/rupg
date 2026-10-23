//! `remove_useless_joins` of `analyzejoins.c`: a left join whose inner side is one relation that gives one row at most for each outer row, and whose columns the query reads only in the condition of the join, gives the rows of its outer side, so that the join leaves the plan.

use std::collections::BTreeSet;

use rupg_analyze::{Expr, ExprKind, FromItem, JoinKind, Query};

use crate::{Members, exprs, from, pushdown};

/// `remove_useless_joins`: each join of the query and of the queries in it that `join_is_removable` finds becomes its outer side. The relation of the inner side stays in the list of relations, and no item of `FROM` reads it. A removal can make another join removable, so the search starts again after each removal.
pub fn remove_joins(query: &mut Query, members: &dyn Members) {
    for relation in &mut query.relations {
        if let Some(sub) = &mut relation.subquery {
            remove_joins(sub, members);
        }
    }
    for cte in &mut query.ctes {
        remove_joins(&mut cte.query, members);
    }
    for expr in query.exprs_mut() {
        expr.each_mut(0, &mut |e, depth| {
            if depth == 0
                && let ExprKind::SubLink(link) = &mut e.kind
            {
                remove_joins(&mut link.query, members);
            }
            false
        });
    }
    // `preprocess_rowmarks` gives a row mark to each relation of a query with a lock and of an `UPDATE`, a `DELETE` or a `MERGE`, and the plan reads the row of the mark above the join.
    if query.write.is_some() || !query.row_marks.is_empty() {
        return;
    }
    while let Some(k) = query.from.iter().find_map(|item| removable(query, item, members)) {
        for item in &mut query.from {
            if remove(item, k) {
                break;
            }
        }
    }
}

/// The relation of the inner side of a join of the item that [`join_is_removable`] finds, with the joins below first.
fn removable(query: &Query, item: &FromItem, members: &dyn Members) -> Option<usize> {
    let FromItem::Join(join) = item else { return None };
    if let Some(k) =
        removable(query, &join.left, members).or_else(|| removable(query, &join.right, members))
    {
        return Some(k);
    }
    let (outer, inner) = match join.kind {
        JoinKind::Left => (&join.left, &join.right),
        JoinKind::Right => (&join.right, &join.left),
        _ => return None,
    };
    let FromItem::Relation(k) = *inner else { return None };
    let mut rels = Vec::new();
    pushdown::rels(outer, &mut rels);
    let outer: BTreeSet<usize> = rels.into_iter().collect();
    join_is_removable(query, join.on.as_ref(), k, &outer, members).then_some(k)
}

/// `join_is_removable`: the left join of the relations `outer` and the relation `k` with the condition `on` gives one row for each outer row, with no column of `k` that the query reads above the join. No expression of the query outside `on` reads a column of `k` or a `PlaceHolderVar` of `k`. The mergejoinable `=` parts of `on` that compare a column of `k` with an expression of the outer side, or with a value that reads no relation, must cover the key columns of a unique index of the table `k` that is immediate and has no predicate, as `relation_has_unique_index_for` finds it. For a subquery, the parts with the outer side must cover a set of columns that the subquery gives once each, as `query_is_distinct_for` finds it.
fn join_is_removable(
    query: &Query,
    on: Option<&Expr>,
    k: usize,
    outer: &BTreeSet<usize>,
    members: &dyn Members,
) -> bool {
    let Some(relation) = query.relations.get(k) else { return false };
    let total: usize = query.all_exprs(0).into_iter().map(|(e, depth)| reads(e, depth, k)).sum();
    if total != on.map_or(0, |on| reads(on, 0, k)) {
        return false;
    }
    let mut parts = Vec::new();
    if let Some(on) = on {
        exprs::and_parts(on, &mut parts);
    }
    // The columns of `k` that a part compares with the outer side, and with a value that reads no relation.
    let mut joined = BTreeSet::new();
    let mut fixed = BTreeSet::new();
    for part in parts {
        let Some((op, left, right)) = exprs::binary(part) else { continue };
        if !exprs::can_merge(op) || exprs::volatile(part) {
            continue;
        }
        for (inner, other) in [(left, right), (right, left)] {
            let rels = exprs::relations(other);
            match from::plain_var(inner) {
                Some(var) if var.levels_up == 0 && var.relation == k && rels.is_subset(outer) => {
                    if rels.is_empty() { &mut fixed } else { &mut joined }.insert(var.attnum);
                }
                _ => {}
            }
        }
    }
    if relation.cte.is_some() || relation.lateral {
        return false;
    }
    if let Some(sub) = &relation.subquery {
        return from::distinct_keys(sub).iter().any(|keys| keys.iter().all(|c| joined.contains(c)));
    }
    if relation.oid == 0
        || relation.function.is_some()
        || relation.values.is_some()
        || relation.tablesample.is_some()
        || members.members(query, k, &[]).is_some()
    {
        return false;
    }
    members.indexes(relation.oid).iter().any(|index| {
        index.unique
            && index.immediate
            && !index.partial
            && index.keys.iter().all(|c| *c > 0 && (joined.contains(c) || fixed.contains(c)))
    })
}

/// The number of columns of the relation `k` and of `PlaceHolderVar`s of `k` that the expression at the depth `depth` reads, also in its subqueries.
fn reads(expr: &Expr, depth: usize, k: usize) -> usize {
    let mut n = 0;
    expr.find(depth, &mut |e, depth| {
        match &e.kind {
            ExprKind::Var(var) if var.levels_up == depth && var.relation == k => n += 1,
            ExprKind::PlaceHolder(p) if p.levels_up == depth && p.relations.contains(&k) => n += 1,
            _ => {}
        }
        None::<()>
    });
    n
}

/// `remove_rel_from_joinlist`: the join of the item whose inner side is the relation `k` becomes its outer side. True when the item had the join.
fn remove(item: &mut FromItem, k: usize) -> bool {
    let FromItem::Join(join) = item else { return false };
    let join = &mut **join;
    let outer = match join.kind {
        JoinKind::Left if matches!(join.right, FromItem::Relation(r) if r == k) => &mut join.left,
        JoinKind::Right if matches!(join.left, FromItem::Relation(r) if r == k) => &mut join.right,
        _ => return remove(&mut join.left, k) || remove(&mut join.right, k),
    };
    let outer = std::mem::replace(outer, FromItem::Relation(k));
    *item = outer;
    true
}
