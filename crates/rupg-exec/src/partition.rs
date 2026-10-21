//! Partitioned tables: the tuple routing of `ExecFindPartition` and `get_partition_for_tuple` of `execPartition.c`, the partition check of `ExecPartitionCheck`, and the order of the partitions that `RelationBuildPartitionDesc` gives, which is the order of a scan.

use std::cmp::Ordering;

use rupg_analyze::{Expr, ExprKind, node};
use rupg_catalog::{PartStrategy, PartitionBound, RangeDatum, RelKind, Relation};
use rupg_common::{Error, Result, SqlState};
use rupg_func::Session;
use rupg_types::Value;

/// The most bytes of a value in the detail of an error, as `ExecBuildSlotPartitionKeyDescription` cuts it.
const MAX_FIELD_LEN: usize = 64;

/// One datum of a range bound, as `PartitionRangeDatumKind` and its value.
#[derive(Debug)]
enum Datum {
    MinValue,
    MaxValue,
    Value(Value),
}

/// The bound of a partition with its constants.
#[derive(Debug)]
enum Bound {
    Default,
    Hash { modulus: u64, remainder: u64 },
    List(Vec<Option<Value>>),
    Range { lower: Vec<Datum>, upper: Vec<Datum> },
}

/// A partition of a partitioned table.
#[derive(Debug)]
struct Part {
    oid: u32,
    bound: Bound,
    /// For each column of the partition, the column of the parent with the same name, as `build_attrmap_by_name` gives it. A dropped column has none.
    map: Vec<Option<usize>>,
}

/// A partitioned table, as a `PartitionDispatch`: its partition key and its partitions in the order of `RelationBuildPartitionDesc`.
#[derive(Debug)]
struct Dispatch {
    oid: u32,
    name: String,
    strategy: PartStrategy,
    /// The column of each key column, or `None` for an expression.
    attrs: Vec<Option<usize>>,
    exprs: Vec<Expr>,
    /// The type of each key column.
    types: Vec<u32>,
    /// The types of the columns of the table.
    columns: Vec<u32>,
    parts: Vec<Part>,
}

/// The partitioned tables that a statement routes rows into, read from the catalog at the first row that needs each one.
#[derive(Debug, Default)]
pub struct Router {
    dispatch: Vec<Dispatch>,
}

impl Router {
    /// `ExecFindPartition`: the leaf partition of a row of the partitioned table `root`, and the row with the columns of the partition. A root that is itself a partition first checks its partition constraint.
    ///
    /// # Errors
    ///
    /// `23514` when no partition takes the row or the root does not take it, and the errors of the expressions of the keys.
    pub fn route(
        &mut self,
        root: u32,
        row: Vec<Value>,
        session: &dyn Session,
    ) -> Result<(u32, Vec<Value>)> {
        let catalog = catalog(session)?;
        let rel = relation(catalog, root)?;
        if rel.partition_of.is_some() && !self.check(root, &row, session)? {
            return Err(partition_check_error(rel, &row, session)?);
        }
        let (mut oid, mut row) = (root, row);
        while relation(catalog, oid)?.kind == RelKind::Partitioned {
            let at = self.dispatch(oid, session)?;
            let d = &self.dispatch[at];
            let values = d.values(&row, session)?;
            let Some(part) = d.find(&values)? else {
                return Err(no_partition(d, relation(catalog, oid)?, &values, session)?);
            };
            row = part
                .map
                .iter()
                .map(|i| i.and_then(|i| row.get(i).cloned()).unwrap_or(Value::Null))
                .collect();
            oid = part.oid;
        }
        Ok((oid, row))
    }

    /// `ExecPartitionCheck`: true when a row of the partition `table` is in the bounds of the partition, and of each partition above it.
    ///
    /// # Errors
    ///
    /// The errors of the expressions of the keys.
    pub fn check(&mut self, table: u32, row: &[Value], session: &dyn Session) -> Result<bool> {
        let catalog = catalog(session)?;
        let (mut child, mut row) = (table, row.to_vec());
        while let Some(of) = &relation(catalog, child)?.partition_of {
            let at = self.dispatch(of.parent, session)?;
            let d = &self.dispatch[at];
            let Some(part) = d.parts.iter().find(|p| p.oid == child) else {
                return Err(Error::internal(format!("relation {child} is not a partition")));
            };
            // The row with the columns of the parent.
            let mut up = vec![Value::Null; d.columns.len()];
            for (value, &to) in row.iter().zip(&part.map) {
                if let Some(to) = to {
                    up[to] = value.clone();
                }
            }
            let values = d.values(&up, session)?;
            if d.find(&values)?.map(|p| p.oid) != Some(child) {
                return Ok(false);
            }
            (child, row) = (of.parent, up);
        }
        Ok(true)
    }

