//! `preprocess_minmax_aggregates` of `planagg.c`: the form of a query whose aggregate calls are each `min` or `max` of one table, with a subquery for each call that reads the first value in the order of the aggregate, which an index can give.

use rupg_analyze::{
    BoolOp, Expr, ExprKind, FromItem, MinMax, Query, SortGroup, SubLink, SubLinkKind, Target,
    equality_for_ordering,
};
use rupg_pgcatalog::builtin;
use rupg_types::{Value, oid};

use crate::{exprs, pullup};

/// Fills [`Query::minmax`] for the query and for its subqueries, as `grouping_planner` calls `preprocess_minmax_aggregates` for each query level.
pub(crate) fn fill_all(query: &mut Query) {
    for relation in &mut query.relations {
        if let Some(sub) = &mut relation.subquery {
            fill_all(sub);
        }
    }
    for cte in &mut query.ctes {
        fill_all(&mut cte.query);
    }
    for expr in query.exprs_mut() {
        expr.each_mut(0, &mut |e, depth| {
            if depth == 0
                && let ExprKind::SubLink(link) = &mut e.kind
            {
                fill_all(&mut link.query);
            }
            false
        });
    }
    query.minmax = minmax(query).map(Box::new);
}

/// `preprocess_minmax_aggregates`: the subqueries of a query with aggregate calls and no `GROUP BY`, no window and no `WITH`, which reads one table. Each aggregate call is `min` or `max` of one argument, with no `ORDER BY` and no `FILTER`, as `can_minmax_aggs` takes it. The planner does not take a query with a function that gives a set, or a subquery in an expression yet.
fn minmax(query: &Query) -> Option<MinMax> {
    let plain = query.grouped
        && query.group.is_empty()
        && query.grouping_sets.len() <= 1
        && query.windows.is_empty()
        && query.ctes.is_empty()
        && query.set_op.is_none()
        && query.row_marks.is_empty()
        && query.write.is_none()
        && !query.has_target_srfs();
    if !plain {
        return None;
    }
    let [FromItem::Relation(r)] = query.from.as_slice() else { return None };
    let relation = &query.relations[*r];
    let table = relation.oid != 0
        && relation.subquery.is_none()
        && relation.function.is_none()
        && relation.values.is_none()
        && relation.cte.is_none()
        && relation.tablesample.is_none();
    let has_sublink = |e: &Expr| {
        e.find(0, &mut |e, _| matches!(e.kind, ExprKind::SubLink(_)).then_some(())).is_some()
    };
    if !table || query.filter.as_ref().is_some_and(has_sublink) {
        return None;
    }
    // A subquery of the targets or of `HAVING` must run once as an `InitPlan` and have no aggregate call of the query, so that the planner can give the subquery of the new targets the plan of the subquery that it planned.
    let init_only = |e: &Expr| {
        e.find(0, &mut |e, depth| match &e.kind {
            ExprKind::SubLink(link)
                if depth == 0
                    && (matches!(link.kind, SubLinkKind::Any | SubLinkKind::All)
                        || exprs::correlated(&link.query, 1)) =>
            {
                Some(())
            }
            ExprKind::Agg(agg) if depth > 0 && agg.levels_up == depth => Some(()),
            _ => None,
        })
        .is_none()
    };
    let exprs: Vec<&Expr> = query.targets.iter().map(|t| &t.expr).chain(&query.having).collect();
    if !exprs.iter().all(|e| init_only(e)) {
        return None;
    }
    let mut calls = Vec::new();
    for expr in &exprs {
        expr.find(0, &mut |e, _| {
            if let ExprKind::Agg(agg) = &e.kind
                && agg.levels_up == 0
                && !calls.iter().any(|c: &&Expr| c.same(e))
            {
                calls.push(e);
            }
            None::<()>
        });
    }
    if calls.is_empty() {
        return None;
    }
    let mut aggs = Vec::new();
    for call in &calls {
        let ExprKind::Agg(agg) = &call.kind else { return None };
        let [arg] = agg.args.as_slice() else { return None };
        if !agg.order.is_empty() || agg.filter.is_some() || mutable(arg) || rowtype(arg.ty) {
            return None;
        }
        let sortop = builtin::aggregate(agg.oid).map_or(0, |a| a.sortop);
        let (eq, reverse) = equality_for_ordering(sortop)?;
        aggs.push(
            [reverse, !reverse].map(|nulls_first| subquery(query, arg, sortop, eq, nulls_first)),
        );
    }
    let link = |at: usize| {
        let link = SubLink {
            kind: SubLinkKind::Expr,
            test: None,
            query: aggs[at][0].clone(),
            alternative: None,
            volatile: false,
            pulled: false,
        };
        Expr::new(ExprKind::SubLink(Box::new(link)), calls[at].ty)
    };
    let replace = |expr: &Expr| {
        let mut expr = expr.clone();
        expr.each_mut(0, &mut |e, depth| {
            if depth > 0 || !matches!(&e.kind, ExprKind::Agg(agg) if agg.levels_up == 0) {
                return false;
            }
            if let Some(at) = calls.iter().position(|c| c.same(e)) {
                *e = link(at);
            }
            true
        });
        expr
    };
    let targets: Vec<Expr> = query.targets.iter().map(|t| replace(&t.expr)).collect();
    let having = query.having.as_ref().map(replace);
    let steps = !query.sort.is_empty()
        || !query.distinct.is_empty()
        || query.limit.is_some()
        || query.offset.is_some();
    let upper = steps.then(|| {
        let mut upper = query.clone();
        for (target, expr) in upper.targets.iter_mut().zip(&targets) {
            target.expr = expr.clone();
        }
        upper.having.clone_from(&having);
        Box::new(upper)
    });
    Some(MinMax { aggs, targets, having, upper })
}

