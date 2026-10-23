//! `generate_partition_qual` of `partcache.c` and `get_qual_from_partbound` of `partbounds.c`: the partition constraint of a partition, the expression that `pg_get_partition_constraintdef` shows.

use std::cmp::Ordering;

use rupg_analyze::{BoolOp, CastForm, Expr, ExprKind, Func, FuncForm, Var, node, types};
use rupg_catalog::{Catalog, PartStrategy, PartitionBound, PartitionKey, RangeDatum, Relation};
use rupg_common::{Error, Result};
use rupg_pgcatalog::builtin::{self, OperatorRow};
use rupg_types::{Value, oid};

use super::expr_collation;
use crate::compare;

/// The btree strategy numbers of the operators of a partition constraint.
const LESS: i16 = 1;
const LESS_EQUAL: i16 = 2;
const EQUAL: i16 = 3;
const GREATER_EQUAL: i16 = 4;
const GREATER: i16 = 5;

/// `F_SATISFIES_HASH_PARTITION`.
const SATISFIES_HASH_PARTITION: u32 = 5028;

/// One column of a partition key, as `RelationGetPartitionKey` gives it.
struct KeyColumn {
    /// The column of the parent, as `makeVar` makes it, or the expression of `partexprs`.
    expr: Expr,
    /// `parttypcoll`: the collation of the column or of the expression.
    collation: u32,
    /// `partcollation`: the collation of the key.
    key_collation: u32,
    /// `partopfamily`.
    family: u32,
    /// `partopcintype`.
    intype: u32,
}

/// An expression of type `boolean`.
fn boolean(kind: ExprKind) -> Expr {
    Expr { kind, ty: oid::BOOL, typmod: -1, location: None }
}

/// The one expression of a list of one, or else `AND` or `OR` of the expressions.
fn combine(op: BoolOp, mut list: Vec<Expr>) -> Expr {
    if list.len() == 1 { list.remove(0) } else { boolean(ExprKind::Bool(op, list)) }
}

/// The value of a constant of a bound.
fn constant(e: &Expr) -> Result<&Value> {
    match &e.kind {
        ExprKind::Const(value) => Ok(value),
        _ => Err(Error::internal("a partition bound that is not a constant")),
    }
}

/// The order of two constants of the type of a key column.
fn order(a: &Expr, b: &Expr) -> Result<Ordering> {
    let (x, y) = (constant(a)?, constant(b)?);
    match (x.is_null(), y.is_null()) {
        (false, false) => compare(a.ty, x, b.ty, y),
        (x_null, y_null) => Ok(x_null.cmp(&y_null)),
    }
}

/// The columns of the partition key of `parent`.
fn key_columns(catalog: &Catalog, parent: &Relation, key: &PartitionKey) -> Result<Vec<KeyColumn>> {
    let exprs = match &key.exprs {
        Some(text) => node::read_list(text)?,
        None => Vec::new(),
    };
    let mut exprs = exprs.into_iter();
    let mut columns = Vec::with_capacity(key.attrs.len());
    for (keyno, &attnum) in key.attrs.iter().enumerate() {
        let (expr, collation) = if attnum != 0 {
            let column = parent.column(attnum).ok_or_else(|| {
                Error::internal(format!("cache lookup failed for attribute {attnum}"))
            })?;
            let var = ExprKind::Var(Var { relation: 0, attnum, levels_up: 0 });
            let expr = Expr { kind: var, ty: column.ty, typmod: column.typmod, location: None };
            (expr, column.collation)
        } else {
            let expr =
                exprs.next().ok_or_else(|| Error::internal("too few entries in partexprs list"))?;
            let collation = expr_collation(&expr, parent);
            (expr, collation)
        };
        let class = key.classes.get(keyno).copied().unwrap_or(0);
        let (family, intype) = match catalog.opclass(class) {
            Some(c) => (c.family, c.intype),
            None => builtin::opclasses()
                .iter()
                .find(|c| c.oid == class)
                .map(|c| (c.family, c.intype))
                .ok_or_else(|| {
                    Error::internal(format!("cache lookup failed for opclass {class}"))
                })?,
        };
        columns.push(KeyColumn {
            expr,
            collation,
            key_collation: key.collations.get(keyno).copied().unwrap_or(0),
            family,
            intype,
        });
    }
    Ok(columns)
}

