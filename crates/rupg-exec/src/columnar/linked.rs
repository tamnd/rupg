//! The join of the tables of a query with groups through keys that are unique on one side, on the columns of the tables (spec/14 section 14.8).
//!
//! Each table keeps the rows that pass its own conditions of `WHERE`, as [`super::selected`] finds them with the vectors. One table is the root: the other tables join it through a tree of `=` of integer keys, and the key of each `=` is unique in the rows of the table further from the root, as a primary key of a dimension table is. For each row of the root, a lookup in an array or in a hash table of the key of each table gives the one row of that table, or no row, and then the row of the root does not join. The join so gives the places of the rows of each table, and no tuple of values. The columns that the query reads are then gathered at these places into columns of the join, and the vector engine runs the groups and the aggregates on them as on the columns of one table, with the other conditions of `WHERE`.
//!
//! The groups of the vector engine do not depend on the order of the rows, and the query has the plan of the join, so that the groups come in the order of the hash table of the plan.

use std::sync::Arc;

use std::cmp::Ordering;

use rupg_analyze::{
    BoolOp, Expr, ExprKind, FromItem, Join, JoinKind, Query, SortGroup, SubLinkKind,
};
use rupg_column::builder::{self, Values};
use rupg_column::segment::Kind;
use rupg_column::stored::{Stored, VECTOR};
use rupg_common::{Error, Result};
use rupg_func::Session;
use rupg_types::{Value, oid};

use super::{
    Decoded, DecodedData, FxMap, FxSet, Link as Ran, Selected, Source, decode, having_once,
    kind_of, numeric_kind, once, plain, unsupported,
};
use crate::{Cache, reduce, scan, sort};

/// No row.
const NONE: u32 = u32::MAX;

/// The rows of a query with groups, or of a query with `ORDER BY` as [`sorted_rows`] tells, whose tables join through keys that are unique on one side, or `None` when the query does not have this form.
///
/// # Errors
///
/// The error of a condition or of an expression of the query.
pub(crate) fn run(
    query: &Query,
    params: &[Value],
    session: &dyn Session,
    cache: &Cache,
    want: Option<usize>,
) -> Result<Option<Vec<Vec<Value>>>> {
    if !shape(query, session) {
        return Ok(None);
    }
    if let [FromItem::Join(join)] = query.from.as_slice() {
        let Some((kept, selected, pos)) = left_join(query, join, params, session, cache)? else {
            return Ok(None);
        };
        let (selected, pos) = (&selected, &pos);
        return finish(query, params, session, cache, want, kept, selected, pos, vec![], vec![]);
    }
    let mut conds = Vec::new();
    if let Some(filter) = &query.filter {
        scan::conjuncts(filter, &mut conds);
    }
    // A subquery that runs once, such as an `EXISTS` of TPC-H Q21 or the `avg` of Q17. A condition with a subquery that reads one table, as the two `EXISTS` of Q21, is a condition of the table, which the scan of the table tests. Another condition with a subquery, as the one of Q17, is tested on the rows of the join.
    let (links, conds): (Vec<&Expr>, Vec<&Expr>) = conds.into_iter().partition(|c| link(c));
    // The `OR` of two or more tables also gives a condition of each table.
    let derived = reduce::restrictions(&conds);
    let all: Vec<&Expr> = conds.iter().copied().chain(&derived).collect();
    let Some(plan) = reduce::plan(query, &all, session) else { return Ok(None) };
    let width = query.relations.len();
    let mut selected = Vec::with_capacity(width);
    for (index, relation) in query.relations.iter().enumerate() {
        // A condition `x IN (SELECT ...)` of the table: the subquery runs one time, and its integers then select the rows that pass the other conditions.
        let (semi, mut own): (Vec<&Expr>, Vec<&Expr>) =
            plan.own[index].iter().copied().partition(|c| matches!(c.kind, ExprKind::SubLink(_)));
        own.extend(links.iter().filter(|c| reduce::relations_of(c) == [index]));
        let mut keys = plan.keys[index].clone();
        let mut sets = Vec::with_capacity(semi.len());
        for cond in semi {
            let Some((column, set)) = semi_set(cond, index, params, session, cache)? else {
                return Ok(None);
            };
            sets.push((keys.len(), set));
            keys.push(column);
        }
        match super::selected(relation, &own, &keys, params, session, cache, true)? {
            Some(mut found) if u32::try_from(found.places.len()).is_ok_and(|n| n < NONE) => {
                if !sets.is_empty() {
                    let keep: Vec<bool> = (0..found.places.len())
                        .map(|row| {
                            sets.iter().all(|(at, set)| {
                                found.keys[*at][row].is_some_and(|key| set.contains(&key))
                            })
                        })
                        .collect();
                    found.keys.truncate(plan.keys[index].len());
                    retain(&mut found.places, &keep);
                    for key in &mut found.keys {
                        retain(key, &keep);
                    }
                }
                selected.push(found);
            }
            _ => return Ok(None),
        }
    }
    let Some(tree) = Tree::of(&plan.edges, &selected) else { return Ok(None) };
    let pos = tree.join(&selected);
    // The conditions of `WHERE` that the vectors test on the rows of the join: the conditions of two or more tables, except the `=` of the tree, and the conditions of no table.
    let residual: Vec<&Expr> = conds
        .iter()
        .filter(|cond| {
            let relations = reduce::relations_of(cond);
            if relations.len() == 1 || matches!(cond.kind, ExprKind::SubLink(_)) {
                return false;
            }
            !reduce::equal_columns(cond).is_some_and(|[(a, _), (b, _)]| tree.joins(a, b))
        })
        .chain(links.iter().filter(|c| reduce::relations_of(c).len() > 1))
        .copied()
        .collect();
    // The links are made with the subqueries of the query, as the lifted state of a subquery is kept by its address.
    let Some(links) = super::links_of(residual.iter().copied(), params, session, cache) else {
        return Ok(None);
    };
    let residual = residual.into_iter().cloned().collect();
    let (root, selected, pos) = (tree.root, &selected, &pos);
    finish(query, params, session, cache, want, root, selected, pos, residual, links)
}

