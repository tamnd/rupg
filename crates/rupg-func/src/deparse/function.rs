//! The functions of `ruleutils.c` that show a function: `pg_get_function_arguments`, `pg_get_function_identity_arguments`, `pg_get_function_result`, `pg_get_function_arg_default` and `pg_get_functiondef`.
//!
//! The arguments come from the rows of `pg_proc` that `rupg-pgcatalog` has, with the functions of the session. [`arg_defaults`] reads the defaults of the arguments from `proargdefaults`.

use rupg_analyze::{Expr, SqlBody, arg_defaults, function_sqlbody};
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin::{self, Owned, ProcRow};
use rupg_types::Value;

use super::Deparser;
use crate::calendar::format_g;
use crate::text::quote_identifier;
use crate::{Call, INTERNAL_LANGUAGE, Session, bad_value, reg};

/// The mode of an argument in `proargmodes`.
const IN: u8 = b'i';
const OUT: u8 = b'o';
const INOUT: u8 = b'b';
const VARIADIC: u8 = b'v';
const TABLE: u8 = b't';

/// `deparse_expression` of a default.
fn default_text(default: &Expr, session: &dyn Session) -> Result<String> {
    let mut d = Deparser::new(session, false, 0);
    d.expr(default, false)?;
    Ok(d.buf)
}

fn bad_node() -> Error {
    Error::new(SqlState::INTERNAL_ERROR, "unrecognized node in proargdefaults")
}

/// The built-in function of the first argument, or `None` for an OID that is not a function.
fn proc_arg(args: &[Value]) -> Result<Option<&'static ProcRow>> {
    let oid = args.first().and_then(Value::as_oid).ok_or_else(bad_value)?;
    Ok(builtin::proc_by_oid(oid))
}

/// `get_func_arg_info`: the type, the name and the mode of each argument, with the output arguments.
fn arg_info(proc: &ProcRow) -> Vec<(u32, &'static str, u8)> {
    let types = proc.allargtypes.unwrap_or(proc.argtypes);
    types
        .iter()
        .enumerate()
        .map(|(i, &ty)| {
            let name = proc.argnames.and_then(|names| names.get(i).copied()).unwrap_or("");
            let mode = proc.argmodes.and_then(|modes| modes.get(i).copied()).unwrap_or(IN);
            (ty, name, mode)
        })
        .collect()
}

/// `print_function_arguments`: the arguments of the function, only the `TABLE` arguments when `table` is true and all the others when it is false. The return value is the text and the number of the arguments in it.
fn print_arguments(
    proc: &ProcRow,
    table: bool,
    mut defaults: bool,
    session: &dyn Session,
) -> Result<(String, usize)> {
    let args = arg_info(proc);
    let mut nlackdefaults = args.len();
    let mut argdefaults = Vec::new();
    if defaults && proc.nargdefaults > 0 && proc.argdefaults.is_some() {
        argdefaults = arg_defaults(proc)?;
        nlackdefaults = usize::try_from(proc.nargs).unwrap_or(0).saturating_sub(argdefaults.len());
    }
    let mut next_default = argdefaults.iter();
    let insert_order_by = builtin::aggregate(proc.oid)
        .filter(|agg| proc.kind == b'a' && matches!(agg.kind, b'o' | b'h'))
        .and_then(|agg| usize::try_from(agg.ndirect).ok());
    let mut buf = String::new();
    let mut printed = 0;
    let mut input_no = 0;
    let mut i = 0;
    while i < args.len() {
        let (ty, name, mode) = args[i];
        let (modename, is_input) = match mode {
            IN if proc.kind == b'p' => ("IN ", true),
            IN => ("", true),
            INOUT => ("INOUT ", true),
            OUT => ("OUT ", false),
            VARIADIC => ("VARIADIC ", true),
            TABLE => ("", false),
            _ => {
                return Err(Error::new(
                    SqlState::INTERNAL_ERROR,
                    format!("invalid parameter mode '{}'", char::from(mode)),
                ));
            }
        };
        if is_input {
            input_no += 1;
        }
        if table != (mode == TABLE) {
            i += 1;
            continue;
        }
        if Some(printed) == insert_order_by {
            if printed > 0 {
                buf.push(' ');
            }
            buf.push_str("ORDER BY ");
        } else if printed > 0 {
            buf.push_str(", ");
        }
        buf.push_str(modename);
        if !name.is_empty() {
            buf.push_str(&quote_identifier(name));
            buf.push(' ');
        }
        buf.push_str(&reg::type_text(ty, None, session)?);
        if defaults && is_input && input_no > nlackdefaults {
            let c = next_default.next().ok_or_else(bad_node)?;
            buf.push_str(" DEFAULT ");
            buf.push_str(&default_text(c, session)?);
        }
        printed += 1;
        // `ruleutils.c` shows the last argument twice for a variadic ordered-set aggregate.
        if Some(printed) == insert_order_by && i == args.len() - 1 {
            defaults = false;
            continue;
        }
        i += 1;
    }
    Ok((buf, printed))
}

