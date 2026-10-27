//! `eval_const_expressions` of `clauses.c`: the planner computes each part of an expression whose arguments are constants and whose function is immutable. An error of such a part, such as a division by zero, comes when the query is planned, before the server sends the description of the rows. The plan then runs the folded expressions, so a part that the fold drops does not run.

use rupg_analyze::{
    BoolOp, BoolTest, Case, CastForm, Expr, ExprKind, FieldStore, FromItem, Func, FuncForm, IsJson,
    Join, JoinKind, JoinRow, JsonCtor, JsonCtorKind, JsonValue, NamedArg, Query, Relation,
    RowCompare, SubLink, SubLinkKind, Subscript, Var, XmlOp,
};
use rupg_common::{Error, Result};
use rupg_func::Session;
use rupg_pgcatalog::builtin;
use rupg_types::{Array, Value, oid};

use super::{agg, evaluate, null_test, relabel, volatile};

mod prepqual;

/// The OID of `count("any")` in `pg_proc`.
const COUNT_ANY: u32 = 2147;
/// The OID of `count()`, which `count(*)` calls.
const COUNT_STAR: u32 = 2803;

/// `subquery_planner`: folds the expressions of a query in the order of `preprocess_expression`, which is the target list, the conditions of the joins and `WHERE`, `HAVING`, `OFFSET` and `LIMIT`. The subqueries of each clause come after the clause, as `SS_process_sublinks` plans them. `params` has the values of the parameters of `Bind`, which a custom plan takes as constants.
pub(crate) fn query(
    query: &Query,
    params: Option<&[Value]>,
    session: &dyn Session,
) -> Result<Query> {
    plan(query, params, session, false)
}

/// Folds a condition of a query and makes it canonical, as `preprocess_expression` folds the conditions of `WHERE`.
pub(crate) fn qual(query: &Query, qual: &Expr, session: &dyn Session) -> Result<Expr> {
    let mut fold = Fold::new(query, None, session);
    Ok(fold.one(Some(qual))?.map_or_else(|| qual.clone(), prepqual::canonicalize))
}

/// Folds a query or a subquery. For the subquery of `EXISTS`, `simplify_EXISTS_query` drops the target list and a constant `LIMIT` first when it can. A subquery in `FROM` that the planner pulls up folds where the query reads its columns, then each subquery in `FROM` folds after the query, with null for each column that the query does not read.
fn plan(
    query: &Query,
    params: Option<&[Value]>,
    session: &dyn Session,
    exists: bool,
) -> Result<Query> {
    let converted = values_to_any(query, params, session)?;
    let query = converted.as_ref().unwrap_or(query);
    let mut fold = Fold::new(query, params, session);
    let mut out = query.clone();
    let mut simple = false;
    if exists
        && query.set_op.is_none()
        && agg::calls(query).is_empty()
        && query.having.is_none()
        && query.offset.is_none()
        && !query.has_target_srfs()
    {
        simple = match &query.limit {
            None => true,
            Some(limit) => match value(&fold.expr(limit)?) {
                Some(Value::Int8(n)) => *n > 0,
                Some(_) => true,
                None => false,
            },
        };
    }
    // preprocess_function_rtes: the calls of the functions in `FROM` fold first.
    for relation in &mut out.relations {
        for (call, _) in relation.function.iter_mut().flat_map(|f| &mut f.calls) {
            *call = fold.clause(&[&*call])?.pop().unwrap_or_else(|| call.clone());
        }
    }
    // `inline_function_in_from`: a call of a function in SQL that gives a set becomes the query of its body, which the planner can pull up.
    for relation in &mut out.relations {
        let lateral = relation.function.iter().flat_map(|f| &f.calls).any(|(call, _)| {
            call.find(0, &mut |e, _| matches!(e.kind, ExprKind::Var(_)).then_some(())).is_some()
        });
        if let Some(body) = inline_from(relation, session)? {
            relation.subquery = Some(Box::new(body));
            relation.function = None;
            // The body reads the columns of the relations before it that the arguments read.
            relation.lateral |= lateral;
        }
    }
    if simple {
        out.limit = None;
    } else {
        let targets: Vec<&Expr> = query.targets.iter().map(|t| &t.expr).collect();
        for (target, expr) in out.targets.iter_mut().zip(fold.clause(&targets)?) {
            // The description of the rows keeps the type and the typmod of the analysis.
            target.expr = Expr { ty: target.expr.ty, typmod: target.expr.typmod, ..expr };
        }
    }
    out.from = query.from.iter().map(|item| fold.join(item)).collect::<Result<_>>()?;
    // `preprocess_expression` makes each condition canonical after it folds, as `canonicalize_qual`.
    out.filter = fold.one(query.filter.as_ref())?.map(prepqual::canonicalize);
    out.having = fold.one(query.having.as_ref())?.map(prepqual::canonicalize);
    out.offset = fold.one(query.offset.as_ref())?;
    if !simple {
        out.limit = fold.one(query.limit.as_ref())?;
    }
    // `preprocess_expression` folds the offsets of the frames as it folds `LIMIT`.
    for (window, old) in out.windows.iter_mut().zip(&query.windows) {
        window.frame.start = fold.one(old.frame.start.as_ref())?;
        window.frame.end = fold.one(old.frame.end.as_ref())?;
    }
    // The arguments of `TABLESAMPLE` and the rows of a `VALUES` list fold after the clauses, as the range table does in `subquery_planner`.
    for (relation, old) in out.relations.iter_mut().zip(&query.relations) {
        if let (Some(sample), Some(old)) = (&mut relation.tablesample, &old.tablesample) {
            let args: Vec<&Expr> = old.args.iter().collect();
            sample.args = fold.clause(&args)?;
            sample.repeatable = fold.one(old.repeatable.as_ref())?;
        }
        let (Some(rows), Some(old)) = (&mut relation.values, &old.values) else { continue };
        for (row, old) in rows.iter_mut().zip(old) {
            let exprs: Vec<&Expr> = old.iter().collect();
            *row = fold.clause(&exprs)?;
        }
    }
    // A relation that is the only part of `FROM`, with a `WHERE` that is false or null, is dummy: the planner makes no plan for it.
    let dummy = matches!(out.from.as_slice(), [FromItem::Relation(_)])
        && out.filter.as_ref().and_then(value).is_some_and(|v| *v != Value::Bool(true));
    let used = used(&out);
    let rows = out.write.as_ref().is_some_and(|w| !w.checks.is_empty())
        && matches!(out.relations.as_slice(), [r] if r.oid == 0);
    for (relation, used) in out.relations.iter_mut().zip(&used) {
        if let Some(sub) = &mut relation.subquery {
            if dummy && !simple_subquery(sub) {
                continue;
            }
            // Each `SELECT` of a set operation gives all its columns, and the rows of a write with check constraints are the rows that it writes.
            if out.set_op.is_none() && !rows {
                prune(sub, used);
            }
            **sub = if out.set_op.is_some() {
                set_leaf(sub, params, session)?
            } else {
                plan(sub, params, session, false)?
            };
        }
    }
    Ok(out)
}

/// `inline_function_in_from` and `inline_sql_function_in_from`: the query of the body of the one function of a relation in `FROM`, with the arguments in the places of its parameters, or `None` when the function cannot be inlined. The relation must have no `WITH ORDINALITY` and no column definition list. The arguments must have no volatile function and no subquery. The function must be in SQL, return a set and not `void`, and must not be strict, volatile or `SECURITY DEFINER` or have settings. Its body must be one `SELECT` with no `WITH`, whose columns have the types of the columns of the relation.
fn inline_from(relation: &Relation, session: &dyn Session) -> Result<Option<Query>> {
    let Some(function) = &relation.function else { return Ok(None) };
    let ([(call, _)], [defined]) = (function.calls.as_slice(), function.coldefs.as_slice()) else {
        return Ok(None);
    };
    let ExprKind::Func(f) = &call.kind else { return Ok(None) };
    if function.ordinality || !f.retset {
        return Ok(None);
    }
    let volatile = |func: u32| builtin::proc_by_oid(func).is_none_or(|p| p.volatile == b'v');
    let unsafe_arg = f.args.iter().any(|arg| {
        arg.find(0, &mut |e, _| match &e.kind {
            ExprKind::Func(g) if volatile(g.oid) => Some(()),
            ExprKind::NullIf { equal: func, .. }
            | ExprKind::Distinct { equal: func, .. }
            | ExprKind::ScalarArrayOp { func, .. }
                if volatile(*func) =>
            {
                Some(())
            }
            ExprKind::SubLink(_) => Some(()),
            _ => None,
        })
        .is_some()
    });
    let Some(proc) = builtin::proc_by_oid(f.oid) else { return Ok(None) };
    if unsafe_arg
        || proc.oid < rupg_catalog::FIRST_NORMAL_OID
        || proc.lang != SQL_LANGUAGE
        || proc.kind != b'f'
        || proc.strict
        || proc.volatile == b'v'
        || proc.rettype == oid::VOID
        || !proc.retset
        || f.args.len() != usize::try_from(proc.nargs).unwrap_or(usize::MAX)
    {
        return Ok(None);
    }
    if session
        .catalog()
        .and_then(|c| c.function(f.oid))
        .is_none_or(|function| function.secdef || !function.config.is_empty())
    {
        return Ok(None);
    }
    let inputs: Vec<u32> = f.args.iter().map(|a| a.ty).collect();
    let wanted: Vec<(u32, i32)> = relation.columns.iter().map(|c| (c.ty, c.typmod)).collect();
    // With a column definition list, `check_sql_fn_retval` casts the result to the types of the list, and its error is an error of the query.
    let body = if *defined {
        let name = builtin::proc_by_oid(f.oid).map_or("", |p| p.name);
        rupg_func::function_body_for(f.oid, &inputs, &wanted, session)
            .map_err(|e| e.with_context(format!("SQL function \"{name}\" during inlining")))?
    } else {
        rupg_func::function_body(f.oid, &inputs, session).ok().flatten()
    };
    let Some(rupg_analyze::Body::Query(mut body)) = body else {
        return Ok(None);
    };
    let columns: Vec<(u32, i32)> =
        body.targets.iter().filter(|t| !t.junk).map(|t| (t.expr.ty, t.expr.typmod)).collect();
    if body.write.is_some()
        || !body.ctes.is_empty()
        || columns.len() != wanted.len()
        || columns.iter().zip(&wanted).any(|(a, b)| a.0 != b.0)
    {
        return Ok(None);
    }
    // `substitute_actual_parameters_in_from`: each parameter takes the value of its argument, whose columns are one level up more in the body.
    body.each_expr_mut(0, &mut |e, top| {
        e.each_mut(top, &mut |e, depth| {
            let ExprKind::Param(n) = e.kind else { return false };
            if let Some(arg) = n.checked_sub(1).and_then(|i| f.args.get(i)) {
                let mut arg = arg.clone();
                arg.raise(depth + 1);
                *e = arg;
            }
            true
        });
    });
    Ok(Some(*body))
}

