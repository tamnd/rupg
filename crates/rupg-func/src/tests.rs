use std::rc::Rc;

use rupg_common::{Result, SqlState};
use rupg_pgcatalog::builtin;
use rupg_types::{
    Array, ByteaOutput, DateFormat, FixedZone, IntervalStyle, Numeric, TimeZone, USECS_PER_DAY,
    Value, oid,
};

use crate::{Call, Session, kernel, output};

struct TestSession {
    zone: Rc<FixedZone>,
}

impl TestSession {
    fn new() -> TestSession {
        TestSession { zone: Rc::new(FixedZone::utc()) }
    }
}

impl Session for TestSession {
    fn date_format(&self) -> DateFormat {
        DateFormat::ISO_MDY
    }
    fn interval_style(&self) -> IntervalStyle {
        IntervalStyle::Postgres
    }
    fn zone(&self) -> Result<Rc<dyn TimeZone>> {
        Ok(Rc::clone(&self.zone) as Rc<dyn TimeZone>)
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
    fn set_setting(&self, _: &str, _: Option<&str>, _: bool) -> Result<String> {
        Err(rupg_common::Error::internal("the test session has no settings to change"))
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

#[test]
fn oidvector_compares_the_length_first() {
    let vector = |oids: &[u32]| {
        Value::Array(Box::new(Array::one(oids.iter().map(|o| Some(Value::Oid(*o))).collect())))
    };
    let types = [oid::OIDVECTOR, oid::OIDVECTOR];
    let (short, long) = (vector(&[9]), vector(&[1, 2]));
    assert_eq!(
        call_types("oidvectorlt", &types, &[short.clone(), long.clone()]),
        Ok(Value::Bool(true))
    );
    assert_eq!(call_types("btoidvectorcmp", &types, &[long.clone(), short]), Ok(Value::Int4(1)));
    assert_eq!(
        call_types("oidvectorgt", &types, &[vector(&[1, 3]), long.clone()]),
        Ok(Value::Bool(true))
    );
    assert_eq!(call_types("oidvectoreq", &types, &[vector(&[1, 2]), long]), Ok(Value::Bool(true)));
}

/// A call of a privilege function. A text value fits an argument of type `name` or `text`.
fn privilege(name: &str, args: &[Value]) -> Result<Value> {
    let fits = |ty: &u32, v: &Value| match v {
        Value::Text(_) => *ty == oid::TEXT || *ty == oid::NAME,
        Value::Oid(_) => *ty == oid::OID,
        Value::Int2(_) => *ty == oid::INT2,
        _ => false,
    };
    let proc = builtin::procs_named(name)
        .find(|p| {
            p.argtypes.len() == args.len() && p.argtypes.iter().zip(args).all(|(t, v)| fits(t, v))
        })
        .unwrap_or_else(|| panic!("no function {name} for {args:?}"));
    call_types(name, proc.argtypes, args)
}

/// The results of PostgreSQL 19 for the same calls, as a superuser.
#[test]
fn table_and_sequence_privileges() {
    let yes = Ok(Value::Bool(true));
    let no = Ok(Value::Bool(false));
    let table = |args: &[&str]| {
        privilege("has_table_privilege", &args.iter().map(|a| text(a)).collect::<Vec<_>>())
    };
    for p in [
        "select",
        "SELECT, INSERT",
        " select with grant option ",
        "select  ,  insert",
        "trigger, truncate",
        "MAINTAIN",
    ] {
        assert_eq!(table(&["pg_class", p]), yes, "{p}");
    }
    assert_eq!(table(&["pg_catalog.pg_class", "references"]), yes);
    assert_eq!(table(&["\"pg_class\"", "select"]), yes);
    assert_eq!(table(&["PG_CLASS", "select"]), yes);
    for p in ["foo", "", "select,", "usage", "rule"] {
        assert_eq!(sqlstate(table(&["pg_class", p])), SqlState::INVALID_PARAMETER_VALUE, "{p}");
    }
    let error = table(&["pg_class", "select,"]).unwrap_err();
    assert_eq!(error.message(), "unrecognized privilege type: \"\"");
    assert_eq!(sqlstate(table(&["nosuch", "select"])), SqlState::UNDEFINED_TABLE);
    assert_eq!(sqlstate(table(&["nosuch.pg_class", "select"])), SqlState::UNDEFINED_SCHEMA);
    assert_eq!(sqlstate(table(&["a.b.c.d", "select"])), SqlState::SYNTAX_ERROR);
    assert_eq!(
        privilege("has_table_privilege", &[Value::Oid(99999), text("select")]),
        Ok(Value::Null)
    );
    assert_eq!(sqlstate(table(&["nobody", "pg_class", "select"])), SqlState::UNDEFINED_OBJECT);
    assert_eq!(
        privilege("has_table_privilege", &[Value::Oid(99999), text("pg_class"), text("select")]),
        yes
    );
    assert_eq!(table(&["pg_monitor", "pg_class", "select"]), yes);
    assert_eq!(table(&["pg_monitor", "pg_class", "insert"]), no);
    assert_eq!(table(&["pg_monitor", "pg_class", "update"]), no);
    assert_eq!(table(&["pg_monitor", "pg_class", "select with grant option"]), no);
    assert_eq!(table(&["pg_monitor", "pg_authid", "select"]), no);
    assert_eq!(table(&["pg_read_all_data", "pg_authid", "select"]), yes);
    assert_eq!(table(&["pg_read_all_data", "pg_class", "select with grant option"]), no);
    assert_eq!(table(&["pg_write_all_data", "pg_class", "update"]), no);
    assert_eq!(table(&["pg_maintain", "pg_class", "maintain"]), yes);
    assert_eq!(table(&["public", "pg_class", "select"]), yes);
    assert_eq!(table(&["public", "pg_authid", "select"]), no);
    assert_eq!(table(&["pg_database_owner", "pg_class", "select"]), yes);
    assert_eq!(table(&["postgres", "pg_class", "select"]), yes);
    assert_eq!(
        privilege("has_table_privilege", &[Value::Oid(10), text("pg_class"), text("select")]),
        yes
    );

    let sequence = |args: &[Value]| privilege("has_sequence_privilege", args);
    assert_eq!(sqlstate(sequence(&[text("pg_class"), text("usage")])), SqlState::WRONG_OBJECT_TYPE);
    assert_eq!(sequence(&[Value::Oid(99999), text("usage")]), Ok(Value::Null));
    let error = sequence(&[Value::Oid(1259), text("usage")]).unwrap_err();
    assert_eq!(
        (error.state(), error.message()),
        (SqlState::WRONG_OBJECT_TYPE, "\"pg_class\" is not a sequence")
    );
    assert_eq!(sqlstate(sequence(&[text("nosuch"), text("usage")])), SqlState::UNDEFINED_TABLE);
}

#[test]
fn column_privileges() {
    let yes = Ok(Value::Bool(true));
    let column = |args: &[Value]| privilege("has_column_privilege", args);
    assert_eq!(column(&[text("pg_class"), text("relname"), text("select")]), yes);
    assert_eq!(column(&[text("pg_class"), text("relname"), text("insert")]), yes);
    assert_eq!(
        sqlstate(column(&[text("pg_class"), text("relname"), text("delete")])),
        SqlState::INVALID_PARAMETER_VALUE
    );
    let error = column(&[text("pg_class"), text("nosuch"), text("select")]).unwrap_err();
    assert_eq!(
        (error.state(), error.message()),
        (SqlState::UNDEFINED_COLUMN, "column \"nosuch\" of relation \"pg_class\" does not exist")
    );
    assert_eq!(
        sqlstate(column(&[Value::Oid(1259), text("nosuch"), text("select")])),
        SqlState::UNDEFINED_COLUMN
    );
    for (attnum, expected) in [
        (1, yes.clone()),
        (-1, yes.clone()),
        (0, Ok(Value::Null)),
        (-7, Ok(Value::Null)),
        (999, Ok(Value::Null)),
    ] {
        assert_eq!(
            column(&[text("pg_class"), Value::Int2(attnum), text("select")]),
            expected,
            "{attnum}"
        );
    }
    assert_eq!(column(&[Value::Oid(99999), Value::Int2(1), text("select")]), Ok(Value::Null));
    let monitor = |table: &str, name: &str| {
        column(&[text("pg_monitor"), text(table), text(name), text("select")])
    };
    assert_eq!(monitor("pg_subscription", "subname"), yes);
    assert_eq!(monitor("pg_subscription", "subconninfo"), Ok(Value::Bool(false)));

    let any = |args: &[&str]| {
        privilege("has_any_column_privilege", &args.iter().map(|a| text(a)).collect::<Vec<_>>())
    };
    assert_eq!(any(&["pg_monitor", "pg_subscription", "select"]), yes);
    assert_eq!(any(&["pg_monitor", "pg_authid", "select"]), Ok(Value::Bool(false)));
    assert_eq!(sqlstate(any(&["pg_class", "delete"])), SqlState::INVALID_PARAMETER_VALUE);
}

#[test]
fn database_privileges() {
    let yes = Ok(Value::Bool(true));
    let no = Ok(Value::Bool(false));
    let check = |name: &str, args: &[&str]| {
        privilege(name, &args.iter().map(|a| text(a)).collect::<Vec<_>>())
    };
    let by_oid = |name: &str, p: &str| privilege(name, &[Value::Oid(99999), text(p)]);

    let database = |args: &[&str]| check("has_database_privilege", args);
    assert_eq!(database(&["postgres", "connect"]), yes);
    assert_eq!(database(&["postgres", "temporary"]), yes);
    assert_eq!(database(&["postgres", "create"]), yes);
    assert_eq!(sqlstate(database(&["postgres", "select"])), SqlState::INVALID_PARAMETER_VALUE);
    assert_eq!(sqlstate(database(&["nosuch", "connect"])), SqlState::UNDEFINED_DATABASE);
    assert_eq!(database(&["pg_monitor", "template0", "connect"]), yes);
    assert_eq!(database(&["pg_monitor", "template1", "connect"]), yes);
    assert_eq!(database(&["pg_monitor", "postgres", "temp"]), yes);
    assert_eq!(database(&["pg_monitor", "postgres", "create"]), no);
    assert_eq!(by_oid("has_database_privilege", "connect"), yes);
}

#[test]
fn schema_privileges() {
    let yes = Ok(Value::Bool(true));
    let no = Ok(Value::Bool(false));
    let check = |name: &str, args: &[&str]| {
        privilege(name, &args.iter().map(|a| text(a)).collect::<Vec<_>>())
    };
    let by_oid = |name: &str, p: &str| privilege(name, &[Value::Oid(99999), text(p)]);

    let schema = |args: &[&str]| check("has_schema_privilege", args);
    assert_eq!(schema(&["pg_catalog", "usage"]), yes);
    assert_eq!(sqlstate(schema(&["nosuch", "usage"])), SqlState::UNDEFINED_SCHEMA);
    assert_eq!(by_oid("has_schema_privilege", "usage"), yes);
    assert_eq!(schema(&["pg_monitor", "pg_catalog", "create"]), no);
    assert_eq!(schema(&["pg_monitor", "public", "create"]), no);
    assert_eq!(schema(&["pg_monitor", "pg_toast", "usage"]), no);
    assert_eq!(schema(&["pg_monitor", "public", "usage"]), yes);
    assert_eq!(schema(&["pg_read_all_data", "pg_toast", "usage"]), yes);
    assert_eq!(schema(&["pg_write_all_data", "pg_toast", "usage"]), yes);
    assert_eq!(schema(&["pg_database_owner", "public", "create"]), yes);
}

#[test]
fn function_privileges() {
    big_stack(function_cases);
}

fn function_cases() {
    let yes = Ok(Value::Bool(true));
    let no = Ok(Value::Bool(false));
    let check = |name: &str, args: &[&str]| {
        privilege(name, &args.iter().map(|a| text(a)).collect::<Vec<_>>())
    };
    let by_oid = |name: &str, p: &str| privilege(name, &[Value::Oid(99999), text(p)]);

    let function = |args: &[&str]| check("has_function_privilege", args);
    assert_eq!(function(&["now()", "execute"]), yes);
    assert_eq!(function(&["pg_catalog.now()", "execute"]), yes);
    assert_eq!(function(&["upper(text)", "EXECUTE"]), yes);
    assert_eq!(by_oid("has_function_privilege", "execute"), yes);
    assert_eq!(function(&["pg_monitor", "pg_ls_waldir()", "execute"]), yes);
    assert_eq!(function(&["pg_monitor", "pg_read_file(text)", "execute"]), no);
    assert_eq!(function(&["pg_read_server_files", "pg_read_file(text)", "execute"]), no);
    assert_eq!(sqlstate(function(&["nosuch()", "execute"])), SqlState::UNDEFINED_FUNCTION);
    assert_eq!(sqlstate(function(&["now", "execute"])), SqlState::INVALID_TEXT_REPRESENTATION);
    assert_eq!(function(&["pg_monitor", "pg_read_file(text)", "execute with grant option"]), no);
    assert_eq!(function(&["pg_monitor", "now()", "execute with grant option"]), no);
}

/// Runs the cases in a thread with 8 MiB of stack. A type name, also in the signature of a function, goes through the parser, and in a debug build the action functions of the parser have large frames, which need more than the 2 MiB of a test thread.
// The thread is not an engine task, so it uses std::thread and not the Tasks trait of rupg-platform.
#[allow(clippy::disallowed_methods)]
fn big_stack(cases: fn()) {
    std::thread::Builder::new().stack_size(8 << 20).spawn(cases).unwrap().join().unwrap();
}

#[test]
fn language_and_type_privileges() {
    big_stack(language_and_type_cases);
}

fn language_and_type_cases() {
    let yes = Ok(Value::Bool(true));
    let check = |name: &str, args: &[&str]| {
        privilege(name, &args.iter().map(|a| text(a)).collect::<Vec<_>>())
    };
    let by_oid = |name: &str, p: &str| privilege(name, &[Value::Oid(99999), text(p)]);

    let language = |args: &[&str]| check("has_language_privilege", args);
    assert_eq!(language(&["sql", "usage"]), yes);
    assert_eq!(language(&["pg_monitor", "c", "usage"]), yes);
    assert_eq!(language(&["pg_monitor", "internal", "usage"]), yes);
    assert_eq!(by_oid("has_language_privilege", "usage"), yes);

    let ty = |args: &[&str]| check("has_type_privilege", args);
    assert_eq!(ty(&["int4", "usage"]), yes);
    assert_eq!(ty(&["pg_monitor", "int4", "usage"]), yes);
    assert_eq!(ty(&["int4[]", "usage"]), yes);
    assert_eq!(ty(&["pg_monitor", "int4[]", "usage"]), yes);
    assert_eq!(ty(&["integer", "usage"]), yes);
    assert_eq!(by_oid("has_type_privilege", "usage"), yes);
    assert_eq!(sqlstate(ty(&["nosuch", "usage"])), SqlState::UNDEFINED_OBJECT);
}

#[test]
fn tablespace_and_foreign_privileges() {
    let yes = Ok(Value::Bool(true));
    let no = Ok(Value::Bool(false));
    let check = |name: &str, args: &[&str]| {
        privilege(name, &args.iter().map(|a| text(a)).collect::<Vec<_>>())
    };
    let by_oid = |name: &str, p: &str| privilege(name, &[Value::Oid(99999), text(p)]);

    let tablespace = |args: &[&str]| check("has_tablespace_privilege", args);
    assert_eq!(tablespace(&["pg_default", "create"]), yes);
    assert_eq!(tablespace(&["pg_monitor", "pg_default", "create"]), no);
    assert_eq!(tablespace(&["pg_monitor", "pg_global", "create"]), no);
    assert_eq!(by_oid("has_tablespace_privilege", "create"), yes);

    let error = check("has_foreign_data_wrapper_privilege", &["nosuch", "usage"]).unwrap_err();
    assert_eq!(
        (error.state(), error.message()),
        (SqlState::UNDEFINED_OBJECT, "foreign-data wrapper \"nosuch\" does not exist")
    );
    let error = check("has_server_privilege", &["nosuch", "usage"]).unwrap_err();
    assert_eq!(
        (error.state(), error.message()),
        (SqlState::UNDEFINED_OBJECT, "server \"nosuch\" does not exist")
    );
}

#[test]
fn role_and_parameter_privileges() {
    let yes = Ok(Value::Bool(true));
    let no = Ok(Value::Bool(false));
    let role =
        |args: &[&str]| privilege("pg_has_role", &args.iter().map(|a| text(a)).collect::<Vec<_>>());
    assert_eq!(role(&["pg_monitor", "member"]), yes);
    assert_eq!(role(&["pg_monitor", "pg_read_all_stats", "member"]), yes);
    assert_eq!(role(&["pg_monitor", "pg_read_all_stats", "usage"]), yes);
    assert_eq!(role(&["pg_monitor", "pg_read_all_stats", "member with admin option"]), no);
    assert_eq!(role(&["pg_monitor", "pg_read_all_stats", "usage with grant option"]), no);
    assert_eq!(role(&["pg_read_all_stats", "pg_monitor", "member"]), no);
    assert_eq!(role(&["pg_monitor", "pg_monitor", "set"]), yes);
    assert_eq!(role(&["pg_monitor", "pg_signal_backend", "member"]), no);
    assert_eq!(sqlstate(role(&["nosuch", "member"])), SqlState::UNDEFINED_OBJECT);
    assert_eq!(sqlstate(role(&["public", "member"])), SqlState::UNDEFINED_OBJECT);
    assert_eq!(sqlstate(role(&["pg_monitor", "foo"])), SqlState::INVALID_PARAMETER_VALUE);
    let unknown = |args: &[Value]| privilege("pg_has_role", args);
    assert_eq!(unknown(&[Value::Oid(99999), text("pg_monitor"), text("member")]), no);
    assert_eq!(unknown(&[text("pg_monitor"), Value::Oid(99999), text("member")]), no);
    assert_eq!(role(&["pg_monitor", "pg_database_owner", "member"]), no);
    assert_eq!(role(&["postgres", "pg_database_owner", "member"]), yes);
    assert_eq!(role(&["postgres", "pg_database_owner", "usage"]), yes);

    let parameter = |args: &[&str]| {
        privilege("has_parameter_privilege", &args.iter().map(|a| text(a)).collect::<Vec<_>>())
    };
    assert_eq!(parameter(&["work_mem", "set"]), yes);
    assert_eq!(parameter(&["work_mem", "SET, ALTER SYSTEM"]), yes);
    assert_eq!(parameter(&["nosuch.x", "set"]), yes);
    assert_eq!(parameter(&["nosuch", "set"]), yes);
    assert_eq!(sqlstate(parameter(&["work_mem", "foo"])), SqlState::INVALID_PARAMETER_VALUE);
    assert_eq!(parameter(&["pg_monitor", "work_mem", "set"]), no);
    assert_eq!(parameter(&["pg_monitor", "shared_buffers", "alter system"]), no);
}
