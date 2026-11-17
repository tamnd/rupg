//! The join of the tables of a query with groups through keys that are unique on one side, on the columns of the tables (spec/14 section 14.8).
//!
//! Each table keeps the rows that pass its own conditions of `WHERE`, as [`super::selected`] finds them with the vectors. One table is the root: the other tables join it through a tree of `=` of integer keys, and the key of each `=` is unique in the rows of the table further from the root, as a primary key of a dimension table is. For each row of the root, a lookup in an array or in a hash table of the key of each table gives the one row of that table, or no row, and then the row of the root does not join. The join so gives the places of the rows of each table, and no tuple of values. The columns that the query reads are then gathered at these places into columns of the join, and the vector engine runs the groups and the aggregates on them as on the columns of one table, with the other conditions of `WHERE`.
//!
//! The groups of the vector engine do not depend on the order of the rows, and the query has the plan of the join, so that the groups come in the order of the hash table of the plan. When the sort of the query gives the order of the rows, as [`Order`] tells, the query has no plan, and the planner does not run.

use std::sync::Arc;

use std::cmp::Ordering;

use rupg_analyze::{
    BoolOp, Expr, ExprKind, FromItem, Join, JoinKind, Query, SortGroup, SubLinkKind,
};
use rupg_column::builder::{self, Values};
use rupg_column::segment::Kind;
use rupg_column::stored::{Stored, VECTOR};
use rupg_common::{Error, Result, SqlState};
use rupg_func::Session;
use rupg_summary::zone::{in_span, spans};
use rupg_types::{Value, oid};

use super::{
    Decoded, DecodedData, FxSet, Link as Ran, Selected, Source, decode, having_once, kind_of,
    numeric_kind, once, plain, unsupported,
};
use crate::{Cache, reduce, scan, sort};

/// No row.
const NONE: u32 = u32::MAX;

/// The least number of rows of the table that [`run`] reads after the other tables when no table has a condition with a subquery.
const DEFER_ROWS: usize = 1 << 16;

