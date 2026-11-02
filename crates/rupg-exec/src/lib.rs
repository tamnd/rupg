//! The vectorized executor, the point path, compressed execution, spill, the per-distinct result tables of document 14 section 14.6.
//!
//! This version reads the tables of the catalog. It joins the parts of `FROM`, tests the `WHERE` clause and computes the target list for each row, with the values of the functions that give a set as the node `ProjectSet` gives them. Then it applies `DISTINCT`, `ORDER BY`, `OFFSET` and `LIMIT` as the plan of PostgreSQL does with the nodes `Sort`, `Unique` and `Limit`. [`prepare`] checks that the engine has every function and every type of the query, so that a query that the engine cannot run is an error `0A000` before it runs. [`Plan::run`] then computes the row as `ExecInterpExpr` of PostgreSQL does.

#![forbid(unsafe_code)]

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::rc::Rc;
use std::sync::Arc;

use rupg_analyze::{
    Aggref, Body, BoolOp, BoolTest, Case, Column, CurrentOf, Expr, ExprKind, FieldStore,
    FromFunction, FromItem, Func, FuncForm, IsJson, JsonCtor, JsonCtorKind, JsonFormatKind, Query,
    RowCompare, SetKind, SetOp, SetTree, SortGroup, SqlValue, SubLink, SubLinkKind, Subscript,
    TableSample, Target, Var, WindowFunc, expand_grouping_sets,
};
use rupg_catalog::FIRST_NORMAL_OID;
use rupg_common::{Error, Result, SqlState};
use rupg_func::{Call, Kernel, OrderingTest, Session, base_type};
use rupg_pgcatalog::builtin;
use rupg_plan::{HashTable, HashTables};
use rupg_types::{Array, ArrayDim, MAXDIM, NAME_MAX_BYTES, Record, Value, format_type, oid};

/// A query that the engine can run.
#[derive(Debug)]
pub struct Plan {
    query: Query,
    /// The rows of `RETURNING` of each statement of `WITH` that changes data, by the number of the statement, which the references to the statement read.
    written: Arc<Vec<Rows>>,
    /// The tables of a cursor that `WHERE CURRENT OF` can read, from [`Plan::cursor`].
    marks: Vec<u32>,
    /// The query that a run of the plan runs, from [`Plan::fold`]: the query with the simple subqueries and views of `FROM` merged into it, as [`rupg_opt::merge_from`] merges them, or `None` when the run runs [`Plan::query`]. `EXPLAIN` shows [`Plan::query`].
    run: Option<Box<Query>>,
    /// `row_security_user_id`: the current user when a table of the query has row security, whose policies the plan has for that role.
    row_security: Option<u32>,
}

/// The query of a `SELECT` with the simple subqueries and views of `FROM` merged into it, as `pull_up_simple_subquery` merges them, or `None` when no subquery merges. A view with `security_barrier` stays a subquery.
fn merged(query: &Query, session: &dyn Session) -> Option<Box<Query>> {
    if query.write.is_some()
        || query.ctes.iter().any(|c| c.write.is_some())
        || !query.relations.iter().any(|r| r.subquery.is_some())
    {
        return None;
    }
    let barrier = |view: u32| {
        session.catalog().and_then(|c| c.relation(view)).is_some_and(rupg_analyze::security_barrier)
    };
    let mut run = query.clone();
    rupg_opt::merge_from(&mut run, &barrier);
    (run != *query).then(|| Box::new(run))
}

/// The error of a feature that the engine does not have yet.
fn not_yet(what: impl std::fmt::Display) -> Error {
    Error::new(SqlState::FEATURE_NOT_SUPPORTED, format!("{what} is not supported yet"))
}

/// The signature of a function for an error, such as `lower(text)`.
fn signature(func: u32) -> String {
    let Some(proc) = builtin::proc_by_oid(func) else { return format!("function {func}") };
    let args: Vec<_> = proc.argtypes.iter().map(|ty| format_type(*ty)).collect();
    format!("{}({})", proc.name, args.join(", "))
}

/// The kernel of a function, or the error that the engine does not have it.
fn kernel(func: u32, location: Option<usize>) -> Result<Kernel> {
    rupg_func::kernel(func).ok_or_else(|| {
        let error = not_yet(format!("function {}", signature(func)));
        match location {
            Some(at) => error.at(at),
            None => error,
        }
    })
}

/// The set kernel of a function in `FROM`, or the error of a function that the engine does not have.
fn set_kernel(func: u32, location: Option<usize>) -> Result<rupg_func::SetKernel> {
    rupg_func::set_kernel(func).ok_or_else(|| {
        let error = not_yet(format!("function {}", signature(func)));
        match location {
            Some(at) => error.at(at),
            None => error,
        }
    })
}

/// The OID of the language `sql`.
const SQL_LANGUAGE: u32 = 14;

/// The OID of the language `plpgsql`.
const PLPGSQL_LANGUAGE: u32 = 13674;

/// The depth of the calls of functions in SQL in the bodies of functions in SQL that a run takes before it stops with the error of `check_stack_depth`, as a function that calls itself with no end does.
const MAX_SQL_DEPTH: usize = 100;

/// True for a function in SQL that a user makes, for a built-in function with a body in SQL that has no kernel, and for a function in PL/pgSQL. The engine runs its body.
fn runs_body(func: u32) -> bool {
    let user = |p: &builtin::ProcRow| p.oid >= FIRST_NORMAL_OID && p.lang == SQL_LANGUAGE;
    procedural(func)
        || builtin::proc_by_oid(func).is_some_and(|p| p.sqlbody.is_some() || user(p))
            && rupg_func::kernel(func).is_none()
            && rupg_func::set_kernel(func).is_none()
}

/// True for a function of `json_populate_record`, which [`Eval::populate_call`] runs with no kernel.
fn populates(func: u32) -> bool {
    populate::populate_function(func).is_some() || func == SATISFIES_HASH_PARTITION
}

/// `fmgr_security_definer` for a function with settings or with `SECURITY DEFINER`: `f` runs as the owner of a `SECURITY DEFINER` function and with the settings of `proconfig` of the function, which go back after it, also after an error. Another function runs `f` alone.
fn with_settings<T>(session: &dyn Session, func: u32, f: impl FnOnce() -> Result<T>) -> Result<T> {
    if func < FIRST_NORMAL_OID {
        return f();
    }
    let (config, definer) = match session.catalog().and_then(|c| c.function(func)) {
        Some(function) if function.secdef || !function.config.is_empty() => {
            (function.config.clone(), function.secdef.then_some(function.owner))
        }
        _ => return f(),
    };
    session.start_function_call(&config, definer)?;
    let result = f();
    session.end_function_call();
    result
}

/// The number of rows between two checks of `CHECK_FOR_INTERRUPTS` in a loop over the rows of a query.
const INTERRUPT_ROWS: usize = 1024;

/// `CHECK_FOR_INTERRUPTS` in a loop over rows, at the row `n` when it is a multiple of [`INTERRUPT_ROWS`]: the error of a cancel or of the end of `statement_timeout`.
pub(crate) fn interrupt(session: &dyn Session, n: usize) -> Result<()> {
    if n.is_multiple_of(INTERRUPT_ROWS) && n > 0 {
        session.interrupted()?;
    }
    Ok(())
}

/// True for a function in PL/pgSQL, which [`Session::call_function`] runs.
fn procedural(func: u32) -> bool {
    builtin::proc_by_oid(func).is_some_and(|p| p.lang == PLPGSQL_LANGUAGE)
}

/// `ExecMakeTableFunctionResult` for a function that gives one row value for each row, as a function in SQL with `OUT` parameters does with `SELECT (1, 2.1)`: the fields of each value are the columns of the row, and `tupledesc_match` checks that the fields have the types of the columns. A null value gives a row of nulls.
fn row_fields(set: Rows, columns: &[Column]) -> Result<Rows> {
    if columns.len() < 2 {
        return Ok(set);
    }
    let mismatch = |detail: String| {
        Error::new(
            SqlState::DATATYPE_MISMATCH,
            "function return row and query-specified return row do not match",
        )
        .with_detail(detail)
    };
    set.into_iter()
        .map(|row| {
            if row.len() != 1 {
                return Ok(row);
            }
            match row.into_iter().next() {
                Some(Value::Record(record)) => {
                    if record.values.len() != columns.len() {
                        let count = record.values.len();
                        let plural = if count == 1 { "" } else { "s" };
                        return Err(mismatch(format!(
                            "Returned row contains {count} attribute{plural}, but query expects {}.",
                            columns.len()
                        )));
                    }
                    for (i, (&ty, column)) in record.types.iter().zip(columns).enumerate() {
                        if ty != column.ty {
                            return Err(mismatch(format!(
                                "Returned type {} at ordinal position {}, but query expects {}.",
                                format_type(ty),
                                i + 1,
                                format_type(column.ty)
                            )));
                        }
                    }
                    Ok(record.values)
                }
                Some(Value::Null) => Ok(vec![Value::Null; columns.len()]),
                value => Ok(value.into_iter().collect()),
            }
        })
        .collect()
}

/// The types of the columns that the query of a function gives: the columns of `RETURNING` for a query that writes, as `check_sql_stmt_retval` reads them.
fn shown_types(query: &Query) -> Vec<u32> {
    let returning = query.write.as_ref().and_then(|w| w.returning.as_deref());
    let targets = &returning.unwrap_or(query).targets;
    targets.iter().filter(|t| !t.junk).map(|t| t.expr.ty).collect()
}

/// `fmgr_sql` for a function that does not return a set and whose last statement gave the rows `rows` with columns of the types `types`: the first column of the first row, or null. The columns of a function that returns a row make a row value. A function that returns `void` gives null.
fn sql_result(ret: u32, rows: Rows, types: Vec<u32>) -> Value {
    if ret == oid::VOID {
        return Value::Null;
    }
    let Some(mut row) = rows.into_iter().next() else { return Value::Null };
    let composite = builtin::type_by_oid(ret).is_some_and(|t| t.kind == b'c');
    let whole = matches!(row.as_slice(), [Value::Record(_) | Value::Null]);
    if row.len() == 1 && ((ret != oid::RECORD && !composite) || whole) {
        return row.pop().unwrap_or(Value::Null);
    }
    Value::Record(Box::new(Record { types, values: row, names: None }))
}

/// True when a call in `FROM` needs the set kernel of the function: the function gives a set, or a row with `width` columns, or it is a procedure, which gives a row also with one column.
fn table_function(func: u32, width: usize) -> bool {
    width > 1 || builtin::proc_by_oid(func).is_some_and(|p| p.retset || p.kind == b'p')
}

/// True for a function that gives one composite value and has a plain kernel but no set kernel. In `FROM`, the fields of the value are the columns of its one row.
fn row_function(func: u32) -> bool {
    builtin::proc_by_oid(func).is_some_and(|p| !p.retset)
        && rupg_func::set_kernel(func).is_none()
        && rupg_func::kernel(func).is_some()
}

/// `ExecEvalRowNull` and `ExecEvalRowNotNull` for a row, else the plain test: a row is null when all its fields are null, and not null when none of its fields is null.
fn null_test(value: &Value, is_null: bool) -> bool {
    match value {
        Value::Record(record) if is_null => record.values.iter().all(Value::is_null),
        Value::Record(record) => !record.values.iter().any(Value::is_null),
        value => value.is_null() == is_null,
    }
}

/// True when the function gives null for a null argument without a call.
fn strict(func: u32) -> bool {
    builtin::proc_by_oid(func).is_none_or(|p| p.strict)
}

/// The calls of functions that give a set in the targets, by level, as `split_pathtarget_at_srfs` puts them in `ProjectSet` nodes: a call is one level above the highest call in its arguments.
fn set_levels(targets: &[Target]) -> Vec<Vec<&Expr>> {
    fn walk<'a>(expr: &'a Expr, levels: &mut Vec<Vec<&'a Expr>>) -> usize {
        let below = expr.children().into_iter().map(|c| walk(c, levels)).max().unwrap_or(0);
        match &expr.kind {
            ExprKind::Func(f) if f.retset => {
                if levels.len() == below {
                    levels.push(Vec::new());
                }
                levels[below].push(expr);
                below + 1
            }
            _ => below,
        }
    }
    let mut levels = Vec::new();
    for target in targets {
        walk(&target.expr, &mut levels);
    }
    levels
}

/// Checks that the engine can run the function of an operator, as for a call of the function: a function in SQL or in PL/pgSQL runs its body.
fn op_kernel(func: u32, location: Option<usize>) -> Result<()> {
    if runs_body(func) { Ok(()) } else { kernel(func, location).map(|_| ()) }
}

/// Checks that the engine has every function and every type that an expression uses.
fn check(expr: &Expr) -> Result<()> {
    match &expr.kind {
        ExprKind::Const(_)
        | ExprKind::Param(_)
        | ExprKind::CaseTest
        | ExprKind::DomainValue
        | ExprKind::SqlValue(_)
        | ExprKind::Var(_)
        | ExprKind::SubColumn(_) => {}
        ExprKind::SubLink(sub) => {
            if let Some(test) = &sub.test {
                check(test)?;
            }
            check_query(&sub.query, false)?;
        }
        ExprKind::Func(f) if f.retset => {
            if !builtin::is_system_proc(f.oid) && !runs_body(f.oid) && !populates(f.oid) {
                set_kernel(f.oid, expr.location)?;
            }
            f.args.iter().try_for_each(check)?;
        }
        ExprKind::Func(f) => {
            // The engine runs the body of a function in SQL, so such a function needs no kernel.
            if !builtin::is_system_proc(f.oid) && !runs_body(f.oid) && !populates(f.oid) {
                kernel(f.oid, expr.location)?;
            }
            f.args.iter().try_for_each(check)?;
        }
        ExprKind::NamedArg(na) => check(&na.arg)?,
        ExprKind::Relabel(arg, _)
        | ExprKind::Collate(arg, _)
        | ExprKind::CoerceToDomain(arg, _)
        | ExprKind::NullTest(arg, _)
        | ExprKind::BooleanTest(arg, _)
        | ExprKind::ConvertRow { arg, .. }
        | ExprKind::FieldSelect(arg, _) => check(arg)?,
        ExprKind::CoerceViaIo(arg, _) => {
            if !rupg_func::output_supported(arg.ty) {
                return Err(not_yet(format!("output of type {}", format_type(arg.ty))));
            }
            if !rupg_func::input_supported(expr.ty) {
                return Err(not_yet(format!("input of type {}", format_type(expr.ty))));
            }
            check(arg)?;
        }
        ExprKind::ArrayCoerce { arg, element, .. } => {
            check(arg)?;
            check(element)?;
        }
        ExprKind::Subscript(sub) => {
            check(&sub.container)?;
            sub.bounds().chain(&sub.assign).try_for_each(check)?;
        }
        ExprKind::FieldStore(store) => {
            check(&store.arg)?;
            store.fields.iter().try_for_each(|(_, value)| check(value))?;
        }
        ExprKind::Bool(_, args) | ExprKind::Coalesce(args) => args.iter().try_for_each(check)?,
        ExprKind::Case(case) => {
            if let Some(arg) = &case.arg {
                check(arg)?;
            }
            for (when, then) in &case.whens {
                check(when)?;
                check(then)?;
            }
            check(&case.default)?;
        }
        ExprKind::MinMax { less: func, args, .. }
        | ExprKind::NullIf { equal: func, args }
        | ExprKind::Distinct { equal: func, args, .. }
        | ExprKind::ScalarArrayOp { func, args, .. } => {
            op_kernel(*func, expr.location)?;
            args.iter().try_for_each(check)?;
        }
        ExprKind::Array { elements, .. } | ExprKind::Row(elements, _) => {
            elements.iter().try_for_each(check)?;
        }
        ExprKind::JoinRow(row) => row.fields.iter().try_for_each(check)?,
        // The executor calls the comparison function of each pair of fields, not the operator.
        ExprKind::RowCompare(rc) => {
            for (op, &compare) in rc.ops.iter().zip(&rc.compares) {
                op_kernel(compare, expr.location)?;
                op.children().into_iter().try_for_each(check)?;
            }
        }
        ExprKind::Agg(agg) => {
            agg.direct.iter().try_for_each(check)?;
            agg.args.iter().try_for_each(check)?;
            if let Some(filter) = &agg.filter {
                check(filter)?;
            }
        }
        ExprKind::Grouping(g) => g.args.iter().try_for_each(check)?,
        ExprKind::PlaceHolder(p) => check(&p.expr)?,
        ExprKind::JsonValue(_) | ExprKind::JsonCtor(_) | ExprKind::IsJson(_) => {
            expr.children().into_iter().try_for_each(check)?;
        }
        ExprKind::JsonExpr(e) => e.children().into_iter().try_for_each(check)?,
        ExprKind::Xml(x) => x.children().into_iter().try_for_each(check)?,
        ExprKind::JsonTable(t) => t.children().into_iter().try_for_each(check)?,
        ExprKind::CurrentOf(c) => c.args.iter().try_for_each(check)?,
        ExprKind::Window(w) => {
            if w.agg {
                agg::method(&agg::window_aggref(w), expr.ty, expr.location)?;
            } else {
                window::method(w.oid, expr.location)?;
            }
            w.args.iter().try_for_each(check)?;
            if let Some(filter) = &w.filter {
                check(filter)?;
            }
        }
    }
    Ok(())
}

/// Checks the conditions of the joins of a part of `FROM`.
fn check_join(item: &FromItem) -> Result<()> {
    if let FromItem::Join(join) = item {
        if let Some(on) = &join.on {
            check(on)?;
        }
        check_join(&join.left)?;
        check_join(&join.right)?;
    }
    Ok(())
}

/// The detail of the error of a `DISTINCT` or a `GROUP BY` that can neither sort nor use a hash table.
const MIXED_KEYS: &str =
    "Some of the datatypes only support hashing, while others only support sorting.";

/// `preprocess_expression` for a condition that `subquery_push_qual` moves into the subquery `query`: the condition folds and becomes canonical, as the planner of the subquery makes it. The condition stays as it is when the fold fails.
pub fn fold_qual(query: &Query, qual: Expr, session: &dyn Session) -> Expr {
    fold::qual(query, &qual, session).unwrap_or(qual)
}

/// Checks a query and makes the plan to run it. The plan reads the generation expression of each virtual generated column in place of the column, as `expand_virtual_generated_columns` puts it in the query.
///
/// # Errors
///
/// `0A000` for a function or a type that the engine does not have yet.
pub fn prepare(mut query: Query, session: &dyn Session) -> Result<Plan> {
    let row_security = rupg_func::apply_row_security(&mut query, session)?;
    if let Some(catalog) = session.catalog() {
        rupg_analyze::expand_virtual(&mut query, catalog)?;
    }
    rupg_analyze::check_outer_joins(&query)?;
    check_query(&query, query.write.is_none())?;
    check_write(&query)?;
    scan::check_foreign(&query, session)?;
    let perms = std::mem::take(&mut query.perms);
    fill_modified(query.write.as_mut(), &perms);
    for cte in &mut query.ctes {
        fill_modified(cte.query.write.as_mut(), &perms);
    }
    query.perms = perms;
    for cte in query.ctes.iter().filter(|c| c.write.is_some()) {
        rupg_analyze::check_outer_joins(&cte.query)?;
        check_query(&cte.query, false)?;
        check_write(&cte.query)?;
    }
    Ok(Plan { query, written: Arc::default(), marks: Vec::new(), run: None, row_security })
}

/// Puts in [`rupg_analyze::Write::modified`] the columns of the table of the write that the privileges `perms` of the statement give values, as `ExecGetInsertedCols` and `ExecGetUpdatedCols` read them from the entry of the result relation.
fn fill_modified(write: Option<&mut rupg_analyze::Write>, perms: &[rupg_analyze::PermInfo]) {
    let Some(write) = write else { return };
    let mut modified = BTreeSet::new();
    for perm in perms.iter().filter(|p| p.relid == write.table) {
        modified.extend(perm.inserted.iter().chain(&perm.updated).copied());
    }
    write.modified = modified.into_iter().collect();
}

/// Checks the queries of `ON CONFLICT DO UPDATE` and of the clauses of `MERGE` of a write.
fn check_write(query: &Query) -> Result<()> {
    if let Some(update) =
        query.write.as_ref().and_then(|w| w.on_conflict.as_ref()?.update.as_deref())
    {
        check_query(update, false)?;
    }
    for clause in query.write.iter().filter_map(|w| w.merge.as_ref()).flat_map(|m| &m.clauses) {
        check_query(&clause.query, false)?;
    }
    Ok(())
}

/// The numbers of the statements of `WITH` that change data whose rows a query reads, in `FROM`, in a subquery, in `RETURNING`, in `ON CONFLICT DO UPDATE` and in the clauses of `MERGE`.
fn read_writes(query: &Query, out: &mut Vec<usize>) {
    fn relations(query: &Query, out: &mut Vec<usize>) {
        for relation in &query.relations {
            out.extend(relation.cte.as_ref().and_then(|c| c.write));
            if let Some(sub) = &relation.subquery {
                relations(sub, out);
            }
        }
    }
    relations(query, out);
    for (expr, _) in query.all_exprs(0) {
        expr.find(0, &mut |e, _| {
            if let ExprKind::SubLink(sub) = &e.kind {
                relations(&sub.query, out);
            }
            None::<()>
        });
    }
    let Some(write) = &query.write else { return };
    let update = write.on_conflict.as_ref().and_then(|c| c.update.as_deref());
    let clauses = write.merge.iter().flat_map(|m| m.clauses.iter().map(|c| &*c.query));
    for part in write.returning.as_deref().into_iter().chain(update).chain(clauses) {
        read_writes(part, out);
    }
}

