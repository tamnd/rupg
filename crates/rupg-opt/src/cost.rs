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
pub(crate) const INEQ_SEL: f64 = 1.0 / 3.0;
/// `DEFAULT_NUM_DISTINCT`.
pub(crate) const NUM_DISTINCT: f64 = 200.0;
/// `disable_cost`: the cost that a plan gets when the planner should use it only when it has no other plan.
pub(crate) const DISABLE: f64 = 1.0e10;
/// `random_page_cost`.
const RANDOM_PAGE: f64 = 4.0;
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

/// `clamp_row_est`: a number of rows that is a whole number and at least 1. As `rint`, a half goes to the even number.
pub(crate) fn clamp_rows(rows: f64) -> f64 {
    if rows <= 1.0 { 1.0 } else { rows.round_ties_even() }
}

/// `clause_selectivity` with no statistics: the fraction of the rows that pass a condition.
pub(crate) fn selectivity(expr: &Expr) -> f64 {
    selectivity_by(expr, &|_, _| None)
}

/// `clause_selectivity` with no statistics, where `eq` can give the selectivity of `=` of two values, as `eqsel` finds it for a unique column. `<>` is the rest of the rows, as `neqsel`.
pub(crate) fn selectivity_by(expr: &Expr, eq: &dyn Fn(&Expr, &Expr) -> Option<f64>) -> f64 {
    let each = |e: &Expr| selectivity_by(e, eq);
    match &expr.kind {
        ExprKind::Bool(BoolOp::And, args) => args.iter().map(each).product(),
        ExprKind::Bool(BoolOp::Or, args) => {
            args.iter().map(each).fold(0.0, |s1, s2| s1 + s2 - s1 * s2)
        }
        ExprKind::Bool(BoolOp::Not, args) => 1.0 - args.first().map_or(0.5, each),
        ExprKind::NullTest(_, true) => EQ_SEL,
        ExprKind::NullTest(_, false) => 1.0 - EQ_SEL,
        ExprKind::Const(_) if exprs::is_true(expr) => 1.0,
        ExprKind::Const(_) => 0.0,
        _ => match exprs::binary(expr) {
            Some((op, left, right)) => {
                let name = builtin::operator_by_oid(op).map(|o| o.name);
                match (name, eq(left, right)) {
                    (Some("="), Some(sel)) => sel,
                    (Some("<>"), Some(sel)) => 1.0 - sel,
                    _ => operator_selectivity(op),
                }
            }
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

/// `relation_byte_size`: the bytes of rows of a width, with the header of each row.
pub(crate) fn byte_size(rows: f64, width: i32) -> f64 {
    rows * f64::from(maxalign(width) + TUPLE_HEADER)
}

/// `page_size`: the pages of rows of a width.
pub(crate) fn page_count(rows: f64, width: i32) -> f64 {
    (byte_size(rows, width) / BLOCK).ceil()
}

/// `MAXALIGN`.
fn maxalign(n: i32) -> i32 {
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
        comparison * tuples * tuples.log2() + accesses * (SEQ_PAGE * 0.75 + RANDOM_PAGE * 0.25)
    } else if tuples > 2.0 * output || input_bytes > WORK_MEM {
        comparison * tuples * (2.0 * output).log2()
    } else {
        comparison * tuples * tuples.log2()
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

/// The smallest power of two that is at least a number.
fn next_power2(n: f64) -> f64 {
    2f64.powf(n.log2().ceil())
}

/// The largest power of two that is at most a number.
fn prev_power2(n: f64) -> f64 {
    2f64.powf(n.log2().floor())
}

/// `estimate_num_groups` with no statistics for a number of keys whose relations the planner does not know, as the columns of a set operation: 200 groups for each key, at most the rows.
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
