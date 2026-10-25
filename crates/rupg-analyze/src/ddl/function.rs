//! `CreateFunction`: `CREATE FUNCTION` for a function in SQL, as `interpret_AS_clause`, `ProcedureCreate` and `fmgr_sql_validator` run it. The body is a `RETURN` expression, a query in `BEGIN ATOMIC` or a query in a string.
//!
//! The catalog keeps the text of the statement and the schemas of `search_path`, and the analyzer reads the body again each time a query calls the function, as it reads the query of a view. The analysis of a call uses the types of the arguments of the call, so a polymorphic argument has its actual type. The setup of a new cluster runs `CREATE FUNCTION` for the functions of `information_schema` too.

use rupg_catalog::{
    Catalog, Function, FunctionBody, ObjRef, PG_PROC, PG_TYPE, eliminate_duplicates,
};
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin;
use rupg_sql::nodes::{
    CreateFunctionStmt, DefElem, FunctionParameterMode, Node, VariableSetKind, VariableSetStmt,
};
use rupg_types::oid;

use super::{Definer, not_yet};
use crate::agg::Kind;
use crate::coerce::{AtOpt, Context};
use crate::expr::Expr;
use crate::select::Query;
use crate::typename::{names, place};
use crate::{Analyzer, Env, Params, node, poly, types};

/// The OID of the language `sql`.
const SQL_LANGUAGE: u32 = 14;
/// The OID of the language `plpgsql`, which the extension `plpgsql` of a new cluster makes.
const PLPGSQL_LANGUAGE: u32 = 13708;
/// The OID of the language `internal`, the built-in functions.
const INTERNAL_LANGUAGE: u32 = 12;
/// The OID of the language `c`, the functions of a library.
const C_LANGUAGE: u32 = 13;
/// The default `procost` of a function in a language other than C and `internal`.
const DEFAULT_COST: f32 = 100.0;
/// The suffix of the file of a library, `DLSUFFIX` on Linux.
const DLSUFFIX: &str = ".so";
/// The name of the library of the regression tests of PostgreSQL, `regress.so`.
const REGRESS_LIBRARY: &str = "regress";

/// The functions of `regress.c` that have `PG_FUNCTION_INFO_V1`. A function in C can link to them in the library of the regression tests, and `rupg-func` runs some of them.
pub const REGRESS_SYMBOLS: &[&str] = &[
    "interpt_pp",
    "overpaid",
    "widget_in",
    "widget_out",
    "pt_in_widget",
    "reverse_name",
    "trigger_return_old",
    "int44in",
    "int44out",
    "test_canonicalize_path",
    "make_tuple_indirect",
    "get_environ",
    "regress_setenv",
    "wait_pid",
    "test_atomic_ops",
    "test_fdw_handler",
    "test_fdw_connection",
    "is_catalog_text_unique_index_oid",
    "test_support_func",
    "test_inline_in_from_support_func",
    "test_opclass_options_func",
    "test_enc_setup",
    "test_enc_conversion",
    "test_bytea_to_text",
    "test_text_to_bytea",
    "test_mblen_func",
    "test_text_to_wchars",
    "test_wchars_to_text",
    "test_valid_server_encoding",
    "binary_coercible",
    "test_relpath",
    "test_translation",
    "test_instr_time",
    "test_pglz_compress",
    "test_pglz_decompress",
    "test_pg_locale_apis",
];

/// `expand_dynamic_library_name` and the `stat` of `internal_load_library`: the full name of the library `name`. The only library that rupg has is the library of the regression tests, by a path that ends in `regress` or `regress.so`. A name without a directory is in `$libdir`, which does not have that library.
///
/// # Errors
///
/// `58P01` for a library that rupg does not have.
pub fn find_library(name: &str) -> Result<String> {
    let file = name.rsplit_once('/').map(|(_, file)| file);
    let stem = file.map(|file| file.strip_suffix(DLSUFFIX).unwrap_or(file));
    if stem != Some(REGRESS_LIBRARY) {
        return Err(Error::new(
            SqlState::UNDEFINED_FILE,
            format!("could not access file \"{name}\": No such file or directory"),
        ));
    }
    Ok(if name.ends_with(DLSUFFIX) { name.to_string() } else { format!("{name}{DLSUFFIX}") })
}
/// The default `prorows` of a function that returns a set.
const DEFAULT_ROWS: u32 = 1000;

/// The body of a function in SQL, as the analyzer reads it for the types of the arguments of a call.
#[derive(Debug)]
pub enum Body {
    /// A `RETURN` body: the value of the function, with `$1` as the first argument.
    Value(Expr),
    /// A body with a query, in `BEGIN ATOMIC` or in a string. A function that returns a set gives the rows of the query, and another function gives the first column of the first row, or null when the query gives no row.
    Query(Box<Query>),
    /// A body with more than one statement, or with a statement that is not `SELECT`. The session runs the statements one after the other with [`function_script`], and the last statement gives the rows of the result. It has the types of the columns of the result, or no types for a function that returns `record` with no `OUT` parameter.
    Statements(Vec<u32>),
}

/// The facts of the options of `CREATE FUNCTION`.
struct Options {
    /// The OID of the language.
    language: u32,
    /// The name of the language, `lanname`.
    language_name: String,
    strict: bool,
    leakproof: bool,
    volatile: u8,
    parallel: u8,
    cost: Option<f32>,
    rows: Option<u32>,
    support: Option<Vec<String>>,
    /// The body in a string, from `AS`. For a function in C it is the file of the library.
    src: Option<String>,
    /// The second item of `AS`, the link symbol of a function in C.
    link: Option<String>,
    /// The place of `AS` in the query text.
    src_at: Option<usize>,
    /// The settings of the clauses `SET`, as `proconfig` keeps them.
    config: Vec<String>,
    /// True for `SECURITY DEFINER`.
    secdef: bool,
}

