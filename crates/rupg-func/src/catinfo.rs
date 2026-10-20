//! The catalog information functions: the `pg_*_is_visible` functions, `pg_get_userbyid`, `pg_my_temp_schema`, `pg_is_other_temp_schema`, `pg_get_sequence_data`, `pg_get_serial_sequence`, the functions of the partition trees, `pg_relation_is_publishable`, and the comments of `obj_description`, `col_description` and `shobj_description`.
//!
//! The visibility functions are a port of the `*IsVisible` functions of `namespace.c`. Each one gives null for an OID that no object has, as the `is_missing` form of PostgreSQL does.

use rupg_catalog::{DepKind, ObjRef, RelKind};
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin::{self, Named};
use rupg_types::{Record, Value, oid, qualified_name_list};

use crate::reg;
use crate::{Call, Kernel, bad_value, type_error};

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
    if matches!(
        kind,
        Named::Collation | Named::Config | Named::Dictionary | Named::Parser | Named::Template
    ) {
        return Ok(reg::catalog_visible(kind, oid, call.session).map_or(Value::Null, Value::Bool));
    }
    Ok(builtin::named_by_oid(kind, oid)
        .map_or(Value::Null, |row| Value::Bool(reg::visible(kind, row, call.session))))
}

/// `pg_table_is_visible`: true when the first relation with the name in the search path is this relation.
fn table_visible(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(reg::class_visible(oid_arg(args, 0)?, call.session).map_or(Value::Null, Value::Bool))
}