/// Folds a `SELECT` of a set operation. `transformSetOperationTree` does not cast the columns of a `SELECT` to the types of the operation: `generate_setop_tlist` adds the casts above the `SELECT` after `subquery_planner` folds it. So the fold here folds the argument of the cast of each column and keeps the casts, also for a constant.
fn set_leaf(sub: &Query, params: Option<&[Value]>, session: &dyn Session) -> Result<Query> {
    let mut bare = sub.clone();
    let mut casts = Vec::with_capacity(bare.targets.len());
    for target in &mut bare.targets {
        let mut cast = target.expr.clone();
        let Some(arg) = leaf_cast_arg(&mut cast).filter(|_| !target.junk) else {
            casts.push(None);
            continue;
        };
        target.expr = std::mem::replace(arg, Expr::new(ExprKind::CaseTest, 0));
        casts.push(Some(cast));
    }
    let mut out = plan(&bare, params, session, false)?;
    for (target, cast) in out.targets.iter_mut().zip(casts) {
        let Some(mut cast) = cast else { continue };
        if let Some(arg) = leaf_cast_arg(&mut cast) {
            *arg = std::mem::replace(&mut target.expr, Expr::new(ExprKind::CaseTest, 0));
        }
        target.expr = cast;
    }
    Ok(out)
}

/// The argument of the implicit casts at the top of a column of a `SELECT` of a set operation: the cast to the type of the operation, and the casts to the types of the operations above it.
fn leaf_cast_arg(expr: &mut Expr) -> Option<&mut Expr> {
    let mut arg = cast_arg_mut(expr)?;
    while cast_arg_mut(arg).is_some() {
        arg = cast_arg_mut(arg)?;
    }
    Some(arg)
}

/// The argument of the implicit cast at the top of `expr`.
fn cast_arg_mut(expr: &mut Expr) -> Option<&mut Expr> {
    match &mut expr.kind {
        ExprKind::Func(Func { form: FuncForm::ImplicitCast, args, .. }) => args.first_mut(),
        ExprKind::Relabel(arg, CastForm::Implicit)
        | ExprKind::CoerceViaIo(arg, CastForm::Implicit)
        | ExprKind::CoerceToDomain(arg, CastForm::Implicit)
        | ExprKind::ConvertRow { arg, form: CastForm::Implicit, .. }
        | ExprKind::ArrayCoerce { arg, form: CastForm::Implicit, .. } => Some(arg),
        _ => None,
    }
}

/// `convert_VALUES_to_ANY` in `pull_up_sublinks`: the query with each part of `WHERE`, and of the `ON` of an inner, a left or a right join, that is `ANY` on a list of `VALUES` as `op ANY` of a constant array, as [`values_any`] makes it. `None` when no part is `ANY` on a subquery. The planner of PostgreSQL does this before it folds the query.
fn values_to_any(
    query: &Query,
    params: Option<&[Value]>,
    session: &dyn Session,
) -> Result<Option<Query>> {
    if query.from.is_empty() {
        return Ok(None);
    }
    let mut ons = Vec::new();
    for item in &query.from {
        inner_ons(item, &mut ons);
    }
    let any = |e: &Expr, _| {
        matches!(&e.kind, ExprKind::SubLink(link) if link.kind == SubLinkKind::Any).then_some(())
    };
    if !query.filter.iter().chain(ons).any(|e| e.find(0, &mut { any }).is_some()) {
        return Ok(None);
    }
    let mut fold = Fold::new(query, params, session);
    let mut out = query.clone();
    if let Some(filter) = &mut out.filter {
        parts_to_any(filter, &mut fold)?;
    }
    for item in &mut out.from {
        on_to_any(item, &mut fold)?;
    }
    Ok(Some(out))
}

/// The conditions of `ON` of the inner, the left and the right joins of `item`, which [`values_to_any`] changes.
fn inner_ons<'q>(item: &'q FromItem, out: &mut Vec<&'q Expr>) {
    let FromItem::Join(join) = item else { return };
    inner_ons(&join.left, out);
    inner_ons(&join.right, out);
    if matches!(join.kind, JoinKind::Inner | JoinKind::Left | JoinKind::Right) {
        out.extend(&join.on);
    }
}

/// [`values_to_any`] for the `ON` of each join of `item`.
fn on_to_any(item: &mut FromItem, fold: &mut Fold<'_>) -> Result<()> {
    let FromItem::Join(join) = item else { return Ok(()) };
    on_to_any(&mut join.left, fold)?;
    on_to_any(&mut join.right, fold)?;
    match &mut join.on {
        Some(on) if matches!(join.kind, JoinKind::Inner | JoinKind::Left | JoinKind::Right) => {
            parts_to_any(on, fold)
        }
        _ => Ok(()),
    }
}

/// [`values_any`] for each part of the `AND` of `expr`.
fn parts_to_any(expr: &mut Expr, fold: &mut Fold<'_>) -> Result<()> {
    match &mut expr.kind {
        ExprKind::Bool(BoolOp::And, args) => {
            for arg in args {
                parts_to_any(arg, fold)?;
            }
        }
        ExprKind::SubLink(link) if link.kind == SubLinkKind::Any => {
            if let Some(new) = values_any(link, fold)? {
                *expr = new;
            }
        }
        _ => {}
    }
    Ok(())
}

/// `convert_VALUES_to_ANY`: `x op ANY (VALUES (a), (b))` is `x op ANY ('{a,b}')` when the test is one operator, the subquery is a list of `VALUES` with one column and two rows at least and no `LIMIT`, `OFFSET` or `ORDER BY`, and the right side of the operator with the value of each row folds to a constant. The list calls no volatile function and reads no column.
fn values_any(link: &SubLink, fold: &mut Fold<'_>) -> Result<Option<Expr>> {
    let Some(test) = &link.test else { return Ok(None) };
    let ExprKind::Func(Func { oid: func, form: FuncForm::Operator(_), args, .. }) = &test.kind
    else {
        return Ok(None);
    };
    let [left, right] = args.as_slice() else { return Ok(None) };
    let sub = &link.query;
    let ([target], [relation]) = (sub.targets.as_slice(), sub.relations.as_slice()) else {
        return Ok(None);
    };
    let Some(rows) = relation.values.as_ref().filter(|rows| rows.len() > 1) else {
        return Ok(None);
    };
    let array = rupg_analyze::types::array_of(right.ty);
    let column = match target.expr.kind {
        ExprKind::Var(Var { relation: 0, attnum, levels_up: 0 }) => {
            usize::try_from(attnum - 1).ok()
        }
        _ => None,
    };
    let other = |e: &Expr, _| {
        matches!(e.kind, ExprKind::Var(_) | ExprKind::SubLink(_) | ExprKind::SubColumn(_))
            .then_some(())
    };
    if sub.limit.is_some()
        || sub.offset.is_some()
        || !sub.sort.is_empty()
        || sub.set_op.is_some()
        || array == 0
        || rows.iter().flatten().any(|e| volatile(e) || e.find(0, &mut { other }).is_some())
    {
        return Ok(None);
    }
    let Some(column) = column else { return Ok(None) };
    let mut values = Vec::with_capacity(rows.len());
    for row in rows {
        let Some(value) = row.get(column) else { return Ok(None) };
        let mut expr = right.clone();
        expr.each_mut(0, &mut |e, depth| {
            if depth == 0 && matches!(e.kind, ExprKind::SubColumn(0)) {
                *e = value.clone();
                return true;
            }
            false
        });
        match fold.expr(&expr)?.kind {
            ExprKind::Const(Value::Null) => values.push(None),
            ExprKind::Const(value) => values.push(Some(value)),
            _ => return Ok(None),
        }
    }
    let array = Expr::new(ExprKind::Const(Value::Array(Box::new(Array::one(values)))), array);
    let args = vec![left.clone(), array];
    Ok(Some(Expr::new(ExprKind::ScalarArrayOp { func: *func, any: true, args }, oid::BOOL)))
}

