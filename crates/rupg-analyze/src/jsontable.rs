//! `parse_jsontable.c`: `JSON_TABLE` in `FROM`.

use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{
    A_Const, JsonBehaviorType, JsonExprOp as RawOp, JsonFuncExpr, JsonOutput, JsonQuotes,
    JsonReturning as RawReturning, JsonTable as RawTable, JsonTableColumn as RawColumn,
    JsonTableColumnType, JsonTablePathSpec, JsonWrapper as RawWrapper, Node,
};
use rupg_types::{Value, oid};

use crate::Analyzer;
use crate::coerce::{AtOpt, Context};
use crate::expr::{
    Expr, ExprKind, JsonBehaviorKind, JsonFormatKind, JsonTable, JsonTableColumn, JsonTablePlan,
    JsonTableScan,
};
use crate::typename::place;
use crate::types;

/// `JsonTableParseContext`: the names of the paths and of the columns, and the columns that the transform made.
struct Parts<'n> {
    /// `pathNames`: the names of the paths and of the columns.
    names: Vec<String>,
    /// `pathNameId`: the number of the next name that the analyzer gives a path.
    next: usize,
    /// The raw `PASSING` clause, which each column transforms again.
    passing: &'n [Option<Node>],
    /// True for `ERROR ON ERROR` at the top.
    error_on_error: bool,
    columns: Vec<JsonTableColumn>,
}

impl Parts<'_> {
    /// `generateJsonTablePathName`.
    fn generate(&mut self) -> String {
        let name = format!("json_table_path_{}", self.next);
        self.next += 1;
        self.names.push(name.clone());
        name
    }

    /// `CheckDuplicateColumnOrPathNames`: the error for a name of a column or of a path that the query gives twice.
    fn check_duplicates(&mut self, columns: &[Option<Node>]) -> Result<()> {
        for node in columns {
            let jtc = column_of(node.as_ref())?;
            let (name, at) = if jtc.coltype == JsonTableColumnType::JTC_NESTED {
                let spec = jtc.pathspec.as_deref();
                match spec.and_then(|s| s.name.as_deref()) {
                    Some(name) => (Some(name), spec.and_then(|s| place(s.name_location))),
                    None => (None, None),
                }
            } else {
                (jtc.name.as_deref(), place(jtc.location))
            };
            if let Some(name) = name {
                if self.names.iter().any(|n| n == name) {
                    return Err(Error::new(
                        SqlState::DUPLICATE_ALIAS,
                        format!("duplicate JSON_TABLE column or path name: {name}"),
                    )
                    .at_opt(at));
                }
                self.names.push(name.to_string());
            }
            if jtc.coltype == JsonTableColumnType::JTC_NESTED {
                self.check_duplicates(&jtc.columns)?;
            }
        }
        Ok(())
    }
}

/// A column of the raw tree.
fn column_of(node: Option<&Node>) -> Result<&RawColumn> {
    match node {
        Some(Node::JsonTableColumn(c)) => Ok(c),
        _ => Err(Error::internal("a JSON_TABLE column that is not JsonTableColumn")),
    }
}

/// `isCompositeType`: true for a type that `JSON_QUERY` gives better than `JSON_VALUE`.
fn is_composite(ty: u32) -> bool {
    let kind = types::row(ty).map_or(0, |r| r.kind);
    matches!(ty, oid::JSON | oid::JSONB | oid::RECORD)
        || types::element(ty) != 0
        || kind == b'c'
        || (kind == b'd' && is_composite(types::base(ty)))
}