/// `update_proconfig_value`: the settings of a function after a clause `SET` or `RESET`. `GUCArrayAdd` puts a new value in place of the value of the same parameter, and `GUCArrayDelete` and `RESET ALL` remove values.
pub(super) fn update_config(
    env: &dyn Env,
    config: &mut Vec<String>,
    set: &VariableSetStmt,
) -> Result<()> {
    if set.kind == VariableSetKind::VAR_RESET_ALL {
        config.clear();
        return Ok(());
    }
    let (name, value) = env.function_setting(set)?;
    let prefix = format!("{name}=");
    let at = config.iter().position(|item| item.starts_with(&prefix));
    match (value, at) {
        (Some(value), Some(at)) => config[at] = format!("{prefix}{value}"),
        (Some(value), None) => config.push(format!("{prefix}{value}")),
        (None, Some(at)) => {
            config.remove(at);
        }
        (None, None) => {}
    }
    Ok(())
}

/// True when the settings of a function set `search_path`, so that a body in a string reads its names with that path.
fn sets_path(config: &[String]) -> bool {
    config.iter().any(|item| item.starts_with("search_path="))
}

/// The error of `CreateFunction` and `AlterFunction` for `LEAKPROOF` from a role that is not a superuser.
pub(super) fn only_superuser_leakproof() -> Error {
    Error::new(SqlState::INSUFFICIENT_PRIVILEGE, "only superuser can define a leakproof function")
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

/// `compute_function_attributes`: the options of the function. Each option that rupg does not have gives an error. An option that only a function can have gives `42P13` for a procedure.
fn options(stmt: &CreateFunctionStmt, env: &dyn Env) -> Result<Options> {
    let mut out = Options {
        language: SQL_LANGUAGE,
        language_name: "sql".to_string(),
        strict: false,
        leakproof: false,
        volatile: b'v',
        parallel: b'u',
        cost: None,
        rows: None,
        support: None,
        src: None,
        link: None,
        src_at: None,
        config: Vec::new(),
        secdef: false,
    };
    for def in stmt.options.iter().flatten() {
        let Node::DefElem(def) = def else { continue };
        let name = def.defname.as_deref().unwrap_or_default();
        let at = place(def.location);
        let bad = || not_yet(&format!("the option {name} of a function"), at);
        let attribute = matches!(
            name,
            "volatility"
                | "strict"
                | "leakproof"
                | "cost"
                | "rows"
                | "support"
                | "parallel"
                | "window"
        );
        if stmt.is_procedure && attribute {
            return Err(Error::new(
                SqlState::INVALID_FUNCTION_DEFINITION,
                "invalid attribute in procedure definition",
            )
            .at_opt(at));
        }
        match (name, option_text(def)) {
            ("language", Some(language)) => {
                out.language = match language {
                    "sql" => SQL_LANGUAGE,
                    "plpgsql" => PLPGSQL_LANGUAGE,
                    "internal" => INTERNAL_LANGUAGE,
                    "c" => C_LANGUAGE,
                    _ => {
                        return Err(not_yet(&format!("a function in the language {language}"), at));
                    }
                };
                out.language_name = language.to_string();
            }
            ("strict", _) => out.strict = matches!(def.arg, Some(Node::Boolean(true))),
            ("leakproof", _) => out.leakproof = matches!(def.arg, Some(Node::Boolean(true))),
            ("volatility", Some("immutable")) => out.volatile = b'i',
            ("volatility", Some("stable")) => out.volatile = b's',
            ("volatility", Some("volatile")) => out.volatile = b'v',
            ("parallel", Some("safe")) => out.parallel = b's',
            ("parallel", Some("restricted")) => out.parallel = b'r',
            ("parallel", Some("unsafe")) => out.parallel = b'u',
            ("cost", _) => out.cost = Some(super::alter_function::positive_cost(def)?),
            ("rows", _) => match def.arg {
                Some(Node::Integer(rows)) if rows > 0 => out.rows = Some(rows.unsigned_abs()),
                _ => return Err(bad()),
            },
            ("support", _) => out.support = Some(option_list(def).ok_or_else(bad)?),
            ("set", _) => match &def.arg {
                Some(Node::VariableSetStmt(set)) => update_config(env, &mut out.config, set)?,
                _ => return Err(bad()),
            },
            ("security", _) => out.secdef = matches!(def.arg, Some(Node::Boolean(true))),
            ("as", _) => match option_list(def).as_deref() {
                // The grammar gives one item, or two for a function in C.
                Some([src, rest @ ..]) if rest.len() < 2 => {
                    out.src = Some(src.clone());
                    out.link = rest.first().cloned();
                    out.src_at = at;
                }
                _ => return Err(bad()),
            },
            _ => return Err(bad()),
        }
    }
    Ok(out)
}

/// `interpret_AS_clause`: `prosrc` and `probin` of the function `name` from `AS`. A function in C has the file of the library in `probin` and the link symbol in `prosrc`, which is the name of the function when `AS` has no symbol or the symbol `-`. A function in `internal` with an empty `AS` has its own name in `prosrc`. The other languages have the body in `prosrc`.
fn interpret_as(
    stmt: &CreateFunctionStmt,
    options: &Options,
    name: &str,
) -> Result<(String, String)> {
    let bad = |message: String| Err(Error::new(SqlState::INVALID_FUNCTION_DEFINITION, message));
    if matches!(options.language, INTERNAL_LANGUAGE | C_LANGUAGE) {
        match (&options.src, &stmt.sql_body) {
            (None, None) => return bad("no function body specified".into()),
            (Some(_), Some(_)) => return bad("duplicate function body specified".into()),
            (None, Some(_)) => {
                return bad("inline SQL function body only valid for language SQL".into());
            }
            (Some(_), None) => {}
        }
    }
    let src = options.src.clone().unwrap_or_default();
    if options.language == C_LANGUAGE {
        let link = options.link.as_deref().filter(|link| *link != "-").unwrap_or(name);
        return Ok((link.to_string(), src));
    }
    if options.link.is_some() {
        return bad(format!("only one AS item needed for language \"{}\"", options.language_name));
    }
    if options.language == INTERNAL_LANGUAGE && src.is_empty() {
        return Ok((name.to_string(), String::new()));
    }
    Ok((src, String::new()))
}

/// `fmgr_internal_validator` and `fmgr_c_validator`: a function in `internal` names a built-in function, and a function in C names a function of a library that rupg has.
fn check_untrusted(function: &Function) -> Result<()> {
    if function.lang == INTERNAL_LANGUAGE {
        let found =
            builtin::procs().iter().any(|p| p.lang == INTERNAL_LANGUAGE && p.src == function.src);
        if !found {
            return Err(Error::new(
                SqlState::UNDEFINED_FUNCTION,
                format!("there is no built-in function named \"{}\"", function.src),
            ));
        }
        return Ok(());
    }
    let file = find_library(&function.bin)?;
    if !REGRESS_SYMBOLS.contains(&function.src.as_str()) {
        return Err(Error::new(
            SqlState::UNDEFINED_FUNCTION,
            format!("could not find function \"{}\" in file \"{file}\"", function.src),
        ));
    }
    Ok(())
}

/// The statements of a body in SQL, from the statement that made the function: the `RETURN` expression, the statements of `BEGIN ATOMIC`, or the statements of the string of `AS`.
enum Statements<'s> {
    Return(Option<&'s Node>),
    Atomic(Vec<&'s Node>),
    /// The statements of a body in a string, each with its text.
    String(Vec<(Node, String)>),
}

/// The body of the statement.
fn statements<'s>(stmt: &'s CreateFunctionStmt, src: Option<&str>) -> Result<Statements<'s>> {
    match (&stmt.sql_body, src) {
        (Some(Node::ReturnStmt(ret)), None) => Ok(Statements::Return(ret.returnval.as_ref())),
        (Some(Node::List(outer)), None) => match outer.first() {
            Some(Some(Node::List(list))) => Ok(Statements::Atomic(list.iter().flatten().collect())),
            // `BEGIN ATOMIC END` gives an empty list, and the tree keeps an empty list as no node.
            Some(None) | None => Ok(Statements::Atomic(Vec::new())),
            Some(Some(_)) => Err(Error::internal("a BEGIN ATOMIC body that is not a list")),
        },
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
                    Node::RawStmt(raw) => {
                        let start = usize::try_from(raw.stmt_location).unwrap_or(0);
                        // A length of 0 is the rest of the text.
                        let end = match usize::try_from(raw.stmt_len) {
                            Ok(0) | Err(_) => src.len(),
                            Ok(len) => start + len,
                        };
                        let text = src.get(start..end).unwrap_or(src).to_string();
                        raw.stmt.map(|stmt| (stmt, text))
                    }
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

/// The error of `check_sql_fn_retval` for a body with the wrong columns.
fn mismatch(rettype: u32, detail: String) -> Error {
    Error::new(
        SqlState::INVALID_FUNCTION_DEFINITION,
        format!("return type mismatch in function declared to return {}", types::name(rettype)),
    )
    .with_detail(detail)
}

/// The error of `check_sql_fn_retval` for a body whose last statement gives no rows.
fn no_final_query(rettype: u32) -> Error {
    mismatch(
        rettype,
        "Function's final statement must be SELECT or INSERT/UPDATE/DELETE/MERGE RETURNING.".into(),
    )
}

/// True for a statement that has a plan, which `fmgr_sql` runs with the executor. The other statements run with `ProcessUtility`.
fn planned(node: &Node) -> bool {
    match node {
        Node::SelectStmt(select) => crate::select_into(select).is_none(),
        Node::InsertStmt(_)
        | Node::UpdateStmt(_)
        | Node::DeleteStmt(_)
        | Node::MergeStmt(_)
        | Node::CallStmt(_) => true,
        _ => false,
    }
}

/// `prepare_next_query`: the query of the statement `node` of the body of `function`, for a call with arguments of the types `inputs`, with the names of the search path `path`. The last statement gives the result of the function, as `check_sql_fn_retval` checks it. `None` for a statement that has no plan, which can be the last statement only of a function that returns `void`.
fn statement_query(
    env: &dyn Env,
    function: &Function,
    path: Option<&[u32]>,
    inputs: &[u32],
    node: &Node,
    last: bool,
) -> Result<Option<Query>> {
    let params = Params { types: inputs.to_vec(), variable: false };
    let mut an = Analyzer::new(env, &params);
    if let Some(path) = path {
        an.set_path(path);
    }
    an.sql_function = Some((function.name.clone(), input_names(function)));
    let query = if planned(node) { Some(an.statement(node)?) } else { None };
    if let Some(query) = &query
        && !query.out_args.is_empty()
    {
        return Err(Error::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "calling procedures with output arguments is not supported in SQL functions",
        ));
    }
    let Some(mut query) = query else {
        if last {
            let (rettype, _) = an.result_shape(function, inputs)?;
            if rettype != oid::VOID {
                return Err(no_final_query(rettype));
            }
        }
        return Ok(None);
    };
    if last {
        let (rettype, columns) = an.result_shape(function, inputs)?;
        // `CALL` gives no rows.
        if rettype != oid::VOID && matches!(node, Node::CallStmt(_)) {
            return Err(no_final_query(rettype));
        }
        an.result_columns(&mut query, rettype, &columns)?;
    }
    an.finish(query).map(Some)
}

/// `interpret_AS_clause` for a `BEGIN ATOMIC` body: each statement in turn takes its analysis, and a statement that has no plan is an error. A `CALL` resolves its procedure before the error.
fn check_atomic(
    env: &dyn Env,
    function: &Function,
    path: Option<&[u32]>,
    list: &[&Node],
) -> Result<()> {
    let params = Params { types: function.argtypes.clone(), variable: false };
    for node in list {
        if let Node::SelectStmt(select) = node
            && let Some(into) = crate::select_into(select)
        {
            return Err(crate::select::into_not_allowed(into));
        }
        if planned(node) {
            let mut an = Analyzer::new(env, &params);
            if let Some(path) = path {
                an.set_path(path);
            }
            an.sql_function = Some((function.name.clone(), input_names(function)));
            an.statement(node)?;
        }
        if !planned(node) || matches!(node, Node::CallStmt(_)) {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!(
                    "{} is not yet supported in unquoted SQL function body",
                    super::cmdtag::command_tag(node)
                ),
            ));
        }
    }
    Ok(())
}

