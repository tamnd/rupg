//! The expected values come from PostgreSQL 19beta4 after `initdb --locale=C` and these statements in the database `postgres`:
//!
//! ```sql
//! CREATE TABLE t0 (a int);
//! CREATE SCHEMA s;
//! CREATE TABLE s.t (a int PRIMARY KEY, b text NOT NULL DEFAULT 'x', c numeric(10,2) CHECK (c > 0), d int UNIQUE, e int REFERENCES s.t (a));
//! CREATE INDEX t_b ON s.t (b);
//! CREATE TABLE s.u (x serial, y int8);
//! CREATE TABLE s.v (a int, b int, CHECK (a > b), CHECK (a > 0), CHECK (b > 0), UNIQUE (a, b), UNIQUE (a, b));
//! CREATE INDEX ON s.v (a);
//! CREATE INDEX ON s.v (a);
//! CREATE INDEX ON s.v ((a + b));
//! CREATE TABLE s.aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa (bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb serial PRIMARY KEY);
//! CREATE TABLE s._w (a int);
//! CREATE TABLE s.w (a int);
//! CREATE TABLE s.p (a int CONSTRAINT pk PRIMARY KEY, b int CONSTRAINT nn NOT NULL CONSTRAINT ck CHECK (b > 1));
//! ```
//!
//! The test makes the calls that the analyzer makes for these statements.

use super::names::{index_column_names, make_object_name, name_addition};
use super::toast::{Shape, needs_toast};
use super::*;

const INT4: u32 = 23;
const INT8: u32 = 20;
const BOOL: u32 = 16;
const TEXT: u32 = 25;
const NUMERIC: u32 = 1700;
const DEFAULT_COLLATION: u32 = 100;
const INT4_OPS: u32 = 1978;
const TEXT_OPS: u32 = 3126;
const SUPERUSER: u32 = 10;

fn table(namespace: u32, name: &str, columns: &[(&str, u32, i32)]) -> NewRelation {
    let columns = columns
        .iter()
        .map(|&(name, ty, typmod)| {
            let collation = if ty == TEXT { DEFAULT_COLLATION } else { 0 };
            Column::new(name, ty, typmod, collation)
        })
        .collect();
    NewRelation { namespace, name: name.to_string(), owner: SUPERUSER, columns }
}

fn serial_sequence(namespace: u32, name: &str) -> (NewRelation, SequenceInfo) {
    let new = table(
        namespace,
        name,
        &[("last_value", INT8, -1), ("log_cnt", INT8, -1), ("is_called", BOOL, -1)],
    );
    let info = SequenceInfo {
        ty: INT4,
        start: 1,
        increment: 1,
        max: 2_147_483_647,
        min: 1,
        cache: 1,
        cycle: false,
    };
    (new, info)
}

fn index(
    table: u32,
    name: Option<&str>,
    columns: &[(&str, i16, u32)],
    constraint: Option<ConKind>,
) -> NewIndex {
    let columns: Vec<NewIndexColumn> = columns
        .iter()
        .map(|&(name, key, ty)| NewIndexColumn {
            name: name.to_string(),
            key,
            ty,
            typmod: -1,
            collation: if ty == TEXT { DEFAULT_COLLATION } else { 0 },
            class: if ty == TEXT { TEXT_OPS } else { INT4_OPS },
            option: 0,
        })
        .collect();
    NewIndex {
        table,
        name: name.map(str::to_string),
        key_count: columns.len(),
        columns,
        method: BTREE,
        unique: constraint.is_some(),
        nulls_not_distinct: false,
        constraint,
        deferrable: false,
        deferred: false,
        exprs: None,
        predicate: None,
        refs: Vec::new(),
    }
}

fn foreign_key(
    table: u32,
    keys: Vec<i16>,
    referenced: u32,
    referenced_keys: Vec<i16>,
    index: u32,
) -> NewForeignKey {
    NewForeignKey {
        table,
        name: None,
        keys,
        index,
        deferrable: false,
        deferred: false,
        foreign: ForeignKey {
            table: referenced,
            keys: referenced_keys,
            update: 'a',
            delete: 'a',
            match_type: 's',
            pf_eq: vec![96],
            pp_eq: vec![96],
            ff_eq: vec![96],
        },
    }
}

