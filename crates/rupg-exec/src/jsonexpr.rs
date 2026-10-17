//! `ExecEvalJsonExprPath` and `json_populate_type`: `JSON_EXISTS`, `JSON_QUERY` and `JSON_VALUE`.
//!
//! The path runs on the context item. A result goes to the type of `RETURNING` with the input function of the type, or as `json_populate_type` casts a `jsonb` value. When the behavior `ON ERROR` is not `ERROR`, an error of the path or of the cast is soft, and the expression gives the value of `ON ERROR`. A path that finds no item gives the value of `ON EMPTY`.

use rupg_analyze::{ExprKind, JsonBehavior, JsonBehaviorKind, JsonExpr, JsonExprOp, JsonWrapper};
use rupg_common::{Error, Result, SqlState};
use rupg_func::{Call, PathVar, SqlJsonItem, base_type};
use rupg_pgcatalog::builtin;
use rupg_types::{Array, ArrayDim, JsonItem, JsonTree, Value, oid};

use crate::{Eval, not_yet, relabel};

/// A result whose inner error is soft. `ON ERROR` handles a soft error when its behavior is not `ERROR`.
type Soft<T> = Result<std::result::Result<T, Error>>;

/// The flags `error` and `empty` of a `JsonExprState`. The path sets them. A null context item or a null path skips the path, and then the steps after it read the flags of the evaluation before, as PostgreSQL does.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Flags {
    error: bool,
    empty: bool,
}

/// True when the plan of PostgreSQL has the steps of the behavior: the behavior is not `ERROR`, and its value is not a null constant or the type of `RETURNING` is a domain.
fn has_steps(e: &JsonExpr, behavior: &JsonBehavior) -> bool {
    let null =
        behavior.expr.as_ref().is_none_or(|expr| matches!(expr.kind, ExprKind::Const(Value::Null)));
    behavior.kind != JsonBehaviorKind::Error
        && (!null || builtin::type_by_oid(e.returning.ty).is_some_and(|row| row.kind == b'd'))
}

/// The state after the path.
enum Outcome {
    /// The context item or the path is null, so the path did not run.
    Skipped,
    /// A value, which can need a cast to the type of `RETURNING`.
    Value(Value),
    /// The path found no item.
    Empty,
    /// A soft error.
    Error,
}

/// `JsValue` for a `jsonb` value: the part of a value that `json_populate_type` casts.
#[derive(Clone, Copy)]
enum Js<'t> {
    /// A SQL null, or a JSON null in an array.
    Null,
    /// The text of the value with no quotes, for `OMIT QUOTES`.
    Unquoted(&'t str),
    /// The whole value as `jbvBinary`. A scalar here is not a string and not a null.
    Root(&'t JsonTree<'t>),
    /// An element of an array.
    Element(&'t JsonTree<'t>, usize),
}

impl Js<'_> {
    /// `JsValueIsString`.
    fn is_string(&self) -> bool {
        match self {
            Js::Unquoted(_) => true,
            Js::Element(tree, at) => matches!(tree.item(*at), JsonItem::String(_)),
            Js::Null | Js::Root(_) => false,
        }
    }
}

/// `PopulateArrayContext`: the dimensions and the elements of an array that `populate_array` makes.
#[derive(Default)]
struct Dims {
    /// The number of dimensions, or 0 when it is not known yet.
    ndims: usize,
    /// The length of each dimension, or -1 when it is not known yet.
    dims: Vec<i32>,
    /// The number of elements of each dimension that the walk has seen.
    sizes: Vec<i32>,
    values: Vec<Option<Value>>,
}

impl Dims {
    /// `populate_array_report_expected_array`.
    fn expected(&self, ndim: usize) -> Error {
        let error = Error::new(SqlState::INVALID_TEXT_REPRESENTATION, "expected JSON array");
        if ndim == 0 {
            return error;
        }
        let indices: String = self.sizes[..ndim].iter().map(|size| format!("[{size}]")).collect();
        error.with_hint(format!("See the array element {indices}."))
    }

