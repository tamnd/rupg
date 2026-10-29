//! The SQL/JSON query functions of `parse_expr.c`: `JSON_EXISTS`, `JSON_VALUE` and `JSON_QUERY`.

use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin;
use rupg_sql::nodes::{
    JsonBehavior as RawBehavior, JsonBehaviorType, JsonExprOp as RawOp, JsonFuncExpr, JsonQuotes,
    JsonWrapper as RawWrapper, Node,
};
use rupg_types::{Value, oid};

use crate::Analyzer;
use crate::coerce::{AtOpt, Context, hide_cast};
use crate::expr::{
    Expr, ExprKind, JsonBehavior, JsonBehaviorKind, JsonExpr, JsonExprOp, JsonFormat,
    JsonFormatKind, JsonReturning, JsonWrapper,
};
use crate::sqljson::format_of;
use crate::typename::place;
use crate::types;

/// The kind of a behavior of the raw tree.
fn behavior_kind(btype: JsonBehaviorType) -> JsonBehaviorKind {
    match btype {
        JsonBehaviorType::JSON_BEHAVIOR_ERROR => JsonBehaviorKind::Error,
        JsonBehaviorType::JSON_BEHAVIOR_EMPTY => JsonBehaviorKind::Empty,
        JsonBehaviorType::JSON_BEHAVIOR_TRUE => JsonBehaviorKind::True,
        JsonBehaviorType::JSON_BEHAVIOR_FALSE => JsonBehaviorKind::False,
        JsonBehaviorType::JSON_BEHAVIOR_UNKNOWN => JsonBehaviorKind::Unknown,
        JsonBehaviorType::JSON_BEHAVIOR_EMPTY_ARRAY => JsonBehaviorKind::EmptyArray,
        JsonBehaviorType::JSON_BEHAVIOR_EMPTY_OBJECT => JsonBehaviorKind::EmptyObject,
        JsonBehaviorType::JSON_BEHAVIOR_DEFAULT => JsonBehaviorKind::Default,
        _ => JsonBehaviorKind::Null,
    }
}

/// The error of a behavior that the function does not allow. `allowed` is the list of the behaviors that the detail shows, and `columns` the kind of column of `JSON_TABLE` that the detail names for a column.
fn invalid_behavior(
    clause: &str,
    behavior: &RawBehavior,
    allowed: &str,
    func: &str,
    column: Option<&str>,
    columns: &str,
) -> Error {
    let (message, detail) = match column {
        None => (
            format!("invalid {clause} behavior"),
            format!("Only {allowed} is allowed in {clause} for {func}."),
        ),
        Some(column) => (
            format!("invalid {clause} behavior for column \"{column}\""),
            format!("Only {allowed} is allowed in {clause} for {columns}."),
        ),
    };
    Error::new(SqlState::SYNTAX_ERROR, message).with_detail(detail).at_opt(place(behavior.location))
}

/// Checks that each behavior is one of `allowed`.
fn check_behaviors(
    behaviors: &[(&str, Option<&RawBehavior>)],
    allowed: &[JsonBehaviorKind],
    text: &str,
    func: &str,
    column: Option<&str>,
    columns: &str,
) -> Result<()> {
    for (clause, behavior) in behaviors {
        if let Some(behavior) = behavior
            && !allowed.contains(&behavior_kind(behavior.btype))
        {
            return Err(invalid_behavior(clause, behavior, text, func, column, columns));
        }
    }
    Ok(())
}

/// `ValidJsonBehaviorDefaultExpr`: true for a constant, a call of a function or an operator, and the casts of them.
fn valid_default(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Const(_) | ExprKind::Func(_) => true,
        ExprKind::CoerceViaIo(arg, _)
        | ExprKind::CoerceToDomain(arg, _)
        | ExprKind::Relabel(arg, _)
        | ExprKind::ArrayCoerce { arg, .. } => valid_default(arg),
        _ => false,
    }
}