fn names_of(catalog: &Catalog, oids: &[u32]) -> Vec<String> {
    oids.iter()
        .map(|oid| {
            catalog
                .relation(*oid)
                .map(|r| r.name.clone())
                .or_else(|| catalog.constraint(*oid).map(|c| c.name.clone()))
                .or_else(|| catalog.type_by_oid(*oid).map(|t| t.name.clone()))
                .unwrap_or_default()
        })
        .collect()
}

#[test]
fn object_names() {
    assert_eq!(make_object_name("t", Some("a"), Some("not_null")), "t_a_not_null");
    assert_eq!(make_object_name("t", None, Some("pkey")), "t_pkey");
    assert_eq!(make_object_name("", Some("w"), None), "_w");
    assert_eq!(make_object_name("", Some("w"), Some("1")), "_w_1");
    let a = "a".repeat(63);
    let b = "b".repeat(63);
    assert_eq!(
        make_object_name(&a, Some(&b), Some("seq")),
        format!("{}_{}_seq", "a".repeat(29), "b".repeat(29))
    );
    assert_eq!(
        make_object_name(&a, Some(&b), Some("not_null")),
        format!("{}_{}_not_null", "a".repeat(27), "b".repeat(26))
    );
    assert_eq!(make_object_name(&a, None, Some("pkey")), format!("{}_pkey", "a".repeat(58)));
    assert_eq!(make_object_name("", Some(&a), None), format!("_{}", "a".repeat(62)));
    let wide = "é".repeat(40);
    let name = make_object_name(&wide, None, Some("pkey"));
    assert_eq!(name, format!("{}_pkey", "é".repeat(29)));
    assert_eq!(name_addition(["a", "b"]), "a_b");
    assert_eq!(
        index_column_names(["a", "a", "expr", "expr", "a"]),
        ["a", "a1", "expr", "expr1", "a2"]
    );
}

