//! The estimates of `costsize.c` and `selfuncs.c` with no statistics: the number of rows of a table, the widths of the values, the selectivity of the conditions and the costs of the nodes.

use rupg_analyze::{BoolOp, BoolTest, Expr, ExprKind, Relation, SubLink, SubLinkKind};
use rupg_pgcatalog::builtin;
use rupg_types::{Value, oid};

use crate::{Member, exprs};

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
        0 => WHOLE_ROW_HEADER + row_width(relation),
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
        ExprKind::Bool(BoolOp::And, args) => list_selectivity_by(args, eq),
        ExprKind::Bool(BoolOp::Or, args) => {
            args.iter().map(each).fold(0.0, |s1, s2| s1 + s2 - s1 * s2)
        }
        ExprKind::Bool(BoolOp::Not, args) => 1.0 - args.first().map_or(0.5, each),
        ExprKind::NullTest(_, true) => EQ_SEL,
        ExprKind::NullTest(_, false) => 1.0 - EQ_SEL,
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
                [left, right] => eq(left, right).unwrap_or(EQ_SEL),
                _ => EQ_SEL,
            };
            if *not { sel } else { 1.0 - sel }
        }
        ExprKind::ScalarArrayOp { func, any, args } => array_selectivity(*func, *any, args, eq),
        _ => match exprs::binary(expr) {
            Some((op, left, right)) => {
                let name = builtin::operator_by_oid(op).map(|o| o.name);
                match (name, eq(left, right)) {
                    (Some("="), Some(sel)) => sel,
                    (Some("<>"), Some(sel)) => 1.0 - sel,
                    _ => pattern_selectivity(op, left, right, eq)
                        .unwrap_or_else(|| operator_selectivity(op)),
                }
            }
            None => INEQ_SEL,
        },
    }
}

/// `scalararraysel` with no statistics: the selectivity of the operator for each item of the array, with the chance that one item passes for `ANY` or that each item passes for `ALL`. For `= ANY` and for `<> ALL` the chances of the items are disjoint, so that they add, when the sum is a probability. An array that is not a constant or a list of `ARRAY[...]` has 10 items that the condition knows nothing about.
fn array_selectivity(
    func: u32,
    any: bool,
    args: &[Expr],
    eq: &dyn Fn(&Expr, &Expr) -> Option<f64>,
) -> f64 {
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
        match (op.name, eq(left, value)) {
            ("=", Some(sel)) => sel,
            ("<>", Some(sel)) => 1.0 - sel,
            _ => pattern_selectivity(op.oid, left, value, eq)
                .unwrap_or_else(|| operator_selectivity(op.oid)),
        }
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
    list_selectivity_by(quals, &|_, _| None)
}

