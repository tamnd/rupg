//! The functions of the arrays in `array_userfuncs.c`, `arrayfuncs.c` and `varlena.c`: `string_to_array`, `array_to_string`, the functions that read the dimensions, `array_append`, `array_prepend` and `array_cat`, `array_remove` and `array_replace`, `array_position` and `array_positions`, `array_fill`, `array_reverse`, `array_shuffle`, `array_sample`, `array_sort`, `trim_array`, the operators `@>`, `<@` and `&&`, and `width_bucket` with an array of thresholds.

use std::borrow::Cow;

use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin;
use rupg_types::{AnyArray, Array, ArrayDim, MAX_ARRAY_SIZE, MAXDIM, Value, array_in, format_type};

use crate::io::{base_type, output};
use crate::{Call, Kernel, bad_value};

/// The array of an argument that is not null.
fn array(args: &[Value], at: usize) -> Result<&Array<Value>> {
    match args.get(at) {
        Some(Value::Array(array)) => Ok(array),
        _ => Err(bad_value()),
    }
}

/// The array of an argument that a function only reads. An `anyarray` value of `pg_statistic` keeps the text of `array_out`, and it gives the array of the text of its elements, as the output function of the element type wrote them.
fn read(args: &[Value], at: usize) -> Result<Cow<'_, Array<Value>>> {
    match args.get(at) {
        Some(Value::AnyArray(any)) => {
            let element = |e: &str| Ok(Value::Text(e.to_owned()));
            let delim = builtin::type_by_oid(base_type(any.elem)).map_or(b',', |t| t.delim);
            let array = array_in(&any.text, delim, true, element).map_err(|_| bad_value())?;
            Ok(Cow::Owned(array))
        }
        _ => array(args, at).map(Cow::Borrowed),
    }
}

