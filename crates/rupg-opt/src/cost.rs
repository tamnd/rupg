//! The estimates of `costsize.c` and `selfuncs.c` with no statistics: the number of rows of a table, the widths of the values, the selectivity of the conditions and the costs of the nodes.

use rupg_analyze::{BoolOp, Expr, ExprKind, Relation};
use rupg_pgcatalog::builtin;
use rupg_types::oid;

use crate::{Member, exprs};

/// `seq_page_cost`.
pub(crate) const SEQ_PAGE: f64 = 1.0;
/// `cpu_tuple_cost`.
pub(crate) const CPU_TUPLE: f64 = 0.01;
/// `APPEND_CPU_COST_MULTIPLIER`: the part of `cpu_tuple_cost` that an `Append` costs for each row.
pub(crate) const APPEND_CPU: f64 = 0.5;
/// `cpu_operator_cost`.
pub(crate) const CPU_OPERATOR: f64 = 0.0025;
/// The number of pages that the planner takes for a table that `VACUUM` and `ANALYZE` have not seen, as `estimate_rel_size`.
const DEFAULT_PAGES: f64 = 10.0;
/// `BLCKSZ - SizeOfPageHeaderData`.
const PAGE_SPACE: i32 = 8168;
/// `MAXALIGN(SizeofHeapTupleHeader) + sizeof(ItemIdData)`.
const TUPLE_OVERHEAD: i32 = 28;
/// `DEFAULT_EQ_SEL`.
pub(crate) const EQ_SEL: f64 = 0.005;
/// `DEFAULT_INEQ_SEL`.
const INEQ_SEL: f64 = 1.0 / 3.0;
/// `DEFAULT_NUM_DISTINCT`.
pub(crate) const NUM_DISTINCT: f64 = 200.0;

/// `get_typavgwidth`: the average width of a value of a type with a typmod.
pub(crate) fn type_width(ty: u32, typmod: i32) -> i32 {
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

/// The width of a column of a relation, as `get_attavgwidth` with no statistics. A system column has the width of its type.
pub(crate) fn column_width(relation: &Relation, attnum: i16) -> i32 {
    match attnum {
        0 => row_width(relation),
        -1 => 6,
        a if a < 0 => 4,
        a => relation
            .columns
            .get(usize::try_from(a - 1).unwrap_or(usize::MAX))
            .map_or(32, |c| type_width(c.ty, c.typmod)),
    }
}

/// `get_rel_data_width`: the width of the columns of a relation that are not dropped.
pub(crate) fn row_width(relation: &Relation) -> i32 {
    relation.columns.iter().filter(|c| c.ty != 0).map(|c| type_width(c.ty, c.typmod)).sum()
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

/// `estimate_rel_size` for a table that has no pages yet: the pages and the rows.
pub(crate) fn table_size(relation: &Relation) -> (f64, f64) {
    let density = (PAGE_SPACE / (row_width(relation) + TUPLE_OVERHEAD)).max(1);
    (DEFAULT_PAGES, DEFAULT_PAGES * f64::from(density))
}

/// `estimate_rel_size` for a member of an inheritance tree that has no pages yet. A table with children has no pages, because `relhassubclass` keeps the planner from taking ten pages for it.
pub(crate) fn member_size(member: &Member) -> (f64, f64) {
    if member.has_children {
        return (0.0, 0.0);
    }
    let width: i32 = member.columns.iter().map(|&(ty, typmod)| type_width(ty, typmod)).sum();
    let density = (PAGE_SPACE / (width + TUPLE_OVERHEAD)).max(1);
    (DEFAULT_PAGES, DEFAULT_PAGES * f64::from(density))
}

/// `clamp_row_est`: a number of rows that is a whole number and at least 1.
pub(crate) fn clamp_rows(rows: f64) -> f64 {
    if rows <= 1.0 { 1.0 } else { rows.round() }
}

/// `clause_selectivity` with no statistics: the fraction of the rows that pass a condition.
pub(crate) fn selectivity(expr: &Expr) -> f64 {
    match &expr.kind {
        ExprKind::Bool(BoolOp::And, args) => args.iter().map(selectivity).product(),
        ExprKind::Bool(BoolOp::Or, args) => {
            args.iter().map(selectivity).fold(0.0, |s1, s2| s1 + s2 - s1 * s2)
        }
        ExprKind::Bool(BoolOp::Not, args) => 1.0 - args.first().map_or(0.5, selectivity),
        ExprKind::NullTest(_, true) => EQ_SEL,
        ExprKind::NullTest(_, false) => 1.0 - EQ_SEL,
        ExprKind::Const(_) if exprs::is_true(expr) => 1.0,
        ExprKind::Const(_) => 0.0,
        _ => match exprs::binary(expr) {
            Some((op, _, _)) => operator_selectivity(op),
            None => INEQ_SEL,
        },
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
        _ => INEQ_SEL,
    }
}

/// The cost of the conditions for each row, as `cost_qual_eval`.
pub(crate) fn qual_cost<'a>(quals: impl IntoIterator<Item = &'a Expr>) -> f64 {
    quals.into_iter().map(|q| CPU_OPERATOR * exprs::calls(q) as f64).sum()
}

/// `cost_sort`: the costs of a sort of `rows` rows that has the input costs `input`, with the number of rows of `LIMIT` when it has one.
pub(crate) fn sort(input: (f64, f64), rows: f64, bound: Option<f64>) -> (f64, f64) {
    let tuples = rows.max(2.0);
    let comparison = 2.0 * CPU_OPERATOR;
    let startup = match bound {
        Some(bound) if bound > 0.0 && tuples > 2.0 * bound => {
            input.1 + comparison * tuples * (2.0 * bound).log2()
        }
        _ => input.1 + comparison * tuples * tuples.log2(),
    };
    (startup, startup + CPU_OPERATOR * tuples)
}

/// `estimate_num_groups` with no statistics: the number of groups of the rows for a number of keys.
pub(crate) fn groups(rows: f64, keys: usize) -> f64 {
    let mut groups: f64 = 1.0;
    for _ in 0..keys {
        groups *= NUM_DISTINCT;
        if groups >= rows {
            break;
        }
    }
    clamp_rows(groups.min(rows))
}
