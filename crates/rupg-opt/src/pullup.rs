//! `pull_up_subqueries`: the subqueries in `FROM` that the planner merges into the query above them before it plans the query, as `pull_up_simple_subquery` merges them.
//!
//! The relations of a merged subquery come after the relations of the query, as the range table of the subquery comes after the range table of the query in PostgreSQL. The subquery takes its place in `FROM`, its `WHERE` joins the `WHERE` of the query or the `ON` of its join, and each column of the subquery becomes the expression of its target.

use rupg_analyze::{BoolOp, Expr, ExprKind, FromItem, JoinKind, Query, Relation};
use rupg_types::oid;

use crate::exprs;

/// The query with each simple subquery in `FROM` merged into the query above it, also in the subqueries of the query.
pub fn pull_up(query: &Query) -> Query {
    let mut query = query.clone();
    pull_all(&mut query);
    query
}

/// Merges the simple subqueries of the query and of its subqueries, the inner subqueries first, as `pull_up_simple_subquery` merges the subqueries of a subquery before the subquery.
fn pull_all(query: &mut Query) {
    for relation in &mut query.relations {
        if let Some(sub) = &mut relation.subquery {
            pull_all(sub);
        }
    }
    for expr in query.exprs_mut() {
        expr.each_mut(0, &mut |e, depth| {
            if depth == 0
                && let ExprKind::SubLink(link) = &mut e.kind
            {
                pull_all(&mut link.query);
            }
            false
        });
    }
    while let Some((r, top)) = candidate(query) {
        merge(query, r, top);
    }
}

/// `is_simple_subquery`: a subquery that the planner can merge into the query above it. It has no set operation, no aggregate, no window, no function that gives a set in its targets, no `ORDER BY`, no `DISTINCT`, no `LIMIT`, no `OFFSET`, no row lock, no `WITH` and no volatile function in its targets.
fn simple(sub: &Query) -> bool {
    sub.set_op.is_none()
        && sub.write.is_none()
        && !sub.grouped
        && sub.grouping_sets.is_empty()
        && sub.having.is_none()
        && sub.windows.is_empty()
        && !sub.has_target_srfs()
        && sub.sort.is_empty()
        && sub.distinct.is_empty()
        && sub.limit.is_none()
        && sub.offset.is_none()
        && sub.row_marks.is_empty()
        && sub.ctes.is_empty()
        && !sub.from.is_empty()
        && !sub.targets.iter().any(|t| t.junk || exprs::volatile(&t.expr))
}

/// Each relation of `FROM`, with true when it is an item of the list of `FROM` and true when a condition of the relation can join the `ON` of a join above it or the `WHERE` of the query. The condition joins the `ON` of a join that has the relation in a side of an inner join or in the side of an outer join that can give nulls, and it goes past a join that keeps the rows of the side of the relation. It cannot go past a full join.
fn places(item: &FromItem, top: bool, on: bool, out: &mut Vec<(usize, bool, bool)>) {
    match item {
        FromItem::Relation(r) => out.push((*r, top, on)),
        FromItem::Join(join) => {
            let (left, right) = match join.kind {
                JoinKind::Inner => (true, true),
                JoinKind::Left => (on, true),
                JoinKind::Right => (true, on),
                JoinKind::Full => (false, false),
            };
            places(&join.left, false, left, out);
            places(&join.right, false, right, out);
        }
    }
}