/// `fmgr_sql_validator` for a body that is not one `SELECT`: the query of each statement that has a plan, with the checks of `check_sql_fn_retval` on the last statement. A body with no statement must be the body of a function that returns `void`.
fn check_statements(
    env: &dyn Env,
    function: &Function,
    path: Option<&[u32]>,
    list: &[&Node],
) -> Result<Vec<Query>> {
    let inputs = &function.argtypes;
    if list.is_empty() {
        let rettype = result_type(function, &result_types(function, inputs)?);
        if rettype != oid::VOID {
            return Err(no_final_query(rettype));
        }
    }
    let mut queries = Vec::new();
    for (i, node) in list.iter().enumerate() {
        let last = i + 1 == list.len();
        queries.extend(statement_query(env, function, path, inputs, node, last)?);
    }
    Ok(queries)
}

/// The result type of a call of `function` whose result has columns of the types `columns`: the one type of a function that does not return `record`, or else the declared type.
fn result_type(function: &Function, columns: &[u32]) -> u32 {
    match columns {
        [ty] if function.rettype != oid::RECORD => *ty,
        _ => function.rettype,
    }
}

/// The objects that an expression of a function refers to, as `recordDependencyOnExpr` records them: the objects that [`super::view::expr_references`] finds, also in the subqueries, in the order of `eliminate_duplicate_dependencies`. The catalog drops the references to pinned objects.
fn body_references(catalog: &Catalog, body: &Expr) -> Result<Vec<ObjRef>> {
    super::shared_constants(&[body])?;
    let mut refs = Vec::new();
    super::view::expr_references(catalog, body, &[], &mut refs);
    Ok(eliminate_duplicates(&refs))
}

