//! The estimates of `costsize.c` and `selfuncs.c`: the number of rows of a table, the widths of the values, the selectivity of the conditions and the costs of the nodes.

use std::cmp::Ordering;

use rupg_analyze::{BoolOp, BoolTest, Expr, ExprKind, Relation, SubLink, SubLinkKind};
use rupg_pgcatalog::builtin;
use rupg_plan::{STATISTIC_KIND_HISTOGRAM, STATISTIC_KIND_MCV, Statistic};
use rupg_types::{DATE_INFINITY, DATE_NEGATIVE_INFINITY, NetFamily, USECS_PER_DAY, Value, oid};

use crate::{Member, Size, exprs};

/// `seq_page_cost`.
pub(crate) const SEQ_PAGE: f64 = 1.0;
/// `cpu_tuple_cost`.
pub(crate) const CPU_TUPLE: f64 = 0.01;
/// `APPEND_CPU_COST_MULTIPLIER`: the part of `cpu_tuple_cost` that an `Append` costs for each row.
pub(crate) const APPEND_CPU: f64 = 0.5;
/// `cpu_operator_cost`.
pub(crate) const CPU_OPERATOR: f64 = 0.0025;
/// `cpu_index_tuple_cost`.
pub(crate) const CPU_INDEX_TUPLE: f64 = 0.005;
/// The number of pages that the planner takes for a table that `VACUUM` and `ANALYZE` have not seen, as `estimate_rel_size`.
const DEFAULT_PAGES: f64 = 10.0;
/// `BLCKSZ - SizeOfPageHeaderData`.
const PAGE_SPACE: i32 = 8168;
/// `MAXALIGN(SizeofHeapTupleHeader) + sizeof(ItemIdData)`.
const TUPLE_OVERHEAD: i32 = 28;
/// `MAXALIGN(SizeofHeapTupleHeader)`: the header of the row of a whole-row column, which `set_rel_width` adds to the width of its columns.
const WHOLE_ROW_HEADER: i32 = 24;
/// `DEFAULT_EQ_SEL`.
pub(crate) const EQ_SEL: f64 = 0.005;
/// `DEFAULT_INEQ_SEL`.
pub(crate) const INEQ_SEL: f64 = 1.0 / 3.0;
/// `DEFAULT_RANGE_INEQ_SEL`.
const RANGE_INEQ_SEL: f64 = 0.005;
/// `DEFAULT_NUM_DISTINCT`.
pub(crate) const NUM_DISTINCT: f64 = 200.0;
/// `disable_cost`: the cost that a plan gets when the planner should use it only when it has no other plan.
pub(crate) const DISABLE: f64 = 1.0e10;
/// `random_page_cost`.
pub(crate) const RANDOM_PAGE: f64 = 4.0;
/// `work_mem`, in bytes.
pub(crate) const WORK_MEM: f64 = 4.0 * 1024.0 * 1024.0;
/// `get_hash_memory_limit`: `work_mem` times `hash_mem_multiplier`, in bytes.
pub(crate) const HASH_MEM: f64 = 2.0 * WORK_MEM;
/// `BLCKSZ`.
const BLOCK: f64 = 8192.0;
/// `MAXALIGN(SizeofHeapTupleHeader)`.
const TUPLE_HEADER: i32 = 24;
/// `sizeof(HashJoinTuple)`.
const POINTER: f64 = 8.0;
/// `tuplesort_merge_order(work_mem)`: the number of runs that a pass of a merge sort reads.
const MERGE_ORDER: f64 = 15.0;

/// `get_typavgwidth`: the average width of a value of a type with a typmod.
pub fn type_width(ty: u32, typmod: i32) -> i32 {
    if let Some(row) = builtin::type_by_oid(ty)
        && row.len > 0
    {
        return i32::from(row.len);
    }
    let max = match ty {
        // `type_maximum_size`: four bytes for each character of UTF-8, and the header.
        oid::BPCHAR | oid::VARCHAR if typmod > 4 => (typmod - 4) * 4 + 4,
        oid::NUMERIC if typmod > 4 => {
            let precision = ((typmod - 4) >> 16) & 0xffff;
            8 + (precision + 6) / 4 * 2
        }
        _ => 0,
    };
    if max > 0 {
        if ty == oid::BPCHAR || max <= 32 {
            return max;
        }
        return 32 + (max.min(1000) - 32) / 2;
    }
    32
}

/// The width of a column of a relation: `stawidth` from `get_attavgwidth` for a column of a table with statistics, else the width of its type. A system column has the width of its type.
pub(crate) fn column_width(relation: &Relation, attnum: i16) -> i32 {
    match attnum {
        0 => WHOLE_ROW_HEADER + row_width(relation),
        -1 => 6,
        a if a < 0 => 4,
        a => {
            let analyzed = analyzed_width(relation, a);
            analyzed.unwrap_or_else(|| {
                relation
                    .columns
                    .get(usize::try_from(a - 1).unwrap_or(usize::MAX))
                    .map_or(32, |c| type_width(c.ty, c.typmod))
            })
        }
    }
}

/// `get_attavgwidth`: `stawidth` of a column of a table with statistics, when it is more than 0.
fn analyzed_width(relation: &Relation, attnum: i16) -> Option<i32> {
    if relation.oid == 0 {
        return None;
    }
    crate::statistics(relation.oid).get(&attnum).map(|s| s.width).filter(|w| *w > 0)
}

/// `get_rel_data_width`: the width of the columns of a relation that are not dropped, from their statistics or their types.
pub(crate) fn row_width(relation: &Relation) -> i32 {
    let width = |(at, c): (usize, &rupg_analyze::Column)| {
        let attnum = i16::try_from(at + 1).unwrap_or(i16::MAX);
        analyzed_width(relation, attnum).unwrap_or_else(|| type_width(c.ty, c.typmod))
    };
    relation.columns.iter().enumerate().filter(|(_, c)| c.ty != 0).map(width).sum()
}

/// The width of a value of an expression: the width of the column for a column, or else the width of its type.
pub(crate) fn expr_width(expr: &Expr, relations: &[Relation]) -> i32 {
    match expr.kind {
        ExprKind::Var(var) if var.levels_up == 0 => {
            relations.get(var.relation).map_or(32, |r| column_width(r, var.attnum))
        }
        _ => type_width(expr.ty, expr.typmod),
    }
}

/// `estimate_rel_size` for a table: the pages and the rows.
pub(crate) fn table_size(relation: &Relation, size: Size) -> (f64, f64) {
    estimate_size(row_width(relation), size, false)
}

/// `estimate_rel_size` for a member of an inheritance tree. `relhassubclass` keeps the planner from taking ten pages for a table with children that has no pages.
pub(crate) fn member_size(member: &Member) -> (f64, f64) {
    let width: i32 = member.columns.iter().map(|&(ty, typmod)| type_width(ty, typmod)).sum();
    estimate_size(width, member.size, member.has_children)
}

/// `table_block_relation_estimate_size`: the pages of the heap, or ten pages for a table that has fewer and was never vacuumed or analyzed, and the rows from the density of `relpages` and `reltuples`, or from the width of a row when there is none.
fn estimate_size(width: i32, size: Size, has_children: bool) -> (f64, f64) {
    let mut pages = size.blocks;
    if pages < DEFAULT_PAGES && size.reltuples < 0.0 && !has_children {
        pages = DEFAULT_PAGES;
    }
    if pages == 0.0 {
        return (0.0, 0.0);
    }
    let density = if size.reltuples >= 0.0 && size.relpages > 0.0 {
        size.reltuples / size.relpages
    } else {
        f64::from((PAGE_SPACE / (width + TUPLE_OVERHEAD)).max(1))
    };
    (pages, (density * pages).round_ties_even())
}

/// `clamp_row_est`: a number of rows that is a whole number and at least 1. As `rint`, a half goes to the even number.
pub(crate) fn clamp_rows(rows: f64) -> f64 {
    if rows <= 1.0 { 1.0 } else { rows.round_ties_even() }
}

/// What the estimates of the conditions know of the values that they read, as `examine_variable` finds it: the selectivity of `=` and of `<>`, and the statistics of a column in `pg_statistic`.
pub(crate) trait Estimate {
    /// `eqsel` for `left = right`, or `None` for the default estimate.
    fn eq(&self, left: &Expr, right: &Expr) -> Option<f64>;

    /// `neqsel` for `left <> right`, or `None` for the default estimate. It is the rest of the rows of `=`.
    fn neq(&self, left: &Expr, right: &Expr) -> Option<f64> {
        self.eq(left, right).map(|sel| 1.0 - sel)
    }