/// `clauselist_selectivity` with no statistics, with `eq` as [`selectivity_by`] reads it. The product of the selectivities of the conditions, but a low bound and a high bound of the same value, such as `x > 1 AND x < 5`, are a range, as `addRangeClause` finds them. Of two low bounds or two high bounds of a value, the smaller selectivity counts. With no statistics each bound has the default selectivity, so that a range has `DEFAULT_RANGE_INEQ_SEL`.
pub(crate) fn list_selectivity_by<'a>(
    quals: impl IntoIterator<Item = &'a Expr>,
    eq: &dyn Fn(&Expr, &Expr) -> Option<f64>,
) -> f64 {
    let mut s1 = 1.0;
    // The value of each range, with the selectivity of its low bound and of its high bound.
    let mut ranges: Vec<(&Expr, Option<f64>, Option<f64>)> = Vec::new();
    for expr in quals {
        let s2 = selectivity_by(expr, eq);
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
    // `clauselist_selectivity` puts each new range first in its list.
    for (_, lo, hi) in ranges.into_iter().rev() {
        s1 *= match (lo, hi) {
            (Some(_), Some(_)) => RANGE_INEQ_SEL,
            (Some(s2), None) | (None, Some(s2)) => s2,
            (None, None) => 1.0,
        };
    }
    s1
}

/// The value of a condition that `clauselist_selectivity` puts in a range, with true when the condition is a low bound of the value: an operator `<`, `<=`, `>` or `>=`, or one of the pattern operators such as `~<~`, with the estimator `scalarltsel`, `scalarlesel`, `scalargtsel` or `scalargesel`, a value of one relation on one side and a value that reads no relation and calls no volatile function on the other side.
fn range_bound(expr: &Expr) -> Option<(&Expr, bool)> {
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

/// `patternsel_common` with no statistics: the selectivity of `LIKE`, `ILIKE`, a regular expression or `^@` of a column and a constant pattern. A pattern that has all of the value is `=` of the value, as `var_eq_const`. Otherwise the selectivity is `DEFAULT_MATCH_SEL` for the fixed prefix of the pattern, times the selectivity of the rest of the pattern, between 0.0001 and 0.9999. The result is `None` when the estimator uses its default.
fn pattern_selectivity(
    op: u32,
    left: &Expr,
    right: &Expr,
    eq: &dyn Fn(&Expr, &Expr) -> Option<f64>,
) -> Option<f64> {
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
    if !matches!(left.ty, oid::TEXT | oid::VARCHAR | oid::NAME | oid::BPCHAR | oid::BYTEA) {
        return None;
    }
    let (prefix, status, rest) = fixed_prefix(text, kind);
    let sel = if status == Prefix::Exact {
        let value = match &pattern.kind {
            ExprKind::Const(Value::Bytea(_)) => Value::Bytea(prefix),
            _ => Value::Text(String::from_utf8_lossy(&prefix).into_owned()),
        };
        eq(left, &Expr::new(ExprKind::Const(value), right.ty)).unwrap_or(EQ_SEL)
    } else {
        let prefix_sel = if status == Prefix::Partial { EQ_SEL } else { 1.0 };
        (prefix_sel * rest).clamp(0.0001, 0.9999)
    };
    Some(if negate { 1.0 - sel } else { sel }.clamp(0.0, 1.0))
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
    fn walk(expr: &Expr, subplans: &[SubCost], calls: &mut usize, out: &mut (f64, f64)) {
        // An aggregate call and a window call cost nothing, as a column, with their arguments.
        if matches!(expr.kind, ExprKind::Agg(_) | ExprKind::Window(_)) {
            return;
        }
        // A call of `GROUPING` costs one operator, with its arguments.
        if matches!(expr.kind, ExprKind::Grouping(_)) {
            *calls += 1;
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
            *calls += exprs::own_calls(expr);
        }
        for child in expr.children() {
            walk(child, subplans, calls, out);
        }
    }
    let (mut startup, mut per_row) = (0.0, 0.0);
    for qual in quals {
        let (mut calls, mut sub) = (0, (0.0, 0.0));
        walk(qual, subplans, &mut calls, &mut sub);
        startup += sub.0;
        per_row += CPU_OPERATOR * calls as f64 + sub.1;
    }
    (startup, per_row)
}

/// The cost of `x op ANY (array)` or `x op ALL (array)`, as `cost_qual_eval_walker` finds it: the start cost and the cost for each row. The operator runs for half of the items of the array. A hash table of the items costs one hash for each item when it starts, then one hash and one comparison for each row.
fn array_op_cost(func: u32, any: bool, args: &[Expr]) -> (f64, f64) {
    let Some(array) = args.get(1) else { return (0.0, CPU_OPERATOR) };
    let array = strip_array_coercion(array);
    let items = array_length(array);
    if hashed_array_op(func, any, args.first().map_or(0, |a| a.ty), array) {
        return (items * CPU_OPERATOR, CPU_OPERATOR + CPU_OPERATOR);
    }
    (0.0, CPU_OPERATOR * items * 0.5)
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
