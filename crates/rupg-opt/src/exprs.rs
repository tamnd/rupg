//! The facts about the expressions of a query that the planner reads: the relations that an expression reads, its volatile calls, and the parts of `AND`.

use std::collections::BTreeSet;

use rupg_analyze::{
    Aggref, BoolOp, Expr, ExprKind, FuncForm, PlaceHolder, Query, Var, WindowFunc, default_opclass,
    types,
};
use rupg_pgcatalog::builtin;
use rupg_types::Value;

/// The relations of the query that an expression reads, also in its subqueries, as `pull_varnos`.
pub(crate) fn relations(expr: &Expr) -> BTreeSet<usize> {
    relations_at(expr, 0)
}

/// The relations of the query that an expression reads, for an expression `levels` subqueries below the query. A `PlaceHolderVar` reads the relations of the node that computes it.
pub(crate) fn relations_at(expr: &Expr, levels: usize) -> BTreeSet<usize> {
    let mut out = BTreeSet::new();
    expr.find(0, &mut |e, depth| {
        match &e.kind {
            ExprKind::Var(var) if var.levels_up == depth + levels => {
                out.insert(var.relation);
            }
            ExprKind::PlaceHolder(p) if p.levels_up == depth + levels => out.extend(eval_at(p)),
            _ => {}
        }
        None::<()>
    });
    out
}

/// `ph_eval_at`: the relations of the scan or the join that computes a `PlaceHolderVar`. They are the relations of the subquery that its expression reads, or all the relations of the subquery when it reads none.
pub(crate) fn eval_at(p: &PlaceHolder) -> BTreeSet<usize> {
    let mut read = BTreeSet::new();
    p.expr.find(0, &mut |e, depth| {
        if let ExprKind::Var(var) = e.kind
            && var.levels_up == depth + p.levels_up
            && p.relations.contains(&var.relation)
        {
            read.insert(var.relation);
        }
        None::<()>
    });
    if read.is_empty() { p.relations.iter().copied().collect() } else { read }
}

/// The columns of the query that an expression `levels` subqueries below the query reads, also in its subqueries, in the order that they first come, as `pull_var_clause`. Each column is a `Var` of the query. A `PlaceHolderVar` of the query is one value, which a node below gives as a column.
pub(crate) fn columns_at(expr: &Expr, levels: usize, out: &mut Vec<Expr>) {
    columns_from(expr, 0, levels, out);
}

/// The columns of the query that an expression reads, as `pull_var_clause` with `PVC_RECURSE_PLACEHOLDERS`: the columns of a `PlaceHolderVar` take its place.
pub(crate) fn vars_of(expr: &Expr, out: &mut Vec<Expr>) {
    let mut columns = Vec::new();
    columns_at(expr, 0, &mut columns);
    for column in columns {
        match &column.kind {
            ExprKind::PlaceHolder(p) => vars_of(&p.expr, out),
            _ if out.iter().any(|o| o.same(&column)) => {}
            _ => out.push(column),
        }
    }
}

/// [`columns_at`] for an expression `depth` subqueries below the expression of the call.
fn columns_from(expr: &Expr, depth: usize, levels: usize, out: &mut Vec<Expr>) {
    match &expr.kind {
        ExprKind::Var(var) if var.levels_up == depth + levels => {
            let var = Var { levels_up: 0, ..*var };
            if !out.iter().any(|o| matches!(o.kind, ExprKind::Var(v) if v == var)) {
                let mut column = Expr::new(ExprKind::Var(var), expr.ty);
                column.typmod = expr.typmod;
                out.push(column);
            }
            return;
        }
        ExprKind::PlaceHolder(p) if p.levels_up == depth + levels => {
            let mut value = expr.clone();
            value.lower(p.levels_up);
            if !out.iter().any(|o| o.same(&value)) {
                out.push(value);
            }
            return;
        }
        _ => {}
    }
    for child in expr.children() {
        columns_from(child, depth, levels, out);
    }
    if let ExprKind::SubLink(link) = &expr.kind {
        for (e, d) in link.query.all_exprs(depth + 1) {
            columns_from(e, d, levels, out);
        }
    }
}

/// True when a node of the query gives a `PlaceHolderVar` from below, as a column.
pub(crate) fn is_placeholder(expr: &Expr) -> bool {
    matches!(&expr.kind, ExprKind::PlaceHolder(p) if p.levels_up == 0)
}