/// `get_partition_operator`: the operator of the strategy in the operator family of the key column.
fn operator(column: &KeyColumn, strategy: i16) -> Result<&'static OperatorRow> {
    builtin::opfamily_member(column.family, column.intype, column.intype, strategy)
        .and_then(builtin::operator_by_oid)
        .ok_or_else(|| {
            Error::internal(format!(
                "missing operator {strategy}({}, {}) in partition opfamily {}",
                column.intype, column.intype, column.family
            ))
        })
}

/// The key column as the left argument of an operator of the key. A column whose type or collation is not the one of the operator class has a `RelabelType` to the input type of the class.
fn key_argument(column: &KeyColumn) -> Expr {
    let relabel = (column.expr.ty != column.intype
        && column.intype != oid::RECORD
        && !types::is_polymorphic(column.intype))
        || column.key_collation != column.collation;
    if !relabel {
        return column.expr.clone();
    }
    let kind = ExprKind::Relabel(Box::new(column.expr.clone()), CastForm::Explicit);
    Expr { kind, ty: column.intype, typmod: -1, location: None }
}

/// `make_partition_op_expr` with one value: `key op value`.
fn op_clause(column: &KeyColumn, strategy: i16, value: Expr) -> Result<Expr> {
    let op = operator(column, strategy)?;
    Ok(boolean(ExprKind::Func(Func {
        oid: op.code,
        args: vec![key_argument(column), value],
        form: FuncForm::Operator(op.oid),
        variadic: false,
        retset: false,
    })))
}

/// `get_range_nulltest`: `IS NOT NULL` of each key column.
fn not_null_tests(columns: &[KeyColumn]) -> Vec<Expr> {
    columns.iter().map(|c| boolean(ExprKind::NullTest(Box::new(c.expr.clone()), false))).collect()
}

/// `get_qual_for_hash`: the call of `satisfies_hash_partition` with the OID of the parent, the modulus, the remainder and the key columns.
fn hash_qual(parent: u32, columns: &[KeyColumn], modulus: i32, remainder: i32) -> Vec<Expr> {
    let value = |value: Value, ty: u32| Expr {
        kind: ExprKind::Const(value),
        ty,
        typmod: -1,
        location: None,
    };
    let mut args = vec![
        value(Value::Oid(parent), oid::OID),
        value(Value::Int4(modulus), oid::INT4),
        value(Value::Int4(remainder), oid::INT4),
    ];
    args.extend(columns.iter().map(|c| c.expr.clone()));
    vec![boolean(ExprKind::Func(Func {
        oid: SATISFIES_HASH_PARTITION,
        args,
        form: FuncForm::Call,
        variadic: false,
        retset: false,
    }))]
}

/// `get_qual_for_list`. The default partition takes the values that no other partition takes, in their order, and has no constraint when no other partition takes a value.
fn list_qual(
    catalog: &Catalog,
    parent: u32,
    column: &KeyColumn,
    values: Option<&[Option<String>]>,
) -> Result<Vec<Expr>> {
    let mut has_null = false;
    let mut elems: Vec<Expr> = Vec::new();
    let mut add = |value: &Option<String>, elems: &mut Vec<Expr>| -> Result<()> {
        match value {
            None => has_null = true,
            Some(text) => elems.push(node::read(text)?),
        }
        Ok(())
    };
    match values {
        Some(values) => {
            for value in values {
                add(value, &mut elems)?;
            }
        }
        None => {
            let mut all = Vec::new();
            for part in catalog.partitions(parent) {
                if let Some(of) = &part.partition_of
                    && let PartitionBound::List(values) = &of.bound
                {
                    for value in values {
                        add(value, &mut all)?;
                    }
                }
            }
            if all.is_empty() && !has_null {
                return Ok(Vec::new());
            }
            for e in all {
                let mut at = elems.len();
                while at > 0 && order(&elems[at - 1], &e)? == Ordering::Greater {
                    at -= 1;
                }
                elems.insert(at, e);
            }
        }
    }
    let opexpr = if elems.is_empty() {
        None
    } else if elems.len() > 1 && types::element(column.expr.ty) == 0 {
        let op = operator(column, EQUAL)?;
        let array = Expr {
            kind: ExprKind::Array { element: column.expr.ty, multidims: false, elements: elems },
            ty: types::array_of(column.expr.ty),
            typmod: -1,
            location: None,
        };
        Some(boolean(ExprKind::ScalarArrayOp {
            func: op.code,
            any: true,
            args: vec![key_argument(column), array],
        }))
    } else {
        let ops = elems.into_iter().map(|e| op_clause(column, EQUAL, e));
        Some(combine(BoolOp::Or, ops.collect::<Result<_>>()?))
    };
    let nulltest = boolean(ExprKind::NullTest(Box::new(column.expr.clone()), has_null));
    let result = match opexpr {
        Some(op) if has_null => vec![boolean(ExprKind::Bool(BoolOp::Or, vec![nulltest, op]))],
        Some(op) => vec![nulltest, op],
        None => vec![nulltest],
    };
    if values.is_some() {
        return Ok(result);
    }
    Ok(vec![boolean(ExprKind::Bool(BoolOp::Not, vec![combine(BoolOp::And, result)]))])
}

