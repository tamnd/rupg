//! Partition pruning, as `prune_append_rel_partitions` of `partprune.c` does it before a scan: the leaf partitions that can have rows that pass the conditions of `WHERE` on the partition key. A condition counts when it compares a key column with a constant by an operator of the operator family of the key column, compares the column with the elements of a constant array by `= ANY`, or is `IS NULL` of the column. An `OR` keeps the leaves that one of its parts keeps. A leaf that no row of the conditions can go to gives no rows, so the scan does not read it.

use std::cmp::Ordering;

use rupg_analyze::{BoolOp, Expr, ExprKind};
use rupg_catalog::{PartStrategy, RelKind, Relation};
use rupg_common::Result;
use rupg_func::Session;
use rupg_pgcatalog::builtin;
use rupg_types::{Value, oid};

use super::{Bound, Datum, Dispatch, catalog, relation};

/// The most sets of key values that the pruning tries for the `=` conditions, as one set for each combination of the values of each key column.
const MAX_COMBINATIONS: usize = 64;

/// The strategy numbers of a btree operator family.
const LESS: i16 = 1;
const LESS_EQUAL: i16 = 2;
const EQUAL: i16 = 3;
const GREATER_EQUAL: i16 = 4;
const GREATER: i16 = 5;

/// The strategy number of `=` in a hash operator family.
const HASH_EQUAL: i16 = 1;

/// A bound of the values of a key column, with true when the bound itself is allowed, or `None` for no bound.
type Limit = Option<(Value, bool)>;

/// The values of a key of one column that no range partition has: from the first value up to and not with the end. `None` is the least value at the start and the greatest value at the end.
type Gap<'v> = (Option<&'v Value>, Option<&'v Value>);

/// A condition of the query on a column of the table, as `match_clause_to_partition_key` finds it.
#[derive(Debug)]
struct Clause {
    /// The number of the column in the table of the query.
    attnum: i16,
    /// The operator. With `= ANY`, the operator of each element.
    operator: u32,
    /// True when the constant is on the left side of the operator.
    commuted: bool,
    /// True for `= ANY` of an array.
    any: bool,
    /// The constants, which are not null, and their type. `IS NULL` has no operator and the one value null.
    values: Vec<Value>,
    ty: u32,
}

/// The values that the conditions let a key column have.
#[derive(Debug, Default)]
struct Allowed {
    /// The values of the first `=`, `= ANY` or `IS NULL`.
    equal: Option<Vec<Value>>,
    /// The greatest lower bound.
    lower: Limit,
    /// The least upper bound.
    upper: Limit,
}

/// The leaf partitions of the partitioned table `oid` that can have rows that pass each of the conditions `conds`, in the order of [`super::leaves`]. `index` is the number of the table in the relations of the query, which the columns of the conditions refer to. `nullable` is true when an outer join can give the table as nulls, so that `IS NULL` is true for a row that the table does not have.
///
/// # Errors
///
/// The errors of the compare of the bounds.
pub(crate) fn pruned_leaves(
    oid: u32,
    index: usize,
    conds: &[&Expr],
    nullable: bool,
    session: &dyn Session,
) -> Result<Vec<u32>> {
    let root = relation(catalog(session)?, oid)?;
    let mut clauses = Vec::new();
    let mut ors = Vec::new();
    for cond in conds {
        match &cond.kind {
            ExprKind::Bool(BoolOp::Or, args) => ors.push(args),
            _ => clauses.extend(clause(cond, index, nullable)),
        }
    }
    let mut out = Vec::new();
    prune(root, root, &clauses, session, &mut out)?;
    // `gen_partprune_steps_internal`: the leaves of an `OR` are the leaves of its parts together.
    for args in ors {
        let mut any = Vec::new();
        for arg in args {
            let mut parts = Vec::new();
            and_parts(arg, &mut parts);
            any.extend(pruned_leaves(oid, index, &parts, nullable, session)?);
        }
        out.retain(|leaf| any.contains(leaf));
    }
    Ok(out)
}

