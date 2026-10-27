//! The functions and the operators of `json` and `jsonb` from `jsonfuncs.c`, `jsonb.c` and `jsonb_op.c` that read into a value: the fields, the elements and the paths, the types, the lengths, the casts of `jsonb`, the tests of the keys, the containment, the order of `jsonb`, `strip_nulls` and `jsonb_pretty`.
//!
//! A function of `json` parses the whole text with the escapes decoded, as `makeJsonLexContext` with `need_escapes` does. Then `\u0000` in any part of the value is an error, also when the result is a different part.

use std::cmp::Ordering;

use rupg_common::{Error, Result, SqlState};
use rupg_types::{JsonItem, JsonTree, Value, json_index, numeric_in, oid};

use crate::cast::{numeric_to_float, numeric_to_int};
use crate::{Call, Kernel, bad_value, type_error};

/// The kernel of a `json` or `jsonb` function by its `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    Some(match src {
        "json_object_field" => json_object_field,
        "json_object_field_text" => json_object_field_text,
        "json_array_element" => json_array_element,
        "json_array_element_text" => json_array_element_text,
        "json_extract_path" => json_extract_path,
        "json_extract_path_text" => json_extract_path_text,
        "json_typeof" => json_typeof,
        "json_array_length" => json_array_length,
        "json_strip_nulls" => json_strip_nulls,
        "jsonb_object_field" => jsonb_object_field,
        "jsonb_object_field_text" => jsonb_object_field_text,
        "jsonb_array_element" => jsonb_array_element,
        "jsonb_array_element_text" => jsonb_array_element_text,
        "jsonb_extract_path" => jsonb_extract_path,
        "jsonb_extract_path_text" => jsonb_extract_path_text,
        "jsonb_typeof" => jsonb_typeof,
        "jsonb_array_length" => jsonb_array_length,
        "jsonb_strip_nulls" => jsonb_strip_nulls,
        "jsonb_pretty" => jsonb_pretty,
        "jsonb_bool" => jsonb_bool,
        "jsonb_numeric" => jsonb_numeric,
        "jsonb_int2" | "jsonb_int4" | "jsonb_int8" => jsonb_int,
        "jsonb_float4" | "jsonb_float8" => jsonb_float,
        "jsonb_exists" => jsonb_exists,
        "jsonb_exists_any" => jsonb_exists_any,
        "jsonb_exists_all" => jsonb_exists_all,
        "jsonb_contains" => jsonb_contains,
        "jsonb_contained" => jsonb_contained,
        "jsonb_eq" => jsonb_eq,
        "jsonb_ne" => jsonb_ne,
        "jsonb_lt" => jsonb_lt,
        "jsonb_gt" => jsonb_gt,
        "jsonb_le" => jsonb_le,
        "jsonb_ge" => jsonb_ge,
        "jsonb_cmp" => jsonb_cmp,
        _ => return None,
    })
}

/// The text of an argument.
pub(crate) fn text(args: &[Value], at: usize) -> Result<&str> {
    args.get(at).and_then(Value::as_str).ok_or_else(bad_value)
}

/// The parsed value of an argument.
pub(crate) fn tree(args: &[Value], at: usize) -> Result<JsonTree<'_>> {
    JsonTree::parse(text(args, at)?).map_err(type_error)
}

/// The elements of a `text[]` argument, or `None` when one of them is null.
fn path(args: &[Value], at: usize) -> Result<Option<Vec<&str>>> {
    let Some(Value::Array(array)) = args.get(at) else { return Err(bad_value()) };
    let mut out = Vec::with_capacity(array.values.len());
    for value in &array.values {
        match value {
            Some(v) => out.push(v.as_str().ok_or_else(bad_value)?),
            None => return Ok(None),
        }
    }
    Ok(Some(out))
}

/// The keys of a `text[]` argument, with no null elements.
fn keys(args: &[Value], at: usize) -> Result<Vec<&str>> {
    let Some(Value::Array(array)) = args.get(at) else { return Err(bad_value()) };
    array.values.iter().flatten().map(|v| v.as_str().ok_or_else(bad_value)).collect()
}

