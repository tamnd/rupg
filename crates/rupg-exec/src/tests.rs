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

/// The text of each column of each row of a query, `NULL` for a null value, or the error.
fn run(sql: &str) -> Result<Vec<Vec<String>>> {
    let session = TestSession { zone: FixedZone::utc() };
    let (stmts, _) = rupg_sql::parse(sql).map_err(rupg_common::Error::from)?;
    let Some(Some(Node::RawStmt(raw))) = stmts.first() else { panic!("no statement in {sql}") };
    let query = analyze(raw.stmt.as_ref().expect("a statement"), &session, &Params::default())?;
    let plan = prepare(query)?;
    let rows = plan.run(&[], &session)?;
    let mut texts = Vec::new();
    for values in rows {
        let mut row = Vec::new();
        for (target, value) in plan.columns().iter().zip(values) {
            if value.is_null() {
                row.push("NULL".to_string());
                continue;
            }
            let mut out = Vec::new();
            rupg_func::output(target.expr.ty, &value, &session, &mut out)?;
            row.push(String::from_utf8(out).expect("utf8"));
        }
        texts.push(row);
    }
    Ok(texts)
}

/// The text of each column of each row of a query, `NULL` for a null value, or the SQLSTATE and the message of the error.
fn rows(sql: &str) -> std::result::Result<Vec<Vec<String>>, (String, String)> {
    run(sql).map_err(|e| (e.state().as_str().to_string(), e.message().to_string()))
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
        // `xid` has `=` and no order, so the groups use only `xideq`. The catalog of rupg has other values of `relfrozenxid` than an oracle after `initdb`, so the test compares two forms of the same count.
        assert_eq!(one("SELECT relfrozenxid = relfrozenxid FROM pg_class WHERE oid = 1259"), "t");
        assert_eq!(
            ordered("SELECT count(*) FROM pg_class WHERE oid < 10000 GROUP BY relfrozenxid").len(),
            ordered("SELECT DISTINCT relfrozenxid::text FROM pg_class WHERE oid < 10000").len()
        );
        assert_eq!(error("SELECT count(DISTINCT relfrozenxid) FROM pg_class"), "42883");
        assert_eq!(
            rows("SELECT count(*) FROM pg_class GROUP BY relfrozenxid, relname::text::varbit"),
            Err(("0A000".to_string(), "could not implement GROUP BY".to_string()))
        );
    });
}

