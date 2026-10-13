//! The functions of `tsquery` in `tsquery_op.c` and `tsquery_cleanup.c` that need no text search configuration: `numnode`, `querytree`, `tsquery_phrase`, the operators `&&`, `||`, `!!`, `<->`, `@>` and `<@`, and `tsquery_cmp`.

use rupg_common::{Error, Result, SqlState};
use rupg_types::{
    QueryItem, TSQUERY_AND, TSQUERY_MAX_DISTANCE, TSQUERY_NOT, TSQUERY_OR, TSQUERY_PHRASE, Value,
};

use crate::{Call, Kernel, bad_value, type_error};

/// The items of a `tsquery` argument.
fn items(value: &Value) -> Result<&[QueryItem]> {
    match value {
        Value::TsQuery(items) => Ok(items),
        _ => Err(bad_value()),
    }
}

/// `tsquery_numnode`: the number of operands and operators.
fn numnode(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [query] = args else { return Err(bad_value()) };
    Ok(Value::Int4(i32::try_from(items(query)?.len()).unwrap_or(i32::MAX)))
}

/// `tsquerytree`: the part of the query that an index can use.
fn querytree(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [query] = args else { return Err(bad_value()) };
    Ok(Value::Text(rupg_types::tsquery_tree(items(query)?)))
}

/// The query `a op b`. When one query is empty, the result is the other query.
fn join(op: u8, distance: u16, a: &Value, b: &Value) -> Result<Value> {
    let (a, b) = (items(a)?, items(b)?);
    if a.is_empty() {
        return Ok(Value::TsQuery(b.to_vec()));
    }
    if b.is_empty() {
        return Ok(Value::TsQuery(a.to_vec()));
    }
    Ok(Value::TsQuery(rupg_types::tsquery_join(op, distance, a, b).map_err(type_error)?))
}

/// `tsquery_and`: the operator `&&`.
fn and(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [a, b] = args else { return Err(bad_value()) };
    join(TSQUERY_AND, 0, a, b)
}

/// `tsquery_or`: the operator `||`.
fn or(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [a, b] = args else { return Err(bad_value()) };
    join(TSQUERY_OR, 0, a, b)
}

/// `tsquery_phrase`: the operator `<->`.
fn phrase(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [a, b] = args else { return Err(bad_value()) };
    join(TSQUERY_PHRASE, 1, a, b)
}

/// `tsquery_phrase_distance`: `tsquery_phrase` with a distance, which must be in range even when a query is empty.
fn phrase_distance(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [a, b, Value::Int4(distance)] = args else { return Err(bad_value()) };
    let distance = u16::try_from(*distance)
        .ok()
        .filter(|d| *d <= TSQUERY_MAX_DISTANCE)
        .ok_or_else(|| type_error(rupg_types::tsquery_bad_distance()))?;
    join(TSQUERY_PHRASE, distance, a, b)
}

/// `tsquery_not`: the operator `!!`. The query stays empty when it is empty.
fn not(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [a] = args else { return Err(bad_value()) };
    let a = items(a)?;
    if a.is_empty() {
        return Ok(Value::TsQuery(Vec::new()));
    }
    Ok(Value::TsQuery(rupg_types::tsquery_join(TSQUERY_NOT, 0, &[], a).map_err(type_error)?))
}

/// `tsq_mcontains`: the operator `@>`.
fn contains(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [query, ex] = args else { return Err(bad_value()) };
    Ok(Value::Bool(rupg_types::tsquery_contains(items(query)?, items(ex)?)))
}

/// `tsq_mcontained`: the operator `<@`.
fn contained(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [ex, query] = args else { return Err(bad_value()) };
    Ok(Value::Bool(rupg_types::tsquery_contains(items(query)?, items(ex)?)))
}

/// `tsquery_cmp`: the order of the B-tree operator class.
fn cmp(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [a, b] = args else { return Err(bad_value()) };
    Ok(Value::Int4(rupg_types::tsquery_cmp(items(a)?, items(b)?)))
}

/// The notice of `tsqueryin` for a query with no lexemes, which the session sends as a `NOTICE` because its SQLSTATE is in class `00`.
pub(crate) fn no_lexemes(text: &str) -> Error {
    Error::new(
        SqlState::SUCCESSFUL_COMPLETION,
        format!("text-search query doesn't contain lexemes: \"{text}\""),
    )
}

/// The kernel of a `tsquery` function by its `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    Some(match src {
        "tsquery_numnode" => numnode,
        "tsquerytree" => querytree,
        "tsquery_and" => and,
        "tsquery_or" => or,
        "tsquery_phrase" => phrase,
        "tsquery_phrase_distance" => phrase_distance,
        "tsquery_not" => not,
        "tsq_mcontains" => contains,
        "tsq_mcontained" => contained,
        "tsquery_cmp" => cmp,
        _ => return None,
    })
}
