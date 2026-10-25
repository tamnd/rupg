//! `pull_up_subqueries`: the subqueries in `FROM` that the planner merges into the query above them before it plans the query, as `pull_up_simple_subquery` merges them.
//!
//! The relations of a merged subquery come after the relations of the query, as the range table of the subquery comes after the range table of the query in PostgreSQL. The subquery takes its place in `FROM`, its `WHERE` joins the `WHERE` of the query or the `ON` of its join, and each column of the subquery becomes the expression of its target.
//!
//! Before that, each part of `WHERE` that is `EXISTS`, `NOT EXISTS` or `ANY` on a subquery that can be a join becomes a semi join or an anti join, as `pull_up_sublinks` makes it.

use std::cell::Cell;
use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::ptr;

use rupg_analyze::{
    BoolOp, BoolTest, Column, Expr, ExprKind, FromItem, Func, FuncForm, Join, JoinKind,
    PlaceHolder, Query, Relation, SubLink, SubLinkKind, Var, WriteKind,
};
use rupg_pgcatalog::builtin;
use rupg_types::{Value, oid};

use crate::{exists, exprs, minmax, pushdown, setcast};

/// The query with each simple subquery in `FROM` merged into the query above it, also in the subqueries of the query, and with the conditions of `WHERE` that the planner moves into the other subqueries of `FROM`.
pub fn pull_up(query: &Query) -> Query {
    let mut query = query.clone();
    merge_join(&mut query);
    flatten(&mut query);
    pull_all(&mut query);
    pushdown::push_all(&mut query);
    setcast::wrap_all(&mut query);
    exists::fill_all(&mut query);
    minmax::fill_all(&mut query);
    query
}

/// `transform_MERGE_to_join`: the join of a `MERGE` has the target table on its left side and the source on its right side. With `NOT MATCHED BY SOURCE`, the executor of PostgreSQL tests the whole row of the source, so the plan does not read the one-row `VALUES` that marks the source rows here. A `MERGE` in `WITH` changes too.
fn merge_join(query: &mut Query) {
    for cte in &mut query.ctes {
        merge_join(&mut cte.query);
    }
    if query.write.as_ref().is_none_or(|w| w.kind != WriteKind::Merge) {
        return;
    }
    view_rows(query);
    view_quals(query);
    let [FromItem::Join(join)] = query.from.as_mut_slice() else { return };
    if !matches!(join.right, FromItem::Relation(0)) {
        return;
    }
    std::mem::swap(&mut join.left, &mut join.right);
    join.kind = match join.kind {
        JoinKind::Left => JoinKind::Right,
        JoinKind::Right => JoinKind::Left,
        kind => kind,
    };
    let marker = query.relations.len().checked_sub(1);
    let FromItem::Join(source) = &join.right else { return };
    let (Some(marker), FromItem::Relation(r)) = (marker, &source.right) else { return };
    let relation = &query.relations[marker];
    let is_marker = *r == marker
        && source.kind == JoinKind::Inner
        && source.on.is_none()
        && relation.name == "*VALUES*"
        && relation.columns.len() == 1
        && relation.columns[0].name == "marker"
        && relation.values.as_ref().is_some_and(|v| v.len() == 1);
    if !is_marker {
        return;
    }
    join.right = source.left.clone();
    query.relations.pop();
    for target in &mut query.targets {
        if matches!(target.expr.kind, ExprKind::Var(v) if v.relation == marker && v.levels_up == 0)
        {
            target.expr.kind = ExprKind::Const(Value::Bool(true));
        }
    }
}

/// `transform_MERGE_to_join`: the conditions of an updatable view are the conditions of the target table below the join of the `MERGE`. When the join can give nulls for the target table, they join the `ON` of the join, and the one-row `VALUES` that holds them leaves the plan. The relation stays in the list of relations.
fn view_quals(query: &mut Query) {
    let [FromItem::Join(join)] = query.from.as_mut_slice() else { return };
    let join = &mut **join;
    if !matches!(join.kind, JoinKind::Inner | JoinKind::Left) {
        return;
    }
    let FromItem::Join(inner) = &mut join.right else { return };
    let (FromItem::Relation(0), FromItem::Relation(r)) = (&inner.left, &inner.right) else {
        return;
    };
    let relation = &query.relations[*r];
    let helper = inner.kind == JoinKind::Inner
        && relation.name == "*VALUES*"
        && relation.columns.len() == 1
        && relation.columns[0].name == "one"
        && relation.values.as_ref().is_some_and(|v| v.len() == 1);
    if !helper {
        return;
    }
    let quals = inner.on.take();
    join.right = FromItem::Relation(0);
    if let Some(quals) = quals {
        join.on = Some(match join.on.take() {
            Some(on) => and(on, Some(quals)),
            None => quals,
        });
    }
}