#[test]
fn subqueries() {
    big_stack(|| {
        // The results come from PostgreSQL 19.
        assert_eq!(
            one(
                "SELECT (SELECT count(*) FROM pg_attribute a WHERE a.attrelid = c.oid AND a.attnum > 0) FROM pg_class c WHERE c.relname = 'pg_class'"
            ),
            "34"
        );
        assert_eq!(
            ordered(
                "SELECT typname FROM pg_type t WHERE EXISTS (SELECT 1 FROM pg_type e WHERE e.oid = t.typelem AND e.typname = 'int4') ORDER BY 1"
            ),
            ["_int4"]
        );
        assert_eq!(
            ordered(
                "SELECT amname FROM pg_am WHERE oid IN (SELECT opcmethod FROM pg_opclass WHERE opcname = 'int4_ops') ORDER BY 1"
            ),
            ["btree", "hash"]
        );
        assert_eq!(
            ordered(
                "SELECT amname FROM pg_am WHERE oid NOT IN (SELECT opcmethod FROM pg_opclass) ORDER BY 1"
            ),
            ["heap"]
        );
        assert_eq!(
            one("SELECT ARRAY(SELECT amname FROM pg_am ORDER BY 1)"),
            "{brin,btree,gin,gist,hash,heap,spgist}"
        );
        assert_eq!(
            row(
                "SELECT 4 = ALL (SELECT typlen FROM pg_type WHERE typname IN ('int4','oid')), 4 = ALL (SELECT typlen FROM pg_type WHERE typname IN ('int4','int8'))"
            ),
            Ok(Some(vec!["t".to_string(), "f".to_string()]))
        );
        assert_eq!(
            ordered(
                "SELECT 1 = ANY (SELECT NULL::int) IS NULL, 1 = ALL (SELECT 1 WHERE false), 1 = ANY (SELECT 1 WHERE false), 1 NOT IN (SELECT NULL::int) IS NULL"
            ),
            ["t|t|f|t"]
        );
        assert_eq!(
            one("SELECT 3 < ANY (SELECT typlen FROM pg_type WHERE typname IN ('int2','int4'))"),
            "t"
        );
        assert_eq!(
            ordered(
                "SELECT amtype, (SELECT count(*) FROM pg_am a2 WHERE a2.amtype = a.amtype) FROM pg_am a GROUP BY amtype ORDER BY 1"
            ),
            ["i|6", "t|1"]
        );
        assert_eq!(
            ordered(
                "SELECT a.amname, (SELECT count(*) FROM pg_opclass o WHERE o.opcmethod = a.oid) FROM pg_am a ORDER BY 1"
            ),
            ["brin|72", "btree|45", "gin|4", "gist|9", "hash|42", "heap|0", "spgist|7"]
        );
        assert_eq!(
            ordered(
                "SELECT a.amname FROM pg_am a WHERE EXISTS (SELECT 1 FROM pg_opclass o WHERE o.opcmethod = a.oid AND EXISTS (SELECT 1 FROM pg_type t WHERE t.oid = o.opcintype AND t.typname = 'int4' AND a.amname = 'hash'))"
            ),
            ["hash"]
        );
        assert_eq!(
            ordered(
                "SELECT (SELECT a2.amname FROM pg_am a2 ORDER BY 1 OFFSET a.oid::int - a.oid::int LIMIT 1) FROM pg_am a LIMIT 1"
            ),
            ["brin"]
        );
        assert_eq!(
            ordered(
                "SELECT amtype FROM pg_am GROUP BY amtype HAVING count(*) > (SELECT 1) ORDER BY 1"
            ),
            ["i"]
        );
        assert_eq!(
            ordered(
                "SELECT a.amname, (SELECT max(o.opcname || a.amname) FROM pg_opclass o WHERE o.opcmethod = a.oid) FROM pg_am a WHERE a.amname = 'hash'"
            ),
            ["hash|xid_opshash"]
        );
        assert_eq!(
            ordered(
                "SELECT a.amname FROM pg_am a JOIN pg_am b ON a.oid = b.oid AND EXISTS (SELECT 1 FROM pg_opclass o WHERE o.opcmethod = b.oid) ORDER BY 1"
            ),
            ["brin", "btree", "gin", "gist", "hash", "spgist"]
        );
        assert_eq!(
            ordered(
                "SELECT amname FROM pg_am a ORDER BY (SELECT count(*) FROM pg_opclass o WHERE o.opcmethod = a.oid) DESC, amname LIMIT 3"
            ),
            ["brin", "btree", "hash"]
        );
        assert_eq!(
            ordered(
                "SELECT pg_typeof((SELECT 'x')), pg_typeof(ARRAY(SELECT 'x')), pg_typeof(ARRAY(SELECT 1::int2))"
            ),
            ["text|text[]|smallint[]"]
        );
        assert_eq!(
            one(
                "SELECT (SELECT amname FROM pg_am WHERE oid = c.oid OR c.relname = 'x') FROM pg_class c WHERE c.relname = 'pg_am'"
            ),
            "NULL"
        );
        assert_eq!(one("SELECT (SELECT oid FROM pg_am WHERE false) IS NULL"), "t");
        assert_eq!(one("SELECT ARRAY(SELECT typname FROM pg_type WHERE false)"), "{}");
        assert_eq!(
            ordered(
                "SELECT ARRAY(SELECT oid::int - oid::int FROM pg_am WHERE amname IN ('heap','hash')), ARRAY(SELECT NULL::int)"
            ),
            ["{0,0}|{NULL}"]
        );
        assert_eq!(
            ordered(
                "SELECT (SELECT relname FROM pg_class WHERE relname = 'pg_am')::text, (SELECT c.relname FROM pg_class c WHERE oid = 1259)"
            ),
            ["pg_am|pg_class"]
        );
        assert_eq!(one("SELECT (SELECT relnatts FROM pg_class WHERE oid = 1259) + 1"), "35");
        assert_eq!(one("SELECT relname FROM pg_class c WHERE relname = (SELECT 'pg_am')"), "pg_am");
        assert_eq!(
            one(
                "SELECT count(*) FROM pg_am a WHERE a.oid > ALL (SELECT o.opcmethod FROM pg_opclass o WHERE o.opcname = 'int4_ops')"
            ),
            "4"
        );
        assert_eq!(one("SELECT 1 FROM pg_am GROUP BY (SELECT 1)"), "1");
        assert_eq!(one("SELECT 1 FROM pg_am a WHERE (SELECT a.oid) = 2 LIMIT (SELECT 1)"), "1");
        assert_eq!(ordered("SELECT (SELECT count(*)) FROM pg_am"), ["1"; 7]);
        assert_eq!(
            ordered("SELECT amname FROM pg_am a WHERE a.oid IN (SELECT a.oid) ORDER BY 1 LIMIT 2"),
            ["brin", "btree"]
        );
        assert_eq!(
            ordered(
                "SELECT amname FROM pg_am a WHERE amname = ANY (SELECT amname FROM pg_am b WHERE b.oid < a.oid)"
            ),
            [""; 0]
        );
        assert_eq!(
            ordered(
                "SELECT (SELECT b.amname FROM pg_am b WHERE b.oid = a.oid) FROM pg_am a ORDER BY 1 LIMIT 2"
            ),
            ["brin", "btree"]
        );
        // simplify_EXISTS_query: with no aggregate, `HAVING` or `OFFSET`, EXISTS does not compute the target list.
        assert_eq!(one("SELECT EXISTS (SELECT 1/0 FROM pg_am WHERE false)"), "f");
        assert_eq!(one("SELECT EXISTS (SELECT 1/(oid::int - oid::int) FROM pg_am)"), "t");
        assert_eq!(one("SELECT 1 WHERE EXISTS (SELECT 1/0 FROM pg_am)"), "1");
        assert_eq!(one("SELECT EXISTS (SELECT 1/0)"), "t");
        assert_eq!(
            ordered(
                "SELECT EXISTS (SELECT 1/(oid::int - oid::int) FROM pg_am LIMIT 1), EXISTS (SELECT 1/(oid::int - oid::int) FROM pg_am LIMIT NULL)"
            ),
            ["t|t"]
        );
        assert_eq!(one("SELECT EXISTS (SELECT 1/(oid::int - oid::int) FROM pg_am LIMIT 0)"), "f");
        assert_eq!(
            error("SELECT EXISTS (SELECT 1/(oid::int - oid::int) FROM pg_am OFFSET 0)"),
            "22012"
        );
        assert_eq!(error("SELECT EXISTS (SELECT count(*)/0 FROM pg_am)"), "22012");
        assert_eq!(one("SELECT (SELECT 1/(oid::int - oid::int) FROM pg_am WHERE false)"), "NULL");
        assert_eq!(
            rows("SELECT (SELECT oid FROM pg_am)"),
            Err((
                "21000".to_string(),
                "more than one row returned by a subquery used as an expression".to_string()
            ))
        );
        // rupg does not take an aggregate of an outer query yet. PostgreSQL gives 2 here.
        assert_eq!(
            error(
                "SELECT (SELECT max(a.oid) FROM pg_type LIMIT 1) FROM pg_am a WHERE a.amname = 'heap'"
            ),
            "0A000"
        );
    });
}