/// `is_simple_subquery` of `prepjointree.c`: true when the planner pulls the subquery in `FROM` up into the query, so that its expressions fold where the query reads them. Such a subquery has no aggregate, `GROUP BY`, `HAVING`, `ORDER BY`, `DISTINCT`, `OFFSET` or `LIMIT`, and no volatile function and no function that gives a set in its target list.
fn simple_subquery(query: &Query) -> bool {
    query.set_op.is_none()
        && !query.grouped
        && agg::calls(query).is_empty()
        && query.sort.is_empty()
        && query.distinct.is_empty()
        && query.offset.is_none()
        && query.limit.is_none()
        && !query.targets.iter().any(|t| volatile(&t.expr))
        && !query.has_target_srfs()
}

/// `attrs_used`: for each relation of the query, the columns that the expressions of the query read, also in its subqueries.
fn used(query: &Query) -> Vec<Vec<bool>> {
    let mut used: Vec<Vec<bool>> =
        query.relations.iter().map(|r| vec![false; r.columns.len()]).collect();
    for (expr, depth) in query.all_exprs(0) {
        expr.find(depth, &mut |e, depth| {
            if let ExprKind::Var(var) = e.kind
                && var.levels_up == depth
                && let Some(used) = used.get_mut(var.relation)
            {
                // A whole-row reference reads all the columns.
                match usize::try_from(var.attnum - 1) {
                    Ok(column) => {
                        if let Some(slot) = used.get_mut(column) {
                            *slot = true;
                        }
                    }
                    Err(_) if var.attnum == 0 => used.fill(true),
                    Err(_) => {}
                }
            }
            None::<()>
        });
    }
    used
}

/// `remove_unused_subquery_outputs`: each column of a subquery in `FROM` that the query does not read becomes a null constant, so that it does not run. With a plain `DISTINCT`, all the columns stay. A column that `ORDER BY`, `GROUP BY` or `DISTINCT` reads, or that calls a volatile function or a function that gives a set, stays.
fn prune(sub: &mut Query, used: &[bool]) {
    if !sub.distinct.is_empty() && !sub.distinct_on {
        return;
    }
    let keep: Vec<usize> =
        sub.sort.iter().chain(&sub.group).chain(&sub.distinct).map(|s| s.target).collect();
    for (i, target) in sub.targets.iter_mut().enumerate() {
        if target.junk
            || used.get(i).is_none_or(|u| *u)
            || keep.contains(&i)
            || volatile(&target.expr)
            || target.expr.first_set_call().is_some()
        {
            continue;
        }
        target.expr = with(&target.expr, ExprKind::Const(Value::Null));
    }
}

/// Marks the relations that an outer join of `item` can make null, as the `varnullingrels` of their columns. With `nullable`, a join above `item` can make all its relations null.
fn mark(item: &FromItem, nullable: bool, nulled: &mut [bool]) {
    match item {
        FromItem::Relation(index) => {
            if nullable && let Some(slot) = nulled.get_mut(*index) {
                *slot = true;
            }
        }
        FromItem::Join(join) => {
            let (left, right) = match join.kind {
                JoinKind::Inner | JoinKind::Semi | JoinKind::RightSemi => (false, false),
                JoinKind::Left | JoinKind::Anti => (false, true),
                JoinKind::Right | JoinKind::RightAnti => (true, false),
                JoinKind::Full => (true, true),
            };
            mark(&join.left, nullable || left, nulled);
            mark(&join.right, nullable || right, nulled);
        }
    }
}

/// The state of the fold of the expressions of one query.
struct Fold<'a> {
    params: Option<&'a [Value]>,
    session: &'a dyn Session,
    relations: &'a [Relation],
    /// For each relation, true when an outer join can make its columns null where the expression is.
    nulled: Vec<bool>,
    /// For each relation, true for a subquery that the planner pulls up into the query.
    pulled: Vec<bool>,
    /// True when the planner pulls up a `VALUES` of one row, which it does not in the query of a write.
    values: bool,
    /// `case_val`: the argument of each open `CASE` when it is a constant.
    case: Vec<Option<Expr>>,
    /// `active_fns`: the SQL functions whose bodies the fold puts in the query now, so that a function that calls itself does not go in again.
    inlining: Vec<u32>,
}

/// An expression with the type, the typmod and the place of another.
fn with(expr: &Expr, kind: ExprKind) -> Expr {
    Expr { kind, ty: expr.ty, typmod: expr.typmod, location: expr.location }
}

/// `negate_clause` of `prepqual.c`: the expression that is true when `arg` is false, which is the `NOT` of `not`. An operator with a negator, also of `ANY` or `ALL`, becomes the call of its negator, `AND` and `OR` change places with the negation of each argument, as the laws of De Morgan, `NOT` drops, and `IS NULL` and the tests of `IS TRUE` and the others take the opposite test. A test of `IS NULL` of a row does not change, because a row with some null fields is not null and not `IS NOT NULL`. The other expressions keep the `NOT`.
fn negate(not: &Expr, arg: &Expr) -> Expr {
    let keep = || with(not, ExprKind::Bool(BoolOp::Not, vec![arg.clone()]));
    match &arg.kind {
        ExprKind::Const(v) => {
            with(arg, ExprKind::Const(v.as_bool().map_or(Value::Null, |b| Value::Bool(!b))))
        }
        ExprKind::Func(f) => {
            let FuncForm::Operator(op) = f.form else { return keep() };
            let Some(row) =
                builtin::operator_by_oid(op).and_then(|o| builtin::operator_by_oid(o.negator))
            else {
                return keep();
            };
            let mut func = f.clone();
            func.oid = row.code;
            func.form = FuncForm::Operator(row.oid);
            with(arg, ExprKind::Func(func))
        }
        ExprKind::ScalarArrayOp { func, any, args } => {
            let [left, array] = args.as_slice() else { return keep() };
            let element = builtin::type_by_oid(array.ty).map_or(0, |t| t.elem);
            let mut ops = builtin::operators().iter().filter(|o| o.code == *func);
            let op = ops
                .clone()
                .find(|o| o.left == left.ty && o.right == element)
                .or_else(|| ops.next());
            match op.and_then(|o| builtin::operator_by_oid(o.negator)) {
                Some(row) => with(
                    arg,
                    ExprKind::ScalarArrayOp { func: row.code, any: !any, args: args.clone() },
                ),
                None => keep(),
            }
        }
        ExprKind::Bool(op @ (BoolOp::And | BoolOp::Or), args) => {
            let op = if *op == BoolOp::And { BoolOp::Or } else { BoolOp::And };
            let not = |a: &Expr| with(a, ExprKind::Bool(BoolOp::Not, vec![a.clone()]));
            let args = args.iter().map(|a| negate(&not(a), a)).collect();
            with(arg, ExprKind::Bool(op, args))
        }
        ExprKind::Bool(BoolOp::Not, args) => match args.as_slice() {
            [inner] => inner.clone(),
            _ => keep(),
        },
        ExprKind::NullTest(inner, is_null) if !rupg_func::is_row_type(inner.ty) => {
            with(arg, ExprKind::NullTest(inner.clone(), !is_null))
        }
        ExprKind::BooleanTest(inner, test) => {
            let test = match test {
                BoolTest::IsTrue => BoolTest::IsNotTrue,
                BoolTest::IsNotTrue => BoolTest::IsTrue,
                BoolTest::IsFalse => BoolTest::IsNotFalse,
                BoolTest::IsNotFalse => BoolTest::IsFalse,
                BoolTest::IsUnknown => BoolTest::IsNotUnknown,
                BoolTest::IsNotUnknown => BoolTest::IsUnknown,
            };
            with(arg, ExprKind::BooleanTest(inner.clone(), test))
        }
        _ => keep(),
    }
}

/// The value of a constant expression.
fn value(expr: &Expr) -> Option<&Value> {
    match &expr.kind {
        ExprKind::Const(value) => Some(value),
        _ => None,
    }
}

/// True when the function is immutable, as `ece_function_is_safe` tests it.
fn immutable(func: u32) -> bool {
    builtin::proc_by_oid(func).is_some_and(|p| p.volatile == b'i')
}