/// `transform_MERGE_to_join`: the target of a `MERGE` into a view with triggers `INSTEAD OF` is the subquery of the view, and a whole row of the subquery tells a row of the view from a source row with no match. The one-row `VALUES` that tells them apart here leaves the plan, and its column becomes a value of the subquery that the join gives nulls for. The relation stays in the list of relations.
fn view_rows(query: &mut Query) {
    let [FromItem::Join(join)] = query.from.as_mut_slice() else { return };
    let join = &mut **join;
    let item =
        if matches!(join.right, FromItem::Join(_)) { &mut join.right } else { &mut join.left };
    let FromItem::Join(inner) = item else { return };
    let FromItem::Relation(r) = inner.right else { return };
    // The target is the subquery of the view, or its join with the conditions of an updatable view above it.
    let target = match &inner.left {
        FromItem::Relation(0) => true,
        FromItem::Join(quals) => matches!(quals.left, FromItem::Relation(0)),
        FromItem::Relation(_) => false,
    };
    let relation = &query.relations[r];
    let helper = target
        && inner.kind == JoinKind::Inner
        && inner.on.is_none()
        && relation.name == "*VALUES*"
        && relation.columns.len() == 1
        && relation.columns[0].name == "ctid"
        && relation.values.as_ref().is_some_and(|v| v.len() == 1);
    if !helper {
        return;
    }
    *item = std::mem::replace(&mut inner.left, FromItem::Relation(0));
    let mut row = |e: &mut Expr, depth: usize| {
        if let ExprKind::Var(v) = e.kind
            && v.relation == r
            && v.levels_up == depth
        {
            let value = Expr::new(ExprKind::Const(Value::Int8(0)), oid::INT8);
            let p = PlaceHolder { expr: value, relations: vec![0], levels_up: depth };
            *e = Expr::new(ExprKind::PlaceHolder(Box::new(p)), oid::INT8);
            return true;
        }
        false
    };
    query.each_expr_mut(0, &mut |e, depth| e.each_mut(depth, &mut row));
}

/// `flatten_simple_union_all`: a query that is a `UNION ALL` of `SELECT`s with the types of its columns becomes a query that reads the `UNION ALL` as its one relation, so that the planner plans an append relation. Only a query with `ORDER BY` changes, as the plan of the append relation is the same as the plan of the set operation when no order is necessary. With `ORDER BY`, the planner can then use a `Merge Append` of the plans of the `SELECT`s in the order.
fn flatten(query: &mut Query) {
    if query.sort.is_empty()
        || query.recursive
        || !query.ctes.is_empty()
        || !query.row_marks.is_empty()
        || query.targets.iter().any(|t| t.junk)
    {
        return;
    }
    let mut inner = Query {
        sort: Vec::new(),
        limit: None,
        offset: None,
        with_ties: false,
        relations: std::mem::take(&mut query.relations),
        set_op: query.set_op.take(),
        targets: query.targets.clone(),
        ..Query::default()
    };
    if !crate::from::simple_union_all(&inner) {
        query.relations = std::mem::take(&mut inner.relations);
        query.set_op = inner.set_op.take();
        return;
    }
    let columns = inner
        .targets
        .iter()
        .map(|t| Column {
            name: t.name.clone(),
            ty: t.expr.ty,
            typmod: t.expr.typmod,
            not_null: false,
        })
        .collect();
    for (at, target) in query.targets.iter_mut().enumerate() {
        let attnum = i16::try_from(at + 1).unwrap_or(i16::MAX);
        let mut var =
            Expr::new(ExprKind::Var(Var { relation: 0, attnum, levels_up: 0 }), target.expr.ty);
        var.typmod = target.expr.typmod;
        target.expr = var;
    }
    let name = inner.relations.first().map(|r| r.name.clone()).unwrap_or_default();
    query.relations = vec![Relation {
        oid: 0,
        columns,
        subquery: Some(Box::new(inner)),
        function: None,
        values: None,
        name,
        alias: None,
        lateral: false,
        cte: None,
        tablesample: None,
        only: false,
        perm: None,
    }];
    query.from = vec![FromItem::Relation(0)];
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
    // PostgreSQL pulls up the subqueries of `WHERE` before the subqueries of `FROM`. Here the subqueries of `FROM` merge first, so that they keep their places in the list of `FROM`, and the subquery of an `ANY` then merges as the inner side of its semi join.
    if pull_sublinks(query) {
        while let Some((r, top)) = candidate(query) {
            merge(query, r, top);
        }
    }
}

/// `pull_up_sublinks`: each part of `WHERE` that is `EXISTS`, `NOT EXISTS` or `ANY` on a subquery becomes a semi join or an anti join of the items of `FROM` with the relations of the subquery. The joins come in the order of the parts, and each join has the join before it as its left side. True when a part became a join. `NOT IN` is an anti join only when no value of the test can be null. The planner does not convert a part of `ON` or an `ANY` on a correlated subquery yet.
fn pull_sublinks(query: &mut Query) -> bool {
    if query.set_op.is_some()
        || checks_wrapper(query)
        || !query.row_marks.is_empty()
        || query.from.is_empty()
    {
        return false;
    }
    let Some(filter) = query.filter.take() else { return false };
    let mut parts = Vec::new();
    pushdown::owned_parts(filter, &mut parts);
    let (mut keep, mut joins) = (Vec::new(), Vec::new());
    for part in parts {
        match sublink_join(query, &part) {
            Some(join) => joins.push(join),
            None => keep.push(part),
        }
    }
    query.filter = match keep.len() {
        0 => None,
        1 => keep.pop(),
        _ => Some(Expr::new(ExprKind::Bool(BoolOp::And, keep), oid::BOOL)),
    };
    if joins.is_empty() {
        return false;
    }
    let items = std::mem::take(&mut query.from).into_iter();
    let Some(left) = items.reduce(|l, r| join_item(JoinKind::Inner, l, r, None)) else {
        return false;
    };
    let item = joins
        .into_iter()
        .fold(left, |left, (kind, right, on)| join_item(kind, left, right, Some(on)));
    query.from = vec![item];
    true
}

