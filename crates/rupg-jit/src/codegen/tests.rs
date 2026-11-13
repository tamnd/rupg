// Lifted from rudb crates/rudb-qc-gen/src/tests.rs at cd9f9676 (2026-10-05).
// Changes: the trees are built by hand over PostgreSQL types instead of parsed from rudb plans, the `vcall` test is gone with the module, and new tests check PostgreSQL's errors for integer and float arithmetic, that a null never raises one, that `OR` skips a division its first child decides, how a `LIKE` is answered or refused, and the refusal of a function by its `prosrc`.
#![allow(unsafe_code)]

use std::sync::Arc;

use rupg_types::{Value, oid};

use super::*;
use crate::interp::Program;
use crate::ir::status;
use crate::pipe::split;
use crate::plan::{BuildSide, Column, Rel};
use crate::rt::abi::{Col, Morsel};

fn ty(oid: u32) -> Type {
    Type::new(oid)
}

fn col(i: usize, oid: u32) -> Expr {
    Expr::column(i, ty(oid))
}

fn konst(v: Value, oid: u32) -> Expr {
    Expr { kind: Kind::Constant(v), ty: ty(oid) }
}

fn int4(x: i32) -> Expr {
    konst(Value::Int4(x), oid::INT4)
}

fn text_of(s: &str) -> Expr {
    konst(Value::Text(s.to_owned()), oid::TEXT)
}

/// A call of the strict function with the `prosrc` `src`.
fn call_of(src: &'static str, oid: u32, args: Vec<Expr>) -> Expr {
    Expr { kind: Kind::Function { oid: 0, src, strict: true, args }, ty: ty(oid) }
}

fn compare(op: CompareOp, left: Expr, right: Expr) -> Expr {
    let kind = Kind::Compare { op, left: Box::new(left), right: Box::new(right) };
    Expr { kind, ty: ty(oid::BOOL) }
}

