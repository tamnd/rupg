//! The expected values come from PostgreSQL 19beta4 after `initdb --locale=C`. The cases of [`CASES`] ran in order in a new database, and each statement that failed changed nothing.

use rupg_catalog::{Catalog, ConKind};
use rupg_sql::nodes::Node;

use super::{Message, define};
use crate::tests::{TestEnv, big_stack};

/// A message before the result: `W` for a warning or `N` for a notice, the SQLSTATE, the message and the byte offset.
type Note = (char, &'static str, &'static str, Option<usize>);
/// An error: the SQLSTATE, the message, the byte offset, the detail and the hint.
type Failure =
    (&'static str, &'static str, Option<usize>, Option<&'static str>, Option<&'static str>);
/// A statement, its messages and its error.
type Case = (&'static str, &'static [Note], Option<Failure>);

/// The role `postgres`, a superuser.
const POSTGRES: u32 = 10;

/// Runs one statement on the catalog. After an error the catalog stays as it was, but the OIDs that the statement used stay used.
fn run(catalog: &mut Catalog, sql: &str) -> (Vec<Note>, Option<rupg_common::Error>) {
    let stmts = match rupg_sql::parse(sql) {
        Ok((stmts, _)) => stmts,
        Err(e) => {
            let at = e.location;
            return (Vec::new(), Some(rupg_common::Error::from(e).with_position(at)));
        }
    };
    let Some(Some(Node::RawStmt(raw))) = stmts.first() else { panic!("no statement in {sql}") };
    let stmt = raw.stmt.as_ref().expect("a statement");
    let mut work = catalog.clone();
    let defined = define(stmt, &TestEnv, &mut work, POSTGRES);
    let notes = defined
        .messages
        .iter()
        .map(|m| {
            let (kind, e) = match m {
                Message::Warning(e) => ('W', e),
                Message::Notice(e) => ('N', e),
            };
            let state: &'static str = e.state().as_str().to_string().leak();
            (kind, state, &*e.message().to_string().leak(), e.position())
        })
        .collect();
    match defined.result {
        Ok(()) => {
            *catalog = work;
            (notes, None)
        }
        Err(e) => {
            catalog.advance_oid(work.next_oid());
            (notes, Some(e))
        }
    }
}

/// The OIDs of the constraints of a table, in OID order, with their names and kinds.
fn constraints(catalog: &Catalog, table: &str) -> Vec<(String, char)> {
    let rel = catalog.relation_by_name(rupg_catalog::PUBLIC, table).expect(table);
    let mut list: Vec<_> = catalog.constraints_of(rel.oid).collect();
    list.sort_by_key(|c| c.oid);
    list.iter().map(|c| (c.name.clone(), c.kind.code())).collect()
}

#[test]
fn postgres_cases() {
    big_stack(|| {
        let mut catalog = Catalog::new();
        for &(sql, notes, failure) in CASES {
            let (got_notes, error) = run(&mut catalog, sql);
            // rupg has no system views yet, so the view pg_tables does not exist.
            if sql.contains("pg_tables") {
                assert_eq!(
                    error.map(|e| e.state().as_str().to_string()).as_deref(),
                    Some("42P01"),
                    "{sql}"
                );
                continue;
            }
            assert_eq!(got_notes, notes, "{sql}");
            match (error, failure) {
                (None, None) => {}
                (Some(e), Some((state, message, position, detail, hint))) => {
                    assert_eq!(
                        (e.state().as_str(), e.message(), e.position(), e.hint()),
                        (state, message, position, hint),
                        "{sql}"
                    );
                    // The detail of a duplicate key has the OID of the table, which depends on the earlier objects of the cluster.
                    let cut = |d: &str| d.split("=(").next().unwrap_or_default().to_string();
                    assert_eq!(e.detail().map(cut), detail.map(cut), "{sql}");
                }
                (e, f) => {
                    panic!("{sql}: got {:?}, expected {f:?}", e.map(|e| e.message().to_string()))
                }
            }
        }

        assert_eq!(
            constraints(&catalog, "t65"),
            [("t65_pkey".into(), 'c'), ("t65_a_not_null".into(), 'n'), ("t65_pkey1".into(), 'p')]
        );
        let t65 = catalog.relation_by_name(rupg_catalog::PUBLIC, "t65").unwrap().oid;
        let index = catalog.relations().find(|r| r.index.as_ref().is_some_and(|i| i.table == t65));
        assert_eq!(index.map(|r| r.name.as_str()), Some("t65_pkey1"));

        let operators = |table: &str| {
            let rel = catalog.relation_by_name(rupg_catalog::PUBLIC, table).unwrap().oid;
            let fk = catalog.constraints_of(rel).find(|c| c.kind == ConKind::Foreign).unwrap();
            let f = fk.foreign.as_ref().unwrap();
            (f.pf_eq.clone(), f.pp_eq.clone(), f.ff_eq.clone())
        };
        assert_eq!(operators("t99"), (vec![15], vec![96], vec![410]));
        assert_eq!(operators("t101"), (vec![98], vec![98], vec![98]));

        let index = catalog.relation_by_name(rupg_catalog::PUBLIC, "p_b_expr_idx").unwrap();
        let info = index.index.as_ref().unwrap();
        assert_eq!(info.collations, [950, 100]);
        assert_eq!(info.classes, [3126, 3126]);
        assert_eq!(info.options, [1, 0]);
        assert_eq!(info.keys, [2, 0]);

        let schema = catalog.schema_by_name("postgres").unwrap();
        assert_eq!(schema.owner, POSTGRES);
    });
}