/// The constant of a datum of a range bound, or `None` for `MINVALUE` and `MAXVALUE`.
fn datum_value(datum: &RangeDatum) -> Result<Option<Expr>> {
    match datum {
        RangeDatum::Value(text) => Ok(Some(node::read(text)?)),
        RangeDatum::MinValue | RangeDatum::MaxValue => Ok(None),
    }
}

/// The order of the lower bounds of two range partitions, as `partition_rbound_cmp` gives it. A column with `MINVALUE` or `MAXVALUE` ends the comparison.
fn lower_order(a: &[RangeDatum], b: &[RangeDatum]) -> Result<Ordering> {
    let rank = |d: &RangeDatum| match d {
        RangeDatum::MinValue => -1,
        RangeDatum::Value(_) => 0,
        RangeDatum::MaxValue => 1,
    };
    for (x, y) in a.iter().zip(b) {
        let found = match (datum_value(x)?, datum_value(y)?) {
            (Some(x), Some(y)) => order(&x, &y)?,
            _ => return Ok(rank(x).cmp(&rank(y))),
        };
        if found != Ordering::Equal {
            return Ok(found);
        }
    }
    Ok(Ordering::Equal)
}

/// The `is_default` part of `get_qual_for_range`: the default partition takes the rows that no other partition takes, and has no constraint when there is no other partition.
fn range_default(catalog: &Catalog, parent: u32, columns: &[KeyColumn]) -> Result<Vec<Expr>> {
    let mut bounds: Vec<(&[RangeDatum], &[RangeDatum])> = Vec::new();
    for part in catalog.partitions(parent) {
        if let Some(of) = &part.partition_of
            && let PartitionBound::Range { lower, upper } = &of.bound
        {
            let mut at = bounds.len();
            while at > 0 && lower_order(bounds[at - 1].0, lower)? == Ordering::Greater {
                at -= 1;
            }
            bounds.insert(at, (lower, upper));
        }
    }
    let mut arms = Vec::with_capacity(bounds.len());
    for (lower, upper) in bounds {
        arms.push(combine(BoolOp::And, range_qual(columns, lower, upper, true)?));
    }
    if arms.is_empty() {
        return Ok(Vec::new());
    }
    let mut others = not_null_tests(columns);
    others.push(combine(BoolOp::Or, arms));
    let others = boolean(ExprKind::Bool(BoolOp::And, others));
    Ok(vec![boolean(ExprKind::Bool(BoolOp::Not, vec![others]))])
}

