//! The functions of the arrays in `array_userfuncs.c`, `arrayfuncs.c` and `varlena.c`: `string_to_array`, `array_to_string`, the functions that read the dimensions, and `array_append`, `array_prepend` and `array_cat`.

use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin;
use rupg_types::{Array, ArrayDim, Value};

use crate::io::{base_type, output};
use crate::{Call, Kernel, bad_value};

/// The array of an argument that is not null.
fn array(args: &[Value], at: usize) -> Result<&Array<Value>> {
    match args.get(at) {
        Some(Value::Array(array)) => Ok(array),
        _ => Err(bad_value()),
    }
}

/// An array as a value.
fn value(array: Array<Value>) -> Value {
    Value::Array(Box::new(array))
}

/// The text of an argument, or `None` for null.
fn text(args: &[Value], at: usize) -> Result<Option<&str>> {
    match args.get(at) {
        Some(Value::Null) | None => Ok(None),
        Some(v) => v.as_str().map(Some).ok_or_else(bad_value),
    }
}

/// The number of a dimension in an argument, from 1.
fn dimension(args: &[Value], at: usize) -> Result<i32> {
    match args.get(at) {
        Some(Value::Int4(n)) => Ok(*n),
        _ => Err(bad_value()),
    }
}

/// An `int4` value of a count, which is not larger than the number of elements of an array.
fn int4(n: usize) -> Value {
    Value::Int4(i32::try_from(n).unwrap_or(i32::MAX))
}

/// The parts of `input` between each `separator`, as `split_text` finds them. A null separator gives each character, and an empty separator gives the full string. An empty input gives no parts. A part that is equal to `null` is a null element.
pub(crate) fn split(
    input: &str,
    separator: Option<&str>,
    null: Option<&str>,
) -> Vec<Option<Value>> {
    let part = |s: &str| if Some(s) == null { None } else { Some(Value::text(s)) };
    match separator {
        _ if input.is_empty() => Vec::new(),
        Some("") => vec![part(input)],
        Some(separator) => input.split(separator).map(part).collect(),
        None => input.char_indices().map(|(at, c)| part(&input[at..at + c.len_utf8()])).collect(),
    }
}

/// `text_to_array` and `text_to_array_null`: `string_to_array`. They are not strict, and a null input gives null.
fn string_to_array(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Some(input) = text(args, 0)? else { return Ok(Value::Null) };
    Ok(value(Array::one(split(input, text(args, 1)?, text(args, 2)?))))
}

/// `array_to_text` and `array_to_text_null`: `array_to_string`. Each element that is not null gives the text of its output function, with the separator between two elements. A null element gives the null string, or nothing when the null string is null. The function with the null string is not strict, and a null array or a null separator gives null.
fn array_to_string(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    if matches!(args.first(), Some(Value::Null)) {
        return Ok(Value::Null);
    }
    let Some(separator) = text(args, 1)? else { return Ok(Value::Null) };
    let null = text(args, 2)?;
    let elem = element_type(call.args.first().copied().unwrap_or(0))?;
    let mut out = Vec::new();
    let mut first = true;
    for element in &array(args, 0)?.values {
        let element = element.as_ref().filter(|v| !matches!(v, Value::Null));
        if element.is_none() && null.is_none() {
            continue;
        }
        if !first {
            out.extend_from_slice(separator.as_bytes());
        }
        first = false;
        match element {
            Some(element) => output(elem, element, call.session, &mut out)?,
            None => out.extend_from_slice(null.unwrap_or_default().as_bytes()),
        }
    }
    String::from_utf8(out).map(Value::Text).map_err(|_| bad_value())
}

/// The element type of an array type, or of a domain over an array type.
fn element_type(ty: u32) -> Result<u32> {
    match builtin::type_by_oid(base_type(ty)) {
        Some(row) if row.elem != 0 => Ok(row.elem),
        _ => Err(crate::not_yet(format_args!("an array function on the type {ty}"))),
    }
}

/// The dimension of an array with the number from 1, or `None` for a dimension that the array does not have.
fn dim(array: &Array<Value>, n: i32) -> Option<ArrayDim> {
    let at = usize::try_from(n).ok()?.checked_sub(1)?;
    array.dims.get(at).copied()
}

/// `array_length`.
fn array_length(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let found = dim(array(args, 0)?, dimension(args, 1)?);
    Ok(found.map_or(Value::Null, |d| Value::Int4(d.len)))
}

/// `array_lower`.
fn array_lower(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let found = dim(array(args, 0)?, dimension(args, 1)?);
    Ok(found.map_or(Value::Null, |d| Value::Int4(d.lower)))
}

/// `array_upper`.
fn array_upper(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let found = dim(array(args, 0)?, dimension(args, 1)?);
    Ok(found.map_or(Value::Null, |d| Value::Int4(d.lower + d.len - 1)))
}

/// `array_ndims`: the number of dimensions, or null for an empty array.
fn array_ndims(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let dims = array(args, 0)?.dims.len();
    Ok(if dims == 0 { Value::Null } else { int4(dims) })
}