/// `pg_type_is_visible`: true when the first type with the name in the search path is this type.
fn type_visible(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(reg::type_visible(oid_arg(args, 0)?, call.session).map_or(Value::Null, Value::Bool))
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
    Ok(Value::Text(match rupg_pgcatalog::roles::current().name(oid) {
        Some(name) => name.to_string(),
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

/// The comment of an object: a comment of the catalog of the session, else a built-in comment.
fn description(call: &Call<'_>, object: ObjRef) -> Value {
    let user = call.session.catalog().and_then(|c| c.description(object));
    comment(user.or_else(|| builtin::description(object.oid, object.class, object.sub)))
}

/// `obj_description(oid, name)`: the comment of an object of the catalog with the name.
fn obj_description(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let oid = oid_arg(args, 0)?;
    Ok(match catalog_oid(args)? {
        Some(class) => description(call, ObjRef::new(class, oid)),
        None => Value::Null,
    })
}

/// `obj_description(oid)`: the first comment of an object with the OID in any catalog. The scan of `pg_description` gives the built-in rows first.
fn obj_description_any(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let oid = oid_arg(args, 0)?;
    let found = builtin::descriptions().iter().find(|d| d.objoid == oid && d.objsubid == 0);
    let user = || {
        let mut rows = call.session.catalog()?.descriptions();
        rows.find(|(object, _)| object.oid == oid && object.sub == 0).map(|(_, text)| text)
    };
    Ok(comment(found.map(|d| d.description).or_else(user)))
}

/// `col_description(oid, integer)`: the comment of a column of a relation.
fn col_description(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let oid = oid_arg(args, 0)?;
    let column = args
        .get(1)
        .and_then(Value::as_i64)
        .and_then(|n| i32::try_from(n).ok())
        .ok_or_else(bad_value)?;
    Ok(description(call, ObjRef { class: PG_CLASS, oid, sub: column }))
}

/// `shobj_description(oid, name)`: the comment of a shared object of the catalog with the name.
fn shobj_description(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let oid = oid_arg(args, 0)?;
    Ok(comment(catalog_oid(args)?.and_then(|class| builtin::shared_description(oid, class))))
}

/// `pg_is_other_temp_schema(oid)`: `isOtherTempNamespace`, true for a temporary schema of another session.
fn is_other_temp_schema(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let oid = oid_arg(args, 0)?;
    let temp = reg::namespace_name(oid, call.session)
        .is_some_and(|name| name.starts_with("pg_temp_") || name.starts_with("pg_toast_temp_"));
    Ok(Value::Bool(temp && !reg::is_own_temp(oid, call.session)))
}

/// `pg_get_sequence_data(regclass)`: the last value, `is_called` and the LSN of the page of a sequence, which `pg_dump` reads. rupg keeps no pages with an LSN, so the LSN is 0/00000000. Each field is null for an OID that no relation has, for a relation that is not a sequence, and for a sequence on which the current user has no `SELECT` and no `USAGE`.
fn get_sequence_data(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let relid = oid_arg(args, 0)?;
    let sequence = call
        .session
        .catalog()
        .and_then(|c| c.relation(relid))
        .filter(|r| r.kind == RelKind::Sequence)
        .and_then(|r| r.sequence.as_ref());
    let values = match sequence {
        Some(info) if crate::acl::sequence_readable(relid, call.session)? => {
            let row = crate::sequence::row(info, call.session);
            vec![Value::Int8(row.last_value), Value::Bool(row.is_called), Value::Int8(0)]
        }
        _ => vec![Value::Null; 3],
    };
    Ok(Value::Record(Box::new(Record::new(vec![oid::INT8, oid::BOOL, oid::PG_LSN], values))))
}

/// `pg_partition_root(regclass)`: the top parent of the partition tree of a relation. rupg has no partitioned tables yet, so `check_rel_can_be_partition` is false for each relation, and the result is null.
fn partition_root(_: &Call<'_>, _: &[Value]) -> Result<Value> {
    Ok(Value::Null)
}

/// `pg_partition_tree(regclass)` and `pg_partition_ancestors(regclass)`: the members of the partition tree of a relation. rupg has no partitioned tables yet, so the result has no rows, as for a relation that is not in a partition tree.
pub(crate) fn partition_members(_: &Call<'_>, _: &[Value]) -> Result<Vec<Vec<Value>>> {
    Ok(Vec::new())
}

/// `pg_relation_is_publishable(regclass)`: `is_publishable_class`, true for a permanent table or sequence that is not a part of the catalog. Each relation of the catalog of the session has an OID from `FirstNormalObjectId` and is permanent, so a table or a sequence of the catalog is publishable, and a built-in relation is not. The result is null for an OID that no relation has.
fn relation_is_publishable(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let relid = oid_arg(args, 0)?;
    if reg::class_name(relid, call.session).is_none() {
        return Ok(Value::Null);
    }
    let kind = call.session.catalog().and_then(|c| c.relation(relid)).map(|r| r.kind);
    Ok(Value::Bool(matches!(kind, Some(RelKind::Table | RelKind::Sequence))))
}

/// `pg_get_serial_sequence(text, text)`: the sequence of a serial or identity column, with its schema. The table is a name that `search_path` finds, and the column is a name as it is. A sequence of a serial column has an automatic dependency on the column, and a sequence of an identity column has an internal one. The result is null for a column without a sequence.
fn serial_sequence(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (Some(table), Some(column)) = (args[0].as_str(), args[1].as_str()) else {
        return Err(bad_value());
    };
    let names = qualified_name_list(table).map_err(type_error)?;
    let relid = reg::relation_oid(&names, call.session)?;
    let Some(attnum) = crate::acl::attnum(relid, column, call.session) else {
        let relname = names.last().map_or("", |n| &**n);
        return Err(Error::new(
            SqlState::UNDEFINED_COLUMN,
            format!("column \"{column}\" of relation \"{relname}\" does not exist"),
        ));
    };
    let Some(catalog) = call.session.catalog() else { return Ok(Value::Null) };
    let column = ObjRef::column(relid, attnum);
    let sequence = catalog.depends().iter().find(|d| {
        d.referenced == column
            && d.object.class == PG_CLASS
            && d.object.sub == 0
            && matches!(d.kind, DepKind::Auto | DepKind::Internal)
            && catalog.relation(d.object.oid).is_some_and(|r| r.kind == RelKind::Sequence)
    });
    let Some(sequence) = sequence else { return Ok(Value::Null) };
    let Some((name, namespace)) = reg::class_name(sequence.object.oid, call.session) else {
        return Ok(Value::Null);
    };
    Ok(Value::Text(reg::qualified(reg::namespace_name(namespace, call.session), name)))
}

/// The kernel of a function of this module by its `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    let kernel: Kernel = match src {
        "pg_table_is_visible" => table_visible,
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
        // A session has no temporary schema yet, so `myTempNamespace` is 0.
        "pg_my_temp_schema" => |call, _| Ok(Value::Oid(reg::temp_namespace(call.session))),
        "pg_is_other_temp_schema" => is_other_temp_schema,
        "pg_get_sequence_data" => get_sequence_data,
        "pg_partition_root" => partition_root,
        "pg_relation_is_publishable" => relation_is_publishable,
        "pg_get_serial_sequence" => serial_sequence,
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
        2074 => crate::regex::similar_substring,
        _ => return None,
    })
}