    /// `populate_array_assign_ndims`.
    fn assign(&mut self, ndims: usize) {
        self.ndims = ndims;
        self.dims = vec![-1; ndims];
        self.sizes = vec![0; ndims];
    }

    /// `populate_array_check_dimension`.
    fn check(&mut self, ndim: usize) -> std::result::Result<(), Error> {
        let dim = self.sizes[ndim];
        if self.dims[ndim] == -1 {
            self.dims[ndim] = dim;
        } else if self.dims[ndim] != dim {
            return Err(Error::new(SqlState::INVALID_TEXT_REPRESENTATION, "malformed JSON array")
                .with_detail(
                    "Multidimensional arrays must have sub-arrays with matching dimensions.",
                ));
        }
        self.sizes[ndim] = 0;
        if ndim > 0 {
            self.sizes[ndim - 1] += 1;
        }
        Ok(())
    }
}

/// The type of the value of a datetime item.
fn datetime_type(value: &Value) -> u32 {
    match value {
        Value::Date(_) => oid::DATE,
        Value::Time(_) => oid::TIME,
        Value::TimeTz(..) => oid::TIMETZ,
        Value::Timestamp(_) => oid::TIMESTAMP,
        _ => oid::TIMESTAMPTZ,
    }
}

/// `getBaseTypeAndTypmod`: the base type of a domain and the type modifier of the domain nearest to the base.
fn base_and_typmod(ty: u32, typmod: i32) -> (u32, i32) {
    let (mut ty, mut typmod) = (ty, typmod);
    while let Some(row) = builtin::type_by_oid(ty)
        && row.kind == b'd'
    {
        (ty, typmod) = (row.base, row.typmod);
    }
    (ty, typmod)
}

/// The error of a behavior whose value has no cast to the type of `RETURNING`.
fn could_not_coerce(clause: &str, behavior: &JsonBehavior, error: &Error) -> Error {
    Error::new(
        SqlState::DATATYPE_MISMATCH,
        format!(
            "could not coerce {clause} expression ({}) to the RETURNING type",
            behavior.kind.name()
        ),
    )
    .with_detail(error.message())
}