/// The rows of the query from the places `pos` in [`Selected::places`] of the row of each table in each tuple of the join, with the root table `base`. The vectors test the conditions `residual` on the tuples, with the subqueries of `links` in their order in `residual`. A place [`NONE`] is the row of nulls of the table on the null side of an outer join.
///
/// # Errors
///
/// The error of a condition or of an expression of the query.
#[allow(clippy::too_many_arguments)]
fn finish(
    query: &Query,
    params: &[Value],
    session: &dyn Session,
    cache: &Cache,
    want: Option<usize>,
    base: usize,
    selected: &[Selected],
    pos: &[Vec<u32>],
    residual: Vec<Expr>,
    mut links: Vec<Ran>,
) -> Result<Option<Vec<Vec<Value>>>> {
    // A subquery of `HAVING` that runs one time, as the `InitPlan` of TPC-H Q11, gives a constant.
    let having = match &query.having {
        Some(having) => match having_once(having, params, session, cache) {
            Some(having) => Some(having),
            None => return Ok(None),
        },
        None => None,
    };
    let exprs = query.targets.iter().map(|t| &t.expr).chain(&having);
    if exprs.chain(&query.offset).chain(&query.limit).any(|e| unsupported(e, false))
        || residual.iter().any(|e| unsupported(e, true))
    {
        return Ok(None);
    }
    // The columns of each table that the query reads after the join.
    let mut used: Vec<Vec<bool>> =
        query.relations.iter().map(|r| vec![false; r.columns.len()]).collect();
    let exprs = query.targets.iter().map(|t| &t.expr).chain(&having).chain(&residual);
    for expr in exprs {
        // A column of a table in a subquery is a column of the query just outside the subquery.
        expr.find(0, &mut |e, depth| {
            if let ExprKind::Var(var) = e.kind
                && var.levels_up == depth
                && let Some(slot) = usize::try_from(var.attnum - 1)
                    .ok()
                    .and_then(|i| used.get_mut(var.relation)?.get_mut(i))
            {
                *slot = true;
            }
            None::<()>
        });
    }
    let Some(catalog) = session.catalog() else { return Ok(None) };
    let rows = pos[base].len();
    let mut columns = Vec::new();
    let mut stored = Vec::new();
    let mut types = Vec::new();
    let mut map: Vec<Vec<i16>> = used.iter().map(|u| vec![0; u.len()]).collect();
    for (index, relation) in query.relations.iter().enumerate() {
        let Some(table) = catalog.relation(relation.oid) else { return Ok(None) };
        let mut wanted = Vec::new();
        for (column, _) in used[index].iter().enumerate().filter(|(_, u)| **u) {
            let def = &relation.columns[column];
            let Some(kind) = kind_of(def.ty).or_else(|| numeric_kind(def.ty, def.typmod)) else {
                return Ok(None);
            };
            if kind == Kind::Text && !table.columns.get(column).is_some_and(|c| plain(c.collation))
            {
                return Ok(None);
            }
            wanted.push((column, kind));
        }
        if wanted.is_empty() {
            continue;
        }
        let Some((_, read)) = session.table_small_columns(relation.oid, &wanted)? else {
            return Ok(None);
        };
        let at: Vec<usize> = pos[index]
            .iter()
            .map(|&p| if p == NONE { usize::MAX } else { selected[index].places[p as usize] })
            .collect();
        let outer = pos[index].contains(&NONE);
        for (&(column, _), read) in wanted.iter().zip(&read) {
            let (values, nulls) = gather(read, &at)?;
            let column_stored =
                builder::gathered(read.kind, &values, nulls.as_deref(), read.dict.clone())?;
            stored.push(Arc::new(column_stored));
            let def = &relation.columns[column];
            types.push(def.ty);
            columns.push(rupg_analyze::Column { not_null: def.not_null && !outer, ..def.clone() });
            map[index][column] = i16::try_from(columns.len()).map_err(|_| {
                Error::internal("a join reads more columns than a relation can have")
            })?;
        }
    }
    let mut virt = query.clone();
    virt.having = having;
    let mut root = query.relations[base].clone();
    root.columns = columns;
    virt.relations = vec![root];
    virt.from = vec![FromItem::Relation(0)];
    virt.filter = match residual.len() {
        0 => None,
        1 => residual.into_iter().next(),
        _ => Some(Expr {
            collation: 0,
            kind: ExprKind::Bool(BoolOp::And, residual),
            ty: oid::BOOL,
            typmod: -1,
            location: None,
        }),
    };
    virt.each_expr_mut(0, &mut |expr, depth| {
        expr.each_mut(depth, &mut |e, depth| {
            if let ExprKind::Var(var) = &mut e.kind
                && var.levels_up == depth
                && let Some(&to) =
                    usize::try_from(var.attnum - 1).ok().and_then(|i| map.get(var.relation)?.get(i))
            {
                var.attnum = to;
                var.relation = 0;
            }
            false
        });
    });
    // The links of the copy of `residual` in `virt`, which read the columns of the join.
    let mut found = Vec::new();
    if let Some(filter) = &virt.filter {
        filter.find(0, &mut |e, depth| {
            if depth == 0 && matches!(e.kind, ExprKind::SubLink(_)) {
                found.push(std::ptr::from_ref(e));
            }
            None::<()>
        });
    }
    if found.len() != links.len() {
        return Ok(None);
    }
    for (link, at) in links.iter_mut().zip(found) {
        link.at = at;
        for expr in link.outer_mut() {
            let mut fail = false;
            expr.each_mut(0, &mut |e, depth| {
                if let ExprKind::Var(var) = &mut e.kind
                    && var.levels_up == depth
                {
                    match usize::try_from(var.attnum - 1)
                        .ok()
                        .and_then(|i| map.get(var.relation)?.get(i))
                    {
                        Some(&to) if to > 0 => {
                            var.attnum = to;
                            var.relation = 0;
                        }
                        _ => fail = true,
                    }
                }
                false
            });
            if fail {
                return Ok(None);
            }
        }
    }
    let places = (0..stored.len()).collect();
    let tables = query
        .relations
        .iter()
        .zip(selected)
        .map(|(relation, selected)| (relation.oid, selected.places.len()))
        .collect();
    let mut source = Source::new(rows, stored, places, types, tables);
    source.links = links;
    // With no groups, the run gives one row more than `LIMIT`, so that a row equal to the last row in the keys of the sort shows.
    let mut limit = None;
    if !query.grouped
        && let Some(Expr { kind: ExprKind::Const(Value::Int8(n)), .. }) = &mut virt.limit
    {
        limit = usize::try_from(*n).ok();
        *n = n.saturating_add(1);
    }
    let want = if query.grouped { want } else { want.map(|w| w.saturating_add(1)) };
    // The groups come in the order of the hash table of the plan of the join.
    session.share_plan(&virt, Some(query));
    let out = super::run_on(&virt, params, session, cache, want, source);
    session.share_plan(&virt, None);
    let Some(mut out) = out? else { return Ok(None) };
    if !query.grouped {
        if ties(query, &out, session)? {
            return Ok(None);
        }
        out.truncate(limit.unwrap_or(usize::MAX).min(want.map_or(usize::MAX, |w| w - 1)));
    }
    Ok(Some(out))
}