/// The first subquery of `FROM` that the planner merges, with true when it is an item of the list of `FROM`. The planner does not merge a `LATERAL` subquery, a view or a query of `WITH` yet. Below the side of an outer join that can give nulls, each target of the subquery that the query reads must be a column, because a `PlaceHolderVar` would keep the other expressions. In a join, the subquery has one item in `FROM`, and its `WHERE` joins the `ON` of the join, so that the subquery is not the side of an outer join that keeps its rows or a side of a full join when it has a `WHERE`. The query does not read the whole row of the subquery, which `pullup_replace_vars` makes a `ROW` of the targets.
fn candidate(query: &Query) -> Option<(usize, bool)> {
    if query.set_op.is_some()
        || query.write.is_some()
        || !query.row_marks.is_empty()
        || !query.grouping_sets.is_empty()
    {
        return None;
    }
    let mut found = Vec::new();
    for item in &query.from {
        places(item, true, true, &mut found);
    }
    let mut nullable = Vec::new();
    for item in &query.from {
        nullable_places(item, false, &mut nullable);
    }
    found.into_iter().find_map(|(r, top, on)| {
        let relation = query.relations.get(r)?;
        let sub = pullable(relation)?;
        let used = used(query, r);
        let column = |attnum: &i16| {
            let target = usize::try_from(*attnum - 1).ok().and_then(|at| sub.targets.get(at));
            target.is_some_and(|t| matches!(t.expr.kind, ExprKind::Var(v) if v.levels_up == 0))
        };
        let columns = !nullable.contains(&r) || used.iter().all(column);
        let fits = top || sub.from.len() == 1 && (sub.filter.is_none() || on);
        (simple(sub) && columns && fits && !used.contains(&0)).then_some((r, top))
    })
}

/// The relations of `FROM` that an outer join can give nulls for.
fn nullable_places(item: &FromItem, nullable: bool, out: &mut Vec<usize>) {
    match item {
        FromItem::Relation(r) if nullable => out.push(*r),
        FromItem::Relation(_) => {}
        FromItem::Join(join) => {
            let left = matches!(join.kind, JoinKind::Right | JoinKind::Full);
            let right = matches!(join.kind, JoinKind::Left | JoinKind::Full);
            nullable_places(&join.left, nullable || left, out);
            nullable_places(&join.right, nullable || right, out);
        }
    }
}

/// The subquery of a relation that is a subquery in `FROM`, not a view, a query of `WITH` or a `LATERAL` subquery.
fn pullable(relation: &Relation) -> Option<&Query> {
    let plain = relation.oid == 0 && relation.cte.is_none() && !relation.lateral;
    relation.subquery.as_deref().filter(|_| plain)
}

/// The numbers of the columns of the relation `r` that the query reads, with 0 for the whole row. The columns of a join do not count, because the expressions of the query read the columns of the relations.
fn used(query: &Query, r: usize) -> Vec<i16> {
    let mut out = Vec::new();
    let mut add = |e: &Expr, depth: usize| {
        if let ExprKind::Var(v) = e.kind
            && v.levels_up == depth
            && v.relation == r
        {
            out.push(v.attnum);
        }
        None::<()>
    };
    for (expr, depth) in query.all_exprs(0) {
        expr.find(depth, &mut add);
    }
    out
}

/// Calls `f` for each column of each join of `FROM`, which the names of `USING` read.
fn join_columns(items: &mut [FromItem], f: &mut impl FnMut(&mut Expr)) {
    for item in items {
        if let FromItem::Join(join) = item {
            let join = &mut **join;
            join_columns(std::slice::from_mut(&mut join.left), f);
            join_columns(std::slice::from_mut(&mut join.right), f);
            join.merged.iter_mut().chain(join.columns.iter_mut().map(|(_, e)| e)).for_each(&mut *f);
        }
    }
}

/// Each relation of the items of `FROM` gets the index that `map` gives for it.
fn renumber(items: &mut [FromItem], map: &impl Fn(usize) -> usize) {
    for item in items {
        match item {
            FromItem::Relation(r) => *r = map(*r),
            FromItem::Join(join) => {
                let join = &mut **join;
                renumber(std::slice::from_mut(&mut join.left), map);
                renumber(std::slice::from_mut(&mut join.right), map);
            }
        }
    }
}

/// The item of `FROM` that is the relation `r`.
fn take_place(items: &mut [FromItem], r: usize, with: &mut Option<FromItem>) {
    for item in items {
        match item {
            FromItem::Relation(at) if *at == r => {
                if let Some(new) = with.take() {
                    *item = new;
                }
            }
            FromItem::Relation(_) => {}
            FromItem::Join(join) => {
                let join = &mut **join;
                take_place(std::slice::from_mut(&mut join.left), r, with);
                take_place(std::slice::from_mut(&mut join.right), r, with);
            }
        }
    }
}

