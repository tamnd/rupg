//! The SQL/JSON constructors of `parse_expr.c`: `JSON_OBJECT`, `JSON_ARRAY`, `JSON`, `JSON_SCALAR`, `JSON_SERIALIZE` and `IS JSON`.

use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{
    JsonArrayConstructor, JsonEncoding as RawEncoding, JsonFormat as RawFormat,
    JsonFormatType as RawFormatType, JsonIsPredicate, JsonObjectConstructor, JsonOutput,
    JsonParseExpr, JsonScalarExpr, JsonSerializeExpr, JsonValueExpr, JsonValueType, Node,
};
use rupg_types::{Value, oid};

use crate::Analyzer;
use crate::coerce::{AtOpt, Context};
use crate::expr::{
    Expr, ExprKind, Func, FuncForm, IsJson, JsonCtor, JsonCtorKind, JsonEncoding, JsonFormat,
    JsonFormatKind, JsonItemType, JsonReturning, JsonValue,
};
use crate::typename::place;
use crate::types;

/// `convert_from(bytea, name)`.
const CONVERT_FROM: u32 = 1714;
/// `convert_to(text, name)`.
const CONVERT_TO: u32 = 1717;
/// `to_json(anyelement)`.
const TO_JSON: u32 = 3176;
/// `to_jsonb(anyelement)`.
const TO_JSONB: u32 = 3787;

/// A `FORMAT` clause of the raw tree with its place.
fn format_of(raw: Option<&RawFormat>) -> (JsonFormat, Option<usize>) {
    let Some(raw) = raw else { return (JsonFormat::default(), None) };
    let kind = match raw.format_type {
        RawFormatType::JS_FORMAT_JSON => JsonFormatKind::Json,
        RawFormatType::JS_FORMAT_JSONB => JsonFormatKind::Jsonb,
        _ => JsonFormatKind::Default,
    };
    let encoding = match raw.encoding {
        RawEncoding::JS_ENC_UTF8 => JsonEncoding::Utf8,
        RawEncoding::JS_ENC_UTF16 => JsonEncoding::Utf16,
        RawEncoding::JS_ENC_UTF32 => JsonEncoding::Utf32,
        _ => JsonEncoding::Default,
    };
    (JsonFormat { kind, encoding }, place(raw.location))
}

/// `getJsonEncodingConst`: the name of the encoding of a format, UTF8 when the format has none.
fn encoding_const(format: JsonFormat) -> Expr {
    let encoding =
        if format.kind == JsonFormatKind::Default { JsonEncoding::Utf8 } else { format.encoding };
    Expr::constant(Value::text(encoding.name()), oid::NAME, -1, None)
}

/// A call of a built-in function that the analyzer adds.
fn call(func: u32, args: Vec<Expr>, ty: u32, at: Option<usize>) -> Expr {
    let kind = ExprKind::Func(Func {
        oid: func,
        args,
        form: FuncForm::Call,
        variadic: false,
        retset: false,
    });
    Expr { kind, ty, typmod: -1, location: at }
}

/// The error of a cast that does not exist.
fn cannot_cast(from: u32, to: u32, at: Option<usize>) -> Error {
    Error::new(
        SqlState::CANNOT_COERCE,
        format!("cannot cast type {} to {}", types::name(from), types::name(to)),
    )
    .at_opt(at)
}

/// True for a type of the string category.
fn is_string(ty: u32) -> bool {
    types::category(ty) == b'S'
}

