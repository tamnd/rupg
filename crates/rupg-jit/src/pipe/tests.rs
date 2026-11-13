use rupg_types::{Value, oid};

use super::*;
use crate::plan::{CompareOp, Type};

fn int4() -> Type {
    Type::new(oid::INT4)
}

fn col(i: usize, ty: u32) -> Expr {
    Expr::column(i, Type::new(ty))
}

fn named(name: &str, ty: u32) -> Column {
    Column { name: name.to_owned(), ty: Type::new(ty) }
}

/// A scan of `table` reading `columns`, each `(name, type)`, as attributes 1 and on.
fn scan(relation: u32, table: &str, columns: &[(&str, u32)]) -> Rel {
    Rel::Scan {
        relation,
        table: table.to_owned(),
        attnums: (1..).take(columns.len()).collect(),
        columns: columns.iter().map(|(n, t)| named(n, *t)).collect(),
    }
}

fn compare(op: CompareOp, left: Expr, right: Expr) -> Expr {
    let kind = Kind::Compare { op, left: Box::new(left), right: Box::new(right) };
    Expr { kind, ty: Type::new(oid::BOOL) }
}

fn count_star() -> Aggregate {
    Aggregate {
        name: "count_star",
        oid: 2803,
        args: Vec::new(),
        distinct: false,
        filter: None,
        ty: Type::new(oid::INT8),
    }
}

fn aggregate(input: Rel, groups: Vec<Expr>, aggregates: Vec<Aggregate>, out: &[&str]) -> Rel {
    let types = groups.iter().map(|g| g.ty).chain(aggregates.iter().map(|a| a.ty));
    let columns =
        out.iter().zip(types).map(|(n, ty)| Column { name: (*n).to_owned(), ty }).collect();
    Rel::Aggregate { input: Box::new(input), groups, aggregates, columns, runs: None }
}

fn join(
    left: Rel,
    right: Rel,
    build: BuildSide,
    kind: JoinType,
    keys: Vec<(Expr, Expr)>,
    residual: Vec<Expr>,
) -> Rel {
    let mut columns = left.columns().to_vec();
    if kind == JoinType::Inner {
        columns.extend(right.columns().iter().cloned());
    }
    Rel::Join { left: Box::new(left), right: Box::new(right), build, kind, keys, residual, columns }
}

fn l() -> Rel {
    scan(16384, "l", &[("a", oid::INT4), ("x", oid::INT4)])
}

fn r() -> Rel {
    scan(16385, "r", &[("a", oid::INT4), ("y", oid::INT4)])
}

#[test]
fn filters_and_projections_fold_into_the_pipeline_before_the_breaker() {
    // SELECT "SearchPhrase", count(*) AS c FROM hits WHERE "SearchPhrase" <> ''
    // GROUP BY 1 ORDER BY c DESC LIMIT 10
    let hits = scan(16386, "hits", &[("SearchPhrase", oid::TEXT)]);
    let empty = Expr { kind: Kind::Constant(Value::Text(String::new())), ty: Type::new(oid::TEXT) };
    let filter = Rel::filtered(hits, vec![compare(CompareOp::NotEqual, col(0, oid::TEXT), empty)]);
    let agg =
        aggregate(filter, vec![col(0, oid::TEXT)], vec![count_star()], &["?column?", "count"]);
    let project = Rel::Project {
        input: Box::new(agg),
        exprs: vec![col(0, oid::TEXT), col(1, oid::INT8)],
        columns: vec![named("SearchPhrase", oid::TEXT), named("c", oid::INT8)],
    };
    let keys = vec![Key { expr: col(1, oid::INT8), descending: true, nulls_first: true }];
    let g = split(&Rel::TopN { input: Box::new(project), keys, count: 10, offset: 0 });
    assert_eq!(g.stages.len(), 2, "{g}");
    let Stage::Pipeline(agg) = &g.stages[0] else { panic!("{g}") };
    assert!(matches!(agg.ops[..], [Op::Filter(_)]));
    assert_eq!(agg.slots(), [SlotKind::AggTable]);
    assert_eq!(agg.steps(), [Step::Init, Step::Body, Step::Finalize]);
    assert_eq!(g.edges(), [Edge { from: 0, to: 1, kind: EdgeKind::Finalize }]);
    assert!(matches!(agg.sink, Sink::Aggregate { .. }));
    assert!(
        matches!(&agg.source, Source::Scan { relation: 16386, attnums, .. } if attnums == &[1])
    );
    // The projection over the aggregate only renames, so the top N reads the aggregate.
    assert!(matches!(g.stages[1], Stage::TopN { input: 0, .. }));
    assert_eq!(g.columns()[0].name, "SearchPhrase");
}

