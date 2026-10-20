//! `CreateFunction`: `CREATE FUNCTION` for a function in SQL, as `interpret_AS_clause`, `ProcedureCreate` and `fmgr_sql_validator` run it. The body is a `RETURN` expression, a query in `BEGIN ATOMIC` or a query in a string.
//!
//! The catalog keeps the text of the statement and the schemas of `search_path`, and the analyzer reads the body again each time a query calls the function, as it reads the query of a view. The analysis of a call uses the types of the arguments of the call, so a polymorphic argument has its actual type. The setup of a new cluster runs `CREATE FUNCTION` for the functions of `information_schema` too.

use rupg_catalog::{Catalog, Function, FunctionBody, ObjRef, PG_PROC, PG_TYPE};
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin;
use rupg_sql::nodes::{CreateFunctionStmt, DefElem, FunctionParameterMode, Node};
use rupg_types::oid;

use super::{Definer, not_yet, references};
use crate::agg::Kind;
use crate::coerce::{AtOpt, Context};
use crate::expr::{Expr, ExprKind};
use crate::select::Query;
use crate::typename::{names, place};
use crate::{Analyzer, Env, Params, node, poly, types};

/// The OID of the language `sql`.
const SQL_LANGUAGE: u32 = 14;
/// The default `procost` of a function in a language other than C and `internal`.
const DEFAULT_COST: u32 = 100;
/// The default `prorows` of a function that returns a set.
const DEFAULT_ROWS: u32 = 1000;

/// The body of a function in SQL, as the analyzer reads it for the types of the arguments of a call.
#[derive(Debug)]
pub enum Body {
    /// A `RETURN` body: the value of the function, with `$1` as the first argument.
    Value(Expr),
    /// A body with a query, in `BEGIN ATOMIC` or in a string. A function that returns a set gives the rows of the query, and another function gives the first column of the first row, or null when the query gives no row.
    Query(Box<Query>),
}

/// The facts of the options of `CREATE FUNCTION`.
struct Options {
    strict: bool,
    volatile: u8,
    parallel: u8,
    cost: u32,
    rows: Option<u32>,
    support: Option<Vec<String>>,
    /// The body in a string, from `AS`.
    src: Option<String>,
    /// The place of `AS` in the query text.
    src_at: Option<usize>,
}

/// The text of the argument of an option.
fn option_text(def: &DefElem) -> Option<&str> {
    match &def.arg {
        Some(Node::String(s)) => Some(s),
        _ => None,
    }
}

/// The texts of the argument of an option that takes a list of names or of strings.
fn option_list(def: &DefElem) -> Option<Vec<String>> {
    match &def.arg {
        Some(Node::List(list)) => list
            .iter()
            .map(|n| match n {
                Some(Node::String(s)) => Some(s.to_string()),
                _ => None,
            })
            .collect(),
        _ => None,
    }
}

/// `compute_function_attributes`: the options of the function. Each option that rupg does not have gives an error.
fn options(stmt: &CreateFunctionStmt) -> Result<Options> {
    let mut out = Options {
        strict: false,
        volatile: b'v',
        parallel: b'u',
        cost: DEFAULT_COST,
        rows: None,
        support: None,
        src: None,
        src_at: None,
    };
    for def in stmt.options.iter().flatten() {
        let Node::DefElem(def) = def else { continue };
        let name = def.defname.as_deref().unwrap_or_default();
        let at = place(def.location);
        let bad = || not_yet(&format!("the option {name} of a function"), at);
        match (name, option_text(def)) {
            ("language", Some("sql")) => {}
            ("language", Some(language)) => {
                return Err(not_yet(&format!("a function in the language {language}"), at));
            }
            ("strict", _) => out.strict = matches!(def.arg, Some(Node::Boolean(true))),
            ("volatility", Some("immutable")) => out.volatile = b'i',
            ("volatility", Some("stable")) => out.volatile = b's',
            ("volatility", Some("volatile")) => out.volatile = b'v',
            ("parallel", Some("safe")) => out.parallel = b's',
            ("parallel", Some("restricted")) => out.parallel = b'r',
            ("parallel", Some("unsafe")) => out.parallel = b'u',
            ("cost", _) => match def.arg {
                Some(Node::Integer(cost)) if cost > 0 => out.cost = cost.unsigned_abs(),
                _ => return Err(bad()),
            },
            ("rows", _) => match def.arg {
                Some(Node::Integer(rows)) if rows > 0 => out.rows = Some(rows.unsigned_abs()),
                _ => return Err(bad()),
            },
            ("support", _) => out.support = Some(option_list(def).ok_or_else(bad)?),
            ("as", _) => match option_list(def).as_deref() {
                Some([src]) => {
                    out.src = Some(src.clone());
                    out.src_at = at;
                }
                _ => return Err(bad()),
            },
            _ => return Err(bad()),
        }
    }
    Ok(out)
}

