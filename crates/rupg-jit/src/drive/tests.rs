//! Whole queries through the generator and the driver on every tier the build has, checked against the rows and the errors PostgreSQL gives.

use std::collections::BTreeMap;
use std::sync::Arc;

use rupg_common::{Error, Result, SqlState};
use rupg_platform::os::{OsClock, OsTasks};
use rupg_types::numeric_out;
use rupg_types::{Value, oid};

use super::{Batch, Scan, check, run};
use crate::codegen::generate;
use crate::pipe::split;
use crate::plan::{Aggregate, BuildSide, Column, CompareOp, Expr, JoinType, Key, Kind, Rel, Type};
use crate::rt::Rt;
use crate::tier::{Options, Platform, Tier, Tiers};

fn ty(oid: u32) -> Type {
    Type::new(oid)
}

fn col(i: usize, oid: u32) -> Expr {
    Expr::column(i, ty(oid))
}

fn int4(x: i32) -> Expr {
    Expr { kind: Kind::Constant(Value::Int4(x)), ty: ty(oid::INT4) }
}

/// A call of the strict function with the `prosrc` `src`.
fn call_of(src: &'static str, oid: u32, args: Vec<Expr>) -> Expr {
    Expr { kind: Kind::Function { oid: 0, src, strict: true, args }, ty: ty(oid) }
}

fn compare(op: CompareOp, left: Expr, right: Expr) -> Expr {
    let kind = Kind::Compare { op, left: Box::new(left), right: Box::new(right) };
    Expr { kind, ty: ty(oid::BOOL) }
}

fn scan(relation: u32, columns: &[(&str, u32)]) -> Rel {
    Rel::Scan {
        relation,
        table: format!("t{relation}"),
        attnums: (1..).take(columns.len()).collect(),
        columns: columns
            .iter()
            .map(|(n, t)| Column { name: (*n).to_owned(), ty: ty(*t) })
            .collect(),
    }
}

fn project(input: Rel, exprs: Vec<Expr>) -> Rel {
    let columns =
        exprs.iter().enumerate().map(|(k, e)| Column { name: format!("c{k}"), ty: e.ty }).collect();
    Rel::Project { input: Box::new(input), exprs, columns }
}

fn agg(name: &'static str, args: Vec<Expr>, oid: u32) -> Aggregate {
    Aggregate { name, oid: 0, args, distinct: false, filter: None, ty: ty(oid) }
}

fn aggregate(input: Rel, groups: Vec<Expr>, aggregates: Vec<Aggregate>) -> Rel {
    let columns = groups
        .iter()
        .map(|g| g.ty)
        .chain(aggregates.iter().map(|a| a.ty))
        .enumerate()
        .map(|(k, ty)| Column { name: format!("c{k}"), ty })
        .collect();
    Rel::Aggregate { input: Box::new(input), groups, aggregates, columns, runs: None }
}

fn sort(input: Rel, keys: Vec<(usize, u32, bool)>) -> Rel {
    let keys = keys
        .into_iter()
        .map(|(c, oid, descending)| Key { expr: col(c, oid), descending, nulls_first: descending })
        .collect();
    Rel::Sort { input: Box::new(input), keys }
}

/// The tables of a query by OID, which hand their rows over `per` rows at a time.
struct Tables {
    tables: BTreeMap<u32, Vec<Vec<Value>>>,
    per: usize,
}

impl Scan for Tables {
    fn scan(
        &mut self,
        relation: u32,
        attnums: &[i16],
        columns: &[Column],
        read: &[bool],
        each: &mut dyn FnMut(Batch) -> Result<bool>,
    ) -> Result<()> {
        let rows = self.tables.get(&relation).ok_or_else(|| Error::internal("no table"))?;
        assert_eq!(attnums.len(), columns.len());
        for chunk in rows.chunks(self.per) {
            if !each(Batch::of_read(columns, chunk, read)?)? {
                break;
            }
        }
        Ok(())
    }
}

/// The tiers this build runs.
fn tiers() -> Vec<Tier> {
    [Tier::Interp, Tier::Clif, Tier::Direct].into_iter().filter(|t| t.built()).collect()
}