/// `contain_nonstrict_functions`: true when the expression can give a value that is not null for a null input. A call of a function that is not strict, an aggregate call, a window call, `AND`, `OR`, a subquery, `CASE`, `COALESCE`, a row, an array and the tests of nulls can.
pub fn nonstrict(expr: &Expr) -> bool {
    let strict = |func: u32| builtin::proc_by_oid(func).is_some_and(|p| p.strict);
    match &expr.kind {
        ExprKind::Agg(_)
        | ExprKind::Grouping(_)
        | ExprKind::Window(_)
        | ExprKind::Distinct { .. }
        | ExprKind::NullIf { .. }
        | ExprKind::Bool(BoolOp::And | BoolOp::Or, _)
        | ExprKind::SubLink(_)
        | ExprKind::FieldStore(_)
        | ExprKind::Case(_)
        | ExprKind::Array { .. }
        | ExprKind::Row(..)
        | ExprKind::JoinRow(_)
        | ExprKind::RowCompare(_)
        | ExprKind::Coalesce(_)
        | ExprKind::MinMax { .. }
        | ExprKind::Xml(_)
        | ExprKind::NullTest(..)
        | ExprKind::BooleanTest(..)
        | ExprKind::JsonCtor(_) => true,
        ExprKind::Subscript(sub) if sub.assign.is_some() => true,
        ExprKind::Func(f) if !strict(f.oid) => true,
        // `is_strict_saop`: the array must have elements.
        ExprKind::ScalarArrayOp { func, args, .. } => {
            let elements = match args.get(1).map(|a| &a.kind) {
                Some(ExprKind::Const(value)) => !value.is_null(),
                Some(ExprKind::Array { elements, .. }) => !elements.is_empty(),
                _ => false,
            };
            !(strict(*func) && elements) || args.iter().any(nonstrict)
        }
        ExprKind::ArrayCoerce { arg, .. } => nonstrict(arg),
        _ => expr.children().into_iter().any(nonstrict),
    }
}

/// `contain_volatile_functions`: true when the expression calls a volatile function, also in a subquery.
pub(crate) fn volatile(expr: &Expr) -> bool {
    let volatile = |func: u32| builtin::proc_by_oid(func).is_none_or(|p| p.volatile == b'v');
    expr.find(0, &mut |e, _| match &e.kind {
        ExprKind::Func(f) if volatile(f.oid) => Some(()),
        ExprKind::NullIf { equal: func, .. }
        | ExprKind::Distinct { equal: func, .. }
        | ExprKind::ScalarArrayOp { func, .. }
            if volatile(*func) =>
        {
            Some(())
        }
        _ => None,
    })
    .is_some()
}

/// True for an expression that a sort takes as a constant, as `add_eq_member` and `get_eclass_for_sort_expr` set `ec_has_const`: it reads no relation of the query and has no volatile call, no call that returns a set, no aggregate call and no window call.
pub(crate) fn pseudo_constant(expr: &Expr) -> bool {
    relations(expr).is_empty()
        && !volatile(expr)
        && expr
            .find(0, &mut |e, depth| match &e.kind {
                ExprKind::Func(f) if f.retset => Some(()),
                ExprKind::Agg(_) | ExprKind::Grouping(_) | ExprKind::Window(_) if depth == 0 => {
                    Some(())
                }
                _ => None,
            })
            .is_none()
}

/// True when a query calls a volatile function in an expression of it or of its subqueries.
pub(crate) fn query_volatile(query: &Query) -> bool {
    query.all_exprs(0).into_iter().any(|(e, _)| volatile(e))
}

/// The parts of a condition that `AND` joins, as `make_ands_implicit`.
pub(crate) fn and_parts<'q>(expr: &'q Expr, out: &mut Vec<&'q Expr>) {
    match &expr.kind {
        ExprKind::Bool(BoolOp::And, args) => args.iter().for_each(|a| and_parts(a, out)),
        _ => out.push(expr),
    }
}

/// True for the constant `true`.
pub(crate) fn is_true(expr: &Expr) -> bool {
    matches!(expr.as_const(), Some(Value::Bool(true)))
}

/// True for the constant `false` or a null constant, which no row passes.
pub(crate) fn is_false(expr: &Expr) -> bool {
    matches!(expr.as_const(), Some(Value::Bool(false) | Value::Null))
}

/// The OID of the operator of a call of an operator with two arguments, with the arguments.
pub(crate) fn binary(expr: &Expr) -> Option<(u32, &Expr, &Expr)> {
    match &expr.kind {
        ExprKind::Func(f) => match (f.form, f.args.as_slice()) {
            (FuncForm::Operator(op), [left, right]) => Some((op, left, right)),
            _ => None,
        },
        _ => None,
    }
}

/// The call of the commutator of an operator, with the arguments in the other order, as `commute_op_clause`.
pub(crate) fn commute(expr: &Expr) -> Option<Expr> {
    let (op, left, right) = binary(expr)?;
    let row = builtin::operator_by_oid(builtin::operator_by_oid(op)?.commutator)?;
    let ExprKind::Func(f) = &expr.kind else { return None };
    let mut func = f.clone();
    func.oid = row.code;
    func.form = FuncForm::Operator(row.oid);
    func.args = vec![right.clone(), left.clone()];
    let mut out = Expr::new(ExprKind::Func(func), expr.ty);
    out.location = expr.location;
    Some(out)
}

/// True when the operator can test a hash join, `oprcanhash`.
pub(crate) fn can_hash(op: u32) -> bool {
    builtin::operator_by_oid(op).is_some_and(|row| row.can_hash)
}

