//! The scan of the tables of the catalog and the joins of `FROM`.
//!
//! A tuple of the join is the row number of each relation of the query, or [`NONE`] for a relation that a row of an outer join does not have. The rows of each relation are read once, with only the columns that the query uses.

use std::collections::BTreeMap;

use rupg_analyze::{Expr, ExprKind, FromItem, JoinKind, Query, TABLE_OID_ATTNUM, Var};
use rupg_common::{Error, Result};
use rupg_func::Session;
use rupg_pgcatalog::{Batch, Values};
use rupg_types::{Array, Value, oid};

/// The row number of a relation that is null in a tuple.
pub(crate) const NONE: usize = usize::MAX;

/// The functions of the `=` operators that a hash join can use, because two values are equal only when they have the same form: `booleq`, `chareq`, `nameeq`, `int2eq`, `int4eq`, `texteq`, `oideq` and `int8eq`.
const HASH_EQUALS: [u32; 8] = [60, 61, 62, 63, 65, 67, 184, 467];

/// The rows of the relations of a query.
pub(crate) struct Tables {
    /// The rows of each relation, with a null for each column that the query does not use.
    pub(crate) rows: Vec<Vec<Vec<Value>>>,
    /// The OID of each relation, for `tableoid`.
    pub(crate) oids: Vec<u32>,
}

impl Tables {
    /// The value of a column in a tuple.
    pub(crate) fn var(&self, tuple: &[usize], var: Var) -> Value {
        let row = tuple.get(var.relation).copied().unwrap_or(NONE);
        if row == NONE {
            return Value::Null;
        }
        if var.attnum == TABLE_OID_ATTNUM {
            return Value::Oid(self.oids[var.relation]);
        }
        let column = usize::try_from(var.attnum - 1).unwrap_or(usize::MAX);
        self.rows[var.relation][row].get(column).cloned().unwrap_or(Value::Null)
    }
}

/// The expressions that an expression has as its arguments.
pub(crate) fn children(expr: &Expr) -> Vec<&Expr> {
    match &expr.kind {
        ExprKind::Const(_)
        | ExprKind::Param(_)
        | ExprKind::CaseTest
        | ExprKind::SqlValue(_)
        | ExprKind::Var(_) => Vec::new(),
        ExprKind::Func(f) => f.args.iter().collect(),
        ExprKind::Relabel(arg)
        | ExprKind::CoerceViaIo(arg)
        | ExprKind::NullTest(arg, _)
        | ExprKind::BooleanTest(arg, _) => vec![&**arg],
        ExprKind::Bool(_, args)
        | ExprKind::Coalesce(args)
        | ExprKind::MinMax { args, .. }
        | ExprKind::NullIf { args, .. }
        | ExprKind::Distinct { args, .. }
        | ExprKind::ScalarArrayOp { args, .. }
        | ExprKind::Array { elements: args, .. } => args.iter().collect(),
        ExprKind::Case(case) => {
            let mut all: Vec<&Expr> = case.arg.as_deref().into_iter().collect();
            for (when, then) in &case.whens {
                all.push(when);
                all.push(then);
            }
            all.push(&case.default);
            all
        }
    }
}

/// Calls `f` for each column that an expression reads.
fn each_var(expr: &Expr, f: &mut impl FnMut(Var)) {
    if let ExprKind::Var(var) = &expr.kind {
        f(*var);
    }
    for child in children(expr) {
        each_var(child, f);
    }
}

/// The relations that an expression reads.
fn relations_of(expr: &Expr) -> Vec<usize> {
    let mut relations = Vec::new();
    each_var(expr, &mut |var| {
        if !relations.contains(&var.relation) {
            relations.push(var.relation);
        }
    });
    relations
}

/// The relations of a part of `FROM`.
fn relations_in(item: &FromItem, out: &mut Vec<usize>) {
    match item {
        FromItem::Relation(index) => out.push(*index),
        FromItem::Join(join) => {
            relations_in(&join.left, out);
            relations_in(&join.right, out);
        }
    }
}

/// Every expression of a query that can read a column.
fn expressions(query: &Query) -> Vec<&Expr> {
    fn joins<'a>(item: &'a FromItem, out: &mut Vec<&'a Expr>) {
        if let FromItem::Join(join) = item {
            out.extend(join.on.as_ref());
            joins(&join.left, out);
            joins(&join.right, out);
        }
    }
    let mut all: Vec<&Expr> = query.targets.iter().map(|t| &t.expr).collect();
    all.extend(query.filter.as_ref());
    for item in &query.from {
        joins(item, &mut all);
    }
    all
}