/// The value of a query of one row and one column, `NULL`, or the error as `ERROR`, the SQLSTATE, the message and the hint.
fn outcome(sql: &str) -> String {
    match run(sql) {
        Ok(mut rows) if rows.len() == 1 && rows[0].len() == 1 => rows[0].remove(0),
        Ok(rows) => panic!("{sql} gave {rows:?}"),
        Err(e) => {
            let mut text = format!("ERROR {} {}", e.state().as_str(), e.message());
            if let Some(hint) = e.hint() {
                text.push_str(" HINT ");
                text.push_str(hint);
            }
            text
        }
    }
}

/// The input and output of the OID alias types and the functions `to_regclass` and the others. Each result is the result of PostgreSQL 19 for `SELECT` and the expression, with `search_path` at its default.
#[test]
fn oid_alias_types() {
    const CASES: &[(&str, &str)] = &[
        ("'pg_class'::regclass", "pg_class"),
        ("2836::regclass", "pg_toast.pg_toast_1255"),
        ("0::regclass", "-"),
        ("99999::regclass", "99999"),
        ("'pg_catalog.pg_class'::regclass::oid", "1259"),
        ("'postgres.pg_catalog.pg_class'::regclass", "pg_class"),
        ("'\"pg_class\"'::regclass", "pg_class"),
        ("'pg_toast.pg_toast_1255'::regclass::oid", "2836"),
        ("'-'::regclass::oid", "0"),
        ("'int4'::regtype", "integer"),
        ("'integer[]'::regtype", "integer[]"),
        ("'varchar(10)'::regtype", "character varying"),
        ("'double precision'::regtype", "double precision"),
        ("'\"char\"'::regtype", "\"char\""),
        ("'pg_catalog.int8'::regtype", "bigint"),
        ("'timestamptz'::regtype", "timestamp with time zone"),
        ("'varbit'::regtype", "bit varying"),
        ("0::regtype", "-"),
        ("'now'::regproc", "now"),
        ("'pg_catalog.now'::regproc::oid", "1299"),
        ("2::regproc", "2"),
        ("'now()'::regprocedure", "now()"),
        ("'abs(int4)'::regprocedure", "abs(integer)"),
        ("'lower(text)'::regprocedure", "lower(text)"),
        ("'abs( integer )'::regprocedure::oid", "1397"),
        ("'pg_catalog.abs(int4)'::regprocedure::oid", "1397"),
        ("1397::regprocedure", "abs(integer)"),
        ("'||/'::regoper", "||/"),
        ("'||/'::regoper::oid", "597"),
        ("0::regoper", "0"),
        ("551::regoper", "pg_catalog.+"),
        ("'+(int4,int4)'::regoperator", "+(integer,integer)"),
        ("'-(NONE,int4)'::regoperator", "-(NONE,integer)"),
        ("'-(none, integer)'::regoperator::oid", "558"),
        ("96::regoperator", "=(integer,integer)"),
        ("0::regoperator", "0"),
        ("'pg_catalog'::regnamespace", "pg_catalog"),
        ("'pg_toast'::regnamespace::oid", "99"),
        ("12345::regnamespace", "12345"),
        ("'postgres'::regrole", "postgres"),
        ("'pg_monitor'::regrole::oid", "3373"),
        ("10::regrole", "postgres"),
        ("'\"C\"'::regcollation", "\"C\""),
        ("'\"C\"'::regcollation::oid", "950"),
        ("'pg_catalog.\"POSIX\"'::regcollation::oid", "951"),
        ("'ucs_basic'::regcollation::oid", "962"),
        ("100::regcollation", "\"default\""),
        ("'simple'::regconfig::oid", "3748"),
        ("3748::regconfig", "simple"),
        ("'simple'::regdictionary::oid", "3765"),
        ("3765::regdictionary", "simple"),
        ("'template1'::regdatabase::oid", "1"),
        ("1::regdatabase", "template1"),
        ("'postgres'::regdatabase::oid", "5"),
        ("to_regclass('pg_class')::oid", "1259"),
        ("to_regclass('nosuch')", "NULL"),
        ("to_regclass('nosch.nosuch')", "NULL"),
        ("to_regclass('a..b')", "NULL"),
        ("to_regclass('09')", "NULL"),
        ("to_regtype('int4')", "integer"),
        ("to_regtype('nosuch')", "NULL"),
        ("to_regtype('nosch.nosuch')", "NULL"),
        ("to_regtypemod('varchar(10)')", "14"),
        ("to_regtypemod('int4')", "-1"),
        ("to_regtypemod('nosuch')", "NULL"),
        ("to_regtypemod('numeric(5,2)')", "327686"),
        ("to_regtypemod('interval year to month')", "458751"),
        ("to_regtypemod('timestamp(3)')", "3"),
        ("to_regtypemod('char')", "5"),
        ("to_regtypemod('bit(3)')", "3"),
        ("to_regproc('abs')", "NULL"),
        ("to_regproc('now')::oid", "1299"),
        ("to_regproc('nosuch')", "NULL"),
        ("to_regprocedure('abs(int4)')::oid", "1397"),
        ("to_regprocedure('abs(text)')", "NULL"),
        ("to_regprocedure('abs')", "NULL"),
        ("to_regprocedure('abs(nosuch)')", "NULL"),
        ("to_regoper('+')", "NULL"),
        ("to_regoper('||/')::oid", "597"),
        ("to_regoperator('+(int4,int4)')::oid", "551"),
        ("to_regoperator('+(int4)')", "NULL"),
        ("to_regoperator('+(int4,int4,int4)')", "NULL"),
        ("to_regoperator('+(text,text)')", "NULL"),
        ("to_regnamespace('pg_catalog')::oid", "11"),
        ("to_regnamespace('nosuch')", "NULL"),
        ("to_regnamespace('a.b')", "NULL"),
        ("to_regrole('postgres')::oid", "10"),
        ("to_regrole('public')", "NULL"),
        ("to_regcollation('\"C\"')::oid", "950"),
        ("to_regcollation('C')", "NULL"),
        ("to_regdatabase('template1')::oid", "1"),
        ("to_regdatabase('nosuch')", "NULL"),
        ("'pg_class'::text::regclass", "pg_class"),
        ("1259::regclass::text", "pg_class"),
        ("1259::regclass::oid::int4", "1259"),
        ("1259::int8::regclass", "pg_class"),
        ("(-1)::regclass", "4294967295"),
        ("'pg_class'::regclass = 1259", "t"),
        ("'nosuch'::regclass", "ERROR 42P01 relation \"nosuch\" does not exist"),
        ("'nosch.nosuch'::regclass", "ERROR 42P01 relation \"nosch.nosuch\" does not exist"),
        ("'\"PG_CLASS\"'::regclass", "ERROR 42P01 relation \"PG_CLASS\" does not exist"),
        (
            "'a.b.c.d'::regclass",
            "ERROR 42601 improper relation name (too many dotted names): a.b.c.d",
        ),
        (
            "'x.pg_catalog.pg_class'::regclass",
            "ERROR 0A000 cross-database references are not implemented: \"x.pg_catalog.pg_class\"",
        ),
        ("'a..b'::regclass", "ERROR 42602 invalid name syntax"),
        ("''::regclass", "ERROR 42602 invalid name syntax"),
        ("'nosuch'::regtype", "ERROR 42704 type \"nosuch\" does not exist"),
        ("'nosch.nosuch'::regtype", "ERROR 3F000 schema \"nosch\" does not exist"),
        ("'int4 int4'::regtype", "ERROR 42601 syntax error at or near \"int4\""),
        ("' '::regtype", "ERROR 42601 invalid type name \" \""),
        ("'setof int4'::regtype", "ERROR 42601 invalid type name \"setof int4\""),
        ("'varchar(0)'::regtype", "ERROR 22023 length for type varchar must be at least 1"),
        ("'void[]'::regtype", "ERROR 42704 type \"void[]\" does not exist"),
        ("'abs'::regproc", "ERROR 42725 more than one function named \"abs\""),
        ("'nosuch'::regproc", "ERROR 42883 function \"nosuch\" does not exist"),
        ("'abs'::regprocedure", "ERROR 22P02 expected a left parenthesis"),
        ("'abs(int4'::regprocedure", "ERROR 22P02 expected a right parenthesis"),
        ("'abs(int4)x'::regprocedure", "ERROR 22P02 expected a right parenthesis"),
        ("'abs(int4,)'::regprocedure", "ERROR 22P02 expected a type name"),
        ("'abs(nosuch)'::regprocedure", "ERROR 42704 type \"nosuch\" does not exist"),
        ("'abs(text)'::regprocedure", "ERROR 42883 function \"abs(text)\" does not exist"),
        ("'+'::regoper", "ERROR 42725 more than one operator named +"),
        ("'!'::regoper", "ERROR 42883 operator does not exist: !"),
        (
            "'+(int4)'::regoperator",
            "ERROR 42P02 missing argument HINT Use NONE to denote the missing argument of a unary operator.",
        ),
        (
            "'+(int4,int4,int4)'::regoperator",
            "ERROR 54023 too many arguments HINT Provide two argument types for operator.",
        ),
        ("'+(text,text)'::regoperator", "ERROR 42883 operator does not exist: +(text,text)"),
        ("'nosuch'::regnamespace", "ERROR 3F000 schema \"nosuch\" does not exist"),
        ("'a.b'::regnamespace", "ERROR 42602 invalid name syntax"),
        ("'nosuch'::regrole", "ERROR 42704 role \"nosuch\" does not exist"),
        ("'public'::regrole", "ERROR 42704 role \"public\" does not exist"),
        ("'C'::regcollation", "ERROR 42704 collation \"c\" for encoding \"UTF8\" does not exist"),
        ("'nosuch'::regconfig", "ERROR 42704 text search configuration \"nosuch\" does not exist"),
        ("'nosuch'::regdictionary", "ERROR 42704 text search dictionary \"nosuch\" does not exist"),
        ("'nosuch'::regdatabase", "ERROR 42704 database \"nosuch\" does not exist"),
        ("'x'::text::regclass", "ERROR 42P01 relation \"x\" does not exist"),
        ("'nosch.x'::text::regclass", "ERROR 3F000 schema \"nosch\" does not exist"),
        ("to_regtype('int4 int4')", "ERROR 42601 syntax error at or near \"int4\""),
        (
            "to_regclass('a.b.c.d')",
            "ERROR 42601 improper relation name (too many dotted names): a.b.c.d",
        ),
        (
            "to_regclass('x.pg_catalog.pg_class')",
            "ERROR 0A000 cross-database references are not implemented: \"x.pg_catalog.pg_class\"",
        ),
        ("to_regtype('varchar(0)')", "ERROR 22023 length for type varchar must be at least 1"),
        ("'abs(\"int4)'::regprocedure", "ERROR 22P02 improper type name"),
        ("'abs(numeric(1)'::regprocedure", "ERROR 22P02 improper type name"),
        ("'abs(int4))'::regprocedure", "ERROR 22P02 improper type name"),
        ("'abs()'::regprocedure", "ERROR 42883 function \"abs()\" does not exist"),
        ("'now( )'::regprocedure::oid", "1299"),
        ("'pg_catalog.+'::regoper", "ERROR 42725 more than one operator named pg_catalog.+"),
        ("'nosch.+'::regoper", "ERROR 42883 operator does not exist: nosch.+"),
        (
            "'a.b.c.d'::regproc",
            "ERROR 42601 improper qualified name (too many dotted names): a.b.c.d",
        ),
        (
            "'x.pg_catalog.now'::regproc",
            "ERROR 0A000 cross-database references are not implemented: x.pg_catalog.now",
        ),
    ];
    big_stack(|| {
        for (expr, expected) in CASES {
            assert_eq!(outcome(&format!("SELECT {expr}")), *expected, "{expr}");
        }
    });
}