/// Checks that the engine can run a query or a subquery. With `output`, it also checks that the engine has the output function of each column.
fn check_query(query: &Query, output: bool) -> Result<()> {
    // `create_ordinary_grouping_paths`: a `GROUP BY` sorts the rows when every type has an order, or else uses a hash table. An aggregate call with `ORDER BY` or `DISTINCT` cannot use a hash table. With grouping sets, each set with a type that has no order needs a hash table, and `preprocess_grouping_sets` gives the error when such a set also has a type with no hash. The planner gives this error before the engine checks its functions.
    let item = |t: &usize| query.group.iter().find(|g| g.target == *t);
    let unsortable = |t: &usize| item(t).is_none_or(|g| g.sort == 0);
    let unhashable = |t: &usize| item(t).is_none_or(|g| !g.hashable);
    let cannot = if query.grouping_sets.is_empty() {
        !query.group.iter().all(|g| g.sort != 0)
            && (!query.group.iter().all(|g| g.hashable) || has_ordered_aggregate(query))
    } else {
        let sets = expand_grouping_sets(&query.grouping_sets, query.group_distinct, usize::MAX)
            .unwrap_or_default();
        let unsortable_sets = sets.iter().filter(|s| s.iter().any(unsortable));
        unsortable_sets.clone().any(|s| s.iter().any(unhashable))
            || (has_ordered_aggregate(query) && unsortable_sets.count() > 0)
    };
    if cannot {
        return Err(Error::new(SqlState::FEATURE_NOT_SUPPORTED, "could not implement GROUP BY")
            .with_detail(MIXED_KEYS));
    }
    // `create_distinct_paths` does the same for `DISTINCT`, after the groups.
    if !query.distinct.iter().all(|g| g.sort != 0) && !query.distinct.iter().all(|g| g.hashable) {
        return Err(Error::new(SqlState::FEATURE_NOT_SUPPORTED, "could not implement DISTINCT")
            .with_detail(MIXED_KEYS));
    }
    if query.group.iter().any(|g| query.targets[g.target].expr.first_set_call().is_some()) {
        return Err(not_yet("a set-returning function in GROUP BY"));
    }
    window::check(query)?;
    for target in &query.targets {
        if output && !rupg_func::output_supported(target.expr.ty) {
            return Err(not_yet(format!("output of type {}", format_type(target.expr.ty))));
        }
        check(&target.expr)?;
    }
    if let Some(filter) = &query.filter {
        check(filter)?;
    }
    if let Some(having) = &query.having {
        check(having)?;
    }
    agg::check(query)?;
    for item in &query.from {
        check_join(item)?;
    }
    for relation in &query.relations {
        if let Some(sub) = &relation.subquery {
            check_query(sub, false)?;
        }
        relation.values.iter().flatten().flatten().try_for_each(check)?;
        if let Some(sample) = &relation.tablesample {
            sample.args.iter().chain(&sample.repeatable).try_for_each(check)?;
        }
        for (call, width) in relation.function.iter().flat_map(|f| &f.calls) {
            match &call.kind {
                ExprKind::Func(f) if table_function(f.oid, *width) => {
                    if !builtin::is_system_proc(f.oid)
                        && !runs_body(f.oid)
                        && !populates(f.oid)
                        && !row_function(f.oid)
                    {
                        set_kernel(f.oid, call.location)?;
                    }
                    f.args.iter().try_for_each(check)?;
                }
                _ => check(call)?,
            }
        }
    }
    for expr in query.offset.iter().chain(&query.limit) {
        check(expr)?;
    }
    for group in &query.sort {
        kernel(group.sort, None)?;
        if query.with_ties {
            kernel(group.equal, None)?;
        }
    }
    for group in &query.distinct {
        kernel(group.equal, None)?;
        if group.sort != 0 {
            kernel(group.sort, None)?;
        }
    }
    if let Some(op) = &query.set_op {
        check_set_op(op)?;
    }
    Ok(())
}

/// True when the query has an aggregate call with `ORDER BY` or `DISTINCT`, which `preprocess_aggrefs` counts in `numOrderedAggs`.
fn has_ordered_aggregate(query: &Query) -> bool {
    let mut ordered = |e: &Expr, depth: usize| match &e.kind {
        ExprKind::Agg(agg)
            if agg.levels_up == depth && (!agg.order.is_empty() || !agg.distinct.is_empty()) =>
        {
            Some(())
        }
        _ => None,
    };
    query
        .targets
        .iter()
        .map(|t| &t.expr)
        .chain(&query.having)
        .any(|e| e.find(0, &mut ordered).is_some())
}

/// Checks the operators of each operation of a set operation. As for `DISTINCT`, the planner sorts the rows or uses a hash table, and gives an error when it can do neither.
fn check_set_op(op: &SetOp) -> Result<()> {
    // `generate_recursion_path`: a recursive `UNION` finds the equal rows with a hash table only.
    if op.work.is_some() {
        if !op.groups.iter().all(|g| g.hashable) {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "could not implement recursive UNION",
            )
            .with_detail("All column datatypes must be hashable."));
        }
    } else if !op.groups.iter().all(|g| g.sort != 0) && !op.groups.iter().all(|g| g.hashable) {
        let name = match op.kind {
            SetKind::Union => "UNION",
            SetKind::Intersect => "INTERSECT",
            SetKind::Except => "EXCEPT",
        };
        return Err(Error::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            format!("could not implement {name}"),
        )
        .with_detail(MIXED_KEYS));
    }
    for group in &op.groups {
        kernel(group.equal, None)?;
        if group.sort != 0 {
            kernel(group.sort, None)?;
        }
    }
    for part in [&op.left, &op.right] {
        if let SetTree::Op(op) = part {
            check_set_op(op)?;
        }
    }
    Ok(())
}

/// The rows of the `SELECT` of a set operation with this index.
type Leaf<'a> = dyn FnMut(usize) -> Result<Rows> + 'a;

/// The rows of a part of a set operation. A `SELECT` gives the rows that `leaf` gives for it. `tables` has the hash tables of the plan of the query, and `path` has the sides from the top operation down to the part.
fn part_rows(
    part: &SetTree,
    leaf: &mut Leaf<'_>,
    session: &dyn Session,
    tables: Option<&HashTables>,
    path: &[bool],
) -> Result<Rows> {
    match part {
        SetTree::Leaf(index) => leaf(*index),
        SetTree::Op(op) => set_rows(op, leaf, session, tables, path),
    }
}

/// The path of a side of the operation at `path`.
fn side_path(path: &[bool], right: bool) -> Vec<bool> {
    let mut side = path.to_vec();
    side.push(right);
    side
}

/// The rows of the members of a `UNION` without `ALL` that hashes its rows, which `recurse_union_children` puts in one `Append`: the rows of each `SELECT`, and of each `UNION` in it with the same types of columns.
fn union_members(
    top: &SetOp,
    part: &SetTree,
    leaf: &mut Leaf<'_>,
    session: &dyn Session,
    tables: Option<&HashTables>,
    path: &[bool],
    rows: &mut Rows,
) -> Result<()> {
    match part {
        SetTree::Op(op)
            if op.kind == SetKind::Union
                && op.work.is_none()
                && op.columns.iter().map(|c| c.0).eq(top.columns.iter().map(|c| c.0)) =>
        {
            for (right, side) in [(false, &op.left), (true, &op.right)] {
                union_members(top, side, leaf, session, tables, &side_path(path, right), rows)?;
            }
        }
        _ => rows.extend(part_rows(part, leaf, session, tables, path)?),
    }
    Ok(())
}

/// The rows of a set operation with the hash table `table`, in the order of the table, as `HashAggregate` gives them for `UNION` and `SetOp` gives them for `INTERSECT` and `EXCEPT`. The rows of one side go into the table, and the rows of the other side only find their groups in it. `None` when a type of a column has no hash function.
fn hashed_set_rows(
    op: &SetOp,
    table: &HashTable,
    left: Rows,
    right: Rows,
    session: &dyn Session,
) -> Result<Option<Rows>> {
    let types: Vec<u32> = op.columns.iter().map(|(ty, _)| *ty).collect();
    let keys = sort::Keys::new(&op.groups, &types, session)?;
    let hash = |row: &[Value]| hashed::hash(table.keys.iter().map(|&k| (types[k], &row[k])));
    let (first, second) = if table.right { (right, left) } else { (left, right) };
    if first.iter().chain(&second).next().is_some_and(|row| hash(row).is_err()) {
        return Ok(None);
    }
    let mut hashes = hashed::Table::new(hashed::buckets(table.groups));
    // Each group is its first row with the number of copies in the first side and in the second side.
    let mut groups: Vec<(usize, usize, usize)> = Vec::new();
    for (i, row) in first.iter().enumerate() {
        let (entry, old) =
            hashes.insert(hash(row)?, &mut |e| keys.equal(&first[groups[e as usize].0], row))?;
        if old {
            groups[entry as usize].1 += 1;
        } else {
            groups.push((i, 1, 0));
        }
    }
    for row in &second {
        let found =
            hashes.find(hash(row)?, &mut |e| keys.equal(&first[groups[e as usize].0], row))?;
        if let Some(entry) = found {
            groups[entry as usize].2 += 1;
        }
    }
    let mut rows = Vec::new();
    for entry in hashes.order() {
        let (row, a, b) = groups[entry as usize];
        let (left, right) = if table.right { (b, a) } else { (a, b) };
        for _ in 0..copies(op, left, right) {
            rows.push(first[row].clone());
        }
    }
    Ok(Some(rows))
}

/// The number of copies of a row of a set operation with `left` copies in the left side and `right` copies in the right side.
fn copies(op: &SetOp, left: usize, right: usize) -> usize {
    match (op.kind, op.all) {
        (SetKind::Union, _) => 1,
        (SetKind::Intersect, true) => left.min(right),
        (SetKind::Intersect, false) => usize::from(left > 0 && right > 0),
        (SetKind::Except, true) => left.saturating_sub(right),
        (SetKind::Except, false) => usize::from(left > 0 && right == 0),
    }
}

/// The rows of `rows` that are not equal to a row of `seen` or to a row before them, as the hash table of `RecursiveUnion` keeps them out. The rows that stay go into `seen` too.
fn new_rows(keys: &sort::Keys<'_>, seen: &mut Rows, rows: Rows) -> Result<Rows> {
    let mut fresh = Vec::new();
    for row in rows {
        let mut found = false;
        for old in seen.iter() {
            if keys.equal(old, &row)? {
                found = true;
                break;
            }
        }
        if !found {
            seen.push(row.clone());
            fresh.push(row);
        }
    }
    Ok(fresh)
}

/// The rows of a run of `RecursiveUnion` for the recursive query of `WITH` whose work table is `work`. The left part gives the first rows, and they go into the work table. Then the right part runs again with the rows of the work table, and its new rows take their place, until it gives no new rows. `UNION` with no `ALL` keeps out each row that is equal to a row before it.
fn recursive_rows(
    query: &Query,
    op: &SetOp,
    work: usize,
    tables: &scan::Tables,
    run: &Run<'_, '_>,
    want: Option<usize>,
) -> Result<Rows> {
    let types: Vec<u32> = op.columns.iter().map(|(ty, _)| *ty).collect();
    let keys = sort::Keys::new(&op.groups, &types, run.session)?;
    let mut seen = Vec::new();
    let mut leaf = |i: usize| Ok((*tables.rows[i]).clone());
    let mut step = part_rows(&op.left, &mut leaf, run.session, None, &[])?;
    if !op.all {
        step = new_rows(&keys, &mut seen, step)?;
    }
    let mut frames = run.outer.to_vec();
    frames.push(Frame { tables, tuple: &[], group: None });
    let mut rows = Vec::new();
    while !step.is_empty() {
        run.session.interrupted()?;
        rows.extend(step.iter().cloned());
        if want.is_some_and(|want| rows.len() >= want) {
            break;
        }
        run.cache.work.borrow_mut().push((work, Rc::new(step)));
        let mut leaf = |i: usize| {
            let Some(sub) = &query.relations[i].subquery else {
                return Err(Error::internal("a SELECT of a set operation with no query"));
            };
            // `RecursiveUnion` stops to read the step when it has enough rows. With `UNION ALL` and one `SELECT` in the recursive term, the step only needs the rows that are not there yet, and a recursive query in the step stops after them.
            if op.all
                && let SetTree::Leaf(_) = op.right
                && let Some(want) = want
            {
                run.cache.want.set(Some(want - rows.len()));
            }
            run_query(sub, run.params, run.session, run.cache, &frames, false)
        };
        let next = part_rows(&op.right, &mut leaf, run.session, None, &[]);
        run.cache.work.borrow_mut().pop();
        step = next?;
        if !op.all {
            step = new_rows(&keys, &mut seen, step)?;
        }
    }
    Ok(rows)
}

/// The parts of a run that the subqueries share.
struct Run<'r, 'f> {
    params: &'r [Value],
    session: &'r dyn Session,
    cache: &'r Cache,
    outer: &'r [Frame<'f>],
}

/// The rows of an operation of a set operation, as `Append`, `Unique` and `SetOp` give them. `UNION ALL` gives the rows of the left part, then the rows of the right part. The other operations make groups of equal rows. When the plan of the operation hashes the rows, the groups come in the order of the hash table. Else they sort the rows when every type has an order, or else keep the groups in the order of the rows. For each group, `UNION` gives one row, `INTERSECT` gives one row when both parts have the row, and `EXCEPT` gives one row when only the left part has it. With `ALL`, `INTERSECT` gives the row as many times as the part with fewer copies has it, and `EXCEPT` gives it as many times as the left part has more copies than the right part.
fn set_rows(
    op: &SetOp,
    leaf: &mut Leaf<'_>,
    session: &dyn Session,
    tables: Option<&HashTables>,
    path: &[bool],
) -> Result<Rows> {
    let table = tables
        .and_then(|t| t.set_ops.iter().find(|(p, _)| p == path))
        .map(|(_, table)| table)
        .filter(|_| !(op.kind == SetKind::Union && op.all));
    let (left, right) = if op.kind == SetKind::Union && table.is_some() {
        let mut rows = Vec::new();
        for (right, side) in [(false, &op.left), (true, &op.right)] {
            union_members(op, side, leaf, session, tables, &side_path(path, right), &mut rows)?;
        }
        (rows, Vec::new())
    } else {
        let left = part_rows(&op.left, leaf, session, tables, &side_path(path, false))?;
        let right = part_rows(&op.right, leaf, session, tables, &side_path(path, true))?;
        (left, right)
    };
    if op.kind == SetKind::Union && op.all {
        let mut rows = left;
        rows.extend(right);
        return Ok(rows);
    }
    if let Some(table) = table
        && let Some(rows) = hashed_set_rows(op, table, left.clone(), right.clone(), session)?
    {
        return Ok(rows);
    }
    let types: Vec<u32> = op.columns.iter().map(|(ty, _)| *ty).collect();
    let keys = sort::Keys::new(&op.groups, &types, session)?;
    let mut tagged: Vec<(Vec<Value>, bool)> =
        left.into_iter().map(|r| (r, false)).chain(right.into_iter().map(|r| (r, true))).collect();
    // Each group is a row with the number of copies in the left part and in the right part.
    let mut groups: Vec<(Vec<Value>, usize, usize)> = Vec::new();
    let sorted = op.groups.iter().all(|g| g.sort != 0);
    if sorted {
        sort::qsort(&mut tagged, &mut |a, b| keys.compare(&a.0, &b.0))?;
    }
    for (row, right) in tagged {
        let found = if sorted {
            match groups.last() {
                Some(last) if keys.equal(&last.0, &row)? => Some(groups.len() - 1),
                _ => None,
            }
        } else {
            let mut found = None;
            for (i, group) in groups.iter().enumerate() {
                if keys.equal(&group.0, &row)? {
                    found = Some(i);
                    break;
                }
            }
            found
        };
        let group = match found {
            Some(i) => &mut groups[i],
            None => {
                groups.push((row, 0, 0));
                groups.last_mut().ok_or_else(|| Error::internal("no group"))?
            }
        };
        if right {
            group.2 += 1;
        } else {
            group.1 += 1;
        }
    }
    let mut rows = Vec::new();
    for (row, left, right) in groups {
        for _ in 0..copies(op, left, right) {
            rows.push(row.clone());
        }
    }
    Ok(rows)
}

/// The rows of a query.
type Rows = Vec<Vec<Value>>;

/// The constraints of a domain and of the domains below it, as `DomainConstraintCache` keeps them.
#[derive(Debug, Default)]
struct DomainChecks {
    /// A domain of the stack has a not-null constraint.
    not_null: bool,
    /// The name and the expression of each check constraint.
    checks: Vec<(String, Expr)>,
}

type Checks = Rc<DomainChecks>;

/// The domains of a domain and of the domains below it, from the top. A type that is not a domain gives no domains.
fn domain_stack(domain: u32) -> impl Iterator<Item = u32> {
    std::iter::successors(Some(domain), |&ty| builtin::type_by_oid(ty).map(|row| row.base))
        .take_while(|&ty| builtin::type_by_oid(ty).is_some_and(|row| row.kind == b'd'))
}

/// True when a domain or a domain below it has a constraint, so that the cast to the domain must run its checks.
pub(crate) fn has_domain_constraints(domain: u32) -> bool {
    domain_stack(domain)
        .filter_map(builtin::domain_by_oid)
        .any(|row| row.not_null || !row.checks.is_empty())
}

/// The body of each function in SQL, by the OID of the function, the types of the arguments of the call and the types of the column definition list of a call in `FROM`, or `None` for a function of the catalog that is not in SQL.
type Bodies = Rc<RefCell<Vec<(BodyKey, Option<Rc<Body>>)>>>;

/// The kernel of a function, true when the function is strict, and the test on the order of the arguments when the kernel is a comparison, as [`Cache::plain_kernel`] finds them.
type PlainKernel = (Kernel, bool, Option<OrderingTest>);

/// The OID of a function, the types of the arguments of a call and the type and the type modifier of each column of its column definition list.
type BodyKey = (u32, Vec<u32>, Vec<(u32, i32)>);

/// The state of a run that the query and its subqueries share.
#[derive(Default)]
struct Cache {
    /// The rows of the relations of each query, by the address of the query, with true when the rows are only those that pass the conditions of `WHERE`, which [`scan::read`] can keep. A subquery reads them once, also when it runs for each row of the query outside it.
    tables: RefCell<Vec<(*const Query, bool, Rc<scan::Tables>)>>,
    /// The rows of each subquery that reads no column of an outer query, as an `InitPlan` keeps them. It runs once, when the query first needs it.
    rows: RefCell<Vec<(*const Query, Rc<Rows>)>>,
    /// The hash table of each such subquery of `ANY`, as a hashed `SubPlan` keeps it, or `None` when a value of the subquery has no key.
    hashed: RefCell<Vec<(*const Query, Kept)>>,
    /// The rows of each query of `WITH`, by its name and its query, as the tuplestore of its `CteScan` nodes keeps them. The query runs once, when a reference first needs it, so that the references read the same values of its volatile functions.
    ctes: RefCell<Vec<KeptCte>>,
    /// The name and the expression of each check constraint of a domain, by the OID of the domain, as the type cache keeps them.
    domains: RefCell<Vec<(u32, Checks)>>,
    /// The body of each function in SQL that the run calls. The run of a body shares them.
    bodies: Bodies,
    /// The rows of the work table of each recursive query of `WITH` that runs now, by the number of its work table. The last entry with a number is the one that the reference reads.
    work: RefCell<Vec<(usize, Rc<Rows>)>>,
    /// The rows of the statements of `WITH` that change data, as [`Plan`] keeps them.
    written: Arc<Vec<Rows>>,
    /// The flags `error` and `empty` of each `JsonExpr`, by the address of the node, as its `JsonExprState` keeps them from one evaluation to the next.
    json_flags: RefCell<Vec<(*const rupg_analyze::JsonExpr, jsonexpr::Flags)>>,
    /// The number of rows that the next run must give, when the query that reads it needs no more: a recursive query of `WITH`, as the `ExecProcNode` calls of a `Limit` above a `CteScan` read them, or a subquery that gives one row, which stops at the second row. The run takes it.
    want: Cell<Option<usize>>,
    /// The number of bodies of functions in SQL that run around this run.
    depth: usize,
    /// The query whose rows go to the client as the run makes them, as `PortalRunSelect` sends them, from [`Plan::run_select`].
    top: Cell<Option<*const Query>>,
    /// The rows of the query `top` that the run made before an error.
    sent: RefCell<Rows>,
    /// The kernel and the `proisstrict` of each function that [`Cache::plain_kernel`] found, by the OID of the function, or `None` for a function that needs the full path of [`Eval::func`].
    kernels: RefCell<Vec<(u32, Option<PlainKernel>)>>,
}

impl Cache {
    /// The state of a run of the body of a function in SQL, which shares the bodies of this run. The body gets a cache of its own, so that a subquery of the body that reads an argument runs again for each call.
    fn nested(&self) -> Result<Cache> {
        if self.depth >= MAX_SQL_DEPTH {
            return Err(Error::new(SqlState::STATEMENT_TOO_COMPLEX, "stack depth limit exceeded")
                .with_hint("Increase the configuration parameter \"max_stack_depth\" (currently 2048kB), after ensuring the platform's stack depth limit is adequate."));
        }
        Ok(Cache { bodies: Rc::clone(&self.bodies), depth: self.depth + 1, ..Cache::default() })
    }

    /// The kernel of the built-in function `func` and true when it is strict, when a call of it runs only the kernel: it has no defaults, it is not in PL/pgSQL, it has no body and no row in the catalog of the session, and [`Eval::func`] does not treat it in a special way. The lookups run once for each run, and not for each row.
    fn plain_kernel(&self, func: u32, session: &dyn Session) -> Option<PlainKernel> {
        if let Some((_, found)) = self.kernels.borrow().iter().find(|(f, _)| *f == func) {
            return *found;
        }
        let found = (|| {
            if func >= FIRST_NORMAL_OID
                || populates(func)
                || func == PG_COLUMN_SIZE
                || session.catalog().is_some_and(|c| c.function(func).is_some())
                || runs_body(func)
            {
                return None;
            }
            let proc = builtin::proc_by_oid(func)?;
            if proc.nargdefaults != 0 || proc.lang == PLPGSQL_LANGUAGE {
                return None;
            }
            let kernel = rupg_func::kernel(func)?;
            Some((kernel, proc.strict, rupg_func::ordering_test(kernel)))
        })();
        self.kernels.borrow_mut().push((func, found));
        found
    }

    /// `load_domaintype_info`: the constraints of a domain and of the domains below it. The checks of a domain below come first, and the checks of one domain come in the order of their names.
    fn domain_checks(&self, domain: u32) -> Result<Checks> {
        if let Some((_, checks)) = self.domains.borrow().iter().find(|(d, _)| *d == domain) {
            return Ok(Rc::clone(checks));
        }
        let mut all = DomainChecks::default();
        for row in domain_stack(domain).filter_map(builtin::domain_by_oid) {
            all.not_null |= row.not_null;
            let mut checks = Vec::new();
            for (name, text) in &row.checks {
                checks.push((name.clone(), rupg_analyze::node::read(text)?));
            }
            checks.sort_by(|a, b| a.0.cmp(&b.0));
            checks.append(&mut all.checks);
            all.checks = checks;
        }
        let checks = Rc::new(all);
        self.domains.borrow_mut().push((domain, Rc::clone(&checks)));
        Ok(checks)
    }

    /// The body of a function in SQL of the catalog of the session, or of a built-in function that [`runs_body`], for a call with arguments of the types `inputs`, which the analyzer reads once for each run, or `None` for another function.
    fn body(&self, func: u32, inputs: &[u32], session: &dyn Session) -> Result<Option<Rc<Body>>> {
        self.body_for(func, inputs, &[], session)
    }

    /// [`Cache::body`] for a call in `FROM` with the column definition list `columns`, or with none when `columns` is empty.
    fn body_for(
        &self,
        func: u32,
        inputs: &[u32],
        columns: &[(u32, i32)],
        session: &dyn Session,
    ) -> Result<Option<Rc<Body>>> {
        if session.catalog().and_then(|c| c.function(func)).is_none() && !runs_body(func) {
            return Ok(None);
        }
        let kept = self
            .bodies
            .borrow()
            .iter()
            .find(|((f, t, c), _)| *f == func && t == inputs && c == columns)
            .map(|(_, body)| body.clone());
        if let Some(body) = kept {
            return Ok(body);
        }
        let body = if columns.is_empty() {
            rupg_func::function_body(func, inputs, session)?
        } else {
            rupg_func::function_body_for(func, inputs, columns, session)?
        };
        let body = body.map(Rc::new);
        self.bodies.borrow_mut().push(((func, inputs.to_vec(), columns.to_vec()), body.clone()));
        Ok(body)
    }