    /// `scalarineqsel_wrapper` for the inequality `op` of `left` and `right`, or `None` for the default estimate.
    fn ineq(&self, _left: &Expr, _right: &Expr, _op: Inequality) -> Option<f64> {
        None
    }

    /// The statistics of an expression, as `examine_variable` finds them.
    fn statistic(&self, _expr: &Expr) -> Option<&Statistic> {
        None
    }

    /// `ineq_histogram_selectivity` for the inequality `op` of the column `column` and the constant `value`: the part of the histogram that passes, or `None` with no histogram.
    fn histogram_ineq(&self, _column: &Expr, _value: &Expr, _op: Inequality) -> Option<f64> {
        None
    }
}

impl<F: Fn(&Expr, &Expr) -> Option<f64>> Estimate for F {
    fn eq(&self, left: &Expr, right: &Expr) -> Option<f64> {
        self(left, right)
    }
}

/// The estimate of `=` with no statistics, which is the default.
fn no_statistics(_: &Expr, _: &Expr) -> Option<f64> {
    None
}

/// `get_variable_numdistinct`: the number of distinct values of an expression with the statistics `statistic`, with true for the default. A unique expression has a distinct value for each row that is not null. With no statistics a `boolean` has 2 values, `ctid` is unique and `tableoid` has 1 value. Else a relative number takes the `tuples` of the relation of the expression, and a small relation has as many values as tuples.
pub(crate) fn numdistinct(
    expr: &Expr,
    statistic: Option<&Statistic>,
    unique: bool,
    tuples: Option<f64>,
) -> (f64, bool) {
    let (mut stadistinct, nullfrac) = match statistic {
        Some(s) => (f64::from(s.distinct), f64::from(s.null_frac)),
        None if expr.ty == oid::BOOL => (2.0, 0.0),
        None => match expr.strip_implicit().kind {
            ExprKind::Var(var) if var.attnum == -1 => (-1.0, 0.0),
            ExprKind::Var(var) if var.attnum == -6 => (1.0, 0.0),
            _ => (0.0, 0.0),
        },
    };
    if unique {
        stadistinct = -(1.0 - nullfrac);
    }
    if stadistinct > 0.0 {
        return (clamp_rows(stadistinct), false);
    }
    let Some(ntuples) = tuples.filter(|t| *t > 0.0) else { return (NUM_DISTINCT, true) };
    if stadistinct < 0.0 {
        return (clamp_rows(-stadistinct * ntuples), false);
    }
    if ntuples < NUM_DISTINCT {
        return (clamp_rows(ntuples), false);
    }
    (NUM_DISTINCT, true)
}

/// `clause_selectivity` with no statistics: the fraction of the rows that pass a condition.
pub(crate) fn selectivity(expr: &Expr) -> f64 {
    selectivity_by(expr, &no_statistics)
}

/// `clause_selectivity`, with the estimates of `est` for `=`, `<>` and the columns with statistics.
pub(crate) fn selectivity_by(expr: &Expr, est: &dyn Estimate) -> f64 {
    let each = |e: &Expr| selectivity_by(e, est);
    match &expr.kind {
        ExprKind::Bool(BoolOp::And, args) => list_selectivity_by(args, est),
        ExprKind::Bool(BoolOp::Or, args) => {
            args.iter().map(each).fold(0.0, |s1, s2| s1 + s2 - s1 * s2)
        }
        ExprKind::Bool(BoolOp::Not, args) => 1.0 - args.first().map_or(0.5, each),
        ExprKind::NullTest(arg, is_null) => {
            let sel = null_selectivity(arg, est);
            if *is_null { sel } else { 1.0 - sel }
        }
        ExprKind::BooleanTest(arg, test) if est.statistic(arg).is_some() => {
            est.statistic(arg).map_or(0.5, |s| bool_test(s, *test))
        }
        // `booltestsel` with no statistics: `IS UNKNOWN` has `DEFAULT_UNK_SEL`, and the other tests take the selectivity of the argument, or the rest of the rows, as if the argument is not null.
        ExprKind::BooleanTest(_, BoolTest::IsUnknown) => EQ_SEL,
        ExprKind::BooleanTest(_, BoolTest::IsNotUnknown) => 1.0 - EQ_SEL,
        ExprKind::BooleanTest(arg, BoolTest::IsTrue | BoolTest::IsNotFalse) => each(arg),
        ExprKind::BooleanTest(arg, BoolTest::IsFalse | BoolTest::IsNotTrue) => 1.0 - each(arg),
        ExprKind::Const(_) if exprs::is_true(expr) => 1.0,
        ExprKind::Const(_) => 0.0,
        // A row comparison with a subquery that runs once is the comparisons of its columns with the columns of the subquery, as `convert_testexpr` makes them.
        ExprKind::SubLink(link)
            if link.kind == SubLinkKind::RowCompare && !exprs::correlated(&link.query, 1) =>
        {
            link.test.as_ref().map_or(0.5, each)
        }
        // `boolvarsel`: a column with statistics is `= true`.
        ExprKind::Var(_) if est.statistic(expr).is_some() => {
            let value = Expr::new(ExprKind::Const(Value::Bool(true)), oid::BOOL);
            est.eq(expr, &value).unwrap_or(0.5)
        }
        // `boolvarsel` with no statistics: a column, a parameter or a subplan.
        ExprKind::Var(_)
        | ExprKind::Param(_)
        | ExprKind::SubLink(_)
        | ExprKind::FieldSelect(..)
        | ExprKind::Case(_)
        | ExprKind::Coalesce(_) => 0.5,
        ExprKind::Relabel(arg, _) | ExprKind::CoerceToDomain(arg, _) => each(arg),
        // `clause_selectivity_ext` estimates `IS DISTINCT FROM` as the rest of the rows of its `=` operator, and `IS NOT DISTINCT FROM` as the operator.
        ExprKind::Distinct { not, args, .. } => {
            let sel = match args.as_slice() {
                [left, right] => est.eq(left, right).unwrap_or(EQ_SEL),
                _ => EQ_SEL,
            };
            if *not { sel } else { 1.0 - sel }
        }
        ExprKind::ScalarArrayOp { func, any, args } => array_selectivity(*func, *any, args, est),
        _ => match exprs::binary(expr) {
            Some((op, left, right)) => {
                let known = match builtin::operator_by_oid(op).map(|o| o.name) {
                    Some("=") => est.eq(left, right),
                    Some("<>") => est.neq(left, right),
                    _ => scalar_inequality(op).and_then(|op| est.ineq(left, right, op)),
                };
                known.unwrap_or_else(|| {
                    pattern_selectivity(op, left, right, est)
                        .unwrap_or_else(|| operator_selectivity(op))
                })
            }
            None => INEQ_SEL,
        },
    }
}

/// `nulltestsel` for `IS NULL`: the null fraction of the statistics. A system column is never null.
fn null_selectivity(arg: &Expr, est: &dyn Estimate) -> f64 {
    match (est.statistic(arg), &arg.kind) {
        (Some(s), _) => f64::from(s.null_frac),
        (None, ExprKind::Var(var)) if var.attnum < 0 => 0.0,
        (None, _) => EQ_SEL,
    }
}

/// An operator that `scalarltsel`, `scalarlesel`, `scalargtsel` or `scalargesel` estimates.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Inequality {
    /// True for `>` and `>=`.
    pub(crate) isgt: bool,
    /// True for `<=` and `>=`.
    pub(crate) iseq: bool,
    /// True when the operator has the order of the default operator class of its type, which the histogram has, as `comparison_ops_are_compatible` finds it. The pattern operators such as `~<~` have the order of the bytes.
    pub(crate) sorted: bool,
}

impl Inequality {
    /// The result of the operator for the order of its two sides.
    pub(crate) fn test(self, order: Ordering) -> bool {
        match (self.isgt, self.iseq) {
            (false, false) => order.is_lt(),
            (false, true) => order.is_le(),
            (true, false) => order.is_gt(),
            (true, true) => order.is_ge(),
        }
    }
}

/// The operators `<`, `<=`, `>` and `>=` and the pattern operators `~<~`, `~<=~`, `~>~` and `~>=~`, of the types that `scalarltsel` and the other estimators of inequalities serve. The geometric types, the ranges and the multiranges have other estimators for these operators.
pub(crate) fn scalar_inequality(op: u32) -> Option<Inequality> {
    const OTHER: [u32; 6] =
        [oid::LSEG, oid::PATH, oid::BOX, oid::CIRCLE, oid::ANYRANGE, oid::ANYMULTIRANGE];
    let row = builtin::operator_by_oid(op).filter(|row| !OTHER.contains(&row.left))?;
    let (isgt, iseq, sorted) = match row.name {
        "<" => (false, false, true),
        "<=" => (false, true, true),
        ">" => (true, false, true),
        ">=" => (true, true, true),
        "~<~" => (false, false, false),
        "~<=~" => (false, true, false),
        "~>~" => (true, false, false),
        "~>=~" => (true, true, false),
        _ => return None,
    };
    Some(Inequality { isgt, iseq, sorted })
}