/// The catalog information functions: the `pg_*_is_visible` functions, `pg_get_userbyid`, `obj_description`, `col_description` and `shobj_description`. Each result is the result of PostgreSQL 19 for `SELECT` and the text, with `search_path` at its default. The counts use only the OIDs below 10000, because the scripts of initdb add more rows.
#[test]
fn catalog_information_functions() {
    const CASES: &[(&str, &str)] = &[
        ("pg_table_is_visible(1259)", "t"),
        ("pg_table_is_visible('pg_class'::regclass)", "t"),
        ("pg_table_is_visible(2604)", "t"),
        ("pg_table_is_visible(0)", "NULL"),
        ("pg_table_is_visible(99999)", "NULL"),
        ("pg_table_is_visible(NULL)", "NULL"),
        ("pg_table_is_visible('pg_toast.pg_toast_1255'::regclass)", "f"),
        ("pg_type_is_visible(23)", "t"),
        ("pg_type_is_visible('int4'::regtype)", "t"),
        ("pg_type_is_visible(0)", "NULL"),
        ("pg_type_is_visible(99999)", "NULL"),
        ("pg_type_is_visible(1007)", "t"),
        ("pg_type_is_visible('pg_class'::regtype)", "t"),
        ("pg_function_is_visible(177)", "t"),
        ("pg_function_is_visible('int4pl'::regproc)", "t"),
        ("pg_function_is_visible(0)", "NULL"),
        ("pg_function_is_visible(99999)", "NULL"),
        ("pg_operator_is_visible(551)", "t"),
        ("pg_operator_is_visible(96)", "t"),
        ("pg_operator_is_visible(0)", "NULL"),
        ("pg_operator_is_visible(99999)", "NULL"),
        ("pg_opclass_is_visible(1978)", "t"),
        ("pg_opclass_is_visible(99999)", "NULL"),
        ("pg_opfamily_is_visible(1976)", "t"),
        ("pg_opfamily_is_visible(99999)", "NULL"),
        ("pg_conversion_is_visible(4402)", "NULL"),
        ("pg_conversion_is_visible(99999)", "NULL"),
        ("pg_ts_parser_is_visible(3722)", "t"),
        ("pg_ts_parser_is_visible(99999)", "NULL"),
        ("pg_ts_template_is_visible(3727)", "t"),
        ("pg_ts_template_is_visible(99999)", "NULL"),
        ("pg_ts_config_is_visible(3748)", "t"),
        ("pg_ts_config_is_visible(99999)", "NULL"),
        ("pg_ts_dict_is_visible(3765)", "t"),
        ("pg_ts_dict_is_visible(99999)", "NULL"),
        ("pg_collation_is_visible(100)", "t"),
        ("pg_collation_is_visible(950)", "t"),
        ("pg_collation_is_visible(99999)", "NULL"),
        ("pg_statistics_obj_is_visible(99999)", "NULL"),
        ("pg_statistics_obj_is_visible(0)", "NULL"),
        ("pg_get_userbyid(10)", "postgres"),
        ("pg_get_userbyid(0)", "unknown (OID=0)"),
        ("pg_get_userbyid(99999)", "unknown (OID=99999)"),
        ("pg_get_userbyid(NULL)", "NULL"),
        ("pg_typeof(pg_get_userbyid(10))", "name"),
        ("obj_description(1259, 'pg_class')", "NULL"),
        ("obj_description(11, 'pg_namespace')", "system catalog schema"),
        ("obj_description(2200, 'pg_namespace')", "standard public schema"),
        ("obj_description(99, 'pg_namespace')", "reserved schema for TOAST tables"),
        ("obj_description(23, 'pg_type')", "-2 billion to 2 billion integer, 4-byte storage"),
        ("obj_description('int4pl'::regproc, 'pg_proc')", "implementation of + operator"),
        ("obj_description(551, 'pg_operator')", "add"),
        ("obj_description(403, 'pg_am')", "b-tree index access method"),
        ("obj_description(23, 'pg_proc')", "NULL"),
        ("obj_description(23, 'no_such_catalog')", "NULL"),
        ("obj_description(23, NULL)", "NULL"),
        ("obj_description(NULL, 'pg_type')", "NULL"),
        ("obj_description(1, 'pg_database')", "NULL"),
        ("obj_description(23)", "-2 billion to 2 billion integer, 4-byte storage"),
        ("obj_description(551)", "add"),
        ("obj_description(11)", "system catalog schema"),
        ("obj_description(99999)", "NULL"),
        ("obj_description(0)", "NULL"),
        ("pg_typeof(obj_description(23))", "text"),
        ("col_description(1259, 1)", "NULL"),
        ("col_description(1259, 0)", "NULL"),
        ("col_description(99999, 1)", "NULL"),
        ("col_description(1259, NULL)", "NULL"),
        ("shobj_description(1, 'pg_database')", "default template for new databases"),
        ("shobj_description(10, 'pg_authid')", "NULL"),
        ("shobj_description(1, 'pg_class')", "NULL"),
        ("shobj_description(99999, 'pg_database')", "NULL"),
        ("shobj_description(1, NULL)", "NULL"),
        (
            "obj_description(oid, 'pg_proc') FROM pg_proc WHERE proname = 'int4pl'",
            "implementation of + operator",
        ),
        ("count(*) FROM pg_type WHERE oid < 10000 AND pg_type_is_visible(oid)", "203"),
        ("count(*) FROM pg_class WHERE oid < 10000 AND pg_table_is_visible(oid)", "188"),
        ("count(*) FROM pg_proc WHERE oid < 1000 AND pg_function_is_visible(oid)", "580"),
        ("count(*) FROM pg_operator WHERE oid < 1000 AND pg_operator_is_visible(oid)", "284"),
        ("count(*) FROM pg_opclass WHERE pg_opclass_is_visible(oid)", "179"),
        ("count(*) FROM pg_opfamily WHERE pg_opfamily_is_visible(oid)", "148"),
        ("count(*) FROM pg_conversion WHERE pg_conversion_is_visible(oid)", "98"),
        (
            "count(*) FROM pg_proc WHERE oid < 1000 AND obj_description(oid, 'pg_proc') IS NOT NULL",
            "580",
        ),
        (
            "count(*) FROM pg_type WHERE obj_description(oid, 'pg_type') IS NOT NULL AND oid < 1000",
            "36",
        ),
        ("count(*) FROM pg_operator WHERE obj_description(oid) IS NOT NULL AND oid < 1000", "284"),
    ];
    big_stack(|| {
        for (text, expected) in CASES {
            assert_eq!(outcome(&format!("SELECT {text}")), *expected, "{text}");
        }
        assert_eq!(
            row(
                "SELECT 'int4'::regtype, pg_type_is_visible('int4'::regtype), obj_description('int4'::regtype, 'pg_type')"
            ),
            Ok(Some(vec![
                "integer".into(),
                "t".into(),
                "-2 billion to 2 billion integer, 4-byte storage".into()
            ]))
        );
    });
}