/// `transformJsonTableColumn`: the call of `JSON_VALUE`, `JSON_EXISTS` or `JSON_QUERY` that gives the value of a column.
fn column_func(
    jtc: &RawColumn,
    coltype: JsonTableColumnType,
    passing: &[Option<Node>],
) -> JsonFuncExpr {
    let op = match coltype {
        JsonTableColumnType::JTC_REGULAR => RawOp::JSON_VALUE_OP,
        JsonTableColumnType::JTC_EXISTS => RawOp::JSON_EXISTS_OP,
        _ => RawOp::JSON_QUERY_OP,
    };
    let pathspec = match jtc.pathspec.as_deref() {
        Some(spec) => spec.string.clone(),
        None => {
            let mut path = String::from("$.");
            rupg_types::json_escape(jtc.name.as_deref().unwrap_or_default(), &mut path);
            let path =
                A_Const { val: Some(Node::String(path.into())), isnull: false, location: -1 };
            Some(Node::A_Const(Box::new(path)))
        }
    };
    let returning = RawReturning { format: jtc.format.clone(), ..RawReturning::default() };
    let output =
        JsonOutput { typeName: jtc.typeName.clone(), returning: Some(Box::new(returning)) };
    JsonFuncExpr {
        op,
        column_name: jtc.name.clone(),
        context_item: None,
        pathspec,
        passing: passing.to_vec(),
        output: Some(Box::new(output)),
        on_empty: jtc.on_empty.clone(),
        on_error: jtc.on_error.clone(),
        wrapper: jtc.wrapper,
        quotes: jtc.quotes,
        location: jtc.location,
    }
}

