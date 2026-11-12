//! The reduction of the rows of the tables of a join before the join (spec/14 section 14.8.2). Each table keeps the rows that pass its own conditions of `WHERE`, and then the rows whose key of an `=` with another table is a key of a row that the other table keeps, until no table loses a row. The join then reads and joins only these rows, so that a join of long tables makes the values of only the rows that can join.
//!
//! The rows that the reduction removes give no tuple of the join, so the join gives the same tuples in the same order. The reduction runs only when no condition of `WHERE` is volatile and no condition can fail, because the join tests the conditions on fewer rows, and in another order. A condition `x IN (SELECT ...)` of one table whose subquery cannot fail and reads no column of the query is also a condition of the table, as the semi-join of `pull_up_sublinks`, so that TPC-H Q18 joins only the orders of the subquery. A comparison of a column with a subquery `(SELECT ...)` that gives one row and cannot fail, such as the subquery of `min(ps_supplycost)` in TPC-H Q2, cannot fail too, so it does not stop the reduction, also when the subquery reads columns of the query. The join tests it on the tuples of its relations, as before. A join of two tables with one `=` of their columns can also have conditions that can fail and read the two tables, such as a subquery that reads a column of each table. The join tests such a condition only on the tuples of the `=`, and the reduction removes only the rows that have no other row for the `=`, so the condition sees the same tuples.

use std::rc::Rc;

use rupg_analyze::{Aggref, BoolOp, Expr, ExprKind, FromItem, Query, SubLinkKind};
use rupg_common::Result;
use rupg_func::Session;
use rupg_pgcatalog::builtin;
use rupg_types::Value;

use crate::columnar::{Selected, int_of};
use crate::scan::{NONE, Narrow, Tables};

/// The `=` functions of two integers or two dates whose values the reduction compares as integers: `int2eq`, `int4eq`, `int8eq`, `date_eq`, `int24eq`, `int42eq`, `int48eq`, `int84eq`, `int28eq` and `int82eq`.
const EQUALS: [u32; 10] = [63, 65, 467, 1086, 158, 159, 852, 474, 1850, 1856];

/// The non-leakproof built-in comparisons that cannot fail.
const COMPARISONS: [&str; 18] = [
    "numeric_eq",
    "numeric_ne",
    "numeric_lt",
    "numeric_le",
    "numeric_gt",
    "numeric_ge",
    "date_eq_timestamp",
    "date_ne_timestamp",
    "date_lt_timestamp",
    "date_le_timestamp",
    "date_gt_timestamp",
    "date_ge_timestamp",
    "timestamp_eq_date",
    "timestamp_ne_date",
    "timestamp_lt_date",
    "timestamp_le_date",
    "timestamp_gt_date",
    "timestamp_ge_date",
];

/// The plan of a reduction.
pub(crate) struct Plan<'q> {
    /// The conditions of each relation that read only its columns.
    pub(crate) own: Vec<Vec<&'q Expr>>,
    /// The columns of each relation that an `=` with another relation reads, in their order.
    pub(crate) keys: Vec<Vec<usize>>,
    /// Each `=` of two relations: the relation and the place in `keys` of the column of each side.
    pub(crate) edges: Vec<[(usize, usize); 2]>,
}