/// The statements of a body in SQL, from the statement that made the function: the `RETURN` expression, the statements of `BEGIN ATOMIC`, or the statements of the string of `AS`.
enum Statements<'s> {
    Return(Option<&'s Node>),
    Atomic(Vec<&'s Node>),
    String(Vec<Node>),
}

/// The body of the statement.
fn statements<'s>(stmt: &'s CreateFunctionStmt, src: Option<&str>) -> Result<Statements<'s>> {
    match (&stmt.sql_body, src) {
        (Some(Node::ReturnStmt(ret)), None) => Ok(Statements::Return(ret.returnval.as_ref())),
        (Some(Node::List(outer)), None) => {
            let Some(Some(Node::List(list))) = outer.first() else {
                return Err(Error::internal("a BEGIN ATOMIC body that is not a list"));
            };
            Ok(Statements::Atomic(list.iter().flatten().collect()))
        }
        (Some(_), Some(_)) => Err(Error::new(
            SqlState::INVALID_FUNCTION_DEFINITION,
            "duplicate function body specified",
        )),
        (Some(_), None) => Err(Error::internal("a function body of an unknown form")),
        (None, Some(src)) => {
            let (list, _) = rupg_sql::parse(src).map_err(|error| {
                let at = error.location;
                Error::from(error).with_position(at)
            })?;
            let stmts = list
                .into_iter()
                .flatten()
                .filter_map(|node| match node {
                    Node::RawStmt(raw) => raw.stmt,
                    _ => None,
                })
                .collect();
            Ok(Statements::String(stmts))
        }
        (None, None) => {
            Err(Error::new(SqlState::INVALID_FUNCTION_DEFINITION, "no function body specified"))
        }
    }
}

/// The types of the columns of the result of a function: the `OUT` parameters of a function that returns `record`, or else the result type. A polymorphic type takes the actual type that `inputs`, the types of the arguments of a call, give. A function that returns `record` with no `OUT` parameter has no known columns.
fn result_types(function: &Function, inputs: &[u32]) -> Result<Vec<u32>> {
    let declared: Vec<u32> = if function.rettype == oid::RECORD {
        function
            .allargtypes
            .iter()
            .zip(&function.argmodes)
            .filter(|(_, mode)| matches!(mode, b'o' | b'b' | b't'))
            .map(|(ty, _)| *ty)
            .collect()
    } else {
        vec![function.rettype]
    };
    declared
        .into_iter()
        .map(|ty| {
            if types::is_polymorphic(ty) {
                poly::resolve(inputs, &mut function.argtypes.clone(), ty)
            } else {
                Ok(ty)
            }
        })
        .collect()
}

/// `function_parse_error_transpose` and `match_prosrc_to_query`: the place in the query text of the body `src` of a function, in a string after `AS`, so that an error in the body has its place in the query. `text` is the text of the statement, and `as_at` is the place of `AS` in the query text. `None` when the string is not in the text.
fn body_place(text: &str, src: &str, as_at: Option<usize>) -> Option<usize> {
    let as_at = as_at?;
    let quoted = src.replace('\'', "''");
    let local = [src, quoted.as_str()].into_iter().find_map(|raw| {
        text.match_indices(raw)
            .map(|(i, _)| i)
            .find(|&i| matches!(text.as_bytes()[..i].last(), Some(b'$' | b'\'')))
    })?;
    // The string comes after `AS`, so `AS` is the last word before it.
    let keyword = text[..local].to_ascii_lowercase().rfind("as")?;
    Some(as_at.checked_sub(keyword)? + local)
}

