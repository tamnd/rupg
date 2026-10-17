//! The functions of `json.c` and `jsonb.c` that make a `json` or a `jsonb` value from SQL values: `to_json`, `array_to_json`, `row_to_json`, `json_build_object`, `json_build_array` and `json_object`, and the same functions of `jsonb`.
//!
//! A value goes to the text of `json` as `datum_to_json_internal` writes it, by the category that `json_categorize_type` gives its type. A function of `jsonb` gives that text to `jsonb_in`, which puts the keys of each object in their order and keeps only the last field of each key, as `pushJsonbValue` does. The text of `json` and the value of `jsonb` agree in the other parts: a number that is not finite is a string, and a key is always a string.

use rupg_analyze::JsonItemType;
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin;
use rupg_types::{
    DateFormat, DateOrder, DateStyle, JsonItem, JsonTree, Record, Value, date_out, json_escape,
    json_in, jsonb_in, oid, timestamp_out, timestamptz_out,
};

use crate::deparse::row_names;
use crate::io::{array_of, base_type, is_row_type, output};
use crate::json::invalid;
use crate::text::variadic_values;
use crate::{Call, Kernel, Session, bad_value, type_error};

/// The kernel of a function that makes a `json` or a `jsonb` value, by its `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    Some(match src {
        "to_json" => to_json,
        "to_jsonb" => to_jsonb,
        "array_to_json" => array_to_json,
        "array_to_json_pretty" => array_to_json_pretty,
        "row_to_json" => row_to_json,
        "row_to_json_pretty" => row_to_json_pretty,
        "json_build_object" => json_build_object,
        "jsonb_build_object" => jsonb_build_object,
        "json_build_array" => json_build_array,
        "jsonb_build_array" => jsonb_build_array,
        "json_build_object_noargs" => |_, _| Ok(Value::text("{}")),
        "jsonb_build_object_noargs" => |_, _| Ok(Value::text("{}")),
        "json_build_array_noargs" => |_, _| Ok(Value::text("[]")),
        "jsonb_build_array_noargs" => |_, _| Ok(Value::text("[]")),
        "json_object" => json_object,
        "jsonb_object" => jsonb_object,
        "json_object_two_arg" => json_object_two_arg,
        "jsonb_object_two_arg" => jsonb_object_two_arg,
        _ => return None,
    })
}

/// The `jsonb` value of the text of a `json` value.
fn jsonb(text: &str) -> Result<Value> {
    jsonb_in(text).map(Value::Text).map_err(type_error)
}

/// True when the category of the type is an array or a composite, after which `json_agg` puts a new line between the values.
pub fn json_structured(ty: u32) -> bool {
    let ty = base_type(ty);
    array_of(ty).is_some() || is_row_type(ty) || ty == oid::RECORD_ARRAY || ty == oid::ANYARRAY
}

/// The text of a value, as `datum_to_json_internal` writes it. A key is always a string, and a key of an array, a composite or a `json` type is an error.
pub fn json_value(
    ty: u32,
    value: &Value,
    key: bool,
    session: &dyn Session,
    out: &mut String,
) -> Result<()> {
    if value.is_null() {
        out.push_str("null");
        return Ok(());
    }
    let ty = base_type(ty);
    let composite = matches!(value, Value::Record(_));
    let array = array_of(ty).is_some() || matches!(value, Value::Array(_));
    if key && (composite || array || ty == oid::JSON || ty == oid::JSONB) {
        return Err(invalid("key value must be scalar, not array, composite, or json"));
    }
    match (ty, value) {
        (_, Value::Record(record)) => put_record(ty, record, false, session, out),
        (_, Value::Array(_)) => put_array(ty, value, false, session, out),
        (oid::BOOL, Value::Bool(b)) => {
            out.push_str(match (key, b) {
                (false, true) => "true",
                (false, false) => "false",
                (true, true) => "\"true\"",
                (true, false) => "\"false\"",
            });
            Ok(())
        }
        (oid::INT2 | oid::INT4 | oid::INT8 | oid::FLOAT4 | oid::FLOAT8 | oid::NUMERIC, _) => {
            let text = output_text(ty, value, session)?;
            let number = matches!(text.as_bytes(), [b'0'..=b'9', ..] | [b'-', b'0'..=b'9', ..]);
            if number && !key {
                out.push_str(&text);
            } else {
                out.push('"');
                out.push_str(&text);
                out.push('"');
            }
            Ok(())
        }
        (oid::DATE | oid::TIMESTAMP | oid::TIMESTAMPTZ, _) => {
            let mut buf = Vec::new();
            let format = DateFormat { style: DateStyle::Xsd, order: DateOrder::Mdy };
            match value {
                Value::Date(v) => date_out(*v, format, &mut buf),
                Value::Timestamp(v) => timestamp_out(*v, format, &mut buf).map_err(type_error)?,
                Value::TimestampTz(v) => {
                    timestamptz_out(*v, format, &*session.zone()?, &mut buf).map_err(type_error)?;
                }
                _ => return Err(bad_value()),
            }
            out.push('"');
            out.push_str(&String::from_utf8_lossy(&buf));
            out.push('"');
            Ok(())
        }
        (oid::JSON | oid::JSONB, _) => {
            out.push_str(value.as_str().ok_or_else(bad_value)?);
            Ok(())
        }
        _ => {
            json_escape(&output_text(ty, value, session)?, out);
            Ok(())
        }
    }
}