/// True for a query with no groups that the join of the vectors can run: a query with `ORDER BY` whose keys are columns of the result, with no `DISTINCT`, no `OFFSET`, a `LIMIT` that is a constant or no `LIMIT`, no volatile function, and no expression that can fail, as [`no_sublinks`] tells. The join of the vectors does not give the tuples in the order of the plan of the join, so [`finish`] then checks with [`ties`] that the sort gives one order of the rows.
fn sorted_rows(query: &Query) -> bool {
    !query.sort.is_empty()
        && query.distinct.is_empty()
        && !query.distinct_on
        && query.offset.is_none()
        && query
            .limit
            .as_ref()
            .is_none_or(|l| matches!(l.kind, ExprKind::Const(Value::Int8(n)) if n >= 0))
        && query.sort.iter().all(|g| query.targets.get(g.target).is_some_and(|t| !t.junk))
        && !query.targets.iter().map(|t| &t.expr).chain(&query.filter).any(crate::volatile)
        && query
            .targets
            .iter()
            .map(|t| &t.expr)
            .chain(&query.filter)
            .all(|e| reduce::harmless(&no_sublinks(e)))
}

/// A copy of `expr` with a null in the place of each subquery, for [`reduce::harmless`]. A plan with `LIMIT` can stop before it reads all the tuples of the join, and then it does not evaluate the expressions of the other tuples, so these expressions must not fail. A subquery that fails makes its link fail, and then the row engine runs the query.
fn no_sublinks(expr: &Expr) -> Expr {
    let mut copy = expr.clone();
    copy.each_mut(0, &mut |e, depth| {
        if depth == 0 && matches!(e.kind, ExprKind::SubLink(_)) {
            e.kind = ExprKind::Const(Value::Null);
            return true;
        }
        false
    });
    copy
}