/// The bodies of the SQL functions of `pg_proc` that `inline_function` puts in a query: the function of the body, its operator, and the argument that the body casts to `text`.
fn inlined_body(func: u32) -> Option<(u32, Option<u32>, usize)> {
    match func {
        // `quote_literal(anyelement)`: `select pg_catalog.quote_literal($1::pg_catalog.text)`.
        1285 => Some((1283, None, 0)),
        // `quote_nullable(anyelement)`: `select pg_catalog.quote_nullable($1::pg_catalog.text)`.
        1290 => Some((1289, None, 0)),
        // `textanycat`: `select $1 operator(pg_catalog.||) $2::pg_catalog.text`.
        2003 => Some((TEXTCAT, Some(TEXT_CONCAT), 1)),
        // `anytextcat`: `select $1::pg_catalog.text operator(pg_catalog.||) $2`.
        2004 => Some((TEXTCAT, Some(TEXT_CONCAT), 0)),
        _ => None,
    }
}

/// The expression of the body of a function in SQL that `inline_function` can put in a query: a `SELECT` of one column with no other clause.
fn simple_body(query: &Query) -> Option<&Expr> {
    let simple = query.relations.is_empty()
        && query.from.is_empty()
        && query.filter.is_none()
        && !query.grouped
        && query.group.is_empty()
        && query.grouping_sets.is_empty()
        && query.having.is_none()
        && query.sort.is_empty()
        && query.distinct.is_empty()
        && query.offset.is_none()
        && query.limit.is_none()
        && query.set_op.is_none()
        && query.windows.is_empty()
        && query.ctes.is_empty()
        && query.row_marks.is_empty()
        && query.write.is_none()
        && !query.has_target_srfs()
        && agg::calls(query).is_empty();
    match query.targets.as_slice() {
        [target] if simple && !target.junk => Some(&target.expr),
        _ => None,
    }
}

/// `contain_context_dependent_node`: true when the expression reads the value of a `CASE` around it. The conditions of a `CASE` with an argument read the argument of that `CASE`.
fn context_dependent(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::CaseTest => true,
        ExprKind::Case(case) if case.arg.is_some() => {
            case.arg.as_deref().is_some_and(context_dependent)
                || case.whens.iter().any(|(_, then)| context_dependent(then))
                || context_dependent(&case.default)
        }
        ExprKind::ArrayCoerce { arg, .. } => context_dependent(arg),
        _ => expr.children().into_iter().any(context_dependent),
    }
}

/// The OID of the language `sql`.
const SQL_LANGUAGE: u32 = 14;

/// `textcat`.
const TEXTCAT: u32 = 1258;
/// The operator `||` of `text` and `text`.
const TEXT_CONCAT: u32 = 654;

/// True when a cast calls no volatile function: the function of the cast, or the output function of the type of the argument and the input function of the target.
fn cast_not_volatile(cast: &Expr) -> bool {
    let not_volatile = |func: u32| builtin::proc_by_oid(func).is_some_and(|p| p.volatile != b'v');
    match &cast.kind {
        ExprKind::Func(f) => not_volatile(f.oid),
        ExprKind::CoerceViaIo(arg, _) => {
            builtin::type_by_oid(arg.ty).is_some_and(|t| not_volatile(t.output))
                && builtin::type_by_oid(cast.ty).is_some_and(|t| not_volatile(t.input))
        }
        _ => true,
    }
}

impl<'a> Fold<'a> {
    /// The state of the fold of a query.
    fn new(query: &'a Query, params: Option<&'a [Value]>, session: &'a dyn Session) -> Self {
        let mut nulled = vec![false; query.relations.len()];
        for item in &query.from {
            mark(item, false, &mut nulled);
        }
        // The `SELECT` parts of a set operation are not subqueries that the planner pulls up.
        let pulled = query
            .relations
            .iter()
            .map(|r| query.set_op.is_none() && r.subquery.as_deref().is_some_and(simple_subquery))
            .collect();
        let values = query.write.is_none() && query.set_op.is_none();
        Fold {
            params,
            session,
            relations: &query.relations,
            nulled,
            pulled,
            values,
            case: Vec::new(),
            inlining: Vec::new(),
        }
    }

