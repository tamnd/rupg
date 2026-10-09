//! `CreateFunction`: `CREATE FUNCTION` for a function in SQL, as `interpret_AS_clause`, `ProcedureCreate` and `fmgr_sql_validator` run it. The body is a `RETURN` expression, a query in `BEGIN ATOMIC` or a query in a string.
//!
//! The catalog keeps the text of the statement and the schemas of `search_path`, and the analyzer reads the body again each time a query calls the function, as it reads the query of a view. The analysis of a call uses the types of the arguments of the call, so a polymorphic argument has its actual type. Only the setup of a new cluster runs `CREATE FUNCTION`, for the functions of `information_schema`.

use rupg_catalog::{Function, FunctionBody, ObjRef, PG_PROC};
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin;
use rupg_sql::nodes::{CreateFunctionStmt, DefElem, FunctionParameterMode, Node};
use rupg_types::oid;

use super::{Definer, not_yet, references};
use crate::agg::Kind;
use crate::coerce::Context;
use crate::expr::{Expr, ExprKind};
use crate::select::Query;
use crate::typename::{names, place};
use crate::{Analyzer, Env, Params, poly, types};

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
                Some([src]) => out.src = Some(src.clone()),
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
            let (list, _) = rupg_sql::parse(src).map_err(Error::from)?;
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

/// The error of `check_sql_fn_retval` for a query with the wrong columns.
fn mismatch(rettype: u32, detail: String) -> Error {
    Error::new(
        SqlState::INVALID_FUNCTION_DEFINITION,
        format!("return type mismatch in function declared to return {}", types::name(rettype)),
    )
    .with_detail(detail)
}

/// The objects that the body refers to, as `recordDependencyOnExpr` records them: the functions that it calls, and the objects that [`references`] finds. The catalog drops the references to pinned objects.
fn body_references(body: &Expr) -> Vec<ObjRef> {
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
    for found in references(0, &[body]) {
        if !refs.contains(&found) {
            refs.push(found);
        }
    }
    refs
}

impl Analyzer<'_> {
    /// `transformReturnStmt` and `check_sql_fn_retval`: the `RETURN` expression, as a value of the result type. A value of type `unknown` becomes `text` first, and then the cast to the result type is an assignment cast.
    fn return_body(&mut self, value: Option<&Node>, rettype: u32) -> Result<Expr> {
        let mut expr = self.with_kind(Kind::Select, |an| an.transform(value))?;
        if expr.ty == oid::UNKNOWN {
            expr = self.coerce(expr, oid::TEXT, -1, Context::Implicit, None)?;
        }
        let actual = expr.ty;
        self.coerce_to_target(expr, rettype, -1, Context::Assignment, None)?.ok_or_else(|| {
            mismatch(rettype, format!("Actual return type is {}.", types::name(actual)))
        })
    }

    /// The query of the last statement of a body, as `check_sql_fn_retval` checks it: the columns of the query must be the columns of the result, and each column takes an assignment cast to the type of the result column. The other statements give an error, as rupg runs only one query.
    fn query_body(&mut self, stmts: &[&Node], rettype: u32, columns: &[u32]) -> Result<Query> {
        let [Node::SelectStmt(select)] = stmts else {
            return Err(not_yet("a function body that is not one SELECT", None));
        };
        let mut query = self.select(select)?;
        query.params.clone_from(&self.params);
        let width = query.targets.iter().take_while(|t| !t.junk).count();
        if rettype != oid::RECORD && width != 1 {
            return Err(mismatch(
                rettype,
                "Final statement must return exactly one column.".into(),
            ));
        }
        if rettype == oid::RECORD && !columns.is_empty() && width != columns.len() {
            let what = if width > columns.len() { "many" } else { "few" };
            return Err(mismatch(rettype, format!("Final statement returns too {what} columns.")));
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
        let columns = result_types(function, inputs)?;
        let rettype = match columns.as_slice() {
            [ty] if function.rettype != oid::RECORD => *ty,
            _ => function.rettype,
        };
        Ok(match stmts {
            Statements::Return(value) => Body::Value(self.return_body(*value, rettype)?),
            Statements::Atomic(list) => {
                Body::Query(Box::new(self.query_body(list, rettype, &columns)?))
            }
            Statements::String(list) => {
                let list: Vec<&Node> = list.iter().collect();
                Body::Query(Box::new(self.query_body(&list, rettype, &columns)?))
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
    let Some(function) = env.catalog().and_then(|c| c.function(oid)) else { return Ok(None) };
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
    an.body(function, &stmts, inputs).map(Some)
}

impl Definer<'_, '_> {
    /// `CreateFunction` for a function in SQL. `text` is the text of the statement, which the catalog keeps. The return value is the OID of the function.
    pub(super) fn create_function(&mut self, stmt: &CreateFunctionStmt, text: &str) -> Result<u32> {
        if stmt.is_procedure {
            return Err(not_yet("CREATE PROCEDURE", None));
        }
        if stmt.replace {
            return Err(not_yet("CREATE OR REPLACE FUNCTION", None));
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
            if param.defexpr.is_some() {
                return Err(not_yet("a default of an argument", at));
            }
            let ty = param.argType.as_deref().ok_or_else(|| Error::internal("an argument type"))?;
            let ty = self.an.type_name(ty)?.0;
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
            src: options.src.clone().unwrap_or_default(),
            body: Some(FunctionBody { text: text.to_string(), path: self.an.path.clone() }),
        };
        let stmts = statements(stmt, options.src.as_deref())?;
        // `fmgr_sql_validator` reads a body in a string only when no argument is polymorphic, and a body in SQL always.
        let polymorphic = function.argtypes.iter().any(|&ty| types::is_polymorphic(ty));
        let mut refs = Vec::new();
        if !(polymorphic && matches!(stmts, Statements::String(_))) {
            let params = Params { types: function.argtypes.clone(), variable: false };
            let mut an = Analyzer::new(self.an.env, &params);
            an.path.clone_from(&self.an.path);
            let body = an.body(&function, &stmts, &function.argtypes)?;
            self.an.notices.append(&mut an.notices);
            refs = match (&body, &stmts) {
                (_, Statements::String(_)) => Vec::new(),
                (Body::Value(expr), _) => body_references(expr),
                (Body::Query(query), _) => super::view::function_references(query),
            };
        }
        if support != 0 {
            refs.push(ObjRef::new(PG_PROC, support));
        }
        self.catalog.create_function(function, &refs)
    }
}