/// The names of the input arguments of a function, in the order of their numbers. An argument with no name has an empty name.
fn input_names(function: &Function) -> Vec<String> {
    if function.argmodes.is_empty() {
        return function.argnames.clone();
    }
    function
        .argmodes
        .iter()
        .zip(&function.argnames)
        .filter(|(mode, _)| matches!(mode, b'i' | b'b' | b'v'))
        .map(|(_, name)| name.clone())
        .collect()
}

/// The error of `check_sql_fn_retval` for a body with the wrong columns, in the function `name`.
fn mismatch(rettype: u32, name: &str, detail: String) -> Error {
    Error::new(
        SqlState::INVALID_FUNCTION_DEFINITION,
        format!(
            "return type mismatch in function declared to return {} (SQL function \"{name}\")",
            types::name(rettype)
        ),
    )
    .with_detail(detail)
}

/// The objects that the body refers to, as `recordDependencyOnExpr` records them: the functions that it calls, and the objects that [`references`] finds. The catalog drops the references to pinned objects.
fn body_references(catalog: &Catalog, body: &Expr) -> Vec<ObjRef> {
    let mut refs = Vec::new();
    body.find(0, &mut |e, _| {
        if let ExprKind::Func(f) = &e.kind {
            let found = ObjRef::new(PG_PROC, f.oid);
            if !refs.contains(&found) {
                refs.push(found);
            }
        }
        None::<()>
    });
    for found in references(catalog, 0, &[body]) {
        if !refs.contains(&found) {
            refs.push(found);
        }
    }
    refs
}