#[test]
fn postgres_order() {
    let mut cat = Catalog::new();

    let t0 = cat.create_table(table(PUBLIC, "t0", &[("a", INT4, -1)])).unwrap();
    assert_eq!(t0, 16384);
    let s = cat.create_schema("s", SUPERUSER).unwrap();
    assert_eq!(s, 16387);

    let numeric_10_2 = (10 << 16 | 2) + 4;
    let cols = [
        ("a", INT4, -1),
        ("b", TEXT, -1),
        ("c", NUMERIC, numeric_10_2),
        ("d", INT4, -1),
        ("e", INT4, -1),
    ];
    let t = cat.create_table(table(s, "t", &cols)).unwrap();
    assert_eq!(t, 16388);
    assert_eq!(cat.add_default(t, 2, "'x'::text".into(), &[]).unwrap(), 16391);
    let check =
        cat.add_check(t, None, "c > 0".into(), vec![3], &[ObjRef::column(t, 3)], false).unwrap();
    assert_eq!(check, 16392);
    assert_eq!(cat.add_not_null(t, None, 1, false).unwrap(), 16393);
    assert_eq!(cat.add_not_null(t, None, 2, false).unwrap(), 16394);
    let shapes = [
        Shape { len: 4, align: b'i', storage: b'p', ty: INT4, typmod: -1 },
        Shape { len: -1, align: b'i', storage: b'x', ty: TEXT, typmod: -1 },
    ];
    assert!(needs_toast(&shapes));
    cat.skip_oids(2);
    let (pkey, pkey_con) =
        cat.create_index(index(t, None, &[("a", 1, INT4)], Some(ConKind::Primary))).unwrap();
    assert_eq!((pkey, pkey_con), (16397, Some(16398)));
    let (d_key, d_con) =
        cat.create_index(index(t, None, &[("d", 4, INT4)], Some(ConKind::Unique))).unwrap();
    assert_eq!((d_key, d_con), (16399, Some(16400)));
    let fkey = cat.add_foreign_key(foreign_key(t, vec![5], t, vec![1], pkey)).unwrap();
    assert_eq!(fkey, 16401);
    let (t_b, _) = cat.create_index(index(t, Some("t_b"), &[("b", 2, TEXT)], None)).unwrap();
    assert_eq!(t_b, 16406);
    assert_eq!(
        names_of(
            &cat,
            &[
                t0,
                16385,
                16386,
                t,
                16389,
                16390,
                check,
                16393,
                16394,
                pkey,
                pkey_con.unwrap(),
                d_key,
                fkey
            ]
        ),
        [
            "t0",
            "_t0",
            "t0",
            "t",
            "_t",
            "t",
            "t_c_check",
            "t_a_not_null",
            "t_b_not_null",
            "t_pkey",
            "t_pkey",
            "t_d_key",
            "t_e_fkey"
        ]
    );
    let rel = cat.relation(t).unwrap();
    assert!(rel.has_index && rel.columns[0].not_null && rel.columns[1].has_default);

    let seq_name = cat.choose_relation_name("u", Some("x"), "seq", s, false);
    let (new, info) = serial_sequence(s, &seq_name);
    let seq = cat.create_sequence(new, info).unwrap();
    assert_eq!((seq, seq_name.as_str()), (16407, "u_x_seq"));
    let u = cat.create_table(table(s, "u", &[("x", INT4, -1), ("y", INT8, -1)])).unwrap();
    assert_eq!(u, 16408);
    let nextval = "nextval('s.u_x_seq'::regclass)".to_string();
    assert_eq!(cat.add_default(u, 1, nextval, &[ObjRef::new(PG_CLASS, seq)]).unwrap(), 16411);
    assert_eq!(cat.add_not_null(u, None, 1, false).unwrap(), 16412);
    cat.set_sequence_owner(seq, u, 1);

    let v = cat.create_table(table(s, "v", &[("a", INT4, -1), ("b", INT4, -1)])).unwrap();
    assert_eq!(v, 16413);
    let both = [ObjRef::column(v, 1), ObjRef::column(v, 2)];
    let c1 = cat.add_check(v, None, "a > b".into(), vec![1, 2], &both, false).unwrap();
    let c2 = cat.add_check(v, None, "a > 0".into(), vec![1], &both[..1], false).unwrap();
    let c3 = cat.add_check(v, None, "b > 0".into(), vec![2], &both[1..], false).unwrap();
    assert_eq!([c1, c2, c3], [16416, 16417, 16418]);
    let (ab, ab_con) = cat
        .create_index(index(v, None, &[("a", 1, INT4), ("b", 2, INT4)], Some(ConKind::Unique)))
        .unwrap();
    assert_eq!((ab, ab_con), (16419, Some(16420)));
    let (i1, _) = cat.create_index(index(v, None, &[("a", 1, INT4)], None)).unwrap();
    let (i2, _) = cat.create_index(index(v, None, &[("a", 1, INT4)], None)).unwrap();
    let mut expr = index(v, None, &[("expr", 0, INT4)], None);
    expr.exprs = Some("a + b".into());
    expr.refs = both.to_vec();
    let (i3, _) = cat.create_index(expr).unwrap();
    assert_eq!([i1, i2, i3], [16421, 16422, 16423]);
    assert_eq!(
        names_of(&cat, &[c1, c2, c3, ab, i1, i2, i3]),
        ["v_check", "v_a_check", "v_b_check", "v_a_b_key", "v_a_idx", "v_a_idx1", "v_expr_idx"]
    );

    let long_table = "a".repeat(63);
    let long_column = "b".repeat(63);
    let seq_name = cat.choose_relation_name(&long_table, Some(&long_column), "seq", s, false);
    let (new, info) = serial_sequence(s, &seq_name);
    let seq = cat.create_sequence(new, info).unwrap();
    let long = cat.create_table(table(s, &long_table, &[(&long_column, INT4, -1)])).unwrap();
    assert_eq!((seq, long), (16424, 16425));
    assert_eq!(
        cat.add_default(long, 1, "nextval".into(), &[ObjRef::new(PG_CLASS, seq)]).unwrap(),
        16428
    );
    let nn = cat.add_not_null(long, None, 1, false).unwrap();
    let (long_pkey, long_con) = cat
        .create_index(index(long, None, &[(&long_column, 1, INT4)], Some(ConKind::Primary)))
        .unwrap();
    cat.set_sequence_owner(seq, long, 1);
    assert_eq!((nn, long_pkey, long_con), (16429, 16430, Some(16431)));
    assert_eq!(
        names_of(&cat, &[seq, 16426, nn, long_pkey]),
        [
            format!("{}_{}_seq", "a".repeat(29), "b".repeat(29)),
            format!("_{}", "a".repeat(62)),
            format!("{}_{}_not_null", "a".repeat(27), "b".repeat(26)),
            format!("{}_pkey", "a".repeat(58)),
        ]
    );

    let under_w = cat.create_table(table(s, "_w", &[("a", INT4, -1)])).unwrap();
    let w = cat.create_table(table(s, "w", &[("a", INT4, -1)])).unwrap();
    assert_eq!((under_w, w), (16432, 16435));
    assert_eq!(names_of(&cat, &[16433, 16434, 16436, 16437]), ["__w", "_w", "_w_1", "w"]);

    let p = cat.create_table(table(s, "p", &[("a", INT4, -1), ("b", INT4, -1)])).unwrap();
    assert_eq!(p, 16438);
    let ck = cat
        .add_check(p, Some("ck"), "b > 1".into(), vec![2], &[ObjRef::column(p, 2)], false)
        .unwrap();
    let a_nn = cat.add_not_null(p, None, 1, false).unwrap();
    let b_nn = cat.add_not_null(p, Some("nn"), 2, false).unwrap();
    let (pk, pk_con) =
        cat.create_index(index(p, Some("pk"), &[("a", 1, INT4)], Some(ConKind::Primary))).unwrap();
    assert_eq!([ck, a_nn, b_nn, pk, pk_con.unwrap()], [16441, 16442, 16443, 16444, 16445]);
    assert_eq!(
        names_of(&cat, &[ck, a_nn, b_nn, pk, 16445]),
        ["ck", "p_a_not_null", "nn", "pk", "pk"]
    );

    // The rows of pg_depend in PostgreSQL, without the rows of the TOAST table of t and of the four triggers of t_e_fkey.
    let expected: &[(u32, u32, i32, u32, u32, i32, char)] = &[
        (PG_CLASS, 16384, 0, PG_NAMESPACE, 2200, 0, 'n'),
        (PG_TYPE, 16385, 0, PG_TYPE, 16386, 0, 'i'),
        (PG_TYPE, 16386, 0, PG_CLASS, 16384, 0, 'i'),
        (PG_CLASS, 16388, 0, PG_NAMESPACE, 16387, 0, 'n'),
        (PG_TYPE, 16389, 0, PG_TYPE, 16390, 0, 'i'),
        (PG_TYPE, 16390, 0, PG_CLASS, 16388, 0, 'i'),
        (PG_ATTRDEF, 16391, 0, PG_CLASS, 16388, 2, 'a'),
        (PG_CONSTRAINT, 16392, 0, PG_CLASS, 16388, 3, 'n'),
        (PG_CONSTRAINT, 16392, 0, PG_CLASS, 16388, 3, 'a'),
        (PG_CONSTRAINT, 16393, 0, PG_CLASS, 16388, 1, 'a'),
        (PG_CONSTRAINT, 16394, 0, PG_CLASS, 16388, 2, 'a'),
        (PG_CLASS, 16397, 0, PG_CONSTRAINT, 16398, 0, 'i'),
        (PG_CONSTRAINT, 16398, 0, PG_CLASS, 16388, 1, 'a'),
        (PG_CLASS, 16399, 0, PG_CONSTRAINT, 16400, 0, 'i'),
        (PG_CONSTRAINT, 16400, 0, PG_CLASS, 16388, 4, 'a'),
        (PG_CONSTRAINT, 16401, 0, PG_CLASS, 16388, 5, 'a'),
        (PG_CONSTRAINT, 16401, 0, PG_CLASS, 16388, 1, 'n'),
        (PG_CONSTRAINT, 16401, 0, PG_CLASS, 16397, 0, 'n'),
        (PG_CLASS, 16406, 0, PG_CLASS, 16388, 2, 'a'),
        (PG_CLASS, 16407, 0, PG_CLASS, 16408, 1, 'a'),
        (PG_CLASS, 16407, 0, PG_NAMESPACE, 16387, 0, 'n'),
        (PG_CLASS, 16408, 0, PG_NAMESPACE, 16387, 0, 'n'),
        (PG_TYPE, 16409, 0, PG_TYPE, 16410, 0, 'i'),
        (PG_TYPE, 16410, 0, PG_CLASS, 16408, 0, 'i'),
        (PG_ATTRDEF, 16411, 0, PG_CLASS, 16407, 0, 'n'),
        (PG_ATTRDEF, 16411, 0, PG_CLASS, 16408, 1, 'a'),
        (PG_CONSTRAINT, 16412, 0, PG_CLASS, 16408, 1, 'a'),
        (PG_CLASS, 16413, 0, PG_NAMESPACE, 16387, 0, 'n'),
        (PG_TYPE, 16414, 0, PG_TYPE, 16415, 0, 'i'),
        (PG_TYPE, 16415, 0, PG_CLASS, 16413, 0, 'i'),
        (PG_CONSTRAINT, 16416, 0, PG_CLASS, 16413, 1, 'n'),
        (PG_CONSTRAINT, 16416, 0, PG_CLASS, 16413, 1, 'a'),
        (PG_CONSTRAINT, 16416, 0, PG_CLASS, 16413, 2, 'a'),
        (PG_CONSTRAINT, 16416, 0, PG_CLASS, 16413, 2, 'n'),
        (PG_CONSTRAINT, 16417, 0, PG_CLASS, 16413, 1, 'a'),
        (PG_CONSTRAINT, 16417, 0, PG_CLASS, 16413, 1, 'n'),
        (PG_CONSTRAINT, 16418, 0, PG_CLASS, 16413, 2, 'a'),
        (PG_CONSTRAINT, 16418, 0, PG_CLASS, 16413, 2, 'n'),
        (PG_CLASS, 16419, 0, PG_CONSTRAINT, 16420, 0, 'i'),
        (PG_CONSTRAINT, 16420, 0, PG_CLASS, 16413, 2, 'a'),
        (PG_CONSTRAINT, 16420, 0, PG_CLASS, 16413, 1, 'a'),
        (PG_CLASS, 16421, 0, PG_CLASS, 16413, 1, 'a'),
        (PG_CLASS, 16422, 0, PG_CLASS, 16413, 1, 'a'),
        (PG_CLASS, 16423, 0, PG_CLASS, 16413, 0, 'a'),
        (PG_CLASS, 16423, 0, PG_CLASS, 16413, 2, 'a'),
        (PG_CLASS, 16423, 0, PG_CLASS, 16413, 1, 'a'),
        (PG_CLASS, 16424, 0, PG_CLASS, 16425, 1, 'a'),
        (PG_CLASS, 16424, 0, PG_NAMESPACE, 16387, 0, 'n'),
        (PG_CLASS, 16425, 0, PG_NAMESPACE, 16387, 0, 'n'),
        (PG_TYPE, 16426, 0, PG_TYPE, 16427, 0, 'i'),
        (PG_TYPE, 16427, 0, PG_CLASS, 16425, 0, 'i'),
        (PG_ATTRDEF, 16428, 0, PG_CLASS, 16424, 0, 'n'),
        (PG_ATTRDEF, 16428, 0, PG_CLASS, 16425, 1, 'a'),
        (PG_CONSTRAINT, 16429, 0, PG_CLASS, 16425, 1, 'a'),
        (PG_CLASS, 16430, 0, PG_CONSTRAINT, 16431, 0, 'i'),
        (PG_CONSTRAINT, 16431, 0, PG_CLASS, 16425, 1, 'a'),
        (PG_CLASS, 16432, 0, PG_NAMESPACE, 16387, 0, 'n'),
        (PG_TYPE, 16433, 0, PG_TYPE, 16434, 0, 'i'),
        (PG_TYPE, 16434, 0, PG_CLASS, 16432, 0, 'i'),
        (PG_CLASS, 16435, 0, PG_NAMESPACE, 16387, 0, 'n'),
        (PG_TYPE, 16436, 0, PG_TYPE, 16437, 0, 'i'),
        (PG_TYPE, 16437, 0, PG_CLASS, 16435, 0, 'i'),
        (PG_CLASS, 16438, 0, PG_NAMESPACE, 16387, 0, 'n'),
        (PG_TYPE, 16439, 0, PG_TYPE, 16440, 0, 'i'),
        (PG_TYPE, 16440, 0, PG_CLASS, 16438, 0, 'i'),
        (PG_CONSTRAINT, 16441, 0, PG_CLASS, 16438, 2, 'n'),
        (PG_CONSTRAINT, 16441, 0, PG_CLASS, 16438, 2, 'a'),
        (PG_CONSTRAINT, 16442, 0, PG_CLASS, 16438, 1, 'a'),
        (PG_CONSTRAINT, 16443, 0, PG_CLASS, 16438, 2, 'a'),
        (PG_CLASS, 16444, 0, PG_CONSTRAINT, 16445, 0, 'i'),
        (PG_CONSTRAINT, 16445, 0, PG_CLASS, 16438, 1, 'a'),
    ];
    let mut expected: Vec<_> = expected.to_vec();
    let mut actual: Vec<_> = cat
        .depends()
        .iter()
        .map(|d| {
            let (o, r) = (d.object, d.referenced);
            (o.class, o.oid, o.sub, r.class, r.oid, r.sub, d.kind.code())
        })
        .collect();
    expected.sort_unstable();
    actual.sort_unstable();
    assert_eq!(actual, expected);
}

