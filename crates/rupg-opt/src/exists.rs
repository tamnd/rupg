//! The form of a correlated `EXISTS` as `ANY` on a subquery that reads no column of the query, which a hash table can run, as `convert_EXISTS_to_ANY` makes it.

use rupg_analyze::{BoolOp, Expr, ExprKind, Query, SubLink, SubLinkKind};
use rupg_types::{Value, oid};

use crate::exprs;

/// Fills [`SubLink::alternative`] for each `EXISTS` in the query and in its subqueries, the inner subqueries first, so that the subquery of the alternative has the alternatives of its own subqueries. The subquery of `EXISTS` loses the clauses that [`simplify`] takes out.
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
                if link.kind == SubLinkKind::Exists
                    && let Some(simple) = simplify(&link.query)
                {
                    link.alternative = alternative(&simple).map(Box::new);
                    link.query = simple;
                }
            }
            false
        });
    }
}

/// `convert_EXISTS_to_ANY`: the subquery of `EXISTS` that [`simplify`] made, as `ANY`. Each part of `WHERE` that compares an expression of the query with an expression of the subquery with an operator that can use a hash table becomes a column of the subquery and a comparison of the test. The other parts stay in `WHERE`, and they must not read a column of the query.
fn alternative(query: &Query) -> Option<SubLink> {
    let mut sub = query.clone();
    let filter = sub.filter.take()?;
    if exprs::correlated(&sub, 1) || exprs::volatile(&filter) {
        return None;
    }
    let mut parts = Vec::new();
    exprs::and_parts(&filter, &mut parts);
    let (mut lefts, mut rights, mut where_parts) = (Vec::new(), Vec::new(), Vec::new());
    for part in parts {
        let Some((_, left, right)) = exprs::binary(part).filter(|_| exprs::hash_ok(part)) else {
            where_parts.push(part.clone());
            continue;
        };
        if outer(left) {
            lefts.push(part.clone());
            rights.push(right.clone());
        } else if outer(right) {
            let commuted = exprs::commute(part).filter(exprs::hash_ok)?;
            lefts.push(commuted);
            rights.push(left.clone());
        } else {
            where_parts.push(part.clone());
        }
    }
    if lefts.is_empty()
        || where_parts.iter().chain(&rights).any(outer_ref)
        || lefts
            .iter()
            .filter_map(exprs::binary)
            .any(|(_, left, _)| !exprs::relations(left).is_empty() || has_sublink(left))
    {
        return None;
    }
    sub.filter = match where_parts.len() {
        0 => None,
        1 => where_parts.pop(),
        _ => Some(Expr::new(ExprKind::Bool(BoolOp::And, where_parts), oid::BOOL)),
    };
    let mut tests = Vec::new();
    for (i, (mut test, right)) in lefts.into_iter().zip(rights).enumerate() {
        let ExprKind::Func(f) = &mut test.kind else { return None };
        f.args[0].lower(1);
        let mut column = Expr::new(ExprKind::SubColumn(i), right.ty);
        column.typmod = right.typmod;
        f.args[1] = column;
        sub.targets.push(rupg_analyze::Target {
            name: "?column?".to_string(),
            expr: right,
            origin: None,
            junk: false,
        });
        tests.push(test);
    }
    let test = match tests.len() {
        1 => tests.pop(),
        _ => Some(Expr::new(ExprKind::Bool(BoolOp::And, tests), oid::BOOL)),
    };
    Some(SubLink { kind: SubLinkKind::Any, test, query: sub, alternative: None })
}

/// `simplify_EXISTS_query`: the subquery of `EXISTS` with no targets, `GROUP BY`, window, `DISTINCT`, `ORDER BY` or `LIMIT`, which do not change whether the subquery gives a row. It is none for a subquery with a set operation, an aggregate, grouping sets, a window function, a function that gives a set in its targets, a query of `WITH` that writes, `HAVING`, `OFFSET`, a row lock or a `LIMIT` that is not a positive constant or null.
pub(crate) fn simplify(query: &Query) -> Option<Query> {
    let aggs = query.targets.iter().any(|t| t.expr.first_agg().is_some());
    let windows = query.targets.iter().any(|t| t.expr.first_window().is_some());
    let sets = query.targets.iter().any(|t| t.expr.first_set_call().is_some());
    if query.set_op.is_some()
        || aggs
        || windows
        || sets
        || !query.grouping_sets.is_empty()
        || query.ctes.iter().any(|c| c.write.is_some())
        || query.having.is_some()
        || query.offset.is_some()
        || !query.row_marks.is_empty()
        || query.write.is_some()
    {
        return None;
    }
    if let Some(limit) = &query.limit {
        match limit.strip_implicit().as_const()? {
            Value::Null => {}
            Value::Int4(n) if *n > 0 => {}
            Value::Int8(n) if *n > 0 => {}
            _ => return None,
        }
    }
    let mut sub = query.clone();
    sub.limit = None;
    sub.targets.clear();
    sub.group.clear();
    sub.grouped = false;
    sub.windows.clear();
    sub.distinct.clear();
    sub.distinct_on = false;
    sub.sort.clear();
    Some(sub)
}

/// `contain_vars_of_level` with level 1: the expression of the subquery reads a column of the query.
fn outer(expr: &Expr) -> bool {
    !exprs::relations_at(expr, 1).is_empty()
}

/// [`outer`], or an aggregate call of the query, as `contain_aggs_of_level` finds it.
fn outer_ref(expr: &Expr) -> bool {
    outer(expr)
        || expr
            .find(0, &mut |e, depth| match &e.kind {
                ExprKind::Agg(agg) if agg.levels_up == depth + 1 => Some(()),
                _ => None,
            })
            .is_some()
}

/// `contain_subplans`: the expression has a subquery.
fn has_sublink(expr: &Expr) -> bool {
    expr.find(0, &mut |e, _| matches!(e.kind, ExprKind::SubLink(_)).then_some(())).is_some()
}
