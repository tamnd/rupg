//! The vectorized executor, the point path, compressed execution, spill, the per-distinct result tables of document 14 section 14.6.
//!
//! This version runs a query without a `FROM` clause: it computes the target list once, after the `WHERE` clause. [`prepare`] checks that the engine has every function and every type of the query, so that a query that the engine cannot run is an error `0A000` before it runs. [`Plan::run`] then computes the row as `ExecInterpExpr` of PostgreSQL does.

#![forbid(unsafe_code)]

use rupg_analyze::{BoolOp, BoolTest, Case, Expr, ExprKind, Func, Query, SqlValue, Target};
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

/// True when the function gives null for a null argument without a call.
fn strict(func: u32) -> bool {
    builtin::proc_by_oid(func).is_none_or(|p| p.strict)
}

/// Checks that the engine has every function and every type that an expression uses.
fn check(expr: &Expr) -> Result<()> {
    match &expr.kind {
        ExprKind::Const(_) | ExprKind::Param(_) | ExprKind::CaseTest | ExprKind::SqlValue(_) => {}
        ExprKind::Func(f) => {
            kernel(f.oid, expr.location)?;
            f.args.iter().try_for_each(check)?;
        }
        ExprKind::Relabel(arg) | ExprKind::NullTest(arg, _) | ExprKind::BooleanTest(arg, _) => {
            check(arg)?
        }
        ExprKind::CoerceViaIo(arg) => {
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
    }
    Ok(())
}

/// Checks a query and makes the plan to run it.
///
/// # Errors
///
/// `0A000` for a function or a type that the engine does not have yet.
pub fn prepare(query: Query) -> Result<Plan> {
    for target in &query.targets {
        if !rupg_func::output_supported(target.expr.ty) {
            return Err(not_yet(format!("output of type {}", format_type(target.expr.ty))));
        }
        check(&target.expr)?;
    }
    if let Some(filter) = &query.filter {
        check(filter)?;
    }
    Ok(Plan { query })
}

impl Plan {
    /// The columns of the result.
    pub fn columns(&self) -> &[Target] {
        &self.query.targets
    }

    /// The warnings of the analysis of the query.
    pub fn notices(&self) -> &[Error] {
        &self.query.notices
    }

    /// The types of the parameters.
    pub fn params(&self) -> &[u32] {
        &self.query.params
    }

    /// Runs the query: the row, or `None` when the `WHERE` clause is not true.
    ///
    /// # Errors
    ///
    /// The error of a function, such as a division by zero.
    pub fn run(&self, params: &[Value], session: &dyn Session) -> Result<Option<Vec<Value>>> {
        let mut eval = Eval { params, session, case: Vec::new() };
        if let Some(filter) = &self.query.filter
            && eval.eval(filter)? != Value::Bool(true)
        {
            return Ok(None);
        }
        let row =
            self.query.targets.iter().map(|t| eval.eval(&t.expr)).collect::<Result<Vec<_>>>()?;
        Ok(Some(row))
    }
}

/// The state of the evaluation of the expressions of one row.
struct Eval<'a> {
    params: &'a [Value],
    session: &'a dyn Session,
    /// The values of the `CASE` expressions that are open, for `CaseTest`.
    case: Vec<Value>,
}

impl Eval<'_> {
    fn eval(&mut self, expr: &Expr) -> Result<Value> {
        match &expr.kind {
            ExprKind::Const(v) => Ok(v.clone()),
            ExprKind::Param(n) => {
                // `$n` counts from 1.
                let value = n.checked_sub(1).and_then(|i| self.params.get(i));
                value.cloned().ok_or_else(|| Error::internal("a parameter has no value"))
            }
            ExprKind::Func(f) => self.func(f, expr),
            ExprKind::Relabel(arg) => Ok(relabel(self.eval(arg)?, expr.ty)),
            ExprKind::CoerceViaIo(arg) => {
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

/// A value as a binary-compatible type holds it: an `int4` as an `oid` keeps its bits, and an `oid` as an `int4` too.
#[allow(clippy::cast_sign_loss, clippy::cast_possible_wrap)]
fn relabel(value: Value, ty: u32) -> Value {
    let ty = base_type(ty);
    match value {
        Value::Int4(v) if ty != oid::INT4 => Value::Oid(v as u32),
        Value::Oid(v) if ty == oid::INT4 => Value::Int4(v as i32),
        value => value,
    }
}

#[cfg(test)]
mod tests;
