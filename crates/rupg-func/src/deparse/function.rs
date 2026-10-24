//! The functions of `ruleutils.c` that show the arguments and the result of a function: `pg_get_function_arguments`, `pg_get_function_identity_arguments`, `pg_get_function_result` and `pg_get_function_arg_default`.
//!
//! The arguments come from the rows of `pg_proc` that `rupg-pgcatalog` has, with the functions of the session. [`arg_defaults`] reads the defaults of the arguments from `proargdefaults`.

use rupg_analyze::{Expr, SqlBody, arg_defaults, function_sqlbody};
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin::{self, ProcRow};
use rupg_types::Value;

use super::Deparser;
use crate::reg;
use crate::text::quote_identifier;
use crate::{Call, Session, bad_value};

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
    if proc.retset {
        let (columns, count) = print_arguments(proc, true, false, call.session)?;
        if count > 0 {
            return Ok(Value::text(format!("TABLE({columns})")));
        }
    }
    let ty = reg::type_text(proc.rettype, None, call.session)?;
    Ok(Value::text(if proc.retset { format!("SETOF {ty}") } else { ty }))
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
    let env = reg::SessionEnv(call.session);
    let Some(body) = function_sqlbody(&env, proc.oid)? else { return Ok(Value::Null) };
    let names = arg_info(proc).into_iter().map(|(_, name, _)| name.to_string()).collect();
    let mut d = Deparser::new(call.session, false, 0);
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
                d.query_def(query, None, false, 1)?;
                d.buf.push_str(";\n");
            }
            d.buf.push_str("END");
        }
    }
    Ok(Value::text(d.buf))
}
