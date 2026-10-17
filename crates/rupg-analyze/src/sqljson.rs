//! The SQL/JSON constructors of `parse_expr.c`: `JSON_OBJECT`, `JSON_ARRAY`, `JSON`, `JSON_SCALAR`, `JSON_SERIALIZE`, `JSON_OBJECTAGG`, `JSON_ARRAYAGG` and `IS JSON`.

use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin;
use rupg_sql::nodes::{
    Alias, ColumnRef, JsonAggConstructor, JsonArrayAgg, JsonArrayConstructor,
    JsonArrayQueryConstructor, JsonEncoding as RawEncoding, JsonFormat as RawFormat,
    JsonFormatType as RawFormatType, JsonIsPredicate, JsonObjectAgg, JsonObjectConstructor,
    JsonOutput, JsonParseExpr, JsonScalarExpr, JsonSerializeExpr, JsonValueExpr, JsonValueType,
    Node, RangeSubselect, ResTarget, SelectStmt, SubLink as RawSubLink, SubLinkType,
};
use rupg_types::{Value, oid};

use crate::Analyzer;
use crate::agg::{Kind, Parts};
use crate::coerce::{AtOpt, Context};
use crate::expr::NullTreatment;
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
/// `json_agg`, `json_agg_strict`, `jsonb_agg` and `jsonb_agg_strict`, by `ABSENT ON NULL`.
const ARRAY_AGGS: [[u32; 2]; 2] = [[3175, 6276], [3267, 6284]];
/// `json_object_agg` and its forms for `jsonb`, by `WITH UNIQUE KEYS` and `ABSENT ON NULL`: the plain one, `_strict`, `_unique` and `_unique_strict`.
const OBJECT_AGGS: [[u32; 4]; 2] = [[3197, 6280, 6281, 6282], [3270, 6288, 6289, 6290]];

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
    #[allow(clippy::too_many_arguments)]
    fn json_constructor(
        &mut self,
        kind: JsonCtorKind,
        args: Vec<Expr>,
        func: Option<Expr>,
        (returning, format_at): (JsonReturning, Option<usize>),
        unique: bool,
        absent_on_null: bool,
        location: i32,
    ) -> Result<Expr> {
        let (value_ty, value_typmod) = match &func {
            Some(func) => (func.ty, func.typmod),
            None if returning.format.kind == JsonFormatKind::Jsonb => (oid::JSONB, -1),
            None => (oid::JSON, -1),
        };
        let coercion = if returning.ty == value_ty {
            None
        } else {
            let placeholder =
                Expr { typmod: value_typmod, ..Expr::new(ExprKind::CaseTest, value_ty) };
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
        let format = JsonFormat::default();
        let ctor =
            JsonCtor { kind, args, func, format, returning, unique, absent_on_null, coercion };
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
            None,
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
            None,
            returning,
            false,
            ctor.absent_on_null,
            ctor.location,
        )
    }

    /// `transformJsonArrayQueryConstructor`: `JSON_ARRAY` with a subquery, which gives `COALESCE((SELECT JSON_ARRAYAGG(q.a) FROM (subquery) q(a)), '[]')`. The analyzer first reads the subquery alone to check that it has one column.
    pub(crate) fn json_array_query(&mut self, ctor: &JsonArrayQueryConstructor) -> Result<Expr> {
        let at = place(ctor.location);
        let query = self.sub_analyze(ctor.query.as_ref())?;
        let mut columns = query.targets.iter().filter(|t| !t.junk);
        let (Some(column), None) = (columns.next(), columns.next()) else {
            return Err(Error::new(SqlState::SYNTAX_ERROR, "subquery must return only one column")
                .at_opt(at));
        };
        let arg = column.expr.clone();
        let colref = Node::ColumnRef(Box::new(ColumnRef {
            fields: vec![Some(Node::String("q".into())), Some(Node::String("a".into()))],
            location: ctor.location,
        }));
        let value = JsonValueExpr {
            raw_expr: Some(colref.clone()),
            formatted_expr: Some(colref),
            format: ctor.format.clone(),
        };
        let constructor = JsonAggConstructor {
            output: ctor.output.clone(),
            location: ctor.location,
            ..Default::default()
        };
        let agg = JsonArrayAgg {
            constructor: Some(Box::new(constructor)),
            arg: Some(Box::new(value)),
            absent_on_null: ctor.absent_on_null,
        };
        let target = ResTarget {
            val: Some(Node::JsonArrayAgg(Box::new(agg))),
            location: ctor.location,
            ..Default::default()
        };
        let alias =
            Alias { aliasname: Some("q".into()), colnames: vec![Some(Node::String("a".into()))] };
        let range = RangeSubselect {
            lateral: false,
            subquery: ctor.query.clone(),
            alias: Some(Box::new(alias)),
        };
        let select = SelectStmt {
            targetList: vec![Some(Node::ResTarget(Box::new(target)))],
            fromClause: vec![Some(Node::RangeSubselect(Box::new(range)))],
            ..Default::default()
        };
        let sublink = RawSubLink {
            subLinkType: SubLinkType::EXPR_SUBLINK,
            subselect: Some(Node::SelectStmt(Box::new(select))),
            location: ctor.location,
            ..Default::default()
        };
        let exec = self.transform(Some(&Node::SubLink(Box::new(sublink))))?;
        // The empty array has the type and the type modifier of the result, so that a short `RETURNING` type applies to it too.
        let (ty, typmod) = (exec.ty, exec.typmod);
        let empty = Expr::constant(Value::text("[]"), oid::UNKNOWN, -1, at);
        let empty = self
            .coerce_to_target(empty, ty, typmod, Context::Assignment, at)?
            .ok_or_else(|| cannot_cast(oid::UNKNOWN, ty, at))?;
        let coalesce =
            Expr { kind: ExprKind::Coalesce(vec![exec, empty]), ty, typmod, location: at };
        let returning = self.json_constructor_output(ctor.output.as_deref(), &[arg])?;
        let mut result = self.json_constructor(
            JsonCtorKind::ArrayQuery,
            Vec::new(),
            Some(coalesce),
            returning,
            false,
            ctor.absent_on_null,
            ctor.location,
        )?;
        if let ExprKind::JsonCtor(c) = &mut result.kind {
            c.format = format_of(ctor.format.as_deref()).0;
        }
        Ok(result)
    }

    /// `transformJsonAggConstructor`: the constructor of `JSON_OBJECTAGG` or `JSON_ARRAYAGG` with the call of the aggregate, or of the window function when the call has `OVER`.
    #[allow(clippy::too_many_arguments)]
    fn json_agg_constructor(
        &mut self,
        agg_ctor: Option<&JsonAggConstructor>,
        returning: (JsonReturning, Option<usize>),
        args: Vec<Expr>,
        func: u32,
        kind: JsonCtorKind,
        unique: bool,
        absent_on_null: bool,
    ) -> Result<Expr> {
        let Some(agg_ctor) = agg_ctor else {
            return Err(Error::internal("a SQL/JSON aggregate with no constructor"));
        };
        let at = place(agg_ctor.location);
        let ty =
            if returning.0.format.kind == JsonFormatKind::Jsonb { oid::JSONB } else { oid::JSON };
        let filter = match &agg_ctor.agg_filter {
            Some(node) => {
                let expr = self.with_kind(Kind::Filter, |a| a.transform(Some(node)))?;
                Some(self.coerce_to_boolean(expr, "FILTER")?)
            }
            None => None,
        };
        let parts = Parts {
            star: false,
            distinct: false,
            within_group: false,
            order: &agg_ctor.agg_order,
            filter,
            over: agg_ctor.over.as_deref(),
            nulls: NullTreatment::None,
            srfs: self.srfs.0,
        };
        let call = if agg_ctor.over.is_some() {
            let proc = builtin::proc_by_oid(func).ok_or_else(|| {
                Error::internal(format!("cache lookup failed for function {func}"))
            })?;
            self.window_call(proc, Some(proc.name), args, parts, ty, at)?
        } else {
            self.aggregate_call(func, b'n', args, parts, false, ty, at)?
        };
        self.json_constructor(
            kind,
            Vec::new(),
            Some(call),
            returning,
            unique,
            absent_on_null,
            agg_ctor.location,
        )
    }

    /// `transformJsonObjectAgg`: a call of `json_object_agg` or of `jsonb_object_agg`, with `_unique` for `WITH UNIQUE KEYS` and `_strict` for `ABSENT ON NULL`.
    pub(crate) fn json_object_agg(&mut self, agg: &JsonObjectAgg) -> Result<Expr> {
        let Some(kv) = agg.arg.as_deref() else {
            return Err(Error::internal("a JSON_OBJECTAGG with no argument"));
        };
        let key = self.transform(kv.key.as_ref())?;
        let value = kv.value.as_deref();
        let value = self.json_value_expr("JSON_OBJECTAGG()", value, JsonFormatKind::Default, 0)?;
        let args = vec![key, value];
        let ctor = agg.constructor.as_deref();
        let returning =
            self.json_constructor_output(ctor.and_then(|c| c.output.as_deref()), &args)?;
        let jsonb = usize::from(returning.0.format.kind == JsonFormatKind::Jsonb);
        let form = usize::from(agg.absent_on_null) + 2 * usize::from(agg.unique);
        self.json_agg_constructor(
            ctor,
            returning,
            args,
            OBJECT_AGGS[jsonb][form],
            JsonCtorKind::ObjectAgg,
            agg.unique,
            agg.absent_on_null,
        )
    }

    /// `transformJsonArrayAgg`: a call of `json_agg` or of `jsonb_agg`, with `_strict` for `ABSENT ON NULL`.
    pub(crate) fn json_array_agg(&mut self, agg: &JsonArrayAgg) -> Result<Expr> {
        let value = agg.arg.as_deref();
        let arg = self.json_value_expr("JSON_ARRAYAGG()", value, JsonFormatKind::Default, 0)?;
        let args = vec![arg];
        let ctor = agg.constructor.as_deref();
        let returning =
            self.json_constructor_output(ctor.and_then(|c| c.output.as_deref()), &args)?;
        let jsonb = usize::from(returning.0.format.kind == JsonFormatKind::Jsonb);
        self.json_agg_constructor(
            ctor,
            returning,
            args,
            ARRAY_AGGS[jsonb][usize::from(agg.absent_on_null)],
            JsonCtorKind::ArrayAgg,
            false,
            agg.absent_on_null,
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
            None,
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
            None,
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
            None,
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
