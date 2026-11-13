use rupg_analyze::{
    Aggref, BoolOp, BoolTest, Case, CastForm, Expr as Analyzed, ExprKind, Func, FuncForm, Var,
};
use rupg_pgcatalog::builtin;
use rupg_types::{Value, oid};

use super::*;

/// The OID of the built-in function with the `prosrc` `src`.
fn proc(src: &str) -> u32 {
    builtin::procs().iter().find(|p| p.src == src).map(|p| p.oid).expect("a built-in function")
}

/// The OID of the built-in aggregate `name` of one argument of type `arg`.
fn agg_oid(name: &str, arg: u32) -> u32 {
    builtin::procs_named(name)
        .find(|p| p.kind == b'a' && p.argtypes == [arg])
        .map(|p| p.oid)
        .expect("a built-in aggregate")
}

fn analyzed(kind: ExprKind, ty: u32) -> Analyzed {
    Analyzed { kind, ty, typmod: -1, location: None, collation: 0 }
}

fn var(attnum: i16, ty: u32) -> Analyzed {
    analyzed(ExprKind::Var(Var { relation: 0, attnum, levels_up: 0 }), ty)
}

fn call(src: &str, args: Vec<Analyzed>, ty: u32) -> Analyzed {
    let f = Func {
        oid: proc(src),
        args,
        form: FuncForm::Call,
        variadic: false,
        retset: false,
        collation: 0,
    };
    analyzed(ExprKind::Func(f), ty)
}

fn aggref(oid: u32, args: Vec<Analyzed>) -> Aggref {
    Aggref {
        oid,
        kind: b'n',
        direct: Vec::new(),
        nargs: args.len(),
        args,
        order: Vec::new(),
        distinct: Vec::new(),
        star: false,
        variadic: false,
        filter: None,
        levels_up: 0,
        collation: 0,
    }
}

/// A table of the columns `a int4`, `b int8`, `c text` and `d numeric`, whose scan reads `c`, `a` and `b` in that order.
const PLACES: [usize; 4] = [1, 2, 0, usize::MAX];

fn scope<'a>(params: &'a [Value]) -> Scope<'a> {
    Scope { relation: 0, places: &PLACES, params }
}

#[test]
fn columns_are_positions_of_the_scan_and_parameters_are_constants() {
    let params = [Value::Int4(7)];
    let sum =
        call("int4pl", vec![var(1, oid::INT4), analyzed(ExprKind::Param(1), oid::INT4)], oid::INT4);
    let e = expr(&sum, &scope(&params)).unwrap();
    let Kind::Function { src: "int4pl", strict: true, args, .. } = &e.kind else { panic!("{e:?}") };
    assert_eq!(args[0].kind, Kind::Column(1));
    assert_eq!(args[1].kind, Kind::Constant(Value::Int4(7)));
    assert_eq!(e.ty, Type::new(oid::INT4));
    assert_eq!(expr(&var(3, oid::TEXT), &scope(&[])).unwrap().kind, Kind::Column(0));
}

#[test]
fn a_comparison_of_two_widths_widens_the_narrower_side() {
    let lt = call("int48lt", vec![var(1, oid::INT4), var(2, oid::INT8)], oid::BOOL);
    let e = expr(&lt, &scope(&[])).unwrap();
    let Kind::Compare { op: CompareOp::Less, left, right } = &e.kind else { panic!("{e:?}") };
    assert_eq!(left.ty, Type::new(oid::INT8));
    assert!(matches!(&left.kind, Kind::Widen(inner) if inner.kind == Kind::Column(1)));
    assert_eq!(right.kind, Kind::Column(2));
    let eq = call("texteq", vec![var(3, oid::TEXT), var(3, oid::TEXT)], oid::BOOL);
    let e = expr(&eq, &scope(&[])).unwrap();
    assert!(matches!(e.kind, Kind::Compare { op: CompareOp::Equal, .. }), "{e:?}");
}

#[test]
fn unsigned_and_padded_comparisons_stay_functions() {
    for (src, ty) in [("oidlt", oid::OID), ("bpchareq", oid::BPCHAR)] {
        let e = call(src, vec![var(1, ty), var(1, ty)], oid::BOOL);
        let e = expr(&e, &scope(&[])).unwrap();
        assert!(matches!(e.kind, Kind::Function { .. }), "{src}: {e:?}");
    }
}

#[test]
fn what_the_generator_cannot_hold_is_refused_by_name() {
    let refused = |e: &Analyzed| expr(e, &scope(&[])).unwrap_err();
    assert_eq!(refused(&var(4, oid::NUMERIC)).what, "type numeric");
    assert_eq!(refused(&analyzed(ExprKind::Param(2), oid::INT4)).what, "parameter");
    let mut icu = call("texteq", vec![var(3, oid::TEXT), var(3, oid::TEXT)], oid::BOOL);
    if let ExprKind::Func(f) = &mut icu.kind {
        f.collation = 12_345;
    }
    assert_eq!(refused(&icu).what, "function texteq");
    let random = call("drandom", Vec::new(), oid::FLOAT8);
    assert_eq!(refused(&random).why, "a volatile function");
    let row = analyzed(ExprKind::Row(vec![var(1, oid::INT4)], CastForm::Implicit), oid::INT4);
    assert_eq!(refused(&row).what, "expression");
}

