//! `contrib/btree_gin`: GIN operator classes for the types that B-tree orders. A port of `btree_gin.c`.

use rupg_common::{Error, Result};
use rupg_func::{Call, Kernel};
use rupg_types::Value;

use crate::support::index_support;

/// The C symbols of `btree_gin` and their kernels.
pub(crate) static SYMBOLS: &[(&str, Kernel)] = &[
    ("gin_btree_consistent", index_support),
    ("gin_compare_prefix_anyenum", index_support),
    ("gin_compare_prefix_bit", index_support),
    ("gin_compare_prefix_bool", index_support),
    ("gin_compare_prefix_bpchar", index_support),
    ("gin_compare_prefix_bytea", index_support),
    ("gin_compare_prefix_char", index_support),
    ("gin_compare_prefix_cidr", index_support),
    ("gin_compare_prefix_date", index_support),
    ("gin_compare_prefix_float4", index_support),
    ("gin_compare_prefix_float8", index_support),
    ("gin_compare_prefix_inet", index_support),
    ("gin_compare_prefix_int2", index_support),
    ("gin_compare_prefix_int4", index_support),
    ("gin_compare_prefix_int8", index_support),
    ("gin_compare_prefix_interval", index_support),
    ("gin_compare_prefix_macaddr", index_support),
    ("gin_compare_prefix_macaddr8", index_support),
    ("gin_compare_prefix_money", index_support),
    ("gin_compare_prefix_name", index_support),
    ("gin_compare_prefix_numeric", index_support),
    ("gin_compare_prefix_oid", index_support),
    ("gin_compare_prefix_text", index_support),
    ("gin_compare_prefix_time", index_support),
    ("gin_compare_prefix_timestamp", index_support),
    ("gin_compare_prefix_timestamptz", index_support),
    ("gin_compare_prefix_timetz", index_support),
    ("gin_compare_prefix_uuid", index_support),
    ("gin_compare_prefix_varbit", index_support),
    ("gin_enum_cmp", gin_enum_cmp),
    ("gin_extract_query_anyenum", index_support),
    ("gin_extract_query_bit", index_support),
    ("gin_extract_query_bool", index_support),
    ("gin_extract_query_bpchar", index_support),
    ("gin_extract_query_bytea", index_support),
    ("gin_extract_query_char", index_support),
    ("gin_extract_query_cidr", index_support),
    ("gin_extract_query_date", index_support),
    ("gin_extract_query_float4", index_support),
    ("gin_extract_query_float8", index_support),
    ("gin_extract_query_inet", index_support),
    ("gin_extract_query_int2", index_support),
    ("gin_extract_query_int4", index_support),
    ("gin_extract_query_int8", index_support),
    ("gin_extract_query_interval", index_support),
    ("gin_extract_query_macaddr", index_support),
    ("gin_extract_query_macaddr8", index_support),
    ("gin_extract_query_money", index_support),
    ("gin_extract_query_name", index_support),
    ("gin_extract_query_numeric", index_support),
    ("gin_extract_query_oid", index_support),
    ("gin_extract_query_text", index_support),
    ("gin_extract_query_time", index_support),
    ("gin_extract_query_timestamp", index_support),
    ("gin_extract_query_timestamptz", index_support),
    ("gin_extract_query_timetz", index_support),
    ("gin_extract_query_uuid", index_support),
    ("gin_extract_query_varbit", index_support),
    ("gin_extract_value_anyenum", index_support),
    ("gin_extract_value_bit", index_support),
    ("gin_extract_value_bool", index_support),
    ("gin_extract_value_bpchar", index_support),
    ("gin_extract_value_bytea", index_support),
    ("gin_extract_value_char", index_support),
    ("gin_extract_value_cidr", index_support),
    ("gin_extract_value_date", index_support),
    ("gin_extract_value_float4", index_support),
    ("gin_extract_value_float8", index_support),
    ("gin_extract_value_inet", index_support),
    ("gin_extract_value_int2", index_support),
    ("gin_extract_value_int4", index_support),
    ("gin_extract_value_int8", index_support),
    ("gin_extract_value_interval", index_support),
    ("gin_extract_value_macaddr", index_support),
    ("gin_extract_value_macaddr8", index_support),
    ("gin_extract_value_money", index_support),
    ("gin_extract_value_name", index_support),
    ("gin_extract_value_numeric", index_support),
    ("gin_extract_value_oid", index_support),
    ("gin_extract_value_text", index_support),
    ("gin_extract_value_time", index_support),
    ("gin_extract_value_timestamp", index_support),
    ("gin_extract_value_timestamptz", index_support),
    ("gin_extract_value_timetz", index_support),
    ("gin_extract_value_uuid", index_support),
    ("gin_extract_value_varbit", index_support),
    ("gin_numeric_cmp", gin_numeric_cmp),
];

/// `enum_cmp`.
const ENUM_CMP: u32 = 3514;
/// `numeric_cmp`.
const NUMERIC_CMP: u32 = 1769;

/// The kernel of the built-in function `func`.
fn builtin(func: u32) -> Result<Kernel> {
    rupg_func::kernel(func)
        .ok_or_else(|| Error::internal(format!("no kernel for the function {func}")))
}

/// `gin_enum_cmp`: `enum_cmp`, where the OID 0 is the leftmost value, before each label.
fn gin_enum_cmp(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let leftmost = |v: &Value| matches!(v, Value::Oid(0));
    match args {
        [a, b] if leftmost(a) => Ok(Value::Int4(if leftmost(b) { 0 } else { -1 })),
        [_, b] if leftmost(b) => Ok(Value::Int4(1)),
        _ => builtin(ENUM_CMP)?(call, args),
    }
}

/// `gin_numeric_cmp`: `numeric_cmp`, where the null pointer is the leftmost value. No SQL value is that pointer, and the function is strict, so a call from SQL is `numeric_cmp`.
fn gin_numeric_cmp(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    builtin(NUMERIC_CMP)?(call, args)
}

#[cfg(test)]
mod tests {
    use rupg_types::{numeric_in, oid};

    use super::*;
    use crate::pg_trgm::test_session::TestSession;

    #[test]
    fn compare() {
        let session = TestSession::default();
        let num = |s: &str| Value::Numeric(numeric_in(s, -1).unwrap());
        let types = [oid::NUMERIC, oid::NUMERIC];
        let cmp = |a, b| session.call(gin_numeric_cmp, &types, oid::INT4, &[a, b]).unwrap();
        assert_eq!(cmp(num("1.5"), num("2")), Value::Int4(-1));
        assert_eq!(cmp(num("2.0"), num("2")), Value::Int4(0));
        let enums = [oid::ANYENUM, oid::ANYENUM];
        let cmp = |a, b| {
            session.call(gin_enum_cmp, &enums, oid::INT4, &[Value::Oid(a), Value::Oid(b)]).unwrap()
        };
        assert_eq!(cmp(0, 0), Value::Int4(0));
        assert_eq!(cmp(0, 16_385), Value::Int4(-1));
        assert_eq!(cmp(16_385, 0), Value::Int4(1));
    }
}