/// A function that gives an array of the type of its first argument, called on the text of an `anyarray` value. It works on the array of the text of the elements, and it gives the text of `array_out` of the result, as `enforce_generic_type_consistency` gives `anyarray` as the result type.
fn on_anyarray(call: &Call<'_>, args: &[Value], kernel: Kernel) -> Result<Value> {
    let Some(Value::AnyArray(any)) = args.first() else {
        return kernel(call, args);
    };
    let elem = any.elem;
    let mut inner_args = args.to_vec();
    inner_args[0] = value(read(args, 0)?.into_owned());
    let mut types = call.args.to_vec();
    if let Some(first) = types.first_mut() {
        *first = rupg_types::oid::TEXT_ARRAY;
    }
    let inner = Call {
        session: call.session,
        args: &types,
        ret: rupg_types::oid::TEXT_ARRAY,
        variadic: call.variadic,
        want: call.want,
    };
    let result = kernel(&inner, &inner_args)?;
    if matches!(result, Value::Null) {
        return Ok(result);
    }
    let mut out = Vec::new();
    output(rupg_types::oid::TEXT_ARRAY, &result, call.session, &mut out)?;
    let text = String::from_utf8(out).map_err(|_| bad_value())?;
    Ok(Value::AnyArray(Box::new(AnyArray { elem, text })))
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
    for element in &read(args, 0)?.values {
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

/// The element type of an array type, or of a domain over an array type. The elements of an `anyarray` value are its text, as `read` gives them.
fn element_type(ty: u32) -> Result<u32> {
    if ty == rupg_types::oid::ANYARRAY {
        return Ok(rupg_types::oid::TEXT);
    }
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
    let found = dim(&*read(args, 0)?, dimension(args, 1)?);
    Ok(found.map_or(Value::Null, |d| Value::Int4(d.len)))
}

/// `array_lower`.
fn array_lower(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let found = dim(&*read(args, 0)?, dimension(args, 1)?);
    Ok(found.map_or(Value::Null, |d| Value::Int4(d.lower)))
}

/// `array_upper`.
fn array_upper(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let found = dim(&*read(args, 0)?, dimension(args, 1)?);
    Ok(found.map_or(Value::Null, |d| Value::Int4(d.lower + d.len - 1)))
}

/// `array_ndims`: the number of dimensions, or null for an empty array.
fn array_ndims(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let dims = read(args, 0)?.dims.len();
    Ok(if dims == 0 { Value::Null } else { int4(dims) })
}

/// `array_dims`: the bounds of each dimension, as `[1:3][2:4]`, or null for an empty array.
fn array_dims(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let array = read(args, 0)?;
    let dims = &array.dims;
    if dims.is_empty() {
        return Ok(Value::Null);
    }
    let text: String =
        dims.iter().map(|d| format!("[{}:{}]", d.lower, d.lower + d.len - 1)).collect();
    Ok(Value::Text(text))
}

/// `array_cardinality`: `cardinality`, the number of elements in all the dimensions.
fn array_cardinality(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(int4(read(args, 0)?.values.len()))
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

/// `array_replace_internal`: the array with each element that is equal to the search value replaced, or removed when `replace` is `None`. A null search value finds the null elements. The array is the same when no element is equal. A removal needs an array with one dimension or less, and gives an empty array when no element stays. The lower bound does not change. It is not strict, and a null array gives null.
fn replace_elements(
    call: &Call<'_>,
    args: &[Value],
    replace: Option<Option<Value>>,
) -> Result<Value> {
    let array = match args.first() {
        Some(Value::Null) => return Ok(Value::Null),
        Some(Value::Array(array)) => array,
        _ => return Err(bad_value()),
    };
    if replace.is_none() && array.dims.len() > 1 {
        return Err(Error::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "removing elements from multidimensional arrays is not supported",
        ));
    }
    let search = element(args, 1)?;
    let types = (
        element_type(call.args.first().copied().unwrap_or(0))?,
        call.args.get(1).copied().unwrap_or(0),
    );
    let mut values = Vec::with_capacity(array.values.len());
    let mut changed = false;
    for item in &array.values {
        let same = match (item, &search) {
            (None, None) => true,
            (Some(a), Some(b)) => crate::compare::equal(types.0, a, types.1, b)?,
            _ => false,
        };
        if !same {
            values.push(item.clone());
            continue;
        }
        changed = true;
        if let Some(with) = &replace {
            values.push(with.clone());
        }
    }
    if !changed {
        return Ok(Value::Array(array.clone()));
    }
    let mut dims = array.dims.clone();
    if replace.is_none() {
        if values.is_empty() {
            return Ok(value(Array::empty()));
        }
        if let Some(d) = dims.first_mut() {
            d.len = i32::try_from(values.len()).map_err(|_| out_of_range())?;
        }
    }
    Ok(value(Array { dims, values }))
}

/// `array_remove(anycompatiblearray, anycompatible)`.
fn array_remove(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    replace_elements(call, args, None)
}

/// `array_replace(anycompatiblearray, anycompatible, anycompatible)`.
fn array_replace(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    replace_elements(call, args, Some(element(args, 2)?))
}

/// The error of a search in an array with more than one dimension.
fn multidimensional_search() -> Error {
    Error::new(
        SqlState::FEATURE_NOT_SUPPORTED,
        "searching for elements in multidimensional arrays is not supported",
    )
}

/// The array of `array_position` and `array_positions`, or `None` for a null array.
fn search_array(args: &[Value]) -> Result<Option<&Array<Value>>> {
    let array = match args.first() {
        Some(Value::Null) => return Ok(None),
        Some(Value::Array(array)) => array,
        _ => return Err(bad_value()),
    };
    if array.dims.len() > 1 {
        return Err(multidimensional_search());
    }
    Ok(Some(array))
}

/// The subscripts of the elements from `from` that are not distinct from `search`: all of them, or only the first one.
fn find(
    call: &Call<'_>,
    array: &Array<Value>,
    search: Option<&Value>,
    from: i32,
    all: bool,
) -> Result<Vec<Option<Value>>> {
    let types = (
        call.args.get(1).copied().unwrap_or(0),
        element_type(call.args.first().copied().unwrap_or(0))?,
    );
    let lower = array.dims.first().map_or(1, |d| d.lower);
    let mut found = Vec::new();
    for (position, item) in (lower..).zip(&array.values) {
        if position < from {
            continue;
        }
        let hit = match (search, item) {
            (None, None) => true,
            (Some(a), Some(b)) => crate::compare::equal(types.0, a, types.1, b)?,
            _ => false,
        };
        if hit {
            found.push(Some(Value::Int4(position)));
            if !all {
                break;
            }
        }
    }
    Ok(found)
}

/// `array_position` and `array_position_start`: the subscript of the first element that is not distinct from the search value, or null. It is not strict.
fn array_position(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Some(array) = search_array(args)? else { return Ok(Value::Null) };
    let search = element(args, 1)?;
    if array.dims.is_empty() || (search.is_none() && array.values.iter().all(Option::is_some)) {
        return Ok(Value::Null);
    }
    let from = match args.get(2) {
        None => array.dims[0].lower,
        Some(Value::Int4(n)) => *n,
        Some(Value::Null) => {
            return Err(Error::new(
                SqlState::NULL_VALUE_NOT_ALLOWED,
                "initial position must not be null",
            ));
        }
        Some(_) => return Err(bad_value()),
    };
    Ok(find(call, array, search.as_ref(), from, false)?.pop().flatten().unwrap_or(Value::Null))
}

/// `array_positions`: the subscripts of the elements that are not distinct from the search value, or an empty array. It is not strict.
fn array_positions(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Some(array) = search_array(args)? else { return Ok(Value::Null) };
    let search = element(args, 1)?;
    if array.dims.is_empty() || (search.is_none() && array.values.iter().all(Option::is_some)) {
        return Ok(value(Array::empty()));
    }
    Ok(value(Array::one(find(call, array, search.as_ref(), i32::MIN, true)?)))
}

/// The values of an `int4[]` argument of `array_fill`, with its errors.
fn fill_dims(array: &Array<Value>) -> Result<Vec<i32>> {
    if array.dims.len() > 1 {
        return Err(Error::new(
            SqlState::ARRAY_SUBSCRIPT_ERROR,
            "wrong number of array subscripts",
        )
        .with_detail("Dimension array must be one dimensional."));
    }
    array
        .values
        .iter()
        .map(|v| match v {
            Some(Value::Int4(n)) => Ok(*n),
            None => {
                Err(Error::new(SqlState::NULL_VALUE_NOT_ALLOWED, "dimension values cannot be null"))
            }
            Some(_) => Err(bad_value()),
        })
        .collect()
}

/// `array_fill` and `array_fill_with_lower_bounds`: an array of the given dimensions with each element the value. It is not strict, so the value can be null.
fn array_fill(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let null_dims = || {
        Error::new(
            SqlState::NULL_VALUE_NOT_ALLOWED,
            "dimension array or low bound array cannot be null",
        )
    };
    let arg = |at: usize| match args.get(at) {
        Some(Value::Array(array)) => Ok(Some(&**array)),
        Some(Value::Null) => Err(null_dims()),
        None => Ok(None),
        Some(_) => Err(bad_value()),
    };
    let (dims, lbs) = (arg(1)?.ok_or_else(bad_value)?, arg(2)?);
    let lens = fill_dims(dims)?;
    if lens.len() > MAXDIM {
        return Err(Error::new(
            SqlState::PROGRAM_LIMIT_EXCEEDED,
            format!(
                "number of array dimensions ({}) exceeds the maximum allowed ({MAXDIM})",
                lens.len()
            ),
        ));
    }
    let lowers = match lbs {
        Some(lbs) => {
            let lowers = fill_dims(lbs)?;
            if lowers.len() != lens.len() {
                return Err(Error::new(
                    SqlState::ARRAY_SUBSCRIPT_ERROR,
                    "wrong number of array subscripts",
                )
                .with_detail("Low bound array has different size than dimensions array."));
            }
            lowers
        }
        None => vec![1; lens.len()],
    };
    let too_large = || {
        Error::new(
            SqlState::PROGRAM_LIMIT_EXCEEDED,
            format!("array size exceeds the maximum allowed ({MAX_ARRAY_SIZE})"),
        )
    };
    let mut count: i32 = if lens.is_empty() { 0 } else { 1 };
    for &len in &lens {
        if len < 0 {
            return Err(too_large());
        }
        count = count.checked_mul(len).ok_or_else(too_large)?;
    }
    if count as usize > MAX_ARRAY_SIZE {
        return Err(too_large());
    }
    for (&len, &lower) in lens.iter().zip(&lowers) {
        if len.checked_add(lower).is_none() {
            return Err(Error::new(
                SqlState::PROGRAM_LIMIT_EXCEEDED,
                format!("array lower bound is too large: {lower}"),
            ));
        }
    }
    if count == 0 {
        return Ok(value(Array::empty()));
    }
    let item = element(args, 0)?;
    let dims = lens.iter().zip(&lowers).map(|(&len, &lower)| ArrayDim { len, lower }).collect();
    Ok(value(Array { dims, values: vec![item; count as usize] }))
}

/// The number of elements in each index of the first dimension.
fn item_len(array: &Array<Value>) -> usize {
    match array.dims.first() {
        Some(d) if d.len > 0 => array.values.len() / d.len as usize,
        _ => 0,
    }
}

/// `array_reverse`: the indexes of the first dimension in the reverse order.
fn array_reverse(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let array = array(args, 0)?;
    let step = item_len(array);
    if step == 0 {
        return Ok(value(array.clone()));
    }
    let values = array.values.rchunks(step).flatten().cloned().collect();
    Ok(value(Array { dims: array.dims.clone(), values }))
}

/// `array_shuffle_n`: `n` indexes of the first dimension in a random order, by the algorithm of Fisher and Yates with `pg_global_prng_state`. The lower bound of the first dimension stays when `keep_lower` is true, and is 1 if not.
fn shuffle(call: &Call<'_>, array: &Array<Value>, n: usize, keep_lower: bool) -> Value {
    let step = item_len(array);
    if step == 0 || n == 0 {
        return value(Array::empty());
    }
    let nitem = array.values.len() / step;
    let mut values = array.values.clone();
    for i in 0..n {
        let j = crate::random::global_range(call.session, i as u64, nitem as u64 - 1) as usize;
        for k in 0..step {
            values.swap(i * step + k, j * step + k);
        }
    }
    values.truncate(n * step);
    let mut dims = array.dims.clone();
    dims[0].len = n as i32;
    if !keep_lower {
        dims[0].lower = 1;
    }
    value(Array { dims, values })
}

/// `array_shuffle`: the indexes of the first dimension in a random order.
fn array_shuffle(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let array = array(args, 0)?;
    match array.dims.first() {
        Some(d) if d.len >= 2 => Ok(shuffle(call, array, d.len as usize, true)),
        _ => Ok(value(array.clone())),
    }
}

/// `array_sample`: `n` indexes of the first dimension chosen at random, with the lower bound 1.
fn array_sample(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let array = array(args, 0)?;
    let n = dimension(args, 1)?;
    let nitem = array.dims.first().map_or(0, |d| d.len);
    if n < 0 || n > nitem {
        return Err(Error::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("sample size must be between 0 and {nitem}"),
        ));
    }
    Ok(shuffle(call, array, n as usize, false))
}

/// `array_sort`, `array_sort_order` and `array_sort_order_nulls_first`: the indexes of the first dimension in order. The indexes of an array with more than one dimension are arrays, which sort as `array_lt` orders them. The lower bounds stay.
fn array_sort(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let array = array(args, 0)?;
    let flag = |at: usize| match args.get(at) {
        Some(Value::Bool(b)) => Ok(Some(*b)),
        None => Ok(None),
        Some(_) => Err(bad_value()),
    };
    let descending = flag(1)?.unwrap_or(false);
    let nulls_first = flag(2)?.unwrap_or(descending);
    let step = item_len(array);
    if array.dims.first().is_none_or(|d| d.len < 2) {
        return Ok(value(array.clone()));
    }
    let ty = call.args.first().copied().unwrap_or(0);
    if let Ok(element) = element_type(ty)
        && !has_order(element)
    {
        // `array_sort_internal` fails for the elements, and `array_lt` fails for the sub-arrays.
        let code = if array.dims.len() == 1 {
            SqlState::FEATURE_NOT_SUPPORTED
        } else {
            SqlState::UNDEFINED_FUNCTION
        };
        return Err(Error::new(
            code,
            format!("could not identify a comparison function for type {}", format_type(element)),
        ));
    }
    let mut items: Vec<Option<Value>> = if array.dims.len() == 1 {
        array.values.clone()
    } else {
        let sub = array.dims[1..].to_vec();
        array
            .values
            .chunks(step)
            .map(|chunk| Some(value(Array { dims: sub.clone(), values: chunk.to_vec() })))
            .collect()
    };
    let elem = if array.dims.len() == 1 { element_type(ty)? } else { ty };
    let mut failed = None;
    items.sort_by(|a, b| match (a, b) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) if nulls_first => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (Some(_), None) if nulls_first => std::cmp::Ordering::Greater,
        (Some(_), None) => std::cmp::Ordering::Less,
        (Some(x), Some(y)) => match crate::compare::compare(elem, x, elem, y) {
            Ok(order) if descending => order.reverse(),
            Ok(order) => order,
            Err(e) => {
                failed.get_or_insert(e);
                std::cmp::Ordering::Equal
            }
        },
    });
    if let Some(e) = failed {
        return Err(e);
    }
    let values = if array.dims.len() == 1 {
        items
    } else {
        items
            .into_iter()
            .flat_map(|item| match item {
                Some(Value::Array(sub)) => sub.values,
                _ => Vec::new(),
            })
            .collect()
    };
    Ok(value(Array { dims: array.dims.clone(), values }))
}