    /// The rows of the relations of a query. With `narrow`, the rows can be only those that pass the conditions of `WHERE`, for a run of the query that tests them again. `EXPLAIN ANALYZE` counts the rows that the conditions remove, so it reads all the rows. The cache does not keep the rows of a key that the parameters `params` gave, because another run of the query can have other parameters.
    fn tables(
        &self,
        query: &Query,
        session: &dyn Session,
        narrow: bool,
        params: &[Value],
    ) -> Result<Rc<scan::Tables>> {
        if let Some((_, _, tables)) =
            self.tables.borrow().iter().find(|(q, n, _)| std::ptr::eq(*q, query) && (narrow || !n))
        {
            return Ok(Rc::clone(tables));
        }
        let mut test = |tables: &scan::Tables, expr: &Expr, tuple: &[usize]| {
            Eval::new(&[], session, tables, self, &[]).at(tuple).eval(expr)
        };
        let mut keyed = false;
        let tables = Rc::new(scan::read(
            query,
            session,
            narrow.then_some(&mut test as &mut scan::Narrow<'_>),
            (params, &mut keyed),
        )?);
        if !keyed {
            self.tables.borrow_mut().push((query, narrow, Rc::clone(&tables)));
        }
        Ok(tables)
    }

    /// The rows of a reference to a query of `WITH`, from [`Self::rows`] for the first reference. Each copy of the query in a reference has the same expressions, so a reference finds the rows of another reference by its name and its query.
    fn cte_rows(
        &self,
        name: &str,
        query: &Query,
        run: impl FnOnce() -> Result<Rc<Rows>>,
    ) -> Result<Rc<Rows>> {
        let kept = self
            .ctes
            .borrow()
            .iter()
            .find(|(n, q, _)| n == name && **q == *query)
            .map(|(_, _, rows)| Rc::clone(rows));
        if let Some(rows) = kept {
            return Ok(rows);
        }
        let rows = run()?;
        if !correlated(query) && free_work(query).is_empty() {
            self.ctes.borrow_mut().push((
                name.to_string(),
                Rc::new(query.clone()),
                Rc::clone(&rows),
            ));
        }
        Ok(rows)
    }

    /// The rows of a subquery: the rows that the cache keeps for a subquery that reads no column of an outer query, or else the rows of a new run.
    fn rows(&self, query: &Query, run: impl FnOnce() -> Result<Rows>) -> Result<Rc<Rows>> {
        let kept = self
            .rows
            .borrow()
            .iter()
            .find(|(q, _)| std::ptr::eq(*q, query))
            .map(|(_, rows)| Rc::clone(rows));
        if let Some(rows) = kept {
            return Ok(rows);
        }
        let rows = Rc::new(run()?);
        if !correlated(query) && free_work(query).is_empty() {
            self.rows.borrow_mut().push((query, Rc::clone(&rows)));
        }
        Ok(rows)
    }
}

/// The name of a query of `WITH`, its query and its rows, as [`Cache`] keeps them.
type KeptCte = (String, Rc<Query>, Rc<Rows>);

/// The hash table of a subquery of `ANY`, or `None` when a value of the subquery has no key.
type Kept = Option<Rc<Hashed>>;

/// The keys of the rows of a subquery of `ANY`, for the columns that the test compares.
struct Hashed {
    keys: BTreeSet<Vec<scan::Key>>,
    /// True when a row has a null in a compared column, so that a value that matches no key can give null.
    nulls: bool,
}

/// The pairs of a test of `ANY` that a hash table can do: the test is an `=` of a value and a column of the subquery, or an `AND` of such tests, as a row comparison gives. Each pair is the value and the column number.
fn hash_pairs(test: &Expr) -> Option<Vec<(&Expr, usize)>> {
    fn column(e: &Expr) -> Option<usize> {
        match &e.kind {
            ExprKind::SubColumn(n) => Some(*n),
            ExprKind::Relabel(arg, _) => column(arg),
            _ => None,
        }
    }
    fn walk<'e>(e: &'e Expr, pairs: &mut Vec<(&'e Expr, usize)>) -> Option<()> {
        match &e.kind {
            ExprKind::Bool(BoolOp::And, args) => args.iter().try_for_each(|a| walk(a, pairs)),
            ExprKind::Func(f) if scan::HASH_EQUALS.contains(&f.oid) && f.args.len() == 2 => {
                let (value, n) = match (column(&f.args[0]), column(&f.args[1])) {
                    (None, Some(n)) => (&f.args[0], n),
                    (Some(n), None) => (&f.args[1], n),
                    _ => return None,
                };
                let inner = value
                    .find(0, &mut |e, _| matches!(e.kind, ExprKind::SubColumn(_)).then_some(()));
                inner.is_none().then(|| pairs.push((value, n)))
            }
            _ => None,
        }
    }
    let mut pairs = Vec::new();
    walk(test, &mut pairs)?;
    Some(pairs)
}

/// The key of a value of a subquery: `Ok(None)` for a null, and an error when the value has no key.
fn hash_key(value: Value) -> std::result::Result<Option<scan::Key>, ()> {
    if value.is_null() {
        return Ok(None);
    }
    scan::key(value).map(Some).ok_or(())
}

/// The hash table of the rows of a subquery for the columns of the pairs, or `None` when a value has no key.
fn hash_rows(rows: &Rows, pairs: &[(&Expr, usize)]) -> Option<Hashed> {
    let mut hashed = Hashed { keys: BTreeSet::new(), nulls: false };
    'row: for row in rows.iter() {
        let mut keys = Vec::with_capacity(pairs.len());
        for &(_, n) in pairs {
            match hash_key(row.get(n)?.clone()).ok()? {
                Some(k) => keys.push(k),
                None => {
                    hashed.nulls = true;
                    continue 'row;
                }
            }
        }
        hashed.keys.insert(keys);
    }
    Some(hashed)
}

/// The row of a query outside a subquery, which the columns of an outer query in the subquery read.
#[derive(Clone, Copy)]
struct Frame<'a> {
    tables: &'a scan::Tables,
    tuple: &'a [usize],
    /// The group of the query, which an aggregate or a `GROUPING` of the query in a subquery reads.
    group: Option<&'a Group>,
}