/// The semi join or the anti join of a part of `WHERE`: its kind, its inner side and its condition.
fn sublink_join(query: &mut Query, part: &Expr) -> Option<(JoinKind, FromItem, Expr)> {
    match &part.kind {
        ExprKind::SubLink(link) if link.kind == SubLinkKind::Exists => {
            exists_join(query, &link.query, JoinKind::Semi)
        }
        ExprKind::SubLink(link) if link.kind == SubLinkKind::Any => any_join(query, link),
        ExprKind::Bool(BoolOp::Not, args) => match args.as_slice() {
            [Expr { kind: ExprKind::SubLink(link), .. }] if link.kind == SubLinkKind::Exists => {
                exists_join(query, &link.query, JoinKind::Anti)
            }
            [Expr { kind: ExprKind::SubLink(link), .. }] if link.kind == SubLinkKind::Any => {
                not_any_join(query, link)
            }
            _ => None,
        },
        _ => None,
    }
}

/// `convert_EXISTS_sublink_to_join`: the relations of the subquery that [`exists::simplify`] makes join the relations of the query, and the `WHERE` of the subquery is the condition of the join. The rest of the subquery reads no column of the query. The `WHERE` reads a column of the query and calls no volatile function. The subquery has no `WITH` and has items in `FROM`.
fn exists_join(
    query: &mut Query,
    sub: &Query,
    kind: JoinKind,
) -> Option<(JoinKind, FromItem, Expr)> {
    if !sub.ctes.is_empty() {
        return None;
    }
    let mut sub = exists::simplify(sub)?;
    let mut filter = sub.filter.take()?;
    if sub.from.is_empty()
        || exprs::correlated(&sub, 1)
        || exprs::relations_at(&filter, 1).is_empty()
        || exprs::volatile(&filter)
    {
        return None;
    }
    let base = query.relations.len();
    move_up(&mut sub, base);
    filter.each_mut(0, &mut moved(base));
    let items = std::mem::take(&mut sub.from).into_iter();
    let right = items.reduce(|l, r| join_item(JoinKind::Inner, l, r, None))?;
    query.relations.append(&mut sub.relations);
    query.pulled += sub.pulled;
    Some((kind, right, filter))
}

/// `convert_ANY_sublink_to_join`: the subquery of `ANY` is a relation of the query, and the test of `ANY` on the columns of the relation is the condition of the join. A subquery that reads a column of the query is a `LATERAL` relation, which the planner converts only when [`candidate`] then merges it. The test reads a column of the query, calls no volatile function and has no subquery.
fn any_join(query: &mut Query, link: &SubLink) -> Option<(JoinKind, FromItem, Expr)> {
    let test = link.test.as_ref()?;
    let sub = &link.query;
    let sublink = test.find(0, &mut |e, _| matches!(e.kind, ExprKind::SubLink(_)).then_some(()));
    let lateral = exprs::correlated(sub, 1);
    if lateral && !(simple(sub) && sub.from.len() == 1 && lateral_fits(sub, None))
        || exprs::relations(test).is_empty()
        || exprs::volatile(test)
        || sublink.is_some()
        || sub.targets.iter().any(|t| t.junk)
    {
        return None;
    }
    let r = query.relations.len();
    let mut on = test.clone();
    on.each_mut(0, &mut |e, depth| {
        if let ExprKind::SubColumn(at) = e.kind
            && depth == 0
        {
            let attnum = i16::try_from(at + 1).unwrap_or(i16::MAX);
            e.kind = ExprKind::Var(Var { relation: r, attnum, levels_up: 0 });
        }
        false
    });
    let columns = sub
        .targets
        .iter()
        .map(|t| Column {
            name: t.name.clone(),
            ty: t.expr.ty,
            typmod: t.expr.typmod,
            not_null: false,
        })
        .collect();
    query.relations.push(Relation {
        oid: 0,
        columns,
        subquery: Some(Box::new(sub.clone())),
        function: None,
        values: None,
        name: "unnamed_subquery".to_string(),
        alias: None,
        lateral,
        cte: None,
        tablesample: None,
        only: false,
        perm: None,
    });
    Some((JoinKind::Semi, FromItem::Relation(r), on))
}

/// `convert_ANY_sublink_to_join` under `NOT`: `NOT (x op ANY (subquery))` is the anti join of [`any_join`] when the test is not null for each row, so that the rows with no match are the rows that pass. Each operator of the test is in a btree or a hash family, each value of the query that it compares is not null, and each column of the subquery is not null, as `sublink_testexpr_is_not_nullable` and `query_outputs_are_not_nullable` prove it.
fn not_any_join(query: &mut Query, link: &SubLink) -> Option<(JoinKind, FromItem, Expr)> {
    let test = link.test.as_ref()?;
    if !test_not_null(query, test) || !outputs_not_null(&link.query) {
        return None;
    }
    let (_, right, on) = any_join(query, link)?;
    Some((JoinKind::Anti, right, on))
}