#[test]
fn a_projection_that_only_renames_the_answer_keeps_its_names() {
    let agg =
        aggregate(scan(16384, "a", &[("x", oid::INT4)]), vec![], vec![count_star()], &["count"]);
    let project = Rel::Project {
        input: Box::new(agg),
        exprs: vec![col(0, oid::INT8)],
        columns: vec![named("revenue", oid::INT8)],
    };
    let g = split(&project);
    assert_eq!(g.stages.len(), 1, "{g}");
    assert_eq!(g.columns()[0].name, "revenue");
}

#[test]
fn a_filter_above_a_projection_reads_the_source() {
    let one = Expr { kind: Kind::Constant(Value::Int4(1)), ty: int4() };
    let plus = Kind::Function {
        oid: 177,
        src: "int4pl",
        strict: true,
        args: vec![col(0, oid::INT4), one],
    };
    let project = Rel::Project {
        input: Box::new(scan(16384, "a", &[("x", oid::INT4)])),
        exprs: vec![Expr { kind: plus, ty: int4() }],
        columns: vec![named("y", oid::INT4)],
    };
    let three = Expr { kind: Kind::Constant(Value::Int4(3)), ty: int4() };
    let g =
        split(&Rel::filtered(project, vec![compare(CompareOp::Greater, col(0, oid::INT4), three)]));
    let Stage::Pipeline(p) = &g.stages[0] else { panic!("{g}") };
    assert_eq!(p.steps(), [Step::Init, Step::Body]);
    assert!(g.edges().is_empty());
    let Op::Filter(filter) = &p.ops[0] else { panic!("{g}") };
    let Kind::Compare { left, .. } = &filter.kind else { panic!("{g}") };
    assert!(matches!(&left.kind, Kind::Function { src: "int4pl", .. }));
    assert_eq!(left.columns(), [0]);
}

#[test]
fn a_join_builds_one_side_and_probes_it_inline_on_the_other() {
    let keys = vec![(col(0, oid::INT4), col(0, oid::INT4))];
    let j = join(l(), r(), BuildSide::Right, JoinType::Inner, keys, Vec::new());
    let filter =
        Rel::filtered(j, vec![compare(CompareOp::Less, col(1, oid::INT4), col(3, oid::INT4))]);
    let g = split(&aggregate(filter, vec![], vec![count_star()], &["count"]));
    assert_eq!(g.stages.len(), 2, "{g}");
    let Stage::Pipeline(build) = &g.stages[0] else { panic!("{g}") };
    assert!(matches!(&build.source, Source::Scan { table, .. } if table == "r"));
    assert!(matches!(&build.sink, Sink::Build { keys, payload, .. }
        if keys.len() == 1 && payload.len() == 2));
    assert_eq!(build.slots(), [SlotKind::HtBuild]);
    assert_eq!(build.steps(), [Step::Init, Step::Body, Step::Finalize]);
    let Stage::Pipeline(probe) = &g.stages[1] else { panic!("{g}") };
    assert_eq!(probe.slots(), [SlotKind::ScalarAcc, SlotKind::HtProbe]);
    let [Op::Probe(p), Op::Filter(f)] = &probe.ops[..] else { panic!("{g}") };
    assert_eq!((p.build, p.first), (0, 2));
    assert_eq!(p.keys[0].columns(), [0]);
    // The filter reads the probe side's second column and the build side's second, which is the
    // second payload column.
    assert_eq!(f.columns(), [1, 3]);
    assert_eq!(probe.columns().len(), 4);
    assert_eq!(g.edges(), [Edge { from: 0, to: 1, kind: EdgeKind::Finalize }]);
    assert!(g.to_string().contains("probe stage 0"), "{g}");
}

