//! The vectorized executor, the point path, compressed execution, spill, the per-distinct result tables of document 14 section 14.6.
//!
//! This version reads the tables of the catalog. It joins the parts of `FROM`, tests the `WHERE` clause and computes the target list for each row. Then it applies `DISTINCT`, `ORDER BY`, `OFFSET` and `LIMIT` as the plan of PostgreSQL does with the nodes `Sort`, `Unique` and `Limit`. [`prepare`] checks that the engine has every function and every type of the query, so that a query that the engine cannot run is an error `0A000` before it runs. [`Plan::run`] then computes the row as `ExecInterpExpr` of PostgreSQL does.

#![forbid(unsafe_code)]

use std::cell::RefCell;
use std::rc::Rc;

use rupg_analyze::{
    Aggref, BoolOp, BoolTest, Case, Expr, ExprKind, FromFunction, FromItem, Func, Query, SortGroup,
    SqlValue, SubLink, SubLinkKind, Target,
};
use rupg_common::{Error, Result, SqlState};
use rupg_func::{Call, Kernel, Session, base_type};
use rupg_pgcatalog::builtin;
use rupg_types::{Array, ArrayDim, MAXDIM, Value, format_type, oid};

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

/// True when a call in `FROM` needs the set kernel of the function: the function gives a set, or a row with `width` columns.
fn table_function(func: u32, width: usize) -> bool {
    width > 1 || builtin::proc_by_oid(func).is_some_and(|p| p.retset)
}

/// True when the function gives null for a null argument without a call.
fn strict(func: u32) -> bool {
    builtin::proc_by_oid(func).is_none_or(|p| p.strict)
}