/// `convert_to_scalar`: the value `value` of the type `ty` and the bounds `low` and `high` of a bin of the histogram of the type `bounds` on a scale of `double`, on which the planner takes the place of the value in the bin. A number, a time and a network address have their own scale. Strings and `bytea` have a scale of the bytes after the prefix that the three values share. `None` for the types that have no scale, or for bounds of a type of another group than the value.
pub(crate) fn convert_to_scalar(
    value: &Value,
    ty: u32,
    low: &Value,
    high: &Value,
    bounds: u32,
) -> Option<(f64, f64, f64)> {
    let each = |f: fn(&Value, u32) -> Option<f64>| {
        Some((f(value, ty)?, f(low, bounds)?, f(high, bounds)?))
    };
    match ty {
        oid::BOOL
        | oid::INT2
        | oid::INT4
        | oid::INT8
        | oid::FLOAT4
        | oid::FLOAT8
        | oid::NUMERIC
        | oid::OID
        | oid::REGPROC
        | oid::REGPROCEDURE
        | oid::REGOPER
        | oid::REGOPERATOR
        | oid::REGCLASS
        | oid::REGTYPE
        | oid::REGCOLLATION
        | oid::REGCONFIG
        | oid::REGDICTIONARY
        | oid::REGROLE
        | oid::REGNAMESPACE
        | oid::REGDATABASE => each(numeric_scalar),
        oid::CHAR | oid::BPCHAR | oid::VARCHAR | oid::TEXT | oid::NAME => {
            let (v, l, h) =
                (string_datum(value, ty)?, string_datum(low, bounds)?, string_datum(high, bounds)?);
            Some(string_scalar(&v, &l, &h))
        }
        oid::BYTEA => match (value, low, high) {
            (Value::Bytea(v), Value::Bytea(l), Value::Bytea(h)) if bounds == oid::BYTEA => {
                Some(bytea_scalar(v, l, h))
            }
            _ => None,
        },
        oid::TIMESTAMP | oid::TIMESTAMPTZ | oid::DATE | oid::INTERVAL | oid::TIME | oid::TIMETZ => {
            each(timevalue_scalar)
        }
        oid::INET | oid::CIDR => each(network_scalar),
        _ => None,
    }
}

/// `convert_numeric_to_scalar`.
fn numeric_scalar(value: &Value, ty: u32) -> Option<f64> {
    if !matches!(
        ty,
        oid::BOOL
            | oid::INT2
            | oid::INT4
            | oid::INT8
            | oid::FLOAT4
            | oid::FLOAT8
            | oid::NUMERIC
            | oid::OID
            | oid::REGPROC
            | oid::REGPROCEDURE
            | oid::REGOPER
            | oid::REGOPERATOR
            | oid::REGCLASS
            | oid::REGTYPE
            | oid::REGCOLLATION
            | oid::REGCONFIG
            | oid::REGDICTIONARY
            | oid::REGROLE
            | oid::REGNAMESPACE
            | oid::REGDATABASE
    ) {
        return None;
    }
    match value {
        Value::Bool(b) => Some(f64::from(u8::from(*b))),
        Value::Int2(n) => Some(f64::from(*n)),
        Value::Int4(n) => Some(f64::from(*n)),
        Value::Int8(n) => Some(*n as f64),
        Value::Float4(n) => Some(f64::from(*n)),
        Value::Float8(n) => Some(*n),
        Value::Numeric(n) => Some(n.to_f64()),
        Value::Oid(n) => Some(f64::from(*n)),
        _ => None,
    }
}

/// `convert_string_datum` for the collation `C`: the bytes of the string.
fn string_datum(value: &Value, ty: u32) -> Option<Vec<u8>> {
    match (value, ty) {
        (Value::Char(c), oid::CHAR) => Some(vec![*c]),
        (Value::Text(s), oid::BPCHAR | oid::VARCHAR | oid::TEXT | oid::NAME) => {
            Some(s.as_bytes().to_vec())
        }
        _ => None,
    }
}

/// `convert_string_to_scalar`: the range of the bytes of the bounds, widened to the letters or the digits that it has a part of, or to the printable ASCII bytes when it is less than 10 bytes, and the three strings after their common prefix on that scale.
fn string_scalar(value: &[u8], low: &[u8], high: &[u8]) -> (f64, f64, f64) {
    let first = i32::from(high.first().copied().unwrap_or(0));
    let (mut rangelo, mut rangehi) = (first, first);
    for &c in low.iter().chain(high) {
        rangelo = rangelo.min(i32::from(c));
        rangehi = rangehi.max(i32::from(c));
    }
    for (a, z) in [(b'A', b'Z'), (b'a', b'z'), (b'0', b'9')] {
        let (a, z) = (i32::from(a), i32::from(z));
        if rangelo <= z && rangehi >= a {
            rangelo = rangelo.min(a);
            rangehi = rangehi.max(z);
        }
    }
    if rangehi - rangelo < 9 {
        rangelo = i32::from(b' ');
        rangehi = 127;
    }
    let common = low.iter().zip(high).zip(value).take_while(|((l, h), v)| l == h && l == v).count();
    let scale = |s: &[u8]| one_string_scalar(&s[common..], rangelo, rangehi, 12);
    (scale(value), scale(low), scale(high))
}

/// `convert_one_string_to_scalar` and `convert_one_bytea_to_scalar`: the first `limit` bytes of a string as the digits of a fraction in the base of the range, with the bytes out of the range just out of it.
fn one_string_scalar(value: &[u8], rangelo: i32, rangehi: i32, limit: usize) -> f64 {
    let base = f64::from(rangehi - rangelo + 1);
    let mut num = 0.0;
    let mut denom = base;
    for &c in value.iter().take(limit) {
        let ch = i32::from(c).clamp(rangelo - 1, rangehi + 1);
        num += f64::from(ch - rangelo) / denom;
        denom *= base;
    }
    num
}

/// `convert_bytea_to_scalar`: the three values after their common prefix on the scale of all the bytes.
fn bytea_scalar(value: &[u8], low: &[u8], high: &[u8]) -> (f64, f64, f64) {
    let common = low.iter().zip(high).zip(value).take_while(|((l, h), v)| l == h && l == v).count();
    let scale = |s: &[u8]| one_string_scalar(&s[common..], 0, 255, 10);
    (scale(value), scale(low), scale(high))
}

/// `convert_timevalue_to_scalar`: the microseconds of a time, and of a date from 2000-01-01, with an infinite date at the end of the scale. A month of an interval is a twelfth of 365.25 days, and a time with a time zone is its time in UTC.
fn timevalue_scalar(value: &Value, ty: u32) -> Option<f64> {
    match (value, ty) {
        (Value::Timestamp(t), oid::TIMESTAMP) | (Value::TimestampTz(t), oid::TIMESTAMPTZ) => {
            Some(*t as f64)
        }
        (Value::Date(d), oid::DATE) => Some(match *d {
            DATE_NEGATIVE_INFINITY => -f64::MAX,
            DATE_INFINITY => f64::MAX,
            d => f64::from(d) * USECS_PER_DAY as f64,
        }),
        (Value::Interval(i), oid::INTERVAL) => Some(
            i.time as f64
                + f64::from(i.day) * USECS_PER_DAY as f64
                + f64::from(i.month) * ((365.25 / 12.0) * USECS_PER_DAY as f64),
        ),
        (Value::Time(t), oid::TIME) => Some(*t as f64),
        (Value::TimeTz(t, zone), oid::TIMETZ) => Some(*t as f64 + f64::from(*zone) * 1000000.0),
        _ => None,
    }
}

/// `convert_network_to_scalar` for `inet` and `cidr`: the family and the first 4 bytes of an IPv4 address or the first 5 bytes of an IPv6 address as the digits of a number of base 256.
fn network_scalar(value: &Value, ty: u32) -> Option<f64> {
    let (Value::Inet(ip), oid::INET | oid::CIDR) = (value, ty) else { return None };
    let (family, len) = match ip.family {
        NetFamily::V4 => (2.0, 4),
        NetFamily::V6 => (3.0, 5),
    };
    Some(ip.addr[..len].iter().fold(family, |res, &b| res * 256.0 + f64::from(b)))
}

