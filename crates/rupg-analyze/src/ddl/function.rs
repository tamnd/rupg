//! `CreateFunction`: `CREATE FUNCTION` for a function in SQL with a `RETURN` body, as `interpret_AS_clause` and `ProcedureCreate` run it.
//!
//! The catalog keeps the text of the statement and the schemas of `search_path`, and the analyzer reads the body again each time a query calls the function, as it reads the query of a view. Only the setup of a new cluster runs `CREATE FUNCTION`, for the functions of `information_schema`.

use rupg_catalog::{Function, FunctionBody, ObjRef, PG_PROC};
use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{CreateFunctionStmt, DefElem, FunctionParameterMode, Node};
use rupg_types::oid;

use super::{Definer, not_yet, references};
use crate::agg::Kind;
use crate::coerce::Context;
use crate::expr::{Expr, ExprKind};
use crate::typename::{names, place};
use crate::{Analyzer, Env, Params, types};

/// The OID of the language `sql`.
const SQL_LANGUAGE: u32 = 14;
/// The default `procost` of a function in a language other than C and `internal`.
const DEFAULT_COST: u32 = 100;

/// The facts of the options of `CREATE FUNCTION`.
struct Options {
    strict: bool,
    volatile: u8,
    parallel: u8,
    cost: u32,
}

/// The text of the argument of an option.
fn option_text(def: &DefElem) -> Option<&str> {
    match &def.arg {
        Some(Node::String(s)) => Some(s),
        _ => None,
    }
}

/// `compute_function_attributes`: the options of the function. Each option that rupg does not have gives an error.
fn options(stmt: &CreateFunctionStmt) -> Result<Options> {
    let mut out = Options { strict: false, volatile: b'v', parallel: b'u', cost: DEFAULT_COST };
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
            _ => return Err(bad()),
        }
    }
    Ok(out)
}

/// The `RETURN` expression of the body of a function, or an error for a body of another form.
fn return_value(stmt: &CreateFunctionStmt) -> Result<Option<&Node>> {
    match &stmt.sql_body {
        Some(Node::ReturnStmt(ret)) => Ok(ret.returnval.as_ref()),
        Some(_) => Err(not_yet("a function body with BEGIN ATOMIC", None)),
        None => Err(not_yet("a function body in a string", None)),
    }
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
            Error::new(
                SqlState::INVALID_FUNCTION_DEFINITION,
                format!(
                    "return type mismatch in function declared to return {}",
                    types::name(rettype)
                ),
            )
            .with_detail(format!("Actual return type is {}.", types::name(actual)))
        })
    }
}

/// The body of a function in SQL as the analyzer reads it from the text that the catalog keeps, with `$1` as the first argument. `None` when the catalog has no function in SQL with the OID.
///
/// # Errors
///
/// The errors of the analysis of the body.
pub fn function_body(env: &dyn Env, oid: u32) -> Result<Option<Expr>> {
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
    let params = Params { types: function.argtypes.clone(), variable: false };
    let mut an = Analyzer::new(env, &params);
    an.path.clone_from(&body.path);
    an.return_body(return_value(stmt)?, function.rettype).map(Some)
}

impl Definer<'_, '_> {
    /// `CreateFunction` for a function in SQL with a `RETURN` body. `text` is the text of the statement, which the catalog keeps. The return value is the OID of the function.
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
        let mut argnames = Vec::new();
        for param in stmt.parameters.iter().flatten() {
            let Node::FunctionParameter(param) = param else { continue };
            let at = place(param.location);
            if !matches!(
                param.mode,
                FunctionParameterMode::FUNC_PARAM_IN | FunctionParameterMode::FUNC_PARAM_DEFAULT
            ) {
                return Err(not_yet("an argument that is not IN", at));
            }
            if param.defexpr.is_some() {
                return Err(not_yet("a default of an argument", at));
            }
            let ty = param.argType.as_deref().ok_or_else(|| Error::internal("an argument type"))?;
            argtypes.push(self.an.type_name(ty)?.0);
            argnames.push(param.name.as_deref().unwrap_or_default().to_string());
        }
        if argnames.iter().all(String::is_empty) {
            argnames.clear();
        }
        let ret = stmt
            .returnType
            .as_deref()
            .ok_or_else(|| not_yet("a function without RETURNS", None))?;
        if ret.setof {
            return Err(not_yet("a function that returns a set", place(ret.location)));
        }
        let rettype = self.an.type_name(ret)?.0;
        let options = options(stmt)?;
        let params = Params { types: argtypes.clone(), variable: false };
        let mut an = Analyzer::new(self.an.env, &params);
        an.path.clone_from(&self.an.path);
        let body = an.return_body(return_value(stmt)?, rettype)?;
        self.an.notices.append(&mut an.notices);
        let refs = body_references(&body);
        let function = Function {
            oid: 0,
            name: name.to_string(),
            namespace,
            owner: self.user,
            lang: SQL_LANGUAGE,
            cost: options.cost,
            rows: 0,
            strict: options.strict,
            volatile: options.volatile,
            parallel: options.parallel,
            rettype,
            argtypes,
            argnames,
            body: Some(FunctionBody { text: text.to_string(), path: self.an.path.clone() }),
        };
        self.catalog.create_function(function, &refs)
    }
}