/// The most integers of a subquery of `x IN (SELECT ...)` that [`run`] tests as a condition `x = ANY (array)` of the scan of the table of `x`, and not as a set on the rows that the scan gives.
const SEMI_ANY: usize = 4096;

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
    // A table with a condition with a subquery, as `lineitem l1` of TPC-H Q21, is read after the other tables. The other tables first lose the rows that join no row of a table next to them, as [`semi_reduce`] gives them, and then the keys of their rows select the rows of the table before its subqueries run, as [`key_filters`] gives them. So the subqueries run only on the rows that can join, as in a plan of PostgreSQL that joins the tables before it tests the subqueries.
    let mut late: Vec<bool> =
        (0..width).map(|i| links.iter().any(|c| reduce::relations_of(c) == [i])).collect();
    // When no table has a condition with a subquery, the table with the most rows, as `lineitem` of TPC-H Q5, is read after the other tables in the same way. So the keys of the rows that the other tables keep select its rows, and the scan makes the keys of fewer rows and the join looks up fewer rows. A table of fewer than [`DEFER_ROWS`] rows is read with the others.
    if width > 1 && !late.contains(&true) {
        let mut most = (0, 0);
        for (index, relation) in query.relations.iter().enumerate() {
            if let Some((rows, _)) = session.table_small_columns(relation.oid, &[])?
                && rows > most.0
            {
                most = (rows, index);
            }
        }
        if most.0 >= DEFER_ROWS {
            late[most.1] = true;
        }
    }
    let mut tables: Vec<Option<Selected>> = (0..width).map(|_| None).collect();
    for pass in [false, true] {
        if pass {
            if !late.contains(&true) {
                break;
            }
            semi_reduce(&plan.edges, &late, &mut tables);
        }
        for (index, relation) in query.relations.iter().enumerate() {
            if late[index] != pass {
                continue;
            }
            let filters =
                if pass { key_filters(index, &plan, &conds, &tables) } else { Vec::new() };
            // A condition `x IN (SELECT ...)` of the table: the subquery runs one time, and its integers then select the rows that pass the other conditions.
            let (semi, mut own): (Vec<&Expr>, Vec<&Expr>) = plan.own[index]
                .iter()
                .copied()
                .partition(|c| matches!(c.kind, ExprKind::SubLink(_)));
            let mut keys = plan.keys[index].clone();
            let mut sets = Vec::with_capacity(semi.len());
            // A subquery of at most [`SEMI_ANY`] integers, as the 57 orders of TPC-H Q18 at SF1, is a condition `x = ANY (array)` of the scan, which the zones of `x` test before the scan reads the other columns.
            let mut anys = Vec::new();
            for cond in semi {
                let Some((column, set, inner)) = semi_set(cond, index, params, session, cache)?
                else {
                    return Ok(None);
                };
                if set.len() <= SEMI_ANY {
                    let mut ints: Vec<i64> = set.iter().copied().collect();
                    ints.sort_unstable();
                    if let Some(any) = crate::lift::any_of(inner, ints) {
                        anys.push(any);
                        continue;
                    }
                }
                sets.push((keys.len(), set));
                keys.push(column);
            }
            own.extend(anys.iter());
            own.extend(filters.iter());
            own.extend(links.iter().filter(|c| reduce::relations_of(c) == [index]));
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
                    tables[index] = Some(found);
                }
                _ => return Ok(None),
            }
        }
    }
    let Some(selected) = tables.into_iter().collect::<Option<Vec<Selected>>>() else {
        return Ok(None);
    };
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
    let order = Order::of(query);
    // When the rows can have ties, the run gives one row more than `LIMIT`, so that a row equal to the last row in the keys of the sort shows.
    let check = order == Order::Ties;
    let mut limit = None;
    if check && let Some(Expr { kind: ExprKind::Const(Value::Int8(n)), .. }) = &mut virt.limit {
        limit = usize::try_from(*n).ok();
        *n = n.saturating_add(1);
    }
    let want = if check { want.map(|w| w.saturating_add(1)) } else { want };
    match order {
        // The groups come in the order of the hash table of the plan of the join.
        Order::Plan => session.share_plan(&virt, Some(query)),
        Order::Sorted | Order::Ties => session.no_plan(&virt),
    }
    let out = super::run_on(&virt, params, session, cache, want, source);
    session.share_plan(&virt, None);
    let out = match out {
        // With no plan, the run can read the groups and the tuples in another order than the plan, so that an expression can fail on another row first. The row engine then runs the query in the order of the plan and gives the error of PostgreSQL. A cancel stops the query.
        Err(error) if order != Order::Plan && error.state() != SqlState::QUERY_CANCELED => {
            return Ok(None);
        }
        out => out?,
    };
    let Some(mut out) = out else { return Ok(None) };
    if check {
        if ties(query, &out, session)? {
            return Ok(None);
        }
        out.truncate(limit.unwrap_or(usize::MAX).min(want.map_or(usize::MAX, |w| w - 1)));
    }
    Ok(Some(out))
}

/// How [`finish`] gives the rows of the join in the order of the query.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Order {
    /// The groups come in the order of the hash table of the plan of the join.
    Plan,
    /// The query has one group, or the sort gives one order of the groups, as each key of `GROUP BY` is a key of the sort, so that two groups have different keys of the sort. The order of the groups before the sort does not change the rows.
    Sorted,
    /// The sort gives the order of the rows when [`ties`] finds no two rows with equal keys of the sort. Else the row engine runs the query.
    Ties,
}