impl Eval<'_> {
    /// `ExecEvalJsonExprPath` and the steps after it.
    pub(crate) fn json_expr(&mut self, e: &JsonExpr) -> Result<Value> {
        let throw = e.on_error.kind == JsonBehaviorKind::Error;
        let mut outcome = self.json_path(e, throw)?;
        if let Outcome::Skipped = outcome {
            return self.json_skipped(e, throw);
        }
        let mut flags = Flags::default();
        if e.json_coercion
            && let Outcome::Value(value) = outcome
        {
            outcome = match self.json_coerce(e, value)? {
                Ok(value) => Outcome::Value(value),
                Err(error) if throw => return Err(error),
                Err(_) => Outcome::Error,
            };
        }
        let result = match outcome {
            Outcome::Skipped | Outcome::Value(_) => None,
            Outcome::Empty => match &e.on_empty {
                Some(on_empty) if on_empty.kind != JsonBehaviorKind::Error => {
                    flags.empty = true;
                    Some((on_empty, "ON EMPTY"))
                }
                None if !throw => {
                    flags.error = true;
                    Some((&e.on_error, "ON ERROR"))
                }
                _ => {
                    self.set_flags(e, flags);
                    let message = match &e.column {
                        Some(column) => {
                            format!(
                                "no SQL/JSON item found for specified path of column \"{column}\""
                            )
                        }
                        None => "no SQL/JSON item found for specified path".to_owned(),
                    };
                    return Err(Error::new(SqlState::NO_SQL_JSON_ITEM, message));
                }
            },
            Outcome::Error => {
                flags.error = true;
                Some((&e.on_error, "ON ERROR"))
            }
        };
        self.set_flags(e, flags);
        match (result, outcome) {
            (Some((behavior, clause)), _) => self.json_behavior(e, behavior, clause, throw),
            (None, Outcome::Value(value)) => Ok(value),
            (None, _) => Ok(Value::Null),
        }
    }

    /// The steps after the path when the context item or the path is null: the cast of null to the type of `RETURNING`, and then the steps of a behavior when the flags that the evaluation before left ask for it.
    fn json_skipped(&mut self, e: &JsonExpr, throw: bool) -> Result<Value> {
        let mut flags = self.flags(e);
        let mut value = Value::Null;
        if e.json_coercion {
            match self.json_coerce(e, Value::Null)? {
                Ok(coerced) => value = coerced,
                Err(error) if throw => return Err(error),
                Err(error) => {
                    if flags.error {
                        return Err(could_not_coerce("ON ERROR", &e.on_error, &error));
                    }
                    if flags.empty
                        && let Some(on_empty) = &e.on_empty
                    {
                        return Err(could_not_coerce("ON EMPTY", on_empty, &error));
                    }
                    flags.error = true;
                }
            }
        }
        self.set_flags(e, flags);
        if has_steps(e, &e.on_error) {
            return if flags.error {
                self.json_behavior(e, &e.on_error, "ON ERROR", throw)
            } else {
                Ok(value)
            };
        }
        if let Some(on_empty) = &e.on_empty
            && has_steps(e, on_empty)
            && flags.empty
        {
            return self.json_behavior(e, on_empty, "ON EMPTY", throw);
        }
        Ok(value)
    }

    /// The flags that the last evaluation of the node left.
    fn flags(&self, e: &JsonExpr) -> Flags {
        let all = self.cache.json_flags.borrow();
        all.iter().find(|(at, _)| std::ptr::eq(*at, e)).map(|(_, flags)| *flags).unwrap_or_default()
    }

    fn set_flags(&self, e: &JsonExpr, flags: Flags) {
        let mut all = self.cache.json_flags.borrow_mut();
        match all.iter_mut().find(|(at, _)| std::ptr::eq(*at, e)) {
            Some((_, kept)) => *kept = flags,
            None => all.push((std::ptr::from_ref(e), flags)),
        }
    }

    /// The path on the context item, and the cast of `JSON_VALUE` with the input function of the type. A null context item or a null path gives null.
    fn json_path(&mut self, e: &JsonExpr, throw: bool) -> Result<Outcome> {
        let formatted = self.eval(&e.formatted)?;
        if formatted.is_null() {
            return Ok(Outcome::Skipped);
        }
        let path = self.eval(&e.path)?;
        if path.is_null() {
            return Ok(Outcome::Skipped);
        }
        let mut vars = Vec::with_capacity(e.passing.len());
        for (name, arg) in &e.passing {
            vars.push(PathVar { name: name.clone(), ty: arg.ty, value: self.eval(arg)? });
        }
        let call = Call { session: self.session, args: &[], ret: e.returning.ty, variadic: false };
        let want = e.op != JsonExprOp::Exists;
        let Some((found, mut items)) =
            rupg_func::sql_json_path(&call, &formatted, &path, &vars, throw, want)?
        else {
            return Ok(Outcome::Error);
        };
        let one = match e.op {
            JsonExprOp::Exists => return Ok(Outcome::Value(Value::Bool(found))),
            JsonExprOp::Query => return json_query(e, &items, throw),
            JsonExprOp::Value if items.is_empty() => return Ok(Outcome::Empty),
            JsonExprOp::Value if items.len() > 1 => {
                if !throw {
                    return Ok(Outcome::Error);
                }
                return Err(Error::new(SqlState::MORE_THAN_ONE_SQL_JSON_ITEM, scalar_message(e)));
            }
            JsonExprOp::Value => items.swap_remove(0),
        };
        let text = match &one {
            SqlJsonItem::Container(_) if !throw => return Ok(Outcome::Error),
            SqlJsonItem::Container(_) => {
                return Err(Error::new(SqlState::SQL_JSON_SCALAR_REQUIRED, scalar_message(e)));
            }
            SqlJsonItem::Null => return Ok(Outcome::Value(Value::Null)),
            _ if e.returning.ty == oid::JSON || e.returning.ty == oid::JSONB => one.jsonb(),
            _ if e.json_coercion => return Ok(Outcome::Value(Value::Text(one.jsonb()))),
            SqlJsonItem::String(s) => s.clone(),
            SqlJsonItem::Numeric(_) => one.jsonb(),
            // `boolout`.
            SqlJsonItem::Bool(b) => (if *b { "t" } else { "f" }).to_owned(),
            SqlJsonItem::Datetime(value, _) => {
                let mut out = Vec::new();
                rupg_func::output(datetime_type(value), value, self.session, &mut out)?;
                String::from_utf8(out).map_err(|_| {
                    Error::internal("an output function gave bytes that are not UTF-8")
                })?
            }
        };
        if !e.io_coercion {
            return Ok(Outcome::Value(Value::Text(text)));
        }
        let ty = e.returning.ty;
        Ok(match rupg_func::input_soft(ty, &text, e.returning.typmod, self.session)? {
            Ok(value) => Outcome::Value(value),
            Err(error) if throw => return Err(error),
            Err(_) => Outcome::Error,
        })
    }

    /// `ExecEvalJsonCoercion`: the cast of the result to the type of `RETURNING`. The result of `JSON_EXISTS` goes to an integer type as `bool_int4` casts it.
    fn json_coerce(&mut self, e: &JsonExpr, value: Value) -> Soft<Value> {
        let (ty, typmod) = (e.returning.ty, e.returning.typmod);
        let value = match value {
            Value::Bool(b) if e.op == JsonExprOp::Exists => {
                if base_type(ty) == oid::INT4 {
                    let value = Value::Int4(i32::from(b));
                    return Ok(match self.domain_failure_of(ty, &value)? {
                        Some(error) => Err(error),
                        None => Ok(value),
                    });
                }
                Value::Text((if b { "true" } else { "false" }).to_owned())
            }
            Value::Null if e.op == JsonExprOp::Exists && base_type(ty) == oid::INT4 => {
                return Ok(match self.domain_failure_of(ty, &Value::Null)? {
                    Some(error) => Err(error),
                    None => Ok(Value::Null),
                });
            }
            value => value,
        };
        self.json_populate(&value, ty, typmod, e.omit_quotes)
    }

    /// The error of the checks of a type when it is a domain.
    fn domain_failure_of(&mut self, ty: u32, value: &Value) -> Result<Option<Error>> {
        if builtin::type_by_oid(ty).is_some_and(|row| row.kind == b'd') {
            self.domain_failure(ty, value)
        } else {
            Ok(None)
        }
    }

    /// `json_populate_type` for a `jsonb` value, which is null or the text of the value.
    fn json_populate(
        &mut self,
        value: &Value,
        ty: u32,
        typmod: i32,
        omit_quotes: bool,
    ) -> Soft<Value> {
        let text = match value {
            Value::Null => return self.populate_field(Js::Null, ty, typmod),
            Value::Text(text) => text,
            _ => return Err(Error::internal("the value of a SQL/JSON cast is not jsonb")),
        };
        let tree = JsonTree::parse(text).map_err(rupg_func::type_error)?;
        if omit_quotes {
            // `JsonbUnquote`.
            let unquoted = match tree.item(0) {
                JsonItem::String(s) => s.clone(),
                _ => tree.text(0).to_owned(),
            };
            return self.populate_field(Js::Unquoted(&unquoted), ty, typmod);
        }
        self.populate_field(Js::Root(&tree), ty, typmod)
    }

    /// `populate_record_field`.
    fn populate_field(&mut self, js: Js<'_>, ty: u32, typmod: i32) -> Soft<Value> {
        let row = builtin::type_by_oid(ty);
        let domain = row.is_some_and(|row| row.kind == b'd');
        let composite = ty == oid::RECORD || row.is_some_and(|row| row.kind == b'c');
        let array = row.is_some_and(|row| row.category == b'A' && row.elem != 0);
        if (array || composite) && js.is_string() {
            return self.populate_scalar(js, ty, typmod);
        }
        if matches!(js, Js::Null) && !domain {
            return Ok(Ok(Value::Null));
        }
        if domain {
            let (base, base_typmod) = base_and_typmod(ty, typmod);
            if builtin::type_by_oid(base).is_some_and(|row| row.kind == b'c') {
                return Err(not_yet("a SQL/JSON cast to a domain over a composite type"));
            }
            let value = if matches!(js, Js::Null) {
                Value::Null
            } else {
                match self.populate_field(js, base, base_typmod)? {
                    Ok(value) => relabel(value, ty),
                    Err(error) => return Ok(Err(error)),
                }
            };
            return Ok(match self.domain_failure(ty, &value)? {
                Some(error) => Err(error),
                None => Ok(value),
            });
        }
        if composite {
            return Err(not_yet("a SQL/JSON cast to a composite type"));
        }
        if let (true, Some(row)) = (array, row) {
            return self.populate_array(js, row.elem, typmod);
        }
        self.populate_scalar(js, ty, typmod)
    }

    /// `populate_scalar`: the input function of the type on the text of the value.
    fn populate_scalar(&mut self, js: Js<'_>, ty: u32, typmod: i32) -> Soft<Value> {
        let text = match js {
            Js::Null => return Ok(Ok(Value::Null)),
            Js::Unquoted(s) => s,
            Js::Root(tree) if ty == oid::JSONB => {
                return Ok(Ok(Value::Text(tree.source().to_owned())));
            }
            Js::Root(tree) => tree.text(0),
            Js::Element(tree, at) if ty == oid::JSONB => {
                return Ok(Ok(Value::Text(tree.text(at).to_owned())));
            }
            Js::Element(tree, at) => match tree.item(at) {
                JsonItem::String(s) if ty != oid::JSON => s,
                _ => tree.text(at),
            },
        };
        rupg_func::input_soft(ty, text, typmod, self.session)
    }

    /// `populate_array`: an array of the elements of a JSON array. The lower bounds are 1.
    fn populate_array(&mut self, js: Js<'_>, element: u32, typmod: i32) -> Soft<Value> {
        let mut dims = Dims::default();
        let (tree, at) = match js {
            Js::Root(tree) => (tree, 0),
            Js::Element(tree, at) => (tree, at),
            Js::Null | Js::Unquoted(_) => return Ok(Err(dims.expected(0))),
        };
        if let Err(error) = self.populate_dim(&mut dims, tree, at, 1, element, typmod)? {
            return Ok(Err(error));
        }
        dims.dims[0] = dims.sizes[0];
        if dims.dims.contains(&0) {
            return Ok(Ok(Value::Array(Box::new(Array::empty()))));
        }
        let dims_out = dims.dims.iter().map(|&len| ArrayDim { len, lower: 1 }).collect();
        Ok(Ok(Value::Array(Box::new(Array { dims: dims_out, values: dims.values }))))
    }

    /// `populate_array_dim_jsonb`: the elements of a sub-array at the dimension `ndim`, which counts from 1.
    fn populate_dim(
        &mut self,
        dims: &mut Dims,
        tree: &JsonTree<'_>,
        at: usize,
        ndim: usize,
        element: u32,
        typmod: i32,
    ) -> Soft<()> {
        let items = match tree.item(at) {
            JsonItem::Array(items) => items,
            _ => return Ok(Err(dims.expected(ndim - 1))),
        };
        if dims.ndims == 0
            && items.first().is_none_or(|&first| !matches!(tree.item(first), JsonItem::Array(_)))
        {
            dims.assign(ndim);
        }
        for &child in items {
            if dims.ndims > 0 && ndim >= dims.ndims {
                let js = match tree.item(child) {
                    JsonItem::Null => Js::Null,
                    _ => Js::Element(tree, child),
                };
                match self.populate_field(js, element, typmod)? {
                    Ok(value) => dims.values.push((!value.is_null()).then_some(value)),
                    Err(error) => return Ok(Err(error)),
                }
                dims.sizes[ndim - 1] += 1;
            } else {
                if let Err(error) =
                    self.populate_dim(dims, tree, child, ndim + 1, element, typmod)?
                {
                    return Ok(Err(error));
                }
                if let Err(error) = dims.check(ndim) {
                    return Ok(Err(error));
                }
            }
        }
        Ok(Ok(()))
    }

    /// The value of a behavior. When the behavior `ON ERROR` is not `ERROR`, a cast of the value that fails gives the error that names the behavior.
    fn json_behavior(
        &mut self,
        e: &JsonExpr,
        behavior: &JsonBehavior,
        clause: &str,
        throw: bool,
    ) -> Result<Value> {
        let Some(expr) = &behavior.expr else { return Ok(Value::Null) };
        let value = match &expr.kind {
            ExprKind::CoerceViaIo(arg, _) if !throw => {
                let value = self.eval(arg)?;
                if value.is_null() {
                    Value::Null
                } else {
                    let mut text = Vec::new();
                    rupg_func::output(arg.ty, &value, self.session, &mut text)?;
                    let text = String::from_utf8(text).map_err(|_| {
                        Error::internal("an output function gave bytes that are not UTF-8")
                    })?;
                    match rupg_func::input_soft(expr.ty, &text, -1, self.session)? {
                        Ok(value) => value,
                        Err(error) => return Err(could_not_coerce(clause, behavior, &error)),
                    }
                }
            }
            ExprKind::CoerceToDomain(arg, _) if !throw => {
                let value = relabel(self.eval(arg)?, expr.ty);
                if let Some(error) = self.domain_failure(expr.ty, &value)? {
                    return Err(could_not_coerce(clause, behavior, &error));
                }
                value
            }
            _ => self.eval(expr)?,
        };
        if !behavior.coerce {
            return Ok(value);
        }
        let (ty, typmod) = (e.returning.ty, e.returning.typmod);
        match self.json_populate(&value, ty, typmod, e.omit_quotes)? {
            Ok(value) => Ok(value),
            Err(error) if throw => Err(error),
            Err(error) => Err(could_not_coerce(clause, behavior, &error)),
        }
    }
}