/// The expressions that the queries outside a subquery make null.
fn inherited(outer: &[Frame<'_>]) -> Vec<*const Expr> {
    outer.iter().rev().find_map(|f| f.group).map(|g| g.nulled.clone()).unwrap_or_default()
}

/// True when a subquery reads a column of a query outside it, so that it must run again for each row of that query.
pub(crate) fn correlated(query: &Query) -> bool {
    query.all_exprs(0).iter().any(|(e, depth)| {
        e.find(*depth, &mut |e, depth| match e.kind {
            ExprKind::Var(var) if var.levels_up > depth => Some(()),
            _ => None,
        })
        .is_some()
    })
}

/// The work tables that a query reads in `FROM` but whose recursive query is not in it. The rows of such a query change at each step of the recursive query.
fn free_work(query: &Query) -> Vec<usize> {
    fn walk(query: &Query, read: &mut Vec<usize>, made: &mut Vec<usize>) {
        if let Some(op) = &query.set_op {
            ops(op, made);
        }
        for relation in &query.relations {
            if let Some(work) = relation.cte.as_ref().and_then(|c| c.work) {
                read.push(work);
            }
            if let Some(sub) = &relation.subquery {
                walk(sub, read, made);
            }
        }
    }
    fn ops(op: &SetOp, made: &mut Vec<usize>) {
        made.extend(op.work);
        for part in [&op.left, &op.right] {
            if let SetTree::Op(op) = part {
                ops(op, made);
            }
        }
    }
    let (mut read, mut made) = (Vec::new(), Vec::new());
    walk(query, &mut read, &mut made);
    read.retain(|w| !made.contains(w));
    read
}

/// `contain_volatile_functions`: true when the expression calls a volatile function, also in a subquery.
pub(crate) fn volatile(expr: &Expr) -> bool {
    let volatile = |func: u32| builtin::proc_by_oid(func).is_some_and(|p| p.volatile == b'v');
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

/// The one-time filter of the `Result` node that `create_gating_plan` puts over the scan: each part of `WHERE` that `AND` joins and that reads no column of the query and calls no volatile function runs once, before the scan. The result is false when such a part is not true, so the query reads no rows.
fn gate(query: &Query, eval: &mut Eval<'_>) -> Result<bool> {
    fn parts<'a>(expr: &'a Expr, out: &mut Vec<&'a Expr>) {
        match &expr.kind {
            ExprKind::Bool(BoolOp::And, args) => args.iter().for_each(|a| parts(a, out)),
            _ => out.push(expr),
        }
    }
    let mut all = Vec::new();
    if let Some(filter) = &query.filter {
        parts(filter, &mut all);
    }
    for expr in all {
        if expr.first_var().is_none() && !volatile(expr) && eval.eval(expr)? != Value::Bool(true) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The rows of the relations of a query with the rows of its subqueries and its functions in `FROM`, as `SubqueryScan` and `FunctionScan` read them. A subquery that reads no column of an outer query runs once. A subquery or a function that reads the relations before it runs in the scan.
fn with_subqueries(
    query: &Query,
    base: &Rc<scan::Tables>,
    params: &[Value],
    session: &dyn Session,
    cache: &Cache,
    outer: &[Frame<'_>],
    want: Option<usize>,
) -> Result<Rc<scan::Tables>> {
    let work = |r: &rupg_analyze::Relation| r.cte.as_ref().and_then(|c| c.work);
    let write = |r: &rupg_analyze::Relation| r.cte.as_ref().and_then(|c| c.write);
    fn transition(r: &rupg_analyze::Relation) -> Option<&rupg_analyze::CteRef> {
        r.cte.as_ref().filter(|c| c.transition)
    }
    if query.relations.iter().all(|r| {
        r.subquery.is_none()
            && r.function.is_none()
            && r.values.is_none()
            && r.tablesample.is_none()
            && work(r).is_none()
            && write(r).is_none()
            && transition(r).is_none()
    }) {
        return Ok(Rc::clone(base));
    }
    // The `SELECT`s of the recursive term of a recursive query run at each step, with the rows of the work table.
    let mut recursive = Vec::new();
    if let Some(op) = query.set_op.as_ref().filter(|op| op.work.is_some()) {
        op.right.each_leaf(&mut |i| recursive.push(i));
    }
    let mut tables = (**base).clone();
    let mut frames = outer.to_vec();
    frames.push(Frame { tables: base, tuple: &[], group: None });
    for (i, relation) in query.relations.iter().enumerate() {
        if let Some(id) = work(relation) {
            let rows =
                cache.work.borrow().iter().rev().find(|(w, _)| *w == id).map(|(_, r)| Rc::clone(r));
            tables.rows[i] = rows.ok_or_else(|| Error::internal("a work table with no rows"))?;
        }
        if let Some(id) = write(relation) {
            let Some(rows) = cache.written.get(id) else {
                return Err(not_yet("a statement that changes data in WITH in this place"));
            };
            tables.rows[i] = Rc::new(rows.clone());
        }
        if let Some(cte) = transition(relation) {
            tables.rows[i] = session.transition_rows(&cte.name).ok_or_else(|| {
                Error::internal(format!("the transition table \"{}\" has no rows", cte.name))
            })?;
        }
        if let Some(sub) = &relation.subquery
            && tables.needs[i].is_empty()
            && !recursive.contains(&i)
        {
            let recursive = sub.set_op.as_ref().is_some_and(|op| op.work.is_some());
            tables.rows[i] = match want.filter(|_| recursive) {
                // The rows of a part of the query do not go into the cache, because another reader of the query can need all of them.
                Some(want) => {
                    cache.want.set(Some(want));
                    Rc::new(run_query(sub, params, session, cache, &frames, false)?)
                }
                None => {
                    let run = || {
                        cache.rows(sub, || run_query(sub, params, session, cache, &frames, false))
                    };
                    // `inline_cte` puts a query of `WITH` into each reference only when it has no volatile function, so each reference can read the rows of the first one.
                    match &relation.cte {
                        Some(cte) => cache.cte_rows(&cte.name, sub, run)?,
                        None => run()?,
                    }
                }
            };
        }
        if let Some(function) = &relation.function
            && tables.needs[i].is_empty()
        {
            let mut eval = Eval::new(params, session, base, cache, outer);
            tables.rows[i] = Rc::new(eval.function_rows(function, &relation.columns)?);
        }
        if let Some(rows) = &relation.values
            && tables.needs[i].is_empty()
        {
            let mut eval = Eval::new(params, session, base, cache, outer);
            tables.rows[i] = Rc::new(eval.values_rows(rows)?);
        }
        if let Some(sample) = &relation.tablesample {
            let mut eval = Eval::new(params, session, base, cache, outer);
            let rows = &tables.rows[i];
            let kept = eval.sample(sample, rows.len())?;
            let kept: Rows = kept.into_iter().map(|row| rows[row].clone()).collect();
            tables.rows[i] = Rc::new(kept);
        }
    }
    Ok(Rc::new(tables))
}

/// True when an expression is a constant for `eval_const_expressions`: constants, parameters and immutable functions of them.
fn constant(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Const(_) | ExprKind::Param(_) => true,
        ExprKind::Relabel(arg, _) | ExprKind::Collate(arg, _) => constant(arg),
        ExprKind::Func(f) => {
            builtin::proc_by_oid(f.oid).is_some_and(|p| p.volatile == b'i')
                && f.args.iter().all(constant)
        }
        _ => false,
    }
}

/// True when the rows of a query are the first rows of its one relation, in their order, so that a `LIMIT` of the query needs only that many rows of the relation.
fn reads_all(query: &Query) -> bool {
    query.relations.len() == 1
        && query.filter.is_none()
        && !query.grouped
        && query.group.is_empty()
        && query.having.is_none()
        && query.sort.is_empty()
        && query.distinct.is_empty()
        && query.windows.is_empty()
        && query.set_op.is_none()
        && query.row_marks.is_empty()
        && query.write.is_none()
        && !query.has_target_srfs()
        && agg::calls(query).is_empty()
        && window::calls(query).is_empty()
}

/// Runs a query or a subquery and gives its rows, with no junk columns. `outer` has the rows of the queries outside a subquery, with the query just outside it last. With `exists`, the query runs as `simplify_EXISTS_query` makes it when it can, and gives one empty row when it has a row.
fn run_query(
    query: &Query,
    params: &[Value],
    session: &dyn Session,
    cache: &Cache,
    outer: &[Frame<'_>],
    exists: bool,
) -> Result<Vec<Vec<Value>>> {
    let want = cache.want.take();
    let filled = cache.tables.borrow().iter().any(|(q, _, _)| std::ptr::eq(*q, query));
    if !exists
        && outer.is_empty()
        && !filled
        && let Some(rows) = columnar::run(query, params, session, cache, want)?
    {
        return Ok(rows);
    }
    let base = cache.tables(query, session, true, params)?;
    let mut eval = Eval::new(params, session, &base, cache, outer);
    // simplify_EXISTS_query: with no aggregate, no `HAVING`, no `OFFSET` and a `LIMIT` that is a constant more than 0 or null, `EXISTS` only needs a row of `FROM` and `WHERE`.
    let mut simple = false;
    if exists
        && query.set_op.is_none()
        && agg::calls(query).is_empty()
        && window::calls(query).is_empty()
        && query.having.is_none()
        && query.offset.is_none()
        && !query.has_target_srfs()
    {
        simple = match &query.limit {
            None => true,
            Some(limit) if constant(limit) => match eval.eval(limit)? {
                Value::Int8(n) => n > 0,
                _ => true,
            },
            Some(_) => false,
        };
    }
    let (offset, count) = if simple { (0, None) } else { eval.limits(query)? };
    if count == Some(0) {
        return Ok(Vec::new());
    }
    // `is_degenerate_grouping`: with `HAVING` and no aggregate and no `GROUP BY`, the query gives one row or no row, and the plan does not read `FROM`.
    let degenerate = query.grouped
        && query.having.is_some()
        && query.group.is_empty()
        && query.grouping_sets.is_empty()
        && query.set_op.is_none()
        && agg::calls(query).is_empty();
    let open = !degenerate && gate(query, &mut eval)?;
    let mut tables = if open {
        // The rows that the caller reads limit the rows of the query, as the `LIMIT` of the query does.
        let limit = match (count, want) {
            (Some(count), Some(want)) => Some(count.min(want)),
            (count, want) => count.or(want),
        };
        let limit = limit.filter(|_| reads_all(query)).map(|limit| limit.saturating_add(offset));
        with_subqueries(query, &base, params, session, cache, outer, limit)?
    } else {
        Rc::clone(&base)
    };
    // The rows of a set operation take the place of the rows of the leftmost `SELECT`, which the targets read.
    let mut set_tuples = Vec::new();
    if let Some(op) = &query.set_op
        && open
    {
        let rows = match op.work {
            Some(work) => {
                let run = Run { params, session, cache, outer };
                recursive_rows(query, op, work, &tables, &run, want)?
            }
            None => {
                let hashes = session.hash_tables(query);
                let leaf = &mut |i: usize| Ok((*tables.rows[i]).clone());
                set_rows(op, leaf, session, hashes.as_deref(), &[])?
            }
        };
        let leftmost = op.left.leftmost();
        set_tuples = (0..rows.len())
            .map(|row| {
                let mut tuple = vec![scan::NONE; query.relations.len()];
                tuple[leftmost] = row;
                tuple
            })
            .collect();
        let mut set = (*tables).clone();
        set.rows[leftmost] = Rc::new(rows);
        tables = Rc::new(set);
    }
    let tables = &*tables;
    let mut eval = Eval { tables, ..eval.at(&[]) };
    let tuples = if query.set_op.is_some() {
        set_tuples
    } else if open {
        // `ExecReScanFunctionScan`: the rows of each call of a function in `FROM` that reads no relation of the query, which a new scan reads again.
        let mut kept: Vec<Vec<Option<Rows>>> = vec![Vec::new(); query.relations.len()];
        let mut source = |index: usize, tuple: &[usize]| {
            let relation = &query.relations[index];
            if let Some(function) = &relation.function {
                let mut eval = Eval::new(params, session, tables, cache, outer);
                eval.tuple = tuple;
                return eval.function_rows_kept(
                    function,
                    &relation.columns,
                    Some(&mut kept[index]),
                );
            }
            if let Some(rows) = &relation.values {
                let mut eval = Eval::new(params, session, tables, cache, outer);
                eval.tuple = tuple;
                return eval.values_rows(rows);
            }
            let Some(sub) = &relation.subquery else { return Ok(Vec::new()) };
            let mut frames = outer.to_vec();
            frames.push(Frame { tables, tuple, group: None });
            run_query(sub, params, session, cache, &frames, false)
        };
        // A join and an index scan give their rows in the order of the plan, which a query with no `ORDER BY` shows.
        let planned = session.join_order(query);
        // `CHECK_FOR_INTERRUPTS` of the scans and the joins, after each 1024 tests.
        let mut tests = 0_usize;
        let test: &mut scan::Test<'_> = &mut |expr, tuple| {
            tests = tests.wrapping_add(1);
            if tests.is_multiple_of(INTERRUPT_ROWS) {
                session.interrupted()?;
            }
            eval.at(tuple).eval(expr)
        };
        let planned = planned.as_deref().map(|p| (p, session));
        scan::tuples(query, tables, test, &mut source, planned)?
    } else {
        Vec::new()
    };
    if simple {
        return Ok(if tuples.is_empty() { Vec::new() } else { vec![Vec::new()] });
    }
    let none = vec![scan::NONE; query.relations.len()];
    let rows = if query.grouped {
        // With no sort and no window, `Limit` stops after the groups that it gives.
        let needed = match count {
            Some(count)
                if query.sort.is_empty()
                    && query.distinct.is_empty()
                    && query.windows.is_empty() =>
            {
                offset.saturating_add(count)
            }
            _ => usize::MAX,
        };
        agg::rows(&mut eval, query, &tuples, &none, needed)?
    } else {
        // With no sort, `Limit` stops after the rows that it gives.
        let ordered = !query.sort.is_empty() || !query.distinct.is_empty();
        let needed = match count {
            Some(count) if !ordered => offset.saturating_add(count),
            _ => usize::MAX,
        };
        // A caller that reads no more rows stops the run, as `ExecProcNode` stops to read.
        let needed = match want {
            Some(want) if !ordered => needed.min(offset.saturating_add(want)),
            _ => needed,
        };
        let levels = set_levels(&query.targets);
        // The window function calls run before the targets, and the rows come in the order of the last sort of the windows.
        let calls = window::calls(query);
        let (sequence, mut values) = if calls.is_empty() {
            ((0..tuples.len()).collect(), Vec::new())
        } else {
            let refs: Vec<&[usize]> = tuples.iter().map(Vec::as_slice).collect();
            window::compute(
                &eval,
                query,
                &calls,
                window::Input { tuples: &refs, groups: &[] },
                session,
            )?
        };
        let mut rows = Vec::with_capacity(tuples.len().min(needed));
        for (n, i) in sequence.into_iter().enumerate() {
            if rows.len() >= needed {
                break;
            }
            interrupt(session, n)?;
            eval.tuple = &tuples[i];
            if let Some(values) = values.get_mut(i) {
                eval.windows = std::mem::take(values);
            }
            if let Err(error) = eval.project(&query.targets, &levels, &mut rows, needed) {
                // With no sort, each row went to the client before the target of the next row failed.
                if !ordered && cache.top.get() == Some(&raw const *query) {
                    let width = query.targets.iter().take_while(|t| !t.junk).count();
                    let sent = rows.into_iter().skip(offset).take(count.unwrap_or(usize::MAX));
                    *cache.sent.borrow_mut() = sent
                        .map(|mut row| {
                            row.truncate(width);
                            row
                        })
                        .collect();
                }
                return Err(error);
            }
        }
        rows
    };
    let types: Vec<u32> = query.targets.iter().map(|t| t.expr.ty).collect();
    let rows = order(query, rows, &types, offset, count, session)?;
    let width = query.targets.iter().take_while(|t| !t.junk).count();
    Ok(rows
        .into_iter()
        .map(|mut row| {
            row.truncate(width);
            row
        })
        .collect())
}

/// True when `expr` is the parameter `$n`, or the length cast of `$n` to the type modifier of its own type, for [`Plan::passes_params`].
fn passed_param(expr: &Expr, n: usize) -> bool {
    match &expr.kind {
        ExprKind::Param(at) => *at == n,
        ExprKind::Func(f) if f.form == FuncForm::ImplicitCast && expr.typmod >= 0 => {
            matches!(f.args.as_slice(), [arg, typmod, ..]
                if arg.kind == ExprKind::Param(n)
                    && arg.ty == expr.ty
                    && typmod.kind == ExprKind::Const(Value::Int4(expr.typmod)))
        }
        _ => false,
    }
}

impl Plan {
    /// The query of the plan after `prepare` and `fold`, which `EXPLAIN` shows.
    pub fn query(&self) -> &Query {
        &self.query
    }

    /// The columns of the result. The junk columns of `ORDER BY` do not count. A query that writes has the columns of its `RETURNING`, or none.
    pub fn columns(&self) -> &[Target] {
        if let Some(write) = &self.query.write {
            return write.returning.as_ref().map_or(&[], |query| &query.targets);
        }
        let n = self.query.targets.iter().take_while(|t| !t.junk).count();
        &self.query.targets[..n - 2 * self.marks.len()]
    }

    /// `ExecProcessReturning`: the rows of `RETURNING` for the rows that the statement wrote. Each row has the columns of the table, then the values that the query of the statement carries for `RETURNING`. A query with no `RETURNING` gives no rows.
    ///
    /// # Errors
    ///
    /// The error of a function of the list, such as a division by zero.
    pub fn returning(
        &self,
        rows: Vec<Vec<Value>>,
        params: &[Value],
        session: &dyn Session,
    ) -> Result<Vec<Vec<Value>>> {
        let Some(write) = self.query.write.as_ref() else { return Ok(Vec::new()) };
        let Some(query) = write.returning.as_deref() else { return Ok(Vec::new()) };
        // A row that the statement inserted or that an `UPDATE` made has the transaction of the session as `xmin`.
        let writer = match write.kind {
            rupg_analyze::WriteKind::Delete => None,
            _ => session.transaction_id(false).and_then(|xid| u32::try_from(xid).ok()),
        };
        let tables = scan::given(query, vec![rows]).written_by(writer);
        let cache = self.cache();
        cache.tables.borrow_mut().push((query, false, Rc::new(tables)));
        run_query(query, params, session, &cache, &[], false)
    }

    /// `ExecWithCheckOptions` for `WCO_VIEW_CHECK`: each row that the statement wrote, which starts with the columns of the table, must pass the check of each view of [`rupg_analyze::Write::view_check`], the inner view first. A null fails the check.
    ///
    /// # Errors
    ///
    /// `44000` for a row that fails a check, and the error of a function of a check.
    pub fn view_check(
        &self,
        rows: &[Vec<Value>],
        params: &[Value],
        session: &dyn Session,
    ) -> Result<()> {
        let Some(info) = self.query.write.as_ref() else { return Ok(()) };
        let Some(check) = info.view_check.as_ref() else { return Ok(()) };
        let rows: Vec<Vec<Value>> =
            rows.iter().map(|row| row[..info.width.min(row.len())].to_vec()).collect();
        let query: &Query = &check.query;
        let cache = self.cache();
        cache.tables.borrow_mut().push((
            query,
            false,
            Rc::new(scan::given(query, vec![rows.clone()])),
        ));
        let results = run_query(query, params, session, &cache, &[], false)?;
        for (row, result) in rows.iter().zip(results) {
            let Some(at) = result.iter().position(|v| !matches!(v, Value::Bool(true))) else {
                continue;
            };
            let types: Vec<u32> = session
                .catalog()
                .and_then(|c| c.relation(info.table))
                .map(row_types)
                .unwrap_or_default();
            return Err(Error::new(
                SqlState::WITH_CHECK_OPTION_VIOLATION,
                format!("new row violates check option for view \"{}\"", check.views[at]),
            )
            .with_detail_opt(failing_row(
                row,
                &types,
                (info.table, &info.modified),
                session,
            )?));
        }
        Ok(())
    }

    /// `ExecOnConflictUpdate`: the new row of `ON CONFLICT DO UPDATE` for the row `existing` of the table, which ends with the ID of its version, and the row `excluded` that the statement proposed, after the checks of the constraints. The new row has the columns of the table, then the values that `RETURNING` carries. It is `None` when the condition of `WHERE` is not true.
    ///
    /// # Errors
    ///
    /// The error of a function of `SET` or `WHERE`, and the error of a constraint of the table.
    pub fn conflict_update(
        &self,
        existing: Vec<Value>,
        excluded: Vec<Value>,
        params: &[Value],
        session: &dyn Session,
    ) -> Result<Option<Vec<Value>>> {
        let Some(info) = self.query.write.as_ref() else {
            return Ok(None);
        };
        let Some(query) = info.on_conflict.as_ref().and_then(|c| c.update.as_deref()) else {
            return Ok(None);
        };
        // The query of the checks of the constraints reads the query of the new row as a subquery.
        let mut base = query;
        while let [relation] = base.relations.as_slice()
            && let Some(inner) = relation.subquery.as_deref()
        {
            base = inner;
        }
        let old = existing[..info.width.min(existing.len())].to_vec();
        let cache = self.cache();
        let tables = scan::given(base, vec![vec![existing], vec![excluded]]);
        cache.tables.borrow_mut().push((base, false, Rc::new(tables)));
        let rows = run_query(query, params, session, &cache, &[], false)?;
        // `ExecOnConflictUpdate` checks the row that the table has after the condition of `WHERE`.
        let conflict = info.on_conflict.as_ref();
        if !rows.is_empty()
            && let Some(check) = conflict.and_then(|c| c.conflict_check.as_ref())
            && let Some(Some(error)) = policy_failures(check, &info.name, vec![old], session)?.pop()
        {
            return Err(error);
        }
        // With triggers `BEFORE ... FOR EACH ROW`, the policies check the new row after the triggers.
        let policy = if insert::checks_now(info) {
            self.policy_failures(&rows, rupg_analyze::WriteKind::Update, session)?
        } else {
            Vec::new()
        };
        Ok(insert::check(info, rows, policy, session)?.into_iter().next())
    }

    /// `ExecMergeMatched` and `ExecMergeNotMatched` for one clause of a `MERGE`: the row that the clause gives for the row `target` of the table, which ends with the ID of its version, and the carried values of the row of the join. It is `None` when the condition of the clause is not true. The new row of an `INSERT` or an `UPDATE` has the columns of the table, after the checks of the constraints. With [`rupg_analyze::Write::recheck`] it has the conditions of the checks after the columns, because the triggers `BEFORE ... FOR EACH ROW` come before the checks.
    ///
    /// # Errors
    ///
    /// The error of a function of the clause, and the error of a constraint of the table.
    pub fn merge_clause(
        &self,
        clause: &rupg_analyze::MergeClause,
        target: Vec<Value>,
        carried: Vec<Value>,
        params: &[Value],
        session: &dyn Session,
    ) -> Result<Option<Vec<Value>>> {
        let Some(info) = self.query.write.as_ref() else {
            return Ok(None);
        };
        let query = &*clause.query;
        // The query of the checks of the constraints reads the query of the new row as a subquery.
        let mut base = query;
        while let [relation] = base.relations.as_slice()
            && let Some(inner) = relation.subquery.as_deref()
        {
            base = inner;
        }
        let old = target[..info.width.min(target.len())].to_vec();
        let cache = self.cache();
        let tables = scan::given(base, vec![vec![target], vec![carried]]);
        cache.tables.borrow_mut().push((base, false, Rc::new(tables)));
        let rows = run_query(query, params, session, &cache, &[], false)?;
        // `ExecMergeMatched` checks the row of the target table after the condition of the clause and before the triggers.
        let merge = info.merge.as_ref();
        let target_check = match clause.command {
            rupg_analyze::MergeCommand::Update => merge.and_then(|m| m.update_target.as_ref()),
            rupg_analyze::MergeCommand::Delete => merge.and_then(|m| m.delete_target.as_ref()),
            _ => None,
        };
        if !rows.is_empty()
            && let Some(check) = target_check
            && let Some(Some(error)) = policy_failures(check, &info.name, vec![old], session)?.pop()
        {
            return Err(error);
        }
        let kind = match clause.command {
            rupg_analyze::MergeCommand::Insert => Some(rupg_analyze::WriteKind::Insert),
            rupg_analyze::MergeCommand::Update => Some(rupg_analyze::WriteKind::Update),
            _ => None,
        };
        let rows = match kind {
            Some(kind) if info.recheck.is_none() => {
                let policy = self.policy_failures(&rows, kind, session)?;
                insert::check_rows(info, kind, rows, policy, session)?
            }
            _ => rows,
        };
        Ok(rows.into_iter().next())
    }

    /// The cache of a run, with the rows of the statements of `WITH` that change data.
    fn cache(&self) -> Cache {
        Cache { written: Arc::clone(&self.written), ..Cache::default() }
    }

    /// [`analyze`] of the tree `tree` of the plan, with the rows of the statements of `WITH` that changed data, from [`Plan::with_written`].
    ///
    /// # Errors
    ///
    /// The error of a function, such as a division by zero, and the error of a node that the run does not support yet.
    pub fn analyze_tree(
        &self,
        tree: &rupg_plan::Plan<'_>,
        params: &[Value],
        session: &dyn Session,
    ) -> Result<rupg_plan::Actuals> {
        analyze::analyze_in(tree, params, session, true, &self.cache())
    }

    /// `hasModifyingCTE`: true when the query has a statement of `WITH` that changes data.
    pub fn modifying_cte(&self) -> bool {
        self.query.ctes.iter().any(|c| c.write.is_some())
    }

    /// The statements of `WITH` that change data, in the order of their numbers, each with true when the statement or another statement of `WITH` reads its rows. The rows of each statement must go to [`Plan::with_written`] before a run of a plan that reads them.
    pub fn cte_writes(&self) -> Vec<(Plan, bool)> {
        let mut read = Vec::new();
        read_writes(&self.query, &mut read);
        let mut writes: Vec<(usize, &Query)> =
            self.query.ctes.iter().filter_map(|c| c.write.map(|n| (n, &*c.query))).collect();
        writes.sort_by_key(|(n, _)| *n);
        for (_, query) in &writes {
            read_writes(query, &mut read);
        }
        writes
            .into_iter()
            .map(|(n, query)| {
                // The statement checks the privileges of all its parts before the first write.
                let query = Query { perms: self.query.perms.clone(), ..query.clone() };
                let plan = Plan {
                    query,
                    written: Arc::clone(&self.written),
                    marks: Vec::new(),
                    run: None,
                    row_security: self.row_security,
                };
                (plan, read.contains(&n))
            })
            .collect()
    }

    /// True when the main query is an `INSERT` that reads the rows of its source one by one as it writes them, so that a statement of `WITH` that the source reads ends after the main query wrote its rows: the source is a scan with no join, no sort, no groups and no limit, as the `CteScan` under the `ModifyTable` reads the tuplestore of the statement.
    #[must_use]
    pub fn inserts_in_turn(&self) -> bool {
        fn in_turn(query: &Query) -> bool {
            if query.grouped
                || !query.sort.is_empty()
                || !query.distinct.is_empty()
                || !query.windows.is_empty()
                || query.set_op.is_some()
                || query.limit.is_some()
                || query.offset.is_some()
            {
                return false;
            }
            match query.from.as_slice() {
                [FromItem::Relation(at)] => query
                    .relations
                    .get(*at)
                    .is_some_and(|r| r.subquery.as_deref().is_none_or(in_turn)),
                _ => false,
            }
        }
        self.query.write.as_ref().is_some_and(|w| w.kind == rupg_analyze::WriteKind::Insert)
            && in_turn(&self.query)
    }

    /// The plan with the rows of `RETURNING` of the statements of `WITH` that change data, by the number of each statement.
    #[must_use]
    pub fn with_written(self, written: Vec<Vec<Vec<Value>>>) -> Plan {
        Plan { written: Arc::new(written), ..self }
    }

    /// The table that the query writes when it is an `INSERT`, an `UPDATE` or a `DELETE`. Then [`Plan::run`] gives the rows that change.
    pub fn write(&self) -> Option<&rupg_analyze::Write> {
        self.query.write.as_ref()
    }

    /// The arguments of a `CALL` for the output arguments of the procedure.
    pub fn out_args(&self) -> &[rupg_analyze::OutArg] {
        &self.query.out_args
    }

    /// The warnings of the analysis of the query.
    pub fn notices(&self) -> &[Error] {
        &self.query.notices
    }

    /// `dependsOnRole`: true when a table of the query has row security, so that the policies of the plan are the policies of the role that made it.
    pub fn row_security(&self) -> bool {
        self.row_security.is_some()
    }

    /// The types of the parameters.
    pub fn params(&self) -> &[u32] {
        &self.query.params
    }

    /// The locking clause of the first row mark of the query, such as `FOR UPDATE`, which `DECLARE CURSOR` checks, or `None` for a query with no row mark of its own.
    pub fn lock_clause(&self) -> Option<&'static str> {
        self.query.row_marks.first().map(|mark| rupg_analyze::strength_name(mark.strength))
    }

    /// Folds the constant parts of the expressions as the planner does, and gives the plan that runs the folded expressions. `params` has the values of the parameters of `Bind`, which the fold takes as constants as a custom plan does.
    ///
    /// # Errors
    ///
    /// The error of an immutable function of constants, such as a division by zero.
    pub fn fold(&self, params: Option<&[Value]>, session: &dyn Session) -> Result<Plan> {
        let mut query = fold::query(&self.query, params, session)?;
        for cte in query.ctes.iter_mut().filter(|c| c.write.is_some()) {
            *cte.query = fold::query(&cte.query, params, session)?;
        }
        let run = merged(&query, session);
        Ok(Plan {
            query,
            written: Arc::clone(&self.written),
            marks: self.marks.clone(),
            run,
            row_security: self.row_security,
        })
    }

    /// The query that a run of the plan runs.
    fn runnable(&self) -> &Query {
        self.run.as_deref().unwrap_or(&self.query)
    }

    /// `ExecSupportsBackwardScan` on the plan that PostgreSQL makes for the query, which decides if a cursor with neither `SCROLL` nor `NO SCROLL` can scan backward.
    pub fn backward_scan(&self) -> bool {
        backward(&self.query)
    }

    /// The checks of `ExecutorStart`, which come before `RowDescription`: a read-only transaction cannot write or lock a row, and a sequence has no rows to lock.
    ///
    /// # Errors
    ///
    /// `25006` for `INSERT`, `UPDATE`, `DELETE`, `FOR UPDATE` or `FOR SHARE` in a read-only transaction, and `42809` for a lock on the rows of a sequence.
    pub fn start(&self, session: &dyn Session) -> Result<()> {
        let writes = self.query.write.is_some() || self.modifying_cte();
        if writes && session.setting("transaction_read_only").as_deref() == Some("on") {
            let name = self.query.write.as_ref().map_or("SELECT", |w| w.kind.name());
            return Err(Error::new(
                SqlState::READ_ONLY_SQL_TRANSACTION,
                format!("cannot execute {name} in a read-only transaction"),
            ));
        }
        if self.row_security.is_some_and(|user| user != rupg_func::current_user_oid(session)) {
            return Err(not_yet("a plan of a query on a table with row security for another role"));
        }
        rupg_func::check_permissions(&self.query.perms, session)?;
        rupg_func::check_execute(&self.query.funcs, session)?;
        check_row_marks(&self.query, session)?;
        scan::check_scannable(&self.query, session)
    }

    /// The checks of `ExecutorStart` with `EXEC_FLAG_EXPLAIN_ONLY`: `ExecCheckPermissions` and the checks of `EXECUTE` of `ExecInitFunc`, with no check of a read-only transaction or of a relation that has no data.
    ///
    /// # Errors
    ///
    /// `42501` for a relation or a function that the role cannot use.
    pub fn start_explain(&self, session: &dyn Session) -> Result<()> {
        rupg_func::check_permissions(&self.query.perms, session)?;
        rupg_func::check_execute(&self.query.funcs, session)
    }

    /// The plan with no checks of the privileges of the statement, for a query that the server makes for its own work, such as the checks of a foreign key and the insert into a partition.
    #[must_use]
    pub fn unchecked(mut self) -> Plan {
        self.query.perms.clear();
        self
    }

    /// Runs the query and gives its rows. For an `INSERT`, the rows are the new rows of the table after the checks of its constraints. For an `UPDATE`, each new row ends with the ID of the version of the old row, and for a `DELETE` each row has only that ID.
    ///
    /// # Errors
    ///
    /// The error of a function, such as a division by zero, and the error of a constraint of the table.
    pub fn run(&self, params: &[Value], session: &dyn Session) -> Result<Vec<Vec<Value>>> {
        let rows = self.run_unchecked(params, session)?;
        self.check(rows, session)
    }

    /// `PortalRunSelect`: [`Plan::run`] for a query that sends its rows to the client. PostgreSQL sends each row of a query with no sort and no aggregate when it makes it, so when a target of a row fails, the rows before that row are already at the client. The error gives these rows with the error. A query with a sort, an aggregate or a write makes no row before the error, so its rows are empty.
    ///
    /// # Errors
    ///
    /// The rows before the error and the error of a function, such as a division by zero.
    pub fn run_select(
        &self,
        params: &[Value],
        session: &dyn Session,
    ) -> std::result::Result<Rows, (Rows, Error)> {
        let cache = self.cache();
        if self.query.write.is_none() && !self.modifying_cte() {
            cache.top.set(Some(&raw const *self.runnable()));
        }
        self.start(session)
            .and_then(|()| run_query(self.runnable(), params, session, &cache, &[], false))
            .and_then(|rows| self.check(rows, session))
            .map_err(|error| (cache.sent.take(), error))
    }

    /// [`Plan::run`] with no checks of the constraints of the table. The rows of an `INSERT` have the values of the conditions of the check constraints after the columns of the table. `COPY ... FROM` drops the rows that its condition `WHERE` rejects before [`Plan::check`], as `CopyFrom` does.
    ///
    /// # Errors
    ///
    /// The error of a function, such as a division by zero.
    pub fn run_unchecked(
        &self,
        params: &[Value],
        session: &dyn Session,
    ) -> Result<Vec<Vec<Value>>> {
        self.start(session)?;
        self.run_started(params, session)
    }

    /// [`Plan::run_unchecked`] with no checks of [`Plan::start`], for a plan that the caller started before. `COPY ... FROM` runs its plan once for each row, and `ExecutorStart` checks the plan only once for the statement.
    ///
    /// # Errors
    ///
    /// The error of a function, such as a division by zero.
    pub fn run_started(&self, params: &[Value], session: &dyn Session) -> Result<Vec<Vec<Value>>> {
        run_query(self.runnable(), params, session, &self.cache(), &[], false)
    }

    /// True when a run of the plan gives its parameters as its one row: an `INSERT` of one row of the parameters `$1` to `$n` in the order of the columns of the table, with no check constraint, no rule, no view, no `ON CONFLICT` and no `RETURNING`. A parameter can have the length cast of the type modifier of its column, as the input function of `COPY FROM` applied the type modifier already. `COPY FROM` then takes the values of each row as its new row and does not run the plan.
    pub fn passes_params(&self) -> bool {
        let query = self.runnable();
        let Some(info) = &query.write else { return false };
        info.kind == rupg_analyze::WriteKind::Insert
            && info.checks.is_empty()
            && info.returning.is_none()
            && info.on_conflict.is_none()
            && info.recheck.is_none()
            && !info.instead
            && info.views == 0
            && query.relations.is_empty()
            && query.from.is_empty()
            && query.filter.is_none()
            && query.ctes.is_empty()
            && query.targets.len() == info.width
            && query.targets.iter().zip(1..).all(|(t, n)| passed_param(&t.expr, n))
    }

    /// The new row of an `INSERT` or an `UPDATE` after the triggers `BEFORE ... FOR EACH ROW` changed it: the columns `row`, then the conditions of the check constraints, from [`rupg_analyze::Write::recheck`]. A write with no such query gives the row as it is.
    ///
    /// # Errors
    ///
    /// The error of a function of a check constraint.
    pub fn recheck(&self, row: Vec<Value>, session: &dyn Session) -> Result<Vec<Value>> {
        let Some(query) = self.query.write.as_ref().and_then(|w| w.recheck.as_deref()) else {
            return Ok(row);
        };
        let cache = self.cache();
        cache.tables.borrow_mut().push((
            query,
            false,
            Rc::new(scan::given(query, vec![vec![row]])),
        ));
        let rows = run_query(query, &[], session, &cache, &[], false)?;
        rows.into_iter().next().ok_or_else(|| Error::internal("a check of a row with no row"))
    }

    /// The checks of `ExecConstraints` on new rows that have the conditions of the check constraints after the columns, as [`Plan::recheck`] gives them. It gives the rows with no conditions.
    ///
    /// # Errors
    ///
    /// The error of a not-null constraint or of a check constraint.
    pub fn check_rows(
        &self,
        rows: Vec<Vec<Value>>,
        session: &dyn Session,
    ) -> Result<Vec<Vec<Value>>> {
        match &self.query.write {
            Some(info) => {
                let policy = self.policy_failures(&rows, info.kind, session)?;
                insert::check_rows(info, info.kind, rows, policy, session)
            }
            None => Ok(rows),
        }
    }

    /// [`Plan::check_rows`] for the rows of the write `kind`, which is `WriteKind::Insert` or `WriteKind::Update` for a row of `ON CONFLICT DO UPDATE` or of an action of a `MERGE`.
    ///
    /// # Errors
    ///
    /// The error of a not-null constraint, of a check constraint or of the partition constraint of a partition.
    pub fn check_rows_as(
        &self,
        rows: Vec<Vec<Value>>,
        kind: rupg_analyze::WriteKind,
        session: &dyn Session,
    ) -> Result<Vec<Vec<Value>>> {
        match &self.query.write {
            Some(info) => {
                let policy = self.policy_failures(&rows, kind, session)?;
                insert::check_rows(info, kind, rows, policy, session)
            }
            None => Ok(rows),
        }
    }

    /// The checks of `ExecConstraints` on the rows of [`Plan::run_unchecked`]. It gives the rows with only the columns of the table.
    ///
    /// # Errors
    ///
    /// The error of a not-null constraint or of a check constraint.
    pub fn check(&self, rows: Vec<Vec<Value>>, session: &dyn Session) -> Result<Vec<Vec<Value>>> {
        match &self.query.write {
            Some(info) if insert::checks_now(info) => {
                let policy = self.policy_failures(&rows, info.kind, session)?;
                insert::check(info, rows, policy, session)
            }
            Some(_) | None => Ok(rows),
        }
    }

    /// [`policy_failures`] for the new rows of the write `kind`. A partitioned table checks each row in its leaf partition, after the routing.
    ///
    /// # Errors
    ///
    /// The error of a function of a check.
    fn policy_failures(
        &self,
        rows: &[Vec<Value>],
        kind: rupg_analyze::WriteKind,
        session: &dyn Session,
    ) -> Result<Vec<Option<Error>>> {
        let Some(info) = self.query.write.as_ref() else { return Ok(Vec::new()) };
        let Some(check) = info.policy_for(kind) else { return Ok(Vec::new()) };
        let rel = session.catalog().and_then(|c| c.relation(info.table));
        if rel.is_some_and(|r| r.kind == rupg_catalog::RelKind::Partitioned) {
            return Ok(Vec::new());
        }
        let rows: Vec<Vec<Value>> =
            rows.iter().map(|row| row[..info.width.min(row.len())].to_vec()).collect();
        policy_failures(check, &info.name, rows, session)
    }
}

/// `ExecWithCheckOptions` for the checks of row-level security: for each new row `rows` of the table `table`, with the columns of the table, the error of the first check of `check` that the row does not pass, or `None`. A null fails the check.
///
/// # Errors
///
/// The error of a function of a check.
pub fn policy_failures(
    check: &rupg_analyze::PolicyCheck,
    table: &str,
    rows: Vec<Vec<Value>>,
    session: &dyn Session,
) -> Result<Vec<Option<Error>>> {
    let query: &Query = &check.query;
    let cache = Cache::default();
    cache.tables.borrow_mut().push((query, false, Rc::new(scan::given(query, vec![rows]))));
    let results = run_query(query, &[], session, &cache, &[], false)?;
    Ok(results
        .iter()
        .map(|result| {
            let at = result.iter().position(|v| !matches!(v, Value::Bool(true)))?;
            let (row, using) = match check.kind {
                rupg_analyze::PolicyKind::New => ("new", ""),
                rupg_analyze::PolicyKind::Conflict => ("new", " (USING expression)"),
                rupg_analyze::PolicyKind::Target => ("target", " (USING expression)"),
            };
            let message = match check.policies.get(at).cloned().flatten() {
                Some(policy) => format!(
                    "{row} row violates row-level security policy \"{policy}\"{using} for table \"{table}\""
                ),
                None => format!(
                    "{row} row violates row-level security policy{using} for table \"{table}\""
                ),
            };
            Some(Error::new(SqlState::INSUFFICIENT_PRIVILEGE, message))
        })
        .collect())
}

/// `TriggerEnabled` for the condition of `WHEN` of a trigger: true when the condition `qual` is true for the old row `old` and the new row `new`. The relations of the query `base` are the old row and the new row, as [`rupg_analyze::trigger_when`] gives them.
///
/// # Errors
///
/// The error of a function of the condition.
pub fn trigger_when(
    base: &Query,
    qual: &Expr,
    old: Vec<Value>,
    new: Vec<Value>,
    session: &dyn Session,
) -> Result<bool> {
    let query = Query {
        from: vec![FromItem::Relation(0), FromItem::Relation(1)],
        targets: vec![Target {
            name: String::new(),
            expr: qual.clone(),
            origin: None,
            junk: false,
        }],
        ..base.clone()
    };
    let cache = Cache::default();
    cache.tables.borrow_mut().push((
        &query,
        false,
        Rc::new(scan::given(&query, vec![vec![old], vec![new]])),
    ));
    let rows = run_query(&query, &[], session, &cache, &[], false)?;
    Ok(matches!(rows.first().and_then(|row| row.first()), Some(Value::Bool(true))))
}

/// The checks of `ExecCheckXactReadOnly` and `CheckValidRowMarkRel` for the tables that `FOR UPDATE` or `FOR SHARE` locks: a read-only transaction cannot lock a row, and a sequence or a materialized view has no rows to lock.
fn check_row_marks(query: &Query, session: &dyn Session) -> Result<()> {
    let mut tables = Vec::new();
    rupg_analyze::locked_tables(query, &mut tables);
    if tables.is_empty() {
        return Ok(());
    }
    if session.setting("transaction_read_only").as_deref() == Some("on") {
        let strength = rupg_analyze::first_row_mark(query);
        let clause = strength.map_or("FOR UPDATE", rupg_analyze::strength_name);
        return Err(Error::new(
            SqlState::READ_ONLY_SQL_TRANSACTION,
            format!("cannot execute SELECT {clause} in a read-only transaction"),
        ));
    }
    let catalog = session.catalog();
    for oid in tables {
        let Some(relation) = catalog.and_then(|c| c.relation(oid)) else { continue };
        let word = match relation.kind {
            rupg_catalog::RelKind::Sequence => "sequence",
            rupg_catalog::RelKind::MatView => "materialized view",
            _ => continue,
        };
        return Err(Error::new(
            SqlState::WRONG_OBJECT_TYPE,
            format!("cannot lock rows in {word} \"{}\"", relation.name),
        ));
    }
    Ok(())
}

/// True when the top node of the plan of a query can scan backward. `Sort`, the scans of a table, a function, a `VALUES` list and a query of `WITH` can, and `Limit` and `SubqueryScan` can when the node below them can. `Append` for `UNION ALL` can when each part can. `Result` with no `FROM`, the joins, the aggregates, `Unique`, `WindowAgg` and `ProjectSet` cannot.
fn backward(query: &Query) -> bool {
    fn append(query: &Query, op: &SetOp) -> bool {
        let part = |part: &SetTree| match part {
            SetTree::Leaf(index) => {
                query.relations[*index].subquery.as_deref().is_some_and(backward)
            }
            SetTree::Op(op) => append(query, op),
        };
        op.kind == SetKind::Union && op.all && part(&op.left) && part(&op.right)
    }
    // `LockRows` cannot scan backward.
    if query.has_target_srfs() || !query.row_marks.is_empty() {
        return false;
    }
    if !query.sort.is_empty() && query.distinct.is_empty() && query.windows.is_empty() {
        return true;
    }
    if let Some(op) = &query.set_op {
        return append(query, op);
    }
    if query.grouped || !query.distinct.is_empty() || !query.windows.is_empty() {
        return false;
    }
    let [relation] = query.relations.as_slice() else { return false };
    if query.from.len() != 1 || !matches!(query.from[0], FromItem::Relation(_)) {
        return false;
    }
    if relation.function.is_some() {
        return true;
    }
    if let Some(rows) = &relation.values {
        // A `VALUES` list of one row is a `Result`.
        return rows.len() > 1;
    }
    if let Some(cte) = &relation.cte {
        if cte.work.is_some() || cte.transition {
            return false;
        }
        if cte.write.is_some() {
            return true;
        }
        // A query of `WITH` that the planner does not put in place of its reference is a `CteScan`.
        let scanned = query
            .ctes
            .iter()
            .find(|c| c.name == cte.name)
            .is_some_and(|c| c.recursive || c.materialized == Some(true));
        if scanned {
            return true;
        }
    }
    match &relation.subquery {
        Some(sub) => backward(sub),
        None => true,
    }
}

/// `DISTINCT`, `ORDER BY`, `OFFSET` and `LIMIT`. A `DISTINCT` sorts the rows on the items of `DISTINCT` and keeps the first row of each group, as `Sort` and `Unique` do. When a type of `DISTINCT` has no order, it keeps the first row of each group in the order of the rows and then sorts for `ORDER BY`. A `Sort` below `Limit` keeps only the first `OFFSET` + `LIMIT` rows, as `tuplesort_set_bound` does. When the plan has no `Sort` and scans an index in the order of its keys, the rows with equal keys keep the order of the scans.
fn order(
    query: &Query,
    mut rows: Vec<Vec<Value>>,
    types: &[u32],
    offset: usize,
    count: Option<usize>,
    session: &dyn Session,
) -> Result<Vec<Vec<Value>>> {
    let mut sorted = query.sort.is_empty();
    if !query.distinct.is_empty() {
        let equal = sort::Keys::new(&query.distinct, types, session)?;
        let table = session.hash_tables(query).and_then(|t| t.distinct.clone());
        let hash = |row: &[Value]| {
            let keys = table.as_ref().map_or(&[][..], |t| &t.keys[..]);
            hashed::hash(keys.iter().map(|&k| (types[k], &row[k])))
        };
        if let Some(table) = &table
            && rows.first().is_none_or(|row| hash(row).is_ok())
        {
            // `HashAggregate` gives the first row of each group in the order of its hash table.
            let groups = hashed::groups(
                rows.len(),
                table.groups,
                &mut |row| hash(&rows[row]),
                &mut |a, b| equal.equal(&rows[a], &rows[b]),
            )?;
            let mut slots: Vec<Option<Vec<Value>>> = rows.into_iter().map(Some).collect();
            rows = groups.iter().filter_map(|group| slots[group[0]].take()).collect();
        } else if query.distinct.iter().all(|g| g.sort != 0) {
            // The items of `DISTINCT ON` come first in `ORDER BY`, so the longer list sorts for both.
            let list: &[SortGroup] =
                if query.sort.len() > query.distinct.len() { &query.sort } else { &query.distinct };
            let keys = sort::Keys::new(list, types, session)?;
            sort::qsort(&mut rows, &mut |a, b| keys.compare(a, b))?;
            let mut unique: Vec<Vec<Value>> = Vec::new();
            for row in rows {
                if let Some(last) = unique.last()
                    && equal.equal(last, &row)?
                {
                    continue;
                }
                unique.push(row);
            }
            rows = unique;
            sorted = true;
        } else {
            let mut unique: Vec<Vec<Value>> = Vec::new();
            for row in rows {
                let mut seen = false;
                for other in &unique {
                    if equal.equal(other, &row)? {
                        seen = true;
                        break;
                    }
                }
                if !seen {
                    unique.push(row);
                }
            }
            rows = unique;
        }
    }
    if !sorted {
        let keys = sort::Keys::new(&query.sort, types, session)?;
        // `compute_tuples_needed` and `tuplesort_set_bound`: the bound must allow `bound * 2` in an `int`.
        let bound = match count {
            Some(count) if !query.with_ties => offset
                .checked_add(count)
                .filter(|b| *b <= usize::try_from(i32::MAX / 2).unwrap_or(usize::MAX)),
            _ => None,
        };
        match bound {
            // A plan with no `Sort` gives the rows in the order of its scans, which `ORDER BY` keeps for equal keys. The order of a scan by distance of a `gist` index is not known.
            _ if session
                .join_order(query)
                .is_some_and(|order| order.iter().any(|s| !s.keys.is_empty())) =>
            {
                let mut tagged: Vec<(usize, Vec<Value>)> = rows.into_iter().enumerate().collect();
                sort::qsort(&mut tagged, &mut |a, b| {
                    Ok(keys.compare(&a.1, &b.1)?.then(a.0.cmp(&b.0)))
                })?;
                rows = tagged.into_iter().map(|(_, row)| row).collect();
            }
            Some(bound) if rows.len() > bound * 2 => {
                rows = sort::bounded(rows, bound, &mut |a, b| keys.compare(a, b))?;
            }
            _ => sort::qsort(&mut rows, &mut |a, b| keys.compare(a, b))?,
        }
    }
    let start = offset.min(rows.len());
    let mut end = count.map_or(rows.len(), |c| start.saturating_add(c).min(rows.len()));
    if query.with_ties && end > start {
        // `LIMIT_WINDOWEND_TIES`: the rows equal to the last row of the window on the items of `ORDER BY` come too.
        let keys = sort::Keys::new(&query.sort, types, session)?;
        let last = end - 1;
        while end < rows.len() && keys.equal(&rows[last], &rows[end])? {
            end += 1;
        }
    }
    rows.truncate(end);
    rows.drain(..start);
    Ok(rows)
}

/// The state of a group of a query with groups.
#[derive(Clone, Debug, Default)]
pub(crate) struct Group {
    /// The result of each aggregate call for the group, by the address of the call in the query.
    pub(crate) aggs: Vec<(*const Aggref, Value)>,
    /// The targets of the items of `GROUP BY` that are not in the grouping set of the group, which `GROUPING` reads.
    pub(crate) out: Vec<usize>,
    /// The expressions that are null in the group, by address: the items of `GROUP BY` that are not in the grouping set of the group, also in the subqueries, and the expressions that the queries outside the query make null.
    pub(crate) nulled: Vec<*const Expr>,
}

/// The state of the evaluation of the expressions of one row.
struct Eval<'a> {
    params: &'a [Value],
    session: &'a dyn Session,
    /// The values of the `CASE` expressions that are open, for `CaseTest`.
    case: Vec<Value>,
    /// The rows of the relations of the query.
    tables: &'a scan::Tables,
    /// The row of each relation. In a query with groups it is the first row of the group.
    tuple: &'a [usize],
    /// The aggregate calls and the grouping set of the group.
    group: Group,
    cache: &'a Cache,
    /// The rows of the queries outside the query, with the query just outside it last.
    outer: &'a [Frame<'a>],
    /// The rows of the subqueries of `ANY` and `ALL` whose test runs now, for `SubColumn`.
    sub: Vec<Vec<Value>>,
    /// The values that the checks of a domain test now, for `DomainValue`.
    domain_value: Vec<Value>,
    /// The value of each call of a function that gives a set in the row that the targets compute now, by the address of the call in the query.
    sets: Vec<(*const Expr, Value)>,
    /// The value of each window function call for the row, by the address of the call in the query.
    windows: Vec<(*const WindowFunc, Value)>,
    /// The rows of the subquery of each `SET (a, b) = (SELECT ...)` for the last row that read it, by the ID of the item, which the columns of the item share, as `ExecSetParamPlan` sets the parameters of `PARAM_MULTIEXPR`.
    multi: Vec<(u32, Vec<usize>, Rc<Rows>)>,
}