impl Analyzer<'_> {
    /// `transformReturnStmt` and `check_sql_fn_retval`: the `RETURN` expression, as a value of the result type. A value of type `unknown` becomes `text` first, and then the cast to the result type is an assignment cast.
    fn return_body(&mut self, value: Option<&Node>, rettype: u32, name: &str) -> Result<Expr> {
        let mut expr = self.with_kind(Kind::Select, |an| an.transform(value))?;
        if expr.ty == oid::UNKNOWN {
            expr = self.coerce(expr, oid::TEXT, -1, Context::Implicit, None)?;
        }
        let actual = expr.ty;
        self.coerce_to_target(expr, rettype, -1, Context::Assignment, None)?.ok_or_else(|| {
            mismatch(rettype, name, format!("Actual return type is {}.", types::name(actual)))
        })
    }

    /// The query of the last statement of a body, as `check_sql_fn_retval` checks it: the columns of the query must be the columns of the result, and each column takes an assignment cast to the type of the result column. A function that returns a composite type can also give one column of that type. The other statements give an error, as rupg runs only one query.
    fn query_body(
        &mut self,
        stmts: &[&Node],
        rettype: u32,
        columns: &[u32],
        name: &str,
    ) -> Result<Query> {
        let [Node::SelectStmt(select)] = stmts else {
            return Err(not_yet("a function body that is not one SELECT", None));
        };
        let mut query = self.select(select)?;
        query.params.clone_from(&self.params);
        let width = query.targets.iter().take_while(|t| !t.junk).count();
        let row = rettype == oid::RECORD || columns.len() != 1 || columns != [rettype];
        let whole_row = [rettype];
        let columns: &[u32] = if row && width == 1 && query.targets[0].expr.ty == rettype {
            &whole_row
        } else {
            columns
        };
        if !row && width != 1 {
            return Err(mismatch(
                rettype,
                name,
                "Final statement must return exactly one column.".into(),
            ));
        }
        if row && !columns.is_empty() && width != columns.len() {
            let what = if width > columns.len() { "many" } else { "few" };
            return Err(mismatch(
                rettype,
                name,
                format!("Final statement returns too {what} columns."),
            ));
        }
        for (i, &ty) in columns.iter().enumerate() {
            let target = &mut query.targets[i];
            let actual = target.expr.ty;
            if actual == ty {
                continue;
            }
            let sorted = query.sort.iter().chain(&query.group).chain(&query.distinct);
            if sorted.into_iter().any(|s| s.target == i) {
                return Err(not_yet("a cast of a sorted column of the result of a function", None));
            }
            let cast =
                self.coerce_to_target(target.expr.clone(), ty, -1, Context::Assignment, None)?;
            target.expr = cast.ok_or_else(|| {
                mismatch(
                    rettype,
                    name,
                    format!(
                        "Final statement returns {} instead of {} at column {}.",
                        types::name(actual),
                        types::name(ty),
                        i + 1
                    ),
                )
            })?;
        }
        Ok(query)
    }

    /// The body of a function for a call with arguments of the types `inputs`.
    fn body(
        &mut self,
        function: &Function,
        stmts: &Statements<'_>,
        inputs: &[u32],
    ) -> Result<Body> {
        let mut columns = result_types(function, inputs)?;
        let rettype = match columns.as_slice() {
            [ty] if function.rettype != oid::RECORD => *ty,
            _ => function.rettype,
        };
        // A composite type gives the columns of its relation.
        if rettype != oid::RECORD
            && let Some(fields) = self.row_columns(rettype)
        {
            columns = fields.iter().map(|c| c.ty).collect();
        }
        Ok(match stmts {
            Statements::Return(value) => {
                Body::Value(self.return_body(*value, rettype, &function.name)?)
            }
            Statements::Atomic(list) => {
                Body::Query(Box::new(self.query_body(list, rettype, &columns, &function.name)?))
            }
            Statements::String(list) => {
                let list: Vec<&Node> = list.iter().collect();
                Body::Query(Box::new(self.query_body(&list, rettype, &columns, &function.name)?))
            }
        })
    }

    /// `interpret_func_support`: the support function with the name, which takes and returns `internal`.
    fn support_function(&self, names: &[String]) -> Result<u32> {
        let parts: Vec<&str> = names.iter().map(String::as_str).collect();
        let (schema, name) = self.split_name(&parts, None)?;
        let path = schema.map_or_else(|| self.path.clone(), |schema| vec![schema]);
        let found = path.iter().find_map(|&namespace| {
            builtin::procs_named(name)
                .find(|p| p.namespace == namespace && p.argtypes == [oid::INTERNAL])
        });
        let proc = found.ok_or_else(|| {
            Error::new(
                SqlState::UNDEFINED_FUNCTION,
                format!("function {}(internal) does not exist", parts.join(".")),
            )
        })?;
        if proc.rettype != oid::INTERNAL {
            return Err(Error::new(
                SqlState::INVALID_OBJECT_DEFINITION,
                format!("support function {} must return type internal", parts.join(".")),
            ));
        }
        Ok(proc.oid)
    }
}

/// The body of a function in SQL as the analyzer reads it from the text that the catalog keeps, for a call with arguments of the types `inputs`, with `$1` as the first argument. `None` when the catalog has no function in SQL with the OID.
///
/// # Errors
///
/// The errors of the analysis of the body.
pub fn function_body(env: &dyn Env, oid: u32, inputs: &[u32]) -> Result<Option<Body>> {
    let built_in;
    let function = match env.catalog().and_then(|c| c.function(oid)) {
        Some(function) => function,
        None => {
            let Some(function) = builtin_function(oid) else { return Ok(None) };
            built_in = function;
            &built_in
        }
    };
    let Some(body) = &function.body else { return Ok(None) };
    let (list, _) = rupg_sql::parse(&body.text).map_err(Error::from)?;
    let stmt = list.iter().flatten().find_map(|node| match node {
        Node::RawStmt(raw) => match raw.stmt.as_ref() {
            Some(Node::CreateFunctionStmt(stmt)) => Some(stmt),
            _ => None,
        },
        _ => None,
    });
    let stmt =
        stmt.ok_or_else(|| Error::internal("the body of a function is not CREATE FUNCTION"))?;
    let src = (!function.src.is_empty()).then_some(function.src.as_str());
    let stmts = statements(stmt, src)?;
    let params = Params { types: inputs.to_vec(), variable: false };
    let mut an = Analyzer::new(env, &params);
    an.path.clone_from(&body.path);
    an.sql_function = Some((function.name.clone(), input_names(function)));
    an.body(function, &stmts, inputs).map(Some)
}