impl Analyzer<'_> {
    /// `transformJsonTable`: the call of `JSON_TABLE` and its columns. The caller makes the parts of `FROM` before it visible.
    pub(crate) fn json_table(&mut self, jt: &RawTable) -> Result<(Expr, Vec<JsonTableColumn>)> {
        let root =
            jt.pathspec.as_deref().ok_or_else(|| Error::internal("JSON_TABLE with no path"))?;
        if let Some(on_error) = jt.on_error.as_deref()
            && !matches!(
                on_error.btype,
                JsonBehaviorType::JSON_BEHAVIOR_ERROR
                    | JsonBehaviorType::JSON_BEHAVIOR_EMPTY
                    | JsonBehaviorType::JSON_BEHAVIOR_EMPTY_ARRAY
            )
        {
            return Err(Error::new(SqlState::SYNTAX_ERROR, "invalid ON ERROR behavior")
                .with_detail(
                    "Only EMPTY [ ARRAY ] or ERROR is allowed in the top-level ON ERROR clause.",
                )
                .at_opt(place(on_error.location)));
        }
        let on_error = match jt.on_error.as_deref().map(|b| b.btype) {
            Some(JsonBehaviorType::JSON_BEHAVIOR_ERROR) => JsonBehaviorKind::Error,
            Some(JsonBehaviorType::JSON_BEHAVIOR_EMPTY) => JsonBehaviorKind::Empty,
            _ => JsonBehaviorKind::EmptyArray,
        };
        let mut parts = Parts {
            names: Vec::new(),
            next: 0,
            passing: &jt.passing,
            error_on_error: on_error == JsonBehaviorKind::Error,
            columns: Vec::new(),
        };
        let name = match root.name.as_deref() {
            Some(name) => {
                parts.names.push(name.to_string());
                name.to_string()
            }
            None => parts.generate(),
        };
        parts.check_duplicates(&jt.columns)?;
        // The `JSON_TABLE_OP` node of the context item and of the path.
        let context = self.json_value_expr(
            "JSON_TABLE",
            jt.context_item.as_deref(),
            JsonFormatKind::Jsonb,
            oid::JSONB,
            false,
        )?;
        self.json_table_path(root, true)?;
        let mut passing = Vec::new();
        for arg in &jt.passing {
            let Some(Node::JsonArgument(arg)) = arg else {
                return Err(Error::internal("a PASSING argument that is not JsonArgument"));
            };
            let value = self.json_value_expr(
                "JSON_TABLE",
                arg.val.as_deref(),
                JsonFormatKind::Jsonb,
                0,
                true,
            )?;
            passing.push((arg.name.as_deref().unwrap_or_default().to_owned(), value));
        }
        let plan = self.json_table_columns(&mut parts, &jt.columns, root, name)?;
        let columns = parts.columns;
        let table = JsonTable { context, passing, on_error, plan, columns: columns.clone() };
        let call =
            Expr::new(ExprKind::JsonTable(Box::new(table)), oid::RECORD).at(place(jt.location));
        Ok((call, columns))
    }

    /// The path of a scan as a `jsonpath` value. The path of the top shows the position of an error, as the transform of the `JSON_TABLE_OP` node does, and a nested path does not, as `makeJsonTablePathScan` reads it.
    fn json_table_path(&mut self, spec: &JsonTablePathSpec, top: bool) -> Result<Value> {
        let path = self.transform(spec.string.as_ref())?;
        let path = if top { path } else { path.at(None) };
        let at = path.place();
        let path = self.coerce_to_target(path, oid::JSONPATH, -1, Context::Explicit, at)?;
        match path.as_ref().and_then(Expr::as_const) {
            Some(value) => Ok(value.clone()),
            None => Err(Error::internal("a JSON_TABLE path that is not a constant")),
        }
    }

    /// `transformJsonTableColumns` and `transformJsonTableNestedColumns`: the scan of a path, with the columns of this level and the plans of the `NESTED` parts.
    fn json_table_columns(
        &mut self,
        parts: &mut Parts<'_>,
        columns: &[Option<Node>],
        spec: &JsonTablePathSpec,
        name: String,
    ) -> Result<JsonTablePlan> {
        let start = parts.columns.len();
        let mut ordinality = false;
        for node in columns {
            let jtc = column_of(node.as_ref())?;
            let mut coltype = jtc.coltype;
            if coltype == JsonTableColumnType::JTC_NESTED {
                continue;
            }
            let name = jtc.name.as_deref().unwrap_or_default().to_string();
            if coltype == JsonTableColumnType::JTC_FOR_ORDINALITY {
                if ordinality {
                    return Err(Error::new(
                        SqlState::SYNTAX_ERROR,
                        "only one FOR ORDINALITY column is allowed",
                    )
                    .at_opt(place(jtc.location)));
                }
                ordinality = true;
                parts.columns.push(JsonTableColumn { name, ty: oid::INT4, typmod: -1, expr: None });
                continue;
            }
            if coltype == JsonTableColumnType::JTC_REGULAR {
                let type_name = jtc
                    .typeName
                    .as_deref()
                    .ok_or_else(|| Error::internal("a JSON_TABLE column with no type"))?;
                let (ty, _) = self.type_name(type_name)?;
                if is_composite(ty)
                    || jtc.quotes != JsonQuotes::JS_QUOTES_UNSPEC
                    || jtc.wrapper != RawWrapper::JSW_UNSPEC
                {
                    coltype = JsonTableColumnType::JTC_FORMATTED;
                }
            }
            let f = column_func(jtc, coltype, parts.passing);
            let expr = self.json_func(&f, Some(Expr::new(ExprKind::CaseTest, oid::JSONB)))?;
            let (ty, typmod) = (expr.ty, expr.typmod);
            parts.columns.push(JsonTableColumn { name, ty, typmod, expr: Some(expr) });
        }
        let end = parts.columns.len();
        let mut child: Option<JsonTablePlan> = None;
        for node in columns {
            let jtc = column_of(node.as_ref())?;
            if jtc.coltype != JsonTableColumnType::JTC_NESTED {
                continue;
            }
            let nested = jtc
                .pathspec
                .as_deref()
                .ok_or_else(|| Error::internal("a NESTED path with no path"))?;
            let nested_name = match nested.name.as_deref() {
                Some(name) => name.to_string(),
                None => parts.generate(),
            };
            let plan = self.json_table_columns(parts, &jtc.columns, nested, nested_name)?;
            child = Some(match child {
                Some(left) => JsonTablePlan::Sibling(Box::new(left), Box::new(plan)),
                None => plan,
            });
        }
        let path = self.json_table_path(spec, false)?;
        let error_on_error = parts.error_on_error;
        let scan = JsonTableScan { path, name, error_on_error, columns: start..end, child };
        Ok(JsonTablePlan::Scan(Box::new(scan)))
    }
}
