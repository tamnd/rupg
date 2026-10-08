use rupg_common::{Result, SqlState};
use rupg_pgcatalog::builtin;
use rupg_types::{
    Array, ByteaOutput, DateFormat, FixedZone, IntervalStyle, Numeric, TimeZone, USECS_PER_DAY,
    Value, oid,
};

use crate::{Call, Session, kernel, output};

struct TestSession {
    zone: FixedZone,
}

impl TestSession {
    fn new() -> TestSession {
        TestSession { zone: FixedZone::utc() }
    }
}

impl Session for TestSession {
    fn date_format(&self) -> DateFormat {
        DateFormat::ISO_MDY
    }
    fn interval_style(&self) -> IntervalStyle {
        IntervalStyle::Postgres
    }
    fn zone(&self) -> Result<&dyn TimeZone> {
        Ok(&self.zone)
    }
    fn extra_float_digits(&self) -> i32 {
        1
    }
    fn bytea_output(&self) -> ByteaOutput {
        ByteaOutput::Hex
    }
    fn array_nulls(&self) -> bool {
        true
    }
    fn transaction_start(&self) -> i64 {
        10 * USECS_PER_DAY
    }
    fn statement_start(&self) -> i64 {
        11 * USECS_PER_DAY
    }
    fn clock(&self) -> i64 {
        12 * USECS_PER_DAY
    }
    fn user(&self) -> &str {
        "alice"
    }
    fn session_user(&self) -> &str {
        "postgres"
    }
    fn database(&self) -> &str {
        "postgres"
    }
    fn schemas(&self) -> Vec<String> {
        vec!["public".to_string()]
    }
    fn backend_pid(&self) -> i32 {
        42
    }
    fn version(&self) -> String {
        "PostgreSQL 19.0".to_string()
    }
    fn setting(&self, name: &str) -> Option<String> {
        (name == "datestyle").then(|| "ISO, MDY".to_string())
    }
}

/// A call of the function in `pg_catalog` with this name and these argument types.
fn call_types(name: &str, types: &[u32], args: &[Value]) -> Result<Value> {
    call_declared(name, types, types, args)
}

/// A call of the function with these declared argument types, with other argument types in the query, as for a polymorphic function.
fn call_declared(name: &str, declared: &[u32], types: &[u32], args: &[Value]) -> Result<Value> {
    let proc = builtin::procs_named(name)
        .find(|p| p.argtypes == declared)
        .unwrap_or_else(|| panic!("no function {name}{declared:?}"));
    let kernel = kernel(proc.oid).unwrap_or_else(|| panic!("no kernel for {name}{types:?}"));
    let session = TestSession::new();
    let call = Call { session: &session, args: types, ret: proc.rettype, variadic: false };
    kernel(&call, args)
}

/// A call with the argument types from the values.
fn call(name: &str, args: &[Value]) -> Result<Value> {
    let types: Vec<u32> = args.iter().map(type_of).collect();
    call_types(name, &types, args)
}

fn type_of(v: &Value) -> u32 {
    match v {
        Value::Bool(_) => oid::BOOL,
        Value::Int2(_) => oid::INT2,
        Value::Int4(_) => oid::INT4,
        Value::Int8(_) => oid::INT8,
        Value::Float4(_) => oid::FLOAT4,
        Value::Float8(_) => oid::FLOAT8,
        Value::Numeric(_) => oid::NUMERIC,
        Value::Oid(_) => oid::OID,
        Value::Date(_) => oid::DATE,
        Value::Timestamp(_) => oid::TIMESTAMP,
        Value::TimestampTz(_) => oid::TIMESTAMPTZ,
        _ => oid::TEXT,
    }
}

/// A call of the function that implements a binary operator.
fn operator(name: &str, a: Value, b: Value) -> Result<Value> {
    let (left, right) = (type_of(&a), type_of(&b));
    let op = builtin::operators_named(name)
        .find(|op| op.left == left && op.right == right)
        .unwrap_or_else(|| panic!("no operator {name}"));
    let proc = builtin::proc_by_oid(op.code).expect("operator function");
    call_types(proc.name, &[left, right], &[a, b])
}

fn text(s: &str) -> Value {
    Value::text(s)
}

fn sqlstate(r: Result<Value>) -> SqlState {
    r.expect_err("an error").state()
}

fn numeric(s: &str) -> Value {
    Value::Numeric(rupg_types::numeric_in(s, -1).expect("numeric"))
}

fn shown(ty: u32, v: &Value) -> String {
    let mut out = Vec::new();
    output(ty, v, &TestSession::new(), &mut out).expect("output");
    String::from_utf8(out).expect("utf8")
}