/// The parts of a condition that `AND` joins.
fn and_parts<'q>(expr: &'q Expr, out: &mut Vec<&'q Expr>) {
    match &expr.kind {
        ExprKind::Bool(BoolOp::And, args) => {
            for arg in args {
                and_parts(arg, out);
            }
        }
        _ => out.push(expr),
    }
}

/// Adds to `out` the leaves under the partitioned table `rel` that the conditions keep. `root` is the table of the query.
fn prune(
    root: &Relation,
    rel: &Relation,
    clauses: &[Clause],
    session: &dyn Session,
    out: &mut Vec<u32>,
) -> Result<()> {
    let catalog = catalog(session)?;
    let d = Dispatch::new(rel, session)?;
    let allowed = d.allowed(rel, root, clauses);
    for (part, keep) in d.parts.iter().zip(d.kept(&allowed, session)) {
        if !keep {
            continue;
        }
        let child = relation(catalog, part.oid)?;
        if child.kind == RelKind::Partitioned {
            prune(root, child, clauses, session, out)?;
        } else {
            out.push(part.oid);
        }
    }
    Ok(())
}

/// The condition `expr` when it compares a column of the relation `index` with a constant that is not null, or when it is `IS NULL` of the column and the relation is not `nullable`.
fn clause(expr: &Expr, index: usize, nullable: bool) -> Option<Clause> {
    match &expr.kind {
        ExprKind::NullTest(arg, true) if !nullable => Some(Clause {
            attnum: column(arg, index)?,
            operator: 0,
            commuted: false,
            any: false,
            values: vec![Value::Null],
            ty: arg.ty,
        }),
        ExprKind::Func(f) if f.args.len() == 2 => {
            let (attnum, (value, ty), commuted) =
                match (column(&f.args[0], index), constant(&f.args[1])) {
                    (Some(attnum), Some(c)) => (attnum, c, false),
                    _ => (column(&f.args[1], index)?, constant(&f.args[0])?, true),
                };
            let operator = operator(f.oid, f.args[0].ty, f.args[1].ty)?;
            Some(Clause { attnum, operator, commuted, any: false, values: vec![value.clone()], ty })
        }
        ExprKind::ScalarArrayOp { func, any: true, args } if args.len() == 2 => {
            let attnum = column(&args[0], index)?;
            let element = builtin::type_by_oid(args[1].ty).map_or(0, |row| row.elem);
            // A null element is not equal to a value, so it does not count.
            let values = match &args[1].kind {
                ExprKind::Const(Value::Array(array)) => {
                    array.values.iter().flatten().cloned().collect()
                }
                // The list of `IN`, which `eval_const_expressions` makes a constant array.
                ExprKind::Array { multidims: false, elements, .. } => {
                    let mut values = Vec::new();
                    for element in elements {
                        match &element.kind {
                            ExprKind::Const(Value::Null) => {}
                            _ => values.push(constant(element)?.0.clone()),
                        }
                    }
                    values
                }
                _ => return None,
            };
            let operator = operator(*func, args[0].ty, element)?;
            Some(Clause { attnum, operator, commuted: false, any: true, values, ty: element })
        }
        _ => None,
    }
}

/// The number of the column when the expression is a column of the relation `index`, also with another type of the same form.
fn column(expr: &Expr, index: usize) -> Option<i16> {
    match &expr.kind {
        ExprKind::Var(var) if var.levels_up == 0 && var.relation == index && var.attnum > 0 => {
            Some(var.attnum)
        }
        ExprKind::Relabel(arg, _) => column(arg, index),
        _ => None,
    }
}

/// The value and the type of a constant that is not null.
fn constant(expr: &Expr) -> Option<(&Value, u32)> {
    match &expr.kind {
        ExprKind::Const(value) if !value.is_null() => Some((value, expr.ty)),
        ExprKind::Relabel(arg, _) => constant(arg).map(|(value, _)| (value, expr.ty)),
        _ => None,
    }
}

