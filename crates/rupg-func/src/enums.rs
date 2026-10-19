//! The functions of the enum types that `enum.c` has: `enum_first`, `enum_last` and `enum_range`. The comparisons are in [`crate::compare`].

use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin;
use rupg_types::{Array, Value};

use crate::io::{base_type, check_safe_enum_use, is_enum, type_name};
use crate::{Call, Kernel, bad_value};

/// The kernel of a function of the enum types by its `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    Some(match src {
        "enum_first" => enum_first,
        "enum_last" => enum_last,
        "enum_range_all" => enum_range_all,
        "enum_range_bounds" => enum_range_bounds,
        _ => return None,
    })
}

/// The labels of the enum type of the first argument, as `get_fn_expr_argtype` gives the type. These functions are not strict, so they get the type also from a null argument.
fn labels(call: &Call<'_>) -> Result<(u32, &'static [(u32, String)])> {
    let ty = call.args.first().map_or(0, |&ty| base_type(ty));
    match builtin::enum_labels(ty) {
        Some(labels) if is_enum(ty) => Ok((ty, labels)),
        _ => {
            Err(Error::new(SqlState::FEATURE_NOT_SUPPORTED, "could not determine actual enum type"))
        }
    }
}

/// `enum_endpoint`: the first or the last label, or an error for a type with no labels.
fn endpoint(call: &Call<'_>, last: bool) -> Result<Value> {
    let (ty, labels) = labels(call)?;
    let label = if last { labels.last() } else { labels.first() };
    match label {
        Some((oid, name)) => {
            check_safe_enum_use(ty, *oid, name)?;
            Ok(Value::Oid(*oid))
        }
        None => Err(Error::new(
            SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
            format!("enum {} contains no values", type_name(ty)),
        )),
    }
}

fn enum_first(call: &Call<'_>, _: &[Value]) -> Result<Value> {
    endpoint(call, false)
}

fn enum_last(call: &Call<'_>, _: &[Value]) -> Result<Value> {
    endpoint(call, true)
}

/// The labels from `lower` to `upper` in their sort order, as an array. A null bound is the first or the last label. `enum_range_internal` checks each label of the array.
fn range(call: &Call<'_>, lower: Option<u32>, upper: Option<u32>) -> Result<Value> {
    let (ty, labels) = labels(call)?;
    let position =
        |bound: u32| labels.iter().position(|(oid, _)| *oid == bound).ok_or_else(bad_value);
    let start = lower.map_or(Ok(0), position)?;
    let end = upper.map_or(Ok(labels.len()), |bound| position(bound).map(|i| i + 1))?;
    let values = labels
        .get(start..end.max(start))
        .unwrap_or_default()
        .iter()
        .map(|(oid, name)| check_safe_enum_use(ty, *oid, name).map(|()| Some(Value::Oid(*oid))))
        .collect::<Result<_>>()?;
    Ok(Value::Array(Box::new(Array::one(values))))
}

fn enum_range_all(call: &Call<'_>, _: &[Value]) -> Result<Value> {
    range(call, None, None)
}

fn enum_range_bounds(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let bound = |i: usize| match args.get(i) {
        Some(Value::Oid(oid)) => Ok(Some(*oid)),
        Some(Value::Null) | None => Ok(None),
        Some(_) => Err(bad_value()),
    };
    range(call, bound(0)?, bound(1)?)
}