/// `sublink_testexpr_is_not_nullable`: true when the test of `ANY` is one operator, the operators of an `AND` or an `OR`, or a row comparison, where each operator is in a btree or a hash family, as `op_is_safe_index_member`, and each value of the query on the left side is not null.
fn test_not_null(query: &Query, test: &Expr) -> bool {
    let ops: Vec<&Expr> = match &test.kind {
        ExprKind::Func(_) => vec![test],
        ExprKind::Bool(BoolOp::And | BoolOp::Or, args) => args.iter().collect(),
        ExprKind::RowCompare(rc) => rc.ops.iter().collect(),
        _ => return false,
    };
    let mut nullable = Vec::new();
    for item in &query.from {
        nullable_places(item, false, &mut nullable);
    }
    ops.into_iter().all(|op| {
        let ExprKind::Func(Func { form: FuncForm::Operator(operator), args, .. }) = &op.kind else {
            return false;
        };
        let safe = builtin::amops_of_operator(*operator).any(|a| matches!(a.method, 403 | 405));
        matches!(args.as_slice(), [outer, _] if safe && not_null(query, &nullable, outer))
    })
}

/// `query_outputs_are_not_nullable`: true when no column of the query can be null. The query has no set operation and no grouping sets. A column is an expression that [`not_null`] proves, or a column of a relation that a condition of `WHERE` or of an inner join is false or null for when the column is null, as `find_nonnullable_vars` finds it.
fn outputs_not_null(sub: &Query) -> bool {
    if sub.set_op.is_some() || !sub.grouping_sets.is_empty() {
        return false;
    }
    let mut nullable = Vec::new();
    for item in &sub.from {
        nullable_places(item, false, &mut nullable);
    }
    let mut safe: Option<BTreeSet<(usize, i16)>> = None;
    sub.targets.iter().filter(|t| !t.junk).all(|t| {
        let mut expr = &t.expr;
        while let ExprKind::Relabel(arg, _) = &expr.kind {
            expr = arg;
        }
        if not_null(sub, &nullable, expr) {
            return true;
        }
        let ExprKind::Var(var) = expr.kind else { return false };
        let safe = safe.get_or_insert_with(|| {
            let mut quals = Vec::new();
            for item in &sub.from {
                safe_quals(item, &mut quals);
            }
            quals.extend(&sub.filter);
            quals.into_iter().flat_map(|q| nonnullable_vars(q, true)).collect()
        });
        var.levels_up == 0 && safe.contains(&(var.relation, var.attnum))
    })
}

/// `find_subquery_safe_quals`: the conditions of `ON` that the rows of the relations outside the nullable sides of the outer joins pass.
fn safe_quals<'q>(item: &'q FromItem, out: &mut Vec<&'q Expr>) {
    let FromItem::Join(join) = item else { return };
    match join.kind {
        JoinKind::Inner => {
            safe_quals(&join.left, out);
            safe_quals(&join.right, out);
            out.extend(&join.on);
        }
        JoinKind::Left | JoinKind::Semi | JoinKind::Anti => safe_quals(&join.left, out),
        JoinKind::Right | JoinKind::RightSemi | JoinKind::RightAnti => {
            safe_quals(&join.right, out);
        }
        JoinKind::Full => {}
    }
}

/// `expr_is_nonnullable`: true when the expression cannot be null. A column is a column of a table with `NOT NULL`, outside the nullable side of an outer join of `FROM`, which `nullable` has, or a system column. A constant is not null, `COALESCE`, `GREATEST` and `LEAST` have an argument that is not null, and `CASE` has an `ELSE` and results that are not null. An array, `IS NULL`, a boolean test and `IS DISTINCT FROM` are never null.
fn not_null(query: &Query, nullable: &[usize], expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Var(var) => {
            var.levels_up == 0
                && !nullable.contains(&var.relation)
                && var.attnum != 0
                && column_not_null(&query.relations, var.relation, var.attnum)
        }
        ExprKind::Const(value) => !matches!(value, Value::Null),
        ExprKind::Coalesce(args) | ExprKind::MinMax { args, .. } => {
            args.iter().any(|a| not_null(query, nullable, a))
        }
        ExprKind::Case(case) => {
            not_null(query, nullable, &case.default)
                && case.whens.iter().all(|(_, result)| not_null(query, nullable, result))
        }
        ExprKind::Array { .. }
        | ExprKind::NullTest(..)
        | ExprKind::BooleanTest(..)
        | ExprKind::Distinct { .. } => true,
        ExprKind::Relabel(arg, _) => not_null(query, nullable, arg),
        _ => false,
    }
}

/// True for a column of a table with `NOT NULL` or a primary key, or a system column of a table, as `find_relation_notnullatts` finds them.
pub(crate) fn column_not_null(relations: &[Relation], r: usize, attnum: i16) -> bool {
    let Some(relation) = relations.get(r) else { return false };
    let table = relation.oid != 0
        && relation.subquery.is_none()
        && relation.function.is_none()
        && relation.values.is_none()
        && relation.cte.is_none();
    let column = usize::try_from(attnum - 1).ok().and_then(|at| relation.columns.get(at));
    table && (attnum < 0 || column.is_some_and(|c| c.not_null))
}

