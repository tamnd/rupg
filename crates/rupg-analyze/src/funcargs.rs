//! The defaults of the arguments of a function and the calls with named arguments.
//!
//! `proargdefaults` holds a list of `Const` nodes as the text of `nodeToString`, which [`arg_defaults`] reads. The analyzer uses only the types of the defaults, and the call keeps the arguments as the query gives them. [`expand_arguments`] is `expand_function_arguments` of `clauses.c`: the planner puts the arguments in the order of the function and adds the defaults.

use std::borrow::Cow;

use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin::{self, ProcRow};
use rupg_types::{Array, Value, oid};

use crate::expr::{Expr, ExprKind, Func};

/// `JB_FOBJECT` and `JB_FARRAY`: the flags of the root container of a `jsonb` value. The low bits hold the number of the items.
const JSONB_OBJECT: u32 = 0x2000_0000;
const JSONB_ARRAY: u32 = 0x4000_0000;
const JSONB_COUNT: u32 = 0x0FFF_FFFF;

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

/// `stringToNode` for the `proargdefaults` of a function: a `Const` for each default, in the order of the arguments. A function with no defaults gives no expressions.
///
/// # Errors
///
/// An internal error for a node that is not a `Const` of a type that the defaults of the built-in functions have.
pub fn arg_defaults(proc: &ProcRow) -> Result<Vec<Expr>> {
    let Some(text) = proc.argdefaults else { return Ok(Vec::new()) };
    let inner = text.strip_prefix('(').and_then(|t| t.strip_suffix(')')).ok_or_else(bad_node)?;
    let mut out = Vec::new();
    for node in inner.split('}').map(str::trim).filter(|n| !n.is_empty()) {
        let node = node.strip_prefix("{CONST ").ok_or_else(bad_node)?;
        let ty = field(node, "consttype")?.parse().map_err(|_| bad_node())?;
        let typmod = field(node, "consttypmod")?.parse().map_err(|_| bad_node())?;
        let value = if field(node, "constisnull")? == "true" {
            Value::Null
        } else {
            let at = node.find('[').ok_or_else(bad_node)?;
            let end = node.rfind(']').ok_or_else(bad_node)?;
            // `outDatum` writes each byte as a signed `char`.
            #[allow(clippy::cast_sign_loss)]
            let bytes = node[at + 1..end]
                .split_whitespace()
                .map(|b| b.parse::<i8>().map(|b| b as u8).map_err(|_| bad_node()))
                .collect::<Result<Vec<u8>>>()?;
            const_value(ty, &bytes)?
        };
        out.push(Expr::constant(value, ty, typmod, None));
    }
    Ok(out)
}

/// The value of the bytes of a `Datum`, for the types that the defaults of the built-in functions have.
fn const_value(ty: u32, bytes: &[u8]) -> Result<Value> {
    let word = |n: usize| -> Result<[u8; 4]> {
        bytes.get(n..n + 4).and_then(|b| b.try_into().ok()).ok_or_else(bad_node)
    };
    let datum = || -> Result<[u8; 8]> { bytes.try_into().map_err(|_| bad_node()) };
    // A value that is passed by value is in the low bytes of the `Datum`.
    #[allow(clippy::cast_possible_truncation)]
    Ok(match ty {
        oid::BOOL => Value::Bool(bytes.first() != Some(&0)),
        oid::INT2 => Value::Int2(i64::from_le_bytes(datum()?) as i16),
        oid::INT4 => Value::Int4(i64::from_le_bytes(datum()?) as i32),
        oid::INT8 => Value::Int8(i64::from_le_bytes(datum()?)),
        oid::FLOAT8 => Value::Float8(f64::from_le_bytes(datum()?)),
        oid::TEXT => {
            // A varlena with a 4-byte header: the size in the upper 30 bits, with the header.
            let size = (u32::from_le_bytes(word(0)?) >> 2) as usize;
            let data = bytes.get(4..size).ok_or_else(bad_node)?;
            Value::Text(String::from_utf8(data.to_vec()).map_err(|_| bad_node())?)
        }
        oid::JSONB => {
            let root = u32::from_le_bytes(word(4)?);
            match (root & JSONB_COUNT, root & (JSONB_OBJECT | JSONB_ARRAY)) {
                (0, JSONB_OBJECT) => Value::Text("{}".to_owned()),
                (0, JSONB_ARRAY) => Value::Text("[]".to_owned()),
                _ => return Err(bad_node()),
            }
        }
        // An array with no dimensions.
        _ if u32::from_le_bytes(word(4)?) == 0 => Value::Array(Box::new(Array::empty())),
        _ => return Err(bad_node()),
    })
}

/// `expand_function_arguments`: the arguments of a call in the order of the function, with the defaults of the arguments that the call does not give. A call with no named argument that gives all the arguments keeps its list.
///
/// # Errors
///
/// An internal error when the call does not fit the function, which the analyzer prevents.
pub fn expand_arguments(f: &Func) -> Result<Cow<'_, [Expr]>> {
    let named = f.args.iter().any(|a| matches!(a.kind, ExprKind::NamedArg(_)));
    let Some(proc) = builtin::proc_by_oid(f.oid) else { return Ok(Cow::Borrowed(&f.args)) };
    let pronargs = proc.argtypes.len();
    if !named {
        if f.args.len() >= pronargs || proc.nargdefaults == 0 {
            return Ok(Cow::Borrowed(&f.args));
        }
        // add_function_defaults: the defaults of the last arguments.
        let defaults = arg_defaults(proc)?;
        let skip = (f.args.len() + defaults.len())
            .checked_sub(pronargs)
            .ok_or_else(|| Error::internal("not enough default arguments"))?;
        let mut args = f.args.clone();
        args.extend(defaults.into_iter().skip(skip));
        return Ok(Cow::Owned(args));
    }
    // reorder_function_arguments: each argument at its number, then the defaults in the gaps.
    let mut slots: Vec<Option<Expr>> = vec![None; pronargs];
    let mut next = 0;
    for arg in &f.args {
        let (number, arg) = match &arg.kind {
            ExprKind::NamedArg(na) => (na.number, &na.arg),
            _ => {
                next += 1;
                (next - 1, arg)
            }
        };
        let slot = slots.get_mut(number).ok_or_else(|| Error::internal("too many arguments"))?;
        *slot = Some(arg.clone());
    }
    if f.args.len() < pronargs {
        let first = pronargs.saturating_sub(usize::try_from(proc.nargdefaults).unwrap_or(0));
        for (slot, default) in slots[first..].iter_mut().zip(arg_defaults(proc)?) {
            if slot.is_none() {
                *slot = Some(default);
            }
        }
    }
    let args = slots
        .into_iter()
        .map(|slot| slot.ok_or_else(|| Error::internal("an argument with no value")))
        .collect::<Result<Vec<Expr>>>()?;
    Ok(Cow::Owned(args))
}