/// `pg_get_function_arguments(oid)`: the arguments as `CREATE FUNCTION` takes them, with the defaults.
pub(super) fn get_function_arguments(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Some(proc) = proc_arg(args)? else { return Ok(Value::Null) };
    Ok(Value::text(print_arguments(proc, false, true, call.session)?.0))
}

/// `pg_get_function_identity_arguments(oid)`: the arguments as `ALTER FUNCTION` takes them, without the defaults.
pub(super) fn get_function_identity_arguments(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Some(proc) = proc_arg(args)? else { return Ok(Value::Null) };
    Ok(Value::text(print_arguments(proc, false, false, call.session)?.0))
}

/// `pg_get_function_result(oid)`: the result type as `RETURNS` takes it, or null for a procedure.
pub(super) fn get_function_result(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Some(proc) = proc_arg(args)? else { return Ok(Value::Null) };
    if proc.kind == b'p' {
        return Ok(Value::Null);
    }
    Ok(Value::text(function_result(proc, call.session)?))
}

/// `print_function_rettype`: `TABLE` with the columns for a function with `TABLE` arguments, else the result type, after `SETOF` for a function that returns a set.
fn function_result(proc: &ProcRow, session: &dyn Session) -> Result<String> {
    if proc.retset {
        let (columns, count) = print_arguments(proc, true, false, session)?;
        if count > 0 {
            return Ok(format!("TABLE({columns})"));
        }
    }
    let ty = reg::type_text(proc.rettype, None, session)?;
    Ok(if proc.retset { format!("SETOF {ty}") } else { ty })
}

/// `pg_get_function_arg_default(oid, int4)`: the default of an input argument, by its number among all the arguments from 1, or null.
pub(super) fn get_function_arg_default(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Some(proc) = proc_arg(args)? else { return Ok(Value::Null) };
    let nth = args.get(1).and_then(Value::as_i64).ok_or_else(bad_value)?;
    let info = arg_info(proc);
    let is_input = |mode: u8| matches!(mode, IN | INOUT | VARIADIC);
    let Some(nth) = usize::try_from(nth).ok().filter(|&n| n >= 1 && n <= info.len()) else {
        return Ok(Value::Null);
    };
    if !is_input(info[nth - 1].2) {
        return Ok(Value::Null);
    }
    if proc.argdefaults.is_none() {
        return Ok(Value::Null);
    }
    let input_no = info[..nth].iter().filter(|(_, _, mode)| is_input(*mode)).count();
    let defaults = arg_defaults(proc)?;
    let lacking = i64::from(proc.nargs) - i64::from(proc.nargdefaults);
    let index = i64::try_from(input_no).unwrap_or(i64::MAX) - 1 - lacking;
    match usize::try_from(index).ok().and_then(|i| defaults.get(i)) {
        Some(c) => Ok(Value::text(default_text(c, call.session)?)),
        None => Ok(Value::Null),
    }
}

/// `pg_get_function_sqlbody(oid)`: the body in SQL of the function, as `print_function_sqlbody` shows it: `RETURN` and the value on one line, or `BEGIN ATOMIC`, each query on its lines with `;`, and `END`. Null for an OID that is not a function and for a function with no body in SQL.
pub(super) fn get_function_sqlbody(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Some(proc) = proc_arg(args)? else { return Ok(Value::Null) };
    Ok(print_function_sqlbody(proc, call.session)?.map_or(Value::Null, Value::text))
}

/// `print_function_sqlbody`: the body in SQL of the function, or `None` when the function has no body in SQL.
fn print_function_sqlbody(proc: &ProcRow, session: &dyn Session) -> Result<Option<String>> {
    let env = reg::SessionEnv(session);
    let Some(body) = function_sqlbody(&env, proc.oid)? else { return Ok(None) };
    let names = arg_info(proc).into_iter().map(|(_, name, _)| name.to_string()).collect();
    let mut d = Deparser::new(session, false, 0);
    d.function = Some((proc.name.to_string(), names));
    match body {
        // The context of a `RETURN` body has no pretty flags.
        SqlBody::Return(value) => {
            d.flat = true;
            d.buf.push_str("RETURN ");
            d.expr(&value, true)?;
        }
        SqlBody::Atomic(queries) => {
            d.buf.push_str("BEGIN ATOMIC\n");
            for query in &queries {
                if query.write.is_some() {
                    d.write_def(query, 1)?;
                } else {
                    d.query_def(query, None, false, 1)?;
                }
                d.buf.push_str(";\n");
            }
            d.buf.push_str("END");
        }
    }
    Ok(Some(d.buf))
}