/// A built-in function with a body in SQL, which `system_functions.sql` gives it. `initdb` runs that script with `search_path` set to `pg_catalog`.
fn builtin_function(oid: u32) -> Option<Function> {
    let proc = builtin::proc_by_oid(oid)?;
    let text = proc.sqlbody?;
    Some(Function {
        oid,
        name: proc.name.into(),
        namespace: proc.namespace,
        // The bootstrap superuser.
        owner: 10,
        lang: proc.lang,
        cost: DEFAULT_COST,
        rows: 0,
        support: 0,
        strict: proc.strict,
        volatile: proc.volatile,
        parallel: b's',
        rettype: proc.rettype,
        retset: proc.retset,
        argtypes: proc.argtypes.to_vec(),
        allargtypes: proc.allargtypes.unwrap_or_default().to_vec(),
        argmodes: proc.argmodes.unwrap_or_default().to_vec(),
        argnames: proc.argnames.unwrap_or_default().iter().map(|n| (*n).into()).collect(),
        // The body does not read the defaults.
        argdefaults: None,
        nargdefaults: 0,
        src: String::new(),
        bin: String::new(),
        body: Some(FunctionBody { text: text.into(), path: vec![crate::PG_CATALOG_NAMESPACE] }),
    })
}

impl Definer<'_, '_> {
    /// `CreateFunction` for a function in SQL. `text` is the text of the statement, which the catalog keeps. The return value is the OID of the function.
    pub(super) fn create_function(&mut self, stmt: &CreateFunctionStmt, text: &str) -> Result<u32> {
        if stmt.is_procedure {
            return Err(not_yet("CREATE PROCEDURE", None));
        }
        let parts = names(&stmt.funcname);
        let (schema, name) = self.an.split_name(&parts, None)?;
        let namespace = match schema {
            Some(namespace) => namespace,
            None => self.an.env.search_path().first().copied().ok_or_else(|| {
                Error::new(SqlState::UNDEFINED_SCHEMA, "no schema has been selected to create in")
            })?,
        };
        self.check_create_in(namespace, None)?;
        let mut argtypes = Vec::new();
        let mut allargtypes = Vec::new();
        let mut argmodes = Vec::new();
        let mut argnames = Vec::new();
        let mut outs = Vec::new();
        let mut defaults = Vec::new();
        for param in stmt.parameters.iter().flatten() {
            let Node::FunctionParameter(param) = param else { continue };
            let at = place(param.location);
            let mode = match param.mode {
                FunctionParameterMode::FUNC_PARAM_IN
                | FunctionParameterMode::FUNC_PARAM_DEFAULT => b'i',
                FunctionParameterMode::FUNC_PARAM_OUT => b'o',
                FunctionParameterMode::FUNC_PARAM_INOUT => b'b',
                FunctionParameterMode::FUNC_PARAM_TABLE => b't',
                _ => return Err(not_yet("a VARIADIC argument", at)),
            };
            let ty = param.argType.as_deref().ok_or_else(|| Error::internal("an argument type"))?;
            let ty = self.an.type_name(ty)?.0;
            let input = matches!(mode, b'i' | b'b');
            if let Some(raw) = &param.defexpr {
                if !input {
                    return Err(Error::new(
                        SqlState::INVALID_FUNCTION_DEFINITION,
                        "only input parameters can have default values",
                    )
                    .at_opt(at));
                }
                let expr =
                    self.an.with_kind(Kind::FunctionDefault, |an| an.transform(Some(raw)))?;
                defaults.push(self.an.coerce_to_specific(expr, ty, "DEFAULT")?);
            } else if input && !defaults.is_empty() {
                return Err(Error::new(
                    SqlState::INVALID_FUNCTION_DEFINITION,
                    "input parameters after one with a default value must also have defaults",
                )
                .at_opt(at));
            }
            if matches!(mode, b'i' | b'b') {
                argtypes.push(ty);
            }
            if mode != b'i' {
                outs.push(ty);
            }
            allargtypes.push(ty);
            argmodes.push(mode);
            argnames.push(param.name.as_deref().unwrap_or_default().to_string());
        }
        if argnames.iter().all(String::is_empty) {
            argnames.clear();
        }
        if argmodes.iter().all(|&mode| mode == b'i') {
            allargtypes.clear();
            argmodes.clear();
        }
        // `interpret_function_parameter_list`: the result type that the `OUT` parameters need.
        let required = match outs.as_slice() {
            [] => None,
            [ty] => Some(*ty),
            _ => Some(oid::RECORD),
        };
        let (rettype, retset) = match (stmt.returnType.as_deref(), required) {
            (Some(ret), _) => {
                let rettype = self.an.type_name(ret)?.0;
                if let Some(required) = required.filter(|&r| r != rettype) {
                    return Err(Error::new(
                        SqlState::INVALID_FUNCTION_DEFINITION,
                        format!(
                            "function result type must be {} because of OUT parameters",
                            types::name(required)
                        ),
                    ));
                }
                (rettype, ret.setof)
            }
            (None, Some(required)) => (required, false),
            (None, None) => {
                return Err(Error::new(
                    SqlState::INVALID_FUNCTION_DEFINITION,
                    "function result type must be specified",
                ));
            }
        };
        let options = options(stmt)?;
        if options.rows.is_some() && !retset {
            return Err(Error::new(
                SqlState::INVALID_PARAMETER_VALUE,
                "ROWS is not applicable when function does not return a set",
            ));
        }
        let support = match &options.support {
            Some(names) => self.an.support_function(names)?,
            None => 0,
        };
        let function = Function {
            oid: 0,
            name: name.to_string(),
            namespace,
            owner: self.user,
            lang: SQL_LANGUAGE,
            cost: options.cost,
            rows: if retset { options.rows.unwrap_or(DEFAULT_ROWS) } else { 0 },
            support,
            strict: options.strict,
            volatile: options.volatile,
            parallel: options.parallel,
            rettype,
            retset,
            argtypes,
            allargtypes,
            argmodes,
            argnames,
            argdefaults: if defaults.is_empty() {
                None
            } else {
                Some(node::write_list(&defaults)?)
            },
            nargdefaults: i16::try_from(defaults.len()).unwrap_or(i16::MAX),
            src: options.src.clone().unwrap_or_default(),
            bin: String::new(),
            body: Some(FunctionBody { text: text.to_string(), path: self.an.path.clone() }),
        };
        // `function_parse_error_transpose`: an error in a body in a string has its place in the query text.
        let transpose = |error: Error| match (&options.src, error.position()) {
            (Some(src), Some(inner)) => {
                let outer = body_place(text, src, options.src_at).map(|at| at + inner);
                error.with_position(outer)
            }
            _ => error,
        };
        let old = self
            .catalog
            .functions()
            .find(|f| f.namespace == namespace && f.name == name && f.argtypes == function.argtypes)
            .cloned();
        if let Some(old) = &old {
            if !stmt.replace {
                return Err(Error::new(
                    SqlState::DUPLICATE_FUNCTION,
                    format!("function \"{name}\" already exists with same argument types"),
                ));
            }
            self.check_replace(old, &function)?;
        }
        let stmts = statements(stmt, options.src.as_deref()).map_err(transpose)?;
        // `fmgr_sql_validator` reads a body in a string only when no argument is polymorphic, and a body in SQL always.
        let polymorphic = function.argtypes.iter().any(|&ty| types::is_polymorphic(ty));
        let mut refs = Vec::new();
        if !(polymorphic && matches!(stmts, Statements::String(_))) {
            let params = Params { types: function.argtypes.clone(), variable: false };
            let mut an = Analyzer::new(self.an.env, &params);
            an.path.clone_from(&self.an.path);
            an.sql_function = Some((function.name.clone(), input_names(&function)));
            let body = an.body(&function, &stmts, &function.argtypes).map_err(transpose)?;
            self.an.notices.append(&mut an.notices);
            refs = match (&body, &stmts) {
                (_, Statements::String(_)) => Vec::new(),
                (Body::Value(expr), _) => body_references(self.catalog, expr),
                (Body::Query(query), _) => super::view::function_references(self.catalog, query),
            };
        }
        if support != 0 {
            refs.push(ObjRef::new(PG_PROC, support));
        }
        for default in &defaults {
            for found in body_references(self.catalog, default) {
                if !refs.contains(&found) {
                    refs.push(found);
                }
            }
        }
        // `ProcedureCreate`: the function depends on the types of its result and of its arguments.
        let args = if function.allargtypes.is_empty() {
            &function.argtypes
        } else {
            &function.allargtypes
        };
        for &ty in std::iter::once(&function.rettype).chain(args) {
            let found = ObjRef::new(PG_TYPE, ty);
            if !refs.contains(&found) {
                refs.push(found);
            }
        }
        match old {
            Some(old) => self.catalog.replace_function(old.oid, function, &refs).map(|()| old.oid),
            None => self.catalog.create_function(function, &refs),
        }
    }