#[test]
fn integer_arithmetic_checks_the_range_of_the_result_type() {
    assert_eq!(operator("+", Value::Int4(1), Value::Int4(2)).unwrap(), Value::Int4(3));
    assert_eq!(operator("+", Value::Int4(1), Value::Int8(2)).unwrap(), Value::Int8(3));
    let error = operator("+", Value::Int4(i32::MAX), Value::Int4(1)).unwrap_err();
    assert_eq!(error.state(), SqlState::NUMERIC_VALUE_OUT_OF_RANGE);
    assert_eq!(error.message(), "integer out of range");
    assert_eq!(sqlstate(operator("/", Value::Int4(1), Value::Int4(0))), SqlState::DIVISION_BY_ZERO);
    assert_eq!(operator("/", Value::Int4(-7), Value::Int4(2)).unwrap(), Value::Int4(-3));
    assert_eq!(operator("%", Value::Int4(-7), Value::Int4(2)).unwrap(), Value::Int4(-1));
}

#[test]
fn float_arithmetic_checks_overflow() {
    assert_eq!(operator("*", Value::Float8(1.5), Value::Float8(2.0)).unwrap(), Value::Float8(3.0));
    assert_eq!(
        sqlstate(operator("*", Value::Float8(1e300), Value::Float8(1e300))),
        SqlState::NUMERIC_VALUE_OUT_OF_RANGE
    );
    assert_eq!(
        sqlstate(operator("/", Value::Float8(1.0), Value::Float8(0.0))),
        SqlState::DIVISION_BY_ZERO
    );
}

#[test]
fn numeric_arithmetic_and_casts() {
    let sum = operator("+", numeric("1.5"), numeric("2.25")).unwrap();
    assert_eq!(shown(oid::NUMERIC, &sum), "3.75");
    assert_eq!(call_types("int4", &[oid::NUMERIC], &[numeric("2.5")]).unwrap(), Value::Int4(3));
    assert_eq!(call_types("int4", &[oid::NUMERIC], &[numeric("-2.5")]).unwrap(), Value::Int4(-3));
    assert_eq!(
        call_types("numeric", &[oid::FLOAT4], &[Value::Float4(1.234_567_9)])
            .map(|v| shown(oid::NUMERIC, &v))
            .unwrap(),
        "1.23457"
    );
    assert_eq!(call_types("int4", &[oid::FLOAT8], &[Value::Float8(2.5)]).unwrap(), Value::Int4(2));
    assert_eq!(
        sqlstate(call_types("int2", &[oid::FLOAT8], &[Value::Float8(f64::NAN)])),
        SqlState::NUMERIC_VALUE_OUT_OF_RANGE
    );
    let n = Numeric::from_integer(7);
    assert_eq!(
        call_types("float8", &[oid::NUMERIC], &[Value::Numeric(n)]).unwrap(),
        Value::Float8(7.0)
    );
}

#[test]
fn comparisons_of_mixed_types() {
    assert_eq!(operator("<", Value::Int4(1), Value::Int8(2)).unwrap(), Value::Bool(true));
    assert_eq!(operator("=", text("a"), text("a")).unwrap(), Value::Bool(true));
    assert_eq!(
        operator(">", Value::Float8(f64::NAN), Value::Float8(1.0)).unwrap(),
        Value::Bool(true)
    );
    let a = Value::Array(Box::new(Array::one(vec![Some(Value::Int4(1)), None])));
    let b = Value::Array(Box::new(Array::one(vec![Some(Value::Int4(1)), Some(Value::Int4(9))])));
    let r = call_declared(
        "array_gt",
        &[oid::ANYARRAY, oid::ANYARRAY],
        &[oid::INT4_ARRAY, oid::INT4_ARRAY],
        &[a, b],
    )
    .unwrap();
    assert_eq!(r, Value::Bool(true));
}

