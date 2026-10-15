//! The functions and the operators of `jsonfuncs.c` that change a `jsonb` value: `||`, `-`, `#-`, `jsonb_set`, `jsonb_set_lax` and `jsonb_insert`.
//!
//! A function writes the text of the new value from the parts of the old value and gives the text to `jsonb_in`. Then the keys of each object are in their order and each key has only its last field, as `pushJsonbValue` makes them.

use rupg_common::{Error, Result, SqlState};
use rupg_types::{JsonItem, JsonPathOp, JsonTree, Value, json_escape, jsonb_in};

use crate::json::{invalid, text, tree};
use crate::{Call, Kernel, bad_value, type_error};

/// The kernel of a function that changes a `jsonb` value, by its `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    Some(match src {
        "jsonb_concat" => jsonb_concat,
        "jsonb_delete" => jsonb_delete,
        "jsonb_delete_array" => jsonb_delete_array,
        "jsonb_delete_idx" => jsonb_delete_idx,
        "jsonb_delete_path" => jsonb_delete_path,
        "jsonb_set" => jsonb_set,
        "jsonb_set_lax" => jsonb_set_lax,
        "jsonb_insert" => jsonb_insert,
        _ => return None,
    })
}

/// The `jsonb` value of a text in the form that `jsonb_out` prints.
fn finish(text: &str) -> Result<Value> {
    jsonb_in(text).map(Value::Text).map_err(type_error)
}

/// The input value as it is.
fn same(tree: &JsonTree<'_>) -> Value {
    Value::text(tree.source())
}

fn is_scalar(tree: &JsonTree<'_>) -> bool {
    !matches!(tree.item(0), JsonItem::Array(_) | JsonItem::Object(_))
}

fn is_object(tree: &JsonTree<'_>) -> bool {
    matches!(tree.item(0), JsonItem::Object(_))
}

/// `JB_ROOT_COUNT`: the number of the elements or the fields at the root. A scalar at the root is one element.
fn root_count(tree: &JsonTree<'_>) -> usize {
    match tree.item(0) {
        JsonItem::Array(items) => items.len(),
        JsonItem::Object(fields) => fields.len(),
        _ => 1,
    }
}

/// Puts the separator before a part that is not the first one.
fn comma(out: &mut String) {
    if !out.ends_with(['[', '{']) {
        out.push_str(", ");
    }
}

/// Puts the fields of the object at the root.
fn put_fields(out: &mut String, tree: &JsonTree<'_>, keep: impl Fn(&str) -> bool) {
    if let JsonItem::Object(fields) = tree.item(0) {
        for (name, at) in fields.iter().filter(|f| keep(&f.0)) {
            comma(out);
            json_escape(name, out);
            out.push_str(": ");
            out.push_str(tree.text(*at));
        }
    }
}

/// Puts the parts that `||` puts in an array: the elements of an array, or else the value itself.
fn put_parts(out: &mut String, tree: &JsonTree<'_>) {
    match tree.item(0) {
        JsonItem::Array(items) => {
            for &at in items {
                comma(out);
                out.push_str(tree.text(at));
            }
        }
        _ => {
            comma(out);
            out.push_str(tree.source());
        }
    }
}

/// `jsonb_concat`: `jsonb || jsonb`. Two objects give one object where the fields on the right win. Else the result is an array of the parts of the two values.
fn jsonb_concat(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (left, right) = (tree(args, 0)?, tree(args, 1)?);
    if is_object(&left) == is_object(&right) {
        if root_count(&left) == 0 && !is_scalar(&right) {
            return Ok(same(&right));
        }
        if root_count(&right) == 0 && !is_scalar(&left) {
            return Ok(same(&left));
        }
    }
    let mut out = String::with_capacity(left.source().len() + right.source().len() + 4);
    if is_object(&left) && is_object(&right) {
        out.push('{');
        put_fields(&mut out, &left, |_| true);
        put_fields(&mut out, &right, |_| true);
        out.push('}');
    } else {
        out.push('[');
        put_parts(&mut out, &left);
        put_parts(&mut out, &right);
        out.push(']');
    }
    finish(&out)
}

/// The value without the fields with one of the keys, or without the string elements equal to one of the keys.
fn delete_keys(tree: &JsonTree<'_>, keys: &[&str]) -> Result<Value> {
    let mut out = String::with_capacity(tree.source().len());
    match tree.item(0) {
        JsonItem::Object(_) => {
            out.push('{');
            put_fields(&mut out, tree, |name| !keys.contains(&name));
            out.push('}');
        }
        JsonItem::Array(items) => {
            out.push('[');
            for &at in items {
                if matches!(tree.item(at), JsonItem::String(s) if keys.contains(&s.as_str())) {
                    continue;
                }
                comma(&mut out);
                out.push_str(tree.text(at));
            }
            out.push(']');
        }
        _ => return Ok(same(tree)),
    }
    finish(&out)
}

/// `jsonb_delete`: `jsonb - text`.
fn jsonb_delete(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let tree = tree(args, 0)?;
    if is_scalar(&tree) {
        return Err(invalid("cannot delete from scalar"));
    }
    if root_count(&tree) == 0 {
        return Ok(same(&tree));
    }
    delete_keys(&tree, &[text(args, 1)?])
}

/// The error for a path or a list of keys with more than one dimension.
fn subscripts() -> Error {
    Error::new(SqlState::ARRAY_SUBSCRIPT_ERROR, "wrong number of array subscripts")
}