/// Checks that the engine has every function and every type that an expression uses.
fn check(expr: &Expr) -> Result<()> {
    match &expr.kind {
        ExprKind::Const(_)
        | ExprKind::Param(_)
        | ExprKind::CaseTest
        | ExprKind::SqlValue(_)
        | ExprKind::Var(_)
        | ExprKind::SubColumn(_) => {}
        ExprKind::SubLink(sub) => {
            if let Some(test) = &sub.test {
                check(test)?;
            }
            check_query(&sub.query, false)?;
        }
        ExprKind::Func(f) => {
            kernel(f.oid, expr.location)?;
            f.args.iter().try_for_each(check)?;
        }
        ExprKind::Relabel(arg, _) | ExprKind::NullTest(arg, _) | ExprKind::BooleanTest(arg, _) => {
            check(arg)?
        }
        ExprKind::CoerceViaIo(arg, _) => {
            if !rupg_func::output_supported(arg.ty) {
                return Err(not_yet(format!("output of type {}", format_type(arg.ty))));
            }
            if !rupg_func::input_supported(expr.ty) {
                return Err(not_yet(format!("input of type {}", format_type(expr.ty))));
            }
            check(arg)?;
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
        ExprKind::Array { elements, .. } => elements.iter().try_for_each(check)?,
        ExprKind::Agg(agg) => {
            agg.args.iter().try_for_each(check)?;
            if let Some(filter) = &agg.filter {
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
        for (call, width) in relation.function.iter().flat_map(|f| &f.calls) {
            match &call.kind {
                ExprKind::Func(f) if table_function(f.oid, *width) => {
                    set_kernel(f.oid, call.location)?;
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
    Ok(())
}

/// The rows of a query.
type Rows = Vec<Vec<Value>>;

/// The state of a run that the query and its subqueries share.
#[derive(Default)]
struct Cache {
    /// The rows of the relations of each query, by the address of the query. A subquery reads them once, also when it runs for each row of the query outside it.
    tables: RefCell<Vec<(*const Query, Rc<scan::Tables>)>>,
    /// The rows of each subquery that reads no column of an outer query, as an `InitPlan` keeps them. It runs once, when the query first needs it.
    rows: RefCell<Vec<(*const Query, Rc<Rows>)>>,
}

impl Cache {
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
        if !correlated(query) {
            self.rows.borrow_mut().push((query, Rc::clone(&rows)));
        }
        Ok(rows)
    }
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
    if query.relations.iter().all(|r| r.subquery.is_none() && r.function.is_none()) {
        return Ok(Rc::clone(base));
    }
    let mut tables = (**base).clone();
    let mut frames = outer.to_vec();
    frames.push(Frame { tables: base, tuple: &[] });
    for (i, relation) in query.relations.iter().enumerate() {
        if let Some(sub) = &relation.subquery
            && tables.needs[i].is_empty()
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
    if exists && agg::calls(query).is_empty() && query.having.is_none() && query.offset.is_none() {
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
    let tables = if open {
        with_subqueries(query, &base, params, session, cache, outer)?
    } else {
        Rc::clone(&base)
    };
    let tables = &*tables;
    let mut eval = Eval { tables, ..eval.at(&[]) };
    let tuples = if open {
        let mut source = |index: usize, tuple: &[usize]| {
            let relation = &query.relations[index];
            if let Some(function) = &relation.function {
                let mut eval = Eval::new(params, session, tables, cache, outer);
                eval.tuple = tuple;
                return eval.function_rows(function);
            }
            let Some(sub) = &relation.subquery else { return Ok(Vec::new()) };
            let mut frames = outer.to_vec();
            frames.push(Frame { tables, tuple });
            run_query(sub, params, session, cache, &frames, false)
        };
        scan::tuples(
            query,
            tables,
            &mut |expr, tuple| Ok(eval.at(tuple).eval(expr)? == Value::Bool(true)),
            &mut source,
        )?
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
        let mut rows = Vec::with_capacity(tuples.len().min(needed));
        for tuple in tuples.iter().take(needed) {
            eval.tuple = tuple;
            let row =
                query.targets.iter().map(|t| eval.eval(&t.expr)).collect::<Result<Vec<_>>>()?;
            rows.push(row);
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

    /// Folds the constant parts of the expressions as the planner does, and gives the plan that runs the folded expressions. `params` has the values of the parameters of `Bind`, which the fold takes as constants as a custom plan does.
    ///
    /// # Errors
    ///
    /// The error of an immutable function of constants, such as a division by zero.
    pub fn fold(&self, params: Option<&[Value]>, session: &dyn Session) -> Result<Plan> {
        Ok(Plan { query: fold::query(&self.query, params, session)? })
    }

    /// Runs the query and gives its rows.
    ///
    /// # Errors
    ///
    /// The error of a function, such as a division by zero.
    pub fn run(&self, params: &[Value], session: &dyn Session) -> Result<Vec<Vec<Value>>> {
        run_query(&self.query, params, session, &Cache::default(), &[], false)
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
    aggs: Vec<(*const Aggref, Value)>,
    cache: &'a Cache,
    /// The rows of the queries outside the query, with the query just outside it last.
    outer: &'a [Frame<'a>],
    /// The rows of the subqueries of `ANY` and `ALL` whose test runs now, for `SubColumn`.
    sub: Vec<Vec<Value>>,
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
        }
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
        let ExprKind::Func(f) = &call.kind else { return Ok(vec![vec![self.eval(call)?]]) };
        if !table_function(f.oid, width) {
            return Ok(vec![vec![self.eval(call)?]]);
        }
        let kernel = set_kernel(f.oid, call.location)?;
        let args = f.args.iter().map(|a| self.eval(a)).collect::<Result<Vec<_>>>()?;
        if strict(f.oid) && args.iter().any(Value::is_null) {
            let retset = builtin::proc_by_oid(f.oid).is_some_and(|p| p.retset);
            return Ok(if retset { Vec::new() } else { vec![vec![Value::Null; width]] });
        }
        let types: Vec<u32> = f.args.iter().map(|a| a.ty).collect();
        let c = Call { session: self.session, args: &types, ret: call.ty, variadic: f.variadic };
        kernel(&c, &args)
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
            ExprKind::Func(f) => self.func(f, expr),
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
            ExprKind::Bool(op, args) => self.bool_op(*op, args),
            ExprKind::NullTest(arg, is_null) => {
                Ok(Value::Bool(self.eval(arg)?.is_null() == *is_null))
            }
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
            ExprKind::SubLink(sub) => self.sublink(sub),
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
        }
    }

    /// `ExecScanSubPlan`: the value of a subquery in an expression.
    fn sublink(&mut self, sub: &SubLink) -> Result<Value> {
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
            SubLinkKind::Array => {
                let values =
                    rows.iter().map(|row| row.first().filter(|v| !v.is_null()).cloned()).collect();
                Ok(Value::Array(Box::new(Array::one(values))))
            }
            SubLinkKind::Any | SubLinkKind::All => {
                let test =
                    sub.test.as_ref().ok_or_else(|| Error::internal("ANY and ALL need a test"))?;
                let any = sub.kind == SubLinkKind::Any;
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
        let kernel = kernel(func, None)?;
        let call = Call { session: self.session, args: types, ret, variadic };
        kernel(&call, args)
    }

    fn func(&mut self, f: &Func, expr: &Expr) -> Result<Value> {
        let args = f.args.iter().map(|a| self.eval(a)).collect::<Result<Vec<_>>>()?;
        let types: Vec<u32> = f.args.iter().map(|a| a.ty).collect();
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
    };
    eval.eval(expr)
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

#[cfg(test)]
mod tests;