/// Reads the rows of the relations of a query.
///
/// # Errors
///
/// The error of the input function of a column that the catalog keeps as text.
pub(crate) fn read(query: &Query, session: &dyn Session) -> Result<Tables> {
    let mut used: Vec<Vec<bool>> =
        query.relations.iter().map(|r| vec![false; r.columns.len()]).collect();
    for expr in expressions(query) {
        each_var(expr, &mut |var| {
            if let Ok(i) = usize::try_from(var.attnum - 1)
                && let Some(slot) = used[var.relation].get_mut(i)
            {
                *slot = true;
            }
        });
    }
    let mut rows = Vec::with_capacity(query.relations.len());
    for (relation, used) in query.relations.iter().zip(&used) {
        let catalog = rupg_pgcatalog::catalog_by_oid(relation.oid).ok_or_else(|| {
            Error::internal(format!("no table of the catalog has the OID {}", relation.oid))
        })?;
        let mut table = Vec::with_capacity(catalog.len);
        for row in 0..catalog.len {
            let mut values = Vec::with_capacity(used.len());
            for (i, column) in relation.columns.iter().enumerate() {
                values.push(if used[i] {
                    value(&catalog.rows[i], row, column.ty, session)?
                } else {
                    Value::Null
                });
            }
            table.push(values);
        }
        rows.push(table);
    }
    Ok(Tables { rows, oids: query.relations.iter().map(|r| r.oid).collect() })
}

/// The value of a row of a column of the catalog.
fn value(batch: &Batch, row: usize, ty: u32, session: &dyn Session) -> Result<Value> {
    if batch.is_null(row) {
        return Ok(Value::Null);
    }
    Ok(match batch.values {
        Values::Null => Value::Null,
        Values::Bool(v) => Value::Bool(v[row]),
        Values::Char(v) => Value::Char(v[row]),
        Values::Int2(v) => Value::Int2(v[row]),
        Values::Int4(v) => Value::Int4(v[row]),
        Values::Float4(v) => Value::Float4(v[row]),
        Values::Oid(v) => Value::Oid(v[row]),
        Values::Text(v) => match ty {
            oid::NAME | oid::TEXT | oid::PG_NODE_TREE | oid::ANYARRAY | oid::PG_LSN => {
                Value::text(v[row])
            }
            oid::ACLITEM_ARRAY => {
                let array = rupg_types::array_in(v[row], b',', true, |s| Ok(Value::text(s)))
                    .map_err(|e| Error::internal(e.message))?;
                Value::Array(Box::new(array))
            }
            _ => rupg_func::input(ty, v[row], -1, session)?,
        },
        Values::Int2Vector(v) => vector(v[row].iter().map(|&n| Value::Int2(n)), true),
        Values::OidVector(v) => vector(v[row].iter().map(|&n| Value::Oid(n)), true),
        Values::OidArray(v) => vector(v[row].iter().map(|&n| Value::Oid(n)), false),
        Values::CharArray(v) => vector(v[row].iter().map(|&n| Value::Char(n)), false),
        Values::TextArray(v) => vector(v[row].iter().map(|&s| Value::text(s)), false),
    })
}

/// An array of values: an `int2vector` or an `oidvector` from 0 when `from_zero` is true, or else an array from 1.
fn vector(values: impl Iterator<Item = Value>, from_zero: bool) -> Value {
    let values: Vec<Option<Value>> = values.map(Some).collect();
    let array = if from_zero { Array::vector(values) } else { Array::one(values) };
    Value::Array(Box::new(array))
}

/// The key of a value for a hash join, or `None` for a null, which equals no value.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Key {
    Int(i64),
    Text(String),
}

fn key(value: Value) -> Option<Key> {
    Some(match value {
        Value::Bool(v) => Key::Int(i64::from(v)),
        Value::Char(v) => Key::Int(i64::from(v)),
        Value::Int2(v) => Key::Int(i64::from(v)),
        Value::Int4(v) => Key::Int(i64::from(v)),
        Value::Int8(v) => Key::Int(v),
        Value::Oid(v) => Key::Int(i64::from(v)),
        Value::Text(v) => Key::Text(v),
        _ => return None,
    })
}

/// The column of an expression that is a column, or a column with another type of the same form.
fn plain_var(expr: &Expr) -> Option<Var> {
    match &expr.kind {
        ExprKind::Var(var) => Some(*var),
        ExprKind::Relabel(arg) => plain_var(arg),
        _ => None,
    }
}