/// The text output of a value.
fn output_text(ty: u32, value: &Value, session: &dyn Session) -> Result<String> {
    if let Some(s) = value.as_str() {
        return Ok(s.to_owned());
    }
    let mut buf = Vec::new();
    output(ty, value, session, &mut buf)?;
    String::from_utf8(buf).map_err(|_| bad_value())
}

/// `composite_to_json`: an object with a field for each field of the record. The names come from the record, then from its type, then they are `f1`, `f2` and so on.
fn put_record(
    ty: u32,
    record: &Record,
    lines: bool,
    session: &dyn Session,
    out: &mut String,
) -> Result<()> {
    let from_type = if record.names.is_none() { row_names(ty, session) } else { None };
    let names: Option<&[String]> = record.names.as_deref().or(from_type.as_deref());
    out.push('{');
    for (i, (&field_ty, value)) in record.types.iter().zip(&record.values).enumerate() {
        if i > 0 {
            out.push_str(if lines { ",\n " } else { "," });
        }
        match names.and_then(|n| n.get(i)) {
            Some(name) => json_escape(name, out),
            None => json_escape(&format!("f{}", i + 1), out),
        }
        out.push(':');
        json_value(field_ty, value, false, session, out)?;
    }
    out.push('}');
    Ok(())
}

/// `array_to_json_internal`: nested arrays for the dimensions. Only the elements of the first dimension are on new lines when `lines` is true.
fn put_array(
    ty: u32,
    value: &Value,
    lines: bool,
    session: &dyn Session,
    out: &mut String,
) -> Result<()> {
    let Value::Array(array) = value else { return Err(bad_value()) };
    if array.values.is_empty() {
        out.push_str("[]");
        return Ok(());
    }
    let element = array_of(base_type(ty))
        .map(|(e, _)| e)
        .or_else(|| builtin::type_by_oid(base_type(ty)).map(|row| row.elem))
        .unwrap_or(oid::UNKNOWN);
    let lens: Vec<usize> = array.dims.iter().map(|d| usize::try_from(d.len).unwrap_or(0)).collect();
    let mut next = 0;
    put_dim(element, &array.values, &lens, lines, &mut next, session, out)
}

/// `array_dim_to_json`: the elements of one dimension.
fn put_dim(
    element: u32,
    values: &[Option<Value>],
    lens: &[usize],
    lines: bool,
    next: &mut usize,
    session: &dyn Session,
    out: &mut String,
) -> Result<()> {
    let Some((&len, inner)) = lens.split_first() else { return Ok(()) };
    out.push('[');
    for i in 0..len {
        if i > 0 {
            out.push_str(if lines { ",\n " } else { "," });
        }
        if inner.is_empty() {
            let value = values.get(*next).ok_or_else(bad_value)?;
            json_value(element, value.as_ref().unwrap_or(&Value::Null), false, session, out)?;
            *next += 1;
        } else {
            put_dim(element, values, inner, false, next, session, out)?;
        }
    }
    out.push(']');
    Ok(())
}

/// The text of the first argument.
fn value_text(call: &Call<'_>, args: &[Value]) -> Result<String> {
    let ty = call.args.first().copied().ok_or_else(bad_value)?;
    let mut out = String::new();
    json_value(ty, args.first().ok_or_else(bad_value)?, false, call.session, &mut out)?;
    Ok(out)
}