    /// `ExecPartitionCheck` with `ExecPartitionCheckEmitError`: an error when a row of the partition `table` is not in the bounds of the partition, or of a partition above it.
    ///
    /// # Errors
    ///
    /// `23514` for a row that the partition does not take, and the errors of the expressions of the keys.
    pub fn check_row(&mut self, table: u32, row: &[Value], session: &dyn Session) -> Result<()> {
        if self.check(table, row, session)? {
            return Ok(());
        }
        Err(partition_check_error(relation(catalog(session)?, table)?, row, session)?)
    }

    /// The index of the dispatch of the partitioned table `oid`.
    fn dispatch(&mut self, oid: u32, session: &dyn Session) -> Result<usize> {
        if let Some(at) = self.dispatch.iter().position(|d| d.oid == oid) {
            return Ok(at);
        }
        let d = Dispatch::new(relation(catalog(session)?, oid)?, session)?;
        self.dispatch.push(d);
        Ok(self.dispatch.len() - 1)
    }
}

impl Dispatch {
    /// `RelationGetPartitionKey` and `RelationGetPartitionDesc` of a partitioned table.
    fn new(rel: &Relation, session: &dyn Session) -> Result<Dispatch> {
        let catalog = catalog(session)?;
        let Some(key) = &rel.partition_key else {
            return Err(Error::internal(format!("relation {} is not partitioned", rel.oid)));
        };
        let mut exprs = match &key.exprs {
            Some(text) => node::read_list(text)?,
            None => Vec::new(),
        }
        .into_iter();
        let (mut attrs, mut types, mut key_exprs) = (Vec::new(), Vec::new(), Vec::new());
        for &attnum in &key.attrs {
            if attnum == 0 {
                let expr =
                    exprs.next().ok_or_else(|| Error::internal("too few entries in partexprs"))?;
                types.push(expr.ty);
                attrs.push(None);
                key_exprs.push(expr);
            } else {
                let column = rel.column(attnum).ok_or_else(|| {
                    Error::internal(format!("cache lookup failed for attribute {attnum}"))
                })?;
                types.push(column.ty);
                attrs.push(usize::try_from(attnum - 1).ok());
            }
        }
        let mut parts = Vec::new();
        for child in catalog.partitions(rel.oid) {
            let Some(of) = &child.partition_of else { continue };
            let map = child
                .columns
                .iter()
                .map(|c| {
                    if c.dropped {
                        return None;
                    }
                    rel.columns.iter().position(|p| !p.dropped && p.name == c.name)
                })
                .collect();
            parts.push(Part { oid: child.oid, bound: bound(&of.bound)?, map });
        }
        let mut d = Dispatch {
            oid: rel.oid,
            name: rel.name.clone(),
            strategy: key.strategy,
            attrs,
            exprs: key_exprs,
            types,
            columns: rel.columns.iter().map(|c| c.ty).collect(),
            parts,
        };
        d.sort()?;
        Ok(d)
    }

    /// `FormPartitionKeyDatum`: the values of the key columns of a row of the table.
    fn values(&self, row: &[Value], session: &dyn Session) -> Result<Vec<Value>> {
        let computed = if self.exprs.is_empty() {
            Vec::new()
        } else {
            crate::evaluate_row(&self.exprs, self.oid, &self.columns, row, session)?
        };
        let mut computed = computed.into_iter();
        let mut values = Vec::with_capacity(self.attrs.len());
        for attr in &self.attrs {
            values.push(match attr {
                Some(i) => row.get(*i).cloned().unwrap_or(Value::Null),
                None => computed.next().unwrap_or(Value::Null),
            });
        }
        Ok(values)
    }