impl Analyzer<'_> {
    /// `transformReturnStmt` and `check_sql_fn_retval`: the `RETURN` expression, as a value of the result type. A value of type `unknown` becomes `text` first, and then the cast to the result type is an assignment cast.
    fn return_body(&mut self, value: Option<&Node>, rettype: u32) -> Result<Expr> {
        let mut expr = self.with_kind(Kind::Select, |an| an.transform(value))?;
        // A function that returns `void` takes any value, and the call gives null.
        if rettype == oid::VOID {
            return Ok(expr);
        }
        if expr.ty == oid::UNKNOWN {
            expr = self.coerce(expr, oid::TEXT, -1, Context::Implicit, None)?;
        }
        let actual = expr.ty;
        self.coerce_to_target(expr, rettype, -1, Context::Assignment, None)?.ok_or_else(|| {
            mismatch(rettype, format!("Actual return type is {}.", types::name(actual)))
        })
    }

    /// The body of the statements `stmts`: a body of one `SELECT` is its query, as [`Analyzer::result_columns`] checks it, and the session runs the statements of another body one after the other.
    fn list_body(&mut self, stmts: &[&Node], rettype: u32, columns: &[u32]) -> Result<Body> {
        let [Node::SelectStmt(select)] = stmts else {
            return Ok(Body::Statements(columns.to_vec()));
        };
        let mut query = self.select(select)?;
        query.params.clone_from(&self.params);
        self.result_columns(&mut query, rettype, columns)?;
        Ok(Body::Query(Box::new(query)))
    }

    /// The query of the last statement of a body, as `check_sql_fn_retval` checks it: the columns of the query, or of the `RETURNING` list of a write, must be the columns of the result, and each column takes an assignment cast to the type of the result column. A function that returns a composite type can also give one column of that type, and a function that returns `void` can give any columns. A write with no `RETURNING` gives no rows, so it can be the last statement only of a function that returns `void`.
    fn result_columns(&mut self, query: &mut Query, rettype: u32, columns: &[u32]) -> Result<()> {
        if rettype == oid::VOID {
            return Ok(());
        }
        let query = if query.write.is_some() {
            let returning = query.write.as_mut().and_then(|w| w.returning.as_deref_mut());
            returning.ok_or_else(|| no_final_query(rettype))?
        } else {
            query
        };
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
                "Final statement must return exactly one column.".into(),
            ));
        }
        if row && !columns.is_empty() && width != columns.len() {
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
        Ok(())
    }

    /// The result type of a call of `function` with arguments of the types `inputs`, and the types of the columns of the result. A composite type gives the columns of its relation.
    fn result_shape(&self, function: &Function, inputs: &[u32]) -> Result<(u32, Vec<u32>)> {
        let mut columns = result_types(function, inputs)?;
        let rettype = result_type(function, &columns);
        if rettype != oid::RECORD
            && let Some(fields) = self.row_columns(rettype)
        {
            columns = fields.iter().map(|c| c.ty).collect();
        }
        Ok((rettype, columns))
    }

    /// The body of a function for a call with arguments of the types `inputs`.
    fn body(
        &mut self,
        function: &Function,
        stmts: &Statements<'_>,
        inputs: &[u32],
    ) -> Result<Body> {
        let (rettype, columns) = self.result_shape(function, inputs)?;
        match stmts {
            Statements::Return(value) => Ok(Body::Value(self.return_body(*value, rettype)?)),
            Statements::Atomic(list) => self.list_body(list, rettype, &columns),
            Statements::String(list) => {
                let list: Vec<&Node> = list.iter().map(|(node, _)| node).collect();
                self.list_body(&list, rettype, &columns)
            }
        }
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
    with_statement(env, oid, |function, body, stmt| {
        let src = (!function.src.is_empty()).then_some(function.src.as_str());
        let stmts = statements(stmt, src)?;
        let params = Params { types: inputs.to_vec(), variable: false };
        let mut an = Analyzer::new(env, &params);
        // A body in a string of a function that sets `search_path` takes the current path, which the settings of the call gave.
        if stmt.sql_body.is_some() || !sets_path(&function.config) {
            an.set_path(&body.path);
        }
        an.sql_function = Some((function.name.clone(), input_names(function)));
        let mut body = an.body(function, &stmts, inputs)?;
        if let Some(catalog) = env.catalog() {
            crate::expand_virtual_body(&mut body, catalog)?;
        }
        Ok(Some(body))
    })
}