    /// The value of the column `var` of a `VALUES` of one row with no volatile function and no function that gives a set, as `is_simple_values` finds it.
    fn values_row(&self, var: Var) -> Option<&'a Expr> {
        if !self.values {
            return None;
        }
        let [row] = self.relations.get(var.relation)?.values.as_deref()? else { return None };
        if row.iter().any(|e| volatile(e) || e.first_set_call().is_some()) {
            return None;
        }
        row.get(usize::try_from(var.attnum - 1).ok()?)
    }

    /// The subquery in `FROM` that the planner pulls up for a relation.
    fn pulled_query(&self, relation: usize) -> Option<&'a Query> {
        let relations = self.relations;
        if !self.pulled.get(relation).copied().unwrap_or(false) {
            return None;
        }
        relations.get(relation)?.subquery.as_deref()
    }

    /// `pull_up_simple_subquery`: a column of a subquery that the planner pulls up is the expression of the subquery, which folds where the query reads the column. A constant takes the place of the column, unless an outer join can make the column null, where a `PlaceHolderVar` keeps it. A column of a `VALUES` of one row is the value of the row, as `pull_up_simple_values` pulls it up when `is_simple_values` lets it.
    fn var(&mut self, expr: &Expr, var: Var) -> Result<Expr> {
        if let Some(value) = (var.levels_up == 0).then(|| self.values_row(var)).flatten() {
            let folded = self.expr(value)?;
            return Ok(match folded.kind {
                ExprKind::Const(v) if !self.nulled[var.relation] => with(expr, ExprKind::Const(v)),
                _ => expr.clone(),
            });
        }
        let sub = if var.levels_up == 0 { self.pulled_query(var.relation) } else { None };
        let target =
            sub.zip(usize::try_from(var.attnum - 1).ok()).and_then(|(s, i)| s.targets.get(i));
        let (Some(sub), Some(target)) = (sub, target) else { return Ok(expr.clone()) };
        let folded = Fold::new(sub, self.params, self.session).expr(&target.expr)?;
        Ok(match folded.kind {
            ExprKind::Const(v) if !self.nulled[var.relation] => with(expr, ExprKind::Const(v)),
            _ => expr.clone(),
        })
    }

    /// `preprocess_expression`: folds the expressions of a clause, then the subqueries that the folded expressions still have.
    fn clause(&mut self, exprs: &[&Expr]) -> Result<Vec<Expr>> {
        let mut folded = exprs.iter().map(|e| self.expr(e)).collect::<Result<Vec<_>>>()?;
        for expr in &mut folded {
            self.subqueries(expr)?;
        }
        Ok(folded)
    }

    /// A clause of one expression, or `None`.
    fn one(&mut self, expr: Option<&Expr>) -> Result<Option<Expr>> {
        let Some(expr) = expr else { return Ok(None) };
        Ok(self.clause(&[expr])?.pop())
    }

    /// `preprocess_qual_conditions`: the conditions of the joins of a part of `FROM`, the inner joins first. In the condition of a join, the join itself does not make the columns null.
    fn join(&mut self, item: &FromItem) -> Result<FromItem> {
        let FromItem::Join(join) = item else {
            // The conditions of a subquery that the planner pulls up fold at its place in `FROM`.
            if let FromItem::Relation(index) = item
                && let Some(sub) = self.pulled_query(*index)
            {
                let mut fold = Fold::new(sub, self.params, self.session);
                for item in &sub.from {
                    fold.join(item)?;
                }
                fold.one(sub.filter.as_ref())?;
            }
            return Ok(item.clone());
        };
        let left = self.join(&join.left)?;
        let right = self.join(&join.right)?;
        let mut on = None;
        if let Some(condition) = &join.on {
            let mut nulled = vec![false; self.relations.len()];
            mark(&join.left, false, &mut nulled);
            mark(&join.right, false, &mut nulled);
            let outer = std::mem::replace(&mut self.nulled, nulled);
            let result = self.one(Some(condition));
            self.nulled = outer;
            on = result?.map(prepqual::canonicalize);
        }
        Ok(FromItem::Join(Box::new(Join {
            kind: join.kind,
            left,
            right,
            on,
            using: join.using.clone(),
            plain_using: join.plain_using,
            alias: join.alias.clone(),
            using_alias: join.using_alias.clone(),
            merged: join.merged.clone(),
            columns: join.columns.clone(),
            aliases: join.aliases.clone(),
            sources: join.sources.clone(),
        })))
    }

    /// `SS_process_sublinks`: folds the subqueries of a folded expression.
    fn subqueries(&self, expr: &mut Expr) -> Result<()> {
        match &mut expr.kind {
            ExprKind::Const(_)
            | ExprKind::Param(_)
            | ExprKind::CaseTest
            | ExprKind::DomainValue
            | ExprKind::SqlValue(_)
            | ExprKind::Var(_)
            | ExprKind::SubColumn(_) => Ok(()),
            ExprKind::Func(f) => self.all(&mut f.args),
            ExprKind::NamedArg(na) => self.subqueries(&mut na.arg),
            ExprKind::Relabel(arg, _)
            | ExprKind::Collate(arg, _)
            | ExprKind::CoerceViaIo(arg, _)
            | ExprKind::CoerceToDomain(arg, _)
            | ExprKind::NullTest(arg, _)
            | ExprKind::BooleanTest(arg, _)
            | ExprKind::ConvertRow { arg, .. }
            | ExprKind::FieldSelect(arg, _) => self.subqueries(arg),
            ExprKind::ArrayCoerce { arg, element, .. } => {
                self.subqueries(arg)?;
                self.subqueries(element)
            }
            ExprKind::Subscript(sub) => {
                let Subscript { container, upper, lower, assign } = &mut **sub;
                for bound in upper.iter_mut().chain(lower.iter_mut().flatten()).flatten() {
                    self.subqueries(bound)?;
                }
                if let Some(assign) = assign {
                    self.subqueries(assign)?;
                }
                self.subqueries(container)
            }
            ExprKind::FieldStore(store) => {
                for (_, value) in &mut store.fields {
                    self.subqueries(value)?;
                }
                self.subqueries(&mut store.arg)
            }
            ExprKind::Bool(_, args)
            | ExprKind::Coalesce(args)
            | ExprKind::MinMax { args, .. }
            | ExprKind::NullIf { args, .. }
            | ExprKind::Distinct { args, .. }
            | ExprKind::ScalarArrayOp { args, .. } => self.all(args),
            ExprKind::Array { elements, .. } | ExprKind::Row(elements, _) => self.all(elements),
            ExprKind::JoinRow(row) => self.all(&mut row.fields),
            ExprKind::RowCompare(rc) => self.all(&mut rc.ops),
            ExprKind::Case(case) => {
                if let Some(arg) = &mut case.arg {
                    self.subqueries(arg)?;
                }
                for (when, then) in &mut case.whens {
                    self.subqueries(when)?;
                    self.subqueries(then)?;
                }
                self.subqueries(&mut case.default)
            }
            ExprKind::Agg(agg) => {
                self.all(&mut agg.direct)?;
                self.all(&mut agg.args)?;
                match &mut agg.filter {
                    Some(filter) => self.subqueries(filter),
                    None => Ok(()),
                }
            }
            ExprKind::Grouping(g) => self.all(&mut g.args),
            ExprKind::PlaceHolder(p) => self.subqueries(&mut p.expr),
            ExprKind::JsonValue(v) => {
                self.subqueries(&mut v.raw)?;
                self.subqueries(&mut v.formatted)
            }
            ExprKind::JsonCtor(c) => {
                self.all(&mut c.args)?;
                if let Some(func) = &mut c.func {
                    self.subqueries(func)?;
                }
                match &mut c.coercion {
                    Some(coercion) => self.subqueries(coercion),
                    None => Ok(()),
                }
            }
            ExprKind::IsJson(p) => self.subqueries(&mut p.expr),
            ExprKind::JsonExpr(j) => {
                j.children_mut().into_iter().try_for_each(|e| self.subqueries(e))
            }
            ExprKind::JsonTable(t) => {
                t.children_mut().into_iter().try_for_each(|e| self.subqueries(e))
            }
            ExprKind::Xml(x) => x.children_mut().into_iter().try_for_each(|e| self.subqueries(e)),
            ExprKind::CurrentOf(c) => self.all(&mut c.args),
            ExprKind::Window(w) => {
                self.all(&mut w.args)?;
                match &mut w.filter {
                    Some(filter) => self.subqueries(filter),
                    None => Ok(()),
                }
            }
            ExprKind::SubLink(sub) => {
                if let Some(test) = &mut sub.test {
                    self.subqueries(test)?;
                }
                let exists = sub.kind == SubLinkKind::Exists;
                sub.query = plan(&sub.query, self.params, self.session, exists)?;
                Ok(())
            }
        }
    }

    fn all(&self, exprs: &mut [Expr]) -> Result<()> {
        exprs.iter_mut().try_for_each(|e| self.subqueries(e))
    }

    /// The constant of the value of an expression, as `evaluate_expr` makes it.
    fn evaluate(&self, expr: Expr) -> Result<Expr> {
        let value = evaluate(&expr, self.session)?;
        Ok(with(&expr, ExprKind::Const(value)))
    }

    fn list(&mut self, exprs: &[Expr]) -> Result<Vec<Expr>> {
        exprs.iter().map(|e| self.expr(e)).collect()
    }

    /// `eval_const_expressions_mutator`.
    fn expr(&mut self, expr: &Expr) -> Result<Expr> {
        match &expr.kind {
            ExprKind::Const(_) | ExprKind::SqlValue(_) | ExprKind::SubColumn(_) => Ok(expr.clone()),
            ExprKind::Var(var) => self.var(expr, *var),
            ExprKind::Param(n) => {
                let given = n.checked_sub(1).and_then(|i| self.params?.get(i));
                Ok(given.map_or_else(|| expr.clone(), |v| with(expr, ExprKind::Const(v.clone()))))
            }
            ExprKind::Func(f) => {
                // `simplify_function`: the arguments in the order of the function, with the defaults.
                let args = self.list(&rupg_analyze::expand_arguments(f)?)?;
                let func = Func { args, ..f.clone() };
                self.function(with(expr, ExprKind::Func(func)))
            }
            ExprKind::NamedArg(na) => {
                let arg = self.expr(&na.arg)?;
                Ok(with(
                    expr,
                    ExprKind::NamedArg(Box::new(NamedArg {
                        arg,
                        name: na.name.clone(),
                        number: na.number,
                    })),
                ))
            }
            // The executor does not use the collation, so the folded argument takes the place of the node.
            ExprKind::Collate(arg, _) => self.expr(arg),
            ExprKind::Relabel(arg, form) => {
                let arg = self.expr(arg)?;
                Ok(match arg.kind {
                    ExprKind::Const(v) => with(expr, ExprKind::Const(relabel(v, expr.ty))),
                    _ => with(expr, ExprKind::Relabel(Box::new(arg), *form)),
                })
            }
            ExprKind::CoerceViaIo(arg, form) => {
                let arg = self.expr(arg)?;
                self.coerce(with(expr, ExprKind::CoerceViaIo(Box::new(arg), *form)))
            }
            // `eval_const_expressions` keeps a `ConvertRowtypeExpr` and folds its argument.
            ExprKind::ConvertRow { arg, fields, form } => {
                let arg = Box::new(self.expr(arg)?);
                Ok(with(expr, ExprKind::ConvertRow { arg, fields: fields.clone(), form: *form }))
            }
            // A domain with no constraint is a relabel of the value. The checks of any other domain run with the query, as in PostgreSQL.
            ExprKind::CoerceToDomain(arg, form) => {
                let arg = self.expr(arg)?;
                let checked = crate::has_domain_constraints(expr.ty);
                Ok(match arg.kind {
                    ExprKind::Const(v) if !checked => {
                        with(expr, ExprKind::Const(relabel(v, expr.ty)))
                    }
                    _ => with(expr, ExprKind::CoerceToDomain(Box::new(arg), *form)),
                })
            }
            ExprKind::DomainValue => Ok(expr.clone()),
            ExprKind::FieldSelect(arg, field) => {
                let arg = self.expr(arg)?;
                // `eval_const_expressions`: a field of a whole-row `Var` of this level is the `Var` of the column, and a field of a `ROW` is its argument, when the type of the field is the type of the result.
                match &arg.kind {
                    ExprKind::Var(var) if var.attnum == 0 && var.levels_up == 0 => {
                        let columns = self.relations.get(var.relation).map(|r| &r.columns);
                        let column = columns.and_then(|columns| {
                            let mut live =
                                columns.iter().enumerate().filter(|(_, c)| !c.is_dropped());
                            live.nth(*field)
                        });
                        if let Some((at, column)) = column
                            && column.ty == expr.ty
                            && column.typmod == expr.typmod
                        {
                            let attnum = i16::try_from(at + 1).unwrap_or(i16::MAX);
                            let var = Var { attnum, ..*var };
                            return Ok(with(expr, ExprKind::Var(var)));
                        }
                    }
                    ExprKind::Row(args, _) => {
                        if let Some(field) = args.get(*field)
                            && field.ty == expr.ty
                            && field.typmod == expr.typmod
                        {
                            return Ok(field.clone());
                        }
                    }
                    _ => {}
                }
                Ok(match value(&arg) {
                    Some(Value::Record(record)) => {
                        let v = record.values.get(*field).cloned().unwrap_or(Value::Null);
                        with(expr, ExprKind::Const(v))
                    }
                    Some(Value::Null) => with(expr, ExprKind::Const(Value::Null)),
                    _ => with(expr, ExprKind::FieldSelect(Box::new(arg), *field)),
                })
            }
            ExprKind::ArrayCoerce { arg, element, form } => {
                let arg = self.expr(arg)?;
                // The CaseTest of the element is not the value of a CASE outside.
                self.case.push(None);
                let element = self.expr(element);
                self.case.pop();
                let element = element?;
                let fold = value(&arg).is_some() && !mutable(&element);
                let kind = ExprKind::ArrayCoerce {
                    arg: Box::new(arg),
                    element: Box::new(element),
                    form: *form,
                };
                if fold { self.evaluate(with(expr, kind)) } else { Ok(with(expr, kind)) }
            }
            ExprKind::Subscript(sub) => {
                let mut bounds = |list: &[Option<Expr>]| -> Result<Vec<Option<Expr>>> {
                    list.iter().map(|b| b.as_ref().map(|b| self.expr(b)).transpose()).collect()
                };
                let upper = bounds(&sub.upper)?;
                let lower = sub.lower.as_deref().map(&mut bounds).transpose()?;
                let container = self.expr(&sub.container)?;
                // The CaseTest of an assignment to a part of the element is not the value of a CASE outside.
                self.case.push(None);
                let assign = sub.assign.as_ref().map(|a| self.expr(a)).transpose();
                self.case.pop();
                let assign = assign?;
                let folded = Subscript { container, upper, lower, assign };
                let all = folded
                    .bounds()
                    .chain([&folded.container])
                    .chain(&folded.assign)
                    .all(|e| value(e).is_some());
                let kind = ExprKind::Subscript(Box::new(folded));
                if all { self.evaluate(with(expr, kind)) } else { Ok(with(expr, kind)) }
            }
            ExprKind::FieldStore(store) => {
                let arg = self.expr(&store.arg)?;
                // The CaseTest of each new value is the old value of the field.
                self.case.push(None);
                let fields: Result<Vec<(usize, Expr)>> =
                    store.fields.iter().map(|(i, v)| Ok((*i, self.expr(v)?))).collect();
                self.case.pop();
                let store = FieldStore { arg, fields: fields?, types: store.types.clone() };
                Ok(with(expr, ExprKind::FieldStore(Box::new(store))))
            }
            ExprKind::Bool(BoolOp::Not, args) => {
                let args = self.list(args)?;
                Ok(match args.first().and_then(value) {
                    Some(v) => {
                        let not = v.as_bool().map_or(Value::Null, |b| Value::Bool(!b));
                        with(expr, ExprKind::Const(not))
                    }
                    None => match args.as_slice() {
                        [arg] => negate(expr, arg),
                        _ => with(expr, ExprKind::Bool(BoolOp::Not, args)),
                    },
                })
            }
            ExprKind::Bool(op, args) => self.and_or(expr, *op, args),
            ExprKind::NullTest(arg, is_null) => {
                let arg = self.expr(arg)?;
                // A test of a `ROW` expression is the test of each field that can be null, with `AND`. A constant field, or a field that is not null, decides the test or drops.
                if let ExprKind::Row(fields, _) = &arg.kind {
                    let mut tests = Vec::new();
                    for field in fields {
                        let null = match value(field) {
                            Some(v) => v.is_null(),
                            None if self.nonnullable(field) => false,
                            None => {
                                tests.push(with(
                                    expr,
                                    ExprKind::NullTest(Box::new(field.clone()), *is_null),
                                ));
                                continue;
                            }
                        };
                        if null != *is_null {
                            return Ok(with(expr, ExprKind::Const(Value::Bool(false))));
                        }
                    }
                    return Ok(match tests.len() {
                        0 => with(expr, ExprKind::Const(Value::Bool(true))),
                        1 => tests.remove(0),
                        _ => with(expr, ExprKind::Bool(BoolOp::And, tests)),
                    });
                }
                if let Some(v) = value(&arg) {
                    return Ok(with(expr, ExprKind::Const(Value::Bool(null_test(v, *is_null)))));
                }
                // A row with a null field is not null and not `IS NOT NULL`.
                if self.nonnullable(&arg) && !rupg_func::is_row_type(arg.ty) {
                    return Ok(with(expr, ExprKind::Const(Value::Bool(!is_null))));
                }
                Ok(with(expr, ExprKind::NullTest(Box::new(arg), *is_null)))
            }
            ExprKind::BooleanTest(arg, test) => {
                let arg = self.expr(arg)?;
                if let Some(v) = value(&arg) {
                    let v = v.as_bool();
                    let result = match test {
                        BoolTest::IsTrue => v == Some(true),
                        BoolTest::IsNotTrue => v != Some(true),
                        BoolTest::IsFalse => v == Some(false),
                        BoolTest::IsNotFalse => v != Some(false),
                        BoolTest::IsUnknown => v.is_none(),
                        BoolTest::IsNotUnknown => v.is_some(),
                    };
                    return Ok(with(expr, ExprKind::Const(Value::Bool(result))));
                }
                if self.nonnullable(&arg) {
                    return Ok(match test {
                        BoolTest::IsTrue | BoolTest::IsNotFalse => arg,
                        BoolTest::IsFalse | BoolTest::IsNotTrue => {
                            with(expr, ExprKind::Bool(BoolOp::Not, vec![arg]))
                        }
                        BoolTest::IsUnknown => with(expr, ExprKind::Const(Value::Bool(false))),
                        BoolTest::IsNotUnknown => with(expr, ExprKind::Const(Value::Bool(true))),
                    });
                }
                Ok(with(expr, ExprKind::BooleanTest(Box::new(arg), *test)))
            }
            ExprKind::Case(case) => self.case(expr, case),
            ExprKind::CaseTest => {
                Ok(self.case.last().cloned().flatten().unwrap_or_else(|| expr.clone()))
            }
            ExprKind::Coalesce(args) => self.coalesce(expr, args),
            ExprKind::MinMax { greatest, less, args } => {
                let args = self.list(args)?;
                let all = args.iter().all(|a| value(a).is_some());
                let folded =
                    with(expr, ExprKind::MinMax { greatest: *greatest, less: *less, args });
                if all { self.evaluate(folded) } else { Ok(folded) }
            }
            // `eval_const_expressions` folds the fields, but not the comparison, and keeps the operator of each pair of fields.
            ExprKind::RowCompare(rc) => {
                let mut ops = Vec::with_capacity(rc.ops.len());
                for op in &rc.ops {
                    let ExprKind::Func(f) = &op.kind else {
                        return Err(Error::internal("a row comparison that is not an operator"));
                    };
                    let args = self.list(&f.args)?;
                    ops.push(with(op, ExprKind::Func(Func { args, ..f.clone() })));
                }
                let rc = RowCompare { ops, ..(**rc).clone() };
                Ok(with(expr, ExprKind::RowCompare(Box::new(rc))))
            }
            // As in `eval_const_expressions`, a `ROW()` of constants is a constant. A view keeps the `ROW()` as it is written, because the fold is only for the plan.
            ExprKind::Row(args, form) => {
                let args = self.list(args)?;
                let all = args.iter().all(|a| value(a).is_some());
                let folded = with(expr, ExprKind::Row(args, *form));
                if all { self.evaluate(folded) } else { Ok(folded) }
            }
            ExprKind::JoinRow(row) => {
                let fields = self.list(&row.fields)?;
                let row = JoinRow { fields, ..(**row).clone() };
                Ok(with(expr, ExprKind::JoinRow(Box::new(row))))
            }
            ExprKind::Array { element, multidims, elements } => {
                let elements = self.list(elements)?;
                let all = elements.iter().all(|a| value(a).is_some());
                let kind = ExprKind::Array { element: *element, multidims: *multidims, elements };
                if all { self.evaluate(with(expr, kind)) } else { Ok(with(expr, kind)) }
            }
            ExprKind::NullIf { equal, args } => {
                let args = self.list(args)?;
                if args.iter().any(|a| value(a).is_some_and(Value::is_null)) {
                    return Ok(args.into_iter().next().unwrap_or_else(|| expr.clone()));
                }
                let constant = args.iter().all(|a| value(a).is_some());
                let folded = with(expr, ExprKind::NullIf { equal: *equal, args });
                if constant && immutable(*equal) { self.evaluate(folded) } else { Ok(folded) }
            }
            ExprKind::Distinct { equal, not, args } => {
                let args = self.list(args)?;
                let values: Option<Vec<&Value>> = args.iter().map(value).collect();
                let folded =
                    |args| with(expr, ExprKind::Distinct { equal: *equal, not: *not, args });
                let Some(values) = values else { return Ok(folded(args)) };
                let nulls = values.iter().filter(|v| v.is_null()).count();
                if nulls > 0 {
                    let distinct = nulls < values.len();
                    return Ok(with(expr, ExprKind::Const(Value::Bool(distinct != *not))));
                }
                if immutable(*equal) { self.evaluate(folded(args)) } else { Ok(folded(args)) }
            }
            ExprKind::ScalarArrayOp { func, any, args } => {
                let args = self.list(args)?;
                let all = args.iter().all(|a| value(a).is_some());
                let folded = with(expr, ExprKind::ScalarArrayOp { func: *func, any: *any, args });
                if all && immutable(*func) { self.evaluate(folded) } else { Ok(folded) }
            }
            ExprKind::Agg(agg) => {
                let mut agg = agg.clone();
                agg.direct = self.list(&agg.direct)?;
                agg.args = self.list(&agg.args)?;
                if let Some(filter) = &agg.filter {
                    agg.filter = Some(Box::new(self.expr(filter)?));
                }
                // `int8inc_support`: `count` of an argument that cannot be null is `count(*)`.
                if agg.oid == COUNT_ANY
                    && agg.levels_up == 0
                    && agg.order.is_empty()
                    && agg.distinct.is_empty()
                    && let [arg] = agg.args.as_slice()
                    && self.nonnullable(arg)
                {
                    agg.oid = COUNT_STAR;
                    agg.args.clear();
                    agg.nargs = 0;
                    agg.star = true;
                }
                Ok(with(expr, ExprKind::Agg(agg)))
            }
            // The value of `GROUPING` depends on the grouping set of the row.
            ExprKind::Grouping(_) => Ok(expr.clone()),
            // The value of a `PlaceHolderVar` depends on the outer join above it.
            ExprKind::PlaceHolder(_) => Ok(expr.clone()),
            // A `JsonValueExpr` with a constant value is the constant.
            ExprKind::JsonValue(v) => {
                let formatted = self.expr(&v.formatted)?;
                if value(&formatted).is_some() {
                    return Ok(formatted);
                }
                let raw = self.expr(&v.raw)?;
                let v = JsonValue { raw, formatted, format: v.format };
                Ok(with(expr, ExprKind::JsonValue(Box::new(v))))
            }
            // `JSON_ARRAY` with a subquery is the expression that gives its value.
            ExprKind::JsonCtor(c) if c.kind == JsonCtorKind::ArrayQuery => match &c.func {
                Some(func) => self.expr(func),
                None => Err(Error::internal("JSON_ARRAY with a subquery and no expression")),
            },
            // The constructor is not folded. The `CaseTest` of the coercion is not the value of a `CASE` outside.
            ExprKind::JsonCtor(c) => {
                let args = self.list(&c.args)?;
                let func = c.func.as_ref().map(|e| self.expr(e)).transpose()?;
                self.case.push(None);
                let coercion = c.coercion.as_ref().map(|e| self.expr(e)).transpose();
                self.case.pop();
                let c = JsonCtor { args, func, coercion: coercion?, ..(**c).clone() };
                Ok(with(expr, ExprKind::JsonCtor(Box::new(c))))
            }
            ExprKind::IsJson(p) => {
                let arg = self.expr(&p.expr)?;
                let p = IsJson { expr: arg, ..(**p).clone() };
                Ok(with(expr, ExprKind::IsJson(Box::new(p))))
            }
            // A SQL/JSON query function is not folded, as in PostgreSQL. Its parts are.
            ExprKind::JsonExpr(j) => {
                let mut j = j.clone();
                for part in j.children_mut() {
                    *part = self.expr(part)?;
                }
                Ok(with(expr, ExprKind::JsonExpr(j)))
            }
            // `XMLELEMENT` and `XMLFOREST` call the output functions of many types, so they are only stable and are not folded. The others are folded when all their arguments are constants.
            ExprKind::Xml(x) => {
                let mut x = x.clone();
                for part in x.children_mut() {
                    *part = self.expr(part)?;
                }
                let all = x.children().into_iter().all(|a| value(a).is_some());
                let immutable = !matches!(x.op, XmlOp::Element | XmlOp::Forest);
                let folded = with(expr, ExprKind::Xml(x));
                if all && immutable { self.evaluate(folded) } else { Ok(folded) }
            }
            ExprKind::CurrentOf(c) => {
                let mut c = c.clone();
                c.args = self.list(&c.args)?;
                Ok(with(expr, ExprKind::CurrentOf(c)))
            }
            // The columns read the row of the plan as a `CaseTest`, which is not the value of a `CASE` around the call.
            ExprKind::JsonTable(t) => {
                let mut t = t.clone();
                self.case.push(None);
                let folded: Result<()> = t.children_mut().into_iter().try_for_each(|part| {
                    *part = self.expr(part)?;
                    Ok(())
                });
                self.case.pop();
                folded?;
                Ok(with(expr, ExprKind::JsonTable(t)))
            }
            ExprKind::Window(w) => {
                let mut w = w.clone();
                w.args = self.list(&rupg_analyze::expand_window_arguments(&w)?)?;
                if let Some(filter) = &w.filter {
                    w.filter = Some(Box::new(self.expr(filter)?));
                }
                Ok(with(expr, ExprKind::Window(w)))
            }
            ExprKind::SubLink(sub) => {
                let volatile = sub.volatile || sub.test.as_ref().is_some_and(volatile);
                let test = sub.test.as_ref().map(|t| self.expr(t)).transpose()?;
                let sub = SubLink {
                    kind: sub.kind,
                    test,
                    query: sub.query.clone(),
                    alternative: None,
                    volatile,
                    pulled: sub.pulled,
                };
                Ok(with(expr, ExprKind::SubLink(Box::new(sub))))
            }
        }
    }

    /// `simplify_function` and `evaluate_function`: a strict function with a null constant argument is null, and an immutable function with constant arguments is its value.
    fn function(&mut self, expr: Expr) -> Result<Expr> {
        let ExprKind::Func(f) = &expr.kind else { return Ok(expr) };
        let Some(proc) = builtin::proc_by_oid(f.oid) else { return Ok(expr) };
        if proc.retset || proc.rettype == oid::RECORD {
            return Ok(expr);
        }
        let values: Vec<Option<&Value>> = f.args.iter().map(value).collect();
        if proc.strict && values.iter().flatten().any(|v| v.is_null()) {
            return Ok(with(&expr, ExprKind::Const(Value::Null)));
        }
        if let Some(inlined) = self.inline(&expr, f)? {
            return Ok(inlined);
        }
        if values.iter().any(Option::is_none) || proc.volatile != b'i' {
            let f = f.clone();
            return Ok(self.inline_sql(&expr, &f, proc)?.unwrap_or(expr));
        }
        self.evaluate(expr)
    }

    /// `inline_function` for a function in SQL of the catalog: the body takes the place of the call, with the arguments in the places of its parameters, and then the body folds. The function must not be `SECURITY DEFINER`, have no settings, and its body must be one `SELECT` of one expression with no `FROM` and no other clause, with no aggregate, no subquery and no function that gives a set. The body of an immutable function must call only immutable functions, and the body of a stable function no volatile function. The body of a strict function must give null for a null argument, so it must use each parameter and have no part that can give a value for a null. An argument that the body uses more than one time must have no subquery, no volatile function and a cost of no more than 10 operators. The body must not read the value of a `CASE` around it.
    fn inline_sql(
        &mut self,
        expr: &Expr,
        f: &Func,
        proc: &builtin::ProcRow,
    ) -> Result<Option<Expr>> {
        if proc.oid < rupg_catalog::FIRST_NORMAL_OID
            || proc.lang != SQL_LANGUAGE
            || proc.kind != b'f'
            || proc.retset
            || proc.rettype == oid::RECORD
            || f.args.len() != usize::try_from(proc.nargs).unwrap_or(usize::MAX)
            || self.inlining.contains(&f.oid)
        {
            return Ok(None);
        }
        let catalog = self.session.catalog();
        if catalog
            .and_then(|c| c.function(f.oid))
            .is_none_or(|function| function.secdef || !function.config.is_empty())
        {
            return Ok(None);
        }
        let inputs: Vec<u32> = f.args.iter().map(|a| a.ty).collect();
        let body = match rupg_func::inline_body(f.oid, &inputs, self.session) {
            Ok(Some(body)) => body,
            Ok(None) => return Ok(None),
            // `sql_inline_error_callback`: the position of an error is in the body, which is the internal query.
            Err(e) => {
                let function = catalog.and_then(|c| c.function(f.oid));
                let src = function.map_or("", |function| function.src.as_str());
                let e = match e.position() {
                    Some(at) if !src.is_empty() => {
                        e.with_position(None).with_internal(src, Some(at))
                    }
                    _ => e.with_position(None),
                };
                let name = function.map_or("", |function| function.name.as_str());
                return Err(e.with_context(format!("SQL function \"{name}\" during inlining")));
            }
        };
        let mut body = match body {
            rupg_analyze::Body::Value(body) => body,
            rupg_analyze::Body::Query(query) => match simple_body(&query) {
                Some(body) => body.clone(),
                None => return Ok(None),
            },
            rupg_analyze::Body::Statements(_) => return Ok(None),
        };
        let unsafe_body = match proc.volatile {
            b'i' => mutable(&body),
            b's' => volatile(&body),
            _ => false,
        };
        if body.ty != expr.ty
            || unsafe_body
            || (proc.strict && rupg_opt::nonstrict(&body))
            || context_dependent(&body)
            || body.first_agg().is_some()
            || body.first_window().is_some()
            || body.first_set_call().is_some()
            || body
                .find(0, &mut |e, _| matches!(e.kind, ExprKind::SubLink(_)).then_some(()))
                .is_some()
        {
            return Ok(None);
        }
        let mut uses = vec![0usize; f.args.len()];
        body.find(0, &mut |e, _| {
            if let ExprKind::Param(n) = e.kind
                && let Some(count) = n.checked_sub(1).and_then(|i| uses.get_mut(i))
            {
                *count += 1;
            }
            None::<()>
        });
        for (arg, &count) in f.args.iter().zip(&uses) {
            if count == 0 && proc.strict {
                return Ok(None);
            }
            if count > 1 {
                let mut cost = 0.0;
                let subplan = arg
                    .find(0, &mut |e, _| {
                        cost += e.own_cost();
                        matches!(e.kind, ExprKind::SubLink(_)).then_some(())
                    })
                    .is_some();
                if subplan || cost > 10.0 || volatile(arg) {
                    return Ok(None);
                }
            }
        }
        body.each_mut(0, &mut |e, _| {
            let ExprKind::Param(n) = e.kind else { return false };
            if let Some(arg) = n.checked_sub(1).and_then(|i| f.args.get(i)) {
                *e = arg.clone();
            }
            true
        });
        body.location = expr.location;
        self.inlining.push(f.oid);
        let folded = self.expr(&body);
        self.inlining.pop();
        let name =
            catalog.and_then(|c| c.function(f.oid)).map_or_else(String::new, |c| c.name.clone());
        folded
            .map(Some)
            .map_err(|e| e.with_context(format!("SQL function \"{name}\" during inlining")))
    }

    /// `inline_function` for the SQL functions of [`inlined_body`]: the body takes the place of the call, with the argument of any type cast to `text`, and then the cast and the call of the body fold. The body must call no volatile function, and the argument must not read the value of a `CASE` around it, as `contain_context_dependent_node` tests it.
    fn inline(&mut self, expr: &Expr, f: &Func) -> Result<Option<Expr>> {
        let Some((func, op, at)) = inlined_body(f.oid) else { return Ok(None) };
        let Some(arg) = f.args.get(at) else { return Ok(None) };
        let dependent = arg
            .find(0, &mut |e, depth| {
                (depth == 0 && matches!(e.kind, ExprKind::CaseTest)).then_some(())
            })
            .is_some();
        let Some(cast) = rupg_analyze::explicit_cast(arg.clone(), oid::TEXT) else {
            return Ok(None);
        };
        if dependent || !cast_not_volatile(&cast) {
            return Ok(None);
        }
        let cast = match &cast.kind {
            ExprKind::Relabel(inner, _) => match &inner.kind {
                ExprKind::Const(v) => with(&cast, ExprKind::Const(relabel(v.clone(), oid::TEXT))),
                _ => cast,
            },
            ExprKind::CoerceViaIo(..) => self.coerce(cast)?,
            ExprKind::Func(_) => self.function(cast)?,
            _ => cast,
        };
        let mut args = f.args.clone();
        args[at] = cast;
        let form = op.map_or(FuncForm::Call, FuncForm::Operator);
        let call = Func { oid: func, args, form, variadic: false, retset: false };
        self.function(Expr {
            kind: ExprKind::Func(call),
            ty: oid::TEXT,
            typmod: -1,
            ..expr.clone()
        })
        .map(Some)
    }

    /// `CoerceViaIO`: the output function of the type of the argument, then the input function of the type of the result. Each one folds as a function does.
    fn coerce(&self, expr: Expr) -> Result<Expr> {
        let ExprKind::CoerceViaIo(arg, _) = &expr.kind else { return Ok(expr) };
        let procs = builtin::type_by_oid(arg.ty)
            .and_then(|t| builtin::proc_by_oid(t.output))
            .zip(builtin::type_by_oid(expr.ty).and_then(|t| builtin::proc_by_oid(t.input)));
        let (Some(v), Some((output, input))) = (value(arg), procs) else { return Ok(expr) };
        if v.is_null() {
            if output.strict && input.strict {
                return Ok(with(&expr, ExprKind::Const(Value::Null)));
            }
            return Ok(expr);
        }
        if output.volatile == b'i' && input.volatile == b'i' {
            self.evaluate(expr)
        } else {
            Ok(expr)
        }
    }

    /// `simplify_and_arguments` and `simplify_or_arguments`. `AND` stops at the first false constant and `OR` at the first true constant, so the arguments after it do not fold.
    fn and_or(&mut self, expr: &Expr, op: BoolOp, args: &[Expr]) -> Result<Expr> {
        let stop = op == BoolOp::Or;
        let mut kept = Vec::new();
        let mut null = false;
        for arg in args {
            let arg = self.expr(arg)?;
            match value(&arg).map(Value::as_bool) {
                Some(Some(v)) if v == stop => {
                    return Ok(with(expr, ExprKind::Const(Value::Bool(stop))));
                }
                Some(Some(_)) => {}
                Some(None) => null = true,
                None => kept.push(arg),
            }
        }
        if null {
            kept.push(with(expr, ExprKind::Const(Value::Null)));
        }
        Ok(match kept.len() {
            0 => with(expr, ExprKind::Const(Value::Bool(!stop))),
            1 => kept.pop().unwrap_or_else(|| expr.clone()),
            _ => with(expr, ExprKind::Bool(op, kept)),
        })
    }

    /// `CaseExpr`: a condition that is a false or null constant drops its result with no fold. A condition that is a true constant makes its result the result of `ELSE`, and the conditions after it do not fold.
    fn case(&mut self, expr: &Expr, case: &Case) -> Result<Expr> {
        let arg = case.arg.as_ref().map(|a| self.expr(a)).transpose()?;
        self.case.push(arg.clone().filter(|a| value(a).is_some()));
        let result = self.case_whens(expr, case, arg);
        self.case.pop();
        result
    }

    fn case_whens(&mut self, expr: &Expr, case: &Case, arg: Option<Expr>) -> Result<Expr> {
        let mut whens = Vec::new();
        let mut default = None;
        for (when, then) in &case.whens {
            let condition = self.expr(when)?;
            let certain = match value(&condition) {
                Some(v) if *v != Value::Bool(true) => continue,
                Some(_) => true,
                None => false,
            };
            let result = self.expr(then)?;
            if !certain {
                whens.push((condition, result));
                continue;
            }
            default = Some(result);
            break;
        }
        let default = match default {
            Some(default) => default,
            None => self.expr(&case.default)?,
        };
        if whens.is_empty() {
            return Ok(default);
        }
        let case = Case { arg: arg.map(Box::new), whens, default: Box::new(default) };
        Ok(with(expr, ExprKind::Case(case)))
    }

    /// `CoalesceExpr`: the null constants drop, and the fold stops after the first argument that cannot be null.
    fn coalesce(&mut self, expr: &Expr, args: &[Expr]) -> Result<Expr> {
        let mut kept = Vec::new();
        for arg in args {
            let arg = self.expr(arg)?;
            if value(&arg).is_some_and(Value::is_null) {
                continue;
            }
            let last = value(&arg).is_some() || self.nonnullable(&arg);
            kept.push(arg);
            if last {
                break;
            }
        }
        Ok(match kept.len() {
            0 => with(expr, ExprKind::Const(Value::Null)),
            1 => kept.pop().unwrap_or_else(|| expr.clone()),
            _ => with(expr, ExprKind::Coalesce(kept)),
        })
    }

    /// `expr_is_nonnullable`: true when the expression cannot be null.
    fn nonnullable(&self, expr: &Expr) -> bool {
        match &expr.kind {
            ExprKind::Var(var) => self.var_nonnullable(*var),
            ExprKind::Const(v) => !v.is_null(),
            ExprKind::Coalesce(args) | ExprKind::MinMax { args, .. } => {
                args.iter().any(|a| self.nonnullable(a))
            }
            ExprKind::Case(case) => {
                self.nonnullable(&case.default)
                    && case.whens.iter().all(|(_, r)| self.nonnullable(r))
            }
            ExprKind::Array { .. } | ExprKind::NullTest(..) | ExprKind::BooleanTest(..) => true,
            // `IS NOT DISTINCT FROM` is a `NOT` above a `DistinctExpr`.
            ExprKind::Distinct { not, .. } => !not,
            ExprKind::Relabel(arg, _) | ExprKind::Collate(arg, _) => self.nonnullable(arg),
            _ => false,
        }
    }

    /// `var_is_nonnullable`: a column of a relation of the query that no outer join makes null, and that is a system column or has a not-null constraint.
    fn var_nonnullable(&self, var: Var) -> bool {
        if var.levels_up != 0 || self.nulled.get(var.relation).is_none_or(|n| *n) {
            return false;
        }
        if var.attnum < 0 {
            return true;
        }
        let column = usize::try_from(var.attnum - 1).ok();
        let relation = self.relations.get(var.relation);
        column.and_then(|c| relation?.columns.get(c)).is_some_and(|c| c.not_null)
    }
}