#[test]
fn boolean_tests_and_coalesce_become_cases_that_never_give_null() {
    let b = var(1, oid::INT4);
    let truth = call("int4lt", vec![b.clone(), b.clone()], oid::BOOL);
    let test =
        analyzed(ExprKind::BooleanTest(Box::new(truth.clone()), BoolTest::IsNotFalse), oid::BOOL);
    let e = expr(&test, &scope(&[])).unwrap();
    let Kind::Case { arms, otherwise: Some(otherwise) } = &e.kind else { panic!("{e:?}") };
    assert!(matches!(arms[0].0.kind, Kind::Not(_)));
    assert_eq!(arms[0].1.kind, Kind::Constant(Value::Bool(false)));
    assert_eq!(otherwise.kind, Kind::Constant(Value::Bool(true)));
    let unknown = analyzed(ExprKind::BooleanTest(Box::new(truth), BoolTest::IsUnknown), oid::BOOL);
    let e = expr(&unknown, &scope(&[])).unwrap();
    assert!(matches!(e.kind, Kind::IsNull { negated: false, .. }));
    let zero = analyzed(ExprKind::Const(Value::Int4(0)), oid::INT4);
    let coalesce = analyzed(ExprKind::Coalesce(vec![b, zero]), oid::INT4);
    let e = expr(&coalesce, &scope(&[])).unwrap();
    let Kind::Case { arms, otherwise: Some(otherwise) } = &e.kind else { panic!("{e:?}") };
    assert!(matches!(arms[0].0.kind, Kind::IsNull { negated: true, .. }));
    assert_eq!(arms[0].1.kind, Kind::Column(1));
    assert_eq!(otherwise.kind, Kind::Constant(Value::Int4(0)));
    let and = analyzed(ExprKind::Bool(BoolOp::And, Vec::new()), oid::BOOL);
    assert_eq!(expr(&and, &scope(&[])).unwrap().kind, Kind::And(Vec::new()));
    let case = Case {
        arg: None,
        whens: Vec::new(),
        default: Box::new(analyzed(ExprKind::Const(Value::Null), oid::INT4)),
    };
    let e = expr(&analyzed(ExprKind::Case(case), oid::INT4), &scope(&[])).unwrap();
    assert_eq!(e.kind, Kind::Case { arms: Vec::new(), otherwise: None });
}

#[test]
fn aggregates_keep_the_postgres_result_type() {
    let s = scope(&[]);
    let sum8 = aggref(agg_oid("sum", oid::INT8), vec![var(2, oid::INT8)]);
    let a = aggregate(&sum8, Type::new(oid::NUMERIC), &s).unwrap();
    assert_eq!((a.name, a.ty.oid, a.args[0].kind.clone()), ("sum", oid::NUMERIC, Kind::Column(2)));
    let mut star = aggref(agg_oid("count", oid::ANY), Vec::new());
    star.oid = builtin::procs_named("count")
        .find(|p| p.kind == b'a' && p.argtypes.is_empty())
        .unwrap()
        .oid;
    star.star = true;
    assert_eq!(aggregate(&star, Type::new(oid::INT8), &s).unwrap().name, "count_star");
    let mut distinct = aggref(agg_oid("max", oid::INT4), vec![var(1, oid::INT4)]);
    distinct.distinct = vec![rupg_analyze::SortGroup {
        target: 0,
        operator: 0,
        sort: 0,
        equal: 0,
        nulls_first: false,
        hashable: true,
        collation: 0,
    }];
    assert_eq!(
        aggregate(&distinct, Type::new(oid::INT4), &s).unwrap_err().why,
        "DISTINCT is only compiled for count"
    );
    let numeric = aggref(agg_oid("sum", oid::INT4), vec![var(1, oid::INT4)]);
    assert!(aggregate(&numeric, Type::new(oid::INT8), &s).is_ok());
    let stddev = aggref(agg_oid("stddev", oid::INT4), vec![var(1, oid::INT4)]);
    assert_eq!(
        aggregate(&stddev, Type::new(oid::NUMERIC), &s).unwrap_err().what,
        "aggregate stddev"
    );
}

#[test]
fn a_filter_is_one_conjunction_and_a_join_splits_into_keys_and_the_rest() {
    let int = Type::new(oid::INT4);
    let scan = Rel::Scan {
        relation: 16_384,
        table: "t".into(),
        attnums: vec![1],
        columns: vec![Column { name: "a".into(), ty: int }],
    };
    assert_eq!(Rel::filtered(scan.clone(), Vec::new()), scan);
    let cmp = |op, l: usize, r: usize| Expr {
        kind: Kind::Compare {
            op,
            left: Box::new(Expr::column(l, int)),
            right: Box::new(Expr::column(r, int)),
        },
        ty: Type::new(oid::BOOL),
    };
    let filtered =
        Rel::filtered(scan, vec![cmp(CompareOp::Less, 0, 0), cmp(CompareOp::Equal, 0, 0)]);
    let Rel::Filter { predicate, .. } = &filtered else { panic!("{filtered:?}") };
    let mut conjuncts = Vec::new();
    flatten(predicate.clone(), &mut conjuncts);
    assert_eq!(conjuncts.len(), 2);
    let right = [Column { name: "x".into(), ty: int }, Column { name: "y".into(), ty: int }];
    let (keys, residual) =
        split(vec![cmp(CompareOp::Equal, 2, 0), cmp(CompareOp::Less, 0, 1)], 2, &right).unwrap();
    assert_eq!(keys[0].0.columns(), [0]);
    assert_eq!(keys[0].1.columns(), [0]);
    assert_eq!(residual.len(), 1);
    assert_eq!(split(vec![cmp(CompareOp::Less, 0, 2)], 2, &right).unwrap_err().what, "Join");
}