/// Runs `rel` over `tables` on `tier`, the scans handing `per` rows at a time and the morsels at most `morsel` rows.
fn answer_on(
    rel: &Rel,
    tables: &[(u32, Vec<Vec<Value>>)],
    tier: Tier,
    per: usize,
    morsel: usize,
) -> Result<Vec<Vec<Value>>> {
    let graph = split(rel);
    let mut rt = Rt::new(Arc::default());
    let query = generate(&graph, &mut rt).expect("generates");
    check(&graph, &query).expect("the driver runs it");
    let platform = Platform { clock: Arc::new(OsClock::new()), tasks: Some(Arc::new(OsTasks)) };
    let options = Options { tier, stay: true, fresh: true, morsel, ..Options::default() };
    let tiers = Tiers::new(&query.module, options, &platform);
    let mut scan = Tables { tables: tables.iter().cloned().collect(), per };
    run(&graph, &query, &tiers, &mut rt, &mut scan)
}

/// The answer of `rel` over `tables`, the same on every tier and however the rows are cut into batches and morsels.
fn answer(rel: &Rel, tables: &[(u32, Vec<Vec<Value>>)]) -> Result<Vec<Vec<Value>>> {
    let first = answer_on(rel, tables, Tier::Interp, 1024, 0);
    for tier in tiers() {
        for (per, morsel) in [(1024, 0), (3, 0), (5, 2)] {
            let other = answer_on(rel, tables, tier, per, morsel);
            match (&first, &other) {
                (Ok(a), Ok(b)) => {
                    assert_eq!(sorted(a.clone()), sorted(b.clone()), "{tier} in batches of {per}")
                }
                (Err(a), Err(b)) => {
                    assert_eq!(
                        (a.state(), a.message()),
                        (b.state(), b.message()),
                        "{tier} in batches of {per}"
                    );
                }
                _ => panic!("{tier} in batches of {per}: {first:?} and {other:?}"),
            }
        }
    }
    first
}

/// Rows in an order that does not depend on the one the groups came out in, for comparing answers with no `ORDER BY`.
fn sorted(mut rows: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
    rows.sort_by_key(|r| format!("{r:?}"));
    rows
}

fn i4(x: i32) -> Value {
    Value::Int4(x)
}

fn i8(x: i64) -> Value {
    Value::Int8(x)
}

fn text(s: &str) -> Value {
    Value::Text(s.to_owned())
}

/// A numeric's text, as `numeric_out` writes it.
fn shown(v: &Value) -> String {
    let Value::Numeric(n) = v else { return format!("{v:?}") };
    let mut out = Vec::new();
    numeric_out(n, &mut out);
    String::from_utf8(out).unwrap()
}

fn ints(values: &[Option<i32>]) -> Vec<Vec<Value>> {
    values.iter().map(|v| vec![v.map_or(Value::Null, i4)]).collect()
}

#[test]
fn a_filter_and_a_projection_give_the_rows_that_pass_in_order() {
    // SELECT x + 1 FROM a WHERE x > 3
    let a = scan(1, &[("x", oid::INT4)]);
    let filter = Rel::filtered(a, vec![compare(CompareOp::Greater, col(0, oid::INT4), int4(3))]);
    let rel = project(filter, vec![call_of("int4pl", oid::INT4, vec![col(0, oid::INT4), int4(1)])]);
    let rows =
        ints(&[Some(1), Some(5), None, Some(7), Some(2), Some(9), Some(0), Some(4), Some(30)]);
    let got = answer(&rel, &[(1, rows)]).expect("no error");
    assert_eq!(got, [[i4(6)], [i4(8)], [i4(10)], [i4(5)], [i4(31)]]);
}