/// `find_nonnullable_vars`: the columns that cannot be null in a row that passes the condition. At the top of the condition, `IS NOT NULL` and the other tests that are false for a null count too, and each part of an `AND` counts. Below the top, an `AND` and an `OR` give the columns of each of their parts.
pub(crate) fn nonnullable_vars(expr: &Expr, top: bool) -> BTreeSet<(usize, i16)> {
    let strict = |func: u32| builtin::proc_by_oid(func).is_some_and(|p| p.strict);
    let union = |args: &[Expr], top: bool| {
        args.iter().flat_map(|a| nonnullable_vars(a, top)).collect::<BTreeSet<_>>()
    };
    match &expr.kind {
        ExprKind::Var(var) if var.levels_up == 0 => BTreeSet::from([(var.relation, var.attnum)]),
        ExprKind::Bool(BoolOp::And, args) if top => union(args, top),
        ExprKind::Bool(BoolOp::And | BoolOp::Or, args) => {
            let mut sets = args.iter().map(|a| nonnullable_vars(a, top));
            let first = sets.next().unwrap_or_default();
            sets.fold(first, |acc, s| acc.intersection(&s).copied().collect())
        }
        ExprKind::Bool(BoolOp::Not, args) => union(args, false),
        ExprKind::Func(f) if strict(f.oid) => union(&f.args, false),
        ExprKind::ScalarArrayOp { func, args, .. } if strict(*func) => union(args, false),
        ExprKind::Relabel(arg, _) | ExprKind::ArrayCoerce { arg, .. } => nonnullable_vars(arg, top),
        ExprKind::CoerceViaIo(arg, _) => nonnullable_vars(arg, false),
        ExprKind::PlaceHolder(p) if p.levels_up == 0 => nonnullable_vars(&p.expr, top),
        ExprKind::NullTest(arg, false) if top => nonnullable_vars(arg, false),
        ExprKind::BooleanTest(
            arg,
            BoolTest::IsTrue | BoolTest::IsFalse | BoolTest::IsNotUnknown,
        ) if top => nonnullable_vars(arg, false),
        _ => BTreeSet::new(),
    }
}

/// A join of two items of `FROM` that the planner makes, with no `USING` and no alias.
fn join_item(kind: JoinKind, left: FromItem, right: FromItem, on: Option<Expr>) -> FromItem {
    FromItem::Join(Box::new(Join {
        kind,
        left,
        right,
        on,
        using: Vec::new(),
        plain_using: false,
        alias: None,
        using_alias: None,
        merged: Vec::new(),
        columns: Vec::new(),
        aliases: Vec::new(),
        sources: Vec::new(),
    }))
}

/// A column of a subquery as a column of the query above it, where the relations of the subquery come after the `base` relations of that query, and a column of a query outside the subquery is one level nearer.
fn moved(base: usize) -> impl FnMut(&mut Expr, usize) -> bool {
    move |e: &mut Expr, depth: usize| {
        match &mut e.kind {
            ExprKind::Var(v) if v.levels_up == depth => v.relation += base,
            ExprKind::Var(v) if v.levels_up > depth => v.levels_up -= 1,
            ExprKind::Agg(agg) if agg.levels_up > depth => agg.levels_up -= 1,
            ExprKind::Grouping(g) if g.levels_up > depth => g.levels_up -= 1,
            ExprKind::PlaceHolder(p) if p.levels_up == depth => {
                p.relations.iter_mut().for_each(|k| *k += base);
            }
            ExprKind::PlaceHolder(p) if p.levels_up > depth => p.levels_up -= 1,
            _ => {}
        }
        false
    }
}

/// The expressions and the items of `FROM` of a subquery as [`moved`] makes them.
fn move_up(sub: &mut Query, base: usize) {
    let mut inner = moved(base);
    sub.each_expr_mut(0, &mut |e, depth| e.each_mut(depth, &mut inner));
    join_columns(&mut sub.from, &mut |e| e.each_mut(0, &mut inner));
    renumber(&mut sub.from, &|j| j + base);
}

/// `is_safe_append_member`: a `SELECT` of a `UNION ALL` that the planner makes a member of the append relation. It is a simple subquery of one relation with no `WHERE` of its own. The planner plans another `SELECT` as a subquery, with no order of its rows.
pub(crate) fn member(leaf: &Query) -> bool {
    simple(leaf)
        && (leaf.filter.is_none() || leaf.pushed)
        && matches!(leaf.from.as_slice(), [FromItem::Relation(_)])
}

/// `is_simple_subquery`: a subquery that the planner can merge into the query above it. It has no set operation, no aggregate, no window, no function that gives a set in its targets, no `ORDER BY`, no `DISTINCT`, no `LIMIT`, no `OFFSET`, no row lock, no `WITH` and no volatile function in its targets. A subquery with no `FROM` is an `RTE_RESULT` that [`merge`] drops, as `remove_useless_result_rtes` drops it.
pub(crate) fn simple(sub: &Query) -> bool {
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
        && !sub.targets.iter().any(|t| t.junk || exprs::volatile(&t.expr))
}

/// Each relation of `FROM`, with true when it is an item of the list of `FROM` and true when a condition of the relation can join the `ON` of a join above it or the `WHERE` of the query. The condition joins the `ON` of a join that has the relation in a side of an inner join or in the side of an outer join that can give nulls, and it goes past a join that keeps the rows of the side of the relation. It cannot go past a full join.
fn places(item: &FromItem, top: bool, on: bool, out: &mut Vec<(usize, bool, bool)>) {
    match item {
        FromItem::Relation(r) => out.push((*r, top, on)),
        FromItem::Join(join) => {
            let (left, right) = match join.kind {
                JoinKind::Inner | JoinKind::Semi | JoinKind::RightSemi => (true, true),
                JoinKind::Left | JoinKind::Anti => (on, true),
                JoinKind::Right | JoinKind::RightAnti => (true, on),
                JoinKind::Full => (false, false),
            };
            places(&join.left, false, left, out);
            places(&join.right, false, right, out);
        }
    }
}