/// `scalararraysel` with no statistics: the selectivity of the operator for each item of the array, with the chance that one item passes for `ANY` or that each item passes for `ALL`. For `= ANY` and for `<> ALL` the chances of the items are disjoint, so that they add, when the sum is a probability. An array that is not a constant or a list of `ARRAY[...]` has 10 items that the condition knows nothing about.
fn array_selectivity(func: u32, any: bool, args: &[Expr], est: &dyn Estimate) -> f64 {
    let [left, array] = args else { return 0.5 };
    let element = builtin::type_by_oid(array.ty).map_or(0, |t| t.elem);
    let Some(op) = array_operator(func, left.ty, element) else { return 0.5 };
    let (equality, inequality) = (op.name == "=", op.name == "<>");
    let one_relation = exprs::relations(left).len() == 1;
    // The estimator of the operator for the value of one item, which is 0 for a null value of a strict operator when the condition reads one relation, as `get_restriction_variable` finds it.
    let item = |value: &Expr| {
        if one_relation && matches!(value.kind, ExprKind::Const(Value::Null)) {
            return 0.0;
        }
        let known = match op.name {
            "=" => est.eq(left, value),
            "<>" => est.neq(left, value),
            _ => None,
        };
        known.unwrap_or_else(|| {
            pattern_selectivity(op.oid, left, value, est)
                .unwrap_or_else(|| operator_selectivity(op.oid))
        })
    };
    let array = strip_array_coercion(array);
    let items: Vec<Expr> = match &array.kind {
        ExprKind::Const(Value::Null) => return 0.0,
        ExprKind::Const(Value::Array(a)) => {
            if !any && a.values.iter().any(Option::is_none) {
                return 0.0;
            }
            let value = |v: &Option<Value>| v.clone().unwrap_or(Value::Null);
            a.values.iter().map(|v| Expr::new(ExprKind::Const(value(v)), element)).collect()
        }
        ExprKind::Array { multidims: false, elements, .. } => {
            if !any && elements.iter().any(|e| matches!(e.kind, ExprKind::Const(Value::Null))) {
                return 0.0;
            }
            elements.clone()
        }
        _ => {
            let s2 = item(&Expr::new(ExprKind::CaseTest, element));
            let start = if any { 0.0 } else { 1.0 };
            let s1 = (0..10).fold(start, |s1, _| if any { s1 + s2 - s1 * s2 } else { s1 * s2 });
            return s1.clamp(0.0, 1.0);
        }
    };
    let (mut s1, mut disjoint) = if any { (0.0, 0.0) } else { (1.0, 1.0) };
    for value in &items {
        let s2 = item(value);
        if any {
            s1 = s1 + s2 - s1 * s2;
            if equality {
                disjoint += s2;
            }
        } else {
            s1 *= s2;
            if inequality {
                disjoint += s2 - 1.0;
            }
        }
    }
    if (if any { equality } else { inequality }) && (0.0..=1.0).contains(&disjoint) {
        s1 = disjoint;
    }
    s1.clamp(0.0, 1.0)
}

/// `booltestsel` with the statistics of the argument: the fraction of `true` from the first most common value, or half of the values that are not null when there is no list.
fn bool_test(s: &Statistic, test: BoolTest) -> f64 {
    let freq_null = f64::from(s.null_frac);
    let first =
        s.slots.iter().find(|slot| slot.kind == STATISTIC_KIND_MCV).and_then(|slot| {
            Some((slot.values.as_ref()?.first()?, *slot.numbers.as_ref()?.first()?))
        });
    let sel = match first {
        Some((value, freq)) => {
            let freq_true = if *value == Value::Bool(true) {
                f64::from(freq)
            } else {
                1.0 - f64::from(freq) - freq_null
            };
            let freq_false = 1.0 - freq_true - freq_null;
            match test {
                BoolTest::IsUnknown => freq_null,
                BoolTest::IsNotUnknown => 1.0 - freq_null,
                BoolTest::IsTrue => freq_true,
                BoolTest::IsNotTrue => 1.0 - freq_true,
                BoolTest::IsFalse => freq_false,
                BoolTest::IsNotFalse => 1.0 - freq_false,
            }
        }
        None => match test {
            BoolTest::IsUnknown => freq_null,
            BoolTest::IsNotUnknown => 1.0 - freq_null,
            BoolTest::IsTrue | BoolTest::IsFalse => (1.0 - freq_null) / 2.0,
            BoolTest::IsNotTrue | BoolTest::IsNotFalse => (freq_null + 1.0) / 2.0,
        },
    };
    sel.clamp(0.0, 1.0)
}

/// The operator of `x op ANY (array)` or `x op ALL (array)` whose function is `func`, for the type of `x` and the type of the items of the array.
pub(crate) fn array_operator(
    func: u32,
    left: u32,
    element: u32,
) -> Option<&'static builtin::OperatorRow> {
    let mut ops = builtin::operators().iter().filter(|o| o.code == func);
    ops.clone().find(|o| o.left == left && o.right == element).or_else(|| ops.next())
}

/// `clauselist_selectivity` with no statistics: the fraction of the rows that pass all the conditions.
pub(crate) fn list_selectivity<'a>(quals: impl IntoIterator<Item = &'a Expr>) -> f64 {
    list_selectivity_by(quals, &no_statistics)
}

/// `clauselist_selectivity`, with the estimates of `est` as [`selectivity_by`] reads them. The product of the selectivities of the conditions, but a low bound and a high bound of the same value, such as `x > 1 AND x < 5`, are a range, as `addRangeClause` finds them. Of two low bounds or two high bounds of a value, the smaller selectivity counts. A bound with the default selectivity makes the range `DEFAULT_RANGE_INEQ_SEL`.
pub(crate) fn list_selectivity_by<'a>(
    quals: impl IntoIterator<Item = &'a Expr>,
    est: &dyn Estimate,
) -> f64 {
    let mut s1 = 1.0;
    // The value of each range, with the selectivity of its low bound and of its high bound.
    let mut ranges: Vec<(&Expr, Option<f64>, Option<f64>)> = Vec::new();
    for expr in quals {
        let s2 = selectivity_by(expr, est);
        let Some((value, low)) = range_bound(expr) else {
            s1 *= s2;
            continue;
        };
        let at = match ranges.iter().position(|(v, _, _)| v.same(value)) {
            Some(at) => at,
            None => {
                ranges.push((value, None, None));
                ranges.len() - 1
            }
        };
        let (_, lo, hi) = &mut ranges[at];
        let bound = if low { lo } else { hi };
        *bound = Some(bound.map_or(s2, |b| b.min(s2)));
    }
    // `clauselist_selectivity` puts each new range first in its list. A range of two bounds with estimates is the rows of the two bounds less the rows of all, with the null rows that the two bounds both leave out.
    for (value, lo, hi) in ranges.into_iter().rev() {
        s1 *= match (lo, hi) {
            (Some(lo), Some(hi)) if lo == INEQ_SEL || hi == INEQ_SEL => RANGE_INEQ_SEL,
            (Some(lo), Some(hi)) => {
                let s2 = hi + lo - 1.0 + null_selectivity(value, est);
                match s2 {
                    s2 if s2 < -0.01 => RANGE_INEQ_SEL,
                    s2 if s2 <= 0.0 => 1.0e-10,
                    s2 => s2,
                }
            }
            (Some(s2), None) | (None, Some(s2)) => s2,
            (None, None) => 1.0,
        };
    }
    s1
}

/// The value of a condition that `clauselist_selectivity` puts in a range, with true when the condition is a low bound of the value: an operator `<`, `<=`, `>` or `>=`, or one of the pattern operators such as `~<~`, with the estimator `scalarltsel`, `scalarlesel`, `scalargtsel` or `scalargesel`, a value of one relation on one side and a value that reads no relation and calls no volatile function on the other side.
pub(crate) fn range_bound(expr: &Expr) -> Option<(&Expr, bool)> {
    // The geometric types, the ranges and the multiranges have other estimators for these operators.
    const OTHER: [u32; 6] =
        [oid::LSEG, oid::PATH, oid::BOX, oid::CIRCLE, oid::ANYRANGE, oid::ANYMULTIRANGE];
    let (op, left, right) = exprs::binary(expr)?;
    let row = builtin::operator_by_oid(op).filter(|row| !OTHER.contains(&row.left))?;
    let less = match row.name {
        "<" | "<=" | "~<~" | "~<=~" => true,
        ">" | ">=" | "~>~" | "~>=~" => false,
        _ => return None,
    };
    if exprs::relations(expr).len() != 1 {
        return None;
    }
    let constant = |e: &Expr| exprs::relations(e).is_empty() && !exprs::volatile(e);
    if constant(right) {
        Some((left, !less))
    } else if constant(left) {
        Some((right, less))
    } else {
        None
    }
}