/// The built-in operator of the function `func` for the two input types.
fn operator(func: u32, left: u32, right: u32) -> Option<u32> {
    builtin::operators()
        .iter()
        .find(|o| o.code == func && o.left == left && o.right == right)
        .map(|o| o.oid)
}

/// A constant of the type `from` as a value of the type `to` of a key column, when the two types have the same values: the same type, two integer types and a value in the range of `to`, or `text` and `varchar`.
fn fit(value: &Value, from: u32, to: u32) -> Option<Value> {
    if from == to {
        return Some(value.clone());
    }
    let integer = |ty: u32| matches!(ty, oid::INT2 | oid::INT4 | oid::INT8);
    let text = |ty: u32| matches!(ty, oid::TEXT | oid::VARCHAR);
    if integer(from) && integer(to) {
        let v = value.as_i64()?;
        return match to {
            oid::INT2 => i16::try_from(v).ok().map(Value::Int2),
            oid::INT4 => i32::try_from(v).ok().map(Value::Int4),
            _ => Some(Value::Int8(v)),
        };
    }
    (text(from) && text(to)).then(|| value.clone())
}

impl Dispatch {
    /// `gen_partprune_steps`: what the conditions let each key column be. A key column of an expression and a column of a type with another set of values get no limit.
    fn allowed(&self, rel: &Relation, root: &Relation, clauses: &[Clause]) -> Vec<Allowed> {
        let mut out: Vec<Allowed> = self.attrs.iter().map(|_| Allowed::default()).collect();
        for (i, (attr, allowed)) in self.attrs.iter().zip(&mut out).enumerate() {
            // The key column in the columns of the table of the query, by its name.
            let Some(name) = attr.and_then(|a| rel.columns.get(a)).map(|c| &c.name) else {
                continue;
            };
            let Some(at) = root.columns.iter().position(|c| !c.dropped && &c.name == name) else {
                continue;
            };
            let Ok(attnum) = i16::try_from(at + 1) else { continue };
            let ty = self.types[i];
            for clause in clauses.iter().filter(|c| c.attnum == attnum) {
                if clause.operator == 0 {
                    match &mut allowed.equal {
                        Some(old) => old.retain(Value::is_null),
                        None => allowed.equal = Some(vec![Value::Null]),
                    }
                    continue;
                }
                let Some(strategy) = self.strategy_of(i, clause) else { continue };
                let values: Option<Vec<Value>> =
                    clause.values.iter().map(|v| fit(v, clause.ty, ty)).collect();
                let Some(values) = values else { continue };
                if strategy == EQUAL {
                    // Two conditions of `=` on the column leave the values that both allow.
                    let merged = match allowed.equal.take() {
                        None => values,
                        Some(old) => old
                            .into_iter()
                            .filter(|o| {
                                values.iter().any(|v| self.cmp(i, o, v) == Some(Ordering::Equal))
                            })
                            .collect(),
                    };
                    allowed.equal = Some(merged);
                    continue;
                }
                // A collation can give another order than the order of the bounds.
                if clause.any || self.collated[i] {
                    continue;
                }
                let Some(value) = values.into_iter().next() else { continue };
                let inclusive = matches!(strategy, LESS_EQUAL | GREATER_EQUAL);
                let (bound, tighter) = match strategy {
                    GREATER | GREATER_EQUAL => (&mut allowed.lower, Ordering::Greater),
                    _ => (&mut allowed.upper, Ordering::Less),
                };
                let replace = match bound {
                    None => true,
                    Some((old, old_inclusive)) => match self.cmp(i, &value, old) {
                        Some(Ordering::Equal) => *old_inclusive && !inclusive,
                        Some(order) => order == tighter,
                        None => false,
                    },
                };
                if replace {
                    *bound = Some((value, inclusive));
                }
            }
        }
        out
    }

