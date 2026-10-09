//! `eval_const_expressions` of `clauses.c`: the planner computes each part of an expression whose arguments are constants and whose function is immutable. An error of such a part, such as a division by zero, comes when the query is planned, before the server sends the description of the rows. The plan then runs the folded expressions, so a part that the fold drops does not run.

use rupg_analyze::{
    BoolOp, BoolTest, Case, Expr, ExprKind, FromItem, Func, Join, JoinKind, Query, Relation,
    SubLink, SubLinkKind, Subscript, Var,
};
use rupg_common::Result;
use rupg_func::Session;
use rupg_pgcatalog::builtin;
use rupg_types::{Value, oid};

use super::{agg, evaluate, null_test, relabel, volatile};

/// `subquery_planner`: folds the expressions of a query in the order of `preprocess_expression`, which is the target list, the conditions of the joins and `WHERE`, `HAVING`, `OFFSET` and `LIMIT`. The subqueries of each clause come after the clause, as `SS_process_sublinks` plans them. `params` has the values of the parameters of `Bind`, which a custom plan takes as constants.
pub(crate) fn query(
    query: &Query,
    params: Option<&[Value]>,
    session: &dyn Session,
) -> Result<Query> {
    plan(query, params, session, false)
}

/// Folds a query or a subquery. For the subquery of `EXISTS`, `simplify_EXISTS_query` drops the target list and a constant `LIMIT` first when it can. A subquery in `FROM` that the planner pulls up folds where the query reads its columns, then each subquery in `FROM` folds after the query, with null for each column that the query does not read.
fn plan(
    query: &Query,
    params: Option<&[Value]>,
    session: &dyn Session,
    exists: bool,
) -> Result<Query> {
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
    out.filter = fold.one(query.filter.as_ref())?;
    out.having = fold.one(query.having.as_ref())?;
    out.offset = fold.one(query.offset.as_ref())?;
    if !simple {
        out.limit = fold.one(query.limit.as_ref())?;
    }
    // The rows of a `VALUES` list fold after the clauses, as the range table does in `subquery_planner`.
    for (relation, old) in out.relations.iter_mut().zip(&query.relations) {
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
    for (relation, used) in out.relations.iter_mut().zip(&used) {
        if let Some(sub) = &mut relation.subquery {
            if dummy && !simple_subquery(sub) {
                continue;
            }
            // Each `SELECT` of a set operation gives all its columns.
            if out.set_op.is_none() {
                prune(sub, used);
            }
            **sub = plan(sub, params, session, false)?;
        }
    }
    Ok(out)
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
                JoinKind::Inner => (false, false),
                JoinKind::Left => (false, true),
                JoinKind::Right => (true, false),
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
    /// For each relation, true when an outer join of the query can make its columns null.
    wrapped: Vec<bool>,
    /// For each relation, true for a subquery that the planner pulls up into the query.
    pulled: Vec<bool>,
    /// `case_val`: the argument of each open `CASE` when it is a constant.
    case: Vec<Option<Expr>>,
}

/// An expression with the type, the typmod and the place of another.
fn with(expr: &Expr, kind: ExprKind) -> Expr {
    Expr { kind, ty: expr.ty, typmod: expr.typmod, location: expr.location }
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
        Fold {
            params,
            session,
            relations: &query.relations,
            wrapped: nulled.clone(),
            nulled,
            pulled,
            case: Vec::new(),
        }
    }

    /// The subquery in `FROM` that the planner pulls up for a relation.
    fn pulled_query(&self, relation: usize) -> Option<&'a Query> {
        let relations = self.relations;
        if !self.pulled.get(relation).copied().unwrap_or(false) {
            return None;
        }
        relations.get(relation)?.subquery.as_deref()
    }

    /// `pull_up_simple_subquery`: a column of a subquery that the planner pulls up is the expression of the subquery, which folds where the query reads the column. A constant takes the place of the column, unless an outer join can make the column null, where a `PlaceHolderVar` keeps it.
    fn var(&mut self, expr: &Expr, var: Var) -> Result<Expr> {
        let sub = if var.levels_up == 0 { self.pulled_query(var.relation) } else { None };
        let target =
            sub.zip(usize::try_from(var.attnum - 1).ok()).and_then(|(s, i)| s.targets.get(i));
        let (Some(sub), Some(target)) = (sub, target) else { return Ok(expr.clone()) };
        let folded = Fold::new(sub, self.params, self.session).expr(&target.expr)?;
        Ok(match folded.kind {
            ExprKind::Const(v) if !self.wrapped[var.relation] => with(expr, ExprKind::Const(v)),
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
            on = result?;
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
            ExprKind::Relabel(arg, _)
            | ExprKind::CoerceViaIo(arg, _)
            | ExprKind::CoerceToDomain(arg, _)
            | ExprKind::NullTest(arg, _)
            | ExprKind::BooleanTest(arg, _)
            | ExprKind::FieldSelect(arg, _) => self.subqueries(arg),
            ExprKind::ArrayCoerce { arg, element, .. } => {
                self.subqueries(arg)?;
                self.subqueries(element)
            }
            ExprKind::Subscript(sub) => {
                let Subscript { container, upper, lower } = &mut **sub;
                for bound in upper.iter_mut().chain(lower.iter_mut().flatten()).flatten() {
                    self.subqueries(bound)?;
                }
                self.subqueries(container)
            }
            ExprKind::Bool(_, args)
            | ExprKind::Coalesce(args)
            | ExprKind::MinMax { args, .. }
            | ExprKind::NullIf { args, .. }
            | ExprKind::Distinct { args, .. }
            | ExprKind::ScalarArrayOp { args, .. } => self.all(args),
            ExprKind::Array { elements, .. } => self.all(elements),
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
                self.all(&mut agg.args)?;
                match &mut agg.filter {
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
                let args = self.list(&f.args)?;
                let func = Func { args, ..f.clone() };
                self.function(with(expr, ExprKind::Func(func)))
            }
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
            // A domain with no check constraint is a relabel of the value. The checks of any other domain run with the query, as in PostgreSQL.
            ExprKind::CoerceToDomain(arg, form) => {
                let arg = self.expr(arg)?;
                let checked = builtin::domain_by_oid(expr.ty).is_some_and(|d| !d.checks.is_empty());
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
                let folded = Subscript { container, upper, lower };
                let all = folded.bounds().chain([&folded.container]).all(|e| value(e).is_some());
                let kind = ExprKind::Subscript(Box::new(folded));
                if all { self.evaluate(with(expr, kind)) } else { Ok(with(expr, kind)) }
            }
            ExprKind::Bool(BoolOp::Not, args) => {
                let args = self.list(args)?;
                Ok(match args.first().and_then(value) {
                    Some(v) => {
                        let not = v.as_bool().map_or(Value::Null, |b| Value::Bool(!b));
                        with(expr, ExprKind::Const(not))
                    }
                    None => with(expr, ExprKind::Bool(BoolOp::Not, args)),
                })
            }
            ExprKind::Bool(op, args) => self.and_or(expr, *op, args),
            ExprKind::NullTest(arg, is_null) => {
                let arg = self.expr(arg)?;
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
                agg.args = self.list(&agg.args)?;
                if let Some(filter) = &agg.filter {
                    agg.filter = Some(Box::new(self.expr(filter)?));
                }
                Ok(with(expr, ExprKind::Agg(agg)))
            }
            ExprKind::SubLink(sub) => {
                let test = sub.test.as_ref().map(|t| self.expr(t)).transpose()?;
                let sub = SubLink { kind: sub.kind, test, query: sub.query.clone() };
                Ok(with(expr, ExprKind::SubLink(Box::new(sub))))
            }
        }
    }

    /// `simplify_function` and `evaluate_function`: a strict function with a null constant argument is null, and an immutable function with constant arguments is its value.
    fn function(&self, expr: Expr) -> Result<Expr> {
        let ExprKind::Func(f) = &expr.kind else { return Ok(expr) };
        let Some(proc) = builtin::proc_by_oid(f.oid) else { return Ok(expr) };
        if proc.retset || proc.rettype == oid::RECORD {
            return Ok(expr);
        }
        let values: Vec<Option<&Value>> = f.args.iter().map(value).collect();
        if proc.strict && values.iter().flatten().any(|v| v.is_null()) {
            return Ok(with(&expr, ExprKind::Const(Value::Null)));
        }
        if values.iter().any(Option::is_none) || proc.volatile != b'i' {
            return Ok(expr);
        }
        self.evaluate(expr)
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
            ExprKind::Relabel(arg, _) => self.nonnullable(arg),
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
            ExprKind::SqlValue(_) | ExprKind::SubLink(_) | ExprKind::Agg(_) => true,
            _ => false,
        };
        mutable.then_some(())
    })
    .is_some()
}