/// The statements of the test of the catalog give the same OIDs and names as there.
#[test]
fn catalog_order() {
    big_stack(|| {
        let mut catalog = Catalog::new();
        let statements = [
            "CREATE TABLE t0 (a int)",
            "CREATE SCHEMA s",
            "CREATE TABLE s.t (a int PRIMARY KEY, b text NOT NULL DEFAULT 'x', c numeric(10,2) CHECK (c > 0), d int UNIQUE, e int REFERENCES s.t (a))",
            "CREATE INDEX t_b ON s.t (b)",
            "CREATE TABLE s.u (x serial, y int8)",
            "CREATE TABLE s.v (a int, b int, CHECK (a > b), CHECK (a > 0), CHECK (b > 0), UNIQUE (a, b), UNIQUE (a, b))",
            "CREATE INDEX ON s.v (a)",
            "CREATE INDEX ON s.v (a)",
            "CREATE INDEX ON s.v ((a + b))",
            "CREATE TABLE s.aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa (bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb serial PRIMARY KEY)",
            "CREATE TABLE s._w (a int)",
            "CREATE TABLE s.w (a int)",
            "CREATE TABLE s.p (a int CONSTRAINT pk PRIMARY KEY, b int CONSTRAINT nn NOT NULL CONSTRAINT ck CHECK (b > 1))",
        ];
        for sql in statements {
            let (notes, error) = run(&mut catalog, sql);
            assert!(
                notes.is_empty() && error.is_none(),
                "{sql}: {:?}",
                error.map(|e| e.message().to_string())
            );
        }
        let names: Vec<(u32, String)> = catalog
            .relations()
            .map(|r| (r.oid, r.name.clone()))
            .chain(catalog.constraints().map(|c| (c.oid, c.name.clone())))
            .filter(|(oid, _)| {
                [
                    16384, 16388, 16392, 16393, 16394, 16397, 16399, 16401, 16406, 16407, 16408,
                    16412, 16416, 16419, 16421, 16422, 16423, 16441, 16442, 16443, 16444,
                ]
                .contains(oid)
            })
            .collect();
        let mut names = names;
        names.sort();
        let expected = [
            (16384, "t0"),
            (16388, "t"),
            (16392, "t_c_check"),
            (16393, "t_a_not_null"),
            (16394, "t_b_not_null"),
            (16397, "t_pkey"),
            (16399, "t_d_key"),
            (16401, "t_e_fkey"),
            (16406, "t_b"),
            (16407, "u_x_seq"),
            (16408, "u"),
            (16412, "u_x_not_null"),
            (16416, "v_check"),
            (16419, "v_a_b_key"),
            (16421, "v_a_idx"),
            (16422, "v_a_idx1"),
            (16423, "v_expr_idx"),
            (16441, "ck"),
            (16442, "p_a_not_null"),
            (16443, "nn"),
            (16444, "pk"),
        ];
        let expected: Vec<(u32, String)> =
            expected.iter().map(|(o, n)| (*o, n.to_string())).collect();
        assert_eq!(names, expected);
        assert_eq!(catalog.depends().len(), 74);
    });
}