/// The error message of `JSON_VALUE` for a result that is not one scalar item.
fn scalar_message(e: &JsonExpr) -> String {
    match &e.column {
        Some(column) => {
            format!("JSON path expression for column \"{column}\" must return single scalar item")
        }
        None => "JSON path expression in JSON_VALUE must return single scalar item".to_owned(),
    }
}

/// `JSON_QUERY`: the wrapper of the items, and the error for more than one item with no wrapper.
fn json_query(e: &JsonExpr, items: &[SqlJsonItem], throw: bool) -> Result<Outcome> {
    let wrap = !items.is_empty()
        && match e.wrapper {
            JsonWrapper::None | JsonWrapper::Unspec => false,
            JsonWrapper::Unconditional => true,
            JsonWrapper::Conditional => items.len() > 1,
        };
    if wrap {
        let parts: Vec<String> = items.iter().map(SqlJsonItem::jsonb).collect();
        return Ok(Outcome::Value(Value::Text(format!("[{}]", parts.join(", ")))));
    }
    match items {
        [] => Ok(Outcome::Empty),
        [one] => Ok(Outcome::Value(Value::Text(one.jsonb()))),
        _ if !throw => Ok(Outcome::Error),
        _ => Err(Error::new(SqlState::MORE_THAN_ONE_SQL_JSON_ITEM, match &e.column {
            Some(column) => format!(
                "JSON path expression for column \"{column}\" must return single item when no wrapper is requested"
            ),
            None => "JSON path expression in JSON_QUERY must return single item when no wrapper is requested".to_owned(),
        })
        .with_hint("Use the WITH WRAPPER clause to wrap SQL/JSON items into an array.")),
    }
}