impl Analyzer<'_> {
    /// `transformJsonValueExpr`: an input of a constructor, as a `json` or a `jsonb` value when its format or `target` asks for one.
    fn json_value_expr(
        &mut self,
        construct: &str,
        ve: Option<&JsonValueExpr>,
        default_format: JsonFormatKind,
        target: u32,
    ) -> Result<Expr> {
        let raw_node = ve.and_then(|ve| ve.raw_expr.as_ref());
        let (ve_format, format_at) = format_of(ve.and_then(|ve| ve.format.as_deref()));
        let mut expr = self.transform(raw_node)?;
        if expr.ty == oid::UNKNOWN {
            expr = self.coerce_to_specific(expr, oid::TEXT, construct)?;
        }
        let raw = expr.clone();
        let mut ty = expr.ty;
        let at = expr.place();
        let json = matches!(ty, oid::JSON | oid::JSONB);
        let format = if ve_format.kind != JsonFormatKind::Default {
            if ve_format.encoding != JsonEncoding::Default && ty != oid::BYTEA {
                return Err(Error::new(
                    SqlState::DATATYPE_MISMATCH,
                    "JSON ENCODING clause is only allowed for bytea input type",
                )
                .at_opt(format_at));
            }
            if json { JsonFormatKind::Default } else { ve_format.kind }
        } else if json {
            JsonFormatKind::Default
        } else {
            default_format
        };
        if format == JsonFormatKind::Default && (target == 0 || ty == target) {
            return Ok(expr);
        }
        let only_cast = target != 0;
        if !only_cast && ty != oid::BYTEA && !is_string(ty) {
            let message = if ve_format.kind == JsonFormatKind::Default {
                "cannot use non-string types with implicit FORMAT JSON clause"
            } else {
                "cannot use non-string types with explicit FORMAT JSON clause"
            };
            return Err(Error::new(SqlState::DATATYPE_MISMATCH, message).at_opt(format_at.or(at)));
        }
        if format == JsonFormatKind::Json && ty == oid::BYTEA {
            expr = call(CONVERT_FROM, vec![expr, encoding_const(ve_format)], oid::TEXT, at);
            ty = oid::TEXT;
        }
        let target = match target {
            0 if format == JsonFormatKind::Jsonb => oid::JSONB,
            0 => oid::JSON,
            target => target,
        };
        let before = expr.clone();
        let coerced = match self.coerce_to_target(expr, target, -1, Context::Explicit, at)? {
            Some(coerced) => coerced,
            None if only_cast => return Err(cannot_cast(ty, target, at)),
            None => {
                let func = if target == oid::JSON { TO_JSON } else { TO_JSONB };
                call(func, vec![before.clone()], target, at)
            }
        };
        if coerced == before {
            return Ok(raw);
        }
        let (ty, typmod) = (coerced.ty, coerced.typmod);
        let location = raw.place();
        let value = JsonValue { raw, formatted: coerced, format: ve_format };
        Ok(Expr { kind: ExprKind::JsonValue(Box::new(value)), ty, typmod, location })
    }

    /// `transformJsonOutput` and `checkJsonOutputFormat`: the type of `RETURNING` and its format, or `None` when the call has no clause. The place is the place of the `FORMAT` clause.
    fn json_output(
        &mut self,
        output: Option<&JsonOutput>,
        allow_format: bool,
    ) -> Result<Option<(JsonReturning, Option<usize>)>> {
        let Some(output) = output else { return Ok(None) };
        let Some(type_name) = output.typeName.as_deref() else {
            return Err(Error::internal("a RETURNING clause with no type"));
        };
        let (ty, typmod) = self.type_name(type_name)?;
        if type_name.setof {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "returning SETOF types is not supported in SQL/JSON functions",
            ));
        }
        if types::row(ty).is_some_and(|r| r.kind == b'p') {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "returning pseudo-types is not supported in SQL/JSON functions",
            ));
        }
        let (mut format, at) =
            format_of(output.returning.as_deref().and_then(|r| r.format.as_deref()));
        if format.kind == JsonFormatKind::Default {
            format.kind =
                if ty == oid::JSONB { JsonFormatKind::Jsonb } else { JsonFormatKind::Json };
        } else {
            if !allow_format && !matches!(ty, oid::BYTEA | oid::JSON | oid::JSONB) && !is_string(ty)
            {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    "cannot use JSON format with non-string output types",
                )
                .at_opt(at));
            }
            if format.kind == JsonFormatKind::Json {
                if ty != oid::BYTEA && format.encoding != JsonEncoding::Default {
                    return Err(Error::new(
                        SqlState::FEATURE_NOT_SUPPORTED,
                        "cannot set JSON encoding for non-bytea output types",
                    )
                    .at_opt(at));
                }
                if !matches!(format.encoding, JsonEncoding::Default | JsonEncoding::Utf8) {
                    return Err(Error::new(
                        SqlState::FEATURE_NOT_SUPPORTED,
                        "unsupported JSON encoding",
                    )
                    .with_hint("Only UTF8 JSON encoding is supported.")
                    .at_opt(at));
                }
            }
        }
        Ok(Some((JsonReturning { format, ty, typmod }, at)))
    }

    /// `transformJsonConstructorOutput`: with no `RETURNING`, the result is `jsonb` when an argument is `jsonb`, or else `json`.
    fn json_constructor_output(
        &mut self,
        output: Option<&JsonOutput>,
        args: &[Expr],
    ) -> Result<(JsonReturning, Option<usize>)> {
        if let Some(returning) = self.json_output(output, true)? {
            return Ok(returning);
        }
        Ok((json_returning(args.iter().any(|a| a.ty == oid::JSONB)), None))
    }

    /// `transformJsonReturning`: the `RETURNING` clause of `JSON` and `JSON_SCALAR`, which must be `json` or `jsonb`.
    fn json_returning(
        &mut self,
        output: Option<&JsonOutput>,
        construct: &str,
    ) -> Result<(JsonReturning, Option<usize>)> {
        let Some(returning) = self.json_output(output, false)? else {
            return Ok((json_returning(false), None));
        };
        if !matches!(returning.0.ty, oid::JSON | oid::JSONB) {
            let at = output.and_then(|o| o.typeName.as_deref()).and_then(|t| place(t.location));
            return Err(Error::new(
                SqlState::DATATYPE_MISMATCH,
                format!(
                    "cannot use type {} in RETURNING clause of {construct}",
                    types::name(returning.0.ty)
                ),
            )
            .with_hint("Try returning json or jsonb.")
            .at_opt(at));
        }
        Ok(returning)
    }

    /// `makeJsonConstructorExpr` with `coerceJsonFuncExpr`: the constructor with the cast of its `json` or `jsonb` value to the type of `RETURNING`.
    fn json_constructor(
        &mut self,
        kind: JsonCtorKind,
        args: Vec<Expr>,
        (returning, format_at): (JsonReturning, Option<usize>),
        unique: bool,
        absent_on_null: bool,
        location: i32,
    ) -> Result<Expr> {
        let value_ty =
            if returning.format.kind == JsonFormatKind::Jsonb { oid::JSONB } else { oid::JSON };
        let coercion = if returning.ty == value_ty {
            None
        } else {
            let placeholder = Expr::new(ExprKind::CaseTest, value_ty);
            if returning.format.kind == JsonFormatKind::Json && returning.ty == oid::BYTEA {
                let text = self.coerce_to_specific(placeholder, oid::TEXT, "JSON_FUNCTION")?;
                let encoding = encoding_const(returning.format);
                Some(call(CONVERT_TO, vec![text, encoding], oid::BYTEA, format_at))
            } else {
                let coerced = self.coerce_to_target(
                    placeholder,
                    returning.ty,
                    returning.typmod,
                    Context::Assignment,
                    format_at,
                )?;
                Some(coerced.ok_or_else(|| cannot_cast(value_ty, returning.ty, format_at))?)
            }
        };
        let (ty, typmod) = (returning.ty, returning.typmod);
        let ctor = JsonCtor { kind, args, returning, unique, absent_on_null, coercion };
        Ok(Expr { kind: ExprKind::JsonCtor(Box::new(ctor)), ty, typmod, location: place(location) })
    }

    /// `transformJsonObjectConstructor`.
    pub(crate) fn json_object(&mut self, ctor: &JsonObjectConstructor) -> Result<Expr> {
        let mut args = Vec::new();
        for item in &ctor.exprs {
            let Some(Node::JsonKeyValue(kv)) = item else {
                return Err(Error::internal("a JSON_OBJECT argument that is not JsonKeyValue"));
            };
            args.push(self.transform(kv.key.as_ref())?);
            let value = kv.value.as_deref();
            args.push(self.json_value_expr("JSON_OBJECT()", value, JsonFormatKind::Default, 0)?);
        }
        let returning = self.json_constructor_output(ctor.output.as_deref(), &args)?;
        self.json_constructor(
            JsonCtorKind::Object,
            args,
            returning,
            ctor.unique,
            ctor.absent_on_null,
            ctor.location,
        )
    }

    /// `transformJsonArrayConstructor`.
    pub(crate) fn json_array(&mut self, ctor: &JsonArrayConstructor) -> Result<Expr> {
        let mut args = Vec::new();
        for item in &ctor.exprs {
            let Some(Node::JsonValueExpr(ve)) = item else {
                return Err(Error::internal("a JSON_ARRAY argument that is not JsonValueExpr"));
            };
            args.push(self.json_value_expr(
                "JSON_ARRAY()",
                Some(ve),
                JsonFormatKind::Default,
                0,
            )?);
        }
        let returning = self.json_constructor_output(ctor.output.as_deref(), &args)?;
        self.json_constructor(
            JsonCtorKind::Array,
            args,
            returning,
            false,
            ctor.absent_on_null,
            ctor.location,
        )
    }

    /// `transformJsonParseArg`: the input of `IS JSON` and of `JSON` with `WITH UNIQUE KEYS`, as `text`, `json` or `jsonb`, and its type.
    fn json_parse_arg(
        &mut self,
        node: Option<&Node>,
        raw_format: Option<&RawFormat>,
    ) -> Result<(Expr, u32)> {
        let raw = self.transform(node)?;
        let (format, format_at) = format_of(raw_format);
        let ty = types::base(raw.ty);
        if ty == oid::BYTEA {
            let at = raw.place();
            let text = call(CONVERT_FROM, vec![raw.clone(), encoding_const(format)], oid::TEXT, at);
            let value = JsonValue { raw, formatted: text, format };
            let expr = Expr {
                kind: ExprKind::JsonValue(Box::new(value)),
                ty: oid::TEXT,
                typmod: -1,
                location: at,
            };
            return Ok((expr, oid::TEXT));
        }
        let mut expr = raw;
        let mut ty = ty;
        if ty == oid::UNKNOWN || is_string(ty) {
            let at = expr.place();
            expr = self
                .coerce_to_target(expr, oid::TEXT, -1, Context::Implicit, None)?
                .ok_or_else(|| cannot_cast(ty, oid::TEXT, at))?;
            ty = oid::TEXT;
        }
        if format.encoding != JsonEncoding::Default {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "cannot use JSON FORMAT ENCODING clause for non-bytea input types",
            )
            .at_opt(format_at));
        }
        Ok((expr, ty))
    }

    /// `transformJsonIsPredicate`.
    pub(crate) fn is_json(&mut self, pred: &JsonIsPredicate) -> Result<Expr> {
        let (expr, ty) = self.json_parse_arg(pred.expr.as_ref(), pred.format.as_deref())?;
        if !matches!(ty, oid::TEXT | oid::JSON | oid::JSONB) {
            return Err(Error::new(
                SqlState::DATATYPE_MISMATCH,
                format!("cannot use type {} in IS JSON predicate", types::name(expr.ty)),
            )
            .at_opt(expr.place()));
        }
        let item = match pred.item_type {
            JsonValueType::JS_TYPE_OBJECT => JsonItemType::Object,
            JsonValueType::JS_TYPE_ARRAY => JsonItemType::Array,
            JsonValueType::JS_TYPE_SCALAR => JsonItemType::Scalar,
            _ => JsonItemType::Any,
        };
        let pred_expr = IsJson { expr, item, unique: pred.unique_keys, base_type: ty };
        Ok(Expr::new(ExprKind::IsJson(Box::new(pred_expr)), oid::BOOL).at(place(pred.location)))
    }

    /// `transformJsonParseExpr`: `JSON(...)`.
    pub(crate) fn json_parse(&mut self, jsexpr: &JsonParseExpr) -> Result<Expr> {
        let returning = self.json_returning(jsexpr.output.as_deref(), "JSON()")?;
        let ve = jsexpr.expr.as_deref();
        let arg = if jsexpr.unique_keys {
            let node = ve.and_then(|ve| ve.raw_expr.as_ref());
            let (arg, ty) = self.json_parse_arg(node, ve.and_then(|ve| ve.format.as_deref()))?;
            if ty != oid::TEXT {
                return Err(Error::new(
                    SqlState::DATATYPE_MISMATCH,
                    "cannot use non-string types with WITH UNIQUE KEYS clause",
                )
                .at_opt(place(jsexpr.location)));
            }
            arg
        } else {
            self.json_value_expr("JSON()", ve, JsonFormatKind::Json, returning.0.ty)?
        };
        self.json_constructor(
            JsonCtorKind::Parse,
            vec![arg],
            returning,
            jsexpr.unique_keys,
            false,
            jsexpr.location,
        )
    }

    /// `transformJsonScalarExpr`: `JSON_SCALAR(...)`.
    pub(crate) fn json_scalar(&mut self, jsexpr: &JsonScalarExpr) -> Result<Expr> {
        let mut arg = self.transform(jsexpr.expr.as_ref())?;
        let returning = self.json_returning(jsexpr.output.as_deref(), "JSON_SCALAR()")?;
        if arg.ty == oid::UNKNOWN {
            arg = self.coerce_to_specific(arg, oid::TEXT, "JSON_SCALAR")?;
        }
        self.json_constructor(
            JsonCtorKind::Scalar,
            vec![arg],
            returning,
            false,
            false,
            jsexpr.location,
        )
    }

    /// `transformJsonSerializeExpr`: `JSON_SERIALIZE(...)`.
    pub(crate) fn json_serialize(&mut self, jsexpr: &JsonSerializeExpr) -> Result<Expr> {
        let arg = self.json_value_expr(
            "JSON_SERIALIZE()",
            jsexpr.expr.as_deref(),
            JsonFormatKind::Json,
            0,
        )?;
        let returning = match self.json_output(jsexpr.output.as_deref(), true)? {
            Some(returning) => {
                let ty = returning.0.ty;
                if ty != oid::BYTEA && !is_string(ty) {
                    return Err(Error::new(
                        SqlState::DATATYPE_MISMATCH,
                        format!(
                            "cannot use type {} in RETURNING clause of JSON_SERIALIZE()",
                            types::name(ty)
                        ),
                    )
                    .with_hint("Try returning a string type or bytea."));
                }
                returning
            }
            None => {
                let format = JsonFormat { kind: JsonFormatKind::Json, ..JsonFormat::default() };
                (JsonReturning { format, ty: oid::TEXT, typmod: -1 }, None)
            }
        };
        self.json_constructor(
            JsonCtorKind::Serialize,
            vec![arg],
            returning,
            false,
            false,
            jsexpr.location,
        )
    }
}

/// The `RETURNING` of a call with no clause: `jsonb` or `json`.
fn json_returning(jsonb: bool) -> JsonReturning {
    let (kind, ty) =
        if jsonb { (JsonFormatKind::Jsonb, oid::JSONB) } else { (JsonFormatKind::Json, oid::JSON) };
    JsonReturning { format: JsonFormat { kind, ..JsonFormat::default() }, ty, typmod: -1 }
}