/// A `json` result, or null.
fn json_value(text: Option<&str>) -> Value {
    text.map_or(Value::Null, Value::text)
}

/// A text result, or null.
fn text_value(text: Option<String>) -> Value {
    text.map_or(Value::Null, Value::Text)
}

/// The node of the last field with the key `name` of the object at the root, as `get_worker` finds it.
fn object_field(tree: &JsonTree<'_>, name: &str) -> Option<usize> {
    tree.field(0, name)
}

/// The node of an element of the array at the root. A negative index counts from the end.
fn array_element(tree: &JsonTree<'_>, index: i64) -> Option<usize> {
    tree.element(0, index)
}

/// The index argument of an operator.
fn index(args: &[Value], at: usize) -> Result<i64> {
    args.get(at).and_then(Value::as_i64).ok_or_else(bad_value)
}

/// `json_object_field`: `json -> text`.
fn json_object_field(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let tree = tree(args, 0)?;
    Ok(json_value(object_field(&tree, text(args, 1)?).map(|at| tree.text(at))))
}

/// `json_object_field_text`: `json ->> text`.
fn json_object_field_text(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let tree = tree(args, 0)?;
    Ok(text_value(object_field(&tree, text(args, 1)?).and_then(|at| tree.as_text(at))))
}

/// `json_array_element`: `json -> integer`.
fn json_array_element(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let tree = tree(args, 0)?;
    Ok(json_value(array_element(&tree, index(args, 1)?).map(|at| tree.text(at))))
}

/// `json_array_element_text`: `json ->> integer`.
fn json_array_element_text(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let tree = tree(args, 0)?;
    Ok(text_value(array_element(&tree, index(args, 1)?).and_then(|at| tree.as_text(at))))
}

/// The node of a path in a `json` value, as `get_worker` finds it. Each step takes all the fields with the key, so a later field with the same key can give the result, also when the earlier one has the rest of the path. Of all the nodes that the path reaches, the last one in the text is the result.
fn json_path(tree: &JsonTree<'_>, path: &[&str]) -> Option<usize> {
    let mut nodes = vec![0];
    for step in path {
        let index = json_index(step);
        let mut next = Vec::new();
        for &node in &nodes {
            match tree.item(node) {
                JsonItem::Object(fields) => {
                    next.extend(fields.iter().filter(|(k, _)| k == step).map(|f| f.1));
                }
                JsonItem::Array(items) => {
                    let Some(index) = index else { continue };
                    let n = items.len() as i64;
                    let mut index = i64::from(index);
                    if index < 0 && -index <= n {
                        index += n;
                    }
                    if let Some(&item) = usize::try_from(index).ok().and_then(|i| items.get(i)) {
                        next.push(item);
                    }
                }
                _ => {}
            }
        }
        nodes = next;
    }
    nodes.last().copied()
}

/// `json_extract_path` and `json_extract_path_text`. With an empty path, a scalar at the root keeps the white space before it, as in PostgreSQL.
fn json_extract(args: &[Value], as_text: bool) -> Result<Value> {
    let Some(path) = path(args, 1)? else { return Ok(Value::Null) };
    let tree = tree(args, 0)?;
    if path.is_empty() {
        let root = tree.node(0);
        return Ok(match tree.item(0) {
            JsonItem::Array(_) | JsonItem::Object(_) => Value::text(tree.text(0)),
            JsonItem::String(s) if as_text => Value::text(s.as_str()),
            JsonItem::Null if as_text => Value::Null,
            _ => Value::text(&tree.source()[..root.end]),
        });
    }
    let Some(at) = json_path(&tree, &path) else { return Ok(Value::Null) };
    Ok(if as_text { text_value(tree.as_text(at)) } else { Value::text(tree.text(at)) })
}

/// `json_extract_path`: `json #> text[]`.
fn json_extract_path(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    json_extract(args, false)
}

/// `json_extract_path_text`: `json #>> text[]`.
fn json_extract_path_text(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    json_extract(args, true)
}