/// `contain_mutable_functions`: true when the expression calls a function that is not immutable.
fn mutable(expr: &Expr) -> bool {
    let mutable_proc = |oid: u32| builtin::proc_by_oid(oid).is_none_or(|p| p.volatile != b'i');
    expr.find(0, &mut |e, _| {
        let mutable = match &e.kind {
            ExprKind::Func(f) => mutable_proc(f.oid),
            ExprKind::NullIf { equal: func, .. }
            | ExprKind::Distinct { equal: func, .. }
            | ExprKind::ScalarArrayOp { func, .. }
            | ExprKind::MinMax { less: func, .. } => mutable_proc(*func),
            ExprKind::CoerceViaIo(arg, _) => {
                let output = builtin::type_by_oid(arg.ty).map_or(0, |t| t.output);
                let input = builtin::type_by_oid(e.ty).map_or(0, |t| t.input);
                mutable_proc(output) || mutable_proc(input)
            }
            ExprKind::SqlValue(_)
            | ExprKind::SubLink(_)
            | ExprKind::Agg(_)
            | ExprKind::Grouping(_)
            | ExprKind::Window(_) => true,
            ExprKind::Xml(x) => matches!(x.op, XmlOp::Element | XmlOp::Forest),
            _ => false,
        };
        mutable.then_some(())
    })
    .is_some()
}