/// `get_qual_for_range` for a partition that is not the default partition. The key columns whose lower and upper datums are equal come first, each as `key = value`. Then an `OR` for the lower bound and an `OR` for the upper bound, with one arm for each of the other key columns. `for_default` leaves out the tests of `IS NOT NULL`, for the constraint of the default partition.
fn range_qual(
    columns: &[KeyColumn],
    lower: &[RangeDatum],
    upper: &[RangeDatum],
    for_default: bool,
) -> Result<Vec<Expr>> {
    let mut result = if for_default { Vec::new() } else { not_null_tests(columns) };
    let count = columns.len().min(lower.len()).min(upper.len());
    let mut i = 0;
    while i < count {
        let (Some(low), Some(high)) = (datum_value(&lower[i])?, datum_value(&upper[i])?) else {
            break;
        };
        if order(&low, &high)? != Ordering::Equal {
            break;
        }
        if i + 1 == count {
            return Err(Error::internal("invalid range bound specification"));
        }
        result.push(op_clause(&columns[i], EQUAL, low)?);
        i += 1;
    }
    let (mut lower_arms, mut upper_arms) = (Vec::new(), Vec::new());
    let (mut need_lower, mut need_upper) = (true, true);
    for current in 0..count - i {
        let (mut lower_args, mut upper_args) = (Vec::new(), Vec::new());
        for j in i..count {
            let (low, high) = (datum_value(&lower[j])?, datum_value(&upper[j])?);
            let (lower_next, upper_next) = (lower.get(j + 1), upper.get(j + 1));
            if need_lower && let Some(low) = &low {
                let strategy = if j - i < current {
                    EQUAL
                } else if j + 1 == count || matches!(lower_next, Some(RangeDatum::MinValue)) {
                    GREATER_EQUAL
                } else {
                    GREATER
                };
                lower_args.push(op_clause(&columns[j], strategy, low.clone())?);
            }
            if need_upper && let Some(high) = &high {
                let strategy = if j - i < current {
                    EQUAL
                } else if matches!(upper_next, Some(RangeDatum::MaxValue)) {
                    LESS_EQUAL
                } else {
                    LESS
                };
                upper_args.push(op_clause(&columns[j], strategy, high.clone())?);
            }
            if j + 1 - i > current {
                if low.is_none() || !matches!(lower_next, Some(RangeDatum::Value(_))) {
                    need_lower = false;
                }
                if high.is_none() || !matches!(upper_next, Some(RangeDatum::Value(_))) {
                    need_upper = false;
                }
                break;
            }
        }
        if !lower_args.is_empty() {
            lower_arms.push(combine(BoolOp::And, lower_args));
        }
        if !upper_args.is_empty() {
            upper_arms.push(combine(BoolOp::And, upper_args));
        }
        if !need_lower && !need_upper {
            break;
        }
    }
    if !lower_arms.is_empty() {
        result.push(combine(BoolOp::Or, lower_arms));
    }
    if !upper_arms.is_empty() {
        result.push(combine(BoolOp::Or, upper_arms));
    }
    if result.is_empty() {
        result = if for_default {
            not_null_tests(columns)
        } else {
            let truth = ExprKind::Const(Value::Bool(true));
            vec![boolean(truth)]
        };
    }
    Ok(result)
}

/// `get_qual_from_partbound`: the constraint that the bound of a partition gives, with the columns of the parent.
fn bound_qual(catalog: &Catalog, parent: &Relation, bound: &PartitionBound) -> Result<Vec<Expr>> {
    let key = parent
        .partition_key
        .as_ref()
        .ok_or_else(|| Error::internal(format!("relation {} is not partitioned", parent.oid)))?;
    let columns = key_columns(catalog, parent, key)?;
    let first = columns.first().ok_or_else(|| Error::internal("a partition key with no column"))?;
    match (key.strategy, bound) {
        (PartStrategy::Hash, PartitionBound::Hash { modulus, remainder }) => {
            Ok(hash_qual(parent.oid, &columns, *modulus, *remainder))
        }
        (PartStrategy::List, PartitionBound::List(values)) => {
            list_qual(catalog, parent.oid, first, Some(values))
        }
        (PartStrategy::List, PartitionBound::Default) => {
            list_qual(catalog, parent.oid, first, None)
        }
        (PartStrategy::Range, PartitionBound::Range { lower, upper }) => {
            range_qual(&columns, lower, upper, false)
        }
        (PartStrategy::Range, PartitionBound::Default) => {
            range_default(catalog, parent.oid, &columns)
        }
        _ => {
            Err(Error::internal(format!("unexpected partition strategy: {}", key.strategy.code())))
        }
    }
}

/// `generate_partition_qual`: the constraint of the partition and of each partitioned table above it, the top one first, with the columns of the partition. A table that is not a partition has no constraint.
pub(super) fn partition_qual(catalog: &Catalog, rel: &Relation) -> Result<Vec<Expr>> {
    let Some(of) = &rel.partition_of else { return Ok(Vec::new()) };
    let parent = catalog.relation(of.parent).ok_or_else(|| {
        Error::internal(format!("cache lookup failed for relation {}", of.parent))
    })?;
    let mut qual = partition_qual(catalog, parent)?;
    qual.extend(bound_qual(catalog, parent, &of.bound)?);
    // `map_partition_varattnos`: the columns of the parent by name in the partition.
    let map: Vec<i16> = parent
        .columns
        .iter()
        .map(|column| {
            let found = (!column.dropped)
                .then(|| rel.columns.iter().position(|c| !c.dropped && c.name == column.name))
                .flatten();
            found.map_or(0, |i| i16::try_from(i + 1).unwrap_or(i16::MAX))
        })
        .collect();
    for e in &mut qual {
        e.map_attnos(&map);
    }
    Ok(qual)
}
