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

/// The text of each column of each row of a query, `NULL` for a null value, or the SQLSTATE and the message of the error.
fn rows(sql: &str) -> std::result::Result<Vec<Vec<String>>, (String, String)> {
    let session = TestSession { zone: FixedZone::utc() };
    let fail = |e: rupg_common::Error| (e.state().as_str().to_string(), e.message().to_string());
    let (stmts, _) = rupg_sql::parse(sql).map_err(|e| ("42601".to_string(), e.message.clone()))?;
    let Some(Some(Node::RawStmt(raw))) = stmts.first() else { panic!("no statement in {sql}") };
    let query = analyze(raw.stmt.as_ref().expect("a statement"), &session, &Params::default())
        .map_err(fail)?;
    let plan = prepare(query).map_err(fail)?;
    let rows = plan.run(&[], &session).map_err(fail)?;
    let texts = rows
        .into_iter()
        .map(|values| {
            plan.columns()
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
                .collect()
        })
        .collect();
    Ok(texts)
}

/// The row of a query that gives at most one row.
fn row(sql: &str) -> std::result::Result<Option<Vec<String>>, (String, String)> {
    let mut all = rows(sql)?;
    assert!(all.len() < 2, "{sql} gave {} rows", all.len());
    Ok(all.pop())
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

/// The rows of a query, each as its columns joined with `|`.
fn lines(sql: &str) -> Vec<String> {
    let mut all: Vec<String> = match rows(sql) {
        Ok(rows) => rows.into_iter().map(|r| r.join("|")).collect(),
        Err(e) => panic!("{sql}: {e:?}"),
    };
    all.sort();
    all
}

#[test]
fn catalog_tables() {
    big_stack(|| {
        assert_eq!(lines("SELECT nspname FROM pg_namespace"), ["pg_catalog", "pg_toast", "public"]);
        assert_eq!(
            lines("SELECT oid, typname, typlen FROM pg_type WHERE typname = 'int4'"),
            ["23|int4|4"]
        );
        assert_eq!(
            lines(
                "SELECT p.proname, t.typname FROM pg_proc p JOIN pg_type t ON p.prorettype = t.oid WHERE p.oid = 184"
            ),
            ["oideq|bool"]
        );
        assert_eq!(
            lines(
                "SELECT a.amname, b.amname FROM pg_am a LEFT JOIN pg_am b ON a.oid = b.oid AND b.amtype = 'i' WHERE a.amname IN ('heap', 'hash')"
            ),
            ["hash|hash", "heap|NULL"]
        );
        assert_eq!(
            lines(
                "SELECT a.oid, b.oid FROM pg_am a FULL JOIN pg_am b ON a.oid = b.oid AND a.oid < 403 AND b.oid < 403"
            ),
            [
                "2742|NULL",
                "2|2",
                "3580|NULL",
                "4000|NULL",
                "403|NULL",
                "405|NULL",
                "783|NULL",
                "NULL|2742",
                "NULL|3580",
                "NULL|4000",
                "NULL|403",
                "NULL|405",
                "NULL|783"
            ]
        );
        assert_eq!(
            lines("SELECT oid FROM pg_am a FULL JOIN pg_am b USING (oid) WHERE oid IN (2, 403)"),
            ["2", "403"]
        );
        assert_eq!(
            lines(
                "SELECT b.oid FROM pg_am a RIGHT JOIN pg_am b ON false WHERE a.oid IS NULL AND b.oid = 2"
            ),
            ["2"]
        );
        assert_eq!(lines("SELECT tableoid FROM pg_am WHERE oid = 2"), ["2601"]);
        assert_eq!(
            lines("SELECT indkey, indclass FROM pg_index WHERE indexrelid = 2662"),
            ["1|1981"]
        );
        assert_eq!(
            lines("SELECT oprcode FROM pg_operator WHERE oid IN (96, 15)"),
            ["int48eq", "int4eq"]
        );
        assert_eq!(
            lines("SELECT 1 FROM pg_am a, pg_am b, pg_am c WHERE a.oid = b.oid AND b.oid = c.oid"),
            ["1", "1", "1", "1", "1", "1", "1"]
        );
    });
}

/// The rows of a query in the order of the result, each as its columns joined with `|`.
fn ordered(sql: &str) -> Vec<String> {
    match rows(sql) {
        Ok(rows) => rows.into_iter().map(|r| r.join("|")).collect(),
        Err(e) => panic!("{sql}: {e:?}"),
    }
}

#[test]
fn order_distinct_and_limit() {
    big_stack(|| {
        assert_eq!(
            ordered("SELECT amname FROM pg_am ORDER BY amname"),
            ["brin", "btree", "gin", "gist", "hash", "heap", "spgist"]
        );
        assert_eq!(
            ordered("SELECT amname FROM pg_am ORDER BY amname DESC LIMIT 2 OFFSET 1"),
            ["heap", "hash"]
        );
        assert_eq!(
            ordered("SELECT amname FROM pg_am ORDER BY amtype DESC, 1 USING >"),
            ["heap", "spgist", "hash", "gist", "gin", "btree", "brin"]
        );
        assert_eq!(
            ordered("SELECT NULLIF(amname, 'heap') AS n FROM pg_am ORDER BY n NULLS FIRST LIMIT 2"),
            ["NULL", "brin"]
        );
        assert_eq!(
            ordered("SELECT NULLIF(amname, 'heap') AS n FROM pg_am ORDER BY n DESC LIMIT 2"),
            ["NULL", "spgist"]
        );
        assert_eq!(ordered("SELECT DISTINCT amtype FROM pg_am ORDER BY 1"), ["i", "t"]);
        assert_eq!(
            ordered(
                "SELECT DISTINCT ON (amtype) amtype, amname FROM pg_am ORDER BY amtype, amname"
            ),
            ["i|brin", "t|heap"]
        );
        assert_eq!(
            ordered("SELECT amtype FROM pg_am ORDER BY amtype FETCH FIRST 1 ROW WITH TIES"),
            ["i", "i", "i", "i", "i", "i"]
        );
        assert_eq!(ordered("SELECT amname FROM pg_am ORDER BY amname LIMIT 0"), [""; 0]);
        assert_eq!(ordered("SELECT amname FROM pg_am ORDER BY amname OFFSET 9"), [""; 0]);
        assert_eq!(ordered("SELECT amname FROM pg_am LIMIT NULL OFFSET NULL").len(), 7);
        assert_eq!(
            ordered("SELECT amname FROM pg_am ORDER BY amname LIMIT 1.5"),
            ["brin", "btree"]
        );
        assert_eq!(error("SELECT amname FROM pg_am LIMIT -1"), "2201W");
        assert_eq!(error("SELECT amname FROM pg_am OFFSET -1"), "2201X");
        // OFFSET comes first, as recompute_limits does.
        assert_eq!(error("SELECT amname FROM pg_am LIMIT -1 OFFSET -1"), "2201X");
    });
}