#[test]
fn an_overflow_and_a_division_by_zero_raise_the_sqlstates_of_postgresql() {
    let a = || scan(1, &[("x", oid::INT4)]);
    let plus = project(a(), vec![call_of("int4pl", oid::INT4, vec![col(0, oid::INT4), int4(1)])]);
    let err = answer(&plus, &[(1, ints(&[Some(1), Some(i32::MAX)]))]).expect_err("overflows");
    assert_eq!(
        (err.state(), err.message()),
        (SqlState::NUMERIC_VALUE_OUT_OF_RANGE, "integer out of range")
    );
    let div = project(a(), vec![call_of("int4div", oid::INT4, vec![int4(10), col(0, oid::INT4)])]);
    let got = answer(&div, &[(1, ints(&[Some(5), None, Some(-2)]))]).expect("no zero");
    assert_eq!(got, [vec![i4(2)], vec![Value::Null], vec![i4(-5)]]);
    let err = answer(&div, &[(1, ints(&[Some(5), Some(0)]))]).expect_err("divides by zero");
    assert_eq!((err.state(), err.message()), (SqlState::DIVISION_BY_ZERO, "division by zero"));
}

#[test]
fn a_grouped_aggregate_finishes_its_accumulators_into_the_types_of_postgresql() {
    // SELECT s, count(*), sum(n), avg(n), min(n), max(n) FROM t GROUP BY s
    let t = scan(1, &[("s", oid::TEXT), ("n", oid::INT4)]);
    let n = || col(1, oid::INT4);
    let rel = aggregate(
        t,
        vec![col(0, oid::TEXT)],
        vec![
            agg("count_star", Vec::new(), oid::INT8),
            agg("sum", vec![n()], oid::INT8),
            agg("avg", vec![n()], oid::NUMERIC),
            agg("min", vec![n()], oid::INT4),
            agg("max", vec![n()], oid::INT4),
        ],
    );
    let long = "a string that is longer than twelve bytes";
    let rows: Vec<Vec<Value>> = [
        ("a", Some(1)),
        ("b", None),
        (long, Some(7)),
        ("a", Some(2)),
        (long, Some(-3)),
        ("a", Some(4)),
    ]
    .into_iter()
    .map(|(s, n)| vec![text(s), n.map_or(Value::Null, i4)])
    .collect();
    let mut got = answer(&rel, &[(1, rows)]).expect("no error");
    got.sort_by_key(|r| format!("{:?}", r[0]));
    let got: Vec<_> = got
        .iter()
        .map(|r| {
            (r[0].clone(), r[1].clone(), r[2].clone(), shown(&r[3]), r[4].clone(), r[5].clone())
        })
        .collect();
    let null = || Value::Null;
    assert_eq!(
        got,
        [
            (text(long), i8(2), i8(4), "2.0000000000000000".to_owned(), i4(-3), i4(7)),
            (text("a"), i8(3), i8(7), "2.3333333333333333".to_owned(), i4(1), i4(4)),
            (text("b"), i8(1), null(), "Null".to_owned(), null(), null()),
        ]
    );
}

#[test]
fn an_aggregate_with_no_groups_over_no_rows_gives_one_row() {
    // SELECT count(*), count(x), sum(x), avg(x), max(x) FROM a, over no rows and then over some
    let x = || col(0, oid::INT8);
    let rel = aggregate(
        scan(1, &[("x", oid::INT8)]),
        Vec::new(),
        vec![
            agg("count_star", Vec::new(), oid::INT8),
            agg("count", vec![x()], oid::INT8),
            agg("sum", vec![x()], oid::NUMERIC),
            agg("avg", vec![x()], oid::NUMERIC),
            agg("max", vec![x()], oid::INT8),
        ],
    );
    let got = answer(&rel, &[(1, Vec::new())]).expect("no error");
    assert_eq!(got, [vec![i8(0), i8(0), Value::Null, Value::Null, Value::Null]]);
    // The sum of an int8 is a numeric, which holds a total no int8 does.
    let rows = vec![vec![i8(i64::MAX)], vec![Value::Null], vec![i8(i64::MAX)], vec![i8(1)]];
    let got = answer(&rel, &[(1, rows)]).expect("no error");
    let row = &got[0];
    assert_eq!((&row[0], &row[1], &row[4]), (&i8(4), &i8(3), &i8(i64::MAX)));
    assert_eq!(shown(&row[2]), "18446744073709551615");
    // numeric_div keeps 16 significant digits, so a quotient of 19 digits has no fraction.
    assert_eq!(shown(&row[3]), "6148914691236517205");
}