    /// `get_partition_for_tuple`: the partition whose bound takes the values of the key, or else the default partition.
    fn find(&self, values: &[Value]) -> Result<Option<&Part>> {
        let mut found = None;
        match self.strategy {
            PartStrategy::Hash => {
                let typed: Vec<(u32, Value)> =
                    self.types.iter().copied().zip(values.iter().cloned()).collect();
                let hash = rupg_func::partition_hash(&typed)?;
                found = self.parts.iter().find(|p| {
                    matches!(p.bound, Bound::Hash { modulus, remainder } if hash % modulus == remainder)
                });
            }
            PartStrategy::List => {
                let ty = self.types.first().copied().unwrap_or(0);
                let value = values.first().unwrap_or(&Value::Null);
                for part in &self.parts {
                    let Bound::List(datums) = &part.bound else { continue };
                    for datum in datums {
                        let hit = match (datum, value) {
                            (None, Value::Null) => true,
                            (None, _) | (Some(_), Value::Null) => false,
                            (Some(datum), value) => {
                                rupg_func::compare(ty, datum, ty, value)? == Ordering::Equal
                            }
                        };
                        if hit {
                            found = Some(part);
                        }
                    }
                    if found.is_some() {
                        break;
                    }
                }
            }
            PartStrategy::Range if values.iter().any(Value::is_null) => {}
            PartStrategy::Range => {
                for part in &self.parts {
                    let Bound::Range { lower, upper } = &part.bound else { continue };
                    if self.datum_cmp(lower, values)? != Ordering::Greater
                        && self.datum_cmp(upper, values)? == Ordering::Greater
                    {
                        found = Some(part);
                        break;
                    }
                }
            }
        }
        Ok(found.or_else(|| self.parts.iter().find(|p| matches!(p.bound, Bound::Default))))
    }

    /// `partition_rbound_datum_cmp`: the order of a range bound against the values of a key.
    fn datum_cmp(&self, datums: &[Datum], values: &[Value]) -> Result<Ordering> {
        for ((datum, value), &ty) in datums.iter().zip(values).zip(&self.types) {
            let order = match datum {
                Datum::MinValue => Ordering::Less,
                Datum::MaxValue => Ordering::Greater,
                Datum::Value(datum) => rupg_func::compare(ty, datum, ty, value)?,
            };
            if order != Ordering::Equal {
                return Ok(order);
            }
        }
        Ok(Ordering::Equal)
    }

    /// `partition_rbound_cmp` of two lower bounds: a lower bound that ends at an infinite datum sorts by its kind.
    fn bound_cmp(&self, a: &[Datum], b: &[Datum]) -> Result<Ordering> {
        for ((a, b), &ty) in a.iter().zip(b).zip(&self.types) {
            let order = match (a, b) {
                (Datum::Value(a), Datum::Value(b)) => rupg_func::compare(ty, a, ty, b)?,
                (a, b) => rank(a).cmp(&rank(b)),
            };
            if order != Ordering::Equal {
                return Ok(order);
            }
            if !matches!(a, Datum::Value(_)) {
                break;
            }
        }
        Ok(Ordering::Equal)
    }

    /// The canonical order of `partition_bounds_create`: hash partitions by modulus and remainder, list partitions by their least value with the partition of the null value after them, and range partitions by their lower bound. The default partition comes last.
    fn sort(&mut self) -> Result<()> {
        let parts = std::mem::take(&mut self.parts);
        let mut keys: Vec<(usize, Option<Value>)> = Vec::with_capacity(parts.len());
        // The first error of a compare, as the sort cannot stop.
        let mut failed = None;
        let ty = self.types.first().copied().unwrap_or(0);
        for (i, part) in parts.iter().enumerate() {
            let least = match &part.bound {
                Bound::List(datums) => {
                    let mut least: Option<&Value> = None;
                    for datum in datums.iter().flatten() {
                        if least.is_none_or(|l| {
                            rupg_func::compare(ty, datum, ty, l).is_ok_and(|o| o == Ordering::Less)
                        }) {
                            least = Some(datum);
                        }
                    }
                    least.cloned()
                }
                _ => None,
            };
            keys.push((i, least));
        }
        let order = |a: &(usize, Option<Value>), b: &(usize, Option<Value>)| -> Ordering {
            let (pa, pb) = (&parts[a.0], &parts[b.0]);
            let (da, db) = (matches!(pa.bound, Bound::Default), matches!(pb.bound, Bound::Default));
            if da || db {
                return da.cmp(&db);
            }
            let result = match (&pa.bound, &pb.bound) {
                (
                    Bound::Hash { modulus: ma, remainder: ra },
                    Bound::Hash { modulus: mb, remainder: rb },
                ) => Ok((ma, ra).cmp(&(mb, rb))),
                (Bound::Range { lower: la, .. }, Bound::Range { lower: lb, .. }) => {
                    self.bound_cmp(la, lb)
                }
                _ => match (&a.1, &b.1) {
                    (Some(x), Some(y)) => rupg_func::compare(ty, x, ty, y),
                    (x, y) => Ok(x.is_none().cmp(&y.is_none())),
                },
            };
            result.unwrap_or_else(|error| {
                failed.get_or_insert(error);
                Ordering::Equal
            })
        };
        keys.sort_by(order);
        if let Some(error) = failed {
            return Err(error);
        }
        let mut slots: Vec<Option<Part>> = parts.into_iter().map(Some).collect();
        self.parts = keys.iter().filter_map(|(i, _)| slots[*i].take()).collect();
        Ok(())
    }
}