    /// The checks of `ProcedureCreate` for `CREATE OR REPLACE FUNCTION` of the function `old`: the owner, the result type, the row type of the `OUT` parameters, the names of the input arguments and the defaults do not change. New defaults can be added before the old ones.
    fn check_replace(&self, old: &Function, new: &Function) -> Result<()> {
        if !self.is_superuser() && old.owner != self.user {
            return Err(super::comment::not_owner("function", &old.name));
        }
        let signature = self.describe(ObjRef::new(PG_PROC, old.oid));
        let signature = signature.strip_prefix("function ").unwrap_or(&signature);
        let error = |message: String| {
            Error::new(SqlState::INVALID_FUNCTION_DEFINITION, message)
                .with_hint(format!("Use DROP FUNCTION {signature} first."))
        };
        let changed = "cannot change return type of existing function".to_string();
        if old.rettype != new.rettype || old.retset != new.retset {
            return Err(error(changed));
        }
        let outs = |f: &Function| -> Vec<(u32, String)> {
            f.allargtypes
                .iter()
                .zip(&f.argmodes)
                .zip(f.argnames.iter().map(String::as_str).chain(std::iter::repeat("")))
                .filter(|((_, mode), _)| matches!(mode, b'o' | b'b' | b't'))
                .map(|((&ty, _), name)| (ty, name.to_string()))
                .collect()
        };
        if old.rettype == oid::RECORD && outs(old) != outs(new) {
            return Err(
                error(changed).with_detail("Row type defined by OUT parameters is different.")
            );
        }
        let names = input_names(new);
        for (i, name) in input_names(old).iter().enumerate() {
            if !name.is_empty() && names.get(i) != Some(name) {
                return Err(error(format!("cannot change name of input parameter \"{name}\"")));
            }
        }
        if old.nargdefaults > 0 {
            if new.nargdefaults < old.nargdefaults {
                return Err(error(
                    "cannot remove parameter defaults from existing function".into(),
                ));
            }
            let defaults =
                |f: &Function| f.argdefaults.as_deref().map_or(Ok(Vec::new()), node::read_list);
            let (olds, news) = (defaults(old)?, defaults(new)?);
            let skip = news.len().saturating_sub(olds.len());
            if olds.iter().zip(&news[skip..]).any(|(o, n)| o.ty != n.ty) {
                return Err(error(
                    "cannot change data type of existing parameter default value".into(),
                ));
            }
        }
        Ok(())
    }
}