const CASES: &[Case] = &[
    ("CREATE TABLE p (a int PRIMARY KEY, b text UNIQUE, c int);", &[], None),
    (
        "CREATE TABLE p (a int);",
        &[],
        Some(("42P07", "relation \"p\" already exists", None, None, None)),
    ),
    (
        "CREATE TABLE IF NOT EXISTS p (a int);",
        &[('N', "42P07", "relation \"p\" already exists, skipping", None)],
        None,
    ),
    (
        "CREATE TABLE nosch.t (a int);",
        &[],
        Some(("3F000", "schema \"nosch\" does not exist", Some(13), None, None)),
    ),
    (
        "CREATE TABLE x.y.t (a int);",
        &[],
        Some((
            "0A000",
            "cross-database references are not implemented: \"x.y.t\"",
            Some(13),
            None,
            None,
        )),
    ),
    (
        "CREATE TABLE t (a int, a text);",
        &[],
        Some(("42701", "column \"a\" specified more than once", None, None, None)),
    ),
    (
        "CREATE TABLE t (ctid int);",
        &[],
        Some((
            "42701",
            "column name \"ctid\" conflicts with a system column name",
            None,
            None,
            None,
        )),
    ),
    (
        "CREATE TABLE t (a int, ctid int, b unknown);",
        &[],
        Some((
            "42701",
            "column name \"ctid\" conflicts with a system column name",
            None,
            None,
            None,
        )),
    ),
    (
        "CREATE TABLE t (a unknown);",
        &[],
        Some(("42P16", "column \"a\" has pseudo-type unknown", None, None, None)),
    ),
    (
        "CREATE TABLE t (a nosuchtype);",
        &[],
        Some(("42704", "type \"nosuchtype\" does not exist", Some(18), None, None)),
    ),
    (
        "CREATE TABLE t (a int DEFAULT 'x');",
        &[],
        Some(("22P02", "invalid input syntax for type integer: \"x\"", Some(30), None, None)),
    ),
    (
        "CREATE TABLE t (a int DEFAULT now());",
        &[],
        Some((
            "42804",
            "column \"a\" is of type integer but default expression is of type timestamp with time zone",
            None,
            None,
            Some("You will need to rewrite or cast the expression."),
        )),
    ),
    (
        "CREATE TABLE t (a int DEFAULT b);",
        &[],
        Some(("0A000", "cannot use column reference in DEFAULT expression", Some(30), None, None)),
    ),
    (
        "CREATE TABLE t (a int DEFAULT (SELECT 1));",
        &[],
        Some(("0A000", "cannot use subquery in DEFAULT expression", Some(30), None, None)),
    ),
    (
        "CREATE TABLE t (a int DEFAULT sum(1));",
        &[],
        Some((
            "42803",
            "aggregate functions are not allowed in DEFAULT expressions",
            Some(30),
            None,
            None,
        )),
    ),
    (
        "CREATE TABLE t (a int DEFAULT generate_series(1,2));",
        &[],
        Some((
            "0A000",
            "set-returning functions are not allowed in DEFAULT expressions",
            Some(30),
            None,
            None,
        )),
    ),
    (
        "CREATE TABLE t (a int DEFAULT row_number() over ());",
        &[],
        Some((
            "42P20",
            "window functions are not allowed in DEFAULT expressions",
            Some(30),
            None,
            None,
        )),
    ),
    (
        "CREATE TABLE t (a int CHECK (b > 0));",
        &[],
        Some(("42703", "column \"b\" does not exist", Some(29), None, None)),
    ),
    (
        "CREATE TABLE t (a int CHECK (a));",
        &[],
        Some((
            "42804",
            "argument of CHECK must be type boolean, not type integer",
            Some(29),
            None,
            None,
        )),
    ),
    (
        "CREATE TABLE t (a int CONSTRAINT c CHECK (a > 0), b int CONSTRAINT c CHECK (b > 0));",
        &[],
        Some(("42710", "check constraint \"c\" already exists", None, None, None)),
    ),
    (
        "CREATE TABLE t (a int PRIMARY KEY, b int PRIMARY KEY);",
        &[],
        Some((
            "42P16",
            "multiple primary keys for table \"t\" are not allowed",
            Some(41),
            None,
            None,
        )),
    ),
    (
        "CREATE TABLE t (a int, PRIMARY KEY (b));",
        &[],
        Some(("42703", "column \"b\" named in key does not exist", Some(23), None, None)),
    ),
    (
        "CREATE TABLE t (a int, UNIQUE (a, a));",
        &[],
        Some(("42701", "column \"a\" appears twice in unique constraint", Some(23), None, None)),
    ),
    (
        "CREATE TABLE t (a int, PRIMARY KEY (a, a));",
        &[],
        Some((
            "42701",
            "column \"a\" appears twice in primary key constraint",
            Some(23),
            None,
            None,
        )),
    ),
    (
        "CREATE TABLE t (a int NULL NOT NULL);",
        &[],
        Some((
            "42601",
            "conflicting NULL/NOT NULL declarations for column \"a\" of table \"t\"",
            Some(27),
            None,
            None,
        )),
    ),
    (
        "CREATE TABLE t (a int DEFAULT 1 DEFAULT 2);",
        &[],
        Some((
            "42601",
            "multiple default values specified for column \"a\" of table \"t\"",
            Some(32),
            None,
            None,
        )),
    ),
    (
        "CREATE TABLE t (a serial DEFAULT 1);",
        &[],
        Some((
            "42601",
            "multiple default values specified for column \"a\" of table \"t\"",
            None,
            None,
            None,
        )),
    ),
    (
        "CREATE TABLE t (a serial[]);",
        &[],
        Some(("0A000", "array of serial is not implemented", Some(18), None, None)),
    ),
    (
        "CREATE TABLE t (a int REFERENCES nosuch);",
        &[],
        Some(("42P01", "relation \"nosuch\" does not exist", None, None, None)),
    ),
    (
        "CREATE TABLE t (a int REFERENCES p (c));",
        &[],
        Some((
            "42830",
            "there is no unique constraint matching given keys for referenced table \"p\"",
            None,
            None,
            None,
        )),
    ),
    (
        "CREATE TABLE t (a text REFERENCES p);",
        &[],
        Some((
            "42804",
            "foreign key constraint \"t_a_fkey\" cannot be implemented",
            None,
            Some(
                "Key columns \"a\" of the referencing table and \"a\" of the referenced table are of incompatible types: text and integer.",
            ),
            None,
        )),
    ),
    (
        "CREATE TABLE t (a int, b int, FOREIGN KEY (a, b) REFERENCES p);",
        &[],
        Some((
            "42830",
            "number of referencing and referenced columns for foreign key disagree",
            None,
            None,
            None,
        )),
    ),
    (
        "CREATE TABLE t (a int REFERENCES p (a, a));",
        &[],
        Some((
            "42830",
            "foreign key referenced-columns list must not contain duplicates",
            None,
            None,
            None,
        )),
    ),
    (
        "CREATE TABLE t (a int REFERENCES pg_class);",
        &[],
        Some(("42501", "permission denied: \"pg_class\" is a system catalog", None, None, None)),
    ),
    (
        "CREATE TABLE t (a int REFERENCES pg_tables);",
        &[],
        Some(("42809", "referenced relation \"pg_tables\" is not a table", None, None, None)),
    ),
    (
        "CREATE TABLE t (b int, FOREIGN KEY (a) REFERENCES p);",
        &[],
        Some((
            "42703",
            "column \"a\" referenced in foreign key constraint does not exist",
            None,
            None,
            None,
        )),
    ),
    (
        "CREATE TABLE t (a int, FOREIGN KEY (ctid) REFERENCES p);",
        &[],
        Some(("0A000", "system columns cannot be used in foreign keys", None, None, None)),
    ),
    (
        "CREATE TABLE t (a int CONSTRAINT x CHECK (a > 0) CONSTRAINT x NOT NULL);",
        &[],
        Some((
            "23505",
            "duplicate key value violates unique constraint \"pg_constraint_conrelid_contypid_conname_index\"",
            None,
            Some("Key (conrelid, contypid, conname)=(23261, 0, x) already exists."),
            None,
        )),
    ),
    (
        "CREATE TABLE t (a int CONSTRAINT x NOT NULL, b int CONSTRAINT x NOT NULL);",
        &[],
        Some(("42710", "constraint \"x\" for relation \"t\" already exists", None, None, None)),
    ),
    (
        "CREATE TABLE t (a int CONSTRAINT x NOT NULL CONSTRAINT y NOT NULL);",
        &[],
        Some(("XX000", "conflicting not-null constraint names \"x\" and \"y\"", None, None, None)),
    ),
    (
        "CREATE TABLE t (a int NOT NULL NO INHERIT PRIMARY KEY);",
        &[],
        Some((
            "42601",
            "conflicting NO INHERIT declarations for not-null constraints on column \"a\"",
            None,
            None,
            None,
        )),
    ),
    (
        "CREATE TABLE t (a int DEFERRABLE);",
        &[],
        Some(("42601", "misplaced DEFERRABLE clause", Some(22), None, None)),
    ),
    (
        "CREATE TABLE t (a int UNIQUE INITIALLY DEFERRED NOT DEFERRABLE);",
        &[],
        Some((
            "42601",
            "constraint declared INITIALLY DEFERRED must be DEFERRABLE",
            Some(48),
            None,
            None,
        )),
    ),
    (
        "CREATE TABLE t (a int COLLATE \"C\");",
        &[],
        Some(("42804", "collations are not supported by type integer", Some(22), None, None)),
    ),
    (
        "CREATE TABLE t (a text COLLATE \"nosuch\");",
        &[],
        Some((
            "42704",
            "collation \"nosuch\" for encoding \"UTF8\" does not exist",
            Some(23),
            None,
            None,
        )),
    ),
    (
        "CREATE TABLE t (a numeric(1001));",
        &[],
        Some(("22023", "NUMERIC precision 1001 must be between 1 and 1000", Some(18), None, None)),
    ),
    (
        "CREATE TABLE t47 (a timestamp(9));",
        &[
            ('W', "22023", "TIMESTAMP(9) precision reduced to maximum allowed, 6", Some(20)),
            ('W', "22023", "TIMESTAMP(9) precision reduced to maximum allowed, 6", None),
        ],
        None,
    ),
    (
        "CREATE TABLE pg_catalog.t (a int);",
        &[],
        Some((
            "42501",
            "permission denied to create \"pg_catalog.t\"",
            None,
            Some("System catalog modifications are currently disallowed."),
            None,
        )),
    ),
    (
        "CREATE TABLE pg_catalog.pg_class (a int);",
        &[],
        Some(("42P07", "relation \"pg_class\" already exists", None, None, None)),
    ),
    (
        "CREATE TABLE t (a int, b int, UNIQUE (a) INCLUDE (c));",
        &[],
        Some(("42703", "column \"c\" named in key does not exist", Some(30), None, None)),
    ),
    (
        "CREATE TABLE t (a int, UNIQUE (ctid));",
        &[],
        Some(("0A000", "index creation on system columns is not supported", None, None, None)),
    ),
    (
        "CREATE TABLE t (a point PRIMARY KEY);",
        &[],
        Some((
            "42704",
            "data type point has no default operator class for access method \"btree\"",
            None,
            None,
            Some(
                "You must specify an operator class for the index or define a default operator class for the data type.",
            ),
        )),
    ),
    (
        "CREATE TABLE t (a int, b int, PRIMARY KEY (a), PRIMARY KEY (b));",
        &[],
        Some((
            "42P16",
            "multiple primary keys for table \"t\" are not allowed",
            Some(47),
            None,
            None,
        )),
    ),
    (
        "CREATE TABLE t (a int DEFAULT $1);",
        &[],
        Some(("42P02", "there is no parameter $1", Some(30), None, None)),
    ),
    (
        "CREATE TABLE t (a int CHECK (a > (SELECT 1)));",
        &[],
        Some(("0A000", "cannot use subquery in check constraint", Some(33), None, None)),
    ),
    ("CREATE TABLE t60 ();", &[], None),
    (
        "CREATE TABLE t (a int, b int, CHECK (a > b), CONSTRAINT t_check CHECK (a < b));",
        &[],
        Some(("42710", "check constraint \"t_check\" already exists", None, None, None)),
    ),
    ("CREATE TABLE t65 (a int CONSTRAINT t65_pkey CHECK (a > 0) PRIMARY KEY);", &[], None),
    ("CREATE TABLE t99 (a int8 REFERENCES p);", &[], None),
    (
        "CREATE TABLE t (a numeric REFERENCES p);",
        &[],
        Some((
            "42804",
            "foreign key constraint \"t_a_fkey\" cannot be implemented",
            None,
            Some(
                "Key columns \"a\" of the referencing table and \"a\" of the referenced table are of incompatible types: numeric and integer.",
            ),
            None,
        )),
    ),
    ("CREATE TABLE t101 (a varchar REFERENCES p (b));", &[], None),
    (
        "CREATE TABLE t (a int REFERENCES p (b));",
        &[],
        Some((
            "42804",
            "foreign key constraint \"t_a_fkey\" cannot be implemented",
            None,
            Some(
                "Key columns \"a\" of the referencing table and \"b\" of the referenced table are of incompatible types: integer and text.",
            ),
            None,
        )),
    ),
    (
        "CREATE INDEX ON nosuch (a);",
        &[],
        Some(("42P01", "relation \"nosuch\" does not exist", None, None, None)),
    ),
    (
        "CREATE INDEX ON nosch.nosuch (a);",
        &[],
        Some(("3F000", "schema \"nosch\" does not exist", None, None, None)),
    ),
    (
        "CREATE INDEX ON p (nosuch);",
        &[],
        Some(("42703", "column \"nosuch\" does not exist", Some(19), None, None)),
    ),
    (
        "CREATE INDEX ON p ((a + 1)) INCLUDE ((a + 2));",
        &[],
        Some(("0A000", "expressions are not supported in included columns", Some(37), None, None)),
    ),
    (
        "CREATE INDEX ON p (a) INCLUDE (b DESC);",
        &[],
        Some(("42P17", "including column does not support ASC/DESC options", Some(31), None, None)),
    ),
    (
        "CREATE INDEX ON p ((random()));",
        &[],
        Some((
            "42P17",
            "functions in index expression must be marked IMMUTABLE",
            Some(19),
            None,
            None,
        )),
    ),
    (
        "CREATE INDEX ON p (a) WHERE random() > 0;",
        &[],
        Some(("42P17", "functions in index predicate must be marked IMMUTABLE", None, None, None)),
    ),
    (
        "CREATE INDEX ON p (a) WHERE a;",
        &[],
        Some((
            "42804",
            "argument of WHERE must be type boolean, not type integer",
            Some(28),
            None,
            None,
        )),
    ),
    (
        "CREATE INDEX ON p (a text_ops);",
        &[],
        Some((
            "42804",
            "operator class \"text_ops\" does not accept data type integer",
            None,
            None,
            None,
        )),
    ),
    (
        "CREATE INDEX ON p (a nosuch_ops);",
        &[],
        Some((
            "42704",
            "operator class \"nosuch_ops\" does not exist for access method \"btree\"",
            None,
            None,
            None,
        )),
    ),
    (
        "CREATE INDEX ON p USING nosuch (a);",
        &[],
        Some(("42704", "access method \"nosuch\" does not exist", None, None, None)),
    ),
    (
        "CREATE INDEX ON p USING heap (a);",
        &[],
        Some((
            "XX000",
            "index access method handler function 3 did not return an IndexAmRoutine struct",
            None,
            None,
            None,
        )),
    ),
    (
        "CREATE INDEX ON pg_class (relname);",
        &[],
        Some(("42501", "permission denied: \"pg_class\" is a system catalog", None, None, None)),
    ),
    (
        "CREATE INDEX ON pg_tables (tablename);",
        &[],
        Some((
            "42809",
            "cannot create index on relation \"pg_tables\"",
            None,
            Some("This operation is not supported for views."),
            None,
        )),
    ),
    (
        "CREATE INDEX p_pkey ON p (a);",
        &[],
        Some(("42P07", "relation \"p_pkey\" already exists", None, None, None)),
    ),
    (
        "CREATE INDEX IF NOT EXISTS p_pkey ON p (a);",
        &[('N', "42P07", "relation \"p_pkey\" already exists, skipping", None)],
        None,
    ),
    (
        "CREATE INDEX ON p (a COLLATE \"C\");",
        &[],
        Some(("42804", "collations are not supported by type integer", Some(19), None, None)),
    ),
    (
        "CREATE INDEX ON p (ctid);",
        &[],
        Some(("0A000", "index creation on system columns is not supported", None, None, None)),
    ),
    (
        "CREATE INDEX ON p_pkey (a);",
        &[],
        Some((
            "42809",
            "cannot open relation \"p_pkey\"",
            None,
            Some("This operation is not supported for indexes."),
            None,
        )),
    ),
    ("CREATE TABLE s1 (x serial);", &[], None),
    (
        "CREATE INDEX ON s1_x_seq (last_value);",
        &[],
        Some((
            "42809",
            "cannot create index on relation \"s1_x_seq\"",
            None,
            Some("This operation is not supported for sequences."),
            None,
        )),
    ),
    (
        "CREATE INDEX ON p (a) WHERE (SELECT true);",
        &[],
        Some(("0A000", "cannot use subquery in index predicate", Some(28), None, None)),
    ),
    (
        "CREATE INDEX ON p (sum(a));",
        &[],
        Some((
            "42803",
            "aggregate functions are not allowed in index expressions",
            Some(19),
            None,
            None,
        )),
    ),
    (
        "CREATE INDEX ON p ();",
        &[],
        Some(("42601", "syntax error at or near \")\"", Some(19), None, None)),
    ),
    ("CREATE INDEX ON p ((1));", &[], None),
    ("CREATE INDEX ON p (a, a);", &[], None),
    ("CREATE INDEX ON p (b COLLATE \"C\" DESC NULLS LAST, (b || 'x'));", &[], None),
    (
        "CREATE SCHEMA pg_x;",
        &[],
        Some((
            "42939",
            "unacceptable schema name \"pg_x\"",
            None,
            Some("The prefix \"pg_\" is reserved for system schemas."),
            None,
        )),
    ),
    (
        "CREATE SCHEMA public;",
        &[],
        Some(("42P06", "schema \"public\" already exists", None, None, None)),
    ),
    (
        "CREATE SCHEMA IF NOT EXISTS public;",
        &[('N', "42P06", "schema \"public\" already exists, skipping", None)],
        None,
    ),
    (
        "CREATE SCHEMA AUTHORIZATION nosuch;",
        &[],
        Some(("42704", "role \"nosuch\" does not exist", None, None, None)),
    ),
    (
        "CREATE SCHEMA x AUTHORIZATION public;",
        &[],
        Some(("42704", "role \"public\" does not exist", None, None, None)),
    ),
    (
        "CREATE SCHEMA AUTHORIZATION pg_monitor;",
        &[],
        Some((
            "42939",
            "unacceptable schema name \"pg_monitor\"",
            None,
            Some("The prefix \"pg_\" is reserved for system schemas."),
            None,
        )),
    ),
    ("CREATE SCHEMA AUTHORIZATION postgres;", &[], None),
    (
        "CREATE TABLE t (a int REFERENCES p (b, c));",
        &[],
        Some((
            "42830",
            "there is no unique constraint matching given keys for referenced table \"p\"",
            None,
            None,
            None,
        )),
    ),
    (
        "CREATE TABLE t (a int, CONSTRAINT p_pkey CHECK (a > 0), CONSTRAINT p_pkey NOT NULL a);",
        &[],
        Some((
            "23505",
            "duplicate key value violates unique constraint \"pg_constraint_conrelid_contypid_conname_index\"",
            None,
            Some("Key (conrelid, contypid, conname)=(23340, 0, p_pkey) already exists."),
            None,
        )),
    ),
    (
        "CREATE TABLE t (a int, b int, CONSTRAINT k UNIQUE (a), CONSTRAINT k UNIQUE (b));",
        &[],
        Some(("42P07", "relation \"k\" already exists", None, None, None)),
    ),
    (
        "CREATE TABLE t (a int, b int, CONSTRAINT k CHECK (a > 0), CONSTRAINT k UNIQUE (b));",
        &[],
        Some(("42710", "constraint \"k\" for relation \"t\" already exists", None, None, None)),
    ),
    (
        "CREATE TABLE t (a int, b int, CONSTRAINT k UNIQUE (b), CONSTRAINT k FOREIGN KEY (a) REFERENCES p);",
        &[],
        Some(("42710", "constraint \"k\" for relation \"t\" already exists", None, None, None)),
    ),
    (
        "CREATE TABLE t (a int CONSTRAINT k REFERENCES p, b int CONSTRAINT k REFERENCES p);",
        &[],
        Some(("42710", "constraint \"k\" for relation \"t\" already exists", None, None, None)),
    ),
];