/// False for a built-in base type that has no `<` operator for `lookup_type_cache` to find: no btree class takes the type or a type that it has a binary cast to, as for `xid` and `point`.
fn has_order(ty: u32) -> bool {
    const BTREE: u32 = 403;
    let ty = base_type(ty);
    let Some(row) = builtin::type_by_oid(ty) else { return true };
    if row.kind != b'b' || row.category == b'A' {
        return true;
    }
    builtin::opclasses().iter().filter(|c| c.method == BTREE).any(|c| {
        c.intype == ty || builtin::cast(ty, c.intype).is_some_and(|cast| cast.method == b'b')
    })
}

/// `trim_array`: the array without the last `n` indexes of the first dimension. The result is a slice, so each lower bound is 1.
fn trim_array(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let array = array(args, 0)?;
    let n = dimension(args, 1)?;
    let len = array.dims.first().map_or(0, |d| d.len);
    if n < 0 || n > len {
        return Err(Error::new(
            SqlState::ARRAY_ELEMENT_ERROR,
            format!("number of elements to trim must be between 0 and {len}"),
        ));
    }
    if n == len {
        return Ok(value(Array::empty()));
    }
    let step = item_len(array);
    let mut dims: Vec<ArrayDim> =
        array.dims.iter().map(|d| ArrayDim { len: d.len, lower: 1 }).collect();
    dims[0].len = len - n;
    let values = array.values[..(len - n) as usize * step].to_vec();
    Ok(value(Array { dims, values }))
}