/// The selectivity of the estimator of an operator, `oprrest`, with no statistics.
fn operator_selectivity(op: u32) -> f64 {
    let Some(row) = builtin::operator_by_oid(op) else { return INEQ_SEL };
    match row.name {
        "=" => EQ_SEL,
        "<>" => 1.0 - EQ_SEL,
        "~~" | "~~*" | "~" | "~*" => EQ_SEL,
        "!~~" | "!~~*" | "!~" | "!~*" => 1.0 - EQ_SEL,
        "^@" => EQ_SEL,
        _ => INEQ_SEL,
    }
}

/// The kinds of pattern of `pattern_fixed_prefix`.
#[derive(Clone, Copy, PartialEq)]
enum Pattern {
    Like,
    LikeIc,
    Regex,
    RegexIc,
    Prefix,
}

/// `patternsel_common`: the selectivity of `LIKE`, `ILIKE`, a regular expression or `^@` of a column and a constant pattern. A pattern that has all of the value is `=` of the value, as `var_eq_const`. Otherwise the part of the histogram whose values match the pattern counts, when the histogram has at least 100 values. With a smaller histogram or none the estimate is the selectivity of the fixed prefix of the pattern times the selectivity of the rest of the pattern, and a histogram of 10 to 100 values counts in proportion to its size. The result is between 0.0001 and 0.9999 for the values that are not the most common values, and the most common values that match the pattern add their frequencies. The result is `None` when the estimator uses its default.
fn pattern_selectivity(op: u32, left: &Expr, right: &Expr, est: &dyn Estimate) -> Option<f64> {
    let row = builtin::operator_by_oid(op)?;
    let (kind, negate) = match row.name {
        "~~" => (Pattern::Like, false),
        "!~~" => (Pattern::Like, true),
        "~~*" => (Pattern::LikeIc, false),
        "!~~*" => (Pattern::LikeIc, true),
        "~" => (Pattern::Regex, false),
        "!~" => (Pattern::Regex, true),
        "~*" => (Pattern::RegexIc, false),
        "!~*" => (Pattern::RegexIc, true),
        "^@" => (Pattern::Prefix, false),
        _ => return None,
    };
    if !matches!(row.right, oid::TEXT | oid::BYTEA) || exprs::relations(left).len() != 1 {
        return None;
    }
    let mut pattern = right;
    while let ExprKind::Relabel(arg, _) = &pattern.kind {
        pattern = arg;
    }
    let text = match &pattern.kind {
        ExprKind::Const(Value::Null) => return Some(0.0),
        ExprKind::Const(Value::Text(t)) => t.as_bytes(),
        ExprKind::Const(Value::Bytea(b)) => b.as_slice(),
        _ => return None,
    };
    let ExprKind::Const(constant) = &pattern.kind else { return None };
    // `rdatatype`: the prefix has the type of the column for `bpchar` and `bytea`, and `text` for `text` and `name`.
    let prefix_type = match left.ty {
        oid::TEXT | oid::VARCHAR | oid::NAME => oid::TEXT,
        oid::BPCHAR => oid::BPCHAR,
        oid::BYTEA => oid::BYTEA,
        _ => return None,
    };
    let statistic = est.statistic(left);
    let nullfrac = statistic.map_or(0.0, |s| f64::from(s.null_frac));
    let (prefix, status, rest) = fixed_prefix(text, kind);
    let prefix = Expr::new(
        ExprKind::Const(match &pattern.kind {
            ExprKind::Const(Value::Bytea(_)) => Value::Bytea(prefix),
            _ => Value::Text(String::from_utf8_lossy(&prefix).into_owned()),
        }),
        prefix_type,
    );
    let sel = if status == Prefix::Exact {
        est.eq(left, &prefix).unwrap_or(EQ_SEL)
    } else {
        // A negated operator estimates the operator that it negates.
        let positive = if negate { row.negator } else { op };
        let test = |v: &Value| crate::pattern_match(positive, v, constant).unwrap_or(false);
        let (hist, hist_size) = histogram_selectivity(statistic, &test, 10, 1);
        let mut selec = match hist {
            Some(hist) if hist_size >= 100 => hist,
            _ => {
                let prefix_sel = if status == Prefix::Partial {
                    prefix_selectivity(est, left, &prefix)
                } else {
                    1.0
                };
                let heuristic = prefix_sel * rest;
                match hist {
                    // A histogram of 10 to 100 values counts more when it is larger.
                    Some(hist) => {
                        let weight = hist_size as f64 / 100.0;
                        hist * weight + heuristic * (1.0 - weight)
                    }
                    None => heuristic,
                }
            }
        };
        selec = selec.clamp(0.0001, 0.9999);
        let (mcv_selec, sumcommon) = mcv_selectivity(statistic, &test);
        selec *= 1.0 - nullfrac - sumcommon;
        selec + mcv_selec
    };
    Some(if negate { 1.0 - sel - nullfrac } else { sel }.clamp(0.0, 1.0))
}

/// `mcv_selectivity`: the sum of the frequencies of the most common values that pass `test`, and the sum of the frequencies of all of them.
fn mcv_selectivity(statistic: Option<&Statistic>, test: &dyn Fn(&Value) -> bool) -> (f64, f64) {
    let Some(mcv) = statistic.and_then(|s| s.slots.iter().find(|s| s.kind == STATISTIC_KIND_MCV))
    else {
        return (0.0, 0.0);
    };
    let values = mcv.values.as_deref().unwrap_or_default();
    let numbers = mcv.numbers.as_deref().unwrap_or_default();
    let (mut mcv_selec, mut sumcommon) = (0.0, 0.0);
    for (v, &n) in values.iter().zip(numbers) {
        if test(v) {
            mcv_selec += f64::from(n);
        }
        sumcommon += f64::from(n);
    }
    (mcv_selec, sumcommon)
}

/// `histogram_selectivity`: the part of the values of the histogram that pass `test`, without the first and the last `skip` values, and the number of the values of the histogram. The part is `None` when the histogram has fewer than `min_size` values.
fn histogram_selectivity(
    statistic: Option<&Statistic>,
    test: &dyn Fn(&Value) -> bool,
    min_size: usize,
    skip: usize,
) -> (Option<f64>, usize) {
    let Some(values) = statistic
        .and_then(|s| s.slots.iter().find(|s| s.kind == STATISTIC_KIND_HISTOGRAM))
        .map(|slot| slot.values.as_deref().unwrap_or_default())
    else {
        return (None, 0);
    };
    let size = values.len();
    if size < min_size {
        return (None, size);
    }
    let inner = &values[skip..size - skip];
    let matches = inner.iter().filter(|v| test(v)).count();
    (Some(matches as f64 / inner.len() as f64), size)
}

/// `prefix_selectivity`: the selectivity of the values that start with the constant `prefix`, from the histogram of `column`, as `column >= prefix AND column < greater`, where `greater` is the least string after the prefix, as `make_greater_string` makes it. It is at least the selectivity of `=` of the prefix. With no histogram it is `DEFAULT_MATCH_SEL`.
fn prefix_selectivity(est: &dyn Estimate, column: &Expr, prefix: &Expr) -> f64 {
    let ge = Inequality { isgt: true, iseq: true, sorted: true };
    let Some(mut sel) = est.histogram_ineq(column, prefix, ge) else { return EQ_SEL };
    if let Some(greater) = greater_string(prefix) {
        let lt = Inequality { isgt: false, iseq: false, sorted: true };
        let top = est.histogram_ineq(column, &greater, lt).unwrap_or(0.0);
        sel = top + sel - 1.0;
    }
    sel.max(est.eq(column, prefix).unwrap_or(EQ_SEL))
}

/// `make_greater_string` in the C collation: the prefix with its last character incremented, as `pg_utf8_increment` increments it, or `byte_increment` for `bytea`. A last character that cannot be incremented is removed, and the character before it is incremented.
fn greater_string(prefix: &Expr) -> Option<Expr> {
    let (mut bytes, bytea) = match &prefix.kind {
        ExprKind::Const(Value::Text(t)) => (t.as_bytes().to_vec(), false),
        ExprKind::Const(Value::Bytea(b)) => (b.clone(), true),
        _ => return None,
    };
    while !bytes.is_empty() {
        let len = bytes.len();
        let width = if bytea {
            1
        } else {
            let text = String::from_utf8_lossy(&bytes);
            text.chars().next_back().map_or(1, char::len_utf8).min(len)
        };
        let last = &mut bytes[len - width..];
        let incremented = if bytea { byte_increment(&mut last[0]) } else { utf8_increment(last) };
        if incremented {
            let value = if bytea {
                Value::Bytea(bytes)
            } else {
                Value::Text(String::from_utf8(bytes).ok()?)
            };
            return Some(Expr::new(ExprKind::Const(value), prefix.ty));
        }
        bytes.truncate(len - width);
    }
    None
}