/// `json_typeof`: the type of the first token.
fn json_typeof(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let s = text(args, 0)?.trim_start_matches([' ', '\t', '\n', '\r']);
    Ok(Value::text(match s.bytes().next() {
        Some(b'{') => "object",
        Some(b'[') => "array",
        Some(b'"') => "string",
        Some(b't' | b'f') => "boolean",
        Some(b'n') => "null",
        _ => "number",
    }))
}

pub(crate) fn invalid(message: &str) -> Error {
    Error::new(SqlState::INVALID_PARAMETER_VALUE, message)
}

/// The length of an array at the root, or the error for an object or a scalar.
fn array_length(tree: &JsonTree<'_>) -> Result<Value> {
    match tree.item(0) {
        JsonItem::Array(items) => Ok(Value::Int4(i32::try_from(items.len()).unwrap_or(i32::MAX))),
        JsonItem::Object(_) => Err(invalid("cannot get array length of a non-array")),
        _ => Err(invalid("cannot get array length of a scalar")),
    }
}

/// `json_array_length`. It does not decode the escapes, so `\u0000` is not an error here.
fn json_array_length(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    array_length(&JsonTree::parse_with(text(args, 0)?, false).map_err(type_error)?)
}

/// The optional `strip_in_arrays` argument.
fn strip_arrays(args: &[Value]) -> bool {
    args.get(1).and_then(Value::as_bool).unwrap_or(false)
}

/// `json_strip_nulls`.
fn json_strip_nulls(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Text(tree(args, 0)?.strip_nulls(strip_arrays(args), false)))
}

/// `jsonb_object_field`: `jsonb -> text`.
fn jsonb_object_field(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let tree = tree(args, 0)?;
    Ok(json_value(object_field(&tree, text(args, 1)?).map(|at| tree.text(at))))
}

/// `jsonb_object_field_text`: `jsonb ->> text`.
fn jsonb_object_field_text(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let tree = tree(args, 0)?;
    Ok(text_value(object_field(&tree, text(args, 1)?).and_then(|at| tree.as_text(at))))
}

/// The node of an element of a `jsonb` value. A scalar at the root is an array with one element.
fn jsonb_element(tree: &JsonTree<'_>, index: i64) -> Option<usize> {
    match tree.item(0) {
        JsonItem::Array(_) => tree.element(0, index),
        JsonItem::Object(_) => None,
        _ => (index == 0 || index == -1).then_some(0),
    }
}

/// `jsonb_array_element`: `jsonb -> integer`.
fn jsonb_array_element(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let tree = tree(args, 0)?;
    Ok(json_value(jsonb_element(&tree, index(args, 1)?).map(|at| tree.text(at))))
}

/// `jsonb_array_element_text`: `jsonb ->> integer`.
fn jsonb_array_element_text(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let tree = tree(args, 0)?;
    Ok(text_value(jsonb_element(&tree, index(args, 1)?).and_then(|at| tree.as_text(at))))
}

/// `jsonb_get_element`: the node of a path. A scalar at the root has no parts, and an index that is not an integer gives null.
fn jsonb_path(tree: &JsonTree<'_>, path: &[&str]) -> Option<usize> {
    let mut node = 0;
    for step in path {
        node = match tree.item(node) {
            JsonItem::Object(_) => tree.field(node, step)?,
            JsonItem::Array(_) => tree.element(node, i64::from(json_index(step)?))?,
            _ => return None,
        };
    }
    Some(node)
}

/// `jsonb_subscript_fetch`: the part of the `jsonb` value at the subscripts `path`, as `jsonb_get_element` finds it, or null.
///
/// # Errors
///
/// A value that is not valid `jsonb`.
pub fn jsonb_subscript_fetch(source: &str, path: &[&str]) -> Result<Value> {
    let tree = JsonTree::parse(source).map_err(type_error)?;
    Ok(jsonb_path(&tree, path).map_or(Value::Null, |at| Value::text(tree.text(at))))
}