/// The plan of the reduction of the rows of a query with the conditions `conds` of `WHERE`, or `None` when the reduction does not run. The query must join two or more tables of the user with no children through a list in `FROM`, with no write and no row lock, and each condition must be one that cannot fail. One condition at least must be an `=` of the integer columns of two tables.
pub(crate) fn plan<'q>(
    query: &'q Query,
    conds: &[&'q Expr],
    session: &dyn Session,
) -> Option<Plan<'q>> {
    let width = query.relations.len();
    if width < 2
        || query.from.len() != width
        || query.write.is_some()
        || !query.row_marks.is_empty()
        || !query.barrier.is_empty()
        || conds.is_empty()
    {
        return None;
    }
    let catalog = session.catalog()?;
    let mut seen = vec![false; width];
    for item in &query.from {
        let FromItem::Relation(index) = item else { return None };
        let relation = query.relations.get(*index)?;
        if std::mem::replace(&mut seen[*index], true)
            || relation.subquery.is_some()
            || relation.function.is_some()
            || relation.values.is_some()
            || relation.cte.is_some()
            || relation.tablesample.is_some()
            || relation.oid < rupg_catalog::FIRST_NORMAL_OID
        {
            return None;
        }
        let table = catalog.relation(relation.oid)?;
        if table.kind != rupg_catalog::RelKind::Table
            || !catalog.all_inheritors(relation.oid).is_empty()
        {
            return None;
        }
    }
    let mut plan =
        Plan { own: vec![Vec::new(); width], keys: vec![Vec::new(); width], edges: Vec::new() };
    // The conditions that can fail and read the two tables, and the conditions that cannot fail and read the two tables.
    let (mut both, mut joins) = (false, 0);
    for &cond in conds {
        if let Some(relation) = semi_join(cond, session) {
            plan.own[relation].push(cond);
            continue;
        }
        if !harmless(cond) && !scalar_test(cond, session) {
            if width != 2 || crate::volatile(cond) || relations_of(cond).len() != 2 {
                return None;
            }
            both = true;
            continue;
        }
        if relations_of(cond).len() > 1 {
            joins += 1;
        }
        if let Some([(a, x), (b, y)]) = equal_columns(cond) {
            let mut key = |relation: usize, column: usize| {
                let keys: &mut Vec<usize> = &mut plan.keys[relation];
                match keys.iter().position(|&c| c == column) {
                    Some(at) => at,
                    None => {
                        keys.push(column);
                        keys.len() - 1
                    }
                }
            };
            let edge = [(a, key(a, x)), (b, key(b, y))];
            plan.edges.push(edge);
            continue;
        }
        if let [relation] = relations_of(cond).as_slice() {
            plan.own[*relation].push(cond);
        }
    }
    // The join tests a condition that can fail only on the tuples of its `=`, when no other condition reads the two tables.
    let one = plan.edges.len() == 1 && joins == 1;
    (!plan.edges.is_empty() && (!both || one)).then_some(plan)
}

/// `extract_restriction_or_clauses`: the conditions that the reduction can also test, from the `OR` conditions of `conds` that read two or more relations and cannot fail. For each relation of such an `OR`, when each arm of the `OR` has one or more conditions of `AND` that read only this relation, the result has the `OR` of these conditions of each arm. A tuple that passes the `OR` passes one arm, so its row of the relation passes the conditions of the relation of this arm. The reduction then keeps fewer rows, as the reduction of TPC-H Q7 keeps only the two nations of its `OR`.
pub(crate) fn restrictions(conds: &[&Expr]) -> Vec<Expr> {
    let mut out = Vec::new();
    for cond in conds {
        let ExprKind::Bool(BoolOp::Or, arms) = &cond.kind else { continue };
        let relations = relations_of(cond);
        if relations.len() < 2 || !harmless(cond) {
            continue;
        }
        'relation: for relation in relations {
            let mut parts = Vec::with_capacity(arms.len());
            for arm in arms {
                let mut all = Vec::new();
                crate::scan::conjuncts(arm, &mut all);
                let mut own: Vec<Expr> =
                    all.into_iter().filter(|e| relations_of(e) == [relation]).cloned().collect();
                let part = match own.len() {
                    0 => continue 'relation,
                    1 => own.remove(0),
                    _ => boolean(BoolOp::And, own),
                };
                parts.push(part);
            }
            out.push(boolean(BoolOp::Or, parts));
        }
    }
    out
}

/// A condition of `AND` or `OR` of `args`.
fn boolean(op: BoolOp, args: Vec<Expr>) -> Expr {
    Expr { collation: 0, kind: ExprKind::Bool(op, args), ty: 16, typmod: -1, location: None }
}