#[test]
fn string_functions() {
    assert_eq!(
        call("substr", &[text("hello"), Value::Int4(2), Value::Int4(3)]).unwrap(),
        text("ell")
    );
    assert_eq!(
        call("substr", &[text("hello"), Value::Int4(-1), Value::Int4(3)]).unwrap(),
        text("h")
    );
    assert_eq!(
        sqlstate(call("substr", &[text("hello"), Value::Int4(1), Value::Int4(-1)])),
        SqlState::SUBSTRING_ERROR
    );
    assert_eq!(
        call("split_part", &[text("a,b,c"), text(","), Value::Int4(-1)]).unwrap(),
        text("c")
    );
    assert_eq!(call("split_part", &[text("a,b,c"), text(","), Value::Int4(4)]).unwrap(), text(""));
    assert_eq!(call("lpad", &[text("hi"), Value::Int4(5), text("xy")]).unwrap(), text("xyxhi"));
    assert_eq!(call("rpad", &[text("hello"), Value::Int4(2), text("x")]).unwrap(), text("he"));
    assert_eq!(call("left", &[text("abcde"), Value::Int4(-2)]).unwrap(), text("abc"));
    assert_eq!(call("right", &[text("hello"), Value::Int4(-2)]).unwrap(), text("llo"));
    assert_eq!(call("upper", &[text("abc\u{e9}")]).unwrap(), text("ABC\u{e9}"));
    assert_eq!(call("initcap", &[text("hello wORLD 1st")]).unwrap(), text("Hello World 1st"));
    assert_eq!(call("btrim", &[text("xxhixx"), text("x")]).unwrap(), text("hi"));
    assert_eq!(call("strpos", &[text("h\u{e9}llo"), text("l")]).unwrap(), Value::Int4(3));
    assert_eq!(call("to_hex", &[Value::Int4(-1)]).unwrap(), text("ffffffff"));
    assert_eq!(call("md5", &[text("abc")]).unwrap(), text("900150983cd24fb0d6963f7d28e17f72"));
    assert_eq!(call("quote_ident", &[text("select")]).unwrap(), text("\"select\""));
    assert_eq!(call("quote_ident", &[text("abort")]).unwrap(), text("abort"));
    assert_eq!(call("quote_literal", &[text("it's \\")]).unwrap(), text("E'it''s \\\\'"));
    assert_eq!(sqlstate(call("chr", &[Value::Int4(0)])), SqlState::PROGRAM_LIMIT_EXCEEDED);
    assert_eq!(call("chr", &[Value::Int4(233)]).unwrap(), text("\u{e9}"));
}

#[test]
fn like_matches_as_postgres_does() {
    use crate::text::like;
    assert!(like("hello", "h%o").unwrap());
    assert!(like("hello", "h_llo").unwrap());
    assert!(!like("hello", "h_lo").unwrap());
    assert!(like("h\u{e9}llo", "h_llo").unwrap());
    assert!(like("50%", "50\\%").unwrap());
    assert!(like("", "%").unwrap());
    assert!(like("abc", "%%c").unwrap());
    assert_eq!(like("abc", "a\\").unwrap_err().state(), SqlState::INVALID_ESCAPE_SEQUENCE);
    assert_eq!(call("like_escape", &[text("a#%b\\"), text("#")]).unwrap(), text("a\\%b\\\\"));
}

#[test]
fn session_functions() {
    assert_eq!(call("current_database", &[]).unwrap(), text("postgres"));
    assert_eq!(call("now", &[]).unwrap(), Value::TimestampTz(10 * USECS_PER_DAY));
    assert_eq!(call("pg_backend_pid", &[]).unwrap(), Value::Int4(42));
    assert_eq!(call("current_setting", &[text("datestyle")]).unwrap(), text("ISO, MDY"));
    let error = call("current_setting", &[text("nope")]).unwrap_err();
    assert_eq!(error.state(), SqlState::UNDEFINED_OBJECT);
    assert_eq!(error.message(), "unrecognized configuration parameter \"nope\"");
    let schemas = call("current_schemas", &[Value::Bool(true)]).unwrap();
    assert_eq!(shown(oid::NAME_ARRAY, &schemas), "{pg_catalog,public}");
    let ty = call_types(
        "format_type",
        &[oid::OID, oid::INT4],
        &[Value::Oid(oid::VARCHAR), Value::Int4(14)],
    )
    .unwrap();
    assert_eq!(ty, text("character varying(10)"));
    let ty =
        call_types("format_type", &[oid::OID, oid::INT4], &[Value::Oid(oid::BPCHAR), Value::Null])
            .unwrap();
    assert_eq!(ty, text("character"));
}

#[test]
fn date_and_timestamp_casts() {
    let ts = call_types("timestamp", &[oid::DATE], &[Value::Date(1)]).unwrap();
    assert_eq!(ts, Value::Timestamp(USECS_PER_DAY));
    let tstz = call_types("timestamptz", &[oid::TIMESTAMP], &[ts]).unwrap();
    assert_eq!(tstz, Value::TimestampTz(USECS_PER_DAY));
    assert_eq!(call_types("date", &[oid::TIMESTAMPTZ], &[tstz]).unwrap(), Value::Date(1));
    assert_eq!(operator("-", Value::Date(5), Value::Date(2)).unwrap(), Value::Int4(3));
    let error = operator("+", Value::Date(2_147_483_000), Value::Int4(1_000)).unwrap_err();
    assert_eq!(error.state(), SqlState::DATETIME_VALUE_OUT_OF_RANGE);
}