fn scan(table: &str, columns: &[(&str, u32)]) -> Rel {
    Rel::Scan {
        relation: 16384,
        table: table.to_owned(),
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

fn generated(rel: &Rel, rt: &mut Rt) -> Query {
    generate(&split(rel), rt).expect("generates")
}

/// A source column as the driver hands it over: values at their width and a validity bitmap.
#[derive(Clone)]
struct Data {
    values: Vec<u8>,
    valid: Vec<u8>,
}

fn ints(values: &[Option<i32>]) -> Data {
    let mut c = Data { values: Vec::new(), valid: vec![0; values.len().div_ceil(8)] };
    for (i, v) in values.iter().enumerate() {
        c.values.extend_from_slice(&v.unwrap_or(0).to_le_bytes());
        if v.is_some() {
            c.valid[i / 8] |= 1 << (i % 8);
        }
    }
    c
}

fn floats(values: &[Option<f64>]) -> Data {
    let mut c = Data { values: Vec::new(), valid: vec![0; values.len().div_ceil(8)] };
    for (i, v) in values.iter().enumerate() {
        c.values.extend_from_slice(&v.unwrap_or(0.0).to_le_bytes());
        if v.is_some() {
            c.valid[i / 8] |= 1 << (i % 8);
        }
    }
    c
}

fn strings(values: &[&'static str]) -> Data {
    let mut c = Data { values: Vec::new(), valid: vec![0xff; values.len().div_ceil(8)] };
    for v in values {
        c.values.extend_from_slice(&text::make(v.as_bytes()).to_le_bytes());
    }
    c
}

/// The codes a flat column is read through, each row its own.
static ROWS: [u32; 8] = [0, 1, 2, 3, 4, 5, 6, 7];

/// Runs the body of `stage` over the eight rows of `columns` in one morsel, with `state` set up
/// by the caller.
fn run(query: &Query, stage: usize, rt: &mut Rt, columns: &[Data], state: &mut [u8]) {
    let status = call(query, stage, rt, columns, state);
    assert_eq!(status, 0, "{:?} {:?}", rt.take_error(), error_of(query, status));
}

/// Like [`run`], and returns the status rather than wanting it to be `Ok`.
fn call(query: &Query, stage: usize, rt: &mut Rt, columns: &[Data], state: &mut [u8]) -> u64 {
    let body = query.bodies[stage].as_ref().expect("a pipeline");
    // Each `LIKE` or comparison the body reads as a column is answered here for the morsel, as
    // the driver does, into one byte a row read through the rows as codes.
    let answers: Vec<Vec<u8>> = body
        .likes
        .iter()
        .map(|m| {
            let like = rt.like(m.like).expect("the pattern the body was made with");
            let values = &columns[m.column].values;
            (0..8)
                .map(|i| {
                    let header =
                        u128::from_le_bytes(values[16 * i..16 * (i + 1)].try_into().unwrap());
                    // SAFETY: the header was made by `text::make` over a `'static` string.
                    u8::from(like.matches(unsafe { text::bytes(&header) }))
                })
                .collect()
        })
        .collect();
    let cols: Vec<Col> = body
        .reads
        .iter()
        .map(|&c| Col {
            values: columns[c].values.as_ptr(),
            valid: columns[c].valid.as_ptr(),
            codes: ROWS.as_ptr(),
        })
        .chain(body.likes.iter().zip(&answers).map(|(m, answer)| Col {
            values: answer.as_ptr(),
            valid: columns[m.column].valid.as_ptr(),
            codes: ROWS.as_ptr(),
        }))
        // A body that reads its group rows by the key's codes has one more column for their
        // array, which a null address leaves unused.
        .chain(body.keyed.map(|_| Col {
            values: std::ptr::null(),
            valid: std::ptr::null(),
            codes: std::ptr::null(),
        }))
        .collect();
    let morsel = Morsel {
        source: 0,
        chunk: 0,
        begin: 0,
        end: 8,
        seq: 0,
        enc: 0,
        flags: 0,
        cols: cols.as_ptr(),
    };
    let program = Program::new(&query.module);
    let f = program.func(&body.func).expect("the function");
    program.call(f, state.as_mut_ptr(), (&raw const morsel).cast(), rt)
}

/// The error a status raises: its kind and PostgreSQL's message.
fn error_of(query: &Query, st: u64) -> Option<(ErrorKind, String)> {
    (status::kind(st) == status::ERROR).then(|| {
        let site = &query.module.errors[status::payload(st) as usize];
        (site.kind, site.text.clone())
    })
}

fn put(st: &mut [u8], at: u32, word: u64) {
    st[at as usize..][..8].copy_from_slice(&word.to_le_bytes());
}

/// Runs a one column projection over `columns` and returns the status, the values as `i32`s and
/// their validity.
fn project_ints(rel: &Rel, columns: &[Data]) -> (Query, u64, Vec<Option<i32>>) {
    let mut rt = Rt::new(Arc::default());
    let query = generated(rel, &mut rt);
    let Some(Body { sink: Out::Result { count, columns: slots, .. }, state, .. }) =
        &query.bodies[0]
    else {
        panic!("{:?}", query.bodies)
    };
    let mut out = vec![0u8; 8 * 4];
    let mut valid = vec![0u8; 8];
    let mut st = vec![0u8; *state as usize];
    put(&mut st, slots[0].values, out.as_mut_ptr() as u64);
    put(&mut st, slots[0].valid, valid.as_mut_ptr() as u64);
    let status = call(&query, 0, &mut rt, columns, &mut st);
    let n = u64::from_le_bytes(st[*count as usize..][..8].try_into().unwrap()) as usize;
    let got = (0..n)
        .map(|i| {
            let v = i32::from_le_bytes(out[4 * i..4 * i + 4].try_into().unwrap());
            (valid[i] == 1).then_some(v)
        })
        .collect();
    (query, status, got)
}

#[test]
fn a_filter_and_a_projection_write_the_rows_that_pass() {
    // SELECT x + 1 FROM a WHERE x > 3
    let a = scan("a", &[("x", oid::INT4)]);
    let filter = Rel::filtered(a, vec![compare(CompareOp::Greater, col(0, oid::INT4), int4(3))]);
    let rel = project(filter, vec![call_of("int4pl", oid::INT4, vec![col(0, oid::INT4), int4(1)])]);
    let x = ints(&[Some(1), Some(5), None, Some(7), Some(2), Some(9), Some(0), Some(4)]);
    let (_, status, got) = project_ints(&rel, &[x]);
    assert_eq!(status, 0);
    assert_eq!(got, [Some(6), Some(8), Some(10), Some(5)]);
}

#[test]
fn a_grouped_count_over_strings_makes_one_group_per_value() {
    // SELECT s, count(*), min(n) FROM hits WHERE s <> '' GROUP BY s
    let hits = scan("hits", &[("s", oid::TEXT), ("n", oid::INT4)]);
    let filter =
        Rel::filtered(hits, vec![compare(CompareOp::NotEqual, col(0, oid::TEXT), text_of(""))]);
    let rel = aggregate(
        filter,
        vec![col(0, oid::TEXT)],
        vec![
            agg("count_star", Vec::new(), oid::INT8),
            agg("min", vec![col(1, oid::INT4)], oid::INT4),
        ],
    );
    let mut rt = Rt::new(Arc::default());
    let query = generated(&rel, &mut rt);
    let Some(Body { sink: Out::Aggregate(grouping), state, .. }) = &query.bodies[0] else {
        panic!("{:?}", query.bodies)
    };
    let long = "a string that is longer than twelve bytes";
    let s = strings(&["a", "", "bb", "a", long, long, "a", ""]);
    let n = ints(&[Some(5), Some(1), Some(2), Some(3), None, Some(8), Some(4), Some(0)]);
    let mut st = vec![0u8; *state as usize];
    run(&query, 0, &mut rt, &[s, n], &mut st);
    let table = rt.table(grouping.table).expect("the table");
    assert_eq!(table.len(), 3);
    let mut groups = Vec::new();
    for gid in 0..table.len() {
        let row = table.row(gid);
        let key = u128::from_le_bytes(row[8..24].try_into().unwrap());
        // SAFETY: the table copied long keys into the runtime heap, which `rt` keeps.
        let key = String::from_utf8(unsafe { text::bytes(&key) }.to_vec()).unwrap();
        let acc = &row[grouping.acc_offset as usize..];
        let count = i64::from_le_bytes(acc[..8].try_into().unwrap());
        let min = i32::from_le_bytes(acc[8..12].try_into().unwrap());
        groups.push((key, count, min));
    }
    groups.sort();
    assert_eq!(groups, [("a".to_owned(), 3, 3), (long.to_owned(), 2, 8), ("bb".to_owned(), 1, 2)]);
}

/// Runs an aggregate with no groups over `columns` and returns the bytes of its accumulators, or
/// the error the body raised.
fn ungrouped(rel: &Rel, columns: &[Data]) -> std::result::Result<Vec<u8>, (ErrorKind, String)> {
    let mut rt = Rt::new(Arc::default());
    let query = generated(rel, &mut rt);
    let Some(Body { sink: Out::Aggregate(grouping), state, .. }) = &query.bodies[0] else {
        panic!("{:?}", query.bodies)
    };
    let mut st = vec![0u8; *state as usize];
    let row = rt.table(grouping.table).expect("the table").address(0);
    put(&mut st, grouping.row.expect("a row slot"), row as u64);
    let status = call(&query, 0, &mut rt, columns, &mut st);
    if let Some(e) = error_of(&query, status) {
        return Err(e);
    }
    assert_eq!(status, 0);
    let row = rt.table(grouping.table).expect("the table").row(0);
    Ok(row[grouping.acc_offset as usize..].to_vec())
}

#[test]
fn an_ungrouped_aggregate_updates_the_one_row() {
    // SELECT sum(x), count(x), max(x) FROM a, where the sum of an int4 is an int8
    let a = scan("a", &[("x", oid::INT4)]);
    let x = || col(0, oid::INT4);
    let rel = aggregate(
        a,
        Vec::new(),
        vec![
            agg("sum", vec![x()], oid::INT8),
            agg("count", vec![x()], oid::INT8),
            agg("max", vec![x()], oid::INT4),
        ],
    );
    let data = ints(&[Some(-4), None, Some(10), Some(3), None, None, None, None]);
    let acc = ungrouped(&rel, &[data]).expect("no error");
    // The sum is an i128 total and the i64 part that has not spilled into it yet.
    let total = i128::from_le_bytes(acc[..16].try_into().unwrap());
    let part = i64::from_le_bytes(acc[16..24].try_into().unwrap());
    let count = i64::from_le_bytes(acc[32..40].try_into().unwrap());
    let max = i32::from_le_bytes(acc[40..44].try_into().unwrap());
    assert_eq!((total + i128::from(part), acc[24], count, max, acc[44]), (9, 1, 3, 10, 1));
}

#[test]
fn a_sum_of_float8_raises_the_overflow_of_float8pl() {
    let a = scan("a", &[("x", oid::FLOAT8)]);
    let rel = aggregate(a, Vec::new(), vec![agg("sum", vec![col(0, oid::FLOAT8)], oid::FLOAT8)]);
    let small = floats(&[Some(1.5), None, Some(-0.25), None, None, None, None, None]);
    let acc = ungrouped(&rel, &[small]).expect("no error");
    assert_eq!((f64::from_le_bytes(acc[..8].try_into().unwrap()), acc[8]), (1.25, 1));
    let big = floats(&[Some(1e308), Some(1e308), None, None, None, None, None, None]);
    let err = ungrouped(&rel, &[big]).expect_err("the sum overflows");
    assert_eq!(err, (ErrorKind::Overflow, "value out of range: overflow".to_owned()));
    // An infinity that is already there is no overflow.
    let inf = floats(&[Some(f64::INFINITY), Some(1e308), None, None, None, None, None, None]);
    let acc = ungrouped(&rel, &[inf]).expect("no error");
    assert_eq!(f64::from_le_bytes(acc[..8].try_into().unwrap()), f64::INFINITY);
}

#[test]
fn integer_arithmetic_raises_the_errors_of_postgresql() {
    let ab = || scan("a", &[("x", oid::INT4), ("y", oid::INT4)]);
    let (x, y) = (|| col(0, oid::INT4), || col(1, oid::INT4));
    let plus = project(ab(), vec![call_of("int4pl", oid::INT4, vec![x(), y()])]);
    let xs = ints(&[Some(i32::MAX), Some(1), None, None, None, None, None, None]);
    let ys = ints(&[Some(1), Some(2), None, None, None, None, None, None]);
    let (query, status, _) = project_ints(&plus, &[xs, ys]);
    assert_eq!(
        error_of(&query, status),
        Some((ErrorKind::Overflow, "integer out of range".to_owned()))
    );

    // A null divisor is a null, and a zero one is an error.
    let div = project(ab(), vec![call_of("int4div", oid::INT4, vec![x(), y()])]);
    let xs = ints(&[Some(10), Some(7), None, Some(-9), Some(i32::MIN), None, None, None]);
    let ys = ints(&[Some(2), None, Some(0), Some(2), Some(1), None, None, None]);
    let (_, status, got) = project_ints(&div, &[xs.clone(), ys]);
    assert_eq!(status, 0);
    assert_eq!(got, [Some(5), None, None, Some(-4), Some(i32::MIN), None, None, None]);
    let zero = ints(&[Some(2), Some(0), None, None, None, None, None, None]);
    let (query, status, _) = project_ints(&div, &[xs.clone(), zero.clone()]);
    assert_eq!(
        error_of(&query, status),
        Some((ErrorKind::DivideByZero, "division by zero".to_owned()))
    );
    let least = ints(&[Some(i32::MIN), None, None, None, None, None, None, None]);
    let minus = ints(&[Some(-1), None, None, None, None, None, None, None]);
    let (query, status, _) = project_ints(&div, &[least.clone(), minus.clone()]);
    assert_eq!(
        error_of(&query, status),
        Some((ErrorKind::Overflow, "integer out of range".to_owned()))
    );

    // The remainder of the least value by -1 is 0, as in `int4mod`.
    let rem = project(ab(), vec![call_of("int4mod", oid::INT4, vec![x(), y()])]);
    let (_, status, got) = project_ints(&rem, &[least, minus]);
    assert_eq!(status, 0);
    assert_eq!(got[0], Some(0));
    let (query, status, _) = project_ints(&rem, &[xs, zero]);
    assert_eq!(
        error_of(&query, status),
        Some((ErrorKind::DivideByZero, "division by zero".to_owned()))
    );
}

#[test]
fn a_narrowing_cast_raises_the_range_error_of_its_type() {
    let a = || scan("a", &[("x", oid::INT8)]);
    let rel = project(a(), vec![call_of("int84", oid::INT4, vec![col(0, oid::INT8)])]);
    let mut rt = Rt::new(Arc::default());
    let query = generated(&rel, &mut rt);
    let Some(Body { sink: Out::Result { columns: slots, .. }, state, .. }) = &query.bodies[0]
    else {
        panic!("{:?}", query.bodies)
    };
    let mut c = Data { values: Vec::new(), valid: vec![0x01] };
    for v in [1i64 << 31, 0, 0, 0, 0, 0, 0, 0] {
        c.values.extend_from_slice(&v.to_le_bytes());
    }
    let (mut out, mut valid) = (vec![0u8; 32], vec![0u8; 8]);
    let mut st = vec![0u8; *state as usize];
    put(&mut st, slots[0].values, out.as_mut_ptr() as u64);
    put(&mut st, slots[0].valid, valid.as_mut_ptr() as u64);
    let status = call(&query, 0, &mut rt, &[c.clone()], &mut st);
    assert_eq!(
        error_of(&query, status),
        Some((ErrorKind::Overflow, "integer out of range".to_owned()))
    );
    // The same value under a null is no error.
    c.valid = vec![0];
    put(&mut st, SINK, 0);
    assert_eq!(call(&query, 0, &mut rt, &[c], &mut st), 0);
}

#[test]
fn or_skips_a_division_its_first_child_decides() {
    // SELECT x FROM a WHERE y = 0 OR x / y > 1, which PostgreSQL runs without dividing by zero.
    let a = scan("a", &[("x", oid::INT4), ("y", oid::INT4)]);
    let (x, y) = (|| col(0, oid::INT4), || col(1, oid::INT4));
    let div = call_of("int4div", oid::INT4, vec![x(), y()]);
    let or = Expr {
        kind: Kind::Or(vec![
            compare(CompareOp::Equal, y(), int4(0)),
            compare(CompareOp::Greater, div, int4(1)),
        ]),
        ty: ty(oid::BOOL),
    };
    let rel = project(Rel::filtered(a, vec![or]), vec![x()]);
    let xs = ints(&[Some(1), Some(9), Some(4), Some(3), None, Some(6), Some(2), Some(8)]);
    let ys = ints(&[Some(0), Some(3), Some(4), Some(0), Some(0), None, Some(1), Some(2)]);
    let (query, status, got) = project_ints(&rel, &[xs, ys]);
    assert_eq!(status, 0, "{:?}", error_of(&query, status));
    assert_eq!(got, [Some(1), Some(9), Some(3), None, Some(2), Some(8)]);
}

#[test]
fn a_like_is_answered_for_the_morsel_or_refused_with_an_escape() {
    // SELECT n FROM a WHERE s LIKE 'a%'
    let a = || scan("a", &[("s", oid::TEXT), ("n", oid::INT4)]);
    let like = |p: &str| call_of("textlike", oid::BOOL, vec![col(0, oid::TEXT), text_of(p)]);
    let rel = project(Rel::filtered(a(), vec![like("a%")]), vec![col(1, oid::INT4)]);
    let s = strings(&["abc", "b", "a", "ba", "", "a string that is longer than twelve", "x", "aa"]);
    let n = ints(&[Some(0), Some(1), Some(2), Some(3), Some(4), Some(5), Some(6), Some(7)]);
    let (query, status, got) = project_ints(&rel, &[s.clone(), n.clone()]);
    assert_eq!(status, 0);
    assert_eq!(query.bodies[0].as_ref().map(|b| b.likes.len()), Some(1));
    assert_eq!(got, [Some(0), Some(2), Some(5), Some(7)]);

    // The same `NOT LIKE` past a filter is asked a row at a time.
    let first = compare(CompareOp::Greater, col(1, oid::INT4), int4(0));
    let not = call_of("textnlike", oid::BOOL, vec![col(0, oid::TEXT), text_of("a%")]);
    let rel =
        project(Rel::filtered(Rel::filtered(a(), vec![first]), vec![not]), vec![col(1, oid::INT4)]);
    let (query, status, got) = project_ints(&rel, &[s, n]);
    assert_eq!(status, 0);
    assert_eq!(query.bodies[0].as_ref().map(|b| b.likes.len()), Some(0));
    assert_eq!(got, [Some(1), Some(3), Some(4), Some(6)]);

    // A backslash is PostgreSQL's escape character, which the runtime does not have.
    let rel = project(Rel::filtered(a(), vec![like("a\\%")]), vec![col(1, oid::INT4)]);
    let refusal = generate(&split(&rel), &mut Rt::new(Arc::default())).expect_err("refused");
    assert_eq!(refusal.what, "textlike(text, text)");
}

#[test]
fn a_function_with_no_translator_is_refused_by_its_prosrc() {
    let a = scan("a", &[("s", oid::TEXT)]);
    let rel = project(a, vec![call_of("upper", oid::TEXT, vec![col(0, oid::TEXT)])]);
    let refusal = generate(&split(&rel), &mut Rt::new(Arc::default())).expect_err("refused");
    assert_eq!(refusal.what, "upper(text)");
}

#[test]
fn a_join_builds_its_table_and_probes_it_once_per_match() {
    let l = scan("l", &[("a", oid::INT4), ("x", oid::INT4)]);
    let r = scan("r", &[("a", oid::INT4), ("s", oid::TEXT)]);
    let mut columns = l.columns().to_vec();
    columns.extend(r.columns().iter().cloned());
    let rel = Rel::Join {
        left: Box::new(l),
        right: Box::new(r),
        build: BuildSide::Right,
        kind: JoinType::Inner,
        keys: vec![(col(0, oid::INT4), col(0, oid::INT4))],
        residual: Vec::new(),
        columns,
    };
    let mut rt = Rt::new(Arc::default());
    let query = generated(&rel, &mut rt);

    // The build reads the right side, drops the null key and keeps both duplicates of 2.
    let Some(Body { sink: Out::Build(b), state, .. }) = &query.bodies[0] else {
        panic!("{:?}", query.bodies)
    };
    let long = "a string that is longer than twelve bytes";
    let ra = ints(&[Some(1), Some(2), Some(2), None, Some(5), Some(6), Some(7), Some(8)]);
    let rs = strings(&["one", "two", "deux", long, long, "six", "seven", "eight"]);
    let mut st = vec![0u8; *state as usize];
    run(&query, 0, &mut rt, &[ra, rs], &mut st);
    drop(st);
    let published = rt.finish_join(b.table).expect("the table finishes");
    assert_eq!(published.rows, 7);

    let Some(Body { sink: Out::Result { count, columns, capacity }, state, probes, .. }) =
        &query.bodies[1]
    else {
        panic!("{:?}", query.bodies)
    };
    let capacity = capacity.expect("a probe pipeline has a capacity");
    let la = ints(&[Some(2), None, Some(3), Some(1), Some(2), Some(9), Some(5), Some(5)]);
    let lx = ints(&[Some(0), Some(1), Some(2), Some(3), Some(4), Some(5), Some(6), Some(7)]);
    let run_with = |rt: &mut Rt, room: u64| {
        let mut out: Vec<Vec<u128>> = vec![vec![0; room as usize]; 4];
        let mut valid: Vec<Vec<u8>> = vec![vec![0; room as usize]; 4];
        let mut st = vec![0u8; *state as usize];
        for (k, slot) in columns.iter().enumerate() {
            put(&mut st, slot.values, out[k].as_mut_ptr() as u64);
            put(&mut st, slot.valid, valid[k].as_mut_ptr() as u64);
        }
        put(&mut st, capacity, room);
        put(&mut st, probes[0].directory, published.directory as u64);
        put(&mut st, probes[0].shift, published.shift);
        put(&mut st, probes[0].tags, published.tags as u64);
        let status = call(&query, 1, rt, &[la.clone(), lx.clone()], &mut st);
        let n = u64::from_le_bytes(st[*count as usize..][..8].try_into().unwrap());
        (status, n, out)
    };
    let (status, _, _) = run_with(&mut rt, 4);
    assert_eq!(status, NEED_MEMORY, "seven matches do not fit in four rows");
    let (status, n, out) = run_with(&mut rt, 16);
    assert_eq!((status, n), (0, 7));
    let mut rows = Vec::new();
    for i in 0..n as usize {
        let word = |k: usize, w: usize| {
            // SAFETY: the buffers are u128s, read here as the bytes the body wrote.
            let bytes = unsafe {
                std::slice::from_raw_parts(out[k].as_ptr().cast::<u8>(), out[k].len() * 16)
            };
            i32::from_le_bytes(bytes[i * w..i * w + 4].try_into().unwrap())
        };
        let s = out[3][i];
        // SAFETY: the long string was copied into the runtime heap when it went into the table.
        let s = String::from_utf8(unsafe { text::bytes(&s) }.to_vec()).unwrap();
        rows.push((word(0, 4), word(1, 4), word(2, 4), s));
    }
    rows.sort();
    let want = [
        (1, 3, 1, "one"),
        (2, 0, 2, "deux"),
        (2, 0, 2, "two"),
        (2, 4, 2, "deux"),
        (2, 4, 2, "two"),
        (5, 6, 5, long),
        (5, 7, 5, long),
    ];
    let want: Vec<_> = want.iter().map(|(a, b, c, d)| (*a, *b, *c, (*d).to_owned())).collect();
    assert_eq!(rows, want);
}