/// `hash_ok_operator`: a call of an operator with two arguments that can use a hash table and whose function is strict. The `=` of arrays and the `=` of records can use a hash table only when the type of the left argument has a hash function, as `op_hashjoinable` finds it: an array whose element type has one, and a record that is not of the type `record`, whose columns `lookup_type_cache` cannot know.
pub(crate) fn hash_ok(expr: &Expr) -> bool {
    const ARRAY_EQ: u32 = 1070;
    const RECORD_EQ: u32 = 2988;
    const HASH: u32 = 405;
    let ExprKind::Func(f) = &expr.kind else { return false };
    let FuncForm::Operator(op) = f.form else { return false };
    if f.args.len() != 2 {
        return false;
    }
    let ty = f.args[0].ty;
    match op {
        ARRAY_EQ => {
            let element = types::element(types::base(ty));
            element != 0 && default_opclass(element, HASH).is_some()
        }
        RECORD_EQ => types::base(ty) != rupg_types::oid::RECORD,
        _ => can_hash(op) && builtin::proc_by_oid(f.oid).is_some_and(|p| p.strict),
    }
}

/// True when the operator can test a merge join, `oprcanmerge`, which makes the condition an equivalence of PostgreSQL.
pub(crate) fn can_merge(op: u32) -> bool {
    builtin::operator_by_oid(op).is_some_and(|row| row.can_merge)
}

/// The cost of the calls of functions and operators in an expression, outside its subqueries, in units of `cpu_operator_cost`, which is the cost of the expression for each row, as `cost_qual_eval`. A row comparison costs the operators of its columns, which are its children.
pub(crate) fn calls(expr: &Expr) -> f64 {
    expr.own_cost() + expr.children().into_iter().map(calls).sum::<f64>()
}

/// The aggregate calls of the query in the expressions, outside the arguments of other aggregate calls, as `get_agg_clause_costs` finds them. A call in a subquery that reads only the query is a call of the query. Two calls that are the same and call no volatile function are one call, as `find_compatible_agg` shares them.
pub(crate) fn aggregates<'q>(exprs: &[&'q Expr], out: &mut Vec<&'q Aggref>) {
    fn walk<'q>(expr: &'q Expr, depth: usize, seen: &mut Vec<&'q Expr>, out: &mut Vec<&'q Aggref>) {
        if let ExprKind::Agg(agg) = &expr.kind
            && agg.levels_up == depth
        {
            if depth == 0 && !volatile(expr) {
                if seen.iter().any(|s| s.same(expr)) {
                    return;
                }
                seen.push(expr);
            }
            out.push(agg);
            return;
        }
        for child in expr.children() {
            walk(child, depth, seen, out);
        }
        if let ExprKind::SubLink(sub) = &expr.kind {
            for (e, d) in sub.query.all_exprs(depth + 1) {
                walk(e, d, seen, out);
            }
        }
    }
    let mut seen = Vec::new();
    for expr in exprs {
        walk(expr, 0, &mut seen, out);
    }
}

/// The window function calls in an expression, outside its subqueries.
pub(crate) fn window_calls<'q>(expr: &'q Expr, out: &mut Vec<&'q WindowFunc>) {
    let mut calls = Vec::new();
    window_exprs(expr, &mut calls);
    out.extend(calls.into_iter().filter_map(|e| match &e.kind {
        ExprKind::Window(w) => Some(&**w),
        _ => None,
    }));
}

/// The expressions of the window function calls in an expression, outside its subqueries.
pub(crate) fn window_exprs<'q>(expr: &'q Expr, out: &mut Vec<&'q Expr>) {
    if let ExprKind::Window(_) = &expr.kind {
        out.push(expr);
    }
    if matches!(expr.kind, ExprKind::SubLink(_)) {
        return;
    }
    for child in expr.children() {
        window_exprs(child, out);
    }
}

/// True when the expression reads a column of the query, also in a subquery.
pub(crate) fn reads_columns(expr: &Expr) -> bool {
    !relations(expr).is_empty()
}

/// True when a query reads a column of the query that is `levels` levels outside it, as the parameters of a correlated subquery.
pub(crate) fn correlated(query: &Query, levels: usize) -> bool {
    query.all_exprs(levels).into_iter().any(|(e, depth)| {
        e.find(depth, &mut |e, depth| match e.kind {
            ExprKind::Var(var) if var.levels_up == depth => Some(()),
            ExprKind::Agg(ref agg) if agg.levels_up == depth => Some(()),
            _ => None,
        })
        .is_some()
    })
}

/// The number of the references in a query and in its subqueries to the query of `WITH` with this name, other than the references of a recursive query to itself.
pub(crate) fn cte_references(query: &Query, name: &str) -> usize {
    let mut count = 0;
    for relation in &query.relations {
        if let Some(cte) = &relation.cte
            && cte.name == name
            && cte.work.is_none()
        {
            count += 1;
        }
        if let Some(sub) = &relation.subquery {
            count += cte_references(sub, name);
        }
    }
    for expr in query.exprs() {
        expr.find(0, &mut |e, _| {
            if let ExprKind::SubLink(sub) = &e.kind {
                count += cte_references(&sub.query, name);
            }
            None::<()>
        });
    }
    for cte in &query.ctes {
        if cte.name != name {
            count += cte_references(&cte.query, name);
        }
    }
    count
}