#[test]
fn name_conflicts() {
    let mut cat = Catalog::new();
    let s = cat.create_schema("s", SUPERUSER).unwrap();
    let err = cat.create_schema("s", SUPERUSER).unwrap_err();
    assert_eq!(
        (err.state(), err.message()),
        (SqlState::DUPLICATE_SCHEMA, "schema \"s\" already exists")
    );

    let t = cat.create_table(table(s, "t", &[("a", INT4, -1)])).unwrap();
    let err = cat.create_table(table(s, "t", &[("a", INT4, -1)])).unwrap_err();
    assert_eq!(
        (err.state(), err.message()),
        (SqlState::DUPLICATE_TABLE, "relation \"t\" already exists")
    );
    // The row type of _t takes the name of the array type of t, so that array type gets a new name.
    let under_t = cat.create_table(table(s, "_t", &[("a", INT4, -1)])).unwrap();
    assert_eq!(names_of(&cat, &[t + 1, under_t + 1, under_t + 2]), ["__t", "__t_1", "_t"]);

    let err = cat.create_table(table(s, "x", &[("a", INT4, -1), ("a", INT4, -1)])).unwrap_err();
    assert_eq!(
        (err.state(), err.message()),
        (SqlState::DUPLICATE_COLUMN, "column \"a\" specified more than once")
    );

    cat.add_check(t, Some("c"), "a > 0".into(), vec![1], &[], false).unwrap();
    let err = cat.add_not_null(t, Some("c"), 1, false).unwrap_err();
    assert_eq!(
        (err.state(), err.message()),
        (SqlState::DUPLICATE_OBJECT, "constraint \"c\" for relation \"t\" already exists")
    );
    let err = cat.create_index(index(t, Some("t"), &[("a", 1, INT4)], None)).unwrap_err();
    assert_eq!(err.message(), "relation \"t\" already exists");

    // A constraint name that another table of the schema has makes a name with a number.
    let t2 = cat.create_table(table(s, "t2", &[("a", INT4, -1)])).unwrap();
    let first = cat.add_check(t2, Some("t_a_check"), "a > 0".into(), vec![1], &[], false).unwrap();
    let second = cat.add_check(t, None, "a > 1".into(), vec![1], &[], false).unwrap();
    assert_eq!(names_of(&cat, &[first, second]), ["t_a_check", "t_a_check1"]);
}