impl Order {
    /// The order of the rows of `query`, a query of [`shape`]. With [`Order::Sorted`] and [`Order::Ties`] the run of the vectors has no plan, so the planner does not run. The row engine can run the query again after an error of a run with no plan, so a query with groups and a volatile function keeps the plan. A query with no groups has no volatile function, as [`sorted_rows`] tells.
    fn of(query: &Query) -> Order {
        if !query.grouped {
            return Order::Ties;
        }
        let mut exprs = query.targets.iter().map(|t| &t.expr).chain(&query.filter);
        let volatile = exprs.any(crate::volatile)
            || query.having.as_ref().is_some_and(crate::volatile)
            || query.from.iter().any(|item| match item {
                FromItem::Join(join) => join.on.as_ref().is_some_and(crate::volatile),
                _ => false,
            });
        if volatile || !query.grouping_sets.is_empty() {
            return Order::Plan;
        }
        if query.group.is_empty() {
            return Order::Sorted;
        }
        if query.sort.is_empty() {
            return Order::Plan;
        }
        if query.group.iter().all(|g| query.sort.iter().any(|s| s.target == g.target)) {
            return Order::Sorted;
        }
        let limit = query
            .limit
            .as_ref()
            .is_none_or(|l| matches!(l.kind, ExprKind::Const(Value::Int8(n)) if n >= 0));
        if query.offset.is_none() && limit { Order::Ties } else { Order::Plan }
    }
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
        pos[null] = index.find_all(&probe, &pos[kept]);
    } else if let Some(index) = Index::of(&key(kept)) {
        let probe = key(null);
        let mut matched = vec![false; selected[kept].places.len()];
        let rows: Vec<u32> = (0..selected[null].places.len() as u32).collect();
        for (row, found) in index.find_all(&probe, &rows).into_iter().enumerate() {
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

/// The column `x` of the table `relation`, the integers of the subquery and the expression of `x` of a condition `x IN (SELECT ...)` that [`reduce::semi_join`] tests on the rows of the table, or `None` when `x` is not an integer column of the table or the subquery does not give integers. A row passes when its `x` is one of the integers: a null `x`, or an `x` that is not in the subquery, makes the condition null or false, and `WHERE` then drops the row.
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
) -> Result<Option<(usize, FxSet<i64>, Expr)>> {
    const INTEGERS: [u32; 3] = [oid::INT2, oid::INT4, oid::INT8];
    let ExprKind::SubLink(sub) = &cond.kind else { return Ok(None) };
    let Some(ExprKind::Func(f)) = sub.test.as_ref().map(|t| &t.kind) else { return Ok(None) };
    let column = f.args.iter().find_map(|arg| match &arg.kind {
        ExprKind::Var(var)
            if var.levels_up == 0 && var.relation == relation && INTEGERS.contains(&arg.ty) =>
        {
            usize::try_from(var.attnum - 1).ok().map(|column| (column, arg))
        }
        _ => None,
    });
    let Some((column, arg)) = column else { return Ok(None) };
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
    Ok(Some((column, set, arg.clone())))
}

/// Keeps the items of `items` whose place in `keep` is true.
fn retain<T>(items: &mut Vec<T>, keep: &[bool]) {
    let mut at = 0;
    items.retain(|_| {
        at += 1;
        keep[at - 1]
    });
}

/// Removes from the rows of each table that is not `late` the rows whose key of an `=` of `edges` with another table that is not `late` is not the key of a row of that table, until no row goes. The joins are inner joins, so these rows are in no tuple.
///
/// An `=` removes rows of one table, the target, with the keys of the other table, the source. It runs again only when the source lost rows after its last run, as the same keys remove no more rows. A source that keeps all the rows of its table does not run, as its keys remove few rows in most joins, for example when the target has a foreign key to it, and the set of all its keys costs the most. This is correct, as the join tests each `=` on the tuples.
fn semi_reduce(edges: &[[(usize, usize); 2]], late: &[bool], tables: &mut [Option<Selected>]) {
    // Each `=` in each direction: the target, the place of its key, the source and the place of its key.
    let mut arcs = Vec::new();
    for &[(a, x), (b, y)] in edges {
        if !late[a] && !late[b] {
            arcs.push((a, x, b, y));
            arcs.push((b, y, a, x));
        }
    }
    // The rows of the source of each arc at its last run.
    let mut ran = vec![usize::MAX; arcs.len()];
    loop {
        let mut removed = false;
        for (&(to, at, from, key), ran) in arcs.iter().zip(&mut ran) {
            let (Some(source), Some(target)) = (&tables[from], &tables[to]) else { continue };
            if source.places.len() >= source.rows || *ran == source.places.len() {
                continue;
            }
            *ran = source.places.len();
            let set = KeySet::of(&source.keys[key]);
            let keep: Vec<bool> = target.keys[at].iter().map(|k| set.has(*k)).collect();
            if keep.iter().all(|k| *k) {
                continue;
            }
            if let Some(target) = &mut tables[to] {
                retain(&mut target.places, &keep);
                for keys in &mut target.keys {
                    retain(keys, &keep);
                }
                removed = true;
            }
        }
        if !removed {
            break;
        }
    }
}

/// The integer keys of the rows of a table, with no null: a bitmap from the least key when the keys make one, as [`spans`] tells, or else a hash set.
enum KeySet {
    Span(i64, Vec<u64>),
    Hash(FxSet<i64>),
}

impl KeySet {
    /// The set of the keys `keys`.
    fn of(keys: &[Option<i64>]) -> KeySet {
        let (mut least, mut most, mut count) = (i64::MAX, i64::MIN, 0usize);
        for &k in keys.iter().flatten() {
            least = least.min(k);
            most = most.max(k);
            count += 1;
        }
        if count == 0 || !spans(least, most, count) {
            return KeySet::Hash(keys.iter().flatten().copied().collect());
        }
        let mut bits = vec![0u64; (most.wrapping_sub(least).cast_unsigned() / 64 + 1) as usize];
        for &k in keys.iter().flatten() {
            let at = k.wrapping_sub(least).cast_unsigned();
            bits[(at / 64) as usize] |= 1 << (at % 64);
        }
        KeySet::Span(least, bits)
    }

    /// True when the key `key` is in the set. A null is in no set.
    #[inline]
    fn has(&self, key: Option<i64>) -> bool {
        match (self, key) {
            (_, None) => false,
            (KeySet::Span(least, bits), Some(k)) => in_span(*least, bits, k),
            (KeySet::Hash(set), Some(k)) => set.contains(&k),
        }
    }
}

/// The conditions `column = ANY (array)` of the table `index`, which is read after the other tables of `tables`: for each `=` of the table with a table that keeps fewer rows than the relation has, the keys of the rows that it keeps, which a row of the table must have to join. The conditions with the fewest keys come first. An `=` with more than 65536 keys gives no condition, as the test of a key then costs more than it saves, except when the keys make a bitmap of [`rupg_summary::zone::Test::Span`], whose test of a key reads one bit, and the table keeps at most a quarter of the rows of its relation, so that the test removes most rows.
fn key_filters(
    index: usize,
    plan: &reduce::Plan<'_>,
    conds: &[&Expr],
    tables: &[Option<Selected>],
) -> Vec<Expr> {
    const MOST: usize = 1 << 16;
    let mut out: Vec<(usize, Expr)> = Vec::new();
    for &[(a, x), (b, y)] in &plan.edges {
        let (at, other, key) = match (a == index, b == index) {
            (true, false) => (x, b, y),
            (false, true) => (y, a, x),
            _ => continue,
        };
        let Some(source) = &tables[other] else { continue };
        if source.places.len() >= source.rows {
            continue;
        }
        let column = plan.keys[index][at];
        let Some(inner) = conds.iter().find_map(|c| key_column(c, index, column)) else {
            continue;
        };
        let mut keys: Vec<i64> = source.keys[key].iter().flatten().copied().collect();
        keys.sort_unstable();
        keys.dedup();
        if keys.len() > MOST
            && (source.places.len().saturating_mul(4) > source.rows
                || !keys
                    .first()
                    .zip(keys.last())
                    .is_some_and(|(&least, &most)| spans(least, most, keys.len())))
        {
            continue;
        }
        let n = keys.len();
        if let Some(cond) = crate::lift::any_of(inner.clone(), keys) {
            out.push((n, cond));
        }
    }
    out.sort_by_key(|(n, _)| *n);
    out.into_iter().map(|(_, cond)| cond).collect()
}

/// The column `column` of the relation `relation` in the `=` `cond` of the columns of two relations, with no `Relabel`, or `None` when `cond` does not have it.
fn key_column(cond: &Expr, relation: usize, column: usize) -> Option<&Expr> {
    reduce::equal_columns(cond)?;
    let ExprKind::Func(f) = &cond.kind else { return None };
    f.args.iter().find_map(|arg| {
        let mut e = arg;
        while let ExprKind::Relabel(inner, _) = &e.kind {
            e = inner;
        }
        match e.kind {
            ExprKind::Var(var)
                if var.levels_up == 0
                    && var.relation == relation
                    && usize::try_from(var.attnum - 1).ok() == Some(column) =>
            {
                Some(e)
            }
            _ => None,
        }
    })
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
    One(Flat<1>),
    Two(Flat<2>),
}

/// The number of rows that [`Index::find_all`] looks up in one step. It first finds the slot of each row of the step and starts the read of the slot, and then it reads the slots, so the misses of the cache overlap.
const STEP: usize = 32;

/// The largest array of [`Index::Dense`] that [`Index::find_all`] reads with no prefetch, as an array of this size stays in the cache of level 2.
const SMALL: usize = 1 << 16;

/// A hash table of the keys of `W` integers with the row of each key, with open addressing and linear probing. A slot has the key and the row together, so a lookup reads one line of the cache in most cases. The table has at least twice as many slots as keys. A slot with the row [`NONE`] has no key.
struct Flat<const W: usize> {
    /// 64 less the number of the bits of the place of a slot, which come from the high bits of the hash.
    shift: u32,
    slots: Vec<([i64; W], u32)>,
}

impl<const W: usize> Flat<W> {
    /// An empty table with room for `count` keys.
    fn with(count: usize) -> Self {
        let size = count.saturating_mul(2).max(16).next_power_of_two();
        Flat { shift: 64 - size.trailing_zeros(), slots: vec![([0; W], NONE); size] }
    }

    /// The first slot to read for `key`. The hash multiplies by the golden ratio, and the high bits of the product give the place, as in Fibonacci hashing.
    #[inline]
    fn place(&self, key: &[i64; W]) -> usize {
        let mut hash = 0_u64;
        for &k in key {
            hash = (hash.rotate_left(32) ^ k.cast_unsigned()).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        }
        usize::try_from(hash >> self.shift).unwrap_or(0)
    }

    /// Puts `row` in for `key`. False when the table has the key already.
    fn insert(&mut self, key: [i64; W], row: u32) -> bool {
        let mask = self.slots.len() - 1;
        let mut at = self.place(&key);
        loop {
            let slot = &mut self.slots[at];
            if slot.1 == NONE {
                *slot = (key, row);
                return true;
            }
            if slot.0 == key {
                return false;
            }
            at = (at + 1) & mask;
        }
    }

    /// The row of `key`, or [`NONE`], from the slot `at` that [`Flat::place`] gives.
    #[inline]
    fn get_at(&self, key: &[i64; W], mut at: usize) -> u32 {
        let mask = self.slots.len() - 1;
        loop {
            let (k, row) = &self.slots[at];
            if *row == NONE || k == key {
                return *row;
            }
            at = (at + 1) & mask;
        }
    }

    /// The row of the key of each row of `rows`, or [`NONE`], with `key` to get the key of a row. The table finds the slots of [`STEP`] rows and starts their reads before it compares a key.
    fn find_all(&self, rows: &[u32], key: impl Fn(usize) -> Option<[i64; W]>, out: &mut Vec<u32>) {
        let mut at = [usize::MAX; STEP];
        for step in rows.chunks(STEP) {
            let mut keys = [[0; W]; STEP];
            for (i, &row) in step.iter().enumerate() {
                at[i] = match key(row as usize) {
                    Some(k) => {
                        let place = self.place(&k);
                        rupg_kernels::hash::prefetch(&self.slots[place]);
                        keys[i] = k;
                        place
                    }
                    None => usize::MAX,
                };
            }
            for (&place, k) in at[..step.len()].iter().zip(&keys) {
                out.push(if place == usize::MAX { NONE } else { self.get_at(k, place) });
            }
        }
    }
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
                let mut flat = Flat::with(count);
                for (row, k) in one.iter().enumerate() {
                    let Some(k) = k else { continue };
                    if !flat.insert([*k], u32::try_from(row).ok()?) {
                        return None;
                    }
                }
                Some(Index::One(flat))
            }
            [a, b] => {
                let mut flat = Flat::with(a.len());
                for (row, (x, y)) in a.iter().zip(b.iter()).enumerate() {
                    let (Some(x), Some(y)) = (x, y) else { continue };
                    if !flat.insert([*x, *y], u32::try_from(row).ok()?) {
                        return None;
                    }
                }
                Some(Index::Two(flat))
            }
            _ => None,
        }
    }

    /// The row whose key is the key of the row `row` of `keys`, or [`NONE`].
    #[cfg(test)]
    fn find(&self, keys: &[&[Option<i64>]], row: usize) -> u32 {
        #[allow(clippy::cast_possible_truncation)]
        self.find_all(keys, &[row as u32])[0]
    }

    /// [`Index::find`] for each row of `rows`, in order. The kind of the index is matched once for all the rows.
    fn find_all(&self, keys: &[&[Option<i64>]], rows: &[u32]) -> Vec<u32> {
        let mut out = Vec::with_capacity(rows.len());
        match (self, keys) {
            (Index::Dense { min, rows: slots }, [one]) => {
                let slot = |row: u32| {
                    one[row as usize]
                        .and_then(|k| usize::try_from(k.checked_sub(*min)?).ok())
                        .filter(|&at| at < slots.len())
                };
                if slots.len() <= SMALL {
                    out.extend(rows.iter().map(|&row| slot(row).map_or(NONE, |at| slots[at])));
                } else {
                    let mut at = [usize::MAX; STEP];
                    for step in rows.chunks(STEP) {
                        for (i, &row) in step.iter().enumerate() {
                            at[i] = slot(row).unwrap_or(usize::MAX);
                            if let Some(s) = slots.get(at[i]) {
                                rupg_kernels::hash::prefetch(s);
                            }
                        }
                        out.extend(
                            at[..step.len()].iter().map(|&a| slots.get(a).copied().unwrap_or(NONE)),
                        );
                    }
                }
            }
            (Index::One(flat), [one]) => flat.find_all(rows, |r| one[r].map(|k| [k]), &mut out),
            (Index::Two(flat), [a, b]) => flat.find_all(rows, |r| Some([a[r]?, b[r]?]), &mut out),
            _ => out.resize(rows.len(), NONE),
        }
        out
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
            let mut found = link.index.find_all(&keys, &pos[link.parent]);
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

    #[test]
    fn a_batch_of_lookups_gives_the_row_of_each_key() {
        let keys: Vec<Option<i64>> = (0..1000).map(|k| (k % 7 != 0).then_some(k << 33)).collect();
        let index = Index::of(&[&keys]).unwrap();
        assert!(matches!(index, Index::One(_)));
        let probe: Vec<Option<i64>> = (0..1100).rev().map(|k| Some(k << 33)).collect();
        let rows: Vec<u32> = (0..1100).collect();
        let found = index.find_all(&[&probe], &rows);
        for (row, &got) in found.iter().enumerate() {
            let k = 1099 - row;
            let want = if k < 1000 && k % 7 != 0 { k as u32 } else { NONE };
            assert_eq!(got, want);
        }
        let (a, b): (Vec<_>, Vec<_>) = (0..500).map(|k| (Some(k / 10), Some(k % 10))).unzip();
        let index = Index::of(&[&a, &b]).unwrap();
        let found = index.find_all(&[&b, &a], &[3, 0, 499]);
        assert_eq!(found, vec![30, 0, NONE]);
    }

    #[test]
    fn a_key_set_has_the_same_keys_as_a_bitmap_and_as_a_hash() {
        let probe: Vec<Option<i64>> = [i64::MIN, -301, -300, -1, 0, 63, 64, 1 << 20, i64::MAX]
            .into_iter()
            .map(Some)
            .chain([None])
            .collect();
        for keys in [
            vec![Some(-300), None, Some(0), Some(64), Some(-300)],
            vec![Some(-1), Some(1 << 20)],
            vec![Some(i64::MIN), Some(i64::MAX), Some(63)],
            vec![None],
            vec![],
        ] {
            let set = KeySet::of(&keys);
            let (least, most) = (keys.iter().flatten().min(), keys.iter().flatten().max());
            let dense = least
                .zip(most)
                .and_then(|(least, most)| most.checked_sub(*least))
                .is_some_and(|span| span < 1 << 18);
            assert_eq!(matches!(set, KeySet::Span(..)), dense, "{keys:?}");
            for &k in &probe {
                assert_eq!(set.has(k), k.is_some() && keys.contains(&k), "{keys:?} {k:?}");
            }
        }
    }

    #[test]
    fn a_semi_reduction_keeps_the_rows_that_join_and_skips_a_full_source() {
        let table = |rows: usize, places: Vec<usize>, keys: Vec<Vec<Option<i64>>>| {
            Some(Selected { rows, places, keys })
        };
        // a(0) = b(0) and b(1) = c(0). a loses its row with a null key, so its keys then remove the row of b with the key 40. c keeps one row of two, so b keeps its rows with the key 7, and in the second round a keeps its rows with the keys of those rows of b.
        let mut tables = vec![
            table(4, vec![0, 1, 2, 3], vec![vec![Some(10), Some(20), None, Some(30)]]),
            table(
                5,
                vec![0, 1, 2, 3],
                vec![
                    vec![Some(10), Some(20), Some(30), Some(40)],
                    vec![Some(7), Some(8), Some(7), None],
                ],
            ),
            table(2, vec![1], vec![vec![Some(7)]]),
        ];
        let edges = [[(0, 0), (1, 0)], [(1, 1), (2, 0)]];
        semi_reduce(&edges, &[false; 3], &mut tables);
        let [Some(a), Some(b), Some(c)] = &tables[..] else { panic!("a table is gone") };
        assert_eq!(b.places, vec![0, 2]);
        assert_eq!(b.keys, vec![vec![Some(10), Some(30)], vec![Some(7), Some(7)]]);
        assert_eq!(a.places, vec![0, 3]);
        assert_eq!(a.keys, vec![vec![Some(10), Some(30)]]);
        assert_eq!(c.places, vec![1]);
        // a keeps all its rows and loses none, so its keys do not run, and b keeps its row with the key 40, which joins no row of a.
        let mut tables = vec![
            table(2, vec![0, 1], vec![vec![Some(1), Some(5)]]),
            table(9, vec![0, 1, 2], vec![vec![Some(1), Some(5), Some(40)]]),
        ];
        semi_reduce(&[[(0, 0), (1, 0)]], &[false; 2], &mut tables);
        assert_eq!(tables[0].as_ref().unwrap().places, vec![0, 1]);
        assert_eq!(tables[1].as_ref().unwrap().places, vec![0, 1, 2]);
        // A `late` table takes no part.
        let mut tables = vec![
            table(3, vec![0, 1, 2], vec![vec![Some(1), Some(2), Some(3)]]),
            table(9, vec![0], vec![vec![Some(2)]]),
        ];
        semi_reduce(&[[(0, 0), (1, 0)]], &[false, true], &mut tables);
        assert_eq!(tables[0].as_ref().unwrap().places, vec![0, 1, 2]);
    }
}