    /// The strategy of the operator of a condition in the operator family of the key column `i`, as the column is on its left side. A hash family gives only `=`.
    fn strategy_of(&self, i: usize, clause: &Clause) -> Option<i16> {
        let family = self.families[i];
        let strategy = builtin::amops_of_operator(clause.operator)
            .find(|a| a.family == family && a.purpose == b's')
            .map(|a| a.strategy)
            .or_else(|| {
                let mut amops = self.amops.iter();
                amops.find(|a| a.0 == family && a.1 == clause.operator).map(|a| a.2)
            })?;
        if self.strategy == PartStrategy::Hash {
            return (strategy == HASH_EQUAL).then_some(EQUAL);
        }
        if !clause.commuted {
            return Some(strategy);
        }
        Some(match strategy {
            LESS => GREATER,
            LESS_EQUAL => GREATER_EQUAL,
            GREATER_EQUAL => LESS_EQUAL,
            GREATER => LESS,
            strategy => strategy,
        })
    }

    /// For each partition, true when it can have rows that pass the conditions.
    fn kept(&self, allowed: &[Allowed], session: &dyn Session) -> Vec<bool> {
        let all = vec![true; self.parts.len()];
        // Conditions that no value passes, as `k = 1 AND k = 2`, leave no partition.
        if allowed.iter().any(|a| a.equal.as_ref().is_some_and(Vec::is_empty)) {
            return vec![false; self.parts.len()];
        }
        if !allowed.is_empty() && allowed.iter().all(|a| a.equal.is_some()) {
            return self.kept_equal(allowed, session).unwrap_or(all);
        }
        let Some(first) = allowed.first() else { return all };
        if allowed.iter().all(|a| a.equal.is_none() && a.lower.is_none() && a.upper.is_none()) {
            return all;
        }
        match self.strategy {
            PartStrategy::Hash => all,
            PartStrategy::List => self
                .parts
                .iter()
                .map(|part| match &part.bound {
                    // A strict operator is not true for a null, so the null value of a list does not count.
                    Bound::List(datums) => {
                        datums.iter().flatten().any(|d| self.within(first, d).unwrap_or(true))
                    }
                    _ => true,
                })
                .collect(),
            // A range partition has no row with a null in a key column.
            PartStrategy::Range
                if allowed
                    .iter()
                    .any(|a| a.equal.as_ref().is_some_and(|e| e.iter().all(Value::is_null))) =>
            {
                self.parts.iter().map(|part| matches!(part.bound, Bound::Default)).collect()
            }
            PartStrategy::Range => {
                let single = self.types.len() == 1;
                self.parts
                    .iter()
                    .map(|part| match &part.bound {
                        Bound::Range { lower, upper } => {
                            self.may_hold(first, &lower[0], &upper[0], single).unwrap_or(true)
                        }
                        // The default partition has the values between the other partitions.
                        _ if single => self.in_gap(first).unwrap_or(true),
                        _ => true,
                    })
                    .collect()
            }
        }
    }

    /// `get_matching_hash_bounds`, `get_matching_list_bounds` and `get_matching_range_bounds` when each key column has an `=`: the partitions that `get_partition_for_tuple` gives for each combination of the values. `None` for too many combinations or the error of a compare.
    fn kept_equal(&self, allowed: &[Allowed], session: &dyn Session) -> Option<Vec<bool>> {
        let sets: Vec<&Vec<Value>> = allowed.iter().filter_map(|a| a.equal.as_ref()).collect();
        let mut count: usize = 1;
        for set in &sets {
            count = count.checked_mul(set.len())?;
        }
        if count > MAX_COMBINATIONS {
            return None;
        }
        let mut keep = vec![false; self.parts.len()];
        for combination in 0..count {
            let mut rest = combination;
            let values: Vec<Value> = sets
                .iter()
                .map(|set| {
                    let value = set[rest % set.len()].clone();
                    rest /= set.len();
                    value
                })
                .collect();
            if let Some(part) = self.find(&values, session).ok()? {
                let at = self.parts.iter().position(|p| p.oid == part.oid)?;
                keep[at] = true;
            }
        }
        Some(keep)
    }

