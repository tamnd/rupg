//! The vectorized executor, the point path, compressed execution, spill, the per-distinct result tables of document 14 section 14.6.
//!
//! This version reads the tables of the catalog. It joins the parts of `FROM`, tests the `WHERE` clause and computes the target list for each row, with the values of the functions that give a set as the node `ProjectSet` gives them. Then it applies `DISTINCT`, `ORDER BY`, `OFFSET` and `LIMIT` as the plan of PostgreSQL does with the nodes `Sort`, `Unique` and `Limit`. [`prepare`] checks that the engine has every function and every type of the query, so that a query that the engine cannot run is an error `0A000` before it runs. [`Plan::run`] then computes the row as `ExecInterpExpr` of PostgreSQL does.

#![forbid(unsafe_code)]

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;
use std::sync::Arc;

use rupg_analyze::{
    Aggref, Body, BoolOp, BoolTest, Case, Expr, ExprKind, FromFunction, FromItem, Func, Query,
    RowCompare, SetKind, SetOp, SetTree, SortGroup, SqlValue, SubLink, SubLinkKind, Subscript,
    Target, WindowFunc,
};
use rupg_common::{Error, Result, SqlState};
use rupg_func::{Call, Kernel, Session, base_type};
use rupg_pgcatalog::builtin;
use rupg_types::{Array, ArrayDim, MAXDIM, NAME_MAX_BYTES, Record, Value, format_type, oid};