/// `jsonb_extract_path` and `jsonb_extract_path_text`.
fn jsonb_extract(args: &[Value], as_text: bool) -> Result<Value> {
    let Some(path) = path(args, 1)? else { return Ok(Value::Null) };
    let tree = tree(args, 0)?;
    let Some(at) = jsonb_path(&tree, &path) else { return Ok(Value::Null) };
    Ok(if as_text { text_value(tree.as_text(at)) } else { Value::text(tree.text(at)) })
}

/// `jsonb_extract_path`: `jsonb #> text[]`.
fn jsonb_extract_path(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    jsonb_extract(args, false)
}

/// `jsonb_extract_path_text`: `jsonb #>> text[]`.
fn jsonb_extract_path_text(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    jsonb_extract(args, true)
}

/// `jsonb_typeof`.
fn jsonb_typeof(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::text(tree(args, 0)?.type_name(0)))
}

/// `jsonb_array_length`.
fn jsonb_array_length(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    array_length(&tree(args, 0)?)
}

/// `jsonb_strip_nulls`. A scalar at the root stays as it is.
fn jsonb_strip_nulls(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let tree = tree(args, 0)?;
    if !matches!(tree.item(0), JsonItem::Array(_) | JsonItem::Object(_)) {
        return Ok(Value::text(tree.source()));
    }
    Ok(Value::Text(tree.strip_nulls(strip_arrays(args), true)))
}

/// `jsonb_pretty`.
fn jsonb_pretty(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Text(tree(args, 0)?.pretty()))
}

/// `cannotCastJsonbValue`.
fn cannot_cast(item: &JsonItem, sqltype: &str) -> Error {
    let kind = match item {
        JsonItem::Null => "null",
        JsonItem::String(_) => "string",
        JsonItem::Number => "numeric",
        JsonItem::Bool(_) => "boolean",
        JsonItem::Array(_) => "array",
        JsonItem::Object(_) => "object",
    };
    invalid(&format!("cannot cast jsonb {kind} to type {sqltype}"))
}

/// `JsonbExtractScalar` for a cast: the number at the root as `numeric`, null for `null`, or the error of the cast.
fn jsonb_number(args: &[Value], sqltype: &str) -> Result<Option<Value>> {
    let tree = tree(args, 0)?;
    match tree.item(0) {
        JsonItem::Null => Ok(None),
        JsonItem::Number => {
            Ok(Some(Value::Numeric(numeric_in(tree.text(0), -1).map_err(type_error)?)))
        }
        item => Err(cannot_cast(item, sqltype)),
    }
}

/// `jsonb_bool`.
fn jsonb_bool(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let tree = tree(args, 0)?;
    match tree.item(0) {
        JsonItem::Null => Ok(Value::Null),
        JsonItem::Bool(b) => Ok(Value::Bool(*b)),
        item => Err(cannot_cast(item, "boolean")),
    }
}

/// `jsonb_numeric`.
fn jsonb_numeric(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(jsonb_number(args, "numeric")?.unwrap_or(Value::Null))
}

/// The name of the type of the result of a cast, as the messages give it.
fn sqltype(call: &Call<'_>) -> &'static str {
    match call.ret {
        oid::INT2 => "smallint",
        oid::INT4 => "integer",
        oid::INT8 => "bigint",
        oid::FLOAT4 => "real",
        _ => "double precision",
    }
}

/// `jsonb_int2`, `jsonb_int4` and `jsonb_int8`: the cast of the number with `numeric_int4` and the others.
fn jsonb_int(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    match jsonb_number(args, sqltype(call))? {
        Some(n) => numeric_to_int(call, &[n]),
        None => Ok(Value::Null),
    }
}

/// `jsonb_float4` and `jsonb_float8`.
fn jsonb_float(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    match jsonb_number(args, sqltype(call))? {
        Some(n) => numeric_to_float(call, &[n]),
        None => Ok(Value::Null),
    }
}

/// `jsonb_exists`: `jsonb ? text`.
fn jsonb_exists(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(tree(args, 0)?.exists(text(args, 1)?)))
}

/// `jsonb_exists_any`: `jsonb ?| text[]`. The null keys do not count.
fn jsonb_exists_any(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let tree = tree(args, 0)?;
    Ok(Value::Bool(keys(args, 1)?.iter().any(|k| tree.exists(k))))
}