/// `build_minmax_path`: the subquery of an aggregate call, which reads the argument of the call from the rows of the query where it is not null, in the order of the sort operator, with `LIMIT 1`. A column of a query outside is one more level up.
fn subquery(query: &Query, arg: &Expr, sortop: u32, eq: u32, nulls_first: bool) -> Query {
    let mut sub = query.clone();
    sub.each_expr_mut(0, &mut |e, depth| {
        e.each_mut(depth, &mut |e, depth| {
            if let ExprKind::Var(v) = &mut e.kind
                && v.levels_up > depth
            {
                v.levels_up += 1;
            }
            false
        });
    });
    let mut arg = arg.clone();
    arg.each_mut(0, &mut |e, depth| {
        if let ExprKind::Var(v) = &mut e.kind
            && v.levels_up > depth
        {
            v.levels_up += 1;
        }
        false
    });
    let test = Expr::new(ExprKind::NullTest(Box::new(arg.clone()), false), oid::BOOL);
    let mut parts = Vec::new();
    if let Some(filter) = &sub.filter {
        exprs::and_parts(filter, &mut parts);
    }
    // `restriction_is_always_true`: the test of a column that is `NOT NULL` or of a constant that is not null takes no place in the plan, as `expr_is_nonnullable` finds it.
    let always = match &arg.kind {
        ExprKind::Var(v) => {
            v.levels_up == 0 && pullup::column_not_null(&query.relations, v.relation, v.attnum)
        }
        ExprKind::Const(value) => !matches!(value, Value::Null),
        _ => false,
    };
    if !always && !parts.iter().any(|p| p.same(&test)) {
        let mut list = vec![test];
        list.extend(parts.into_iter().cloned());
        sub.filter = Some(match list.len() {
            1 => list.remove(0),
            _ => Expr::new(ExprKind::Bool(BoolOp::And, list), oid::BOOL),
        });
    }
    sub.targets = vec![Target { name: "agg_target".into(), expr: arg, origin: None, junk: false }];
    sub.grouped = false;
    sub.grouping_sets.clear();
    sub.having = None;
    sub.distinct.clear();
    sub.distinct_on = false;
    let function = |op: u32| builtin::operator_by_oid(op).map_or(0, |o| o.code);
    sub.sort = vec![SortGroup {
        target: 0,
        operator: sortop,
        sort: function(sortop),
        equal: function(eq),
        nulls_first,
        hashable: false,
        collation: 0,
    }];
    sub.offset = None;
    sub.limit = Some(Expr::new(ExprKind::Const(Value::Int8(1)), oid::INT8));
    sub.minmax = None;
    sub
}

/// `contain_mutable_functions`: true when the expression calls a function that is not immutable, or reads a value of the session such as `CURRENT_DATE`. A conversion through text counts as mutable, as the planner does not know the volatility of the input and output functions of each type.
pub(crate) fn mutable(expr: &Expr) -> bool {
    let mutable = |func: u32| builtin::proc_by_oid(func).is_none_or(|p| p.volatile != b'i');
    expr.find(0, &mut |e, _| match &e.kind {
        ExprKind::Func(f) if mutable(f.oid) => Some(()),
        ExprKind::NullIf { equal: func, .. }
        | ExprKind::Distinct { equal: func, .. }
        | ExprKind::ScalarArrayOp { func, .. }
            if mutable(*func) =>
        {
            Some(())
        }
        ExprKind::SqlValue(_) | ExprKind::CoerceViaIo(..) | ExprKind::SubLink(_) => Some(()),
        _ => None,
    })
    .is_some()
}

/// `type_is_rowtype`: true for `record` and a composite type, and for a type that the catalog of the planner does not have.
fn rowtype(ty: u32) -> bool {
    ty == oid::RECORD || builtin::type_by_oid(ty).is_none_or(|t| t.kind == b'c')
}