/// The relation and the column of each side of an `=` of two columns of two relations, with integer or date values.
pub(crate) fn equal_columns(cond: &Expr) -> Option<[(usize, usize); 2]> {
    let ExprKind::Func(f) = &cond.kind else { return None };
    if !EQUALS.contains(&f.oid) {
        return None;
    }
    let [a, b] = f.args.as_slice() else { return None };
    let column = |e: &Expr| {
        let mut e = e;
        while let ExprKind::Relabel(arg, _) = &e.kind {
            e = arg;
        }
        match e.kind {
            ExprKind::Var(var) if var.levels_up == 0 && var.attnum > 0 => {
                Some((var.relation, usize::try_from(var.attnum - 1).ok()?))
            }
            _ => None,
        }
    };
    let (a, b) = (column(a)?, column(b)?);
    (a.0 != b.0).then_some([a, b])
}

/// The relations that an expression reads.
pub(crate) fn relations_of(expr: &Expr) -> Vec<usize> {
    let mut relations = Vec::new();
    expr.find(0, &mut |e, depth| {
        if let ExprKind::Var(var) = e.kind
            && var.levels_up == depth
            && !relations.contains(&var.relation)
        {
            relations.push(var.relation);
        }
        None::<()>
    });
    relations
}

/// True when an expression cannot fail and is not volatile: columns of the query, constants, `AND`, `OR`, `NOT`, the tests of nulls and of truth values, and the built-in functions that are leakproof, the comparisons of `numeric` values and of a date and a timestamp, and `LIKE` with a constant pattern that does not end in an escape, with no collation of ICU.
fn harmless(expr: &Expr) -> bool {
    safe(expr, false, 0)
}

/// True when an expression cannot fail and is not volatile, as [`harmless`] tells. With `aggs`, an aggregate of [`aggregate`] can also be in the expression. A column can be a column of a query `up` levels or fewer outside the expression.
fn safe(expr: &Expr, aggs: bool, up: usize) -> bool {
    let all = |args: &[Expr]| args.iter().all(|arg| safe(arg, aggs, up));
    match &expr.kind {
        ExprKind::Var(var) => var.levels_up <= up && var.attnum > 0,
        ExprKind::Const(_) => true,
        ExprKind::Relabel(arg, _) | ExprKind::NullTest(arg, _) | ExprKind::BooleanTest(arg, _) => {
            safe(arg, aggs, up)
        }
        ExprKind::Bool(_, args) => all(args),
        ExprKind::Func(f) => {
            !f.retset && f.collation == 0 && function(f.oid, &f.args) && all(&f.args)
        }
        ExprKind::ScalarArrayOp { func, args, collation, .. } => {
            *collation == 0 && function(*func, args) && all(args)
        }
        ExprKind::Agg(agg) => aggs && aggregate(agg) && agg.args.iter().all(harmless),
        _ => false,
    }
}

/// True for an aggregate that cannot fail: a built-in `count`, a `sum` or an `avg` of integers or `numeric` values, or a `min` or a `max` of integers, `numeric` values or dates, with no `DISTINCT`, no `ORDER BY`, no `FILTER` and no collation. A `sum` of floats fails when it overflows.
fn aggregate(agg: &Aggref) -> bool {
    const SUMS: [u32; 4] = [21, 23, 20, 1700];
    const ORDERED: [u32; 5] = [21, 23, 20, 1700, 1082];
    if agg.oid >= rupg_catalog::FIRST_NORMAL_OID
        || agg.levels_up != 0
        || agg.kind != b'n'
        || agg.collation != 0
        || !agg.order.is_empty()
        || !agg.distinct.is_empty()
        || agg.filter.is_some()
        || !agg.direct.is_empty()
    {
        return false;
    }
    let Some(proc) = builtin::proc_by_oid(agg.oid) else { return false };
    let ty = agg.args.first().map(|arg| arg.ty);
    match proc.name {
        "count" => true,
        "sum" | "avg" => ty.is_some_and(|ty| SUMS.contains(&ty)),
        "min" | "max" => ty.is_some_and(|ty| ORDERED.contains(&ty)),
        _ => false,
    }
}