fn to_json(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Text(value_text(call, args)?))
}

fn to_jsonb(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    jsonb(&value_text(call, args)?)
}

/// `array_to_json` with the pretty flag.
fn array_text(call: &Call<'_>, args: &[Value], lines: bool) -> Result<Value> {
    let ty = call.args.first().copied().ok_or_else(bad_value)?;
    let mut out = String::new();
    put_array(ty, args.first().ok_or_else(bad_value)?, lines, call.session, &mut out)?;
    Ok(Value::Text(out))
}

fn array_to_json(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    array_text(call, args, false)
}

fn array_to_json_pretty(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    array_text(call, args, matches!(args.get(1), Some(Value::Bool(true))))
}

/// `row_to_json` with the pretty flag.
fn row_text(call: &Call<'_>, args: &[Value], lines: bool) -> Result<Value> {
    let ty = call.args.first().copied().ok_or_else(bad_value)?;
    let Some(Value::Record(record)) = args.first() else { return Err(bad_value()) };
    let mut out = String::new();
    put_record(base_type(ty), record, lines, call.session, &mut out)?;
    Ok(Value::Text(out))
}

fn row_to_json(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    row_text(call, args, false)
}

fn row_to_json_pretty(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    row_text(call, args, matches!(args.get(1), Some(Value::Bool(true))))
}

/// `json_build_object_worker` and `jsonb_build_object_worker`: the text of the object, or `None` for a null `VARIADIC` array.
fn build_object(call: &Call<'_>, args: &[Value], is_jsonb: bool) -> Result<Option<String>> {
    let Some(values) = variadic_values(call, args, 0)? else { return Ok(None) };
    if values.len() % 2 != 0 {
        let name = if is_jsonb { "jsonb_build_object()" } else { "json_build_object()" };
        return Err(invalid("argument list must have even number of elements").with_hint(format!(
            "The arguments of {name} must consist of alternating keys and values."
        )));
    }
    let mut out = String::from("{");
    for (i, pair) in values.chunks(2).enumerate() {
        let [(key_ty, key), (ty, value)] = pair else { return Err(bad_value()) };
        if i > 0 {
            out.push_str(", ");
        }
        if key.is_null() {
            return Err(if is_jsonb {
                invalid(&format!("argument {}: key must not be null", 2 * i + 1))
            } else {
                Error::new(
                    SqlState::NULL_VALUE_NOT_ALLOWED,
                    "null value not allowed for object key",
                )
            });
        }
        json_value(*key_ty, key, true, call.session, &mut out)?;
        out.push_str(" : ");
        json_value(*ty, value, false, call.session, &mut out)?;
    }
    out.push('}');
    Ok(Some(out))
}

fn json_build_object(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(build_object(call, args, false)?.map_or(Value::Null, Value::Text))
}

fn jsonb_build_object(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    build_object(call, args, true)?.map_or(Ok(Value::Null), |text| jsonb(&text))
}

/// `json_build_array_worker`: the text of the array, or `None` for a null `VARIADIC` array.
fn build_array(call: &Call<'_>, args: &[Value]) -> Result<Option<String>> {
    let Some(values) = variadic_values(call, args, 0)? else { return Ok(None) };
    let mut out = String::from("[");
    for (i, (ty, value)) in values.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        json_value(*ty, value, false, call.session, &mut out)?;
    }
    out.push(']');
    Ok(Some(out))
}

fn json_build_array(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(build_array(call, args)?.map_or(Value::Null, Value::Text))
}

fn jsonb_build_array(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    build_array(call, args)?.map_or(Ok(Value::Null), |text| jsonb(&text))
}

fn subscript_error(message: &str) -> Error {
    Error::new(SqlState::ARRAY_SUBSCRIPT_ERROR, message)
}

/// The lengths of the dimensions of an array and its elements.
type TextArray<'a> = (Vec<i32>, Vec<Option<&'a str>>);

/// The lengths of the dimensions of a `text[]` argument and its elements.
fn text_array(args: &[Value], at: usize) -> Result<TextArray<'_>> {
    let Some(Value::Array(array)) = args.get(at) else { return Err(bad_value()) };
    let values = array
        .values
        .iter()
        .map(|v| v.as_ref().map(|v| v.as_str().ok_or_else(bad_value)).transpose())
        .collect::<Result<_>>()?;
    Ok((array.dims.iter().map(|d| d.len).collect(), values))
}

