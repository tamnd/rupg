//! The reduction of the rows of the tables of a join before the join (spec/14 section 14.8.2). Each table keeps the rows that pass its own conditions of `WHERE`, and then the rows whose key of an `=` with another table is a key of a row that the other table keeps, until no table loses a row. The join then reads and joins only these rows, so that a join of long tables makes the values of only the rows that can join.
//!
//! The rows that the reduction removes give no tuple of the join, so the join gives the same tuples in the same order. The reduction runs only when no condition of `WHERE` can fail and no condition is volatile, because the join tests the conditions on fewer rows, and in another order.

use std::rc::Rc;

use rupg_analyze::{Expr, ExprKind, FromItem, Query};
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
    edges: Vec<[(usize, usize); 2]>,
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
    for &cond in conds {
        if !harmless(cond) {
            return None;
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
    (!plan.edges.is_empty()).then_some(plan)
}

/// The relation and the column of each side of an `=` of two columns of two relations, with integer or date values.
fn equal_columns(cond: &Expr) -> Option<[(usize, usize); 2]> {
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
fn relations_of(expr: &Expr) -> Vec<usize> {
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
    match &expr.kind {
        ExprKind::Var(var) => var.levels_up == 0 && var.attnum > 0,
        ExprKind::Const(_) => true,
        ExprKind::Relabel(arg, _) | ExprKind::NullTest(arg, _) | ExprKind::BooleanTest(arg, _) => {
            harmless(arg)
        }
        ExprKind::Bool(_, args) => args.iter().all(harmless),
        ExprKind::Func(f) => {
            !f.retset && f.collation == 0 && function(f.oid, &f.args) && f.args.iter().all(harmless)
        }
        ExprKind::ScalarArrayOp { func, args, collation, .. } => {
            *collation == 0 && function(*func, args) && args.iter().all(harmless)
        }
        _ => false,
    }
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
    // spec/14 section 14.8.2: each pass keeps the rows of each side of each `=` whose key the other side has, until a pass keeps all the rows.
    for _ in 0..8 {
        let mut changed = false;
        for &[a, b] in &plan.edges {
            for ((x, kx), (y, ky)) in [(a, b), (b, a)] {
                let Some(other) = &sides[y] else { continue };
                let found: rupg_kernels::hash::HashSet<i64> = other.keys[ky]
                    .iter()
                    .zip(&other.alive)
                    .filter_map(|(key, &alive)| if alive { *key } else { None })
                    .collect();
                let Some(side) = &mut sides[x] else { continue };
                for (alive, key) in side.alive.iter_mut().zip(&side.keys[kx]) {
                    if *alive && !key.is_some_and(|key| found.contains(&key)) {
                        *alive = false;
                        changed = true;
                    }
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