impl<'a> Eval<'a> {
    /// A new state for the evaluation of the expressions of a query with no row.
    fn new(
        params: &'a [Value],
        session: &'a dyn Session,
        tables: &'a scan::Tables,
        cache: &'a Cache,
        outer: &'a [Frame<'a>],
    ) -> Eval<'a> {
        Eval {
            params,
            session,
            case: Vec::new(),
            tables,
            tuple: &[],
            group: Group { nulled: inherited(outer), ..Group::default() },
            cache,
            outer,
            sub: Vec::new(),
            domain_value: Vec::new(),
            sets: Vec::new(),
            windows: Vec::new(),
            multi: Vec::new(),
        }
    }

    /// `tablesample_init`: the arguments of `TABLESAMPLE`, which cannot be null, then the seed of `REPEATABLE`, then the numbers of the rows, of `count`, that the sample keeps.
    fn sample(&mut self, sample: &TableSample, count: usize) -> Result<Vec<usize>> {
        let mut percent = 0.0;
        for arg in &sample.args {
            match self.eval(arg)? {
                Value::Null => {
                    return Err(Error::new(
                        SqlState::INVALID_TABLESAMPLE_ARGUMENT,
                        "TABLESAMPLE parameter cannot be null",
                    ));
                }
                Value::Float4(value) => percent = value,
                _ => return Err(Error::internal("an argument of TABLESAMPLE that is not real")),
            }
        }
        let seed = match &sample.repeatable {
            Some(expr) => match self.eval(expr)? {
                Value::Null => {
                    return Err(Error::new(
                        SqlState::INVALID_TABLESAMPLE_REPEAT,
                        "TABLESAMPLE REPEATABLE parameter cannot be null",
                    ));
                }
                Value::Float8(value) => Some(value),
                _ => return Err(Error::internal("a seed of REPEATABLE that is not a float")),
            },
            None => None,
        };
        rupg_func::sample_rows(percent, seed, count, self.session)
    }

    /// The rows of a `VALUES` list, as `ValuesScan` gives them.
    fn values_rows(&mut self, rows: &[Vec<Expr>]) -> Result<Rows> {
        rows.iter().map(|row| row.iter().map(|e| self.eval(e)).collect()).collect()
    }

    /// `ExecFunctionScan`: the rows of a function in `FROM`. The rows of the calls of `ROWS FROM` go side by side, and a call with fewer rows gives nulls. With ordinality, the last column is the number of the row.
    fn function_rows(&mut self, function: &FromFunction, columns: &[Column]) -> Result<Rows> {
        self.function_rows_kept(function, columns, None)
    }

    /// [`Eval::function_rows`] for a new scan of a function in `FROM` that reads the other relations. A call that reads none of them gives the rows in `kept` from the first scan, as `ExecReScanFunctionScan` runs again only the calls whose arguments read a changed parameter.
    fn function_rows_kept(
        &mut self,
        function: &FromFunction,
        columns: &[Column],
        mut kept: Option<&mut Vec<Option<Rows>>>,
    ) -> Result<Rows> {
        let mut sets = Vec::with_capacity(function.calls.len());
        let mut first = 0;
        for (k, (call, width)) in function.calls.iter().enumerate() {
            let list = columns.get(first..first + width).unwrap_or_default();
            // The columns of the column definition list of the call.
            let defined: Vec<(u32, i32)> = if function.coldefs.get(k) == Some(&true) {
                list.iter().map(|c| (c.ty, c.typmod)).collect()
            } else {
                Vec::new()
            };
            first += width;
            let keep = kept.as_deref_mut().filter(|_| call.first_var().is_none());
            let Some(kept) = keep else {
                let set = self.call_rows(call, *width, &defined, list)?;
                sets.push(row_fields(set, list)?);
                continue;
            };
            if kept.len() <= k {
                kept.resize(k + 1, None);
            }
            if kept[k].is_none() {
                let set = self.call_rows(call, *width, &defined, list)?;
                kept[k] = Some(row_fields(set, list)?);
            }
            sets.push(kept[k].clone().unwrap_or_default());
        }
        let count = sets.iter().map(Vec::len).max().unwrap_or(0);
        let mut rows = Vec::with_capacity(count);
        for i in 0..count {
            let mut row = Vec::new();
            for ((_, width), set) in function.calls.iter().zip(&sets) {
                match set.get(i) {
                    Some(values) => row.extend(values.iter().cloned()),
                    None => row.extend(std::iter::repeat_n(Value::Null, *width)),
                }
            }
            if function.ordinality {
                row.push(Value::Int8(i64::try_from(i + 1).unwrap_or(i64::MAX)));
            }
            rows.push(row);
        }
        Ok(rows)
    }

    /// [`Eval::function_set`] for a call in `FROM`. A function of `json_populate_record` reads the names of the columns of the column definition list `list`.
    fn call_rows(
        &mut self,
        call: &Expr,
        width: usize,
        defined: &[(u32, i32)],
        list: &[Column],
    ) -> Result<Rows> {
        if let ExprKind::Func(f) = &call.kind
            && let Some(p) = populate::populate_function(f.oid)
        {
            let list = if defined.is_empty() { &[] } else { list };
            let values = self.populate_call(f, p, list)?;
            if list.len() > 1 {
                // `tupledesc_match`: the record of a base record of type `record` can have other fields than the list.
                return Ok(values.into_iter().map(|value| vec![value]).collect());
            }
            return Ok(values
                .into_iter()
                .map(|value| match value {
                    Value::Record(record) => record.values,
                    Value::Null => vec![Value::Null; width],
                    value => vec![value],
                })
                .collect());
        }
        self.function_set(call, width, defined, None)
    }

    /// `ExecMakeTableFunctionResult`: the rows of one call of a function in `FROM`. A strict function with a null argument gives no rows when it gives a set, and else one row of nulls. Another expression gives one row with its value. `want` is the number of rows that the caller reads, which a set kernel can stop after.
    fn function_set(
        &mut self,
        call: &Expr,
        width: usize,
        defined: &[(u32, i32)],
        want: Option<usize>,
    ) -> Result<Rows> {
        self.function_set_rows(call, width, defined, want, &mut Vec::new())
    }

    /// [`Eval::function_set`], which also puts in `shown` the types of the columns of the last statement of a body that [`Body::Statements`] runs.
    fn function_set_rows(
        &mut self,
        call: &Expr,
        width: usize,
        defined: &[(u32, i32)],
        want: Option<usize>,
        shown: &mut Vec<u32>,
    ) -> Result<Rows> {
        if let ExprKind::JsonTable(table) = &call.kind {
            return self.json_table(table);
        }
        let ExprKind::Func(f) = &call.kind else { return self.function_row(call, width) };
        if !table_function(f.oid, width) {
            return self.function_row(call, width);
        }
        let exprs = rupg_analyze::expand_arguments(f)?;
        let args = exprs.iter().map(|a| self.eval(a)).collect::<Result<Vec<_>>>()?;
        if strict(f.oid) && args.iter().any(Value::is_null) {
            let retset = builtin::proc_by_oid(f.oid).is_some_and(|p| p.retset);
            return Ok(if retset { Vec::new() } else { vec![vec![Value::Null; width]] });
        }
        let types: Vec<u32> = exprs.iter().map(|a| a.ty).collect();
        let session = self.session;
        if procedural(f.oid) {
            // A row value gives a column for each field, and a null value gives a row of nulls.
            let composite = call.ty == oid::RECORD
                || builtin::type_by_oid(call.ty).is_some_and(|t| t.kind == b'c');
            let values = with_settings(session, f.oid, || {
                session.call_function(f.oid, &types, call.ty, &args)
            })?;
            return Ok(values
                .into_iter()
                .map(|value| match value {
                    Value::Record(record) if composite => record.values,
                    Value::Null if composite => vec![Value::Null; width],
                    value => vec![value],
                })
                .collect());
        }
        let rows = with_settings(session, f.oid, || {
            let body = self.cache.body_for(f.oid, &types, defined, session).map_err(|e| {
                // `sql_compile_error_callback`: the error of the check of the result of the body.
                match session.catalog().and_then(|c| c.function(f.oid)) {
                    Some(function) if !defined.is_empty() => {
                        e.with_context(format!("SQL function \"{}\" statement 1", function.name))
                    }
                    _ => e,
                }
            })?;
            let Some(body) = body else { return Ok(None) };
            match &*body {
                Body::Query(query) => {
                    let cache = self.cache.nested()?;
                    // `sql_exec_error_callback`: a function that gives a set is not inlined here, so its error has the line of the function.
                    run_query(query, &args, session, &cache, &[], false).map(Some).map_err(|e| {
                        let name = builtin::proc_by_oid(f.oid).map_or("", |p| p.name);
                        e.with_context(format!("SQL function \"{name}\" statement 1"))
                    })
                }
                Body::Statements(_) => {
                    let (columns, rows) = session.call_sql_function(f.oid, &types, &args)?;
                    *shown = columns;
                    Ok(Some(rows))
                }
                Body::Value(_) => {
                    Err(Error::internal("a function that returns a set with no query"))
                }
            }
        })?;
        if let Some(mut rows) = rows {
            // `fmgr_sql`: a function that does not give a set gives the first row of its last query, or a row of nulls when the query gives no row.
            if builtin::proc_by_oid(f.oid).is_some_and(|p| !p.retset) {
                rows.truncate(1);
                if rows.is_empty() {
                    rows.push(vec![Value::Null; width]);
                }
            } else if call.ty == oid::VOID {
                // `fmgr_sql`: a function that gives a set of `void` runs its last query and gives no row.
                rows.clear();
            }
            return Ok(rows);
        }
        if row_function(f.oid) {
            // A null value gives a row of nulls.
            return match self.call(f.oid, &types, call.ty, f.variadic, &args)? {
                Value::Record(record) => Ok(vec![record.values]),
                Value::Null => Ok(vec![vec![Value::Null; width]]),
                _ => Err(Error::internal("a function in FROM did not give a row")),
            };
        }
        let kernel = set_kernel(f.oid, call.location)?;
        let c =
            Call { session: self.session, args: &types, ret: call.ty, variadic: f.variadic, want };
        kernel(&c, &args)
    }

    /// The one row of an expression in `FROM` that does not give a set. A value of a composite type gives a column for each field, and a null value gives a row of nulls.
    fn function_row(&mut self, call: &Expr, width: usize) -> Result<Rows> {
        let value = self.eval(call)?;
        if !builtin::type_by_oid(call.ty).is_some_and(|t| t.kind == b'c') {
            return Ok(vec![vec![value]]);
        }
        match value {
            Value::Record(record) if record.values.len() == width => Ok(vec![record.values]),
            Value::Null => Ok(vec![vec![Value::Null; width]]),
            _ => Err(Error::internal("a function in FROM did not give a row of its type")),
        }
    }

    /// `ExecProjectSet`: the rows of the targets for the row of the query. The calls of functions that give a set run level by level, and the calls of a level read the values of the levels below. The calls of one level go side by side, and a call with fewer values gives nulls, as `ExecProjectSRF` does. When no call of a level gives a value, the row gives no rows. `needed` is the number of rows that `out` must have at most, or `usize::MAX`: each value of the top level gives one row, so the calls of the top level stop after the rows that `out` does not have yet, as `Limit` stops to read from `ProjectSet`.
    fn project(
        &mut self,
        targets: &[Target],
        levels: &[Vec<&Expr>],
        out: &mut Vec<Vec<Value>>,
        needed: usize,
    ) -> Result<()> {
        let Some((calls, above)) = levels.split_first() else {
            out.push(targets.iter().map(|t| self.eval(&t.expr)).collect::<Result<_>>()?);
            return Ok(());
        };
        let want =
            (above.is_empty() && needed != usize::MAX).then(|| needed.saturating_sub(out.len()));
        let sets =
            calls.iter().map(|call| self.set_values(call, want)).collect::<Result<Vec<_>>>()?;
        let count = sets.iter().map(Vec::len).max().unwrap_or(0);
        let base = self.sets.len();
        for i in 0..count {
            for (call, set) in calls.iter().zip(&sets) {
                let value = set.get(i).cloned().unwrap_or(Value::Null);
                self.sets.push((std::ptr::from_ref(*call), value));
            }
            let result = self.project(targets, above, out, needed);
            self.sets.truncate(base);
            result?;
        }
        Ok(())
    }

    /// `ExecMakeFunctionResultSet`: the values of a call of a function that gives a set. A function that gives rows of more than one column, or of type `record`, gives a row value for each row.
    fn set_values(&mut self, call: &Expr, want: Option<usize>) -> Result<Vec<Value>> {
        let ExprKind::Func(f) = &call.kind else {
            return Err(Error::internal("a set that is not a function call"));
        };
        if procedural(f.oid) {
            return self.procedural_set(f, call.ty);
        }
        if let Some(p) = populate::populate_function(f.oid) {
            return self.populate_call(f, p, &[]);
        }
        let mut shown = Vec::new();
        let rows = self.function_set_rows(call, 1, &[], want, &mut shown)?;
        // The types of the `OUT` parameters, with the actual types of the call for a polymorphic type.
        let columns = rupg_analyze::out_columns(f)?.unwrap_or_default();
        let mut types: Vec<u32> = columns.iter().map(|c| c.ty).collect();
        // A function in SQL that returns a composite type gives the types of the columns of its query.
        if types.is_empty() && rows.first().is_some_and(|row| row.len() > 1) {
            let args: Vec<u32> = rupg_analyze::expand_arguments(f)?.iter().map(|a| a.ty).collect();
            match self.cache.body(f.oid, &args, self.session)?.as_deref() {
                Some(Body::Query(query)) => {
                    types = shown_types(query);
                }
                Some(Body::Statements(columns)) if columns.is_empty() => types = shown,
                Some(Body::Statements(columns)) => types.clone_from(columns),
                _ => {}
            }
        }
        let names: Option<Arc<[String]>> =
            (!columns.is_empty()).then(|| columns.into_iter().map(|c| c.name).collect());
        Ok(rows
            .into_iter()
            .map(|mut row| {
                if row.len() == 1 && call.ty != oid::RECORD {
                    row.pop().unwrap_or(Value::Null)
                } else {
                    let (types, names) = (types.clone(), names.clone());
                    Value::Record(Box::new(Record { types, values: row, names }))
                }
            })
            .collect())
    }

    /// The values of a call of a function in PL/pgSQL that gives a set, with a row value for each row of a composite type. A strict function with a null argument gives no values.
    fn procedural_set(&mut self, f: &Func, ret: u32) -> Result<Vec<Value>> {
        let exprs = rupg_analyze::expand_arguments(f)?;
        let args = exprs.iter().map(|a| self.eval(a)).collect::<Result<Vec<_>>>()?;
        if strict(f.oid) && args.iter().any(Value::is_null) {
            return Ok(Vec::new());
        }
        let types: Vec<u32> = exprs.iter().map(|a| a.ty).collect();
        let session = self.session;
        with_settings(session, f.oid, || session.call_function(f.oid, &types, ret, &args))
    }

    /// A new state for the evaluation of a condition on another tuple of the same query.
    fn at<'b>(&self, tuple: &'b [usize]) -> Eval<'b>
    where
        'a: 'b,
    {
        Eval {
            params: self.params,
            session: self.session,
            case: Vec::new(),
            tables: self.tables,
            tuple,
            group: Group { nulled: self.group.nulled.clone(), ..Group::default() },
            cache: self.cache,
            outer: self.outer,
            sub: Vec::new(),
            domain_value: Vec::new(),
            sets: Vec::new(),
            windows: Vec::new(),
            multi: Vec::new(),
        }
    }

    /// `recompute_limits` of `nodeLimit.c`: the number of rows that `OFFSET` skips, and the number of rows that `LIMIT` gives or `None` for all rows. A null `OFFSET` is 0.
    fn limits(&mut self, query: &Query) -> Result<(usize, Option<usize>)> {
        let mut value = |expr: Option<&Expr>| -> Result<Option<i64>> {
            match expr.map(|e| self.eval(e)).transpose()? {
                None | Some(Value::Null) => Ok(None),
                Some(Value::Int8(v)) => Ok(Some(v)),
                Some(_) => Err(Error::internal("LIMIT and OFFSET must be bigint")),
            }
        };
        let offset = value(query.offset.as_ref())?.unwrap_or(0);
        if offset < 0 {
            return Err(Error::new(
                SqlState::INVALID_ROW_COUNT_IN_RESULT_OFFSET_CLAUSE,
                "OFFSET must not be negative",
            ));
        }
        let count = value(query.limit.as_ref())?;
        if count.is_some_and(|c| c < 0) {
            return Err(Error::new(
                SqlState::INVALID_ROW_COUNT_IN_LIMIT_CLAUSE,
                "LIMIT must not be negative",
            ));
        }
        // A number of rows that does not fit in memory is the same as all rows.
        let size = |v: i64| usize::try_from(v).unwrap_or(usize::MAX);
        Ok((size(offset), count.map(size)))
    }

    fn eval(&mut self, expr: &Expr) -> Result<Value> {
        if !self.group.nulled.is_empty() && self.group.nulled.contains(&std::ptr::from_ref(expr)) {
            return Ok(Value::Null);
        }
        match &expr.kind {
            ExprKind::Const(v) => Ok(v.clone()),
            ExprKind::Param(n) => {
                // `$n` counts from 1.
                let value = n.checked_sub(1).and_then(|i| self.params.get(i));
                value.cloned().ok_or_else(|| Error::internal("a parameter has no value"))
            }
            ExprKind::Func(f) if f.retset => {
                let at = std::ptr::from_ref(expr);
                let value = self.sets.iter().rev().find(|(call, _)| *call == at);
                value.map(|(_, v)| v.clone()).ok_or_else(|| {
                    Error::new(
                        SqlState::FEATURE_NOT_SUPPORTED,
                        "set-valued function called in context that cannot accept a set",
                    )
                })
            }
            ExprKind::Func(f) => self.func(f, expr),
            ExprKind::NamedArg(na) => self.eval(&na.arg),
            ExprKind::Relabel(arg, _) => Ok(relabel(self.eval(arg)?, expr.ty)),
            ExprKind::Collate(arg, _) => self.eval(arg),
            // `ExecEvalConvertRowtype`: the fields of the row in the order of the fields of the type.
            ExprKind::ConvertRow { arg, fields, .. } => Ok(match self.eval(arg)? {
                Value::Record(record) => {
                    let types = fields.iter().map(|&i| record.types.get(i).copied().unwrap_or(0));
                    let values = fields
                        .iter()
                        .map(|&i| record.values.get(i).cloned().unwrap_or(Value::Null));
                    Value::Record(Box::new(Record::new(types.collect(), values.collect())))
                }
                _ => Value::Null,
            }),
            ExprKind::CoerceViaIo(arg, _) => {
                let value = self.eval(arg)?;
                if value.is_null() {
                    return Ok(Value::Null);
                }
                let mut text = Vec::new();
                rupg_func::output(arg.ty, &value, self.session, &mut text)?;
                let text = String::from_utf8(text).map_err(|_| {
                    Error::internal("an output function gave bytes that are not UTF-8")
                })?;
                rupg_func::input(expr.ty, &text, -1, self.session)
            }
            ExprKind::ArrayCoerce { arg, element, .. } => self.array_coerce(arg, element),
            ExprKind::CoerceToDomain(arg, _) => {
                let value = relabel(self.eval(arg)?, expr.ty);
                self.domain_check(expr.ty, value)
            }
            ExprKind::DomainValue => self
                .domain_value
                .last()
                .cloned()
                .ok_or_else(|| Error::internal("a VALUE is outside of the check of a domain")),
            ExprKind::Subscript(sub) => self.subscript(sub),
            ExprKind::FieldStore(store) => self.field_store(store),
            ExprKind::Bool(op, args) => self.bool_op(*op, args),
            ExprKind::NullTest(arg, is_null) => {
                Ok(Value::Bool(null_test(&self.eval(arg)?, *is_null)))
            }
            ExprKind::FieldSelect(arg, field) => Ok(match self.eval(arg)? {
                Value::Record(record) => record.values.get(*field).cloned().unwrap_or(Value::Null),
                _ => Value::Null,
            }),
            ExprKind::BooleanTest(arg, test) => {
                let v = self.eval(arg)?.as_bool();
                Ok(Value::Bool(match test {
                    BoolTest::IsTrue => v == Some(true),
                    BoolTest::IsNotTrue => v != Some(true),
                    BoolTest::IsFalse => v == Some(false),
                    BoolTest::IsNotFalse => v != Some(false),
                    BoolTest::IsUnknown => v.is_none(),
                    BoolTest::IsNotUnknown => v.is_some(),
                }))
            }
            ExprKind::Case(case) => self.case(case),
            ExprKind::CaseTest => self
                .case
                .last()
                .cloned()
                .ok_or_else(|| Error::internal("a CaseTest is outside of a CASE")),
            ExprKind::Coalesce(args) => {
                for arg in args {
                    let v = self.eval(arg)?;
                    if !v.is_null() {
                        return Ok(v);
                    }
                }
                Ok(Value::Null)
            }
            ExprKind::MinMax { greatest, less, args } => self.min_max(*greatest, *less, args),
            ExprKind::RowCompare(rc) => self.row_compare(rc),
            ExprKind::NullIf { equal, args } => {
                let [a, b] = args.as_slice() else {
                    return Err(Error::internal("NULLIF needs two arguments"));
                };
                let (x, y) = (self.eval(a)?, self.eval(b)?);
                if !x.is_null() && !y.is_null() {
                    let types = [a.ty, b.ty];
                    if self.call(*equal, &types, oid::BOOL, false, &[x.clone(), y])?
                        == Value::Bool(true)
                    {
                        return Ok(Value::Null);
                    }
                }
                Ok(x)
            }
            ExprKind::Distinct { equal, not, args } => {
                let [a, b] = args.as_slice() else {
                    return Err(Error::internal("DISTINCT needs two arguments"));
                };
                let (x, y) = (self.eval(a)?, self.eval(b)?);
                let distinct = match (x.is_null(), y.is_null()) {
                    (true, true) => false,
                    (true, false) | (false, true) => true,
                    (false, false) => {
                        self.call(*equal, &[a.ty, b.ty], oid::BOOL, false, &[x, y])?
                            != Value::Bool(true)
                    }
                };
                Ok(Value::Bool(distinct != *not))
            }
            ExprKind::ScalarArrayOp { func, any, args } => self.scalar_array_op(*func, *any, args),
            ExprKind::Array { multidims, elements, .. } => self.array(*multidims, elements),
            ExprKind::Row(args, _) => {
                let values = args.iter().map(|e| self.eval(e)).collect::<Result<Vec<_>>>()?;
                let types = args.iter().map(|e| e.ty).collect();
                Ok(Value::Record(Box::new(Record::new(types, values))))
            }
            ExprKind::JoinRow(row) => {
                let values = row.fields.iter().map(|e| self.eval(e)).collect::<Result<Vec<_>>>()?;
                let types = row.fields.iter().map(|e| e.ty).collect();
                let names = Some(row.names.iter().cloned().collect());
                Ok(Value::Record(Box::new(Record { types, values, names })))
            }
            ExprKind::SqlValue(v) => self.sql_value(*v),
            ExprKind::Var(var) if var.levels_up == 0 => {
                Ok(scan::item_pointer(self.tables.var(self.tuple, *var), *var, expr.ty))
            }
            ExprKind::Var(var) => {
                let frame = self
                    .outer
                    .len()
                    .checked_sub(var.levels_up)
                    .and_then(|i| self.outer.get(i))
                    .ok_or_else(|| Error::internal("a column of an outer query has no row"))?;
                Ok(scan::item_pointer(frame.tables.var(frame.tuple, *var), *var, expr.ty))
            }
            ExprKind::SubLink(sub) => self.sublink(sub, expr.ty),
            ExprKind::SubColumn(n) => self
                .sub
                .last()
                .and_then(|row| row.get(*n))
                .cloned()
                .ok_or_else(|| Error::internal("a subquery column is outside of ANY or ALL")),
            ExprKind::Agg(agg) if agg.levels_up > 0 => self
                .outer
                .len()
                .checked_sub(agg.levels_up)
                .and_then(|i| self.outer.get(i))
                .and_then(|frame| frame.group)
                .and_then(|g| g.aggs.iter().find(|(call, _)| std::ptr::eq(*call, &**agg)))
                .map(|(_, value)| value.clone())
                .ok_or_else(|| {
                    Error::internal("an aggregate of an outer query is outside of a group")
                }),
            ExprKind::Agg(agg) => self
                .group
                .aggs
                .iter()
                .find(|(call, _)| std::ptr::eq(*call, &**agg))
                .map(|(_, value)| value.clone())
                .ok_or_else(|| Error::internal("an aggregate call is outside of a group")),
            // `ExecEvalGroupingFunc`: a bit for each argument, the last one lowest, which is 1 when the argument is not in the grouping set of the group.
            ExprKind::Grouping(g) => {
                let out = if g.levels_up == 0 {
                    Some(&self.group.out)
                } else {
                    self.outer
                        .len()
                        .checked_sub(g.levels_up)
                        .and_then(|i| self.outer.get(i))
                        .and_then(|frame| frame.group)
                        .map(|g| &g.out)
                };
                let out = out.ok_or_else(|| Error::internal("GROUPING is outside of a group"))?;
                let bits =
                    g.refs.iter().fold(0, |bits, r| (bits << 1) | i32::from(out.contains(r)));
                Ok(Value::Int4(bits))
            }
            ExprKind::JsonValue(v) => self.eval(&v.formatted),
            // A `PlaceHolderVar` is null in a row of an outer join that has no row of its relations.
            ExprKind::PlaceHolder(p) => {
                let tuple = if p.levels_up == 0 {
                    self.tuple
                } else {
                    self.outer
                        .len()
                        .checked_sub(p.levels_up)
                        .and_then(|i| self.outer.get(i))
                        .ok_or_else(|| Error::internal("a column of an outer query has no row"))?
                        .tuple
                };
                if !p.relations.is_empty()
                    && p.relations
                        .iter()
                        .all(|r| tuple.get(*r).is_none_or(|row| *row == scan::NONE))
                {
                    return Ok(Value::Null);
                }
                self.eval(&p.expr)
            }
            ExprKind::JsonCtor(c) => self.json_ctor(c),
            ExprKind::IsJson(p) => self.is_json(p),
            ExprKind::JsonExpr(e) => self.json_expr(e),
            ExprKind::Xml(x) => {
                let values =
                    x.children().into_iter().map(|e| self.eval(e)).collect::<Result<Vec<_>>>()?;
                rupg_func::xml_expr(x, &values, self.session)
            }
            ExprKind::JsonTable(_) => Err(Error::internal("JSON_TABLE out of FROM")),
            ExprKind::CurrentOf(c) => self.current_of(c),
            ExprKind::Window(w) => self
                .windows
                .iter()
                .find(|(call, _)| std::ptr::eq(*call, &**w))
                .map(|(_, value)| value.clone())
                .ok_or_else(|| Error::internal("a window function call is outside of a window")),
        }
    }

    /// `ExecEvalCurrentOfExpr` as the scan checks it: true for the row of the table that the cursor is on, which `execCurrentOf` finds.
    fn current_of(&mut self, c: &CurrentOf) -> Result<Value> {
        let [name, tableoid, ctid] = c.args.as_slice() else {
            return Err(Error::internal("CURRENT OF has no cursor"));
        };
        let name = match self.eval(name)? {
            Value::Text(name) => name,
            Value::Null => {
                return Err(Error::new(SqlState::INVALID_CURSOR_NAME, "cursor variable is null"));
            }
            _ => return Err(Error::internal("the cursor of CURRENT OF is not text")),
        };
        let (oid, tid) = self.session.current_of(&name, c.table)?;
        Ok(Value::Bool(self.eval(tableoid)? == oid && self.eval(ctid)? == tid))
    }

    /// `ExecScanSubPlan`: the value of a subquery in an expression.
    fn sublink(&mut self, sub: &SubLink, ty: u32) -> Result<Value> {
        let exists = sub.kind == SubLinkKind::Exists;
        let query = &sub.query;
        let mut frames = self.outer.to_vec();
        frames.push(Frame { tables: self.tables, tuple: self.tuple, group: Some(&self.group) });
        let kept = match sub.kind {
            SubLinkKind::Multi { id, .. } => self
                .multi
                .iter()
                .find(|(other, tuple, _)| *other == id && tuple[..] == *self.tuple)
                .map(|(_, _, rows)| Rc::clone(rows)),
            _ => None,
        };
        let rows = match kept {
            Some(rows) => rows,
            None => {
                let rows = self.cache.rows(query, || {
                    // `ExecScanSubPlan` stops at the second row of a subquery that gives one row.
                    if matches!(
                        sub.kind,
                        SubLinkKind::Expr | SubLinkKind::RowCompare | SubLinkKind::Multi { .. }
                    ) {
                        self.cache.want.set(Some(2));
                    }
                    run_query(query, self.params, self.session, self.cache, &frames, exists)
                })?;
                if let SubLinkKind::Multi { id, .. } = sub.kind {
                    self.multi.retain(|(other, _, _)| *other != id);
                    self.multi.push((id, self.tuple.to_vec(), Rc::clone(&rows)));
                }
                rows
            }
        };
        match sub.kind {
            SubLinkKind::Exists => Ok(Value::Bool(!rows.is_empty())),
            SubLinkKind::Expr => match rows.as_slice() {
                [] => Ok(Value::Null),
                [row] => row
                    .first()
                    .cloned()
                    .ok_or_else(|| Error::internal("a subquery row has no column")),
                _ => Err(Error::new(
                    SqlState::CARDINALITY_VIOLATION,
                    "more than one row returned by a subquery used as an expression",
                )),
            },
            // No row gives a null for each column.
            SubLinkKind::Multi { column, .. } => match rows.as_slice() {
                [] => Ok(Value::Null),
                [row] => row
                    .get(column)
                    .cloned()
                    .ok_or_else(|| Error::internal("a subquery row has too few columns")),
                _ => Err(Error::new(
                    SqlState::CARDINALITY_VIOLATION,
                    "more than one row returned by a subquery used as an expression",
                )),
            },
            SubLinkKind::RowCompare => match rows.as_slice() {
                [] => Ok(Value::Null),
                [row] => {
                    let test = sub
                        .test
                        .as_ref()
                        .ok_or_else(|| Error::internal("a row comparison needs a test"))?;
                    self.sub.push(row.clone());
                    let result = self.eval(test);
                    self.sub.pop();
                    result
                }
                _ => Err(Error::new(
                    SqlState::CARDINALITY_VIOLATION,
                    "more than one row returned by a subquery used as an expression",
                )),
            },
            SubLinkKind::Array
                if query.targets.iter().find(|t| !t.junk).is_some_and(|t| t.expr.ty == ty) =>
            {
                // The column is an array, so the result has one more dimension, as `accumArrayResultArr`.
                if rows.is_empty() {
                    return Ok(Value::Array(Box::new(Array::empty())));
                }
                let (mut dims, mut count, mut values) = (Vec::new(), 0, Vec::new());
                for row in rows.iter() {
                    agg::accumulate_array(row.first(), &mut dims, &mut count, &mut values)?;
                }
                let mut all = Vec::with_capacity(dims.len() + 1);
                all.push(ArrayDim { len: count, lower: 1 });
                all.extend(dims);
                Ok(Value::Array(Box::new(Array { dims: all, values })))
            }
            SubLinkKind::Array => {
                let values =
                    rows.iter().map(|row| row.first().filter(|v| !v.is_null()).cloned()).collect();
                Ok(Value::Array(Box::new(Array::one(values))))
            }
            SubLinkKind::Any | SubLinkKind::All => {
                let test =
                    sub.test.as_ref().ok_or_else(|| Error::internal("ANY and ALL need a test"))?;
                let any = sub.kind == SubLinkKind::Any;
                if any && let Some(found) = self.hashed_any(query, test, &rows)? {
                    return Ok(Value::Bool(found));
                }
                let mut unknown = false;
                for row in rows.iter() {
                    self.sub.push(row.clone());
                    let result = self.eval(test);
                    self.sub.pop();
                    match result?.as_bool() {
                        Some(v) if v == any => return Ok(Value::Bool(any)),
                        Some(_) => {}
                        None => unknown = true,
                    }
                }
                Ok(if unknown { Value::Null } else { Value::Bool(!any) })
            }
        }
    }

    /// `ExecHashSubPlan`: the result of `ANY` through the hash table of a subquery that runs once. `None` when the test or a value has no key, or when the result can be null, so that the test of each row must decide.
    fn hashed_any(&mut self, query: &Query, test: &Expr, rows: &Rc<Rows>) -> Result<Option<bool>> {
        let kept = self
            .cache
            .rows
            .borrow()
            .iter()
            .any(|(q, r)| std::ptr::eq(*q, query) && Rc::ptr_eq(r, rows));
        if !kept {
            return Ok(None);
        }
        let Some(pairs) = hash_pairs(test) else { return Ok(None) };
        let found = self
            .cache
            .hashed
            .borrow()
            .iter()
            .find(|(q, _)| std::ptr::eq(*q, query))
            .map(|(_, h)| h.clone());
        let hashed = match found {
            Some(hashed) => hashed,
            None => {
                let hashed = hash_rows(rows, &pairs).map(Rc::new);
                self.cache.hashed.borrow_mut().push((query, hashed.clone()));
                hashed
            }
        };
        let Some(hashed) = hashed else { return Ok(None) };
        let mut keys = Vec::with_capacity(pairs.len());
        for (value, _) in &pairs {
            match hash_key(self.eval(value)?) {
                Ok(Some(k)) => keys.push(k),
                _ => return Ok(None),
            }
        }
        if hashed.keys.contains(&keys) {
            Ok(Some(true))
        } else if hashed.nulls {
            Ok(None)
        } else {
            Ok(Some(false))
        }
    }

    /// A call of a function with the values of its arguments.
    fn call(
        &self,
        func: u32,
        types: &[u32],
        ret: u32,
        variadic: bool,
        args: &[Value],
    ) -> Result<Value> {
        if strict(func) && args.iter().any(Value::is_null) {
            return Ok(Value::Null);
        }
        with_settings(self.session, func, || self.run_function(func, types, ret, variadic, args))
    }

    /// [`Eval::call`] after the test of a strict function, with the settings of the function.
    fn run_function(
        &self,
        func: u32,
        types: &[u32],
        ret: u32,
        variadic: bool,
        args: &[Value],
    ) -> Result<Value> {
        if procedural(func) {
            let values = self.session.call_function(func, types, ret, args)?;
            return Ok(values.into_iter().next().unwrap_or(Value::Null));
        }
        // A built-in function with a kernel has no body to run, which [`Cache::body`] finds with more lookups.
        if func < FIRST_NORMAL_OID
            && let Some(kernel) = rupg_func::kernel(func)
            && self.session.catalog().and_then(|c| c.function(func)).is_none()
        {
            let call = Call { session: self.session, args: types, ret, variadic, want: None };
            return kernel(&call, args);
        }
        if let Some(body) = self.cache.body(func, types, self.session)? {
            // The body runs with a cache of its own, so that a subquery of the body that reads an argument runs again for each call.
            let cache = self.cache.nested()?;
            return match &*body {
                Body::Value(expr) => {
                    let tables = scan::Tables::default();
                    let value = Eval::new(args, self.session, &tables, &cache, &[]).eval(expr)?;
                    Ok(if ret == oid::VOID { Value::Null } else { value })
                }
                Body::Query(query) => {
                    let rows = run_query(query, args, self.session, &cache, &[], false);
                    // `sql_exec_error_callback`: a call that runs here is not inlined, so its error has the line of the function.
                    let rows = rows.map_err(|error| {
                        let name = builtin::proc_by_oid(func).map_or("", |p| p.name);
                        error.with_context(format!("SQL function \"{name}\" statement 1"))
                    })?;
                    Ok(sql_result(ret, rows, shown_types(query)))
                }
                Body::Statements(_) => {
                    let (columns, rows) = self.session.call_sql_function(func, types, args)?;
                    Ok(sql_result(ret, rows, columns))
                }
            };
        }
        let kernel = kernel(func, None)?;
        let call = Call { session: self.session, args: types, ret, variadic, want: None };
        kernel(&call, args)
    }

    fn func(&mut self, f: &Func, expr: &Expr) -> Result<Value> {
        if let Some(p) = populate::populate_function(f.oid) {
            return Ok(self.populate_call(f, p, &[])?.pop().unwrap_or(Value::Null));
        }
        if !matches!(f.form, FuncForm::ExplicitCast | FuncForm::ImplicitCast)
            && !f.args.iter().any(|a| matches!(a.kind, ExprKind::NamedArg(_)))
            && let Some((kernel, strict, test)) = self.cache.plain_kernel(f.oid, self.session)
        {
            if let Some(test) = test.filter(|_| strict)
                && let Some(value) = self.borrowed_comparison(f, test)
            {
                return Ok(value);
            }
            return self.kernel_call(kernel, strict, f, expr.ty);
        }
        // The planner adds the defaults, so this is for an expression that the planner did not see.
        let exprs = rupg_analyze::expand_arguments(f)?;
        // An operator of two arguments, as the test of a row in `WHERE` has, needs no vectors.
        if let [left, right] = &exprs[..]
            && !matches!(f.form, FuncForm::ExplicitCast | FuncForm::ImplicitCast)
            && f.oid != SATISFIES_HASH_PARTITION
        {
            let args = [self.eval(left)?, self.eval(right)?];
            return self.call(f.oid, &[left.ty, right.ty], expr.ty, f.variadic, &args);
        }
        let args = exprs.iter().map(|a| self.eval(a)).collect::<Result<Vec<_>>>()?;
        let types: Vec<u32> = exprs.iter().map(|a| a.ty).collect();
        if f.oid == SATISFIES_HASH_PARTITION {
            return partition::satisfies_hash_partition(&types, f.variadic, &args, self.session);
        }
        // `pg_column_size` of a column reads the value as the table keeps it, where a short value has a header of 1 byte.
        if f.oid == PG_COLUMN_SIZE
            && let ([arg], [value]) = (&exprs[..], &args[..])
            && matches!(&arg.kind, ExprKind::Var(var) if var.attnum > 0)
            && !value.is_null()
        {
            return rupg_func::column_size(arg.ty, value, true);
        }
        if matches!(f.form, FuncForm::ExplicitCast | FuncForm::ImplicitCast)
            && let Some(ret) = cast_result(f.oid, expr.ty)
        {
            // A cast function can give a type that is binary-compatible with the target, as `int4(smallint)` does for the cast of `int2` to `oid`. The value then takes the target type.
            return Ok(relabel(self.call(f.oid, &types, ret, f.variadic, &args)?, expr.ty));
        }
        self.call(f.oid, &types, expr.ty, f.variadic, &args)
    }

    /// [`Eval::func`] for a function that [`Cache::plain_kernel`] found: the kernel with the values of the arguments, or null for a strict function with a null argument.
    fn kernel_call(&mut self, kernel: Kernel, strict: bool, f: &Func, ret: u32) -> Result<Value> {
        let call = |eval: &Self, types: &[u32], args: &[Value]| {
            if strict && args.iter().any(Value::is_null) {
                return Ok(Value::Null);
            }
            let call =
                Call { session: eval.session, args: types, ret, variadic: f.variadic, want: None };
            kernel(&call, args)
        };
        match &f.args[..] {
            [left, right] => {
                let args = [self.eval(left)?, self.eval(right)?];
                call(self, &[left.ty, right.ty], &args)
            }
            [arg] => {
                let args = [self.eval(arg)?];
                call(self, &[arg.ty], &args)
            }
            exprs => {
                let args = exprs.iter().map(|a| self.eval(a)).collect::<Result<Vec<_>>>()?;
                let types: Vec<u32> = exprs.iter().map(|a| a.ty).collect();
                call(self, &types, &args)
            }
        }
    }

    /// The result of a comparison of a column of the user with a constant, from a reference to the value of the column, or `None` when the values are not both `text` or `bpchar`. Such a comparison is often in a filter of a scan, and a copy of the text for each row costs more than the comparison.
    fn borrowed_comparison(&self, f: &Func, test: OrderingTest) -> Option<Value> {
        let [left, right] = &f.args[..] else { return None };
        fn column(e: &Expr) -> Option<Var> {
            match &e.kind {
                ExprKind::Var(var) if var.levels_up == 0 && var.attnum > 0 => Some(*var),
                _ => None,
            }
        }
        fn constant(e: &Expr) -> Option<&Value> {
            match &e.kind {
                ExprKind::Const(value) => Some(value),
                _ => None,
            }
        }
        // A grouping set can make the column null, and [`Self::eval`] gives the null.
        let nulled = &self.group.nulled;
        if [left, right].iter().any(|a| nulled.contains(&std::ptr::from_ref(*a))) {
            return None;
        }
        let compare = |a: &Value, b: &Value| match (a, b) {
            (Value::Null, _) | (_, Value::Null) => Some(Value::Null),
            (Value::Text(_), Value::Text(_)) => {
                rupg_func::compare(left.ty, a, right.ty, b).ok().map(|o| Value::Bool(test(o)))
            }
            _ => None,
        };
        if let (Some(var), Some(b)) = (column(left), constant(right)) {
            return self.tables.with_column(self.tuple, var, |a| compare(a, b));
        }
        if let (Some(a), Some(var)) = (constant(left), column(right)) {
            return self.tables.with_column(self.tuple, var, |b| compare(a, b));
        }
        None
    }

    /// `AND`, `OR` and `NOT` with the logic of three values. `AND` stops at the first false argument, and `OR` at the first true argument.
    fn bool_op(&mut self, op: BoolOp, args: &[Expr]) -> Result<Value> {
        if op == BoolOp::Not {
            let arg = args.first().ok_or_else(|| Error::internal("NOT needs an argument"))?;
            return Ok(self.eval(arg)?.as_bool().map_or(Value::Null, |v| Value::Bool(!v)));
        }
        let stop = op == BoolOp::Or;
        let mut unknown = false;
        for arg in args {
            match self.eval(arg)?.as_bool() {
                Some(v) if v == stop => return Ok(Value::Bool(stop)),
                Some(_) => {}
                None => unknown = true,
            }
        }
        Ok(if unknown { Value::Null } else { Value::Bool(!stop) })
    }

    /// `ExecEvalConstraintCheck` for each check constraint of the domain, with the value as `VALUE`. A check that gives false is an error, and a check that gives null passes.
    fn domain_check(&mut self, domain: u32, value: Value) -> Result<Value> {
        match self.domain_failure(domain, &value)? {
            Some(error) => Err(error),
            None => Ok(value),
        }
    }

    /// The error of the first check constraint of the domain that gives false for the value, or `None` when no check gives false.
    fn domain_failure(&mut self, domain: u32, value: &Value) -> Result<Option<Error>> {
        let checks = self.cache.domain_checks(domain)?;
        if checks.not_null && value.is_null() {
            let error = Error::new(
                SqlState::NOT_NULL_VIOLATION,
                format!(
                    "domain {} does not allow null values",
                    rupg_func::type_message_name(domain, self.session)
                ),
            );
            return Ok(Some(rupg_func::with_datatype(error, domain, self.session)));
        }
        for (name, check) in &checks.checks {
            self.domain_value.push(value.clone());
            let result = self.eval(check);
            self.domain_value.pop();
            if result?.as_bool() == Some(false) {
                let error = Error::new(
                    SqlState::CHECK_VIOLATION,
                    format!(
                        "value for domain {} violates check constraint \"{name}\"",
                        rupg_func::type_message_name(domain, self.session)
                    ),
                );
                return Ok(Some(rupg_func::with_domain_constraint(
                    error,
                    domain,
                    name,
                    self.session,
                )));
            }
        }
        Ok(None)
    }

    /// `ExecEvalArrayCoerce`: the cast of each element of the array, with the element as the value of `CaseTest`. The result has the dimensions of the array.
    fn array_coerce(&mut self, arg: &Expr, element: &Expr) -> Result<Value> {
        let array = match self.eval(arg)? {
            Value::Null => return Ok(Value::Null),
            Value::Array(array) => *array,
            _ => return Err(Error::internal("the argument of an ArrayCoerceExpr is not an array")),
        };
        let mut values = Vec::with_capacity(array.values.len());
        for value in array.values {
            self.case.push(value.unwrap_or(Value::Null));
            let result = self.eval(element);
            self.case.pop();
            let result = result?;
            values.push((!result.is_null()).then_some(result));
        }
        Ok(Value::Array(Box::new(Array { dims: array.dims, values })))
    }

    /// `ExecEvalSubscriptingRef` for a fetch from an array. A null array gives null before the subscripts run, and a null subscript gives null.
    fn subscript(&mut self, sub: &Subscript) -> Result<Value> {
        if let Some(assign) = &sub.assign {
            return self.subscript_assign(sub, assign);
        }
        if base_type(sub.container.ty) == oid::JSONB {
            return self.jsonb_subscript(sub);
        }
        let array = match self.eval(&sub.container)? {
            Value::Null => return Ok(Value::Null),
            Value::Array(array) => array,
            Value::Text(name) => return self.name_subscript(&name, sub),
            Value::Bytea(bytes) => return self.geo_subscript(&bytes, sub),
            _ => return Err(Error::internal("the container of a SubscriptingRef is not an array")),
        };
        let upper = self.subscript_list(&sub.upper)?;
        let lower = match &sub.lower {
            Some(lower) => Some(self.subscript_list(lower)?),
            None => None,
        };
        let (Some(upper), Some(lower)) = (upper, lower.unwrap_or(Some(Vec::new()))) else {
            return Ok(Value::Null);
        };
        Ok(if sub.lower.is_some() {
            Value::Array(Box::new(array_slice(&array, &lower, &upper)))
        } else {
            array_element(&array, &upper)
        })
    }

    /// `ExecEvalSubscriptingRef` for an assignment to an element or a slice of an array: the array with the new value. A null array is the empty array, and a null subscript is an error. A null value for a slice leaves the array as it is.
    fn subscript_assign(&mut self, sub: &Subscript, assign: &Expr) -> Result<Value> {
        if base_type(sub.container.ty) == oid::JSONB {
            return self.jsonb_subscript_assign(sub, assign);
        }
        let array = match self.eval(&sub.container)? {
            Value::Null if is_geo(sub.container.ty) => {
                // `array_subscript_assign`: a null of a fixed-length type stays null.
                self.assign_subscripts(&sub.upper)?;
                return Ok(Value::Null);
            }
            Value::Null => Array::empty(),
            Value::Array(array) => *array,
            Value::Bytea(bytes) => return self.geo_assign(bytes, sub, assign),
            Value::Text(_) => {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    "an assignment to an element of a fixed-length array is not supported yet",
                ));
            }
            _ => return Err(Error::internal("the container of a SubscriptingRef is not an array")),
        };
        let upper = self.assign_subscripts(&sub.upper)?;
        let lower = match &sub.lower {
            Some(lower) => Some(self.assign_subscripts(lower)?),
            None => None,
        };
        let value = if is_indirection(assign) {
            // `sbsrefoldvalue`: the new value reads the old element or slice as `CaseTest`.
            let old = match &lower {
                Some(lower) => Value::Array(Box::new(array_slice(&array, lower, &upper))),
                None => array_element(&array, &upper),
            };
            self.case.push(old);
            let value = self.eval(assign);
            self.case.pop();
            value?
        } else {
            self.eval(assign)?
        };
        let array = match lower {
            Some(lower) => match value {
                Value::Null => array,
                Value::Array(source) => array.set_slice(&lower, &upper, *source)?,
                _ => return Err(Error::internal("the source of a slice is not an array")),
            },
            None => {
                let upper: Vec<i32> = upper.into_iter().flatten().collect();
                array.set_element(&upper, (!value.is_null()).then_some(value))?
            }
        };
        Ok(Value::Array(Box::new(array)))
    }

    /// `jsonb_subscript_fetch`: the part of a `jsonb` value at the subscripts. A null value gives null before the subscripts run, and a null subscript gives null.
    fn jsonb_subscript(&mut self, sub: &Subscript) -> Result<Value> {
        let source = match self.eval(&sub.container)? {
            Value::Null => return Ok(Value::Null),
            Value::Text(source) => source,
            _ => return Err(Error::internal("the container of a jsonb subscript is not jsonb")),
        };
        let Some(path) = self.jsonb_subscript_path(sub, false)? else { return Ok(Value::Null) };
        let path: Vec<&str> = path.iter().map(String::as_str).collect();
        rupg_func::jsonb_subscript_fetch(&source, &path)
    }

    /// `jsonb_subscript_assign`: the `jsonb` value with the new value at the subscripts. A null value is an empty array when the first subscript is an `int4` and an empty object when it is not. A null new value is the JSON `null`.
    fn jsonb_subscript_assign(&mut self, sub: &Subscript, assign: &Expr) -> Result<Value> {
        let source = match self.eval(&sub.container)? {
            Value::Null => None,
            Value::Text(source) => Some(source),
            _ => return Err(Error::internal("the container of a jsonb subscript is not jsonb")),
        };
        let path = self.jsonb_subscript_path(sub, true)?.unwrap_or_default();
        let path: Vec<&str> = path.iter().map(String::as_str).collect();
        let value = if is_indirection(assign) {
            // `jsonb_subscript_fetch_old`: the new value reads the old part as `CaseTest`.
            let old = match &source {
                Some(source) => rupg_func::jsonb_subscript_fetch(source, &path)?,
                None => Value::Null,
            };
            self.case.push(old);
            let value = self.eval(assign);
            self.case.pop();
            value?
        } else {
            self.eval(assign)?
        };
        let new = match &value {
            Value::Null => "null",
            value => value.as_str().ok_or_else(|| {
                Error::internal("the new value of a jsonb subscript is not jsonb")
            })?,
        };
        let array = matches!(sub.upper.first(), Some(Some(first)) if first.ty == oid::INT4);
        let source = source.as_deref().unwrap_or(if array { "[]" } else { "{}" });
        rupg_func::jsonb_subscript_assign(source, &path, new)
    }

    /// `jsonb_subscript_check_subscripts`: the subscripts of a `jsonb` value as text, or `None` when one of them is null. A null subscript in an assignment is an error.
    fn jsonb_subscript_path(
        &mut self,
        sub: &Subscript,
        assign: bool,
    ) -> Result<Option<Vec<String>>> {
        let mut path = Vec::with_capacity(sub.upper.len());
        for item in &sub.upper {
            let Some(expr) = item else {
                return Err(Error::internal("a jsonb subscript with no upper bound"));
            };
            path.push(match self.eval(expr)? {
                Value::Int4(n) => n.to_string(),
                Value::Null if assign => {
                    return Err(Error::new(
                        SqlState::NULL_VALUE_NOT_ALLOWED,
                        "jsonb subscript in assignment must not be null",
                    ));
                }
                Value::Null => return Ok(None),
                value => value
                    .as_str()
                    .ok_or_else(|| Error::internal("a jsonb subscript is not an int4 or a text"))?
                    .to_owned(),
            });
        }
        Ok(Some(path))
    }

    /// `ExecEvalFieldStoreDeForm` and `ExecEvalFieldStoreForm`: the row with the new values of its fields. Each new value reads the old value of its field as `CaseTest`. A null row is a row whose fields are all null.
    fn field_store(&mut self, store: &FieldStore) -> Result<Value> {
        let mut record = match self.eval(&store.arg)? {
            Value::Record(record) => record,
            Value::Null => {
                let nulls = vec![Value::Null; store.types.len()];
                Box::new(Record::new(store.types.clone(), nulls))
            }
            _ => return Err(Error::internal("the argument of a FieldStore is not a row")),
        };
        for (index, value) in &store.fields {
            let Some(old) = record.values.get(*index).cloned() else {
                return Err(Error::internal("a FieldStore of a field that the row does not have"));
            };
            self.case.push(old);
            let value = self.eval(value);
            self.case.pop();
            record.values[*index] = value?;
        }
        Ok(Value::Record(record))
    }

    /// `array_subscript_check_subscripts` for an assignment: the values of the subscripts of one bound, with `None` for a bound that the query does not give.
    fn assign_subscripts(&mut self, list: &[Option<Expr>]) -> Result<Vec<Option<i32>>> {
        let mut out = Vec::with_capacity(list.len());
        for item in list {
            out.push(match item {
                Some(expr) => match self.eval(expr)? {
                    Value::Int4(n) => Some(n),
                    Value::Null => {
                        return Err(Error::new(
                            SqlState::NULL_VALUE_NOT_ALLOWED,
                            "array subscript in assignment must not be null",
                        ));
                    }
                    _ => return Err(Error::internal("a subscript is not an int4")),
                },
                None => None,
            });
        }
        Ok(out)
    }

    /// A fetch from a `name`, which `raw_array_subscript_handler` reads as a fixed-length array of 64 `"char"` elements from 0. The bytes after the name are zero. A subscript out of the array or more than one subscript gives null. A slice is an error after its subscripts, as in `array_get_slice`.
    fn name_subscript(&mut self, name: &str, sub: &Subscript) -> Result<Value> {
        let Some(upper) = self.subscript_list(&sub.upper)? else { return Ok(Value::Null) };
        if let Some(lower) = &sub.lower {
            if self.subscript_list(lower)?.is_none() {
                return Ok(Value::Null);
            }
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "slices of fixed-length arrays not implemented",
            ));
        }
        let [Some(index)] = upper[..] else { return Ok(Value::Null) };
        match usize::try_from(index) {
            Ok(i) if i <= NAME_MAX_BYTES => {
                Ok(Value::Char(name.as_bytes().get(i).copied().unwrap_or(0)))
            }
            _ => Ok(Value::Null),
        }
    }

    /// A fetch from a `point`, an `lseg`, a `box` or a `line`, which `raw_array_subscript_handler` reads as a fixed-length array from 0 of its `float8` or `point` elements. A subscript out of the array or more than one subscript gives null. A slice is an error after its subscripts, as in `array_get_slice`.
    fn geo_subscript(&mut self, bytes: &[u8], sub: &Subscript) -> Result<Value> {
        let Some(upper) = self.subscript_list(&sub.upper)? else { return Ok(Value::Null) };
        if let Some(lower) = &sub.lower {
            if self.subscript_list(lower)?.is_none() {
                return Ok(Value::Null);
            }
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "slices of fixed-length arrays not implemented",
            ));
        }
        let [Some(index)] = upper[..] else { return Ok(Value::Null) };
        let width = geo_element_width(bytes);
        let Ok(i) = usize::try_from(index) else { return Ok(Value::Null) };
        let Some(element) = bytes.get(i * width..(i + 1) * width) else { return Ok(Value::Null) };
        Ok(geo_element(element))
    }

    /// `array_set_element` and `array_set_slice` for a `point`, an `lseg`, a `box` or a `line`: the value with the new element. A null value leaves the value as it is, as in `array_subscript_assign`.
    fn geo_assign(&mut self, mut bytes: Vec<u8>, sub: &Subscript, assign: &Expr) -> Result<Value> {
        let upper = self.assign_subscripts(&sub.upper)?;
        if let Some(lower) = &sub.lower {
            self.assign_subscripts(lower)?;
        }
        let width = geo_element_width(&bytes);
        let index = match upper[..] {
            [Some(index)] => usize::try_from(index).ok().filter(|i| (i + 1) * width <= bytes.len()),
            _ => None,
        };
        let value = if is_indirection(assign) {
            let old =
                index.map_or(Value::Null, |i| geo_element(&bytes[i * width..(i + 1) * width]));
            self.case.push(old);
            let value = self.eval(assign);
            self.case.pop();
            value?
        } else {
            self.eval(assign)?
        };
        if value.is_null() {
            return Ok(Value::Bytea(bytes));
        }
        if sub.lower.is_some() {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "updates on slices of fixed-length arrays not implemented",
            ));
        }
        if upper.len() != 1 {
            return Err(Error::new(
                SqlState::ARRAY_SUBSCRIPT_ERROR,
                "wrong number of array subscripts",
            ));
        }
        let Some(i) = index else {
            return Err(Error::new(
                SqlState::ARRAY_SUBSCRIPT_ERROR,
                "array subscript out of range",
            ));
        };
        let element = match value {
            Value::Float8(f) => f.to_be_bytes().to_vec(),
            Value::Bytea(point) => point,
            _ => {
                return Err(Error::internal(
                    "the element of a geometric value is not a float8 or a point",
                ));
            }
        };
        if element.len() != width {
            return Err(Error::internal("the element of a geometric value has the wrong length"));
        }
        bytes[i * width..(i + 1) * width].copy_from_slice(&element);
        Ok(Value::Bytea(bytes))
    }

    /// The values of the subscripts of one bound, with `None` for a bound that the query does not give. A null subscript gives `None` for the list.
    fn subscript_list(&mut self, list: &[Option<Expr>]) -> Result<Option<Vec<Option<i32>>>> {
        let mut out = Vec::with_capacity(list.len());
        let mut null = false;
        for item in list {
            out.push(match item {
                Some(expr) => match self.eval(expr)? {
                    Value::Int4(n) => Some(n),
                    Value::Null => {
                        null = true;
                        None
                    }
                    _ => return Err(Error::internal("a subscript is not an int4")),
                },
                None => None,
            });
        }
        Ok(if null { None } else { Some(out) })
    }

    /// `ExecEvalJsonConstructor`: the `json` or `jsonb` value of a SQL/JSON constructor, then its cast to the type of `RETURNING`.
    fn json_ctor(&mut self, c: &JsonCtor) -> Result<Value> {
        let jsonb = c.returning.format.kind == JsonFormatKind::Jsonb;
        let first = || {
            c.args.first().ok_or_else(|| Error::internal("a SQL/JSON constructor with no argument"))
        };
        let value = match c.kind {
            JsonCtorKind::Parse if c.unique => match self.eval(first()?)? {
                Value::Null => Value::Null,
                v => {
                    let text = v
                        .as_str()
                        .ok_or_else(|| Error::internal("JSON of a value that is not text"))?;
                    rupg_func::json_ctor_parse(text, jsonb)?
                }
            },
            JsonCtorKind::Parse | JsonCtorKind::Serialize => self.eval(first()?)?,
            JsonCtorKind::ObjectAgg | JsonCtorKind::ArrayAgg | JsonCtorKind::ArrayQuery => {
                let func = c.func.as_ref();
                self.eval(
                    func.ok_or_else(|| Error::internal("a SQL/JSON aggregate with no call"))?,
                )?
            }
            JsonCtorKind::Scalar => {
                let arg = first()?;
                let v = self.eval(arg)?;
                rupg_func::json_ctor_scalar(arg.ty, &v, jsonb, self.session)?
            }
            JsonCtorKind::Object | JsonCtorKind::Array => {
                let mut args = Vec::with_capacity(c.args.len());
                for arg in &c.args {
                    args.push((arg.ty, self.eval(arg)?));
                }
                if c.kind == JsonCtorKind::Object {
                    rupg_func::json_ctor_object(
                        &args,
                        jsonb,
                        c.absent_on_null,
                        c.unique,
                        self.session,
                    )?
                } else {
                    rupg_func::json_ctor_array(&args, jsonb, c.absent_on_null, self.session)?
                }
            }
        };
        let Some(coercion) = &c.coercion else { return Ok(value) };
        self.case.push(value);
        let result = self.eval(coercion);
        self.case.pop();
        result
    }

    /// `ExecEvalJsonIsPredicate`: null for a null value.
    fn is_json(&mut self, p: &IsJson) -> Result<Value> {
        let v = self.eval(&p.expr)?;
        if v.is_null() {
            return Ok(Value::Null);
        }
        let text =
            v.as_str().ok_or_else(|| Error::internal("IS JSON of a value that is not text"))?;
        Ok(Value::Bool(rupg_func::is_json(text, p.base_type, p.item, p.unique)))
    }

    fn case(&mut self, case: &Case) -> Result<Value> {
        let has_arg = case.arg.is_some();
        if let Some(arg) = &case.arg {
            let value = self.eval(arg)?;
            self.case.push(value);
        }
        let result = self.case_whens(case);
        if has_arg {
            self.case.pop();
        }
        result
    }

    fn case_whens(&mut self, case: &Case) -> Result<Value> {
        for (when, then) in &case.whens {
            if self.eval(when)? == Value::Bool(true) {
                return self.eval(then);
            }
        }
        self.eval(&case.default)
    }

    /// `GREATEST` and `LEAST`: the null arguments do not count.
    fn min_max(&mut self, greatest: bool, less: u32, args: &[Expr]) -> Result<Value> {
        let mut result = Value::Null;
        let mut ty = 0;
        for arg in args {
            let value = self.eval(arg)?;
            if value.is_null() {
                continue;
            }
            if result.is_null() {
                (result, ty) = (value, arg.ty);
                continue;
            }
            let (types, pair) = if greatest {
                ([ty, arg.ty], [result.clone(), value.clone()])
            } else {
                ([arg.ty, ty], [value.clone(), result.clone()])
            };
            if self.call(less, &types, oid::BOOL, false, &pair)? == Value::Bool(true) {
                (result, ty) = (value, arg.ty);
            }
        }
        Ok(result)
    }

    /// `EEOP_ROWCOMPARE_STEP` and `EEOP_ROWCOMPARE_FINAL`: the comparison function of each pair of fields, until a pair is not equal. The result of the last comparison decides. A null field in a pair that the executor compares gives null.
    fn row_compare(&mut self, rc: &RowCompare) -> Result<Value> {
        let mut order = 0;
        for (op, &compare) in rc.ops.iter().zip(&rc.compares) {
            let ExprKind::Func(Func { args, .. }) = &op.kind else {
                return Err(Error::internal("a row comparison that is not an operator"));
            };
            let [left, right] = args.as_slice() else {
                return Err(Error::internal("a row comparison operator needs two arguments"));
            };
            let pair = [self.eval(left)?, self.eval(right)?];
            order = match self.call(compare, &[left.ty, right.ty], oid::INT4, false, &pair)? {
                Value::Int4(order) => order,
                Value::Null => return Ok(Value::Null),
                _ => return Err(Error::internal("a comparison function that is not int4")),
            };
            if order != 0 {
                break;
            }
        }
        Ok(Value::Bool(rc.kind.test(order)))
    }

    /// `x op ANY (array)` and `x op ALL (array)`, as `ExecEvalScalarArrayOp` gives them.
    fn scalar_array_op(&mut self, func: u32, any: bool, args: &[Expr]) -> Result<Value> {
        let [left, right] = args else {
            return Err(Error::internal("ANY and ALL need two arguments"));
        };
        let scalar = self.eval(left)?;
        let array = match self.eval(right)? {
            Value::Null => return Ok(Value::Null),
            Value::Array(array) => array,
            _ => return Err(Error::internal("ANY and ALL need an array")),
        };
        if array.values.is_empty() {
            return Ok(Value::Bool(!any));
        }
        let is_strict = strict(func);
        if scalar.is_null() && is_strict {
            return Ok(Value::Null);
        }
        let element = builtin::type_by_oid(base_type(right.ty)).map_or(0, |row| row.elem);
        let types = [left.ty, element];
        let mut unknown = false;
        for item in &array.values {
            let result = match item {
                None if is_strict => None,
                item => {
                    let item = item.clone().unwrap_or(Value::Null);
                    self.call(func, &types, oid::BOOL, false, &[scalar.clone(), item])?.as_bool()
                }
            };
            match result {
                Some(v) if v == any => return Ok(Value::Bool(any)),
                Some(_) => {}
                None => unknown = true,
            }
        }
        Ok(if unknown { Value::Null } else { Value::Bool(!any) })
    }

    /// `ARRAY[...]`, as `ExecEvalArrayExpr` gives it.
    fn array(&mut self, multidims: bool, elements: &[Expr]) -> Result<Value> {
        let values = elements.iter().map(|e| self.eval(e)).collect::<Result<Vec<_>>>()?;
        if !multidims {
            let values = values.into_iter().map(|v| (!v.is_null()).then_some(v)).collect();
            return Ok(Value::Array(Box::new(Array::one(values))));
        }
        let mismatch = || {
            Error::new(
                SqlState::ARRAY_SUBSCRIPT_ERROR,
                "multidimensional arrays must have array expressions with matching dimensions",
            )
        };
        let mut inner: Option<Vec<ArrayDim>> = None;
        let mut empty = false;
        let mut items = Vec::new();
        let mut count = 0;
        for value in values {
            let array = match value {
                Value::Null => {
                    empty = true;
                    continue;
                }
                Value::Array(array) => array,
                _ => {
                    return Err(Error::internal(
                        "a part of a multidimensional ARRAY is not an array",
                    ));
                }
            };
            if array.dims.is_empty() {
                empty = true;
                continue;
            }
            match &inner {
                None => {
                    if array.dims.len() + 1 > MAXDIM {
                        return Err(Error::new(
                            SqlState::PROGRAM_LIMIT_EXCEEDED,
                            format!(
                                "number of array dimensions ({}) exceeds the maximum allowed ({MAXDIM})",
                                array.dims.len() + 1
                            ),
                        ));
                    }
                    inner = Some(array.dims.clone());
                }
                Some(dims) if *dims != array.dims => return Err(mismatch()),
                Some(_) => {}
            }
            items.extend(array.values);
            count += 1;
        }
        let Some(inner) = inner else { return Ok(Value::Array(Box::new(Array::empty()))) };
        if empty {
            return Err(mismatch());
        }
        let mut dims = vec![ArrayDim { len: count, lower: 1 }];
        dims.extend(inner);
        Ok(Value::Array(Box::new(Array { dims, values: items })))
    }

    /// `CURRENT_DATE`, `CURRENT_TIMESTAMP`, `CURRENT_USER` and the other functions of SQL without parentheses.
    fn sql_value(&self, v: SqlValue) -> Result<Value> {
        let session = self.session;
        let now = session.transaction_start();
        let precision = |p: Option<i32>, t: i64| -> Result<i64> {
            match p {
                Some(p) => rupg_types::adjust_timestamp(t, p).map_err(rupg_func::type_error),
                None => Ok(t),
            }
        };
        let time = |p: Option<i32>, t: i64| match p {
            Some(p) => rupg_types::adjust_time(t, p),
            None => t,
        };
        let day = rupg_types::USECS_PER_DAY;
        Ok(match v {
            SqlValue::CurrentDate => {
                let (local, _) = rupg_func::local_time(session, now)?;
                Value::Date(
                    i32::try_from(local.div_euclid(day))
                        .map_err(|_| Error::internal("the date is out of range"))?,
                )
            }
            SqlValue::CurrentTime(p) => {
                let (local, offset) = rupg_func::local_time(session, now)?;
                Value::TimeTz(time(p, local.rem_euclid(day)), -offset)
            }
            SqlValue::CurrentTimestamp(p) => Value::TimestampTz(precision(p, now)?),
            SqlValue::LocalTime(p) => {
                let (local, _) = rupg_func::local_time(session, now)?;
                Value::Time(time(p, local.rem_euclid(day)))
            }
            SqlValue::LocalTimestamp(p) => {
                let (local, _) = rupg_func::local_time(session, now)?;
                Value::Timestamp(precision(p, local)?)
            }
            SqlValue::CurrentRole | SqlValue::CurrentUser | SqlValue::User => {
                Value::text(session.user())
            }
            SqlValue::SessionUser => Value::text(session.session_user()),
            SqlValue::CurrentCatalog => Value::text(session.database()),
            SqlValue::CurrentSchema => {
                session.schemas().into_iter().next().map_or(Value::Null, Value::Text)
            }
        })
    }
}