#[test]
fn a_semi_probe_tests_its_residual_and_passes_on_only_the_probe_side() {
    let keys = vec![(col(0, oid::INT4), col(0, oid::INT4))];
    let residual = vec![compare(CompareOp::NotEqual, col(1, oid::INT4), col(3, oid::INT4))];
    let j = join(l(), r(), BuildSide::Right, JoinType::Semi, keys.clone(), residual);
    let sum = Aggregate {
        name: "sum",
        oid: 2108,
        args: vec![col(1, oid::INT4)],
        distinct: false,
        filter: None,
        ty: Type::new(oid::INT8),
    };
    let g = split(&aggregate(j, vec![], vec![sum], &["sum"]));
    let Stage::Pipeline(probe) = &g.stages[1] else { panic!("{g}") };
    let [Op::Probe(p)] = &probe.ops[..] else { panic!("{g}") };
    assert_eq!((p.kind, p.first), (JoinType::Semi, 2));
    assert_eq!(p.residual[0].columns(), [1, 3]);
    assert!(g.to_string().contains("semi probe stage 0"), "{g}");

    let g = split(&join(l(), r(), BuildSide::Right, JoinType::Anti, keys, Vec::new()));
    let Stage::Pipeline(build) = &g.stages[0] else { panic!("{g}") };
    assert!(matches!(&build.sink, Sink::Build { payload, .. } if payload.is_empty()), "{g}");
    let Stage::Pipeline(probe) = &g.stages[1] else { panic!("{g}") };
    assert_eq!(probe.columns().len(), 2);
    assert!(matches!(&probe.sink, Sink::Result { exprs, .. } if exprs.len() == 2), "{g}");
}

#[test]
fn an_anti_join_that_builds_on_its_left_marks_the_table_and_reads_it_back() {
    let r = scan(16385, "r", &[("a", oid::INT4), ("y", oid::INT4), ("z", oid::INT4)]);
    let keys = vec![(col(0, oid::INT4), col(0, oid::INT4))];
    let residual = vec![compare(CompareOp::NotEqual, col(1, oid::INT4), col(3, oid::INT4))];
    let g = split(&join(l(), r, BuildSide::Left, JoinType::Anti, keys, residual));
    assert_eq!(g.stages.len(), 3, "{g}");
    let Stage::Pipeline(build) = &g.stages[0] else { panic!("{g}") };
    assert!(
        matches!(&build.sink, Sink::Build { payload, marked: Some(JoinType::Anti), .. }
        if payload.len() == 2),
        "{g}"
    );
    let Stage::Pipeline(probe) = &g.stages[1] else { panic!("{g}") };
    assert!(matches!(probe.sink, Sink::Mark), "{g}");
    let [Op::Probe(p)] = &probe.ops[..] else { panic!("{g}") };
    assert!(p.marks);
    // The payload comes after the probe side's three columns, and the residual reads the left's
    // second column, which is the payload's second.
    assert_eq!(p.first, 3);
    assert_eq!(p.residual[0].columns(), [4, 1]);
    assert!(matches!(&g.stages[2], Stage::Marked { build: 0, probe: 1, semi: false, .. }));
    assert_eq!(g.columns().len(), 2);
    assert!(g.to_string().contains("marking probe stage 0"), "{g}");
}

#[test]
fn a_join_that_builds_on_its_left_keeps_the_columns_in_order() {
    let l = scan(16384, "l", &[("a", oid::INT4), ("x", oid::TEXT)]);
    let r = scan(16385, "r", &[("a", oid::INT4)]);
    let keys = vec![(col(0, oid::INT4), col(0, oid::INT4))];
    let g = split(&join(l, r, BuildSide::Left, JoinType::Inner, keys, Vec::new()));
    let Stage::Pipeline(probe) = &g.stages[1] else { panic!("{g}") };
    assert!(matches!(&probe.source, Source::Scan { table, .. } if table == "r"));
    let Sink::Result { exprs, columns } = &probe.sink else { panic!("{g}") };
    let read: Vec<_> = exprs.iter().map(Expr::columns).collect();
    assert_eq!(read, [vec![1], vec![2], vec![0]]);
    assert_eq!(columns[1].name, "x");
}