/// `array_contain_compare`: true when each element of `a` that is not null equals an element of `b`, when `all` is true, or when some element of `a` equals an element of `b`, when `all` is false. A null element of `a` makes `all` false.
fn contain(a: (u32, &Array<Value>), b: (u32, &Array<Value>), all: bool) -> Result<bool> {
    let (ta, tb) = (element_type(a.0)?, element_type(b.0)?);
    if base_type(ta) != base_type(tb) {
        return Err(Error::new(
            SqlState::DATATYPE_MISMATCH,
            "cannot compare arrays of different element types",
        ));
    }
    for x in &a.1.values {
        let Some(x) = x else {
            if all {
                return Ok(false);
            }
            continue;
        };
        let mut hit = false;
        for y in b.1.values.iter().flatten() {
            if crate::compare::equal(ta, x, tb, y)? {
                hit = true;
                break;
            }
        }
        if hit != all {
            return Ok(hit);
        }
    }
    Ok(all)
}

/// The two arrays of `arraycontains`, `arraycontained` and `arrayoverlap`, with their types.
fn two_arrays<'v>(call: &Call<'_>, args: &'v [Value]) -> Result<[(u32, &'v Array<Value>); 2]> {
    let ty = |at: usize| call.args.get(at).copied().unwrap_or(0);
    Ok([(ty(0), array(args, 0)?), (ty(1), array(args, 1)?)])
}