/// The place of an error in the input of an OID alias type. An error of a literal is at the literal, as `pcb_error_callback` gives it. A syntax error in a type name at run time is at its place in the type name, with the line of `CONTEXT`. The places are the places of PostgreSQL 19, from 0.
#[test]
fn oid_alias_type_error_places() {
    big_stack(|| {
        let place = |sql: &str| {
            let e = run(sql).expect_err(sql);
            (e.position(), e.context().map(str::to_string))
        };
        let context = Some("invalid type name \"int4 int4\"".to_string());
        assert_eq!(place("SELECT 'nosuch'::regclass"), (Some(7), None));
        assert_eq!(place("SELECT 1, 'int4 int4'::regtype"), (Some(10), context.clone()));
        assert_eq!(place("SELECT 1, 'a.b.c.d'::regclass"), (Some(10), None));
        assert_eq!(place("SELECT 1, 'abs(int4 int4)'::regprocedure"), (Some(10), context.clone()));
        assert_eq!(place("SELECT 1, to_regtype('int4 int4')"), (Some(5), context));
        assert_eq!(place("SELECT 1, to_regclass('a.b.c.d')"), (None, None));
        assert_eq!(place("SELECT 1, 'varchar(0)'::regtype"), (Some(10), None));
        assert_eq!(place("SELECT 1, to_regtype('varchar(0)')"), (None, None));
    });
}
