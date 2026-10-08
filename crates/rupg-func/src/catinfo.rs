//! The catalog information functions: the `pg_*_is_visible` functions, `pg_get_userbyid`, and the comments of `obj_description`, `col_description` and `shobj_description`.
//!
//! The visibility functions are a port of the `*IsVisible` functions of `namespace.c`. Each one gives null for an OID that no object has, as the `is_missing` form of PostgreSQL does.

use rupg_common::Result;
use rupg_pgcatalog::builtin::{self, Named};
use rupg_types::Value;

use crate::reg::{self, path};
use crate::{Call, Kernel, bad_value};

/// The OID of `pg_catalog`.
const PG_CATALOG: u32 = 11;
/// The OID of `pg_class` in `pg_class`, the catalog of the comments of the columns.
const PG_CLASS: u32 = 1259;

/// The OID argument at `index`.
fn oid_arg(args: &[Value], index: usize) -> Result<u32> {
    args.get(index).and_then(Value::as_oid).ok_or_else(bad_value)
}

/// The visibility of an object of a kind that [`Named`] has, such as a relation or an operator class.
fn named_visible(kind: Named, call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let oid = oid_arg(args, 0)?;
    Ok(builtin::named_by_oid(kind, oid)
        .map_or(Value::Null, |row| Value::Bool(reg::visible(kind, row, call.session))))
}

/// `pg_type_is_visible`: true when the first type with the name in the search path is this type.
fn type_visible(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Some(row) = builtin::type_by_oid(oid_arg(args, 0)?) else { return Ok(Value::Null) };
    let found = path(call.session).into_iter().find_map(|ns| builtin::type_by_name(ns, row.name));
    Ok(Value::Bool(found.is_some_and(|t| t.oid == row.oid)))
}

/// `pg_function_is_visible`.
fn function_visible(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(builtin::proc_by_oid(oid_arg(args, 0)?)
        .map_or(Value::Null, |proc| Value::Bool(reg::function_visible(proc, call.session))))
}

/// `pg_operator_is_visible`.
fn operator_visible(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(builtin::operator_by_oid(oid_arg(args, 0)?)
        .map_or(Value::Null, |op| Value::Bool(reg::operator_visible(op, call.session))))
}

/// `pg_statistics_obj_is_visible`. The catalog has no extended statistics object, so each OID gives null.
fn statistics_obj_visible(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    oid_arg(args, 0)?;
    Ok(Value::Null)
}

/// `pg_get_userbyid`: the name of the role, or `unknown (OID=n)`.
fn get_userbyid(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let oid = oid_arg(args, 0)?;
    Ok(Value::Text(match builtin::named_by_oid(Named::Role, oid) {
        Some(row) => row.name.to_string(),
        None => format!("unknown (OID={oid})"),
    }))
}

/// The OID of the catalog with this name in `pg_catalog`, as the subquery of `obj_description` finds it.
fn catalog_oid(args: &[Value]) -> Result<Option<u32>> {
    let name = args.get(1).and_then(Value::as_str).ok_or_else(bad_value)?;
    let rows = builtin::named(Named::Class);
    Ok(rows.iter().find(|r| r.namespace == PG_CATALOG && r.name == name).map(|r| r.oid))
}

/// A comment as the result of a function, or null.
fn comment(text: Option<&str>) -> Value {
    text.map_or(Value::Null, Value::text)
}

/// `obj_description(oid, name)`: the comment of an object of the catalog with the name.
fn obj_description(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let oid = oid_arg(args, 0)?;
    Ok(comment(catalog_oid(args)?.and_then(|class| builtin::description(oid, class, 0))))
}

/// `obj_description(oid)`: the first comment of an object with the OID in any catalog.
fn obj_description_any(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let oid = oid_arg(args, 0)?;
    let found = builtin::descriptions().iter().find(|d| d.objoid == oid && d.objsubid == 0);
    Ok(comment(found.map(|d| d.description)))
}

/// `col_description(oid, integer)`: the comment of a column of a relation.
fn col_description(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let oid = oid_arg(args, 0)?;
    let column = args
        .get(1)
        .and_then(Value::as_i64)
        .and_then(|n| i32::try_from(n).ok())
        .ok_or_else(bad_value)?;
    Ok(comment(builtin::description(oid, PG_CLASS, column)))
}

/// `shobj_description(oid, name)`: the comment of a shared object of the catalog with the name.
fn shobj_description(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let oid = oid_arg(args, 0)?;
    Ok(comment(catalog_oid(args)?.and_then(|class| builtin::shared_description(oid, class))))
}

/// The kernel of a function of this module by its `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    let kernel: Kernel = match src {
        "pg_table_is_visible" => |c, a| named_visible(Named::Class, c, a),
        "pg_collation_is_visible" => |c, a| named_visible(Named::Collation, c, a),
        "pg_conversion_is_visible" => |c, a| named_visible(Named::Conversion, c, a),
        "pg_opclass_is_visible" => |c, a| named_visible(Named::Opclass, c, a),
        "pg_opfamily_is_visible" => |c, a| named_visible(Named::Opfamily, c, a),
        "pg_ts_config_is_visible" => |c, a| named_visible(Named::Config, c, a),
        "pg_ts_dict_is_visible" => |c, a| named_visible(Named::Dictionary, c, a),
        "pg_ts_parser_is_visible" => |c, a| named_visible(Named::Parser, c, a),
        "pg_ts_template_is_visible" => |c, a| named_visible(Named::Template, c, a),
        "pg_type_is_visible" => type_visible,
        "pg_function_is_visible" => function_visible,
        "pg_operator_is_visible" => operator_visible,
        "pg_statistics_obj_is_visible" => statistics_obj_visible,
        "pg_get_userbyid" => get_userbyid,
        _ => return None,
    };
    Some(kernel)
}

/// The kernel of a function in SQL of `system_functions.sql` by its OID.
pub(crate) fn sql_function(func: u32) -> Option<Kernel> {
    Some(match func {
        1215 => obj_description,
        1216 => col_description,
        1348 => obj_description_any,
        1993 => shobj_description,
        _ => return None,
    })
}