/// `evaluate_expr`: the value of an expression whose arguments are constants, such as a value of the bound of a partition.
///
/// # Errors
///
/// The error of a function of the expression.
pub fn evaluate(expr: &Expr, session: &dyn Session) -> Result<Value> {
    let tables = scan::Tables::default();
    let cache = Cache::default();
    let mut eval = Eval {
        params: &[],
        session,
        case: Vec::new(),
        tables: &tables,
        tuple: &[],
        group: Group::default(),
        cache: &cache,
        outer: &[],
        sub: Vec::new(),
        domain_value: Vec::new(),
        sets: Vec::new(),
        windows: Vec::new(),
        multi: Vec::new(),
    };
    eval.eval(expr)
}

/// The value of the check expression `expr` of a domain for the value `value` of a column, as `validateDomainCheckConstraint` sets `domainValue_datum` for each row.
///
/// # Errors
///
/// The error of a function of the expression.
pub fn evaluate_domain_check(expr: &Expr, value: &Value, session: &dyn Session) -> Result<Value> {
    let tables = scan::Tables::default();
    let cache = Cache::default();
    let mut eval = Eval {
        params: &[],
        session,
        case: Vec::new(),
        tables: &tables,
        tuple: &[],
        group: Group::default(),
        cache: &cache,
        outer: &[],
        sub: Vec::new(),
        domain_value: vec![value.clone()],
        sets: Vec::new(),
        windows: Vec::new(),
        multi: Vec::new(),
    };
    eval.eval(expr)
}