/// `GetJsonBehaviorConst`: the value of a behavior that has no expression.
fn behavior_const(kind: JsonBehaviorKind, at: Option<usize>) -> Expr {
    let (value, ty) = match kind {
        JsonBehaviorKind::EmptyArray => (Value::text("[]"), oid::JSONB),
        JsonBehaviorKind::EmptyObject => (Value::text("{}"), oid::JSONB),
        JsonBehaviorKind::True => (Value::Bool(true), oid::BOOL),
        JsonBehaviorKind::False => (Value::Bool(false), oid::BOOL),
        _ => (Value::Null, oid::INT4),
    };
    Expr::constant(value, ty, -1, at)
}

/// `DomainHasConstraints`: true for a domain, or a domain below it, with a check or `NOT NULL`.
pub(crate) fn domain_has_constraints(ty: u32) -> bool {
    let mut ty = ty;
    while let Some(row) = types::row(ty)
        && row.kind == b'd'
    {
        if builtin::domain_by_oid(ty).is_some_and(|d| d.not_null || !d.checks.is_empty()) {
            return true;
        }
        ty = row.base;
    }
    false
}

impl Analyzer<'_> {
    /// `transformJsonFuncExpr`: `JSON_EXISTS`, `JSON_QUERY` or `JSON_VALUE`.
    pub(crate) fn json_func_expr(&mut self, f: &JsonFuncExpr) -> Result<Expr> {
        self.json_func(f, None)
    }

    /// [`Analyzer::json_func_expr`], also for a column of `JSON_TABLE`. `context` is the context item of a column, a `CaseTest` of type `jsonb` that the analyzer already made.
    pub(crate) fn json_func(&mut self, f: &JsonFuncExpr, context: Option<Expr>) -> Result<Expr> {
        let (op, func_name, default_format) = match f.op {
            RawOp::JSON_EXISTS_OP => (JsonExprOp::Exists, "JSON_EXISTS", JsonFormatKind::Default),
            RawOp::JSON_QUERY_OP => (JsonExprOp::Query, "JSON_QUERY", JsonFormatKind::Jsonb),
            RawOp::JSON_VALUE_OP => (JsonExprOp::Value, "JSON_VALUE", JsonFormatKind::Default),
            _ => return Err(Error::internal("JSON_TABLE as an expression")),
        };
        if op != JsonExprOp::Query
            && let Some(raw) = f
                .output
                .as_deref()
                .and_then(|o| o.returning.as_deref())
                .and_then(|r| r.format.as_deref())
        {
            let (format, at) = format_of(Some(raw));
            if format != JsonFormat::default() {
                return Err(Error::new(
                    SqlState::SYNTAX_ERROR,
                    format!("cannot specify FORMAT JSON in RETURNING clause of {func_name}()"),
                )
                .at_opt(at));
            }
        }
        let column = f.column_name.as_deref();
        let behaviors = [("ON EMPTY", f.on_empty.as_deref()), ("ON ERROR", f.on_error.as_deref())];
        match op {
            JsonExprOp::Query => {
                if f.quotes == JsonQuotes::JS_QUOTES_OMIT
                    && matches!(
                        f.wrapper,
                        RawWrapper::JSW_CONDITIONAL | RawWrapper::JSW_UNCONDITIONAL
                    )
                {
                    return Err(Error::new(
                        SqlState::SYNTAX_ERROR,
                        "SQL/JSON QUOTES behavior must not be specified when WITH WRAPPER is used",
                    )
                    .at_opt(place(f.location)));
                }
                check_behaviors(
                    &behaviors,
                    &[
                        JsonBehaviorKind::Error,
                        JsonBehaviorKind::Null,
                        JsonBehaviorKind::Empty,
                        JsonBehaviorKind::EmptyArray,
                        JsonBehaviorKind::EmptyObject,
                        JsonBehaviorKind::Default,
                    ],
                    "ERROR, NULL, EMPTY ARRAY, EMPTY OBJECT, or DEFAULT expression",
                    "JSON_QUERY()",
                    column,
                    "formatted columns",
                )?;
            }
            JsonExprOp::Exists => check_behaviors(
                &behaviors[1..],
                &[
                    JsonBehaviorKind::Error,
                    JsonBehaviorKind::True,
                    JsonBehaviorKind::False,
                    JsonBehaviorKind::Unknown,
                ],
                "ERROR, TRUE, FALSE, or UNKNOWN",
                "JSON_EXISTS()",
                column,
                "EXISTS columns",
            )?,
            JsonExprOp::Value => check_behaviors(
                &behaviors,
                &[JsonBehaviorKind::Error, JsonBehaviorKind::Null, JsonBehaviorKind::Default],
                "ERROR, NULL, or DEFAULT expression",
                "JSON_VALUE()",
                column,
                "scalar columns",
            )?,
        }
        let formatted = match context {
            Some(context) => context,
            None => self.json_value_expr(
                func_name,
                f.context_item.as_deref(),
                default_format,
                oid::JSONB,
                false,
            )?,
        };
        let path = self.transform(f.pathspec.as_ref())?;
        let (path_ty, path_at) = (path.ty, path.place());
        let Some(mut path) =
            self.coerce_to_target(path, oid::JSONPATH, -1, Context::Explicit, path_at)?
        else {
            return Err(Error::new(
                SqlState::DATATYPE_MISMATCH,
                format!(
                    "JSON path expression must be of type jsonpath, not of type {}",
                    types::name(path_ty)
                ),
            )
            .at_opt(path_at));
        };
        hide_cast(&mut path);
        let mut passing = Vec::new();
        for arg in &f.passing {
            let Some(Node::JsonArgument(arg)) = arg else {
                return Err(Error::internal("a PASSING argument that is not JsonArgument"));
            };
            let value = self.json_value_expr(
                func_name,
                arg.val.as_deref(),
                JsonFormatKind::Jsonb,
                0,
                true,
            )?;
            passing.push((arg.name.as_deref().unwrap_or_default().to_owned(), value));
        }
        let mut returning = self.json_output(f.output.as_deref(), false)?.map(|r| r.0);
        let mut json_coercion = false;
        let mut io_coercion = false;
        let mut omit_quotes = false;
        let mut wrapper = JsonWrapper::Unspec;
        let default_returning = |ty| {
            let kind = if ty == oid::JSONB { JsonFormatKind::Jsonb } else { JsonFormatKind::Json };
            JsonReturning { format: JsonFormat { kind, ..JsonFormat::default() }, ty, typmod: -1 }
        };
        let (returning, on_empty, on_error) = match op {
            JsonExprOp::Exists => {
                let returning = returning.take().unwrap_or_else(|| default_returning(oid::BOOL));
                json_coercion = returning.ty != oid::BOOL;
                let on_error =
                    self.json_behavior(f.on_error.as_deref(), JsonBehaviorKind::False, &returning)?;
                (returning, None, on_error)
            }
            JsonExprOp::Query => {
                let returning = returning.take().unwrap_or_else(|| default_returning(oid::JSONB));
                omit_quotes = f.quotes == JsonQuotes::JS_QUOTES_OMIT;
                wrapper = match f.wrapper {
                    RawWrapper::JSW_NONE => JsonWrapper::None,
                    RawWrapper::JSW_CONDITIONAL => JsonWrapper::Conditional,
                    RawWrapper::JSW_UNCONDITIONAL => JsonWrapper::Unconditional,
                    _ => JsonWrapper::Unspec,
                };
                json_coercion = returning.ty != oid::JSONB || omit_quotes;
                let on_empty =
                    self.json_behavior(f.on_empty.as_deref(), JsonBehaviorKind::Null, &returning)?;
                let on_error =
                    self.json_behavior(f.on_error.as_deref(), JsonBehaviorKind::Null, &returning)?;
                (returning, Some(on_empty), on_error)
            }
            JsonExprOp::Value => {
                let mut returning =
                    returning.take().unwrap_or_else(|| default_returning(oid::TEXT));
                returning.format = JsonFormat::default();
                omit_quotes = true;
                if returning.ty != oid::TEXT {
                    if domain_has_constraints(returning.ty) {
                        json_coercion = true;
                    } else {
                        io_coercion = true;
                    }
                }
                let on_empty =
                    self.json_behavior(f.on_empty.as_deref(), JsonBehaviorKind::Null, &returning)?;
                let on_error =
                    self.json_behavior(f.on_error.as_deref(), JsonBehaviorKind::Null, &returning)?;
                (returning, Some(on_empty), on_error)
            }
        };
        let (ty, typmod) = (returning.ty, returning.typmod);
        let j = JsonExpr {
            op,
            formatted,
            path,
            passing,
            returning,
            on_empty,
            on_error,
            wrapper,
            omit_quotes,
            json_coercion,
            io_coercion,
            column: column.map(str::to_string),
        };
        Ok(Expr { kind: ExprKind::JsonExpr(Box::new(j)), ty, typmod, location: place(f.location) })
    }

    /// `transformJsonBehavior`: an `ON EMPTY` or an `ON ERROR` clause, or the default behavior when the call has no clause, with its value cast to the type of `RETURNING`.
    fn json_behavior(
        &mut self,
        raw: Option<&RawBehavior>,
        default: JsonBehaviorKind,
        returning: &JsonReturning,
    ) -> Result<JsonBehavior> {
        let (kind, at) = match raw {
            Some(raw) => (behavior_kind(raw.btype), place(raw.location)),
            None => (default, None),
        };
        let mut expr = None;
        if kind == JsonBehaviorKind::Default
            && let Some(raw) = raw
        {
            let value = self.transform(raw.expr.as_ref())?;
            let fail = |message: &str| {
                Error::new(SqlState::DATATYPE_MISMATCH, message.to_owned()).at_opt(value.place())
            };
            if !valid_default(&value) {
                return Err(fail(
                    "can only specify a constant, non-aggregate function, or operator expression for DEFAULT",
                ));
            }
            if value.first_var().is_some() {
                return Err(fail("DEFAULT expression must not contain column references"));
            }
            if value.first_set_call().is_some() {
                return Err(fail("DEFAULT expression must not return a set"));
            }
            expr = Some(value);
        }
        if expr.is_none() && kind != JsonBehaviorKind::Error {
            expr = Some(behavior_const(kind, at));
        }
        let mut coerce = false;
        if let Some(value) = expr.take() {
            let null = matches!(value.kind, ExprKind::Const(Value::Null));
            let value = if value.ty != returning.ty || (returning.typmod >= 0 && !null) {
                if null
                    || value.ty == oid::JSONB
                    || (value.ty == oid::BOOL && types::base(returning.ty) != oid::INT4)
                {
                    coerce = true;
                    if value.ty == oid::BOOL {
                        let text = if kind == JsonBehaviorKind::True { "true" } else { "false" };
                        Expr::constant(Value::text(text), oid::JSONB, -1, value.location)
                    } else {
                        value
                    }
                } else {
                    let context = if matches!(types::category(returning.ty), b'S' | b'V') {
                        Context::Assignment
                    } else {
                        Context::Explicit
                    };
                    let (from, value_at) = (value.ty, value.place());
                    match self.coerce_to_target(
                        value,
                        returning.ty,
                        returning.typmod,
                        context,
                        at,
                    )? {
                        Some(coerced) => coerced,
                        None => {
                            let to = types::name(returning.ty);
                            let mut error = Error::new(
                                SqlState::CANNOT_COERCE,
                                format!(
                                    "cannot cast behavior expression of type {} to {to}",
                                    types::name(from)
                                ),
                            );
                            if kind == JsonBehaviorKind::Default {
                                error = error.with_hint(format!(
                                    "You will need to explicitly cast the expression to type {to}."
                                ));
                            }
                            return Err(error.at_opt(value_at));
                        }
                    }
                }
            } else {
                value
            };
            expr = Some(value);
        }
        Ok(JsonBehavior { kind, expr, coerce })
    }
}