/// `jsonb_exists_all`: `jsonb ?& text[]`. The null keys do not count.
fn jsonb_exists_all(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let tree = tree(args, 0)?;
    Ok(Value::Bool(keys(args, 1)?.iter().all(|k| tree.exists(k))))
}

/// `jsonb_contains`: `jsonb @> jsonb`.
fn jsonb_contains(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(tree(args, 0)?.contains(&tree(args, 1)?).map_err(type_error)?))
}

/// `jsonb_contained`: `jsonb <@ jsonb`.
fn jsonb_contained(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(tree(args, 1)?.contains(&tree(args, 0)?).map_err(type_error)?))
}

/// The order of the two `jsonb` arguments.
fn order(args: &[Value]) -> Result<Ordering> {
    Ok(tree(args, 0)?.compare(&tree(args, 1)?))
}

fn jsonb_eq(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(order(args)?.is_eq()))
}

fn jsonb_ne(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(order(args)?.is_ne()))
}

fn jsonb_lt(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(order(args)?.is_lt()))
}

fn jsonb_gt(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(order(args)?.is_gt()))
}

fn jsonb_le(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(order(args)?.is_le()))
}

fn jsonb_ge(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bool(order(args)?.is_ge()))
}

/// `jsonb_cmp`: -1, 0 or 1.
fn jsonb_cmp(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Int4(order(args)? as i32))
}

/// The first byte of a `json` text after the white space.
fn first_byte(s: &str) -> Option<u8> {
    s.trim_start_matches([' ', '\t', '\n', '\r']).bytes().next()
}

/// Parses a `json` argument for a set function. The parser of PostgreSQL calls the semantic action of the root before it reads the rest, so an array or an object at the root gives the error of the function before a syntax error after it.
fn json_root<'a>(
    args: &'a [Value],
    escapes: bool,
    root_error: impl Fn(u8) -> Option<Error>,
) -> Result<JsonTree<'a>> {
    let s = text(args, 0)?;
    JsonTree::parse_with(s, escapes)
        .map_err(|error| first_byte(s).and_then(&root_error).unwrap_or_else(|| type_error(error)))
}

/// A row of `json_each` or `jsonb_each`: the key and the value as `json` or as text.
fn each_rows(tree: &JsonTree<'_>, as_text: bool) -> Vec<Vec<Value>> {
    let JsonItem::Object(fields) = tree.item(0) else { return Vec::new() };
    fields
        .iter()
        .map(|(name, at)| {
            let value =
                if as_text { text_value(tree.as_text(*at)) } else { Value::text(tree.text(*at)) };
            vec![Value::text(name), value]
        })
        .collect()
}

/// `json_each` and `json_each_text`. All the fields are in the rows, also when a key is in the object more than once.
fn json_each_worker(args: &[Value], as_text: bool) -> Result<Vec<Vec<Value>>> {
    let array = || invalid("cannot deconstruct an array as an object");
    let tree = json_root(args, true, |b| (b == b'[').then(array))?;
    match tree.item(0) {
        JsonItem::Object(_) => Ok(each_rows(&tree, as_text)),
        JsonItem::Array(_) => Err(array()),
        _ => Err(invalid("cannot deconstruct a scalar")),
    }
}

/// `json_each`.
pub(crate) fn json_each(_: &Call<'_>, args: &[Value]) -> Result<Vec<Vec<Value>>> {
    json_each_worker(args, false)
}

/// `json_each_text`.
pub(crate) fn json_each_text(_: &Call<'_>, args: &[Value]) -> Result<Vec<Vec<Value>>> {
    json_each_worker(args, true)
}

/// `jsonb_each` and `jsonb_each_text`.
fn jsonb_each_worker(args: &[Value], name: &str, as_text: bool) -> Result<Vec<Vec<Value>>> {
    let tree = tree(args, 0)?;
    if !matches!(tree.item(0), JsonItem::Object(_)) {
        return Err(invalid(&format!("cannot call {name} on a non-object")));
    }
    Ok(each_rows(&tree, as_text))
}