/// The text of an object with a string or a null for each key.
fn object_text<'a>(
    pairs: impl Iterator<Item = (Option<&'a str>, Option<&'a str>)>,
) -> Result<String> {
    let mut out = String::from("{");
    for (i, (key, value)) in pairs.enumerate() {
        let Some(key) = key else {
            return Err(Error::new(
                SqlState::NULL_VALUE_NOT_ALLOWED,
                "null value not allowed for object key",
            ));
        };
        if i > 0 {
            out.push_str(", ");
        }
        json_escape(key, &mut out);
        out.push_str(" : ");
        match value {
            Some(value) => json_escape(value, &mut out),
            None => out.push_str("null"),
        }
    }
    out.push('}');
    Ok(out)
}

/// `json_object(text[])`: the keys and the values in turn, in one dimension or in two dimensions of two columns.
fn object_one_arg(args: &[Value]) -> Result<String> {
    let (lens, values) = text_array(args, 0)?;
    match lens.len() {
        0 => return Ok("{}".to_owned()),
        1 if lens[0] % 2 != 0 => {
            return Err(subscript_error("array must have even number of elements"));
        }
        1 => {}
        2 if lens[1] != 2 => return Err(subscript_error("array must have two columns")),
        2 => {}
        _ => return Err(subscript_error("wrong number of array subscripts")),
    }
    object_text(values.chunks(2).map(|pair| (pair[0], pair.get(1).copied().flatten())))
}

/// `json_object(text[], text[])`: the keys in one array and the values in the other.
fn object_two_arg(args: &[Value]) -> Result<String> {
    let (key_dims, keys) = text_array(args, 0)?;
    let (value_dims, values) = text_array(args, 1)?;
    let (key_dims, value_dims) = (key_dims.len(), value_dims.len());
    if key_dims > 1 || key_dims != value_dims {
        return Err(subscript_error("wrong number of array subscripts"));
    }
    if key_dims == 0 {
        return Ok("{}".to_owned());
    }
    if keys.len() != values.len() {
        return Err(subscript_error("mismatched array dimensions"));
    }
    object_text(keys.into_iter().zip(values))
}

fn json_object(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Text(object_one_arg(args)?))
}

fn jsonb_object(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    jsonb(&object_one_arg(args)?)
}

fn json_object_two_arg(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Text(object_two_arg(args)?))
}

fn jsonb_object_two_arg(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    jsonb(&object_two_arg(args)?)
}

/// The JSON whitespace that the lexer skips.
const JSON_SPACE: [char; 4] = [' ', '\t', '\n', '\r'];

/// The error of a key that is not unique. A `jsonb` value does not show the key.
fn duplicate_key(key: Option<&str>) -> Error {
    let message = match key {
        Some(key) => format!("duplicate JSON object key value: {key}"),
        None => "duplicate JSON object key value".to_owned(),
    };
    Error::new(SqlState::DUPLICATE_JSON_OBJECT_KEY_VALUE, message)
}

/// `json_build_object_worker` and `jsonb_build_object_worker` for `JSON_OBJECT`: an object of the keys and the values, one after the other.
///
/// A `json` value keeps each field in its order. A `jsonb` value keeps the last field of each key, and then `ABSENT ON NULL` removes each field with a JSON null value, as `uniqueifyJsonbObject` does. A key of a field that `ABSENT ON NULL` skips still counts for `WITH UNIQUE KEYS`.
///
/// # Errors
///
/// The error of a null key, of a key that is not a scalar, or of a key that is not unique.
pub fn json_ctor_object(
    args: &[(u32, Value)],
    jsonb: bool,
    absent_on_null: bool,
    unique: bool,
    session: &dyn Session,
) -> Result<Value> {
    let mut fields: Vec<(String, String)> = Vec::new();
    let mut keys = std::collections::HashSet::new();
    for (i, pair) in args.chunks(2).enumerate() {
        let [(key_ty, key), (ty, value)] = pair else { return Err(bad_value()) };
        let skip = absent_on_null && value.is_null();
        if jsonb && key.is_null() {
            return Err(invalid(&format!("argument {}: key must not be null", 2 * i + 1)));
        }
        if skip && !unique {
            continue;
        }
        if key.is_null() {
            return Err(Error::new(
                SqlState::NULL_VALUE_NOT_ALLOWED,
                "null value not allowed for object key",
            ));
        }
        let mut name = String::new();
        json_value(*key_ty, key, true, session, &mut name)?;
        if unique && !keys.insert(name.clone()) {
            return Err(duplicate_key((!jsonb).then_some(&*name)));
        }
        if skip && !jsonb {
            continue;
        }
        let mut text = String::new();
        json_value(*ty, value, false, session, &mut text)?;
        fields.push((name, text));
    }
    if jsonb {
        // The last field of each key, then the fields with a value that is not a JSON null.
        let mut last: Vec<(String, String)> = Vec::new();
        for (name, text) in fields.into_iter().rev() {
            if !last.iter().any(|(n, _)| *n == name) {
                last.push((name, text));
            }
        }
        last.reverse();
        if absent_on_null {
            last.retain(|(_, text)| text.trim_matches(JSON_SPACE) != "null");
        }
        fields = last;
    }
    let mut out = String::from("{");
    for (i, (name, text)) in fields.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(name);
        out.push_str(" : ");
        out.push_str(text);
    }
    out.push('}');
    if jsonb { self::jsonb(&out) } else { Ok(Value::Text(out)) }
}