/// `arraycontains`: `@>`.
fn arraycontains(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [a, b] = two_arrays(call, args)?;
    Ok(Value::Bool(contain(b, a, true)?))
}

/// `arraycontained`: `<@`.
fn arraycontained(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [a, b] = two_arrays(call, args)?;
    Ok(Value::Bool(contain(a, b, true)?))
}

/// `arrayoverlap`: `&&`.
fn arrayoverlap(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [a, b] = two_arrays(call, args)?;
    Ok(Value::Bool(contain(a, b, false)?))
}

/// `width_bucket_array`: the number of thresholds that are less than or equal to the operand, by a binary search. The thresholds must be in order.
fn width_bucket_array(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [operand, _] = args else { return Err(bad_value()) };
    let thresholds = array(args, 1)?;
    if thresholds.dims.len() > 1 {
        return Err(Error::new(
            SqlState::ARRAY_SUBSCRIPT_ERROR,
            "thresholds must be one-dimensional array",
        ));
    }
    if thresholds.values.iter().any(Option::is_none) {
        return Err(Error::new(
            SqlState::NULL_VALUE_NOT_ALLOWED,
            "thresholds array must not contain NULLs",
        ));
    }
    let left_type = call.args.first().copied().unwrap_or(0);
    let right_type = element_type(call.args.get(1).copied().unwrap_or(0))?;
    let (mut left, mut right) = (0, thresholds.values.len());
    while left < right {
        let mid = (left + right) / 2;
        let Some(threshold) = &thresholds.values[mid] else { return Err(bad_value()) };
        if crate::compare::compare(left_type, operand, right_type, threshold)?.is_lt() {
            right = mid;
        } else {
            left = mid + 1;
        }
    }
    Ok(int4(left))
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
        "array_remove" => array_remove,
        "array_replace" => array_replace,
        "array_position" | "array_position_start" => array_position,
        "array_positions" => array_positions,
        "array_fill" | "array_fill_with_lower_bounds" => array_fill,
        "array_reverse" => |call, args| on_anyarray(call, args, array_reverse),
        "array_shuffle" => |call, args| on_anyarray(call, args, array_shuffle),
        "array_sample" => |call, args| on_anyarray(call, args, array_sample),
        "array_sort" | "array_sort_order" | "array_sort_order_nulls_first" => array_sort,
        "trim_array" => |call, args| on_anyarray(call, args, trim_array),
        "arraycontains" => arraycontains,
        "arraycontained" => arraycontained,
        "arrayoverlap" => arrayoverlap,
        "width_bucket_array" => width_bucket_array,
        _ => return None,
    })
}