/// The statements of the body of a function in SQL that [`Body::Statements`] runs, which `fmgr_sql` analyzes and runs one after the other, as `prepare_next_query` and `init_execution_state` do. A statement can see the changes of the statements before it.
#[derive(Debug)]
pub struct Script {
    function: Function,
    /// The search path of the names of the statements, or `None` for the current path.
    path: Option<Vec<u32>>,
    /// Each statement, with its text.
    pub statements: Vec<(Node, String)>,
}

impl Script {
    /// The name of the function.
    pub fn name(&self) -> &str {
        &self.function.name
    }

    /// The body in a string, or an empty text for a body in `BEGIN ATOMIC`.
    pub fn src(&self) -> &str {
        &self.function.src
    }

    /// True for a function that is not volatile, which can run only statements that do not write, as `readonly_func` of `SQLFunctionHashEntry` is.
    pub fn read_only(&self) -> bool {
        self.function.volatile != b'v'
    }

    /// True when the statement `i` has a plan. The other statements run as utility statements.
    pub fn planned(&self, i: usize) -> bool {
        self.statements.get(i).is_some_and(|(node, _)| planned(node))
    }

    /// `prepare_next_query`: the query of the statement `i` for a call with arguments of the types `inputs`. The last statement gives the result of the function, as `check_sql_fn_retval` checks it. `None` for a statement with no plan.
    ///
    /// # Errors
    ///
    /// The errors of the analysis of the statement, and `42P13` for a last statement that does not give the result of the function.
    pub fn analyze(&self, env: &dyn Env, inputs: &[u32], i: usize) -> Result<Option<Query>> {
        let Some((node, _)) = self.statements.get(i) else { return Ok(None) };
        let last = i + 1 == self.statements.len();
        statement_query(env, &self.function, self.path.as_deref(), inputs, node, last)
    }
}

/// The statements of the body of the function in SQL with the OID, for [`Body::Statements`]. `None` when the catalog has no function in SQL with the OID, or when the body is `RETURN`.
///
/// # Errors
///
/// The errors of the parse of the body, and `42P13` for a body with no statement in a function that does not return `void`.
pub fn function_script(env: &dyn Env, oid: u32) -> Result<Option<Script>> {
    with_statement(env, oid, |function, body, stmt| {
        let src = (!function.src.is_empty()).then_some(function.src.as_str());
        let statements = match statements(stmt, src)? {
            Statements::Return(_) => return Ok(None),
            Statements::Atomic(list) => {
                list.into_iter().map(|node| (node.clone(), String::new())).collect()
            }
            Statements::String(list) => list,
        };
        if statements.is_empty() && function.rettype != oid::VOID {
            return Err(no_final_query(function.rettype));
        }
        let path =
            (stmt.sql_body.is_some() || !sets_path(&function.config)).then(|| body.path.clone());
        Ok(Some(Script { function: function.clone(), path, statements }))
    })
}

/// The body in SQL of a function, as `prosqlbody` keeps it.
#[derive(Debug)]
pub enum SqlBody {
    /// A `RETURN` body: the value, with no cast to the result type.
    Return(Expr),
    /// A `BEGIN ATOMIC` body: the query of each statement.
    Atomic(Vec<Query>),
}

/// The body in SQL of the function with the OID, as `interpret_AS_clause` analyzes it for `prosqlbody`: the arguments have their declared types, and the value of `RETURN` and the columns of the queries do not take a cast to the result type. `None` when the catalog has no function in SQL with the OID, or when the function has its body in a string.
///
/// # Errors
///
/// The errors of the analysis of the body.
pub fn function_sqlbody(env: &dyn Env, oid: u32) -> Result<Option<SqlBody>> {
    with_statement(env, oid, |function, body, stmt| {
        if stmt.sql_body.is_none() {
            return Ok(None);
        }
        let params = Params { types: function.argtypes.clone(), variable: false };
        let analyzer = || {
            let mut an = Analyzer::new(env, &params);
            an.set_path(&body.path);
            an.sql_function = Some((function.name.clone(), input_names(function)));
            an
        };
        Ok(Some(match statements(stmt, None)? {
            Statements::Return(value) => {
                let mut an = analyzer();
                let expr = an.with_kind(Kind::Select, |an| an.transform(value))?;
                // `transformReturnStmt` gives the type `text` to a literal of an unknown type.
                if expr.ty == oid::UNKNOWN {
                    SqlBody::Return(an.coerce(expr, oid::TEXT, -1, Context::Implicit, None)?)
                } else {
                    SqlBody::Return(expr)
                }
            }
            Statements::Atomic(list) => SqlBody::Atomic(
                list.into_iter()
                    .map(|node| crate::analyze_in(analyzer(), node, None))
                    .collect::<Result<_>>()?,
            ),
            Statements::String(_) => return Ok(None),
        }))
    })
}