/// `ClanguageId`.
const C_LANGUAGE: u32 = 13;
/// `SQLlanguageId`.
const SQL_LANGUAGE: u32 = 14;

/// The columns of `pg_proc` that `pg_get_functiondef` shows and that [`ProcRow`] does not have.
struct Attributes {
    cost: f64,
    rows: f64,
    support: u32,
    parallel: u8,
    leakproof: bool,
    secdef: bool,
    /// `probin`, or `None` for a null.
    bin: Option<String>,
    /// `prosrc`.
    src: String,
    /// The function has a body in SQL in `prosqlbody`.
    sql_body: bool,
    /// `proconfig`, each item as `name=value`.
    config: Vec<String>,
}

/// The attributes of a function of the catalog of the session, or of a built-in function from its static row. `proconfig` is null for each built-in function.
fn attributes(proc: &ProcRow, session: &dyn Session) -> Attributes {
    if let Some(f) = session.catalog().and_then(|c| c.function(proc.oid)) {
        return Attributes {
            cost: f64::from(f.cost),
            rows: f64::from(f.rows),
            support: f.support,
            parallel: f.parallel,
            leakproof: f.leakproof,
            secdef: f.secdef,
            bin: (!f.bin.is_empty()).then(|| f.bin.clone()),
            src: f.src.clone(),
            sql_body: f.lang == SQL_LANGUAGE && f.src.is_empty() && f.body.is_some(),
            config: f.config.clone(),
        };
    }
    let static_row = builtin::proc_row(proc.oid).and_then(|row| {
        let catalog = rupg_pgcatalog::catalog("pg_proc")?;
        Some((catalog, row))
    });
    let text = |column: &str| {
        let (catalog, row) = static_row?;
        catalog.rows.get(catalog.column(column)?)?.text(row)
    };
    let number = |column: &str, default: f64| {
        text(column).and_then(|t| t.parse::<f64>().ok()).unwrap_or(default)
    };
    Attributes {
        cost: number("procost", 1.0),
        rows: number("prorows", 0.0),
        support: text("prosupport").and_then(|t| t.parse().ok()).unwrap_or(0),
        parallel: text("proparallel").and_then(|t| t.bytes().next()).unwrap_or(b'u'),
        leakproof: text("proleakproof").as_deref() == Some("t"),
        secdef: text("prosecdef").as_deref() == Some("t"),
        bin: text("probin"),
        src: proc.src.to_string(),
        sql_body: proc.sqlbody.is_some(),
        config: Vec::new(),
    }
}

/// The name of the language with this OID.
fn language_name(oid: u32, session: &dyn Session) -> Result<String> {
    builtin::owned_by_oid(Owned::Language, oid)
        .map(|l| l.name.to_string())
        .or_else(|| session.catalog()?.languages().find(|l| l.oid == oid).map(|l| l.name.clone()))
        .ok_or_else(|| Error::internal(format!("cache lookup failed for language {oid}")))
}

/// `simple_quote_literal`: the value in single quotes, with each quote twice, and with each backslash twice when `standard_conforming_strings` is off.
fn simple_quote_literal(buf: &mut String, value: &str, session: &dyn Session) {
    let conforming = session.setting("standard_conforming_strings").is_none_or(|v| v == "on");
    buf.push('\'');
    for c in value.chars() {
        if c == '\'' || (c == '\\' && !conforming) {
            buf.push(c);
        }
        buf.push(c);
    }
    buf.push('\'');
}

