use rupg_analyze::{Env, Params, analyze};
use rupg_common::Result;
use rupg_sql::nodes::Node;
use rupg_types::{
    ByteaOutput, DateFormat, FixedZone, IntervalStyle, TimeZone, USECS_PER_DAY, Value,
};

use crate::prepare;

/// A session at 2000-01-11 00:00 UTC with the default settings.
struct TestSession {
    zone: FixedZone,
}

impl rupg_func::Session for TestSession {
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
        10 * USECS_PER_DAY
    }
    fn clock(&self) -> i64 {
        10 * USECS_PER_DAY
    }
    fn user(&self) -> &str {
        "postgres"
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
        1
    }
    fn version(&self) -> String {
        "PostgreSQL 19.0".to_string()
    }
    fn setting(&self, _: &str) -> Option<String> {
        None
    }
}

impl Env for TestSession {
    fn search_path(&self) -> Vec<u32> {
        vec![2200]
    }

    fn namespace(&self, name: &str) -> Option<u32> {
        match name {
            "pg_catalog" => Some(11),
            "public" => Some(2200),
            _ => None,
        }
    }

    fn database(&self) -> String {
        "postgres".into()
    }

    fn input(&self, ty: u32, text: &str, typmod: i32) -> Result<Value> {
        rupg_func::input(ty, text, typmod, self)
    }
}

/// The text of each column of the row of a query, `NULL` for a null value, or the SQLSTATE and the message of the error.
fn row(sql: &str) -> std::result::Result<Option<Vec<String>>, (String, String)> {
    let session = TestSession { zone: FixedZone::utc() };
    let fail = |e: rupg_common::Error| (e.state().as_str().to_string(), e.message().to_string());
    let (stmts, _) = rupg_sql::parse(sql).map_err(|e| ("42601".to_string(), e.message.clone()))?;
    let Some(Some(Node::RawStmt(raw))) = stmts.first() else { panic!("no statement in {sql}") };
    let query = analyze(raw.stmt.as_ref().expect("a statement"), &session, &Params::default())
        .map_err(fail)?;
    let plan = prepare(query).map_err(fail)?;
    let Some(values) = plan.run(&[], &session).map_err(fail)? else { return Ok(None) };
    let texts = plan
        .columns()
        .iter()
        .zip(values)
        .map(|(target, value)| {
            if value.is_null() {
                return "NULL".to_string();
            }
            let mut out = Vec::new();
            rupg_func::output(target.expr.ty, &value, &session, &mut out).expect("output");
            String::from_utf8(out).expect("utf8")
        })
        .collect();
    Ok(Some(texts))
}

fn one(sql: &str) -> String {
    match row(sql) {
        Ok(Some(mut row)) => row.remove(0),
        other => panic!("{sql}: {other:?}"),
    }
}

fn error(sql: &str) -> String {
    match row(sql) {
        Err((state, _)) => state,
        other => panic!("{sql}: {other:?}"),
    }
}

/// Runs the cases in a thread with 8 MiB of stack, as the tests of rupg-analyze do.
// The thread is not an engine task, so it uses std::thread and not the Tasks trait of rupg-platform.
#[allow(clippy::disallowed_methods)]
fn big_stack(cases: fn()) {
    std::thread::Builder::new().stack_size(8 << 20).spawn(cases).unwrap().join().unwrap();
}

#[test]
fn expressions() {
    big_stack(|| {
        assert_eq!(row("SELECT 1, 'a' || 'b', 1 + 2.5").unwrap().unwrap(), ["1", "ab", "3.5"]);
        assert_eq!(one("SELECT 7 / 2"), "3");
        assert_eq!(one("SELECT 2 ^ 10 > 1000"), "t");
        assert_eq!(one("SELECT -(3)"), "-3");
        assert_eq!(one("SELECT 'abc' LIKE 'a%'"), "t");
        assert_eq!(one("SELECT 'x' || 1"), "x1");
        assert_eq!(one("SELECT 1 || true::text"), "1true");
        assert_eq!(one("SELECT CASE 2 WHEN 1 THEN 'one' WHEN 2 THEN 'two' END"), "two");
        assert_eq!(one("SELECT CASE WHEN false THEN 1 END"), "NULL");
        assert_eq!(one("SELECT COALESCE(NULL, 2, 3)"), "2");
        assert_eq!(one("SELECT GREATEST(1, NULL, 3.5, 2)"), "3.5");
        assert_eq!(one("SELECT LEAST('b', 'a', NULL)"), "a");
        assert_eq!(one("SELECT NULLIF(1, 1)"), "NULL");
        assert_eq!(one("SELECT NULLIF(1, 2)"), "1");
        assert_eq!(one("SELECT NULL IS DISTINCT FROM 1"), "t");
        assert_eq!(one("SELECT NULL IS NOT DISTINCT FROM NULL"), "t");
        assert_eq!(one("SELECT 2 IN (1, 2, NULL)"), "t");
        assert_eq!(one("SELECT 3 IN (1, 2, NULL)"), "NULL");
        assert_eq!(one("SELECT 3 NOT IN (1, 2)"), "t");
        assert_eq!(one("SELECT 1 = ANY ('{}'::int[])"), "f");
        assert_eq!(one("SELECT 1 = ALL ('{}'::int[])"), "t");
        assert_eq!(one("SELECT NULL AND false"), "f");
        assert_eq!(one("SELECT NULL OR false"), "NULL");
        assert_eq!(one("SELECT NOT NULL::bool"), "NULL");
        assert_eq!(one("SELECT 1 IS NULL"), "f");
        assert_eq!(one("SELECT NULL::bool IS UNKNOWN"), "t");
        assert_eq!(one("SELECT ARRAY[1, NULL, 3]"), "{1,NULL,3}");
        assert_eq!(one("SELECT ARRAY[ARRAY[1, 2], ARRAY[3, 4]]"), "{{1,2},{3,4}}");
    });
}

#[test]
fn errors() {
    big_stack(|| {
        assert_eq!(error("SELECT 1 / 0"), "22012");
        assert_eq!(error("SELECT 2147483647 + 1"), "22003");
        assert_eq!(error("SELECT ARRAY[ARRAY[1, 2], ARRAY[3]]"), "2202E");
        assert_eq!(error("SELECT 'abc'::int"), "22P02");
        assert_eq!(error("SELECT substr('abc', 1, -1)"), "22011");
    });
}

#[test]
fn filter_and_session_values() {
    big_stack(|| {
        assert_eq!(row("SELECT 1 WHERE false").unwrap(), None);
        assert_eq!(row("SELECT 1 WHERE NULL").unwrap(), None);
        assert_eq!(one("SELECT 1 WHERE 1 < 2"), "1");
        assert_eq!(one("SELECT current_user"), "postgres");
        assert_eq!(one("SELECT current_schema"), "public");
        assert_eq!(one("SELECT CURRENT_DATE"), "2000-01-11");
        assert_eq!(one("SELECT now()"), "2000-01-11 00:00:00+00");
        assert_eq!(one("SELECT LOCALTIMESTAMP"), "2000-01-11 00:00:00");
        assert_eq!(one("SELECT CURRENT_TIME"), "00:00:00+00");
    });
}