/// `byte_increment`: the byte plus one, or false when it is the greatest byte.
fn byte_increment(b: &mut u8) -> bool {
    match b.checked_add(1) {
        Some(next) => {
            *b = next;
            true
        }
        None => false,
    }
}

/// `pg_utf8_increment`: the character with its last byte that can grow incremented, or false when no byte can.
fn utf8_increment(c: &mut [u8]) -> bool {
    let len = c.len();
    if len > 4 || len == 0 {
        return false;
    }
    for at in (2..len).rev() {
        if c[at] < 0xBF {
            c[at] += 1;
            return true;
        }
    }
    if len >= 2 {
        let limit = match c[0] {
            0xED => 0x9F,
            0xF4 => 0x8F,
            _ => 0xBF,
        };
        if c[1] < limit {
            c[1] += 1;
            return true;
        }
    }
    if matches!(c[0], 0x7F | 0xDF | 0xEF | 0xF4) {
        return false;
    }
    c[0] += 1;
    true
}

/// `pattern_fixed_prefix` for `match_pattern_prefix`: the fixed prefix of the pattern of a positive `LIKE`, `ILIKE`, regular expression or `^@`, and true when the pattern is all prefix. The pattern must be a constant that is not null. The result is `None` for another operator or pattern, and for a pattern with no fixed prefix.
pub(crate) fn operator_prefix(op: u32, pattern: &Expr) -> Option<(Vec<u8>, bool)> {
    let kind = match builtin::operator_by_oid(op)?.name {
        "~~" => Pattern::Like,
        "~~*" => Pattern::LikeIc,
        "~" => Pattern::Regex,
        "~*" => Pattern::RegexIc,
        "^@" => Pattern::Prefix,
        _ => return None,
    };
    let text = match &pattern.kind {
        ExprKind::Const(Value::Text(t)) => t.as_bytes(),
        ExprKind::Const(Value::Bytea(b)) => b.as_slice(),
        _ => return None,
    };
    let (prefix, status, _) = fixed_prefix(text, kind);
    (status != Prefix::None).then_some((prefix, status == Prefix::Exact))
}

/// `Pattern_Prefix_Status`: whether a pattern has no fixed prefix, a fixed prefix and more, or a fixed prefix only.
#[derive(Clone, Copy, PartialEq)]
enum Prefix {
    None,
    Partial,
    Exact,
}

/// `pattern_fixed_prefix`: the fixed prefix of a pattern, how much of the pattern it is, and the selectivity of the rest of the pattern.
fn fixed_prefix(patt: &[u8], kind: Pattern) -> (Vec<u8>, Prefix, f64) {
    match kind {
        Pattern::Like | Pattern::LikeIc => like_fixed_prefix(patt, kind == Pattern::LikeIc),
        Pattern::Regex | Pattern::RegexIc => {
            match regex_fixed_prefix(patt, kind == Pattern::RegexIc) {
                Some((prefix, true)) => (prefix, Prefix::Exact, 1.0),
                Some((prefix, false)) => {
                    let rest = regex_selectivity(patt, prefix.len());
                    (prefix, Prefix::Partial, rest)
                }
                None => (Vec::new(), Prefix::None, regex_selectivity(patt, 0)),
            }
        }
        Pattern::Prefix => (patt.to_vec(), Prefix::Partial, 1.0),
    }
}

/// `like_fixed_prefix` and `like_fixed_prefix_ci`: the characters of a `LIKE` pattern before its first wildcard, where a backslash quotes the next character. For `ILIKE` a character that has a case also ends the prefix.
fn like_fixed_prefix(patt: &[u8], ci: bool) -> (Vec<u8>, Prefix, f64) {
    let mut prefix = Vec::new();
    let mut pos = 0;
    while pos < patt.len() {
        if patt[pos] == b'%' || patt[pos] == b'_' {
            break;
        }
        if patt[pos] == b'\\' {
            pos += 1;
            if pos >= patt.len() {
                break;
            }
        }
        let width = match char_at(patt, pos) {
            Some(c) if ci && cased(c) => break,
            Some(c) if ci => c.len_utf8(),
            _ => 1,
        };
        prefix.extend_from_slice(&patt[pos..pos + width]);
        pos += width;
    }
    let rest = like_selectivity(&patt[pos.min(patt.len())..]);
    let status = if pos >= patt.len() {
        Prefix::Exact
    } else if prefix.is_empty() {
        Prefix::None
    } else {
        Prefix::Partial
    };
    (prefix, status, rest)
}

/// The character of UTF-8 text that starts at a byte.
fn char_at(text: &[u8], pos: usize) -> Option<char> {
    let end = (pos + 4).min(text.len());
    let valid = match std::str::from_utf8(&text[pos..end]) {
        Ok(s) => s,
        Err(e) => std::str::from_utf8(&text[pos..pos + e.valid_up_to()]).ok()?,
    };
    valid.chars().next()
}

/// Whether a character has an upper case and a lower case form, as `pg_iswcased`.
fn cased(c: char) -> bool {
    c.is_lowercase() || c.is_uppercase()
}

/// `regexp_fixed_prefix`: the literal characters after a leading `^` of a regular expression, and whether a `$` ends the pattern after them, so that the pattern matches the prefix only. A pattern with an alternative at the top level has no prefix, and a character that a quantifier can repeat or skip ends the prefix. Parentheses do not end the prefix, and alternatives in parentheses add the first character that they all start with, as `findprefix` follows the states of the automaton. In a pattern that ignores case, a character that has a case ends the prefix.
fn regex_fixed_prefix(patt: &[u8], ci: bool) -> Option<(Vec<u8>, bool)> {
    let text = std::str::from_utf8(patt).ok()?;
    let chars: Vec<char> = text.chars().collect();
    if chars.first() != Some(&'^') || alternatives(&chars).len() > 1 {
        return None;
    }
    let mut prefix = String::new();
    let mut pos = 1;
    let mut exact = false;
    while pos < chars.len() {
        if chars[pos] == '(' {
            let inner = match (chars.get(pos + 1), chars.get(pos + 2)) {
                (Some('?'), Some(':')) => pos + 3,
                (Some('?'), _) => break,
                _ => pos + 1,
            };
            let Some(close) = group_end(&chars, pos) else { break };
            if matches!(chars.get(close + 1), Some('*' | '?' | '{')) {
                break;
            }
            let alts = alternatives(&chars[inner..close]);
            if alts.len() > 1 {
                let firsts: Vec<Option<char>> = alts.iter().map(|alt| literal(alt, ci)).collect();
                if let Some(Some(c)) = firsts.first()
                    && firsts.iter().all(|f| *f == Some(*c))
                {
                    prefix.push(*c);
                }
                break;
            }
            pos = inner;
            continue;
        }
        if chars[pos] == ')' {
            if chars.get(pos + 1) == Some(&'+') {
                break;
            }
            pos += 1;
            continue;
        }
        if chars[pos] == '$' {
            exact = pos + 1 == chars.len();
            break;
        }
        let Some(c) = literal(&chars[pos..], ci) else { break };
        prefix.push(c);
        pos += if chars[pos] == '\\' { 2 } else { 1 };
        if chars.get(pos) == Some(&'+') {
            break;
        }
    }
    (exact || !prefix.is_empty()).then(|| (prefix.into_bytes(), exact))
}

/// The literal character that a part of a regular expression starts with, which is not a character that a quantifier after it can skip, nor a character that has a case in a pattern that ignores case.
fn literal(chars: &[char], ci: bool) -> Option<char> {
    let (c, next) = match chars.first()? {
        '\\' => match chars.get(1) {
            Some(&q) if !q.is_alphanumeric() => (q, 2),
            _ => return None,
        },
        c if ".[()|*+?{}^$".contains(*c) => return None,
        &c => (c, 1),
    };
    let skipped = matches!(chars.get(next), Some('*' | '?' | '{'));
    (!skipped && !(ci && cased(c))).then_some(c)
}

/// The position of the `)` that closes the `(` at `open`.
fn group_end(chars: &[char], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut pos = open;
    while pos < chars.len() {
        match chars[pos] {
            '\\' => pos += 1,
            '[' => pos = class_end(chars, pos),
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(pos);
                }
            }
            _ => {}
        }
        pos += 1;
    }
    None
}

/// The parts of a regular expression between the `|` that are outside of parentheses and brackets.
fn alternatives(chars: &[char]) -> Vec<&[char]> {
    let mut out = Vec::new();
    let (mut depth, mut start, mut pos) = (0usize, 0, 0);
    while pos < chars.len() {
        match chars[pos] {
            '\\' => pos += 1,
            '[' => pos = class_end(chars, pos),
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            '|' if depth == 0 => {
                out.push(&chars[start..pos]);
                start = pos + 1;
            }
            _ => {}
        }
        pos += 1;
    }
    out.push(&chars[start.min(chars.len())..]);
    out
}