/// `pull_up_simple_subquery`: merges the subquery of the relation `r` into the query.
fn merge(query: &mut Query, r: usize, top: bool) {
    let Some(sub) = query.relations.remove(r).subquery else { return };
    let mut sub = *sub;
    let base = query.relations.len();
    // The relations of the subquery come after the relations of the query, and a column of a query outside the subquery is one level nearer.
    let mut inner = |e: &mut Expr, depth: usize| {
        match &mut e.kind {
            ExprKind::Var(v) if v.levels_up == depth => v.relation += base,
            ExprKind::Var(v) if v.levels_up > depth => v.levels_up -= 1,
            ExprKind::Agg(agg) if agg.levels_up > depth => agg.levels_up -= 1,
            ExprKind::Grouping(g) if g.levels_up > depth => g.levels_up -= 1,
            _ => {}
        }
        false
    };
    sub.each_expr_mut(0, &mut |e, depth| e.each_mut(depth, &mut inner));
    join_columns(&mut sub.from, &mut |e| e.each_mut(0, &mut inner));
    renumber(&mut sub.from, &|j| j + base);
    // `pullup_replace_vars`: a column of the subquery becomes the expression of its target, and the relations after the subquery move down one place.
    let targets: Vec<Expr> = sub.targets.iter().map(|t| t.expr.clone()).collect();
    let mut outer = |e: &mut Expr, depth: usize| {
        let ExprKind::Var(v) = &mut e.kind else { return false };
        if v.levels_up != depth {
            return false;
        }
        if v.relation == r {
            let at = usize::try_from(v.attnum - 1).ok();
            if let Some(target) = at.and_then(|at| targets.get(at)) {
                let mut new = target.clone();
                new.raise(depth);
                *e = new;
                return true;
            }
        } else if v.relation > r {
            v.relation -= 1;
        }
        false
    };
    query.each_expr_mut(0, &mut |e, depth| e.each_mut(depth, &mut outer));
    join_columns(&mut query.from, &mut |e| e.each_mut(0, &mut outer));
    // The subquery takes the place of its relation in `FROM`: its items in the list of `FROM`, or its one item in a join.
    let gone = usize::MAX;
    take_place(&mut query.from, r, &mut Some(FromItem::Relation(gone)));
    renumber(&mut query.from, &|k| if k > r && k != gone { k - 1 } else { k });
    if top {
        let at =
            query.from.iter().position(|item| matches!(item, FromItem::Relation(k) if *k == gone));
        if let Some(at) = at {
            query.from.splice(at..=at, std::mem::take(&mut sub.from));
        }
    } else {
        add_on(&mut query.from, gone, &mut sub.filter);
        take_place(&mut query.from, gone, &mut sub.from.pop());
    }
    query.relations.append(&mut sub.relations);
    query.pulled += 1 + sub.pulled;
    if let Some(filter) = sub.filter {
        query.filter = Some(and(filter, query.filter.take()));
    }
}

/// The condition `first AND then`.
fn and(first: Expr, then: Option<Expr>) -> Expr {
    match then {
        None => first,
        Some(then) => Expr::new(ExprKind::Bool(BoolOp::And, vec![first, then]), oid::BOOL),
    }
}

/// The condition of the relation `r` joins the `ON` of a join above it, as [`places`] finds the join, or stays in `filter` for the `WHERE` of the query.
fn add_on(items: &mut [FromItem], r: usize, filter: &mut Option<Expr>) {
    for item in items {
        if lift(item, r, filter) {
            return;
        }
    }
}

/// True when the item has the relation `r`. The condition stays in `filter` when it goes past the item.
fn lift(item: &mut FromItem, r: usize, filter: &mut Option<Expr>) -> bool {
    let FromItem::Join(join) = item else {
        return matches!(item, FromItem::Relation(at) if *at == r);
    };
    let join = &mut **join;
    let (on, here) = if lift(&mut join.left, r, filter) {
        (matches!(join.kind, JoinKind::Inner | JoinKind::Right), true)
    } else if lift(&mut join.right, r, filter) {
        (matches!(join.kind, JoinKind::Inner | JoinKind::Left), true)
    } else {
        (false, false)
    };
    if on && let Some(filter) = filter.take() {
        join.on = Some(and(filter, join.on.take()));
    }
    here
}