/// Runs `f` with the function in SQL with the OID, the text that the catalog keeps for it, and the `CREATE FUNCTION` statement of that text. `None` when the catalog has no function in SQL with the OID.
fn with_statement<T>(
    env: &dyn Env,
    oid: u32,
    f: impl FnOnce(&Function, &FunctionBody, &CreateFunctionStmt) -> Result<Option<T>>,
) -> Result<Option<T>> {
    let built_in;
    let function = match env.catalog().and_then(|c| c.function(oid)) {
        Some(function) => function,
        None => {
            let Some(function) = builtin_function(oid) else { return Ok(None) };
            built_in = function;
            &built_in
        }
    };
    let Some(body) = function.body.as_ref().filter(|_| function.lang == SQL_LANGUAGE) else {
        return Ok(None);
    };
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
    f(function, body, stmt)
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
        kind: b'f',
        strict: proc.strict,
        leakproof: false,
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
        config: Vec::new(),
        secdef: false,
    })
}

impl Definer<'_, '_> {
    /// `CreateFunction` for a function in SQL. `text` is the text of the statement, which the catalog keeps. The return value is the OID of the function.
    pub(super) fn create_function(&mut self, stmt: &CreateFunctionStmt, text: &str) -> Result<u32> {
        let parts = names(&stmt.funcname);
        let (schema, name) = self.an.split_name(&parts, None)?;
        let namespace = match schema {
            Some(namespace) => namespace,
            None => self.an.env.search_path().first().copied().ok_or_else(|| {
                Error::new(SqlState::UNDEFINED_SCHEMA, "no schema has been selected to create in")
            })?,
        };
        self.check_create_in(namespace, None)?;
        let options = options(stmt, self.an.env)?;
        // A function in C or in `internal` runs code that the database does not check, so these languages are not trusted.
        let untrusted = matches!(options.language, INTERNAL_LANGUAGE | C_LANGUAGE);
        if untrusted && !self.is_superuser() {
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("permission denied for language {}", options.language_name),
            ));
        }
        if options.leakproof && !self.is_superuser() {
            return Err(only_superuser_leakproof());
        }
        let procedure = stmt.is_procedure;
        let mut argtypes = Vec::new();
        let mut allargtypes = Vec::new();
        let mut argmodes = Vec::new();
        let mut argnames = Vec::new();
        let mut outs = Vec::new();
        let mut defaults = Vec::new();
        let bad = |message: &str, at| {
            Err(Error::new(SqlState::INVALID_FUNCTION_DEFINITION, message.to_string()).at_opt(at))
        };
        for param in stmt.parameters.iter().flatten() {
            let Node::FunctionParameter(param) = param else { continue };
            let at = place(param.location);
            let mode = match param.mode {
                FunctionParameterMode::FUNC_PARAM_OUT => b'o',
                FunctionParameterMode::FUNC_PARAM_INOUT => b'b',
                FunctionParameterMode::FUNC_PARAM_TABLE => b't',
                FunctionParameterMode::FUNC_PARAM_VARIADIC => b'v',
                _ => b'i',
            };
            let ty = param.argType.as_deref().ok_or_else(|| Error::internal("an argument type"))?;
            if ty.setof && procedure {
                return bad("procedures cannot accept set arguments", at);
            }
            if ty.setof {
                return bad("functions cannot accept set arguments", at);
            }
            let ty = self.an.type_name(ty)?.0;
            let input = matches!(mode, b'i' | b'b' | b'v');
            if input && argmodes.contains(&b'v') {
                return bad("VARIADIC parameter must be the last input parameter", at);
            }
            // A `CALL` gives the output arguments too, so in a procedure an output parameter cannot come after `VARIADIC`.
            if procedure && !input && argmodes.contains(&b'v') {
                return bad("VARIADIC parameter must be the last parameter", at);
            }
            if mode == b'v'
                && !matches!(ty, oid::ANYARRAY | oid::ANYCOMPATIBLEARRAY | oid::ANY)
                && types::element(ty) == 0
            {
                return bad("VARIADIC parameter must be an array", at);
            }
            // Two input parameters or two output parameters cannot have the same name. An input parameter and an output parameter can.
            let name = param.name.as_deref().unwrap_or_default();
            let clash = argnames.iter().zip(&argmodes).any(|(other, &other_mode)| {
                let pure = |m: u8| match m {
                    b'i' | b'v' => Some(true),
                    b'o' | b't' => Some(false),
                    _ => None,
                };
                let apart = matches!((pure(mode), pure(other_mode)), (Some(a), Some(b)) if a != b);
                !name.is_empty() && other == name && !apart
            });
            if clash {
                return bad(&format!("parameter name \"{name}\" used more than once"), at);
            }
            if let Some(raw) = &param.defexpr {
                if !input {
                    return bad("only input parameters can have default values", at);
                }
                let expr =
                    self.an.with_kind(Kind::FunctionDefault, |an| an.transform(Some(raw)))?;
                defaults.push(self.an.coerce_to_specific(expr, ty, "DEFAULT")?);
            } else if input && !defaults.is_empty() {
                return bad(
                    "input parameters after one with a default value must also have defaults",
                    at,
                );
            } else if procedure && !defaults.is_empty() {
                return bad(
                    "procedure OUT parameters cannot appear after one with a default value",
                    at,
                );
            }
            if input {
                argtypes.push(ty);
            }
            if matches!(mode, b'o' | b'b' | b't') {
                outs.push(ty);
            }
            allargtypes.push(ty);
            argmodes.push(mode);
            argnames.push(name.to_string());
        }
        if argnames.iter().all(String::is_empty) {
            argnames.clear();
        }
        if argmodes.iter().all(|&mode| mode == b'i') {
            allargtypes.clear();
            argmodes.clear();
        }
        // `interpret_function_parameter_list`: the result type that the `OUT` parameters need.
        // A procedure with output parameters gives a row, also with one of them.
        let required = match outs.as_slice() {
            [] => None,
            [ty] if !procedure => Some(*ty),
            _ => Some(oid::RECORD),
        };
        let (rettype, retset) = match (stmt.returnType.as_deref(), required) {
            (_, required) if procedure => (required.unwrap_or(oid::VOID), false),
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
        let (src, bin) = interpret_as(stmt, &options, name)?;
        let function = Function {
            oid: 0,
            name: name.to_string(),
            namespace,
            owner: self.user,
            lang: options.language,
            cost: options.cost.unwrap_or(if untrusted { 1.0 } else { DEFAULT_COST }),
            rows: if retset { options.rows.unwrap_or(DEFAULT_ROWS) } else { 0 },
            support,
            kind: if stmt.is_procedure { b'p' } else { b'f' },
            strict: options.strict,
            leakproof: options.leakproof,
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
            src,
            bin,
            body: (!untrusted)
                .then(|| FunctionBody { text: text.to_string(), path: self.an.path.clone() }),
            config: options.config.clone(),
            secdef: options.secdef,
        };
        // `function_parse_error_transpose`: an error in a body in a string has its place in the query text.
        let transpose = |error: Error| match (&options.src, error.position()) {
            (Some(src), Some(inner)) => {
                let outer = body_place(text, src, options.src_at).map(|at| at + inner);
                error.with_position(outer)
            }
            _ => error,
        };
        // `sql_function_parse_error_callback`: an error in the check of a body that has no place in the query text names the function.
        let checked = |error: Error| match error.position() {
            Some(_) => transpose(error),
            None => error.with_context(format!("SQL function \"{name}\"")),
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
        let pseudo = |ty: u32| {
            builtin::type_by_oid(ty).is_some_and(|t| t.kind == b'p') && !types::is_polymorphic(ty)
        };
        if untrusted {
            check_untrusted(&function)?;
            return self.store_function(old, function, &defaults, support, Vec::new());
        }
        if function.lang == PLPGSQL_LANGUAGE {
            // `plpgsql_validator` compiles the body only when `check_function_bodies` is on.
            if self.an.env.check_function_bodies() {
                self.with_settings(&function.config, |d| d.check_plpgsql(stmt, &function))
                    .map_err(transpose)?;
            }
            return self.store_function(old, function, &defaults, support, Vec::new());
        }
        // `fmgr_sql_validator`: the only pseudo-types that a function in SQL can take are the polymorphic types, and it can also return `void` and `record`.
        if pseudo(rettype) && !matches!(rettype, oid::VOID | oid::RECORD) {
            return Err(Error::new(
                SqlState::INVALID_FUNCTION_DEFINITION,
                format!("SQL functions cannot return type {}", types::name(rettype)),
            ));
        }
        if let Some(&ty) = function.argtypes.iter().find(|&&ty| pseudo(ty)) {
            return Err(Error::new(
                SqlState::INVALID_FUNCTION_DEFINITION,
                format!("SQL functions cannot have arguments of type {}", types::name(ty)),
            ));
        }
        let stmts = statements(stmt, options.src.as_deref()).map_err(transpose)?;
        if let Statements::Atomic(list) = &stmts {
            check_atomic(self.an.env, &function, Some(&self.an.path), list)?;
        }
        // `fmgr_sql_validator` reads a body in a string only when no argument is polymorphic and `check_function_bodies` is on, and a body in SQL always.
        let polymorphic = function.argtypes.iter().any(|&ty| types::is_polymorphic(ty));
        if let Statements::String(_) = &stmts {
            // `ProcedureCreate` adds the function before `fmgr_sql_validator` reads a body in a string, so the body can call the function.
            let oid = self.store_function(old, function.clone(), &defaults, support, Vec::new())?;
            if !polymorphic && self.an.env.check_function_bodies() {
                self.an.env.command_counter_increment(self.catalog);
                self.with_settings(&function.config, |d| d.sql_body(&function, &stmts))
                    .map_err(checked)?;
            }
            return Ok(oid);
        }
        let (body, queries) = self.sql_body(&function, &stmts).map_err(checked)?;
        let refs = match &body {
            Body::Value(expr) => body_references(self.catalog, expr)?,
            Body::Query(query) => {
                eliminate_duplicates(&super::view::function_references(self.catalog, query))
            }
            Body::Statements(_) => {
                let refs: Vec<ObjRef> = queries
                    .iter()
                    .flat_map(|query| super::view::function_references(self.catalog, query))
                    .collect();
                eliminate_duplicates(&refs)
            }
        };
        self.store_function(old, function, &defaults, support, refs)
    }

    /// `ProcedureCreate`: the validator of the language runs with the settings of the function, which `ProcessGUCArray` gives with `GUC_ACTION_SAVE`, and the settings go back after it.
    fn with_settings<T>(
        &mut self,
        config: &[String],
        f: impl FnOnce(&mut Self) -> Result<T>,
    ) -> Result<T> {
        if config.is_empty() {
            return f(self);
        }
        self.an.env.push_function_settings(config)?;
        let result = f(self);
        self.an.env.pop_function_settings();
        result
    }

    /// `fmgr_sql_validator`: the body of a new function in SQL, for a call with arguments of the types of its parameters, and the queries of the statements of a body that is not one `SELECT`. A body in a string reads its names with the `search_path` of the settings of the function, when they set it.
    fn sql_body(
        &mut self,
        function: &Function,
        stmts: &Statements<'_>,
    ) -> Result<(Body, Vec<Query>)> {
        let params = Params { types: function.argtypes.clone(), variable: false };
        let mut an = Analyzer::new(self.an.env, &params);
        // The new analyzer takes the current path, which the settings gave.
        let path = (!matches!(stmts, Statements::String(_)) || !sets_path(&function.config))
            .then(|| self.an.path.clone());
        if let Some(path) = &path {
            an.set_path(path);
        }
        an.sql_function = Some((function.name.clone(), input_names(function)));
        let body = an.body(function, stmts, &function.argtypes)?;
        self.an.notices.append(&mut an.notices);
        let mut queries = Vec::new();
        if let Body::Statements(_) = &body {
            let list: Vec<&Node> = match stmts {
                Statements::Return(_) => Vec::new(),
                Statements::Atomic(list) => list.clone(),
                Statements::String(list) => list.iter().map(|(node, _)| node).collect(),
            };
            queries = check_statements(self.an.env, function, path.as_deref(), &list)?;
            for query in &mut queries {
                self.an.notices.append(&mut query.notices);
            }
        }
        Ok((body, queries))
    }

    /// `ProcedureCreate`: adds the function to the catalog, or replaces `old`, with the dependencies `refs` of its body and the dependencies of its support function, its defaults and its types.
    fn store_function(
        &mut self,
        old: Option<Function>,
        function: Function,
        defaults: &[Expr],
        support: u32,
        refs: Vec<ObjRef>,
    ) -> Result<u32> {
        let refs = self.function_refs(&function, defaults, support, refs)?;
        match old {
            Some(old) => self.catalog.replace_function(old.oid, function, &refs).map(|()| old.oid),
            None => {
                let oid = self.catalog.create_function(function, &refs)?;
                self.apply_default_acl(ObjRef::new(PG_PROC, oid))?;
                Ok(oid)
            }
        }
    }

    /// The dependencies of `ProcedureCreate` for a function: `refs`, then its support function, its defaults and its types.
    pub(super) fn function_refs(
        &self,
        function: &Function,
        defaults: &[Expr],
        support: u32,
        mut refs: Vec<ObjRef>,
    ) -> Result<Vec<ObjRef>> {
        if support != 0 {
            refs.push(ObjRef::new(PG_PROC, support));
        }
        for default in defaults {
            for found in body_references(self.catalog, default)? {
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
        Ok(refs)
    }

    /// `plpgsql_validator`: a function in PL/pgSQL has a body in a string, and the only pseudo-types that it can take are the polymorphic types and `record`. It can also return `void`, `trigger` and `event_trigger`. Then the session compiles the body.
    fn check_plpgsql(&self, stmt: &CreateFunctionStmt, function: &Function) -> Result<()> {
        if stmt.sql_body.is_some() {
            return Err(Error::new(
                SqlState::INVALID_FUNCTION_DEFINITION,
                "inline SQL function body only valid for language SQL",
            ));
        }
        if function.src.is_empty()
            && !stmt
                .options
                .iter()
                .flatten()
                .any(|o| matches!(o, Node::DefElem(def) if def.defname.as_deref() == Some("as")))
        {
            return Err(Error::new(
                SqlState::INVALID_FUNCTION_DEFINITION,
                "no function body specified",
            ));
        }
        let pseudo = |ty: u32| builtin::type_by_oid(ty).is_some_and(|t| t.kind == b'p');
        let ret = function.rettype;
        if pseudo(ret)
            && !matches!(ret, oid::TRIGGER | oid::EVENT_TRIGGER | oid::RECORD | oid::VOID)
            && !types::is_polymorphic(ret)
        {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!("PL/pgSQL functions cannot return type {}", types::name(ret)),
            ));
        }
        let args = if function.allargtypes.is_empty() {
            &function.argtypes
        } else {
            &function.allargtypes
        };
        if let Some(&ty) =
            args.iter().find(|&&ty| pseudo(ty) && ty != oid::RECORD && !types::is_polymorphic(ty))
        {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!("PL/pgSQL functions cannot accept type {}", types::name(ty)),
            ));
        }
        self.an.env.check_function(function)
    }

    /// The checks of `ProcedureCreate` for `CREATE OR REPLACE FUNCTION` of the function `old`: the owner, the kind, the result type, the row type of the `OUT` parameters, the names of the input arguments and the defaults do not change. New defaults can be added before the old ones.
    pub(super) fn check_replace(&self, old: &Function, new: &Function) -> Result<()> {
        if !self.is_superuser() && old.owner != self.user {
            return Err(super::comment::not_owner("function", &old.name));
        }
        if old.kind != new.kind {
            let what = match old.kind {
                b'a' => "an aggregate function",
                b'p' => "a procedure",
                b'w' => "a window function",
                _ => "a function",
            };
            return Err(Error::new(SqlState::WRONG_OBJECT_TYPE, "cannot change routine kind")
                .with_detail(format!("\"{}\" is {what}.", old.name)));
        }
        let signature = self.describe(ObjRef::new(PG_PROC, old.oid));
        let signature = signature.split_once(' ').map_or(&*signature, |(_, rest)| rest);
        let drop = match new.kind {
            b'p' => "DROP PROCEDURE",
            b'a' => "DROP AGGREGATE",
            _ => "DROP FUNCTION",
        };
        let error = |message: String| {
            Error::new(SqlState::INVALID_FUNCTION_DEFINITION, message)
                .with_hint(format!("Use {drop} {signature} first."))
        };
        let changed = "cannot change return type of existing function".to_string();
        if (old.rettype != new.rettype || old.retset != new.retset) && new.kind == b'p' {
            return Err(error("cannot change whether a procedure has output parameters".into()));
        }
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