/// The position of the `]` that closes the class of characters at `open`, where a `]` first in the class is a member.
fn class_end(chars: &[char], open: usize) -> usize {
    let mut pos = open + 1;
    if chars.get(pos) == Some(&'^') {
        pos += 1;
    }
    if chars.get(pos) == Some(&']') {
        pos += 1;
    }
    while pos < chars.len() && chars[pos] != ']' {
        pos += 1;
    }
    pos
}

/// `FIXED_CHAR_SEL`: the selectivity of a fixed character of a pattern.
const FIXED_CHAR_SEL: f64 = 0.20;
/// `CHAR_RANGE_SEL`: the selectivity of a class of characters of a regular expression.
const CHAR_RANGE_SEL: f64 = 0.25;
/// `ANY_CHAR_SEL`: the selectivity of a wildcard for one character, which is not 1 since it does not match the end of the string.
const ANY_CHAR_SEL: f64 = 0.9;
/// `FULL_WILDCARD_SEL`: the selectivity of a wildcard for any characters.
const FULL_WILDCARD_SEL: f64 = 5.0;
/// `PARTIAL_WILDCARD_SEL`: the selectivity of a quantifier.
const PARTIAL_WILDCARD_SEL: f64 = 2.0;

/// `like_selectivity`: the selectivity of the rest of a `LIKE` pattern after its leading wildcards, where each byte that is not a wildcard is a fixed character.
fn like_selectivity(patt: &[u8]) -> f64 {
    let mut pos = patt.iter().take_while(|&&b| b == b'%' || b == b'_').count();
    let mut sel: f64 = 1.0;
    while pos < patt.len() {
        match patt[pos] {
            b'%' => sel *= FULL_WILDCARD_SEL,
            b'_' => sel *= ANY_CHAR_SEL,
            b'\\' => {
                pos += 1;
                if pos >= patt.len() {
                    break;
                }
                sel *= FIXED_CHAR_SEL;
            }
            _ => sel *= FIXED_CHAR_SEL,
        }
        pos += 1;
    }
    sel.min(1.0)
}

/// `regex_selectivity_sub`: the selectivity of a part of a regular expression, where the alternatives at the top level add and a part in parentheses multiplies its selectivity.
fn regex_selectivity_sub(patt: &[u8]) -> f64 {
    let mut sel: f64 = 1.0;
    let (mut depth, mut start) = (0usize, 0);
    let mut pos = 0;
    while pos < patt.len() {
        match patt[pos] {
            b'(' => {
                if depth == 0 {
                    start = pos;
                }
                depth += 1;
            }
            b')' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    sel *= regex_selectivity_sub(&patt[start + 1..pos]);
                }
            }
            b'|' if depth == 0 => {
                sel += regex_selectivity_sub(&patt[pos + 1..]);
                break;
            }
            b'[' => {
                pos += 1;
                let negated = patt.get(pos) == Some(&b'^');
                if negated {
                    pos += 1;
                }
                if patt.get(pos) == Some(&b']') {
                    pos += 1;
                }
                while pos < patt.len() && patt[pos] != b']' {
                    pos += 1;
                }
                if depth == 0 {
                    sel *= if negated { 1.0 - CHAR_RANGE_SEL } else { CHAR_RANGE_SEL };
                }
            }
            b'.' => {
                if depth == 0 {
                    sel *= ANY_CHAR_SEL;
                }
            }
            b'*' | b'?' | b'+' => {
                if depth == 0 {
                    sel *= PARTIAL_WILDCARD_SEL;
                }
            }
            b'{' => {
                while pos < patt.len() && patt[pos] != b'}' {
                    pos += 1;
                }
                if depth == 0 {
                    sel *= PARTIAL_WILDCARD_SEL;
                }
            }
            b'\\' => {
                pos += 1;
                if pos >= patt.len() {
                    break;
                }
                if depth == 0 {
                    sel *= FIXED_CHAR_SEL;
                }
            }
            _ => {
                if depth == 0 {
                    sel *= FIXED_CHAR_SEL;
                }
            }
        }
        pos += 1;
    }
    sel.min(1.0)
}

/// `regex_selectivity`: the selectivity of a regular expression, which has a wildcard at the end unless a `$` ends it, without the selectivity of its fixed prefix of `prefix` bytes.
fn regex_selectivity(patt: &[u8], prefix: usize) -> f64 {
    let n = patt.len();
    let mut sel = if n > 0 && patt[n - 1] == b'$' && (n == 1 || patt[n - 2] != b'\\') {
        regex_selectivity_sub(&patt[..n - 1])
    } else {
        regex_selectivity_sub(patt) * FULL_WILDCARD_SEL
    };
    if prefix > 0 {
        let prefix_sel = FIXED_CHAR_SEL.powi(i32::try_from(prefix).unwrap_or(i32::MAX));
        if prefix_sel > 0.0 {
            sel /= prefix_sel;
        }
    }
    sel.clamp(0.0, 1.0)
}

/// The cost of the conditions for each row, as `cost_qual_eval`.
pub(crate) fn qual_cost<'a>(quals: impl IntoIterator<Item = &'a Expr>) -> f64 {
    qual_costs(quals, &[]).1
}

/// A subquery of an expression that runs as a `SubPlan` for each row, with the costs of `cost_subplan`: the start cost, and the cost of each call with its test.
#[derive(Clone, Copy, Debug)]
pub(crate) struct SubCost {
    pub(crate) link: *const SubLink,
    pub(crate) startup: f64,
    pub(crate) per_call: f64,
}

/// `cost_qual_eval`: the start cost and the cost for each row of the conditions. A subquery that runs as a `SubPlan` costs its subplan, which has the cost of its test, as `cost_qual_eval_walker` adds `startup_cost` and `per_call_cost`.
pub(crate) fn qual_costs<'a>(
    quals: impl IntoIterator<Item = &'a Expr>,
    subplans: &[SubCost],
) -> (f64, f64) {
    fn walk(expr: &Expr, subplans: &[SubCost], calls: &mut f64, out: &mut (f64, f64)) {
        // An aggregate call and a window call cost nothing, as a column, with their arguments.
        if matches!(expr.kind, ExprKind::Agg(_) | ExprKind::Window(_)) {
            return;
        }
        // A `PlaceHolderVar` costs nothing above the node that computes it, which counts the cost of its expression.
        if matches!(expr.kind, ExprKind::PlaceHolder(_)) {
            return;
        }
        // A call of `GROUPING` costs one operator, with its arguments.
        if matches!(expr.kind, ExprKind::Grouping(_)) {
            *calls += 1.0;
            return;
        }
        if let ExprKind::SubLink(link) = &expr.kind
            && let Some(sub) = subplans.iter().find(|s| std::ptr::eq(s.link, &**link))
        {
            out.0 += sub.startup;
            out.1 += sub.per_call;
            return;
        }
        if let ExprKind::ScalarArrayOp { func, any, args } = &expr.kind {
            let (startup, per_row) = array_op_cost(*func, *any, args);
            out.0 += startup;
            out.1 += per_row;
        } else {
            *calls += expr.own_cost();
        }
        for child in expr.children() {
            walk(child, subplans, calls, out);
        }
    }
    let (mut startup, mut per_row) = (0.0, 0.0);
    for qual in quals {
        let (mut calls, mut sub) = (0.0, (0.0, 0.0));
        walk(qual, subplans, &mut calls, &mut sub);
        startup += sub.0;
        per_row += CPU_OPERATOR * calls + sub.1;
    }
    (startup, per_row)
}

/// The cost of the node of an expression for each row, without its children, in units of `cpu_operator_cost`, as `cost_qual_eval_walker` adds it. A call costs its `procost`, and `x op ANY (array)` costs its operator for half of the items of the array, or one hash and one comparison for a constant array of 9 items or more.
pub fn node_cost(expr: &Expr) -> f64 {
    match &expr.kind {
        ExprKind::ScalarArrayOp { func, any, args } => {
            array_op_cost(*func, *any, args).1 / CPU_OPERATOR
        }
        _ => expr.own_cost(),
    }
}

/// The cost of `x op ANY (array)` or `x op ALL (array)`, as `cost_qual_eval_walker` finds it: the start cost and the cost for each row. The operator runs for half of the items of the array. A hash table of the items costs one hash for each item when it starts, then one hash and one comparison for each row.
fn array_op_cost(func: u32, any: bool, args: &[Expr]) -> (f64, f64) {
    let Some(array) = args.get(1) else { return (0.0, CPU_OPERATOR) };
    let array = strip_array_coercion(array);
    let items = array_length(array);
    let op = CPU_OPERATOR * rupg_analyze::func_cost(func);
    if hashed_array_op(func, any, args.first().map_or(0, |a| a.ty), array) {
        return (items * CPU_OPERATOR, CPU_OPERATOR + op);
    }
    (0.0, op * items * 0.5)
}

