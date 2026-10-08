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

#[test]
fn aggregates_and_groups() {
    big_stack(|| {
        // The values come from PostgreSQL 19 with the same catalog rows.
        let all = |sql: &str| row(sql).unwrap_or_else(|e| panic!("{sql}: {e:?}")).expect(sql);
        assert_eq!(
            all(
                "SELECT count(*), sum(relnatts), avg(relnatts), min(relname), max(relname) FROM pg_class WHERE oid < 10000"
            ),
            [
                "258",
                "994",
                "3.8527131782945736",
                "pg_aggregate",
                "pg_user_mapping_user_server_index"
            ]
        );
        assert_eq!(
            all(
                "SELECT bool_and(relhasindex), bool_or(relhasindex), every(relispopulated), bit_and(relnatts), bit_or(relnatts), bit_xor(relnatts) FROM pg_class WHERE oid < 10000"
            ),
            ["f", "t", "t", "0", "63", "22"]
        );
        assert_eq!(
            all(
                "SELECT avg(relnatts::int8), sum(relnatts::numeric), avg(relnatts::numeric / 7), avg(relnatts::float4), sum(relnatts::float8) FROM pg_class WHERE oid < 10000"
            ),
            ["3.8527131782945736", "994", "0.55038759689922480742", "3.852713178294574", "994"]
        );
        assert_eq!(
            all(
                "SELECT sum(2147483647), sum(9223372036854775807), avg(9223372036854775807) FROM pg_class WHERE oid < 10000"
            ),
            ["554050780926", "2379629985508532158206", "9223372036854775807"]
        );
        assert_eq!(
            all(
                "SELECT var_pop(relnatts::float8 ORDER BY oid), stddev_samp(relnatts::float8 ORDER BY oid) FROM pg_class WHERE oid < 10000"
            ),
            ["27.63722132083409", "5.267329413183231"]
        );
        assert_eq!(
            all(
                "SELECT var_samp(relnatts::float8), avg(relnatts::float8), sum(relnatts), count(*) FROM pg_class WHERE oid < 0"
            ),
            ["NULL", "NULL", "NULL", "0"]
        );
        assert_eq!(
            one("SELECT string_agg(relname, ',' ORDER BY relname) FROM pg_class WHERE oid < 1260"),
            "pg_attribute,pg_class,pg_default_acl,pg_default_acl_oid_index,pg_default_acl_role_nsp_obj_index,pg_foreign_data_wrapper_name_index,pg_foreign_data_wrapper_oid_index,pg_foreign_server_name_index,pg_foreign_server_oid_index,pg_proc,pg_shdepend,pg_shdepend_depender_index,pg_shdepend_reference_index,pg_tablespace,pg_type,pg_user_mapping_oid_index,pg_user_mapping_user_server_index"
        );
        let bytes = one(
            "SELECT string_agg(relname::bytea, '\\x00'::bytea ORDER BY relname) FROM pg_class WHERE oid < 1250",
        );
        assert!(
            bytes.starts_with("\\x70675f6174747269627574650070675f64656661756c745f61636c00"),
            "{bytes}"
        );
        assert!(
            bytes.ends_with("0070675f757365725f6d617070696e675f757365725f7365727665725f696e646578"),
            "{bytes}"
        );
        assert_eq!(
            all(
                "SELECT array_agg(DISTINCT relkind ORDER BY relkind DESC), array_agg(DISTINCT nullif(relnatts, 3)) FROM pg_class WHERE oid < 10000"
            ),
            ["{t,r,i}", "{1,2,4,5,6,7,8,9,11,12,15,18,19,21,22,23,25,28,30,31,32,34,NULL}"]
        );
        assert_eq!(
            all(
                "SELECT count(DISTINCT relkind), count(*) FILTER (WHERE relkind = 'r'), sum(DISTINCT relnatts) FROM pg_class WHERE oid < 10000"
            ),
            ["3", "64", "366"]
        );
        assert_eq!(
            all(
                "SELECT sum(CASE WHEN oid = 1259 THEN 'NaN'::numeric ELSE 1 END), sum(CASE WHEN oid = 1259 THEN 'Infinity'::numeric WHEN oid = 1255 THEN '-Infinity' ELSE 1 END), avg(CASE WHEN oid = 1259 THEN '-Infinity'::numeric ELSE 1 END) FROM pg_class WHERE oid < 10000"
            ),
            ["NaN", "NaN", "-Infinity"]
        );
        // For two equal values, larger and smaller give the second.
        assert_eq!(
            all(
                "SELECT numeric_larger(1.0, 1.00), numeric_smaller(1.0, 1.00), float8larger(0, -0.0::float8), float8smaller(0, -0.0::float8), int4_sum(NULL, 3), booland_statefunc(true, false)"
            ),
            ["1.00", "1.00", "-0", "-0", "3", "f"]
        );
        assert_eq!(
            ordered(
                "SELECT relkind, count(*), max(relnatts), min(oid) FROM pg_class WHERE oid < 10000 GROUP BY relkind ORDER BY 1"
            ),
            ["i|159|4|112", "r|64|34|826", "t|35|3|2336"]
        );
        assert_eq!(
            ordered(
                "SELECT relkind FROM pg_class WHERE oid < 10000 GROUP BY relkind HAVING count(*) > 100 ORDER BY 1"
            ),
            ["i"]
        );
        assert_eq!(
            ordered(
                "SELECT relnatts % 3 AS m, count(*) FROM pg_class WHERE oid < 10000 GROUP BY relnatts % 3 ORDER BY 1"
            ),
            ["0|74", "1|97", "2|87"]
        );
        assert_eq!(
            ordered(
                "SELECT c.relname, count(a.attnum) FROM pg_class c JOIN pg_attribute a ON a.attrelid = c.oid WHERE c.oid < 1250 GROUP BY c.oid ORDER BY 1 LIMIT 3"
            ),
            ["pg_attribute|31", "pg_default_acl|11", "pg_default_acl_oid_index|1"]
        );
        assert_eq!(
            ordered(
                "SELECT relkind, count(*) FROM pg_class WHERE oid < 10000 GROUP BY 1 ORDER BY sum(relnatts) DESC"
            ),
            ["r|64", "i|159", "t|35"]
        );
        assert_eq!(
            ordered("SELECT count(*) FROM pg_class WHERE oid < 0 GROUP BY relkind"),
            [""; 0]
        );
        assert_eq!(one("SELECT count(*) FROM pg_class WHERE oid < 0 GROUP BY ()"), "0");
        assert_eq!(ordered("SELECT 1 HAVING false"), [""; 0]);
        assert_eq!(one("SELECT 1 HAVING true"), "1");
        assert_eq!(error("SELECT sum(1e308::float8) FROM pg_class WHERE oid < 10000"), "22003");
        assert_eq!(
            rows("SELECT count(*) FROM pg_class GROUP BY relfrozenxid, relname::text::varbit"),
            Err(("0A000".to_string(), "could not implement GROUP BY".to_string()))
        );
    });
}