/// The relation of a condition `x IN (SELECT ...)` that the reduction can test on the rows of one relation, as `pull_up_sublinks` makes a semi-join of it. `x` must be an expression that cannot fail and reads one relation, the test must be an `=` of two integers or two dates, and the subquery must read no column of the query and must not fail, as [`harmless_query`] tells. The subquery then gives the same rows for each row of the relation.
pub(crate) fn semi_join(cond: &Expr, session: &dyn Session) -> Option<usize> {
    let ExprKind::SubLink(sub) = &cond.kind else { return None };
    if sub.kind != SubLinkKind::Any
        || sub.volatile
        || crate::volatile(cond)
        || crate::correlated(&sub.query)
        || !harmless_query(&sub.query, session, 0)
    {
        return None;
    }
    let ExprKind::Func(f) = &sub.test.as_ref()?.kind else { return None };
    if !EQUALS.contains(&f.oid) || f.collation != 0 {
        return None;
    }
    let [a, b] = f.args.as_slice() else { return None };
    let column = |e: &Expr| {
        let mut e = e;
        while let ExprKind::Relabel(arg, _) = &e.kind {
            e = arg;
        }
        matches!(e.kind, ExprKind::SubColumn(_))
    };
    let value = match (column(a), column(b)) {
        (false, true) => a,
        (true, false) => b,
        _ => return None,
    };
    match relations_of(value).as_slice() {
        [relation] if harmless(value) => Some(*relation),
        _ => None,
    }
}

/// True when a subquery cannot fail: a query of tables of the user with no children, with no set operation, no write, no window, no row lock, no policy, no `WITH`, no grouping sets, no `LIMIT` and no `OFFSET`, whose expressions cannot fail and whose aggregates are those of [`aggregate`]. The expressions can read the columns of the queries `up` levels or fewer outside the subquery.
fn harmless_query(query: &Query, session: &dyn Session, up: usize) -> bool {
    let Some(catalog) = session.catalog() else { return false };
    let plain = query.set_op.is_none()
        && query.write.is_none()
        && query.windows.is_empty()
        && query.row_marks.is_empty()
        && query.limit.is_none()
        && query.offset.is_none()
        && query.ctes.is_empty()
        && query.grouping_sets.is_empty()
        && query.barrier.is_empty()
        && query.security.is_empty()
        && query.minmax.is_none();
    plain
        && query.relations.iter().all(|relation| {
            relation.subquery.is_none()
                && relation.function.is_none()
                && relation.values.is_none()
                && relation.cte.is_none()
                && relation.tablesample.is_none()
                && relation.oid >= rupg_catalog::FIRST_NORMAL_OID
                && catalog
                    .relation(relation.oid)
                    .is_some_and(|table| table.kind == rupg_catalog::RelKind::Table)
                && catalog.all_inheritors(relation.oid).is_empty()
        })
        && query.exprs().iter().all(|expr| safe(expr, true, up))
}

/// True for a comparison of a value with a subquery `(SELECT ...)` that cannot fail, such as `ps_supplycost = (SELECT min(ps_supplycost) ...)` of TPC-H Q2. The comparison must be a built-in function that cannot fail, and the value must not fail. The subquery must make one group with no `GROUP BY`, so that it gives one row or no row, and it cannot fail, as [`harmless_query`] tells. It can read the columns of the query.
fn scalar_test(cond: &Expr, session: &dyn Session) -> bool {
    let ExprKind::Func(f) = &cond.kind else { return false };
    if f.retset || f.collation != 0 || !function(f.oid, &f.args) || crate::volatile(cond) {
        return false;
    }
    let [a, b] = f.args.as_slice() else { return false };
    let (value, link) = match (&a.kind, &b.kind) {
        (_, ExprKind::SubLink(link)) => (a, link),
        (ExprKind::SubLink(link), _) => (b, link),
        _ => return false,
    };
    let query = &link.query;
    link.kind == SubLinkKind::Expr
        && !link.volatile
        && harmless(value)
        && query.grouped
        && query.group.is_empty()
        && query.grouping_sets.is_empty()
        && harmless_query(query, session, 1)
}