/// `json_build_array_worker` and `jsonb_build_array_worker` for `JSON_ARRAY`: an array of the values. `ABSENT ON NULL` skips the null values.
///
/// # Errors
///
/// The error of the output of a value.
pub fn json_ctor_array(
    args: &[(u32, Value)],
    jsonb: bool,
    absent_on_null: bool,
    session: &dyn Session,
) -> Result<Value> {
    let mut out = String::from("[");
    let mut first = true;
    for (ty, value) in args {
        if absent_on_null && value.is_null() {
            continue;
        }
        if !first {
            out.push_str(", ");
        }
        first = false;
        json_value(*ty, value, false, session, &mut out)?;
    }
    out.push(']');
    if jsonb { self::jsonb(&out) } else { Ok(Value::Text(out)) }
}

/// `datum_to_json` and `datum_to_jsonb` for `JSON_SCALAR`: the JSON value of a SQL value, or null for null.
///
/// # Errors
///
/// The error of the output of the value.
pub fn json_ctor_scalar(
    ty: u32,
    value: &Value,
    jsonb: bool,
    session: &dyn Session,
) -> Result<Value> {
    if value.is_null() {
        return Ok(Value::Null);
    }
    let mut out = String::new();
    json_value(ty, value, false, session, &mut out)?;
    if jsonb { self::jsonb(&out) } else { Ok(Value::Text(out)) }
}

/// True when no object of the parsed value has a key two times.
fn unique_keys(tree: JsonTree<'_>) -> bool {
    tree.into_nodes().iter().all(|node| match &node.item {
        JsonItem::Object(fields) => {
            let mut keys = std::collections::HashSet::new();
            fields.iter().all(|(key, _)| keys.insert(key.as_str()))
        }
        _ => true,
    })
}

/// `JSON` with `WITH UNIQUE KEYS`: the `json` or `jsonb` value of a text that has no object with a key two times, as `json_validate` and `jsonb_from_text` check it.
///
/// # Errors
///
/// The error of the input of the type, or of a key that is not unique.
pub fn json_ctor_parse(text: &str, jsonb: bool) -> Result<Value> {
    let tree = JsonTree::parse(text).map_err(type_error)?;
    if !unique_keys(tree) {
        return Err(duplicate_key(None));
    }
    if jsonb { self::jsonb(text) } else { Ok(Value::text(text)) }
}

/// `ExecEvalJsonIsPredicate`: true when the value is JSON of the kind of the predicate. `base_type` is `text`, `json` or `jsonb`. The first token of a `text` or a `json` value gives its kind, and only a `text` value or a check of the keys parses all of the value.
pub fn is_json(value: &str, base_type: u32, item: JsonItemType, unique: bool) -> bool {
    let first = value.trim_start_matches(JSON_SPACE).chars().next();
    let kind = match first {
        Some('{') => JsonItemType::Object,
        Some('[') => JsonItemType::Array,
        Some(_) => JsonItemType::Scalar,
        None => return false,
    };
    if base_type == oid::JSONB {
        return item == JsonItemType::Any || item == kind;
    }
    if item != JsonItemType::Any && item != kind {
        return false;
    }
    if unique {
        return JsonTree::parse(value).is_ok_and(unique_keys);
    }
    base_type != oid::TEXT || json_in(value).is_ok()
}