/// `pg_get_functiondef(oid)`: the `CREATE OR REPLACE FUNCTION` or `CREATE OR REPLACE PROCEDURE` statement that makes the function again, or null for an OID that is not a function. An aggregate gives an error. A function in SQL with a body in SQL shows the body as `pg_get_function_sqlbody` does, in place of `AS` and the string.
pub(super) fn get_functiondef(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Some(proc) = proc_arg(args)? else { return Ok(Value::Null) };
    let session = call.session;
    if proc.kind == b'a' {
        return Err(Error::new(
            SqlState::WRONG_OBJECT_TYPE,
            format!("\"{}\" is an aggregate function", proc.name),
        ));
    }
    let attrs = attributes(proc, session);
    let is_function = proc.kind != b'p';
    let schema = if proc.namespace != 0 && proc.namespace == reg::temp_namespace(session) {
        Some("pg_temp")
    } else {
        reg::namespace_name(proc.namespace, session)
    };
    let mut buf = format!(
        "CREATE OR REPLACE {} {}(",
        if is_function { "FUNCTION" } else { "PROCEDURE" },
        reg::qualified(schema, proc.name)
    );
    buf.push_str(&print_arguments(proc, false, true, session)?.0);
    buf.push_str(")\n");
    if is_function {
        buf.push_str(" RETURNS ");
        buf.push_str(&function_result(proc, session)?);
        buf.push('\n');
    }
    buf.push_str(&format!(" LANGUAGE {}\n", quote_identifier(&language_name(proc.lang, session)?)));
    // Some other options, on one line.
    let start = buf.len();
    if proc.kind == b'w' {
        buf.push_str(" WINDOW");
    }
    match proc.volatile {
        b'i' => buf.push_str(" IMMUTABLE"),
        b's' => buf.push_str(" STABLE"),
        _ => {}
    }
    match attrs.parallel {
        b's' => buf.push_str(" PARALLEL SAFE"),
        b'r' => buf.push_str(" PARALLEL RESTRICTED"),
        _ => {}
    }
    if proc.strict {
        buf.push_str(" STRICT");
    }
    if attrs.secdef {
        buf.push_str(" SECURITY DEFINER");
    }
    if attrs.leakproof {
        buf.push_str(" LEAKPROOF");
    }
    // The default cost and rows of `functioncmds.c`.
    let default_cost =
        if matches!(proc.lang, INTERNAL_LANGUAGE | C_LANGUAGE) { 1.0 } else { 100.0 };
    #[allow(clippy::float_cmp)]
    if attrs.cost != default_cost {
        buf.push_str(&format!(" COST {}", format_g(attrs.cost)));
    }
    #[allow(clippy::float_cmp)]
    if attrs.rows > 0.0 && attrs.rows != 1000.0 {
        buf.push_str(&format!(" ROWS {}", format_g(attrs.rows)));
    }
    if attrs.support != 0 {
        buf.push_str(" SUPPORT ");
        buf.push_str(&support_name(attrs.support, session)?);
    }
    if buf.len() != start {
        buf.push('\n');
    }
    // Each setting of `proconfig` on its own line. A parameter with `GUC_LIST_QUOTE` shows each element of its list as a literal, and an empty list as `NULL`.
    for item in &attrs.config {
        let Some((name, value)) = item.split_once('=') else { continue };
        buf.push_str(&format!(" SET {} TO ", quote_identifier(name)));
        match session.setting_list(name, value) {
            Some(list) if list.is_empty() => buf.push_str("NULL"),
            Some(list) => {
                for (i, element) in list.iter().enumerate() {
                    if i > 0 {
                        buf.push_str(", ");
                    }
                    simple_quote_literal(&mut buf, element, session);
                }
            }
            None => simple_quote_literal(&mut buf, value, session),
        }
        buf.push('\n');
    }
    if attrs.sql_body && proc.lang == SQL_LANGUAGE {
        let body = print_function_sqlbody(proc, session)?.ok_or_else(|| {
            Error::internal(format!("cache lookup failed for function {}", proc.oid))
        })?;
        buf.push_str(&body);
        buf.push('\n');
        return Ok(Value::text(buf));
    }
    buf.push_str("AS ");
    if let Some(bin) = &attrs.bin {
        simple_quote_literal(&mut buf, bin, session);
        buf.push_str(", ");
    }
    // Always a dollar quote, with a tag that the body does not have.
    let mut tag = String::from(if is_function { "$function" } else { "$procedure" });
    while attrs.src.contains(&tag) {
        tag.push('x');
    }
    tag.push('$');
    buf.push_str(&tag);
    buf.push_str(&attrs.src);
    buf.push_str(&tag);
    buf.push('\n');
    Ok(Value::text(buf))
}

/// `generate_function_name` for a support function, which takes one argument of the type `internal`: the name has its schema when the name alone does not find the function.
fn support_name(oid: u32, session: &dyn Session) -> Result<String> {
    let proc = builtin::proc_by_oid(oid)
        .ok_or_else(|| Error::internal(format!("cache lookup failed for function {oid}")))?;
    if reg::function_visible(proc, session) {
        Ok(quote_identifier(proc.name))
    } else {
        Ok(reg::qualified(reg::namespace_name(proc.namespace, session), proc.name))
    }
}