/// The first subquery of `FROM` that the planner merges, with true when it is an item of the list of `FROM`. The planner does not merge a view or a query of `WITH` yet, and it merges a `LATERAL` subquery when [`lateral_fits`] lets it and the query does not read a target of the subquery that reads the query below an outer join that gives nulls for the subquery. Below the side of an outer join that can give nulls, a target of the subquery that is not a column becomes a `PlaceHolderVar`, which the planner makes here only for a subquery with a `FROM`, for a reference of the query itself and for a target with no subquery. In a join, the subquery has one item in `FROM`, and its `WHERE` joins the `ON` of the join, so that the subquery is not the side of an outer join that keeps its rows or a side of a full join when it has a `WHERE`. The query does not read the whole row of the subquery, which `pullup_replace_vars` makes a `ROW` of the targets.
fn candidate(query: &Query) -> Option<(usize, bool)> {
    if query.set_op.is_some()
        || checks_wrapper(query)
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
    // The target table of an `UPDATE`, a `DELETE` or a `MERGE` is the relation 0, which stays.
    let target = query.write.as_ref().is_some_and(|w| w.kind != WriteKind::Insert);
    found.into_iter().find_map(|(r, top, on)| {
        if target && r == 0 {
            return None;
        }
        let relation = query.relations.get(r)?;
        let sub = pullable(relation)?;
        let used = used(query, r);
        if relation.lateral {
            let safe = query.from.iter().find_map(|item| lowest_outer(item, r))?;
            let reads = |attnum: &i16| {
                let at = usize::try_from(attnum - 1).ok();
                at.and_then(|at| sub.targets.get(at))
                    .is_some_and(|t| !exprs::relations_at(&t.expr, 1).is_empty())
            };
            if !lateral_fits(sub, safe.as_ref()) || nullable.contains(&r) && used.iter().any(reads)
            {
                return None;
            }
        }
        let target =
            |attnum: i16| usize::try_from(attnum - 1).ok().and_then(|at| sub.targets.get(at));
        let column = |attnum: &i16| {
            target(*attnum)
                .is_some_and(|t| matches!(t.expr.kind, ExprKind::Var(v) if v.levels_up == 0))
        };
        let wrappable = |attnum: &i16| {
            target(*attnum).is_some_and(|t| {
                t.expr
                    .find(0, &mut |e, _| matches!(e.kind, ExprKind::SubLink(_)).then_some(()))
                    .is_none()
            })
        };
        let columns = !nullable.contains(&r)
            || used_from(query, r, 1).iter().all(column)
                && (!sub.from.is_empty() && used.iter().all(wrappable) || used.iter().all(column));
        let fits = top
            || sub.from.len() == 1 && (sub.filter.is_none() || on)
            || sub.from.is_empty() && query.from.iter().any(|item| removable(item, r));
        (simple(sub) && columns && fits && !used.contains(&0)).then_some((r, top))
    })
}

/// True for the query of a write with check constraints, whose one relation is the query of the rows, as [`crate::unwrap_checks`] finds it. The planner plans the query of the rows.
fn checks_wrapper(query: &Query) -> bool {
    !ptr::eq(crate::unwrap_checks(query), query)
}