#[test]
fn rollback_keeps_oids() {
    let mut cat = Catalog::new();
    let mut work = cat.clone();
    work.create_schema("s", SUPERUSER).unwrap();
    cat.advance_oid(work.next_oid());
    assert_eq!(cat.create_schema("s", SUPERUSER).unwrap(), 16385);
}

#[test]
fn toast_decision() {
    // The tables of PostgreSQL that have a TOAST table, with one column each unless the name says more.
    let varlena = |ty, typmod, storage| Shape { len: -1, align: b'i', storage, ty, typmod };
    let char_mod = |n: i32| n + 4;
    let cases = [
        ("varchar(501)", vec![varlena(1043, char_mod(501), b'x')], false),
        ("varchar(502)", vec![varlena(1043, char_mod(502), b'x')], true),
        ("char(500)", vec![varlena(1042, char_mod(500), b'x')], false),
        ("char(502)", vec![varlena(1042, char_mod(502), b'x')], true),
        ("numeric(1000)", vec![varlena(NUMERIC, (1000 << 16) + 4, b'm')], false),
        ("numeric(100) twice", vec![varlena(NUMERIC, (100 << 16) + 4, b'm'); 2], false),
        ("bit(16000)", vec![varlena(1560, 16000, b'x')], false),
        ("varbit(17000)", vec![varlena(1562, 17000, b'x')], true),
        (
            "int, int8, name",
            vec![
                Shape { len: 4, align: b'i', storage: b'p', ty: INT4, typmod: -1 },
                Shape { len: 8, align: b'd', storage: b'p', ty: INT8, typmod: -1 },
                Shape { len: 64, align: b'c', storage: b'p', ty: 19, typmod: -1 },
            ],
            false,
        ),
        ("jsonb", vec![varlena(3802, -1, b'x')], true),
    ];
    for (name, shapes, toast) in cases {
        assert_eq!(needs_toast(&shapes), toast, "{name}");
    }
}