/// `strip_array_coercion`: the array below a relabel and below a coercion of an array whose items only relabel.
fn strip_array_coercion(mut expr: &Expr) -> &Expr {
    loop {
        expr = match &expr.kind {
            ExprKind::Relabel(arg, _) => arg,
            ExprKind::ArrayCoerce { arg, element, .. } if matches!(&element.kind, ExprKind::Relabel(e, _) if matches!(e.kind, ExprKind::CaseTest)) => {
                arg
            }
            _ => return expr,
        };
    }
}

/// `estimate_array_length`: the items of a constant array or of a list of `ARRAY[...]` with one dimension, 0 for a null array, or 10 with no statistics.
pub(crate) fn array_length(array: &Expr) -> f64 {
    match &array.kind {
        ExprKind::Const(Value::Null) => 0.0,
        ExprKind::Const(Value::Array(a)) => a.values.len() as f64,
        ExprKind::Array { multidims: false, elements, .. } => elements.len() as f64,
        _ => 10.0,
    }
}

/// `convert_saop_to_hashed_saop`: true when the executor tests the condition with a hash table of the items. The array is a constant with 9 items or more that is not null, and the operator of `ANY`, or the negator of the operator of `ALL`, hashes its two inputs with the same function, as `get_op_hash_functions` finds it.
fn hashed_array_op(func: u32, any: bool, left: u32, array: &Expr) -> bool {
    let ExprKind::Const(Value::Array(a)) = &array.kind else { return false };
    if a.values.len() < 9 {
        return false;
    }
    let element = builtin::type_by_oid(array.ty).map_or(0, |t| t.elem);
    let Some(op) = array_operator(func, left, element) else { return false };
    let op = if any { op.oid } else { op.negator };
    builtin::amops_of_operator(op).any(|amop| {
        let hash = |ty: u32| {
            builtin::amprocs(amop.family, ty)
                .iter()
                .find(|p| p.right == ty && p.number == 1)
                .map(|p| p.proc)
        };
        amop.method == HASH_AM
            && amop.strategy == 1
            && hash(amop.left).is_some()
            && hash(amop.left) == hash(amop.right)
    })
}

/// `HASH_AM_OID`, the OID of the access method `hash`.
const HASH_AM: u32 = 405;

/// `relation_byte_size`: the bytes of rows of a width, with the header of each row.
pub(crate) fn byte_size(rows: f64, width: i32) -> f64 {
    rows * f64::from(maxalign(width) + TUPLE_HEADER)
}

/// `page_size`: the pages of rows of a width.
pub(crate) fn page_count(rows: f64, width: i32) -> f64 {
    (byte_size(rows, width) / BLOCK).ceil()
}

/// `MAXALIGN`.
pub(crate) fn maxalign(n: i32) -> i32 {
    (n + 7) & !7
}

/// The cost to write rows of a width to disk and to read them again when they do not fit in `work_mem`, as `cost_material` and `cost_rescan` find it.
pub(crate) fn spill(rows: f64, width: i32) -> f64 {
    if byte_size(rows, width) > WORK_MEM { SEQ_PAGE * page_count(rows, width) } else { 0.0 }
}

/// `cost_sort`: the costs of a sort of `rows` rows of a width that has the input costs `input`, with the number of rows of `LIMIT` when it has one.
pub(crate) fn sort(input: (f64, f64), rows: f64, width: i32, bound: Option<f64>) -> (f64, f64) {
    let (startup, run) = tuplesort(rows, width, bound);
    let startup = input.1 + startup;
    (startup, startup + run)
}

/// `cost_incremental_sort`: the costs of a sort of `rows` rows whose first keys are in order already, with `groups` groups of rows with equal first keys. Each group is a sort of its own, which starts when the input has given the rows of the group.
pub(crate) fn incremental_sort(
    input: (f64, f64),
    rows: f64,
    width: i32,
    groups: f64,
    bound: Option<f64>,
) -> (f64, f64) {
    let tuples = rows.max(2.0);
    let group_rows = tuples / groups;
    let group_input = (input.1 - input.0) / groups;
    let (group_startup, group_run) = tuplesort(group_rows, width, bound);
    let startup = group_startup + input.0 + group_input;
    let mut run =
        group_run + (group_run + group_startup) * (groups - 1.0) + group_input * (groups - 1.0);
    // Each row is compared with the first keys of its group, and each group starts and ends a sort.
    run += CPU_TUPLE * tuples + 2.0 * CPU_TUPLE * groups;
    (startup, startup + run)
}

/// The `LOG2` of `costsize.c`, which divides by a constant that is a little less than the logarithm of 2, so that the result is a little more than the exact logarithm.
#[allow(clippy::approx_constant)]
fn log2(x: f64) -> f64 {
    x.ln() / 0.693147180559945
}

/// `cost_tuplesort`: the start cost and the run cost of a sort of `rows` rows of a width, with the number of rows of `LIMIT` when it has one. A sort that does not fit in `work_mem` is a merge sort on disk.
fn tuplesort(rows: f64, width: i32, bound: Option<f64>) -> (f64, f64) {
    let input_bytes = byte_size(rows, width);
    let tuples = rows.max(2.0);
    let comparison = 2.0 * CPU_OPERATOR;
    let (output, output_bytes) = match bound {
        Some(bound) if bound > 0.0 && bound < tuples => (bound, byte_size(bound, width)),
        _ => (tuples, input_bytes),
    };
    let startup = if output_bytes > WORK_MEM {
        let pages = (input_bytes / BLOCK).ceil();
        let runs = input_bytes / WORK_MEM;
        let log_runs = if runs > MERGE_ORDER { (runs.ln() / MERGE_ORDER.ln()).ceil() } else { 1.0 };
        // Each pass reads and writes each page, and a quarter of the reads are random.
        let accesses = 2.0 * pages * log_runs;
        comparison * tuples * log2(tuples) + accesses * (SEQ_PAGE * 0.75 + RANDOM_PAGE * 0.25)
    } else if tuples > 2.0 * output || input_bytes > WORK_MEM {
        comparison * tuples * log2(2.0 * output)
    } else {
        comparison * tuples * log2(tuples)
    };
    (startup, CPU_OPERATOR * tuples)
}

/// `ExecChooseHashTableSize`: the number of buckets and of batches of the hash table of a hash join on rows of a width, with the space for the buckets of the most common values.
pub(crate) fn hash_table(rows: f64, width: i32) -> (f64, f64) {
    let rows = if rows <= 0.0 { 1000.0 } else { rows };
    // `HJTUPLE_OVERHEAD` and `MAXALIGN(SizeofMinimalTupleHeader)`.
    let tuple = 32.0 + f64::from(maxalign(width));
    let bytes = rows * tuple;
    let mut space = HASH_MEM;
    // The skew buckets take two percent of the space: a tuple, eight pointers, an `int` and the `HashSkewBucket` for each.
    let per_value = tuple + 64.0 + 4.0 + 16.0;
    space -= ((space / per_value).floor() * 2.0 / 100.0).floor() * per_value;
    let max_pointers = prev_power2((space / POINTER).floor());
    let mut buckets = next_power2(rows.ceil().min(max_pointers).max(1024.0));
    let mut batches = 1.0;
    if bytes + POINTER * buckets > space {
        let bucket = tuple + POINTER;
        let fit = if space <= bucket { 1.0 } else { next_power2((space / bucket).floor()) };
        buckets = next_power2(fit.min(max_pointers));
        let needed = (bytes / (space - POINTER * buckets)).ceil().min(max_pointers);
        batches = next_power2(needed.max(2.0));
    }
    (buckets, batches)
}

/// `EstimateSubplanHashTableSpace` with `unknownEqFalse`: the bytes of the hash table of a subplan with rows of a width. `tuplehash_estimate_space` gives a bucket of 16 bytes for each row over the fill factor of 0.9, as a power of two, with 48 bytes for the table, and each row is a `MinimalTuple`.
pub(crate) fn subplan_hash_space(rows: f64, width: i32) -> f64 {
    let buckets = next_power2((rows / 0.9).floor().max(2.0));
    16.0 * buckets + 48.0 + rows * f64::from(16 + maxalign(width))
}

/// The smallest power of two that is at least a number.
fn next_power2(n: f64) -> f64 {
    2f64.powf(n.log2().ceil())
}

/// The largest power of two that is at most a number.
fn prev_power2(n: f64) -> f64 {
    2f64.powf(n.log2().floor())
}