/// True for a built-in function that cannot fail with the arguments `args`.
fn function(oid: u32, args: &[Expr]) -> bool {
    if oid >= rupg_catalog::FIRST_NORMAL_OID {
        return false;
    }
    let Some(proc) = builtin::proc_by_oid(oid) else { return false };
    if proc.volatile == b'v' || proc.retset {
        return false;
    }
    if proc.leakproof || COMPARISONS.contains(&proc.name) {
        return true;
    }
    // `MatchText`: a pattern that ends in the escape fails.
    matches!(proc.name, "textlike" | "textnlike" | "bpcharlike" | "bpcharnlike")
        && matches!(
            args.get(1).map(|a| &a.kind),
            Some(ExprKind::Const(Value::Text(pattern))) if !pattern.as_bytes().ends_with(b"\\")
        )
}

/// The keys of the rows that are left of one side of an `=`: a bitmap over the range of the keys when the keys are dense, as the keys of a table often are, or else a hash set.
enum Found {
    Bits { min: i64, bits: Vec<u64> },
    Set(rupg_kernels::hash::HashSet<i64>),
}

impl Found {
    /// The keys `keys` of the rows that `alive` marks.
    fn of(keys: &[Option<i64>], alive: &[bool]) -> Found {
        let left =
            || keys.iter().zip(alive).filter_map(|(key, &alive)| if alive { *key } else { None });
        let (mut min, mut max, mut count) = (i64::MAX, i64::MIN, 0_u64);
        for key in left() {
            min = min.min(key);
            max = max.max(key);
            count += 1;
        }
        if count == 0 {
            return Found::Bits { min: 0, bits: Vec::new() };
        }
        // A bitmap has one bit for each key of the range, which is at most 8 bytes for each key that is left.
        let span = max.abs_diff(min);
        if span < count.saturating_mul(64).max(1 << 16) && span < 1 << 32 {
            let mut bits = vec![0_u64; usize::try_from(span / 64 + 1).unwrap_or(0)];
            for key in left() {
                let at = key.abs_diff(min);
                if let Some(word) = usize::try_from(at / 64).ok().and_then(|w| bits.get_mut(w)) {
                    *word |= 1 << (at % 64);
                }
            }
            return Found::Bits { min, bits };
        }
        Found::Set(left().collect())
    }

    fn contains(&self, key: i64) -> bool {
        match self {
            Found::Bits { min, bits } => {
                let at = key.abs_diff(*min);
                key >= *min
                    && usize::try_from(at / 64)
                        .ok()
                        .and_then(|word| bits.get(word))
                        .is_some_and(|word| word >> (at % 64) & 1 == 1)
            }
            Found::Set(set) => set.contains(&key),
        }
    }
}

/// The rows of one relation for the reduction: true for each row that is left, and the key of each row for each key column of the relation.
struct Side {
    alive: Vec<bool>,
    keys: Vec<Vec<Option<i64>>>,
}