/// `jsonb_each`.
pub(crate) fn jsonb_each(_: &Call<'_>, args: &[Value]) -> Result<Vec<Vec<Value>>> {
    jsonb_each_worker(args, "jsonb_each", false)
}

/// `jsonb_each_text`.
pub(crate) fn jsonb_each_text(_: &Call<'_>, args: &[Value]) -> Result<Vec<Vec<Value>>> {
    jsonb_each_worker(args, "jsonb_each_text", true)
}

/// The keys of the object at the root, or the error of `json_object_keys` or `jsonb_object_keys`.
fn object_keys(tree: &JsonTree<'_>, name: &str) -> Result<Vec<Vec<Value>>> {
    match tree.item(0) {
        JsonItem::Object(fields) => Ok(fields.iter().map(|f| vec![Value::text(&f.0)]).collect()),
        JsonItem::Array(_) => Err(invalid(&format!("cannot call {name} on an array"))),
        _ => Err(invalid(&format!("cannot call {name} on a scalar"))),
    }
}

/// `json_object_keys`. A key is in the rows once for each field with it.
pub(crate) fn json_object_keys(_: &Call<'_>, args: &[Value]) -> Result<Vec<Vec<Value>>> {
    let array = || invalid("cannot call json_object_keys on an array");
    object_keys(&json_root(args, true, |b| (b == b'[').then(array))?, "json_object_keys")
}

/// `jsonb_object_keys`.
pub(crate) fn jsonb_object_keys(_: &Call<'_>, args: &[Value]) -> Result<Vec<Vec<Value>>> {
    object_keys(&tree(args, 0)?, "jsonb_object_keys")
}

/// A row for each element of the array at the root, as `json` or as text.
fn element_rows(tree: &JsonTree<'_>, as_text: bool) -> Vec<Vec<Value>> {
    let JsonItem::Array(items) = tree.item(0) else { return Vec::new() };
    items
        .iter()
        .map(|&at| {
            vec![if as_text { text_value(tree.as_text(at)) } else { Value::text(tree.text(at)) }]
        })
        .collect()
}

/// `json_array_elements` and `json_array_elements_text`. Only the text variant decodes the escapes.
fn json_elements_worker(args: &[Value], name: &str, as_text: bool) -> Result<Vec<Vec<Value>>> {
    let object = || invalid(&format!("cannot call {name} on a non-array"));
    let tree = json_root(args, as_text, |b| (b == b'{').then(object))?;
    match tree.item(0) {
        JsonItem::Array(_) => Ok(element_rows(&tree, as_text)),
        JsonItem::Object(_) => Err(object()),
        _ => Err(invalid(&format!("cannot call {name} on a scalar"))),
    }
}

/// `json_array_elements`.
pub(crate) fn json_array_elements(_: &Call<'_>, args: &[Value]) -> Result<Vec<Vec<Value>>> {
    json_elements_worker(args, "json_array_elements", false)
}

/// `json_array_elements_text`.
pub(crate) fn json_array_elements_text(_: &Call<'_>, args: &[Value]) -> Result<Vec<Vec<Value>>> {
    json_elements_worker(args, "json_array_elements_text", true)
}

/// `jsonb_array_elements` and `jsonb_array_elements_text`.
fn jsonb_elements_worker(args: &[Value], as_text: bool) -> Result<Vec<Vec<Value>>> {
    let tree = tree(args, 0)?;
    match tree.item(0) {
        JsonItem::Array(_) => Ok(element_rows(&tree, as_text)),
        JsonItem::Object(_) => Err(invalid("cannot extract elements from an object")),
        _ => Err(invalid("cannot extract elements from a scalar")),
    }
}

/// `jsonb_array_elements`.
pub(crate) fn jsonb_array_elements(_: &Call<'_>, args: &[Value]) -> Result<Vec<Vec<Value>>> {
    jsonb_elements_worker(args, false)
}

/// `jsonb_array_elements_text`.
pub(crate) fn jsonb_array_elements_text(_: &Call<'_>, args: &[Value]) -> Result<Vec<Vec<Value>>> {
    jsonb_elements_worker(args, true)
}