/// The relations of `FROM` that an outer join can give nulls for.
pub(crate) fn nullable_places(item: &FromItem, nullable: bool, out: &mut Vec<usize>) {
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

/// The subquery of a relation that is a subquery in `FROM`, not a view or a query of `WITH`. A `LATERAL` subquery has no function in `FROM`, as the planner does not mark the functions `LATERAL` when it merges the subquery.
fn pullable(relation: &Relation) -> Option<&Query> {
    let plain = relation.oid == 0 && relation.cte.is_none();
    relation.subquery.as_deref().filter(|sub| {
        plain && !(relation.lateral && sub.relations.iter().any(|r| r.function.is_some()))
    })
}

/// The relations of the lowest outer join above the relation `r`, as `lowest_outer_join` of `pull_up_subqueries_recurse` finds it, with `Some(None)` when no outer join is above the relation and `None` when the item does not have it. A semi join and an anti join are outer joins here.
fn lowest_outer(item: &FromItem, r: usize) -> Option<Option<BTreeSet<usize>>> {
    match item {
        FromItem::Relation(k) => (*k == r).then_some(None),
        FromItem::Join(join) => {
            let below = lowest_outer(&join.left, r).or_else(|| lowest_outer(&join.right, r))?;
            if below.is_some() || join.kind == JoinKind::Inner {
                return Some(below);
            }
            let mut all = Vec::new();
            pushdown::rels(item, &mut all);
            Some(Some(all.into_iter().collect()))
        }
    }
}

/// `jointree_contains_lateral_outer_refs` and the check of the targets of a `LATERAL` subquery in `is_simple_subquery`: true when the `WHERE`, the conditions of the joins and the targets of the subquery read only the relations of the query in `safe`, which are the relations of the lowest outer join above the subquery. With no outer join above it, `safe` is `None` and they can read any relation of the query. The conditions of an outer join of the subquery, and the conditions below it, read no relation of the query.
fn lateral_fits(sub: &Query, safe: Option<&BTreeSet<usize>>) -> bool {
    fn within(expr: &Expr, safe: Option<&BTreeSet<usize>>) -> bool {
        safe.is_none_or(|safe| exprs::relations_at(expr, 1).is_subset(safe))
    }
    fn items(item: &FromItem, safe: Option<&BTreeSet<usize>>) -> bool {
        let FromItem::Join(join) = item else { return true };
        let none = BTreeSet::new();
        let safe = if join.kind == JoinKind::Inner { safe } else { Some(&none) };
        items(&join.left, safe)
            && items(&join.right, safe)
            && join.on.as_ref().is_none_or(|on| within(on, safe))
    }
    sub.from.iter().all(|item| items(item, safe))
        && sub.filter.as_ref().is_none_or(|filter| within(filter, safe))
        && sub.targets.iter().all(|t| within(&t.expr, safe))
}

/// The numbers of the columns of the relation `r` that the query reads, with 0 for the whole row. The columns of a join do not count, because the expressions of the query read the columns of the relations.
pub(crate) fn used(query: &Query, r: usize) -> Vec<i16> {
    used_from(query, r, 0)
}

/// [`used`] for the expressions that are `below` or more subqueries below the query.
fn used_from(query: &Query, r: usize, below: usize) -> Vec<i16> {
    let mut out = Vec::new();
    let mut add = |e: &Expr, depth: usize| {
        if let ExprKind::Var(v) = e.kind
            && v.levels_up == depth
            && depth >= below
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
    let mut nullable = Vec::new();
    for item in &query.from {
        nullable_places(item, false, &mut nullable);
    }
    let nulled = nullable.contains(&r);
    let mut plain = Vec::new();
    for item in &query.from {
        plain_on(item, r, &mut plain);
    }
    let relation = query.relations.remove(r);
    let lateral = relation.lateral;
    let Some(sub) = relation.subquery else { return };
    let mut sub = *sub;
    if lateral {
        // A column of a relation of the query after the subquery moves down one place.
        let mut shift = |e: &mut Expr, depth: usize| {
            match &mut e.kind {
                ExprKind::Var(v) if v.levels_up == depth + 1 && v.relation > r => v.relation -= 1,
                ExprKind::PlaceHolder(p) if p.levels_up == depth + 1 => {
                    p.relations.iter_mut().filter(|k| **k > r).for_each(|k| *k -= 1);
                }
                _ => {}
            }
            false
        };
        sub.each_expr_mut(0, &mut |e, depth| e.each_mut(depth, &mut shift));
        join_columns(&mut sub.from, &mut |e| e.each_mut(0, &mut shift));
    }
    // `replace_empty_jointree`: a subquery with no `FROM` has an `RTE_RESULT` in its range table.
    let result = usize::from(sub.from.is_empty() && sub.pulled == 0);
    move_up(&mut sub, query.relations.len());
    let mut inside = Vec::new();
    for item in &sub.from {
        pushdown::rels(item, &mut inside);
    }
    // `pullup_replace_vars`: a column of the subquery becomes the expression of its target, and the relations after the subquery move down one place. Above an outer join that gives nulls for the subquery, a target that can be not null for a null row is a `PlaceHolderVar`, and `substitute_phv_relids` gives a `PlaceHolderVar` of the relation the relations of the subquery.
    let targets: Vec<Expr> = sub.targets.iter().map(|t| t.expr.clone()).collect();
    let wrap = Cell::new(false);
    let mut outer = |e: &mut Expr, depth: usize| {
        if let ExprKind::PlaceHolder(p) = &mut e.kind
            && p.levels_up == depth
        {
            let mut relations: Vec<usize> = p
                .relations
                .iter()
                .flat_map(|&k| match k.cmp(&r) {
                    Ordering::Less => vec![k],
                    Ordering::Equal => inside.clone(),
                    Ordering::Greater => vec![k - 1],
                })
                .collect();
            relations.sort_unstable();
            relations.dedup();
            p.relations = relations;
            return false;
        }
        let ExprKind::Var(v) = &mut e.kind else { return false };
        if v.levels_up != depth {
            return false;
        }
        if v.relation == r {
            let at = usize::try_from(v.attnum - 1).ok();
            if let Some(target) = at.and_then(|at| targets.get(at)) {
                let mut new = target.clone();
                if wrap.get() && needs_placeholder(&new) {
                    let (ty, typmod) = (new.ty, new.typmod);
                    let p = PlaceHolder { expr: new, relations: inside.clone(), levels_up: 0 };
                    new = Expr::new(ExprKind::PlaceHolder(Box::new(p)), ty);
                    new.typmod = typmod;
                }
                new.raise(depth);
                *e = new;
                return true;
            }
        } else if v.relation > r {
            v.relation -= 1;
        }
        false
    };
    query.each_expr_mut(0, &mut |e, depth| {
        wrap.set(nulled && !plain.contains(&ptr::from_ref(&*e)));
        e.each_mut(depth, &mut outer);
    });
    wrap.set(nulled);
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
    } else if sub.from.is_empty() {
        // The join stays in the range table of PostgreSQL.
        query.pulled += 1;
        for item in &mut query.from {
            if lift(item, &mut |item, quals| drop_result(item, gone, quals), &mut sub.filter) {
                break;
            }
        }
    } else {
        add_on(&mut query.from, gone, &mut sub.filter);
        take_place(&mut query.from, gone, &mut sub.from.pop());
    }
    query.relations.append(&mut sub.relations);
    query.pulled += 1 + sub.pulled + result;
    if let Some(filter) = sub.filter {
        query.filter = Some(and(filter, query.filter.take()));
    }
}

/// The conditions of `ON` that read the values of the relation `r` with no outer join between them that gives nulls for it: the `ON` of each join of `r` with no outer join below it that gives nulls for `r`, as the `varnullingrels` of the columns of `r` in the condition are empty. Gives true when an outer join in the item gives nulls for `r`, or `None` when the item does not have `r`.
fn plain_on(item: &FromItem, r: usize, out: &mut Vec<*const Expr>) -> Option<bool> {
    match item {
        FromItem::Relation(k) => (*k == r).then_some(false),
        FromItem::Join(join) => {
            let (nulled, left) = match (plain_on(&join.left, r, out), plain_on(&join.right, r, out))
            {
                (Some(nulled), _) => (nulled, true),
                (None, Some(nulled)) => (nulled, false),
                (None, None) => return None,
            };
            if !nulled && let Some(on) = &join.on {
                out.push(ptr::from_ref(on));
            }
            let nulls = match join.kind {
                JoinKind::Full => true,
                JoinKind::Left => !left,
                JoinKind::Right => left,
                _ => false,
            };
            Some(nulled || nulls)
        }
    }
}

/// True when a target of a subquery needs a `PlaceHolderVar` above an outer join that gives nulls for the subquery, as `pullup_replace_vars_callback` finds it. A column and a `PlaceHolderVar` of the query do not, and neither does an expression that reads one of them with no construct that can give a value for a null.
fn needs_placeholder(target: &Expr) -> bool {
    match &target.kind {
        ExprKind::Var(v) => v.levels_up != 0,
        ExprKind::PlaceHolder(p) => p.levels_up != 0,
        _ => {
            let reads = target.find(0, &mut |e, depth| match &e.kind {
                ExprKind::Var(v) if v.levels_up == depth => Some(()),
                ExprKind::PlaceHolder(p) if p.levels_up == depth => Some(()),
                _ => None,
            });
            reads.is_none() || exprs::nonstrict(target)
        }
    }
}

/// The condition `first AND then`.
pub(crate) fn and(first: Expr, then: Option<Expr>) -> Expr {
    match then {
        None => first,
        Some(then) => Expr::new(ExprKind::Bool(BoolOp::And, vec![first, then]), oid::BOOL),
    }
}

/// The condition of the relation `r` joins the `ON` of a join above it, as [`places`] finds the join, or stays in `filter` for the `WHERE` of the query.
fn add_on(items: &mut [FromItem], r: usize, filter: &mut Option<Expr>) {
    let mut found = |item: &mut FromItem, _: &mut Option<Expr>| matches!(item, FromItem::Relation(at) if *at == r);
    for item in items {
        if lift(item, &mut found, filter) {
            return;
        }
    }
}

/// True when `found` takes the item or a part of it. The condition stays in `filter` when it goes past the item.
fn lift(
    item: &mut FromItem,
    found: &mut impl FnMut(&mut FromItem, &mut Option<Expr>) -> bool,
    filter: &mut Option<Expr>,
) -> bool {
    if found(item, filter) {
        return true;
    }
    let FromItem::Join(join) = item else { return false };
    let join = &mut **join;
    let (on, here) = if lift(&mut join.left, found, filter) {
        let keeps = matches!(join.kind, JoinKind::Left | JoinKind::Anti | JoinKind::Full);
        (!keeps, true)
    } else if lift(&mut join.right, found, filter) {
        let keeps = matches!(join.kind, JoinKind::Right | JoinKind::RightAnti | JoinKind::Full);
        (!keeps, true)
    } else {
        (false, false)
    };
    if on && let Some(filter) = filter.take() {
        join.on = Some(and(filter, join.on.take()));
    }
    here
}

/// `remove_useless_results_recurse`: true when the relation `r` is a side of an inner join that the planner drops, as it drops an inner join with an `RTE_RESULT` on a side. The conditions of the join go to the join above it or to `WHERE`, so that no join above it is a full join, a semi join or an anti join.
fn removable(item: &FromItem, r: usize) -> bool {
    let FromItem::Join(join) = item else { return false };
    let side = |item: &FromItem| matches!(item, FromItem::Relation(at) if *at == r);
    match join.kind {
        JoinKind::Inner if side(&join.left) || side(&join.right) => true,
        JoinKind::Inner | JoinKind::Left | JoinKind::Right => {
            removable(&join.left, r) || removable(&join.right, r)
        }
        _ => false,
    }
}

/// `remove_useless_results_recurse`: when the item is the inner join that [`removable`] finds, the other side of the join takes its place, and the condition of the join comes after `quals`.
fn drop_result(item: &mut FromItem, r: usize, quals: &mut Option<Expr>) -> bool {
    let FromItem::Join(join) = item else { return false };
    let side = |item: &FromItem| matches!(item, FromItem::Relation(at) if *at == r);
    if join.kind != JoinKind::Inner || !side(&join.left) && !side(&join.right) {
        return false;
    }
    let join = &mut **join;
    let other = if side(&join.left) { &mut join.right } else { &mut join.left };
    let other = std::mem::replace(other, FromItem::Relation(r));
    if let Some(on) = join.on.take() {
        *quals = Some(match quals.take() {
            Some(quals) => and(quals, Some(on)),
            None => on,
        });
    }
    *item = other;
    true
}