/// `array_dims`: the bounds of each dimension, as `[1:3][2:4]`, or null for an empty array.
fn array_dims(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let dims = &array(args, 0)?.dims;
    if dims.is_empty() {
        return Ok(Value::Null);
    }
    let text: String =
        dims.iter().map(|d| format!("[{}:{}]", d.lower, d.lower + d.len - 1)).collect();
    Ok(Value::Text(text))
}

/// `array_cardinality`: `cardinality`, the number of elements in all the dimensions.
fn array_cardinality(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(int4(array(args, 0)?.values.len()))
}

/// The array of an argument of `array_append` or `array_prepend`, as `fetch_array_arg_replace_nulls` gives it: a null array is empty. The array must be empty or have one dimension.
fn one_dimension(args: &[Value], at: usize) -> Result<Array<Value>> {
    let array = match args.get(at) {
        Some(Value::Null) => Array::empty(),
        Some(Value::Array(array)) => (**array).clone(),
        _ => return Err(bad_value()),
    };
    if array.dims.len() > 1 {
        return Err(Error::new(
            SqlState::DATA_EXCEPTION,
            "argument must be empty or one-dimensional array",
        ));
    }
    Ok(array)
}

/// The element of an argument, with `None` for null.
fn element(args: &[Value], at: usize) -> Result<Option<Value>> {
    match args.get(at) {
        Some(Value::Null) => Ok(None),
        Some(v) => Ok(Some(v.clone())),
        None => Err(bad_value()),
    }
}

/// The error of a subscript that does not fit in `int4`.
fn out_of_range() -> Error {
    Error::new(SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "integer out of range")
}

/// `array_append`: the array with the element after its last element. It is not strict.
fn array_append(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let mut array = one_dimension(args, 0)?;
    let element = element(args, 1)?;
    match array.dims.first_mut() {
        Some(d) => {
            d.lower.checked_add(d.len).ok_or_else(out_of_range)?;
            d.len += 1;
            array.values.push(element);
        }
        None => array = Array::one(vec![element]),
    }
    Ok(value(array))
}

/// `array_prepend`: the array with the element before its first element. The lower bound of the result is the lower bound of the array. It is not strict.
fn array_prepend(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let element = element(args, 0)?;
    let mut array = one_dimension(args, 1)?;
    match array.dims.first_mut() {
        Some(d) => {
            d.lower.checked_sub(1).ok_or_else(out_of_range)?;
            d.len += 1;
            array.values.insert(0, element);
        }
        None => array = Array::one(vec![element]),
    }
    Ok(value(array))
}

/// The error of `array_cat` for two arrays that do not fit together.
fn incompatible(detail: String) -> Error {
    Error::new(SqlState::ARRAY_SUBSCRIPT_ERROR, "cannot concatenate incompatible arrays")
        .with_detail(detail)
}

/// `array_cat`: the elements of the first array, then the elements of the second array. Two arrays with the same number of dimensions join on the first dimension. An array with one dimension less is one more element of the first dimension of the other array. A null array or an empty array gives the other array. It is not strict.
fn array_cat(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (left, right) = match (args.first(), args.get(1)) {
        (Some(Value::Null), Some(Value::Null)) => return Ok(Value::Null),
        (Some(Value::Null), Some(other)) | (Some(other), Some(Value::Null)) => {
            return Ok(other.clone());
        }
        (Some(Value::Array(left)), Some(Value::Array(right))) => (left, right),
        _ => return Err(bad_value()),
    };
    let (n1, n2) = (left.dims.len(), right.dims.len());
    if n1 == 0 && n2 > 0 {
        return Ok(Value::Array(right.clone()));
    }
    if n2 == 0 {
        return Ok(Value::Array(left.clone()));
    }
    let dims = if n1 == n2 {
        if left.dims[1..] != right.dims[1..] {
            return Err(incompatible(
                "Arrays with differing element dimensions are not compatible for concatenation."
                    .to_string(),
            ));
        }
        let mut dims = left.dims.clone();
        dims[0].len += right.dims[0].len;
        dims
    } else if n1 + 1 == n2 || n1 == n2 + 1 {
        let (outer, inner) = if n1 < n2 { (right, left) } else { (left, right) };
        if outer.dims[1..] != inner.dims[..] {
            return Err(incompatible(
                "Arrays with differing dimensions are not compatible for concatenation."
                    .to_string(),
            ));
        }
        let mut dims = outer.dims.clone();
        dims[0].len += 1;
        dims
    } else {
        return Err(incompatible(format!(
            "Arrays of {n1} and {n2} dimensions are not compatible for concatenation."
        )));
    };
    let mut values = left.values.clone();
    values.extend(right.values.iter().cloned());
    Ok(value(Array { dims, values }))
}

/// The kernel of an array function by its `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    Some(match src {
        "text_to_array" | "text_to_array_null" => string_to_array,
        "array_to_text" | "array_to_text_null" => array_to_string,
        "array_length" => array_length,
        "array_lower" => array_lower,
        "array_upper" => array_upper,
        "array_ndims" => array_ndims,
        "array_dims" => array_dims,
        "array_cardinality" => array_cardinality,
        "array_append" => array_append,
        "array_prepend" => array_prepend,
        "array_cat" => array_cat,
        _ => return None,
    })
}