/// The parts of a condition that `AND` joins.
fn conjuncts(expr: &Expr, out: &mut Vec<Expr>) {
    match &expr.kind {
        ExprKind::Bool(rupg_analyze::BoolOp::And, args) => {
            for arg in args {
                conjuncts(arg, out);
            }
        }
        _ => out.push(expr.clone()),
    }
}

/// A join: the tuples of both sides, the condition, the kind and the relations of each side.
struct JoinInput<'a> {
    left: Vec<Vec<usize>>,
    right: Vec<Vec<usize>>,
    on: &'a [Expr],
    kind: JoinKind,
    left_relations: &'a [usize],
    right_relations: &'a [usize],
}

/// The test of a condition on a tuple.
type Test<'a> = dyn FnMut(&Expr, &[usize]) -> Result<bool> + 'a;

/// True when the relations of the expression are all in the list.
fn covered(expr: &Expr, relations: &[usize]) -> bool {
    relations_of(expr).iter().all(|r| relations.contains(r))
}

/// The tuples that pass every condition.
fn keep(tuples: Vec<Vec<usize>>, conds: &[Expr], test: &mut Test<'_>) -> Result<Vec<Vec<usize>>> {
    if conds.is_empty() {
        return Ok(tuples);
    }
    let mut kept = Vec::with_capacity(tuples.len());
    'tuple: for tuple in tuples {
        for expr in conds {
            if !test(expr, &tuple)? {
                continue 'tuple;
            }
        }
        kept.push(tuple);
    }
    Ok(kept)
}

/// The tuples of the query before the target list: the parts of `FROM` joined with a cross join, and the `WHERE` condition. Each part of `WHERE` that `AND` joins goes down to the smallest part of `FROM` that has its relations, where the query can test it on fewer rows. `test` gives true when a condition is true for a tuple.
///
/// # Errors
///
/// The error of a condition.
pub(crate) fn tuples(
    query: &Query,
    tables: &Tables,
    test: &mut Test<'_>,
) -> Result<Vec<Vec<usize>>> {
    let width = query.relations.len();
    let mut filter = Vec::new();
    if let Some(expr) = &query.filter {
        conjuncts(expr, &mut filter);
    }
    let items: Vec<(&FromItem, Vec<usize>)> = query
        .from
        .iter()
        .map(|item| {
            let mut relations = Vec::new();
            relations_in(item, &mut relations);
            (item, relations)
        })
        .collect();
    // A condition with no relation stays at the top, so that it runs once for each tuple as in a query with no FROM.
    let mut own: Vec<Vec<Expr>> = vec![Vec::new(); items.len()];
    let mut rest = Vec::new();
    for expr in filter {
        let relations = relations_of(&expr);
        match items.iter().position(|(_, r)| !relations.is_empty() && covered(&expr, r)) {
            Some(i) => own[i].push(expr),
            None => rest.push(expr),
        }
    }
    let mut acc: Vec<Vec<usize>> = vec![vec![NONE; width]];
    let mut acc_relations: Vec<usize> = Vec::new();
    for ((item, relations), conds) in items.iter().zip(own) {
        let tuples = item_tuples(item, conds, tables, width, test)?;
        let mut all = acc_relations.clone();
        all.extend(relations);
        let mut on = Vec::new();
        rest.retain(|expr| {
            let relations = relations_of(expr);
            if !relations.is_empty() && covered(expr, &all) {
                on.push(expr.clone());
                false
            } else {
                true
            }
        });
        let input = JoinInput {
            left: acc,
            right: tuples,
            on: &on,
            kind: JoinKind::Inner,
            left_relations: &acc_relations,
            right_relations: relations,
        };
        acc = join(input, tables, test)?;
        acc_relations = all;
    }
    keep(acc, &rest, test)
}