/// A query that the engine can run.
#[derive(Debug)]
pub struct Plan {
    query: Query,
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

/// True for a built-in function with a body in SQL that has no kernel. The engine runs its body, as for a function that a user makes.
fn runs_body(func: u32) -> bool {
    builtin::proc_by_oid(func).is_some_and(|p| p.sqlbody.is_some())
        && rupg_func::kernel(func).is_none()
        && rupg_func::set_kernel(func).is_none()
}

/// True when a call in `FROM` needs the set kernel of the function: the function gives a set, or a row with `width` columns.
fn table_function(func: u32, width: usize) -> bool {
    width > 1 || builtin::proc_by_oid(func).is_some_and(|p| p.retset)
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
            if !builtin::is_system_proc(f.oid) && !runs_body(f.oid) {
                set_kernel(f.oid, expr.location)?;
            }
            f.args.iter().try_for_each(check)?;
        }
        ExprKind::Func(f) => {
            // The engine runs the body of a function in SQL, so such a function needs no kernel.
            if !builtin::is_system_proc(f.oid) && !runs_body(f.oid) {
                kernel(f.oid, expr.location)?;
            }
            f.args.iter().try_for_each(check)?;
        }
        ExprKind::NamedArg(na) => check(&na.arg)?,
        ExprKind::Relabel(arg, _)
        | ExprKind::CoerceToDomain(arg, _)
        | ExprKind::NullTest(arg, _)
        | ExprKind::BooleanTest(arg, _)
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
            sub.bounds().try_for_each(check)?;
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
            kernel(*func, expr.location)?;
            args.iter().try_for_each(check)?;
        }
        ExprKind::Array { elements, .. } | ExprKind::Row(elements, _) => {
            elements.iter().try_for_each(check)?;
        }
        // The executor calls the comparison function of each pair of fields, not the operator.
        ExprKind::RowCompare(rc) => {
            for (op, &compare) in rc.ops.iter().zip(&rc.compares) {
                kernel(compare, expr.location)?;
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

/// Checks a query and makes the plan to run it.
///
/// # Errors
///
/// `0A000` for a function or a type that the engine does not have yet.
pub fn prepare(query: Query) -> Result<Plan> {
    rupg_analyze::check_outer_joins(&query)?;
    check_query(&query, true)?;
    Ok(Plan { query })
}

/// Checks that the engine can run a query or a subquery. With `output`, it also checks that the engine has the output function of each column.
fn check_query(query: &Query, output: bool) -> Result<()> {
    // `create_ordinary_grouping_paths`: a `GROUP BY` sorts the rows when every type has an order, or else uses a hash table. The planner gives this error before the engine checks its functions.
    if !query.group.iter().all(|g| g.sort != 0) && !query.group.iter().all(|g| g.hashable) {
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
        for (call, width) in relation.function.iter().flat_map(|f| &f.calls) {
            match &call.kind {
                ExprKind::Func(f) if table_function(f.oid, *width) => {
                    if !builtin::is_system_proc(f.oid) && !runs_body(f.oid) && !row_function(f.oid)
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

/// The rows of a part of a set operation. A `SELECT` gives the rows that `leaf` gives for it.
fn part_rows(part: &SetTree, leaf: &mut Leaf<'_>, session: &dyn Session) -> Result<Rows> {
    match part {
        SetTree::Leaf(index) => leaf(*index),
        SetTree::Op(op) => set_rows(op, leaf, session),
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
) -> Result<Rows> {
    let types: Vec<u32> = op.columns.iter().map(|(ty, _)| *ty).collect();
    let keys = sort::Keys::new(&op.groups, &types, run.session)?;
    let mut seen = Vec::new();
    let mut leaf = |i: usize| Ok((*tables.rows[i]).clone());
    let mut step = part_rows(&op.left, &mut leaf, run.session)?;
    if !op.all {
        step = new_rows(&keys, &mut seen, step)?;
    }
    let mut frames = run.outer.to_vec();
    frames.push(Frame { tables, tuple: &[] });
    let mut rows = Vec::new();
    while !step.is_empty() {
        run.session.interrupted()?;
        rows.extend(step.iter().cloned());
        run.cache.work.borrow_mut().push((work, Rc::new(step)));
        let mut leaf = |i: usize| {
            let Some(sub) = &query.relations[i].subquery else {
                return Err(Error::internal("a SELECT of a set operation with no query"));
            };
            run_query(sub, run.params, run.session, run.cache, &frames, false)
        };
        let next = part_rows(&op.right, &mut leaf, run.session);
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

/// The rows of an operation of a set operation, as `Append`, `Unique` and `SetOp` give them. `UNION ALL` gives the rows of the left part, then the rows of the right part. The other operations make groups of equal rows: they sort the rows when every type has an order, or else keep the groups in the order of the rows, as a hash table does. For each group, `UNION` gives one row, `INTERSECT` gives one row when both parts have the row, and `EXCEPT` gives one row when only the left part has it. With `ALL`, `INTERSECT` gives the row as many times as the part with fewer copies has it, and `EXCEPT` gives it as many times as the left part has more copies than the right part.
fn set_rows(op: &SetOp, leaf: &mut Leaf<'_>, session: &dyn Session) -> Result<Rows> {
    let left = part_rows(&op.left, leaf, session)?;
    let right = part_rows(&op.right, leaf, session)?;
    if op.kind == SetKind::Union && op.all {
        let mut rows = left;
        rows.extend(right);
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
        let copies = match (op.kind, op.all) {
            (SetKind::Union, _) => 1,
            (SetKind::Intersect, true) => left.min(right),
            (SetKind::Intersect, false) => usize::from(left > 0 && right > 0),
            (SetKind::Except, true) => left.saturating_sub(right),
            (SetKind::Except, false) => usize::from(left > 0 && right == 0),
        };
        for _ in 0..copies {
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

/// The body of each function in SQL, by the OID of the function and the types of the arguments of the call, or `None` for a function of the catalog that is not in SQL.
type Bodies = Rc<RefCell<Vec<((u32, Vec<u32>), Option<Rc<Body>>)>>>;

/// The state of a run that the query and its subqueries share.
#[derive(Default)]
struct Cache {
    /// The rows of the relations of each query, by the address of the query. A subquery reads them once, also when it runs for each row of the query outside it.
    tables: RefCell<Vec<(*const Query, Rc<scan::Tables>)>>,
    /// The rows of each subquery that reads no column of an outer query, as an `InitPlan` keeps them. It runs once, when the query first needs it.
    rows: RefCell<Vec<(*const Query, Rc<Rows>)>>,
    /// The hash table of each such subquery of `ANY`, as a hashed `SubPlan` keeps it, or `None` when a value of the subquery has no key.
    hashed: RefCell<Vec<(*const Query, Kept)>>,
    /// The name and the expression of each check constraint of a domain, by the OID of the domain, as the type cache keeps them.
    domains: RefCell<Vec<(u32, Checks)>>,
    /// The body of each function in SQL that the run calls. The run of a body shares them.
    bodies: Bodies,
    /// The rows of the work table of each recursive query of `WITH` that runs now, by the number of its work table. The last entry with a number is the one that the reference reads.
    work: RefCell<Vec<(usize, Rc<Rows>)>>,
}

impl Cache {
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
        if session.catalog().and_then(|c| c.function(func)).is_none() && !runs_body(func) {
            return Ok(None);
        }
        let kept = self
            .bodies
            .borrow()
            .iter()
            .find(|((f, t), _)| *f == func && t == inputs)
            .map(|(_, body)| body.clone());
        if let Some(body) = kept {
            return Ok(body);
        }
        let body = rupg_func::function_body(func, inputs, session)?.map(Rc::new);
        self.bodies.borrow_mut().push(((func, inputs.to_vec()), body.clone()));
        Ok(body)
    }

    fn tables(&self, query: &Query, session: &dyn Session) -> Result<Rc<scan::Tables>> {
        if let Some((_, tables)) =
            self.tables.borrow().iter().find(|(q, _)| std::ptr::eq(*q, query))
        {
            return Ok(Rc::clone(tables));
        }
        let tables = Rc::new(scan::read(query, session)?);
        self.tables.borrow_mut().push((query, Rc::clone(&tables)));
        Ok(tables)
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
}

/// True when a subquery reads a column of a query outside it, so that it must run again for each row of that query.
fn correlated(query: &Query) -> bool {
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
fn volatile(expr: &Expr) -> bool {
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
) -> Result<Rc<scan::Tables>> {
    let work = |r: &rupg_analyze::Relation| r.cte.as_ref().and_then(|c| c.work);
    if query.relations.iter().all(|r| {
        r.subquery.is_none() && r.function.is_none() && r.values.is_none() && work(r).is_none()
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
    frames.push(Frame { tables: base, tuple: &[] });
    for (i, relation) in query.relations.iter().enumerate() {
        if let Some(id) = work(relation) {
            let rows =
                cache.work.borrow().iter().rev().find(|(w, _)| *w == id).map(|(_, r)| Rc::clone(r));
            tables.rows[i] = rows.ok_or_else(|| Error::internal("a work table with no rows"))?;
        }
        if let Some(sub) = &relation.subquery
            && tables.needs[i].is_empty()
            && !recursive.contains(&i)
        {
            tables.rows[i] =
                cache.rows(sub, || run_query(sub, params, session, cache, &frames, false))?;
        }
        if let Some(function) = &relation.function
            && tables.needs[i].is_empty()
        {
            let mut eval = Eval::new(params, session, base, cache, outer);
            tables.rows[i] = Rc::new(eval.function_rows(function)?);
        }
        if let Some(rows) = &relation.values
            && tables.needs[i].is_empty()
        {
            let mut eval = Eval::new(params, session, base, cache, outer);
            tables.rows[i] = Rc::new(eval.values_rows(rows)?);
        }
    }
    Ok(Rc::new(tables))
}

/// True when an expression is a constant for `eval_const_expressions`: constants, parameters and immutable functions of them.
fn constant(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Const(_) | ExprKind::Param(_) => true,
        ExprKind::Relabel(arg, _) => constant(arg),
        ExprKind::Func(f) => {
            builtin::proc_by_oid(f.oid).is_some_and(|p| p.volatile == b'i')
                && f.args.iter().all(constant)
        }
        _ => false,
    }
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
    let base = cache.tables(query, session)?;
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
    let open = gate(query, &mut eval)?;
    let mut tables = if open {
        with_subqueries(query, &base, params, session, cache, outer)?
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
                recursive_rows(query, op, work, &tables, &run)?
            }
            None => set_rows(op, &mut |i| Ok((*tables.rows[i]).clone()), session)?,
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
        let mut source = |index: usize, tuple: &[usize]| {
            let relation = &query.relations[index];
            if let Some(function) = &relation.function {
                let mut eval = Eval::new(params, session, tables, cache, outer);
                eval.tuple = tuple;
                return eval.function_rows(function);
            }
            if let Some(rows) = &relation.values {
                let mut eval = Eval::new(params, session, tables, cache, outer);
                eval.tuple = tuple;
                return eval.values_rows(rows);
            }
            let Some(sub) = &relation.subquery else { return Ok(Vec::new()) };
            let mut frames = outer.to_vec();
            frames.push(Frame { tables, tuple });
            run_query(sub, params, session, cache, &frames, false)
        };
        scan::tuples(query, tables, &mut |expr, tuple| eval.at(tuple).eval(expr), &mut source)?
    } else {
        Vec::new()
    };
    if simple {
        return Ok(if tuples.is_empty() { Vec::new() } else { vec![Vec::new()] });
    }
    let none = vec![scan::NONE; query.relations.len()];
    let rows = if query.grouped {
        agg::rows(&mut eval, query, &tuples, &none)?
    } else {
        // With no sort, `Limit` stops after the rows that it gives.
        let ordered = !query.sort.is_empty() || !query.distinct.is_empty();
        let needed = match count {
            Some(count) if !ordered => offset.saturating_add(count),
            _ => usize::MAX,
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
                window::Input { tuples: &refs, aggs: &[] },
                session,
            )?
        };
        let mut rows = Vec::with_capacity(tuples.len().min(needed));
        for i in sequence {
            if rows.len() >= needed {
                break;
            }
            eval.tuple = &tuples[i];
            if let Some(values) = values.get_mut(i) {
                eval.windows = std::mem::take(values);
            }
            eval.project(&query.targets, &levels, &mut rows)?;
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

impl Plan {
    /// The columns of the result. The junk columns of `ORDER BY` do not count.
    pub fn columns(&self) -> &[Target] {
        let n = self.query.targets.iter().take_while(|t| !t.junk).count();
        &self.query.targets[..n]
    }

    /// The warnings of the analysis of the query.
    pub fn notices(&self) -> &[Error] {
        &self.query.notices
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
        Ok(Plan { query: fold::query(&self.query, params, session)? })
    }

    /// `ExecSupportsBackwardScan` on the plan that PostgreSQL makes for the query, which decides if a cursor with neither `SCROLL` nor `NO SCROLL` can scan backward.
    pub fn backward_scan(&self) -> bool {
        backward(&self.query)
    }

    /// The checks of `ExecutorStart`, which come before `RowDescription`: a read-only transaction cannot lock a row, and a sequence has no rows to lock.
    ///
    /// # Errors
    ///
    /// `25006` for `FOR UPDATE` or `FOR SHARE` in a read-only transaction, and `42809` for a lock on the rows of a sequence.
    pub fn start(&self, session: &dyn Session) -> Result<()> {
        check_row_marks(&self.query, session)
    }

    /// Runs the query and gives its rows.
    ///
    /// # Errors
    ///
    /// The error of a function, such as a division by zero.
    pub fn run(&self, params: &[Value], session: &dyn Session) -> Result<Vec<Vec<Value>>> {
        self.start(session)?;
        run_query(&self.query, params, session, &Cache::default(), &[], false)
    }
}

/// The checks of `ExecCheckXactReadOnly` and `CheckValidRowMarkRel` for the tables that `FOR UPDATE` or `FOR SHARE` locks: a read-only transaction cannot lock a row, and a sequence has no rows to lock.
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
        if let Some(relation) = catalog.and_then(|c| c.relation(oid))
            && relation.kind == rupg_catalog::RelKind::Sequence
        {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("cannot lock rows in sequence \"{}\"", relation.name),
            ));
        }
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
        if cte.work.is_some() {
            return false;
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

/// `DISTINCT`, `ORDER BY`, `OFFSET` and `LIMIT`. A `DISTINCT` sorts the rows on the items of `DISTINCT` and keeps the first row of each group, as `Sort` and `Unique` do. When a type of `DISTINCT` has no order, it keeps the first row of each group in the order of the rows and then sorts for `ORDER BY`. A `Sort` below `Limit` keeps only the first `OFFSET` + `LIMIT` rows, as `tuplesort_set_bound` does.
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
        if query.distinct.iter().all(|g| g.sort != 0) {
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

/// The result of each aggregate call of a group, by the address of the call in the query.
type AggValues = Vec<(*const Aggref, Value)>;

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
    /// The result of each aggregate call for the group, by the address of the call in the query.
    aggs: AggValues,
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
            aggs: Vec::new(),
            cache,
            outer,
            sub: Vec::new(),
            domain_value: Vec::new(),
            sets: Vec::new(),
            windows: Vec::new(),
        }
    }

    /// The rows of a `VALUES` list, as `ValuesScan` gives them.
    fn values_rows(&mut self, rows: &[Vec<Expr>]) -> Result<Rows> {
        rows.iter().map(|row| row.iter().map(|e| self.eval(e)).collect()).collect()
    }

    /// `ExecFunctionScan`: the rows of a function in `FROM`. The rows of the calls of `ROWS FROM` go side by side, and a call with fewer rows gives nulls. With ordinality, the last column is the number of the row.
    fn function_rows(&mut self, function: &FromFunction) -> Result<Rows> {
        let mut sets = Vec::with_capacity(function.calls.len());
        for (call, width) in &function.calls {
            sets.push(self.function_set(call, *width)?);
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

    /// `ExecMakeTableFunctionResult`: the rows of one call of a function in `FROM`. A strict function with a null argument gives no rows when it gives a set, and else one row of nulls. Another expression gives one row with its value.
    fn function_set(&mut self, call: &Expr, width: usize) -> Result<Rows> {
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
        if let Some(body) = self.cache.body(f.oid, &types, self.session)? {
            let Body::Query(query) = &*body else {
                return Err(Error::internal("a function that returns a set with no query"));
            };
            let cache = Cache { bodies: Rc::clone(&self.cache.bodies), ..Cache::default() };
            return run_query(query, &args, self.session, &cache, &[], false);
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
        let c = Call { session: self.session, args: &types, ret: call.ty, variadic: f.variadic };
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

    /// `ExecProjectSet`: the rows of the targets for the row of the query. The calls of functions that give a set run level by level, and the calls of a level read the values of the levels below. The calls of one level go side by side, and a call with fewer values gives nulls, as `ExecProjectSRF` does. When no call of a level gives a value, the row gives no rows.
    fn project(
        &mut self,
        targets: &[Target],
        levels: &[Vec<&Expr>],
        out: &mut Vec<Vec<Value>>,
    ) -> Result<()> {
        let Some((calls, above)) = levels.split_first() else {
            out.push(targets.iter().map(|t| self.eval(&t.expr)).collect::<Result<_>>()?);
            return Ok(());
        };
        let sets = calls.iter().map(|call| self.set_values(call)).collect::<Result<Vec<_>>>()?;
        let count = sets.iter().map(Vec::len).max().unwrap_or(0);
        let base = self.sets.len();
        for i in 0..count {
            for (call, set) in calls.iter().zip(&sets) {
                let value = set.get(i).cloned().unwrap_or(Value::Null);
                self.sets.push((std::ptr::from_ref(*call), value));
            }
            let result = self.project(targets, above, out);
            self.sets.truncate(base);
            result?;
        }
        Ok(())
    }

    /// `ExecMakeFunctionResultSet`: the values of a call of a function that gives a set. A function that gives rows of more than one column, or of type `record`, gives a row value for each row.
    fn set_values(&mut self, call: &Expr) -> Result<Vec<Value>> {
        let ExprKind::Func(f) = &call.kind else {
            return Err(Error::internal("a set that is not a function call"));
        };
        let rows = self.function_set(call, 1)?;
        // The types of the `OUT` parameters, with the actual types of the call for a polymorphic type.
        let columns = rupg_analyze::out_columns(f)?.unwrap_or_default();
        let types: Vec<u32> = columns.iter().map(|c| c.ty).collect();
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
            aggs: Vec::new(),
            cache: self.cache,
            outer: self.outer,
            sub: Vec::new(),
            domain_value: Vec::new(),
            sets: Vec::new(),
            windows: Vec::new(),
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
            ExprKind::SqlValue(v) => self.sql_value(*v),
            ExprKind::Var(var) if var.levels_up == 0 => Ok(self.tables.var(self.tuple, *var)),
            ExprKind::Var(var) => {
                let frame = self
                    .outer
                    .len()
                    .checked_sub(var.levels_up)
                    .and_then(|i| self.outer.get(i))
                    .ok_or_else(|| Error::internal("a column of an outer query has no row"))?;
                Ok(frame.tables.var(frame.tuple, *var))
            }
            ExprKind::SubLink(sub) => self.sublink(sub, expr.ty),
            ExprKind::SubColumn(n) => self
                .sub
                .last()
                .and_then(|row| row.get(*n))
                .cloned()
                .ok_or_else(|| Error::internal("a subquery column is outside of ANY or ALL")),
            ExprKind::Agg(agg) => self
                .aggs
                .iter()
                .find(|(call, _)| std::ptr::eq(*call, &**agg))
                .map(|(_, value)| value.clone())
                .ok_or_else(|| Error::internal("an aggregate call is outside of a group")),
            ExprKind::Window(w) => self
                .windows
                .iter()
                .find(|(call, _)| std::ptr::eq(*call, &**w))
                .map(|(_, value)| value.clone())
                .ok_or_else(|| Error::internal("a window function call is outside of a window")),
        }
    }

    /// `ExecScanSubPlan`: the value of a subquery in an expression.
    fn sublink(&mut self, sub: &SubLink, ty: u32) -> Result<Value> {
        let exists = sub.kind == SubLinkKind::Exists;
        let query = &sub.query;
        let mut frames = self.outer.to_vec();
        frames.push(Frame { tables: self.tables, tuple: self.tuple });
        let rows = self.cache.rows(query, || {
            run_query(query, self.params, self.session, self.cache, &frames, exists)
        })?;
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
        if let Some(body) = self.cache.body(func, types, self.session)? {
            // The body runs with a cache of its own, so that a subquery of the body that reads an argument runs again for each call.
            let cache = Cache { bodies: Rc::clone(&self.cache.bodies), ..Cache::default() };
            return match &*body {
                Body::Value(expr) => {
                    let tables = scan::Tables::default();
                    Eval::new(args, self.session, &tables, &cache, &[]).eval(expr)
                }
                // `fmgr_sql` for a function that does not return a set: the first column of the first row, or null.
                Body::Query(query) => {
                    let rows = run_query(query, args, self.session, &cache, &[], false)?;
                    Ok(rows
                        .into_iter()
                        .next()
                        .and_then(|row| row.into_iter().next())
                        .unwrap_or(Value::Null))
                }
            };
        }
        let kernel = kernel(func, None)?;
        let call = Call { session: self.session, args: types, ret, variadic };
        kernel(&call, args)
    }

    fn func(&mut self, f: &Func, expr: &Expr) -> Result<Value> {
        // The planner adds the defaults, so this is for an expression that the planner did not see.
        let exprs = rupg_analyze::expand_arguments(f)?;
        let args = exprs.iter().map(|a| self.eval(a)).collect::<Result<Vec<_>>>()?;
        let types: Vec<u32> = exprs.iter().map(|a| a.ty).collect();
        self.call(f.oid, &types, expr.ty, f.variadic, &args)
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
            return Ok(Some(Error::new(
                SqlState::NOT_NULL_VIOLATION,
                format!(
                    "domain {} does not allow null values",
                    rupg_func::type_message_name(domain, self.session)
                ),
            )));
        }
        for (name, check) in &checks.checks {
            self.domain_value.push(value.clone());
            let result = self.eval(check);
            self.domain_value.pop();
            if result?.as_bool() == Some(false) {
                return Ok(Some(Error::new(
                    SqlState::CHECK_VIOLATION,
                    format!(
                        "value for domain {} violates check constraint \"{name}\"",
                        rupg_func::type_message_name(domain, self.session)
                    ),
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
        let array = match self.eval(&sub.container)? {
            Value::Null => return Ok(Value::Null),
            Value::Array(array) => array,
            Value::Text(name) => return self.name_subscript(&name, sub),
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

/// `evaluate_expr`: the value of an expression whose arguments are constants.
fn evaluate(expr: &Expr, session: &dyn Session) -> Result<Value> {
    let tables = scan::Tables::default();
    let cache = Cache::default();
    let mut eval = Eval {
        params: &[],
        session,
        case: Vec::new(),
        tables: &tables,
        tuple: &[],
        aggs: Vec::new(),
        cache: &cache,
        outer: &[],
        sub: Vec::new(),
        domain_value: Vec::new(),
        sets: Vec::new(),
        windows: Vec::new(),
    };
    eval.eval(expr)
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

/// The checks of [`check_input`] as `domain_check_safe` makes them: the error of the first check constraint that rejects the value is a soft error, and `None` means that the value passes.
///
/// # Errors
///
/// The errors of the check expressions.
pub fn input_failure(ty: u32, value: &Value, session: &dyn Session) -> Result<Option<Error>> {
    let element = builtin::type_by_oid(ty).map_or(0, |row| row.elem);
    let (domain, values) = match value {
        Value::Array(array) if builtin::domain_by_oid(element).is_some() => {
            (element, array.values.iter().flatten().collect())
        }
        Value::Null => return Ok(None),
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
        aggs: Vec::new(),
        cache: &cache,
        outer: &[],
        sub: Vec::new(),
        domain_value: Vec::new(),
        sets: Vec::new(),
        windows: Vec::new(),
    };
    for value in values {
        if let Some(error) = eval.domain_failure(domain, value)? {
            return Ok(Some(error));
        }
    }
    Ok(None)
}

/// A value as a binary-compatible type holds it: an `int4` as an `oid` keeps its bits, and an `oid` as an `int4` too.
#[allow(clippy::cast_sign_loss, clippy::cast_possible_wrap)]
fn relabel(value: Value, ty: u32) -> Value {
    let ty = base_type(ty);
    match value {
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
mod fold;
mod scan;
mod sort;
mod user;
mod window;

#[cfg(test)]
mod tests;

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