/// `jsonb_delete_array`: `jsonb - text[]`. The null keys have no effect.
fn jsonb_delete_array(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Some(Value::Array(array)) = args.get(1) else { return Err(bad_value()) };
    if array.dims.len() > 1 {
        return Err(subscripts());
    }
    let tree = tree(args, 0)?;
    if is_scalar(&tree) {
        return Err(invalid("cannot delete from scalar"));
    }
    let keys: Vec<&str> = array
        .values
        .iter()
        .flatten()
        .map(|v| v.as_str().ok_or_else(bad_value))
        .collect::<Result<_>>()?;
    if root_count(&tree) == 0 || keys.is_empty() {
        return Ok(same(&tree));
    }
    delete_keys(&tree, &keys)
}

/// `jsonb_delete_idx`: `jsonb - integer`. A negative index counts from the end, and an index out of the array has no effect.
fn jsonb_delete_idx(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let tree = tree(args, 0)?;
    let index = args.get(1).and_then(Value::as_i64).ok_or_else(bad_value)?;
    let items = match tree.item(0) {
        JsonItem::Array(items) => items,
        JsonItem::Object(_) => {
            return Err(invalid("cannot delete from object using integer index"));
        }
        _ => return Err(invalid("cannot delete from scalar")),
    };
    let count = items.len() as i64;
    let index = if index < 0 && -index <= count { index + count } else { index };
    if index < 0 || index >= count {
        return Ok(same(&tree));
    }
    let mut out = String::with_capacity(tree.source().len());
    out.push('[');
    for (i, &at) in items.iter().enumerate() {
        if i as i64 != index {
            comma(&mut out);
            out.push_str(tree.text(at));
        }
    }
    out.push(']');
    finish(&out)
}

/// The elements of a path argument. A null element stays in its place, as `setPath` gives an error only when it gets to it.
fn path(args: &[Value], at: usize) -> Result<Vec<Option<&str>>> {
    let Some(Value::Array(array)) = args.get(at) else { return Err(bad_value()) };
    if array.dims.len() > 1 {
        return Err(subscripts());
    }
    array
        .values
        .iter()
        .map(|v| match v {
            Some(v) => v.as_str().map(Some).ok_or_else(bad_value),
            None => Ok(None),
        })
        .collect()
}

/// `setPath` on the value at the root.
fn set_path(
    tree: &JsonTree<'_>,
    path: &[Option<&str>],
    new: &str,
    op: JsonPathOp,
) -> Result<Value> {
    finish(&tree.set_path(path, new, op).map_err(type_error)?)
}

/// `jsonb_set` with the text of the new value.
fn set(args: &[Value], new: &str) -> Result<Value> {
    let path = path(args, 1)?;
    let create = args.get(3).and_then(Value::as_bool).unwrap_or(true);
    let tree = tree(args, 0)?;
    if is_scalar(&tree) {
        return Err(invalid("cannot set path in scalar"));
    }
    if (root_count(&tree) == 0 && !create) || path.is_empty() {
        return Ok(same(&tree));
    }
    set_path(&tree, &path, new, if create { JsonPathOp::Create } else { JsonPathOp::Replace })
}

/// `jsonb_set`.
fn jsonb_set(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    set(args, text(args, 2)?)
}

fn treatment_error() -> Error {
    invalid(
        "null_value_treatment must be \"delete_key\", \"return_target\", \"use_json_null\", or \"raise_exception\"",
    )
}

/// `jsonb_set_lax`: `jsonb_set`, and a choice of what a null new value does.
fn jsonb_set_lax(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    if [0, 1, 3].iter().any(|&at| matches!(args.get(at), None | Some(Value::Null))) {
        return Ok(Value::Null);
    }
    let Some(treatment) = args.get(4).and_then(Value::as_str) else {
        return Err(treatment_error());
    };
    if !matches!(args.get(2), None | Some(Value::Null)) {
        return jsonb_set(call, args);
    }
    match treatment {
        "raise_exception" => Err(Error::new(SqlState::NULL_VALUE_NOT_ALLOWED, "JSON value must not be null")
            .with_detail("Exception was raised because null_value_treatment is \"raise_exception\".")
            .with_hint("To avoid, either change the null_value_treatment argument or ensure that an SQL NULL is not passed.")),
        "use_json_null" => set(args, "null"),
        "delete_key" => jsonb_delete_path(call, args),
        "return_target" => Ok(args[0].clone()),
        _ => Err(treatment_error()),
    }
}

/// `jsonb_delete_path`: `jsonb #- text[]`.
fn jsonb_delete_path(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let path = path(args, 1)?;
    let tree = tree(args, 0)?;
    if is_scalar(&tree) {
        return Err(invalid("cannot delete path in scalar"));
    }
    if root_count(&tree) == 0 || path.is_empty() {
        return Ok(same(&tree));
    }
    set_path(&tree, &path, "", JsonPathOp::Delete)
}

/// `jsonb_insert`: a new element before or after the element at the path, or a new field when the key is not in the object.
fn jsonb_insert(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let path = path(args, 1)?;
    let after = args.get(3).and_then(Value::as_bool).unwrap_or(false);
    let tree = tree(args, 0)?;
    if is_scalar(&tree) {
        return Err(invalid("cannot set path in scalar"));
    }
    if path.is_empty() {
        return Ok(same(&tree));
    }
    let op = if after { JsonPathOp::InsertAfter } else { JsonPathOp::InsertBefore };
    set_path(&tree, &path, text(args, 2)?, op)
}