/// The tuples of a part of `FROM` that pass the conditions `conds`, which read only the relations of the part.
fn item_tuples(
    item: &FromItem,
    conds: Vec<Expr>,
    tables: &Tables,
    width: usize,
    test: &mut Test<'_>,
) -> Result<Vec<Vec<usize>>> {
    let j = match item {
        FromItem::Relation(index) => {
            let tuples = (0..tables.rows[*index].len())
                .map(|row| {
                    let mut tuple = vec![NONE; width];
                    tuple[*index] = row;
                    tuple
                })
                .collect();
            return keep(tuples, &conds, test);
        }
        FromItem::Join(j) => j,
    };
    let (mut left_relations, mut right_relations) = (Vec::new(), Vec::new());
    relations_in(&j.left, &mut left_relations);
    relations_in(&j.right, &mut right_relations);
    let mut on = Vec::new();
    if let Some(expr) = &j.on {
        conjuncts(expr, &mut on);
    }
    // A condition on one side goes down to that side when the join keeps every row of that side. The other conditions of an inner join join the condition of the join, and those of an outer join test its result.
    let (mut to_left, mut to_right, mut after) = (Vec::new(), Vec::new(), Vec::new());
    let push_left = matches!(j.kind, JoinKind::Inner | JoinKind::Left);
    let push_right = matches!(j.kind, JoinKind::Inner | JoinKind::Right);
    for expr in conds {
        let relations = relations_of(&expr);
        let nonempty = !relations.is_empty();
        if push_left && nonempty && covered(&expr, &left_relations) {
            to_left.push(expr);
        } else if push_right && nonempty && covered(&expr, &right_relations) {
            to_right.push(expr);
        } else if j.kind == JoinKind::Inner {
            on.push(expr);
        } else {
            after.push(expr);
        }
    }
    // A condition of ON that reads one side only goes down to that side for an inner join, and to the side that can be null for an outer join.
    if j.kind != JoinKind::Full {
        let mut kept = Vec::with_capacity(on.len());
        for expr in on {
            let relations = relations_of(&expr);
            let nonempty = !relations.is_empty();
            if nonempty && j.kind != JoinKind::Left && covered(&expr, &left_relations) {
                to_left.push(expr);
            } else if nonempty && j.kind != JoinKind::Right && covered(&expr, &right_relations) {
                to_right.push(expr);
            } else {
                kept.push(expr);
            }
        }
        on = kept;
    }
    let left = item_tuples(&j.left, to_left, tables, width, test)?;
    let right = item_tuples(&j.right, to_right, tables, width, test)?;
    let input = JoinInput {
        left,
        right,
        on: &on,
        kind: j.kind,
        left_relations: &left_relations,
        right_relations: &right_relations,
    };
    let tuples = join(input, tables, test)?;
    keep(tuples, &after, test)
}

/// A join of two lists of tuples: a join through an index of the right side when the condition has an `=` of a column of each side that a hash can test, or else a nested loop. The rows of an outer join that match no row get nulls for the other side.
fn join(input: JoinInput<'_>, tables: &Tables, test: &mut Test<'_>) -> Result<Vec<Vec<usize>>> {
    let JoinInput { left, right, on, kind, left_relations, right_relations } = input;
    let merge = |l: &[usize], r: &[usize]| -> Vec<usize> {
        let mut tuple = l.to_vec();
        for &rel in right_relations {
            tuple[rel] = r[rel];
        }
        tuple
    };
    // The first `=` with a column of each side.
    let hash = on.iter().find_map(|expr| {
        let ExprKind::Func(f) = &expr.kind else { return None };
        if !HASH_EQUALS.contains(&f.oid) || f.args.len() != 2 {
            return None;
        }
        let (a, b) = (plain_var(&f.args[0])?, plain_var(&f.args[1])?);
        if left_relations.contains(&a.relation) && right_relations.contains(&b.relation) {
            Some((a, b))
        } else if left_relations.contains(&b.relation) && right_relations.contains(&a.relation) {
            Some((b, a))
        } else {
            None
        }
    });
    let mut right_matched = vec![false; right.len()];
    let mut out = Vec::new();
    let mut index: BTreeMap<Key, Vec<usize>> = BTreeMap::new();
    if let Some((_, r)) = hash {
        for (i, tuple) in right.iter().enumerate() {
            if let Some(k) = key(tables.var(tuple, r)) {
                index.entry(k).or_default().push(i);
            }
        }
    }
    let all: Vec<usize> = (0..right.len()).collect();
    for l in &left {
        let candidates: &[usize] = match hash {
            Some((lv, _)) => match key(tables.var(l, lv)) {
                Some(k) => index.get(&k).map_or(&[], Vec::as_slice),
                None => &[],
            },
            None => &all,
        };
        let mut matched = false;
        for &i in candidates {
            let tuple = merge(l, &right[i]);
            let mut pass = true;
            for expr in on {
                if !test(expr, &tuple)? {
                    pass = false;
                    break;
                }
            }
            if pass {
                matched = true;
                right_matched[i] = true;
                out.push(tuple);
            }
        }
        if !matched && matches!(kind, JoinKind::Left | JoinKind::Full) {
            out.push(l.clone());
        }
    }
    if matches!(kind, JoinKind::Right | JoinKind::Full) {
        for (i, r) in right.iter().enumerate() {
            if !right_matched[i] {
                let mut tuple = vec![NONE; r.len()];
                for &rel in right_relations {
                    tuple[rel] = r[rel];
                }
                out.push(tuple);
            }
        }
    }
    Ok(out)
}