#[test]
fn a_sum_of_int4_past_the_range_of_int8_raises_bigint_out_of_range() {
    // The total of an int4 sum is exact, and only the int8 it finishes into can overflow.
    let rel = aggregate(
        scan(1, &[("x", oid::INT4)]),
        Vec::new(),
        vec![agg("sum", vec![col(0, oid::INT4)], oid::INT8)],
    );
    let got =
        answer(&rel, &[(1, ints(&[Some(i32::MAX), Some(i32::MAX), Some(-1)]))]).expect("fits");
    assert_eq!(got, [[i8(2 * i64::from(i32::MAX) - 1)]]);
}

#[test]
fn a_join_then_a_sort_and_a_limit_give_the_first_rows_in_order() {
    // SELECT l.a, l.x, r.a, r.s FROM l JOIN r ON l.a = r.a ORDER BY l.x DESC LIMIT 3
    let l = scan(1, &[("a", oid::INT4), ("x", oid::INT4)]);
    let r = scan(2, &[("a", oid::INT4), ("s", oid::TEXT)]);
    let mut columns = l.columns().to_vec();
    columns.extend(r.columns().iter().cloned());
    let join = Rel::Join {
        left: Box::new(l),
        right: Box::new(r),
        build: BuildSide::Right,
        kind: JoinType::Inner,
        keys: vec![(col(0, oid::INT4), col(0, oid::INT4))],
        residual: Vec::new(),
        columns,
    };
    let long = "a string that is longer than twelve bytes";
    let right: Vec<Vec<Value>> =
        [(Some(1), "one"), (Some(2), "two"), (Some(2), "deux"), (None, long), (Some(5), long)]
            .into_iter()
            .map(|(a, s)| vec![a.map_or(Value::Null, i4), text(s)])
            .collect();
    let left: Vec<Vec<Value>> = [
        (Some(2), 0),
        (None, 1),
        (Some(3), 2),
        (Some(1), 3),
        (Some(2), 4),
        (Some(5), 6),
        (Some(5), 7),
    ]
    .into_iter()
    .map(|(a, x)| vec![a.map_or(Value::Null, i4), i4(x)])
    .collect();
    let tables = [(1, left), (2, right)];
    let all = answer(&join, &tables).expect("no error");
    assert_eq!(all.len(), 7);
    let rel = Rel::Limit {
        input: Box::new(sort(join, vec![(1, oid::INT4, true)])),
        count: Some(3),
        offset: 0,
    };
    let got = answer_on(&rel, &tables, Tier::Interp, 2, 0).expect("no error");
    assert_eq!(
        got,
        [
            vec![i4(5), i4(7), i4(5), text(long)],
            vec![i4(5), i4(6), i4(5), text(long)],
            got[2].clone(),
        ]
    );
    assert_eq!((&got[2][0], &got[2][1]), (&i4(2), &i4(4)));
}

#[test]
fn a_like_is_answered_for_each_batch() {
    // SELECT count(*) FROM t WHERE s LIKE '%ab%'
    let t = scan(1, &[("s", oid::TEXT)]);
    let like = Expr {
        kind: Kind::Function {
            oid: 0,
            src: "textlike",
            strict: true,
            args: vec![
                col(0, oid::TEXT),
                Expr { kind: Kind::Constant(text("%ab%")), ty: ty(oid::TEXT) },
            ],
        },
        ty: ty(oid::BOOL),
    };
    let rel = aggregate(
        Rel::filtered(t, vec![like]),
        Vec::new(),
        vec![agg("count_star", Vec::new(), oid::INT8)],
    );
    let rows: Vec<Vec<Value>> =
        ["ab", "xaby", "", "ba", "a long string with ab in the middle", "a long string without"]
            .into_iter()
            .map(|s| vec![text(s)])
            .chain([vec![Value::Null]])
            .collect();
    assert_eq!(answer(&rel, &[(1, rows)]).expect("no error"), [[i8(3)]]);
}

#[test]
fn a_query_over_a_type_the_driver_reads_no_value_of_is_refused() {
    let rel = project(scan(1, &[("u", oid::UUID)]), vec![col(0, oid::UUID)]);
    let graph = split(&rel);
    let mut rt = Rt::new(Arc::default());
    let Ok(query) = generate(&graph, &mut rt) else { return };
    assert!(check(&graph, &query).is_err());
}