/// True when two rows next to each other in `rows`, the rows of a query of [`sorted_rows`] in the order of its sort, have equal keys of the sort. The order of such rows is the order in which the plan of the query gives them to the sort, which the row engine must then give.
///
/// # Errors
///
/// The error of a comparison.
fn ties(query: &Query, rows: &[Vec<Value>], session: &dyn Session) -> Result<bool> {
    let shown: Vec<usize> =
        query.targets.iter().enumerate().filter(|(_, t)| !t.junk).map(|(i, _)| i).collect();
    let types: Vec<u32> = shown.iter().map(|&i| query.targets[i].expr.ty).collect();
    let mut list = Vec::with_capacity(query.sort.len());
    for g in &query.sort {
        let Some(at) = shown.iter().position(|&i| i == g.target) else { return Ok(true) };
        list.push(SortGroup { target: at, ..g.clone() });
    }
    let keys = sort::Keys::new(&list, &types, session)?;
    for pair in rows.windows(2) {
        if keys.compare(&pair[0], &pair[1])? == Ordering::Equal {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The table that keeps all its rows, and the rows and the places of the rows of the tuples of a left join of two tables with groups, such as the join of TPC-H Q13, or `None` when the join does not have this form. `ON` must be a list of `=` of the integer columns of the two tables and of conditions of the null side, and `WHERE` a list of conditions of the kept side. One of the two sides must have a key that no two rows have.
///
/// When the key is unique on the null side, each row of the kept side has one row of the null side, or the row of nulls. When the key is unique on the kept side, each row of the null side that matches gives a tuple, and then each row of the kept side with no row of the null side gives a tuple with the row of nulls. As in Postgres, the conditions of the null side in `ON` and the conditions of `WHERE` filter the rows of their tables before the join.
///
/// # Errors
///
/// The error of a condition of a table.
#[allow(clippy::type_complexity, clippy::cast_possible_truncation)]
fn left_join(
    query: &Query,
    join: &Join,
    params: &[Value],
    session: &dyn Session,
    cache: &Cache,
) -> Result<Option<(usize, Vec<Selected>, Vec<Vec<u32>>)>> {
    let (FromItem::Relation(a), FromItem::Relation(b)) = (&join.left, &join.right) else {
        return Ok(None);
    };
    let (kept, null) = match join.kind {
        JoinKind::Left => (*a, *b),
        JoinKind::Right => (*b, *a),
        _ => return Ok(None),
    };
    if query.relations.len() != 2 || kept == null || join.natural || !join.using.is_empty() {
        return Ok(None);
    }
    let mut own: [Vec<&Expr>; 2] = [Vec::new(), Vec::new()];
    let mut keys: [Vec<usize>; 2] = [Vec::new(), Vec::new()];
    let mut filter = Vec::new();
    if let Some(cond) = &query.filter {
        scan::conjuncts(cond, &mut filter);
    }
    for cond in filter {
        match reduce::relations_of(cond).as_slice() {
            [r] if *r == kept => own[kept].push(cond),
            _ => return Ok(None),
        }
    }
    let mut on = Vec::new();
    if let Some(cond) = &join.on {
        scan::conjuncts(cond, &mut on);
    }
    for cond in on {
        if let Some(sides) = reduce::equal_columns(cond) {
            for (relation, column) in sides {
                keys[relation].push(column);
            }
            continue;
        }
        match reduce::relations_of(cond).as_slice() {
            [r] if *r == null => own[null].push(cond),
            _ => return Ok(None),
        }
    }
    if keys[kept].is_empty() {
        return Ok(None);
    }
    let mut selected = Vec::with_capacity(2);
    for (index, relation) in query.relations.iter().enumerate() {
        match super::selected(relation, &own[index], &keys[index], params, session, cache, true)? {
            Some(found) if u32::try_from(found.places.len()).is_ok_and(|n| n < NONE) => {
                selected.push(found);
            }
            _ => return Ok(None),
        }
    }
    let key = |table: usize| -> Vec<&[Option<i64>]> {
        selected[table].keys.iter().map(Vec::as_slice).collect()
    };
    let mut pos = vec![Vec::new(), Vec::new()];
    if let Some(index) = Index::of(&key(null)) {
        let probe = key(kept);
        pos[kept] = (0..selected[kept].places.len() as u32).collect();
        pos[null] = (0..selected[kept].places.len()).map(|row| index.find(&probe, row)).collect();
    } else if let Some(index) = Index::of(&key(kept)) {
        let probe = key(null);
        let mut matched = vec![false; selected[kept].places.len()];
        for row in 0..selected[null].places.len() {
            let found = index.find(&probe, row);
            if found != NONE {
                matched[found as usize] = true;
                pos[kept].push(found);
                pos[null].push(row as u32);
            }
        }
        for (row, _) in matched.iter().enumerate().filter(|(_, m)| !**m) {
            pos[kept].push(row as u32);
            pos[null].push(NONE);
        }
    } else {
        return Ok(None);
    }
    Ok(Some((kept, selected, pos)))
}

/// The column `x` of the table `relation` and the integers of the subquery of a condition `x IN (SELECT ...)` that [`reduce::semi_join`] tests on the rows of the table, or `None` when `x` is not an integer column of the table or the subquery does not give integers. A row passes when its `x` is one of the integers: a null `x`, or an `x` that is not in the subquery, makes the condition null or false, and `WHERE` then drops the row.
///
/// # Errors
///
/// The error of the subquery.
fn semi_set(
    cond: &Expr,
    relation: usize,
    params: &[Value],
    session: &dyn Session,
    cache: &Cache,
) -> Result<Option<(usize, FxSet<i64>)>> {
    const INTEGERS: [u32; 3] = [oid::INT2, oid::INT4, oid::INT8];
    let ExprKind::SubLink(sub) = &cond.kind else { return Ok(None) };
    let Some(ExprKind::Func(f)) = sub.test.as_ref().map(|t| &t.kind) else { return Ok(None) };
    let column = f.args.iter().find_map(|arg| match &arg.kind {
        ExprKind::Var(var)
            if var.levels_up == 0 && var.relation == relation && INTEGERS.contains(&arg.ty) =>
        {
            usize::try_from(var.attnum - 1).ok()
        }
        _ => None,
    });
    let Some(column) = column else { return Ok(None) };
    match sub.query.targets.iter().find(|t| !t.junk) {
        Some(target) if INTEGERS.contains(&target.expr.ty) => {}
        _ => return Ok(None),
    }
    cache.unordered.set(true);
    let rows = crate::run_sub(&sub.query, params, session, cache, &[], false)?;
    let mut set = FxSet::default();
    for row in &rows {
        match row.first() {
            Some(Value::Int2(v)) => set.insert(i64::from(*v)),
            Some(Value::Int4(v)) => set.insert(i64::from(*v)),
            Some(Value::Int8(v)) => set.insert(*v),
            Some(Value::Null) => false,
            _ => return Ok(None),
        };
    }
    Ok(Some((column, set)))
}

/// Keeps the items of `items` whose place in `keep` is true.
fn retain<T>(items: &mut Vec<T>, keep: &[bool]) {
    let mut at = 0;
    items.retain(|_| {
        at += 1;
        keep[at - 1]
    });
}

/// True when the query has groups over a list of relations in `FROM`, with no set operation, no window and no write, and reads only the columns of its own relations. The only subqueries are the conditions `x IN (SELECT ...)` of `WHERE` that [`reduce::semi_join`] tests on the rows of one table, and the conditions of one table with an `EXISTS` that [`link`] tells.
fn shape(query: &Query, session: &dyn Session) -> bool {
    if query.relations.len() < 2
        || !(query.grouped || sorted_rows(query))
        || query.set_op.is_some()
        || !query.windows.is_empty()
        || !query.ctes.is_empty()
        || !query.row_marks.is_empty()
        || query.write.is_some()
        || !query.grouping_sets.is_empty()
        || query.with_ties
        || query.has_target_srfs()
        || !query.barrier.is_empty()
        // A table with row security is a subquery with the conditions of its policies, which the scans of the stored columns do not test.
        || query.relations.iter().any(|r| r.subquery.is_some())
    {
        return false;
    }
    let flat = query.from.len() == query.relations.len()
        && query.from.iter().all(|item| matches!(item, FromItem::Relation(_)));
    let outer = match query.from.as_slice() {
        [FromItem::Join(join)] => matches!(join.kind, JoinKind::Left | JoinKind::Right),
        _ => false,
    };
    if !flat && !outer {
        return false;
    }
    for item in &query.group {
        let expr = &query.targets[item.target].expr;
        // As in [`super::run`]: a `character` value with no length can differ from another in trailing spaces only.
        let padded = matches!(expr.kind, ExprKind::Var(_)) && expr.typmod > 4;
        if !plain(item.collation) || (expr.ty == oid::BPCHAR && !padded) {
            return false;
        }
    }
    let mut conds = Vec::new();
    if let Some(filter) = &query.filter {
        scan::conjuncts(filter, &mut conds);
    }
    conds.retain(|cond| reduce::semi_join(cond, session).is_none() && !link(cond));
    if let [FromItem::Join(join)] = query.from.as_slice() {
        conds.extend(&join.on);
    }
    // `HAVING` can have a subquery that runs one time, which [`finish`] runs before the groups.
    if let Some(having) = &query.having
        && having
            .find(0, &mut |e, depth| match &e.kind {
                ExprKind::SubLink(sub) if depth == 0 => (!once(sub)).then_some(()),
                ExprKind::Var(var) if depth == 0 && (var.levels_up != 0 || var.attnum <= 0) => {
                    Some(())
                }
                ExprKind::Agg(agg) if depth == 0 && agg.levels_up != 0 => Some(()),
                _ => None,
            })
            .is_some()
    {
        return false;
    }
    let exprs =
        query.targets.iter().map(|t| &t.expr).chain(conds).chain(&query.offset).chain(&query.limit);
    for expr in exprs {
        let other = expr.find(0, &mut |e, depth| match &e.kind {
            ExprKind::SubLink(_) => Some(()),
            ExprKind::Var(var) if var.levels_up != depth || var.attnum <= 0 => Some(()),
            _ => None,
        });
        if other.is_some() {
            return false;
        }
    }
    true
}

/// True when `cond` is a condition of `WHERE` that reads the tables of the query and has an `EXISTS` or a subquery of one value, which can run once as [`super::links_of`] tells, as the two `EXISTS` of TPC-H Q21 and the `avg` of Q17. The subquery reads only its own columns and the columns of the query.
fn link(cond: &Expr) -> bool {
    let mut exists = false;
    let other = cond.find(0, &mut |e, depth| match &e.kind {
        ExprKind::SubLink(_) if depth == 0 => {
            exists = true;
            None
        }
        ExprKind::Var(var)
            if var.levels_up > depth || (var.levels_up == depth && var.attnum <= 0) =>
        {
            Some(())
        }
        _ => None,
    });
    // A condition `x IN (SELECT ...)` of one table is a set of the keys of the table, as [`semi_set`] gives it.
    let semi = matches!(&cond.kind, ExprKind::SubLink(sub) if sub.kind == SubLinkKind::Any);
    exists
        && !semi
        && other.is_none()
        && !reduce::relations_of(cond).is_empty()
        && !unsupported(cond, true)
}

/// The rows of a table by the value of a key that no two rows have.
enum Index {
    /// The row of each key from `min`, or [`NONE`].
    Dense {
        min: i64,
        rows: Vec<u32>,
    },
    One(FxMap<i64, u32>),
    Two(FxMap<(i64, i64), u32>),
}

impl Index {
    /// The index of the key `keys`, one vector of the rows for each column of the key, or `None` when two rows have the same key or the key has more than two columns. A row with a null in its key is not in the index, because it joins no row.
    fn of(keys: &[&[Option<i64>]]) -> Option<Index> {
        Index::probed(keys, 0)
    }

    /// [`Index::of`] for a join that looks up the keys of `probes` rows. A key of one column whose values span no more than two slots for each lookup gets an array, as the cost to fill the array is less than the cost of the lookups in a hash table, as the `l_orderkey` of the orders of a year in TPC-H Q5. The array then has at most 2^24 slots.
    fn probed(keys: &[&[Option<i64>]], probes: usize) -> Option<Index> {
        match keys {
            [one] => {
                let (mut min, mut max, mut count) = (i64::MAX, i64::MIN, 0usize);
                for &k in one.iter().flatten() {
                    min = min.min(k);
                    max = max.max(k);
                    count += 1;
                }
                let span = max.checked_sub(min).and_then(|s| usize::try_from(s).ok());
                if let Some(span) = span
                    && ((span < (count * 8).max(1 << 16) && span < 1 << 27)
                        || span < probes.saturating_mul(2).min(1 << 24))
                {
                    let mut rows = vec![NONE; span + 1];
                    for (row, k) in one.iter().enumerate() {
                        let Some(k) = k else { continue };
                        let slot = &mut rows[usize::try_from(k - min).ok()?];
                        if *slot != NONE {
                            return None;
                        }
                        *slot = u32::try_from(row).ok()?;
                    }
                    return Some(Index::Dense { min, rows });
                }
                let mut map = FxMap::default();
                map.reserve(count);
                for (row, k) in one.iter().enumerate() {
                    let Some(k) = k else { continue };
                    if map.insert(*k, u32::try_from(row).ok()?).is_some() {
                        return None;
                    }
                }
                Some(Index::One(map))
            }
            [a, b] => {
                let mut map = FxMap::default();
                map.reserve(a.len());
                for (row, (x, y)) in a.iter().zip(b.iter()).enumerate() {
                    let (Some(x), Some(y)) = (x, y) else { continue };
                    if map.insert((*x, *y), u32::try_from(row).ok()?).is_some() {
                        return None;
                    }
                }
                Some(Index::Two(map))
            }
            _ => None,
        }
    }

    /// The row whose key is the key of the row `row` of `keys`, or [`NONE`].
    #[inline]
    fn find(&self, keys: &[&[Option<i64>]], row: usize) -> u32 {
        match (self, keys) {
            (Index::Dense { min, rows }, [one]) => one[row]
                .and_then(|k| usize::try_from(k.checked_sub(*min)?).ok())
                .and_then(|at| rows.get(at).copied())
                .unwrap_or(NONE),
            (Index::One(map), [one]) => one[row].and_then(|k| map.get(&k).copied()).unwrap_or(NONE),
            (Index::Two(map), [a, b]) => match (a[row], b[row]) {
                (Some(x), Some(y)) => map.get(&(x, y)).copied().unwrap_or(NONE),
                _ => NONE,
            },
            _ => NONE,
        }
    }
}

/// Two tables, or the places of the key columns of the two tables of an `=`.
type Pair = (usize, usize);

/// The join of a table to its parent in the tree through a key that is unique in the table.
struct Link {
    parent: usize,
    child: usize,
    /// The places in [`Selected::keys`] of the parent of the columns of the key.
    parent_keys: Vec<usize>,
    index: Index,
}

/// The tree of the joins of the tables.
struct Tree {
    root: usize,
    /// The links in the order of the join: the parent of each link comes before it.
    links: Vec<Link>,
    width: usize,
}

impl Tree {
    /// The tree of the joins of the `=` `edges` of the reduction, with each table as the root in turn from the table with the most rows, or `None` when no root gives a tree that reaches all the tables.
    fn of(edges: &[[(usize, usize); 2]], selected: &[Selected]) -> Option<Tree> {
        let width = selected.len();
        // The places of the key columns of the `=` of each pair of tables, with the table of the lower index first.
        let mut pairs: Vec<(Pair, Vec<Pair>)> = Vec::new();
        for &[(a, x), (b, y)] in edges {
            let (pair, columns) = if a < b { ((a, b), (x, y)) } else { ((b, a), (y, x)) };
            match pairs.iter_mut().find(|(p, _)| *p == pair) {
                Some((_, list)) if list.contains(&columns) => {}
                Some((_, list)) => list.push(columns),
                None => pairs.push((pair, vec![columns])),
            }
        }
        let mut order: Vec<usize> = (0..width).collect();
        order.sort_by_key(|&r| std::cmp::Reverse(selected[r].places.len()));
        for root in order {
            let mut done = vec![false; width];
            done[root] = true;
            let mut links = Vec::new();
            loop {
                let mut grew = false;
                for ((a, b), columns) in &pairs {
                    if done[*a] == done[*b] {
                        continue;
                    }
                    let (parent, child, parent_keys, child_keys): (usize, usize, Vec<_>, Vec<_>) =
                        if done[*a] {
                            (
                                *a,
                                *b,
                                columns.iter().map(|c| c.0).collect(),
                                columns.iter().map(|c| c.1).collect(),
                            )
                        } else {
                            (
                                *b,
                                *a,
                                columns.iter().map(|c| c.1).collect(),
                                columns.iter().map(|c| c.0).collect(),
                            )
                        };
                    let keys: Vec<&[Option<i64>]> =
                        child_keys.iter().map(|&k| selected[child].keys[k].as_slice()).collect();
                    let probes = selected[parent].places.len();
                    let Some(index) = Index::probed(&keys, probes) else { continue };
                    links.push(Link { parent, child, parent_keys, index });
                    done[child] = true;
                    grew = true;
                }
                if !grew {
                    break;
                }
            }
            if done.iter().all(|d| *d) {
                return Some(Tree { root, links: ordered(links, selected), width });
            }
        }
        None
    }

    /// True when the tree joins the tables `a` and `b`, so that it tests all their `=`.
    fn joins(&self, a: usize, b: usize) -> bool {
        self.links
            .iter()
            .any(|l| (l.parent == a && l.child == b) || (l.parent == b && l.child == a))
    }

    /// For each table, the place in its [`Selected::places`] of its row in each tuple of the join.
    fn join(&self, selected: &[Selected]) -> Vec<Vec<u32>> {
        let mut pos: Vec<Vec<u32>> = vec![Vec::new(); self.width];
        #[allow(clippy::cast_possible_truncation)]
        let all: Vec<u32> = (0..selected[self.root].places.len() as u32).collect();
        pos[self.root] = all;
        let mut have = vec![false; self.width];
        have[self.root] = true;
        for link in &self.links {
            let keys: Vec<&[Option<i64>]> = link
                .parent_keys
                .iter()
                .map(|&k| selected[link.parent].keys[k].as_slice())
                .collect();
            let mut found: Vec<u32> =
                pos[link.parent].iter().map(|&p| link.index.find(&keys, p as usize)).collect();
            if found.contains(&NONE) {
                for (table, places) in pos.iter_mut().enumerate() {
                    if have[table] {
                        let mut at = found.iter();
                        places.retain(|_| at.next().is_some_and(|&f| f != NONE));
                    }
                }
                found.retain(|&f| f != NONE);
            }
            pos[link.child] = found;
            have[link.child] = true;
        }
        pos
    }
}

/// The links in the order of the join: from the links whose parent has its rows, the link to the table that keeps the fewest of its rows first, so that the first lookups remove the most tuples.
fn ordered(mut links: Vec<Link>, selected: &[Selected]) -> Vec<Link> {
    let mut out = Vec::with_capacity(links.len());
    let mut have: Vec<bool> = vec![false; selected.len()];
    if let Some(first) = links.first() {
        have[first.parent] = true;
    }
    while !links.is_empty() {
        let kept = |l: &Link| {
            let s = &selected[l.child];
            #[allow(clippy::cast_precision_loss)]
            let share = s.places.len() as f64 / s.rows.max(1) as f64;
            share
        };
        let next = links
            .iter()
            .enumerate()
            .filter(|(_, l)| have[l.parent])
            .min_by(|(_, x), (_, y)| kept(x).total_cmp(&kept(y)))
            .map_or(0, |(i, _)| i);
        let link = links.remove(next);
        have[link.child] = true;
        out.push(link);
    }
    out
}

/// The values of the column `stored` at the rows `at`, with the null flag of each row when a row is null.
fn gather(stored: &Stored, at: &[usize]) -> Result<(Values, Option<Vec<bool>>)> {
    let ascending = at.windows(2).all(|w| w[0] <= w[1]);
    let mut decoded: Vec<Option<Decoded>> = Vec::new();
    decoded.resize_with(stored.rows.div_ceil(VECTOR), || None);
    let mut last = usize::MAX;
    let mut out = match stored.kind.store() {
        rupg_column::segment::Store::Int => Values::Ints(Vec::with_capacity(at.len())),
        rupg_column::segment::Store::Float => Values::Floats(Vec::with_capacity(at.len())),
        rupg_column::segment::Store::Code => Values::Codes(Vec::with_capacity(at.len())),
    };
    let mut nulls = Vec::with_capacity(at.len());
    let mut any = false;
    for &row in at {
        // The row of nulls of an outer join.
        if row == usize::MAX {
            nulls.push(true);
            any = true;
            match &mut out {
                Values::Ints(o) => o.push(0),
                Values::Floats(o) => o.push(0.0),
                Values::Codes(o) => o.push(0),
            }
            continue;
        }
        let vector = row / VECTOR;
        let Some(slot) = decoded.get(vector) else {
            return Err(Error::internal("a row of a join is not in its table"));
        };
        if slot.is_none() {
            // Rows in their order read each vector one time, so only the last vector stays.
            if ascending && let Some(old) = decoded.get_mut(last) {
                *old = None;
            }
            let segment = row / rupg_column::segment::ROWS;
            let in_segment = (row % rupg_column::segment::ROWS) / VECTOR;
            decoded[vector] = Some(decode(stored, segment, in_segment)?);
            last = vector;
        }
        let Some(d) = &decoded[vector] else {
            return Err(Error::internal("a vector is not decoded"));
        };
        let i = row % VECTOR;
        let null = d.nulls.as_ref().is_some_and(|n| n[i]);
        nulls.push(null);
        any |= null;
        match (&mut out, &d.data) {
            (Values::Ints(o), DecodedData::Int(v)) => o.push(v[i]),
            (Values::Floats(o), DecodedData::Float(v)) => o.push(v[i]),
            (Values::Codes(o), DecodedData::Code(v)) => o.push(v[i]),
            _ => return Err(Error::internal("a vector has another store than its column")),
        }
    }
    Ok((out, any.then_some(nulls)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_index_gives_the_one_row_of_each_key() {
        let keys = [Some(5), None, Some(9), Some(6)];
        let index = Index::of(&[&keys]).unwrap();
        assert!(matches!(index, Index::Dense { .. }));
        let probe = [Some(9), Some(7), None, Some(5)];
        let found: Vec<u32> = (0..4).map(|r| index.find(&[&probe], r)).collect();
        assert_eq!(found, vec![2, NONE, NONE, 0]);
        let wide = [Some(1), Some(1 << 40)];
        let index = Index::of(&[&wide]).unwrap();
        assert!(matches!(index, Index::One(_)));
        assert_eq!(index.find(&[&[Some(1 << 40)]], 0), 1);
        assert!(Index::of(&[&[Some(3), Some(3)]]).is_none());
        let (a, b) = ([Some(1), Some(1)], [Some(2), Some(3)]);
        let index = Index::of(&[&a, &b]).unwrap();
        assert_eq!(index.find(&[&[Some(1)], &[Some(3)]], 0), 1);
        assert!(Index::of(&[&[Some(1), Some(1)], &[Some(2), Some(2)]]).is_none());
    }
}