/// Runs the reduction of `plan`. `rows` has the rows of each relation that the scan read, and `selected` has the rows that pass the own conditions of each relation that the vectors tested. `tables` has the rows of `rows`, for `test`, which gives the value of a condition for a tuple. A relation of `rows` with keys keeps only the rows that are left. The result has the places of the rows to read of each relation of `selected`.
///
/// # Errors
///
/// The error of a condition.
pub(crate) fn run(
    plan: &Plan<'_>,
    rows: &mut [Rc<Vec<Vec<Value>>>],
    selected: &mut [Option<Selected>],
    tables: Tables,
    test: &mut Narrow<'_>,
) -> Result<Vec<Option<Vec<usize>>>> {
    let width = rows.len();
    let mut sides: Vec<Option<Side>> = Vec::with_capacity(width);
    for relation in 0..width {
        if let Some(selected) = &mut selected[relation] {
            let keys = std::mem::take(&mut selected.keys);
            sides.push(Some(Side { alive: vec![true; selected.places.len()], keys }));
            continue;
        }
        let columns = &plan.keys[relation];
        if columns.is_empty() {
            sides.push(None);
            continue;
        }
        let Some(places) =
            columns.iter().map(|&c| tables.place(relation, c)).collect::<Option<Vec<usize>>>()
        else {
            sides.push(None);
            continue;
        };
        let table = &tables.rows[relation];
        let mut alive = vec![true; table.len()];
        let mut tuple = vec![NONE; width];
        for (row, slot) in alive.iter_mut().enumerate() {
            tuple[relation] = row;
            for cond in &plan.own[relation] {
                if test(&tables, cond, &tuple)? != Value::Bool(true) {
                    *slot = false;
                    break;
                }
            }
        }
        let keys = places
            .iter()
            .map(|&place| table.iter().map(|row| row.get(place).and_then(int_of)).collect())
            .collect();
        sides.push(Some(Side { alive, keys }));
    }
    drop(tables);
    // The number of changes of each side, and for each direction of each `=` the number of changes of the other side when the direction last ran. A side only loses rows, so a direction whose other side did not change since it ran keeps all the rows again, and does not run.
    let mut changes = vec![0_usize; width];
    let mut ran = vec![[None::<usize>; 2]; plan.edges.len()];
    // spec/14 section 14.8.2: each pass keeps the rows of each side of each `=` whose key the other side has, until a pass keeps all the rows.
    for _ in 0..8 {
        let mut changed = false;
        for (edge, &[a, b]) in plan.edges.iter().enumerate() {
            for (direction, ((x, kx), (y, ky))) in [(a, b), (b, a)].into_iter().enumerate() {
                let Some(other) = &sides[y] else { continue };
                if sides[x].is_none() || ran[edge][direction] == Some(changes[y]) {
                    continue;
                }
                ran[edge][direction] = Some(changes[y]);
                let found = Found::of(&other.keys[ky], &other.alive);
                let Some(side) = &mut sides[x] else { continue };
                let mut lost = false;
                for (alive, key) in side.alive.iter_mut().zip(&side.keys[kx]) {
                    if *alive && !key.is_some_and(|key| found.contains(key)) {
                        *alive = false;
                        lost = true;
                    }
                }
                if lost {
                    changes[x] += 1;
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    let mut out = vec![None; width];
    for (relation, side) in sides.into_iter().enumerate() {
        let Some(side) = side else { continue };
        match &mut selected[relation] {
            Some(selected) => {
                let places = std::mem::take(&mut selected.places);
                let kept = places
                    .into_iter()
                    .zip(&side.alive)
                    .filter(|(_, alive)| **alive)
                    .map(|(p, _)| p);
                out[relation] = Some(kept.collect());
            }
            None if side.alive.contains(&false) => {
                let all = std::mem::take(&mut rows[relation]);
                let mut table = Rc::try_unwrap(all).unwrap_or_else(|all| (*all).clone());
                let mut alive = side.alive.iter();
                table.retain(|_| alive.next().copied().unwrap_or(false));
                rows[relation] = Rc::new(table);
            }
            None => {}
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::Found;

    #[test]
    fn the_keys_that_are_left_are_the_same_in_a_bitmap_and_in_a_set() {
        let cases: Vec<Vec<Option<i64>>> = vec![
            (1..=5000).map(Some).collect(),
            (0..3000).map(|i| Some(i * 3 - 4000)).chain([None, Some(i64::MIN + 1)]).collect(),
            vec![Some(i64::MIN), Some(i64::MAX), Some(0), None],
            vec![Some(7), Some(7), Some(-7)],
            (0..100).map(|i| Some(i * 1_000_000_007)).collect(),
            vec![None, None],
        ];
        for keys in cases {
            let alive: Vec<bool> = (0..keys.len()).map(|i| i % 7 != 3).collect();
            let found = Found::of(&keys, &alive);
            let want: std::collections::HashSet<i64> =
                keys.iter().zip(&alive).filter_map(|(k, &a)| if a { *k } else { None }).collect();
            let probes =
                keys.iter().flatten().flat_map(|&k| [k, k.wrapping_add(1), k.wrapping_sub(1)]);
            for probe in probes.chain([i64::MIN, i64::MAX, 0, -1, 64, 65]) {
                assert_eq!(found.contains(probe), want.contains(&probe), "key {probe}");
            }
        }
    }
}