/// The order of the kinds of range datums: `MINVALUE`, a value, then `MAXVALUE`.
fn rank(datum: &Datum) -> i8 {
    match datum {
        Datum::MinValue => -1,
        Datum::Value(_) => 0,
        Datum::MaxValue => 1,
    }
}

/// The leaf partitions of the partitioned table `oid` in the order of a scan, as `expand_partitioned_rtentry` adds them: the partitions in the order of their bounds, and the partitions of a partitioned partition in its place.
///
/// # Errors
///
/// The errors of the compare of the bounds.
pub fn leaves(oid: u32, session: &dyn Session) -> Result<Vec<u32>> {
    let catalog = catalog(session)?;
    let d = Dispatch::new(relation(catalog, oid)?, session)?;
    let mut out = Vec::new();
    for part in &d.parts {
        if relation(catalog, part.oid)?.kind == RelKind::Partitioned {
            out.extend(leaves(part.oid, session)?);
        } else {
            out.push(part.oid);
        }
    }
    Ok(out)
}

/// The constants of the bound of a partition.
fn bound(bound: &PartitionBound) -> Result<Bound> {
    let datums = |list: &[RangeDatum]| -> Result<Vec<Datum>> {
        list.iter()
            .map(|d| {
                Ok(match d {
                    RangeDatum::MinValue => Datum::MinValue,
                    RangeDatum::MaxValue => Datum::MaxValue,
                    RangeDatum::Value(text) => Datum::Value(constant(text)?),
                })
            })
            .collect()
    };
    Ok(match bound {
        PartitionBound::Default => Bound::Default,
        PartitionBound::Hash { modulus, remainder } => Bound::Hash {
            modulus: u64::try_from(*modulus).unwrap_or(1).max(1),
            remainder: u64::try_from(*remainder).unwrap_or(0),
        },
        PartitionBound::List(values) => Bound::List(
            values.iter().map(|v| v.as_deref().map(constant).transpose()).collect::<Result<_>>()?,
        ),
        PartitionBound::Range { lower, upper } => {
            Bound::Range { lower: datums(lower)?, upper: datums(upper)? }
        }
    })
}

/// The value of a constant of a bound in its stored form.
fn constant(text: &str) -> Result<Value> {
    match node::read(text)?.kind {
        ExprKind::Const(value) => Ok(value),
        _ => Err(Error::internal("a partition bound that is not a constant")),
    }
}

/// The catalog of the user.
fn catalog(session: &dyn Session) -> Result<&rupg_catalog::Catalog> {
    session.catalog().ok_or_else(|| Error::internal("a partitioned table with no catalog"))
}

/// A relation of the catalog.
fn relation(catalog: &rupg_catalog::Catalog, oid: u32) -> Result<&Relation> {
    catalog
        .relation(oid)
        .ok_or_else(|| Error::internal(format!("cache lookup failed for relation {oid}")))
}

/// The error of `ExecFindPartition` for a row that no partition takes, with the detail of `ExecBuildSlotPartitionKeyDescription`.
fn no_partition(
    d: &Dispatch,
    rel: &Relation,
    values: &[Value],
    session: &dyn Session,
) -> Result<Error> {
    let mut shown = Vec::with_capacity(values.len());
    for (value, &ty) in values.iter().zip(&d.types) {
        shown.push(crate::insert::shown(ty, value, MAX_FIELD_LEN, session)?);
    }
    let columns = rupg_func::partition_key_columns(rel, session)?;
    Ok(Error::new(
        SqlState::CHECK_VIOLATION,
        format!("no partition of relation \"{}\" found for row", d.name),
    )
    .with_detail(format!(
        "Partition key of the failing row contains ({columns}) = ({}).",
        shown.join(", ")
    )))
}

/// `ExecPartitionCheckEmitError`: the error for a row that is not in the bounds of the partition `rel`.
pub(crate) fn partition_check_error(
    rel: &Relation,
    row: &[Value],
    session: &dyn Session,
) -> Result<Error> {
    let types: Vec<u32> = rel.columns.iter().map(|c| c.ty).collect();
    Ok(Error::new(
        SqlState::CHECK_VIOLATION,
        format!("new row for relation \"{}\" violates partition constraint", rel.name),
    )
    .with_detail(crate::insert::failing_row(row, &types, session)?))
}