/// `FormIndexDatum` for a unique index with an expression or a predicate: the values of the key columns of a row of the table, or `None` when the row does not pass the predicate and is not in the index, as `ExecInsertIndexTuples` checks it.
///
/// # Errors
///
/// The error of a function of an expression or of the predicate.
pub fn index_key(
    derived: &rupg_analyze::Derived,
    row: &[Value],
    session: &dyn Session,
) -> Result<Option<Vec<Value>>> {
    let (table, types) = (derived.table, derived.types.as_slice());
    if !derived.predicate.is_empty() {
        let passed = evaluate_row(&derived.predicate, table, types, row, session)?;
        if !passed.iter().all(|v| matches!(v, Value::Bool(true))) {
            return Ok(None);
        }
    }
    evaluate_row(&derived.exprs, table, types, row, session).map(Some)
}

/// The test of `check_exclusion_or_unique_constraint` for two keys of an exclusion constraint: the operator of each key column, with the key of the existing version on the left, gives true. A key with a null conflicts with no key.
///
/// # Errors
///
/// The error of an operator.
pub fn exclusion_conflict(
    exclusion: &rupg_analyze::Exclusion,
    new: &[Value],
    old: &[Value],
    session: &dyn Session,
) -> Result<bool> {
    if new.iter().chain(old).any(|v| matches!(v, Value::Null)) {
        return Ok(false);
    }
    let parts = exclusion.operators.iter().zip(&exclusion.functions).zip(&exclusion.keys.exprs);
    for (((&op, &function), key), (new, old)) in parts.zip(new.iter().zip(old)) {
        let arg = |value: &Value| Expr::constant(value.clone(), key.ty, key.typmod, None);
        let call = Expr::new(
            ExprKind::Func(Func {
                oid: function,
                args: vec![arg(old), arg(new)],
                form: FuncForm::Operator(op),
                variadic: false,
                retset: false,
            }),
            oid::BOOL,
        );
        if !matches!(evaluate(&call, session)?, Value::Bool(true)) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The sort of `copy_table_data` for `CLUSTER` and `REPACK` on a `btree` index: the rows of the table in the order of the keys of the index, as `tuplesort_begin_cluster` sorts them. Each row has the ID of its version at the end, and a row from before an `ADD COLUMN` has no value for the new columns, which are null in the keys. Rows with equal keys keep their physical order, as the scan of the index gives them.
///
/// # Errors
///
/// The error of a function of an expression of the index or of a comparison.
pub fn cluster_order(
    order: &rupg_analyze::Order,
    rows: Vec<Vec<Value>>,
    session: &dyn Session,
) -> Result<Vec<Vec<Value>>> {
    let derived = &order.keys;
    let width = derived.types.len();
    let mut keyed = Vec::with_capacity(rows.len());
    for (at, row) in rows.into_iter().enumerate() {
        let stored = row.len().saturating_sub(1).min(width);
        let mut values = row[..stored].to_vec();
        values.resize(width, Value::Null);
        let key = evaluate_row(&derived.exprs, derived.table, &derived.types, &values, session)?;
        keyed.push((key, at, row));
    }
    let types: Vec<u32> = derived.exprs.iter().map(|e| e.ty).collect();
    let keys = sort::Keys::new(&order.sort, &types, session)?;
    sort::qsort(&mut keyed, &mut |a, b| Ok(keys.compare(&a.0, &b.0)?.then(a.1.cmp(&b.1))))?;
    Ok(keyed.into_iter().map(|(_, _, row)| row).collect())
}

/// The values of expressions for one row of the relation `oid`, whose columns have the types `types`, such as the expressions of a partition key. A variable of an expression reads the column of the row.
pub(crate) fn evaluate_row(
    exprs: &[Expr],
    oid: u32,
    types: &[u32],
    row: &[Value],
    session: &dyn Session,
) -> Result<Vec<Value>> {
    let tables = scan::Tables::one_row(oid, types.to_vec(), row.to_vec());
    let cache = Cache::default();
    let mut eval = Eval {
        params: &[],
        session,
        case: Vec::new(),
        tables: &tables,
        tuple: &[0],
        group: Group::default(),
        cache: &cache,
        outer: &[],
        sub: Vec::new(),
        domain_value: Vec::new(),
        sets: Vec::new(),
        windows: Vec::new(),
        multi: Vec::new(),
    };
    exprs.iter().map(|expr| eval.eval(expr)).collect()
}

/// The checks that the input function of a domain or of an array of a domain makes: `domain_in` checks the value with the check constraints of the domain, and `array_in` checks each element in the same way. A value of another type passes.
///
/// # Errors
///
/// `23514` for a value that a check constraint rejects, and the errors of the checks.
pub fn check_input(ty: u32, value: &Value, session: &dyn Session) -> Result<()> {
    match input_failure(ty, value, session)? {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// The checks of [`check_input`] as `domain_check_safe` makes them: the error of the first check constraint that rejects the value is a soft error, and `None` means that the value passes. A null value of a domain must pass the not-null constraint and the check constraints of the domain, as `domain_in` checks it for a null input.
///
/// # Errors
///
/// The errors of the check expressions.
pub fn input_failure(ty: u32, value: &Value, session: &dyn Session) -> Result<Option<Error>> {
    let element = builtin::type_by_oid(ty).map_or(0, |row| row.elem);
    // `range_in` calls the input function of the subtype for each bound that is not infinite, and `multirange_in` calls `range_in` for each range.
    fn bounds(range: &rupg_types::Range) -> impl Iterator<Item = &Value> {
        [&range.lower, &range.upper].into_iter().filter(|v| !v.is_null())
    }
    let range_sub = builtin::range_subtype(ty)
        .or_else(|| builtin::multirange_range(ty).and_then(builtin::range_subtype))
        .filter(|&sub| builtin::domain_by_oid(sub).is_some());
    let (domain, values) = match value {
        Value::Array(array) if builtin::domain_by_oid(element).is_some() => {
            (element, array.values.iter().flatten().collect())
        }
        Value::Range(range) if let Some(sub) = range_sub => (sub, bounds(range).collect()),
        Value::Multirange(ranges) if let Some(sub) = range_sub => {
            (sub, ranges.iter().flat_map(bounds).collect())
        }
        value if builtin::domain_by_oid(ty).is_some() => (ty, vec![value]),
        _ => return Ok(None),
    };
    let tables = scan::Tables::default();
    let cache = Cache::default();
    let mut eval = Eval {
        params: &[],
        session,
        case: Vec::new(),
        tables: &tables,
        tuple: &[],
        group: Group::default(),
        cache: &cache,
        outer: &[],
        sub: Vec::new(),
        domain_value: Vec::new(),
        sets: Vec::new(),
        windows: Vec::new(),
        multi: Vec::new(),
    };
    for value in values {
        if let Some(error) = eval.domain_failure(domain, value)? {
            return Ok(Some(error));
        }
    }
    Ok(None)
}

/// The result type of the cast function `func` when it is not the base type of the target `ty`, such as `int4` for the cast of `int2` to `oid`.
fn cast_result(func: u32, ty: u32) -> Option<u32> {
    let ret = builtin::proc_by_oid(func)?.rettype;
    let pseudo = builtin::type_by_oid(ret).is_none_or(|t| t.kind == b'p');
    (ret != base_type(ty) && !pseudo).then_some(ret)
}

/// A value as a binary-compatible type holds it: an `int4` as an `oid` keeps its bits, and an `oid` as an `int4` too.
#[allow(clippy::cast_sign_loss, clippy::cast_possible_wrap)]
fn relabel(value: Value, ty: u32) -> Value {
    let ty = base_type(ty);
    // A cast of `CREATE CAST ... WITHOUT FUNCTION` keeps the bits of the value, also between an integer type and a float type.
    let ty = builtin::base_representation(ty).unwrap_or(ty);
    match value {
        Value::Int4(v) if ty == oid::FLOAT4 => Value::Float4(f32::from_bits(v as u32)),
        Value::Float4(v) if ty == oid::INT4 => Value::Int4(v.to_bits() as i32),
        Value::Int8(v) if ty == oid::FLOAT8 => Value::Float8(f64::from_bits(v as u64)),
        Value::Float8(v) if ty == oid::INT8 => Value::Int8(v.to_bits() as i64),
        Value::Int4(v) if ty != oid::INT4 => Value::Oid(v as u32),
        Value::Oid(v) if ty == oid::INT4 => Value::Int4(v as i32),
        Value::Array(mut array) => {
            // The elements of an array take the element type, as an array cast with a relabel of each element does.
            let element = builtin::type_by_oid(ty).map_or(0, |row| row.elem);
            if element != 0 {
                for value in array.values.iter_mut().flatten() {
                    *value = relabel(std::mem::replace(value, Value::Null), element);
                }
            }
            Value::Array(array)
        }
        value => value,
    }
}

mod agg;
mod analyze;
mod columnar;
mod cursor;
mod fold;
mod hashed;
mod insert;
mod jsonexpr;
mod jsontable;
mod partition;
mod populate;
mod scan;
mod sort;
mod user;
mod window;

pub use analyze::analyze;
pub use insert::{failing_row, row_types};
pub use partition::{Router, leaves as partition_leaves};
pub use scan::members;
pub use user::variadic_type;

#[cfg(test)]
mod tests;

/// Whether `ty` is `point`, `lseg`, `box` or `line`, or a domain over one of them: a type of fixed length whose subscripts `raw_array_subscript_handler` reads.
/// The OID of `pg_column_size`.
const PG_COLUMN_SIZE: u32 = 1269;

/// The OID of `satisfies_hash_partition`, which reads the partition key of the table. [`partition::satisfies_hash_partition`] runs it with no kernel.
const SATISFIES_HASH_PARTITION: u32 = 5028;

fn is_geo(ty: u32) -> bool {
    matches!(base_type(ty), oid::POINT | oid::LSEG | oid::BOX | oid::LINE)
}

/// The length of an element of a geometric value that subscripts read: the 16 bytes of a `point` in an `lseg` or a `box`, which have 32 bytes, or the 8 bytes of a `float8` in a `point` or a `line`.
fn geo_element_width(bytes: &[u8]) -> usize {
    if bytes.len() == 32 { 16 } else { 8 }
}

/// The element of a geometric value in its bytes: a `point` or a `float8`.
fn geo_element(bytes: &[u8]) -> Value {
    match <[u8; 8]>::try_from(bytes) {
        Ok(float) => Value::Float8(f64::from_be_bytes(float)),
        Err(_) => Value::Bytea(bytes.to_vec()),
    }
}

/// `isAssignmentIndirectionExpr`: whether the new value of an assignment is an assignment to a part of the old value, which it reads as `CaseTest`.
fn is_indirection(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::FieldStore(_) => true,
        ExprKind::Subscript(sub) => sub.assign.is_some(),
        ExprKind::CoerceToDomain(arg, _) => is_indirection(arg),
        _ => false,
    }
}

/// `array_get_element`: the element at the subscripts, or null when the number of subscripts is not the number of dimensions or a subscript is outside its dimension.
fn array_element(array: &Array<Value>, subscripts: &[Option<i32>]) -> Value {
    if subscripts.len() != array.dims.len() {
        return Value::Null;
    }
    let mut offset = 0usize;
    for (dim, at) in array.dims.iter().zip(subscripts) {
        let at = i64::from(at.unwrap_or(dim.lower));
        let (lower, len) = (i64::from(dim.lower), i64::from(dim.len));
        if at < lower || at >= lower + len {
            return Value::Null;
        }
        let (Ok(step), Ok(len)) = (usize::try_from(at - lower), usize::try_from(len)) else {
            return Value::Null;
        };
        offset = offset * len + step;
    }
    array.values.get(offset).cloned().flatten().unwrap_or(Value::Null)
}

/// `array_get_slice`: the elements between the bounds, which the bounds of the array limit. A bound that the query does not give is the bound of the array, and a dimension with no subscripts is the full dimension. The result has the lower bound 1 in each dimension. A slice with no elements, or with more subscripts than the array has dimensions, is the empty array.
fn array_slice(array: &Array<Value>, lower: &[Option<i32>], upper: &[Option<i32>]) -> Array<Value> {
    let ndim = array.dims.len();
    if ndim < upper.len() || ndim == 0 {
        return Array::empty();
    }
    let mut ranges = Vec::with_capacity(ndim);
    for (i, dim) in array.dims.iter().enumerate() {
        let (first, past) = (i64::from(dim.lower), i64::from(dim.lower) + i64::from(dim.len));
        let from = lower.get(i).copied().flatten().map_or(first, |n| i64::from(n).max(first));
        let to = upper.get(i).copied().flatten().map_or(past - 1, |n| i64::from(n).min(past - 1));
        if from > to {
            return Array::empty();
        }
        ranges.push((from - first, to - from + 1));
    }
    let mut dims = Vec::with_capacity(ndim);
    for &(_, len) in &ranges {
        dims.push(ArrayDim { len: i32::try_from(len).unwrap_or(i32::MAX), lower: 1 });
    }
    let mut values = Vec::new();
    slice_values(array, &ranges, 0, 0, &mut values);
    Array { dims, values }
}

/// Pushes the elements of the slice in the dimensions from `level`, in the order of storage. `base` is the offset of the first element of the part of the array at `level`.
fn slice_values(
    array: &Array<Value>,
    ranges: &[(i64, i64)],
    level: usize,
    base: usize,
    out: &mut Vec<Option<Value>>,
) {
    let Some(&(start, len)) = ranges.get(level) else {
        out.push(array.values.get(base).cloned().flatten());
        return;
    };
    let size = usize::try_from(array.dims[level].len).unwrap_or(0);
    for step in start..start + len {
        let step = usize::try_from(step).unwrap_or(0);
        slice_values(array, ranges, level + 1, base * size + step, out);
    }
}