    /// True when the value `datum` of the first key column passes the bounds of `allowed`. `None` for the error of a compare.
    fn within(&self, allowed: &Allowed, datum: &Value) -> Option<bool> {
        if let Some((lower, inclusive)) = &allowed.lower {
            match self.cmp(0, datum, lower)? {
                Ordering::Less => return Some(false),
                Ordering::Equal if !inclusive => return Some(false),
                _ => {}
            }
        }
        if let Some((upper, inclusive)) = &allowed.upper {
            match self.cmp(0, datum, upper)? {
                Ordering::Greater => return Some(false),
                Ordering::Equal if !inclusive => return Some(false),
                _ => {}
            }
        }
        Some(true)
    }

    /// True when a range partition from `lower` to `upper` in the first key column can have a value that passes the bounds of `allowed`. With one key column the partition has the values from `lower` up to and not with `upper`. With more key columns, the first column of a row can also be `upper`. `None` for the error of a compare.
    fn may_hold(
        &self,
        allowed: &Allowed,
        lower: &Datum,
        upper: &Datum,
        single: bool,
    ) -> Option<bool> {
        let (below, above) = bounds(allowed);
        if let (Some((value, inclusive)), Datum::Value(lower)) = (&above, lower) {
            match self.cmp(0, value, lower)? {
                Ordering::Less => return Some(false),
                Ordering::Equal if !inclusive => return Some(false),
                _ => {}
            }
        }
        if let (Some((value, inclusive)), Datum::Value(upper)) = (&below, upper) {
            match self.cmp(0, value, upper)? {
                Ordering::Greater => return Some(false),
                Ordering::Equal if single || !inclusive => return Some(false),
                _ => {}
            }
        }
        Some(true)
    }

    /// True when a value of a key of one column that passes the bounds of `allowed` can be in no range partition, so that it is in the default partition. `None` for the error of a compare.
    fn in_gap(&self, allowed: &Allowed) -> Option<bool> {
        let (below, above) = bounds(allowed);
        // The start of the values after the partitions so far, or `None` for the least value.
        let mut from: Option<&Value> = None;
        let mut gaps: Vec<Gap<'_>> = Vec::new();
        for part in &self.parts {
            let Bound::Range { lower, upper } = &part.bound else { continue };
            match (&lower[0], from) {
                (Datum::Value(lower), None) => gaps.push((None, Some(lower))),
                (Datum::Value(lower), Some(at)) if self.cmp(0, at, lower)? == Ordering::Less => {
                    gaps.push((Some(at), Some(lower)));
                }
                _ => {}
            }
            match &upper[0] {
                Datum::Value(upper) => from = Some(upper),
                _ => return self.hits(&gaps, below.as_ref(), above.as_ref()),
            }
        }
        gaps.push((from, None));
        self.hits(&gaps, below.as_ref(), above.as_ref())
    }

    /// True when one of the gaps, each from its first value up to and not with its end, has a value between `below` and `above`.
    fn hits(
        &self,
        gaps: &[Gap<'_>],
        below: Option<&(Value, bool)>,
        above: Option<&(Value, bool)>,
    ) -> Option<bool> {
        for &(start, end) in gaps {
            if let (Some((value, inclusive)), Some(start)) = (above, start) {
                match self.cmp(0, value, start)? {
                    Ordering::Less => continue,
                    Ordering::Equal if !inclusive => continue,
                    _ => {}
                }
            }
            if let (Some((value, _)), Some(end)) = (below, end)
                && self.cmp(0, value, end)? != Ordering::Less
            {
                continue;
            }
            return Some(true);
        }
        Some(false)
    }

    /// The order of two values of the key column `i`, or `None` for the error of the compare.
    fn cmp(&self, i: usize, a: &Value, b: &Value) -> Option<Ordering> {
        let ty = self.types[i];
        rupg_func::compare(ty, a, ty, b).ok()
    }
}

/// The lower and the upper bound of a key column, also from one value of `=`.
fn bounds(allowed: &Allowed) -> (Limit, Limit) {
    match &allowed.equal {
        Some(values) if values.len() == 1 => {
            let value = (values[0].clone(), true);
            (Some(value.clone()), Some(value))
        }
        _ => (allowed.lower.clone(), allowed.upper.clone()),
    }
}
