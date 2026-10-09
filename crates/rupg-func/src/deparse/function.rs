//! The functions of `ruleutils.c` that show the arguments and the result of a function: `pg_get_function_arguments`, `pg_get_function_identity_arguments`, `pg_get_function_result` and `pg_get_function_arg_default`.
//!
//! All the functions are built in, so the arguments come from the static rows of `pg_proc`. Each default of an argument in `proargdefaults` is a `Const` node, which this module reads from the text of the node tree.

use rupg_analyze::Expr;
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin::{self, ProcRow};
use rupg_types::{Value, oid};

use super::Deparser;
use crate::io::input;
use crate::reg;
use crate::text::quote_identifier;
use crate::{Call, Session, bad_value};

/// The mode of an argument in `proargmodes`.
const IN: u8 = b'i';
const OUT: u8 = b'o';
const INOUT: u8 = b'b';
const VARIADIC: u8 = b'v';
const TABLE: u8 = b't';

/// `JB_FOBJECT` and `JB_FARRAY`: the flags of the root container of a `jsonb` value. The low bits hold the number of the items.
const JSONB_OBJECT: u32 = 0x2000_0000;
const JSONB_ARRAY: u32 = 0x4000_0000;
const JSONB_COUNT: u32 = 0x0FFF_FFFF;

/// One `Const` node of `proargdefaults`: its type, and its value as the bytes of the datum, or `None` for a null.
#[derive(Debug)]
struct Const {
    ty: u32,
    typmod: i32,
    bytes: Option<Vec<u8>>,
}

fn bad_node() -> Error {
    Error::new(SqlState::INTERNAL_ERROR, "unrecognized node in proargdefaults")
}

/// The value of a field `:name value` of a node.
fn field<'t>(node: &'t str, name: &str) -> Result<&'t str> {
    let key = format!(":{name} ");
    let at = node.find(&key).ok_or_else(bad_node)? + key.len();
    let rest = &node[at..];
    Ok(rest.split(' ').next().unwrap_or(""))
}

/// `stringToNode` for a list of `Const` nodes, which is the form of each `proargdefaults` of the built-in functions.
fn consts(text: &str) -> Result<Vec<Const>> {
    let inner = text.strip_prefix('(').and_then(|t| t.strip_suffix(')')).ok_or_else(bad_node)?;
    let mut out = Vec::new();
    for node in inner.split('}').map(str::trim).filter(|n| !n.is_empty()) {
        let node = node.strip_prefix("{CONST ").ok_or_else(bad_node)?;
        let ty = field(node, "consttype")?.parse().map_err(|_| bad_node())?;
        let typmod = field(node, "consttypmod")?.parse().map_err(|_| bad_node())?;
        let bytes = if field(node, "constisnull")? == "true" {
            None
        } else {
            let at = node.find('[').ok_or_else(bad_node)?;
            let end = node.rfind(']').ok_or_else(bad_node)?;
            // `outDatum` writes each byte as a signed `char`.
            #[allow(clippy::cast_sign_loss)]
            let bytes = node[at + 1..end]
                .split_whitespace()
                .map(|b| b.parse::<i8>().map(|b| b as u8).map_err(|_| bad_node()))
                .collect::<Result<Vec<u8>>>()?;
            Some(bytes)
        };
        out.push(Const { ty, typmod, bytes });
    }
    Ok(out)
}

/// The text form of the value of a `Const`, for the types that the defaults of the built-in functions have.
fn const_text(c: &Const, bytes: &[u8]) -> Result<String> {
    let word = |n: usize| -> Result<[u8; 4]> {
        bytes.get(n..n + 4).and_then(|b| b.try_into().ok()).ok_or_else(bad_node)
    };
    let datum = || -> Result<[u8; 8]> { bytes.try_into().map_err(|_| bad_node()) };
    Ok(match c.ty {
        oid::BOOL => (bytes.first() != Some(&0)).to_string(),
        oid::INT2 | oid::INT4 | oid::INT8 => i64::from_le_bytes(datum()?).to_string(),
        oid::FLOAT8 => f64::from_le_bytes(datum()?).to_string(),
        oid::TEXT => {
            // A varlena with a 4-byte header: the size in the upper 30 bits, with the header.
            let size = (u32::from_le_bytes(word(0)?) >> 2) as usize;
            let data = bytes.get(4..size).ok_or_else(bad_node)?;
            String::from_utf8(data.to_vec()).map_err(|_| bad_node())?
        }
        oid::JSONB => {
            let root = u32::from_le_bytes(word(4)?);
            match (root & JSONB_COUNT, root & (JSONB_OBJECT | JSONB_ARRAY)) {
                (0, JSONB_OBJECT) => "{}".to_owned(),
                (0, JSONB_ARRAY) => "[]".to_owned(),
                _ => return Err(bad_node()),
            }
        }
        // An array with no dimensions.
        _ if u32::from_le_bytes(word(4)?) == 0 => "{}".to_owned(),
        _ => return Err(bad_node()),
    })
}

/// `deparse_expression` of a `Const` default.
fn default_text(c: &Const, session: &dyn Session) -> Result<String> {
    let value = match &c.bytes {
        Some(bytes) => input(c.ty, &const_text(c, bytes)?, c.typmod, session)?,
        None => Value::Null,
    };
    let expr = Expr::constant(value.clone(), c.ty, c.typmod, None);
    let mut d = Deparser::new(session, false, 0);
    d.constant(&expr, &value, 0)?;
    Ok(d.buf)
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
    if defaults
        && proc.nargdefaults > 0
        && let Some(text) = proc.argdefaults
    {
        argdefaults = consts(text)?;
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
    let Some(text) = proc.argdefaults else { return Ok(Value::Null) };
    let input_no = info[..nth].iter().filter(|(_, _, mode)| is_input(*mode)).count();
    let defaults = consts(text)?;
    let lacking = i64::from(proc.nargs) - i64::from(proc.nargdefaults);
    let index = i64::try_from(input_no).unwrap_or(i64::MAX) - 1 - lacking;
    match usize::try_from(index).ok().and_then(|i| defaults.get(i)) {
        Some(c) => Ok(Value::text(default_text(c, call.session)?)),
        None => Ok(Value::Null),
    }
}
