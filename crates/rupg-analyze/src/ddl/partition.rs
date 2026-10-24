//! The partition key of `PARTITION BY` and the bound of `PARTITION OF`, as `ComputePartitionAttrs`, `transformPartitionBound` and `check_new_partition_bound` give them.

use std::cmp::Ordering;
use std::collections::BTreeSet;

use rupg_catalog::{ConKind, PartStrategy, PartitionBound, PartitionKey, RangeDatum};
use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{Node, PartitionBoundSpec, PartitionElem, PartitionSpec, PartitionStrategy};
use rupg_types::Value;

use super::index::{BTREE, HASH, system_column};
use super::partition_attach::without_locations;
use super::{Definer, Found, is_mutable, references, relname};
use crate::agg::Kind;
use crate::coerce::{AtOpt, Context};
use crate::expr::{Expr, ExprKind};
use crate::node;
use crate::typcache::default_opclass;
use crate::typename::{names, place};
use crate::types;

/// One column of a partition key, as `RelationGetPartitionKey` gives it.
struct KeyColumn {
    /// The name of the column, or the text of the expression.
    name: String,
    ty: u32,
    typmod: i32,
}

/// A datum of a bound of a range partition before `make_one_partition_rbound`: -1 for `MINVALUE`, 0 for a value and 1 for `MAXVALUE`.
struct NewDatum {
    kind: i8,
    /// The constant of a value, or a null for `MINVALUE` and `MAXVALUE`.
    value: Value,
    /// The stored form of the constant.
    text: Option<String>,
    location: Option<usize>,
}

/// `PartitionRangeBound`: one bound of a range partition.
#[derive(Clone)]
struct RangeBound {
    kinds: Vec<i8>,
    values: Vec<Value>,
    lower: bool,
    /// The partition of the bound, in the list of the partitions of the parent.
    part: usize,
}

/// An existing partition of the parent: its name and its bound.
struct Part {
    name: String,
    bound: PartitionBound,
}

/// The error of `ComputePartitionAttrs` for a generated column in a partition key, which the routing of a row cannot read, as it comes before the column is computed.
fn generated_key(column: &str, at: Option<usize>) -> Error {
    Error::new(SqlState::INVALID_OBJECT_DEFINITION, "cannot use generated column in partition key")
        .with_detail(format!("Column \"{column}\" is a generated column."))
        .at_opt(at)
}

/// The error of a bound that does not fit the strategy of the partition key.
fn invalid_bound(strategy: &str, at: Option<usize>) -> Error {
    Error::new(
        SqlState::INVALID_TABLE_DEFINITION,
        format!("invalid bound specification for a {strategy} partition"),
    )
    .at_opt(at)
}

/// The error of `check_new_partition_bound` for a bound that overlaps the bound of another partition.
fn overlap(relname: &str, with: &str, at: Option<usize>) -> Error {
    Error::new(
        SqlState::INVALID_OBJECT_DEFINITION,
        format!("partition \"{relname}\" would overlap partition \"{with}\""),
    )
    .at_opt(at)
}

/// The value of a stored constant.
fn stored_value(text: &str) -> Result<Value> {
    match node::read(text)?.kind {
        ExprKind::Const(value) => Ok(value),
        _ => Err(Error::internal("a partition bound that is not a constant")),
    }
}

/// `make_one_partition_rbound` for the bound of an existing partition.
fn stored_bound(datums: &[RangeDatum], lower: bool, part: usize) -> Result<RangeBound> {
    let mut kinds = Vec::with_capacity(datums.len());
    let mut values = Vec::with_capacity(datums.len());
    for datum in datums {
        let (kind, value) = match datum {
            RangeDatum::MinValue => (-1, Value::Null),
            RangeDatum::MaxValue => (1, Value::Null),
            RangeDatum::Value(text) => (0, stored_value(text)?),
        };
        kinds.push(kind);
        values.push(value);
    }
    Ok(RangeBound { kinds, values, lower, part })
}

/// `make_one_partition_rbound` for the bound of the new partition.
fn new_bound(datums: &[NewDatum], lower: bool) -> RangeBound {
    RangeBound {
        kinds: datums.iter().map(|d| d.kind).collect(),
        values: datums.iter().map(|d| d.value.clone()).collect(),
        lower,
        part: usize::MAX,
    }
}

impl Definer<'_, '_> {
    /// `transformPartitionSpec` and `ComputePartitionAttrs`: the partition key of the new table `relname`.
    pub(super) fn partition_key(
        &mut self,
        table: u32,
        relname: &str,
        spec: &PartitionSpec,
    ) -> Result<PartitionKey> {
        let strategy = match spec.strategy {
            PartitionStrategy::PARTITION_STRATEGY_LIST => PartStrategy::List,
            PartitionStrategy::PARTITION_STRATEGY_RANGE => PartStrategy::Range,
            PartitionStrategy::PARTITION_STRATEGY_HASH => PartStrategy::Hash,
            _ => return Err(Error::internal("unrecognized partitioning strategy")),
        };
        let elems: Vec<&PartitionElem> = spec
            .partParams
            .iter()
            .flatten()
            .filter_map(|n| match n {
                Node::PartitionElem(e) => Some(&**e),
                _ => None,
            })
            .collect();
        if strategy == PartStrategy::List && elems.len() != 1 {
            return Err(Error::new(
                SqlState::INVALID_OBJECT_DEFINITION,
                "cannot use \"list\" partition strategy with more than one column",
            ));
        }
        self.an.scope = crate::from::Scope::default();
        if let Some(rel) = self.catalog.relation(table) {
            let columns: Vec<(String, u32, i32)> =
                rel.columns.iter().map(crate::from::scope_column).collect();
            let row_type = rel.row_type;
            self.an.add_table(table, relname, &columns, row_type);
        }
        let mut transformed = Vec::with_capacity(elems.len());
        for elem in &elems {
            let expr = match &elem.expr {
                Some(raw) => Some(
                    self.an.with_kind(Kind::PartitionExpression, |an| an.transform(Some(raw)))?,
                ),
                None => None,
            };
            transformed.push(expr);
        }
        let method = if strategy == PartStrategy::Hash { HASH } else { BTREE };
        let mut partkey = PartitionKey {
            strategy,
            attrs: Vec::new(),
            classes: Vec::new(),
            collations: Vec::new(),
            exprs: None,
        };
        let mut exprs = Vec::new();
        for (elem, expr) in elems.iter().zip(transformed) {
            let at = place(elem.location);
            let (ty, mut collation) = match (&elem.name, expr) {
                (Some(name), _) => {
                    let rel = self.catalog.relation(table);
                    let column = rel.and_then(|r| r.column_number(name).map(|n| (n, r.column(n))));
                    match column {
                        Some((_, Some(c))) if c.generated != 0 => {
                            return Err(generated_key(name, at));
                        }
                        Some((n, Some(c))) => {
                            partkey.attrs.push(n);
                            (c.ty, c.collation)
                        }
                        _ if system_column(name).is_some() => {
                            return Err(Error::new(
                                SqlState::INVALID_OBJECT_DEFINITION,
                                format!("cannot use system column \"{name}\" in partition key"),
                            )
                            .at_opt(at));
                        }
                        _ => {
                            return Err(Error::new(
                                SqlState::UNDEFINED_COLUMN,
                                format!("column \"{name}\" named in partition key does not exist"),
                            )
                            .at_opt(at));
                        }
                    }
                }
                (None, Some(expr)) => {
                    let collation = self.expr_collation(table, elem.expr.as_ref(), &expr)?;
                    let system = expr.find(0, &mut |e, _| match &e.kind {
                        ExprKind::Var(var) if var.attnum < 0 => Some(()),
                        _ => None,
                    });
                    if system.is_some() {
                        return Err(Error::new(
                            SqlState::INVALID_OBJECT_DEFINITION,
                            "partition key expressions cannot contain system column references",
                        ));
                    }
                    // A whole-row reference reads all the columns of the table.
                    let rel = self.catalog.relation(table);
                    let mut read = BTreeSet::new();
                    expr.find(0, &mut |e, _| {
                        if let ExprKind::Var(var) = &e.kind {
                            read.insert(var.attnum);
                        }
                        None::<()>
                    });
                    if read.remove(&0) {
                        let width = rel.map_or(0, |r| r.columns.len());
                        read.extend((1..=width).map(|n| i16::try_from(n).unwrap_or(i16::MAX)));
                    }
                    let generated = read
                        .iter()
                        .find_map(|&n| rel.and_then(|r| r.column(n)).filter(|c| c.generated != 0));
                    if let Some(column) = generated {
                        return Err(generated_key(&column.name, at));
                    }
                    let ty = expr.ty;
                    match &expr.kind {
                        ExprKind::Var(var) if var.attnum > 0 => partkey.attrs.push(var.attnum),
                        _ => {
                            if is_mutable(self.catalog, &expr) {
                                return Err(Error::new(
                                    SqlState::INVALID_OBJECT_DEFINITION,
                                    "functions in partition key expression must be marked IMMUTABLE",
                                ));
                            }
                            // `expression_planner` folds an immutable expression with no column to a constant.
                            let column = expr.find(0, &mut |e, _| match &e.kind {
                                ExprKind::Var(_) => Some(()),
                                _ => None,
                            });
                            if column.is_none() {
                                return Err(Error::new(
                                    SqlState::INVALID_OBJECT_DEFINITION,
                                    "cannot use constant expression as partition key",
                                ));
                            }
                            partkey.attrs.push(0);
                            exprs.push(expr);
                        }
                    }
                    (ty, collation)
                }
                (None, None) => {
                    return Err(Error::internal("partition element has no name and no expression"));
                }
            };
            if !elem.collation.is_empty() {
                collation = self.an.collation_oid(&elem.collation, None)?;
            }
            if types::collatable(ty) {
                if collation == 0 {
                    return Err(Error::new(
                        SqlState::INDETERMINATE_COLLATION,
                        "could not determine which collation to use for partition expression",
                    )
                    .with_hint("Use the COLLATE clause to set the collation explicitly."));
                }
            } else if collation != 0 {
                return Err(Error::new(
                    SqlState::DATATYPE_MISMATCH,
                    format!("collations are not supported by type {}", types::name(ty)),
                ));
            }
            partkey.collations.push(collation);
            let class = if elem.opclass.is_empty() {
                let method_name = if method == HASH { "hash" } else { "btree" };
                default_opclass(ty, method).map(|c| c.oid).ok_or_else(|| {
                    Error::new(
                        SqlState::UNDEFINED_OBJECT,
                        format!(
                            "data type {} has no default operator class for access method \"{method_name}\"",
                            types::name(ty)
                        ),
                    )
                    .with_hint(format!(
                        "You must specify a {method_name} operator class or define a default {method_name} operator class for the data type."
                    ))
                })?
            } else {
                self.resolve_opclass(&elem.opclass, ty, method)?
            };
            partkey.classes.push(class);
        }
        if !exprs.is_empty() {
            partkey.exprs = Some(node::write_list(&exprs)?);
        }
        Ok(partkey)
    }

    /// The parent table of `PARTITION OF`, with the checks of `MergeAttributes` and of `DefineRelation`. `namespace` is the schema of the new partition.
    pub(super) fn partition_parent(&self, inh: &[Option<Node>], namespace: u32) -> Result<u32> {
        let Some(Node::RangeVar(rv)) = inh.iter().flatten().next() else {
            return Err(Error::internal("a partition with no parent"));
        };
        let found = self.lookup_relation(rv)?;
        let name = relname(rv);
        let kind = self.kind_of(found);
        // `table_openrv` refuses indexes and composite types first.
        if matches!(kind, b'i' | b'I' | b'c') {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("cannot open relation \"{name}\""),
            )
            .with_detail(super::relkind_detail(kind)));
        }
        if !matches!(kind, b'r' | b'f' | b'p') {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("inherited relation \"{name}\" is not a table or foreign table"),
            ));
        }
        let (parent, parent_namespace, owner) = match found {
            Found::User(oid) => {
                let rel = self.catalog.relation(oid);
                (oid, rel.map_or(0, |r| r.namespace), rel.map_or(0, |r| r.owner))
            }
            Found::Builtin(row) => (row.oid, row.namespace, row.owner),
        };
        let temp = self.catalog.is_temp_schema(namespace);
        if temp != self.catalog.is_temp_schema(parent_namespace) {
            let message = if temp {
                format!(
                    "cannot create a temporary relation as partition of permanent relation \"{name}\""
                )
            } else {
                format!(
                    "cannot create a permanent relation as partition of temporary relation \"{name}\""
                )
            };
            return Err(Error::new(SqlState::WRONG_OBJECT_TYPE, message));
        }
        if !self.is_superuser() && owner != self.user {
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("must be owner of table {name}"),
            ));
        }
        if kind != b'p' {
            return Err(Error::new(
                SqlState::INVALID_OBJECT_DEFINITION,
                format!("\"{name}\" is not partitioned"),
            ));
        }
        Ok(parent)
    }

    /// The check constraints that a table takes from its parents in `MergeAttributes`, from each parent in the order of their names, with the numbers of the columns of the table. Each keeps its name, and a constraint `NO INHERIT` stays with the parent. Two parents can give a constraint with one name only when the expressions are the same, and then it is one constraint that counts both.
    pub(super) fn inherit_checks(&mut self, table: u32, parents: &[u32]) -> Result<()> {
        // The name, the expression and the number of the parents that give it.
        let mut merged: Vec<(String, String, Vec<i16>, i16)> = Vec::new();
        for &parent in parents {
            let map = self.att_map(parent, table)?;
            let mut checks: Vec<(String, String, Vec<i16>)> = self
                .catalog
                .constraints_of(parent)
                .filter(|c| c.kind == ConKind::Check && !c.no_inherit)
                .map(|c| (c.name.clone(), c.expr.clone().unwrap_or_default(), c.keys.clone()))
                .collect();
            checks.sort_by(|a, b| a.0.cmp(&b.0));
            for (name, text, keys) in checks {
                let (text, keys) = (map.text(&text)?, map.columns(&keys));
                let Some(prev) = merged.iter_mut().find(|m| m.0 == name) else {
                    merged.push((name, text, keys, 1));
                    continue;
                };
                if without_locations(Some(&prev.1)) != without_locations(Some(&text)) {
                    return Err(Error::new(
                        SqlState::DUPLICATE_OBJECT,
                        format!(
                            "check constraint name \"{name}\" appears multiple times but with different expressions"
                        ),
                    ));
                }
                prev.3 = prev.3.saturating_add(1);
            }
        }
        for (name, text, keys, count) in merged {
            let expr = node::read(&text)?;
            let refs = references(self.catalog, table, &[&expr])?;
            let oid = self.catalog.add_check(table, Some(&name), text, keys, &refs, false)?;
            self.catalog.set_constraint_inherited(oid, false, count)?;
        }
        Ok(())
    }

    /// The part of `AddRelationNotNullConstraints` for the not-null constraints of the parents of a table. The constraints of the parents on one column are one constraint, with the name of the first parent and a count of all of them. A constraint of the table on the same column takes that count, and another column gets a constraint with the name of the constraint of the parent, or a new name when the table has that name.
    pub(super) fn inherit_not_nulls(&mut self, table: u32, parents: &[u32]) -> Result<()> {
        let mut inherited: Vec<(String, i16, i16)> = Vec::new();
        for &parent in parents {
            let map = self.att_map(parent, table)?;
            let columns: Vec<(String, i16)> = self
                .catalog
                .constraints_of(parent)
                .filter(|c| c.kind == ConKind::NotNull && !c.no_inherit)
                .filter_map(|c| Some((c.name.clone(), map.column(*c.keys.first()?))))
                .collect();
            for (name, attnum) in columns {
                match inherited.iter_mut().find(|i| i.1 == attnum) {
                    Some(prev) => prev.2 = prev.2.saturating_add(1),
                    None => inherited.push((name, attnum, 1)),
                }
            }
        }
        for (name, attnum, count) in inherited {
            let local = self
                .catalog
                .constraints_of(table)
                .find(|c| c.kind == ConKind::NotNull && c.keys.first() == Some(&attnum))
                .map(|c| (c.oid, c.no_inherit));
            if let Some((oid, no_inherit)) = local {
                if no_inherit {
                    let column = self
                        .catalog
                        .relation(table)
                        .and_then(|r| r.column(attnum))
                        .map(|c| c.name.clone())
                        .unwrap_or_default();
                    return Err(Error::new(
                        SqlState::DATATYPE_MISMATCH,
                        format!("cannot define not-null constraint with NO INHERIT on column \"{column}\""),
                    )
                    .with_detail("The column has an inherited not-null constraint."));
                }
                self.catalog.set_constraint_inherited(oid, true, count)?;
                continue;
            }
            let used = self.catalog.constraints_of(table).any(|c| c.name == name);
            let given = (!used).then_some(name.as_str());
            let oid = self.catalog.add_not_null(table, given, attnum, false)?;
            self.catalog.set_constraint_inherited(oid, false, count)?;
        }
        Ok(())
    }

    /// The columns of the partition key of the parent, with the names that the errors of a bound show.
    fn key_columns(&self, parent: u32, partkey: &PartitionKey) -> Result<Vec<KeyColumn>> {
        let rel = self
            .catalog
            .relation(parent)
            .ok_or_else(|| Error::internal(format!("cache lookup failed for relation {parent}")))?;
        let exprs = match &partkey.exprs {
            Some(text) => node::read_list(text)?,
            None => Vec::new(),
        };
        let mut exprs = exprs.into_iter();
        let mut columns = Vec::with_capacity(partkey.attrs.len());
        for &attnum in &partkey.attrs {
            if attnum != 0 {
                let c = rel.column(attnum).ok_or_else(|| {
                    Error::internal(format!("cache lookup failed for attribute {attnum}"))
                })?;
                columns.push(KeyColumn { name: c.name.clone(), ty: c.ty, typmod: c.typmod });
            } else {
                let expr =
                    exprs.next().ok_or_else(|| Error::internal("too few entries in partexprs"))?;
                let name = self.an.env.deparse(&node::write(&expr)?, parent)?;
                columns.push(KeyColumn { name, ty: expr.ty, typmod: expr.typmod });
            }
        }
        Ok(columns)
    }

    /// `transformPartitionBoundValue`: the constant of one value of a bound, of the type of the key column.
    fn bound_value(&mut self, raw: &Node, column: &KeyColumn) -> Result<Expr> {
        let scope = std::mem::take(&mut self.an.scope);
        let expr = self.an.with_kind(Kind::PartitionBound, |an| an.transform(Some(raw)));
        self.an.scope = scope;
        let expr = expr?;
        let at = expr.place();
        let Some(expr) =
            self.an.coerce_to_target(expr, column.ty, column.typmod, Context::Assignment, None)?
        else {
            return Err(Error::new(
                SqlState::DATATYPE_MISMATCH,
                format!(
                    "specified value cannot be cast to type {} for column \"{}\"",
                    types::name(column.ty),
                    column.name
                ),
            )
            .at_opt(at));
        };
        let value = match expr.kind {
            ExprKind::Const(value) => value,
            _ => self.an.env.evaluate(&expr)?,
        };
        Ok(Expr {
            kind: ExprKind::Const(value),
            ty: column.ty,
            typmod: column.typmod,
            location: at,
        })
    }

    /// `transformPartitionRangeBounds`: the datums of `FROM` or `TO`. A column reference `MINVALUE` or `MAXVALUE` is a datum of its own.
    fn range_datums(
        &mut self,
        list: &[Option<Node>],
        columns: &[KeyColumn],
    ) -> Result<Vec<NewDatum>> {
        let mut datums = Vec::with_capacity(list.len());
        for (raw, column) in list.iter().flatten().zip(columns) {
            if let Node::ColumnRef(c) = raw
                && let [name] = names(&c.fields).as_slice()
                && c.fields.len() == 1
                && let kind @ (-1 | 1) = match *name {
                    "minvalue" => -1,
                    "maxvalue" => 1,
                    _ => 0,
                }
            {
                let location = place(c.location);
                datums.push(NewDatum { kind, value: Value::Null, text: None, location });
                continue;
            }
            let expr = self.bound_value(raw, column)?;
            let ExprKind::Const(value) = &expr.kind else {
                return Err(Error::internal("could not evaluate partition bound expression"));
            };
            if value.is_null() {
                return Err(Error::new(
                    SqlState::INVALID_OBJECT_DEFINITION,
                    "cannot specify NULL in range bound",
                ));
            }
            let value = value.clone();
            let text = Some(node::write(&expr)?);
            datums.push(NewDatum { kind: 0, value, text, location: expr.location });
        }
        // `validateInfiniteBounds`.
        let mut kind = 0;
        for datum in &datums {
            if kind == datum.kind {
                continue;
            }
            let name = match kind {
                0 => {
                    kind = datum.kind;
                    continue;
                }
                1 => "MAXVALUE",
                _ => "MINVALUE",
            };
            return Err(Error::new(
                SqlState::DATATYPE_MISMATCH,
                format!("every bound following {name} must also be {name}"),
            )
            .at_opt(datum.location));
        }
        Ok(datums)
    }

    /// `transformPartitionBound` and `check_new_partition_bound`: the bound of the new partition `relname` of the parent. The bound must fit the strategy of the partition key and, when `check` is true, must not overlap the bound of another partition. `ATTACH PARTITION` transforms the bound before its other checks, and it checks the overlap after them.
    pub(super) fn partition_bound(
        &mut self,
        relname: &str,
        parent: u32,
        spec: &PartitionBoundSpec,
        check: bool,
    ) -> Result<PartitionBound> {
        let partkey =
            self.catalog.relation(parent).and_then(|r| r.partition_key.clone()).ok_or_else(
                || Error::internal(format!("relation {parent} has no partition key")),
            )?;
        let parts: Vec<Part> = self
            .catalog
            .partitions(parent)
            .into_iter()
            .filter_map(|r| {
                let bound = r.partition_of.as_ref()?.bound.clone();
                Some(Part { name: r.name.clone(), bound })
            })
            .collect();
        let at = place(spec.location);
        let columns = self.key_columns(parent, &partkey)?;
        let strategy = char::from(spec.strategy);
        if spec.is_default {
            if partkey.strategy == PartStrategy::Hash {
                return Err(Error::new(
                    SqlState::INVALID_TABLE_DEFINITION,
                    "a hash-partitioned table may not have a default partition",
                ));
            }
            if let Some(found) = parts.iter().find(|p| p.bound == PartitionBound::Default)
                && check
            {
                return Err(Error::new(
                    SqlState::INVALID_OBJECT_DEFINITION,
                    format!(
                        "partition \"{relname}\" conflicts with existing default partition \"{}\"",
                        found.name
                    ),
                )
                .at_opt(at));
            }
            return Ok(PartitionBound::Default);
        }
        match partkey.strategy {
            PartStrategy::Hash => {
                if strategy != 'h' {
                    return Err(invalid_bound("hash", at));
                }
                if spec.modulus <= 0 {
                    return Err(Error::new(
                        SqlState::INVALID_TABLE_DEFINITION,
                        "modulus for hash partition must be an integer value greater than zero",
                    ));
                }
                if spec.remainder >= spec.modulus {
                    return Err(Error::new(
                        SqlState::INVALID_TABLE_DEFINITION,
                        "remainder for hash partition must be less than modulus",
                    ));
                }
                if check {
                    check_hash(relname, &parts, spec.modulus, spec.remainder, at)?;
                }
                Ok(PartitionBound::Hash { modulus: spec.modulus, remainder: spec.remainder })
            }
            PartStrategy::List => {
                if strategy != 'l' {
                    return Err(invalid_bound("list", at));
                }
                let mut values: Vec<Expr> = Vec::new();
                for raw in spec.listdatums.iter().flatten() {
                    let value = self.bound_value(raw, &columns[0])?;
                    if !values.iter().any(|v| v.kind == value.kind) {
                        values.push(value);
                    }
                }
                if check {
                    self.check_list(relname, &parts, &values, columns[0].ty)?;
                }
                let mut stored = Vec::with_capacity(values.len());
                for value in &values {
                    let null = matches!(value.kind, ExprKind::Const(Value::Null));
                    stored.push(if null { None } else { Some(node::write(value)?) });
                }
                Ok(PartitionBound::List(stored))
            }
            PartStrategy::Range => {
                if strategy != 'r' {
                    return Err(invalid_bound("range", at));
                }
                for (list, clause) in [(&spec.lowerdatums, "FROM"), (&spec.upperdatums, "TO")] {
                    if list.len() != columns.len() {
                        return Err(Error::new(
                            SqlState::INVALID_TABLE_DEFINITION,
                            format!(
                                "{clause} must specify exactly one value per partitioning column"
                            ),
                        ));
                    }
                }
                let lower = self.range_datums(&spec.lowerdatums, &columns)?;
                let upper = self.range_datums(&spec.upperdatums, &columns)?;
                let types: Vec<u32> = columns.iter().map(|c| c.ty).collect();
                if check {
                    self.check_range(relname, &parts, &types, (&lower, &upper))?;
                }
                let stored = |datums: Vec<NewDatum>| -> Vec<RangeDatum> {
                    datums
                        .into_iter()
                        .map(|d| match (d.kind, d.text) {
                            (-1, _) => RangeDatum::MinValue,
                            (1, _) => RangeDatum::MaxValue,
                            (_, text) => RangeDatum::Value(text.unwrap_or_default()),
                        })
                        .collect()
                };
                Ok(PartitionBound::Range { lower: stored(lower), upper: stored(upper) })
            }
        }
    }

    /// The list part of `check_new_partition_bound`: a value that another partition has, or a null when another partition takes nulls, overlaps.
    fn check_list(&self, relname: &str, parts: &[Part], values: &[Expr], ty: u32) -> Result<()> {
        for value in values {
            let ExprKind::Const(value_of) = &value.kind else { continue };
            for part in parts {
                let PartitionBound::List(existing) = &part.bound else { continue };
                for other in existing {
                    let found = match other {
                        None => value_of.is_null(),
                        Some(_) if value_of.is_null() => false,
                        Some(text) => {
                            let other = stored_value(text)?;
                            self.an.env.compare(ty, value_of, &other)? == Ordering::Equal
                        }
                    };
                    if found {
                        return Err(overlap(relname, &part.name, value.location));
                    }
                }
            }
        }
        Ok(())
    }

    /// `partition_rbound_cmp`: the order of two bounds of a range partition, as a column number with a sign. A bound that has `MINVALUE` or `MAXVALUE` in a column ignores the later columns, and of two bounds with equal datums the upper bound comes first.
    fn rbound_cmp(&self, types: &[u32], a: &RangeBound, b: &RangeBound) -> Result<i32> {
        let mut colnum = 0;
        let mut order = Ordering::Equal;
        for (i, &ty) in types.iter().enumerate() {
            colnum += 1;
            match a.kinds[i].cmp(&b.kinds[i]) {
                Ordering::Less => return Ok(-colnum),
                Ordering::Greater => return Ok(colnum),
                Ordering::Equal if a.kinds[i] != 0 => break,
                Ordering::Equal => {}
            }
            order = self.an.env.compare(ty, &a.values[i], &b.values[i])?;
            if order != Ordering::Equal {
                break;
            }
        }
        if order == Ordering::Equal && a.lower != b.lower {
            order = if a.lower { Ordering::Greater } else { Ordering::Less };
        }
        Ok(match order {
            Ordering::Equal => 0,
            Ordering::Less => -colnum,
            Ordering::Greater => colnum,
        })
    }

    /// A merge sort of the bounds with `qsort_partition_rbound_cmp`. The comparison can fail, so the sort of the standard library does not fit.
    fn sort_bounds(&self, types: &[u32], mut all: Vec<RangeBound>) -> Result<Vec<RangeBound>> {
        if all.len() <= 1 {
            return Ok(all);
        }
        let right = all.split_off(all.len() / 2);
        let left = self.sort_bounds(types, all)?;
        let right = self.sort_bounds(types, right)?;
        let mut out = Vec::with_capacity(left.len() + right.len());
        let (mut left, mut right) = (left.into_iter().peekable(), right.into_iter().peekable());
        while let (Some(l), Some(r)) = (left.peek(), right.peek()) {
            let next = if self.rbound_cmp(types, l, r)? <= 0 { left.next() } else { right.next() };
            out.extend(next);
        }
        out.extend(left);
        out.extend(right);
        Ok(out)
    }

    /// `get_range_partbound_string`: the text of the datums of a bound, as the detail of an error shows them.
    fn range_text(&self, datums: &[NewDatum]) -> Result<String> {
        let mut parts = Vec::with_capacity(datums.len());
        for datum in datums {
            parts.push(match (datum.kind, &datum.text) {
                (-1, _) => "MINVALUE".to_string(),
                (1, _) => "MAXVALUE".to_string(),
                (_, Some(text)) => self.an.env.deparse_constant(text)?,
                (_, None) => String::new(),
            });
        }
        Ok(format!("({})", parts.join(", ")))
    }

    /// The range part of `check_new_partition_bound`: the lower bound must be below the upper bound, and the range must not overlap the range of another partition. The bounds of the partitions go in order with `create_range_bounds`, and `partition_range_bsearch` finds the place of the new lower bound.
    fn check_range(
        &self,
        relname: &str,
        parts: &[Part],
        types: &[u32],
        (lower, upper): (&[NewDatum], &[NewDatum]),
    ) -> Result<()> {
        let low = new_bound(lower, true);
        let high = new_bound(upper, false);
        let mut cmpval = self.rbound_cmp(types, &low, &high)?;
        if cmpval > 0 {
            let at = usize::try_from(cmpval - 1).ok().and_then(|i| lower.get(i)?.location);
            return Err(Error::new(
                SqlState::INVALID_OBJECT_DEFINITION,
                format!("empty range bound specified for partition \"{relname}\""),
            )
            .with_detail(format!(
                "Specified lower bound {} is greater than or equal to upper bound {}.",
                self.range_text(lower)?,
                self.range_text(upper)?
            ))
            .at_opt(at));
        }
        if parts.is_empty() {
            return Ok(());
        }
        let mut all = Vec::new();
        for (i, part) in parts.iter().enumerate() {
            if let PartitionBound::Range { lower, upper } = &part.bound {
                all.push(stored_bound(lower, true, i)?);
                all.push(stored_bound(upper, false, i)?);
            }
        }
        let all = self.sort_bounds(types, all)?;
        // The distinct bounds: of two equal bounds, the first one stays, which is the upper bound of the partition before.
        let mut bounds: Vec<RangeBound> = Vec::with_capacity(all.len());
        for cur in all {
            let same = bounds.last().is_some_and(|prev| {
                for (j, &kind) in cur.kinds.iter().enumerate() {
                    if kind != prev.kinds[j] {
                        return false;
                    }
                    if kind != 0 {
                        return true;
                    }
                    if self.an.env.compare(types[j], &cur.values[j], &prev.values[j]).ok()
                        != Some(Ordering::Equal)
                    {
                        return false;
                    }
                }
                true
            });
            if !same {
                bounds.push(cur);
            }
        }
        // `indexes`: the partition of each upper bound, `None` for a lower bound, and one more `None` at the end.
        let mut indexes: Vec<Option<usize>> =
            bounds.iter().map(|b| (!b.lower).then_some(b.part)).collect();
        indexes.push(None);
        let (mut lo, mut hi) = (-1_isize, bounds.len().cast_signed() - 1);
        while lo < hi {
            let mid = (lo + hi + 1) / 2;
            let at = mid.cast_unsigned();
            cmpval = self.rbound_cmp(types, &bounds[at], &low)?;
            if cmpval <= 0 {
                lo = mid;
                if cmpval == 0 {
                    break;
                }
            } else {
                hi = mid - 1;
            }
        }
        let next = (lo + 1).cast_unsigned();
        let datum_at = |datums: &[NewDatum], cmpval: i32| {
            let i = usize::try_from(cmpval.unsigned_abs()).unwrap_or(1).saturating_sub(1);
            datums.get(i).and_then(|d| d.location)
        };
        let (with, at) = if indexes[next].is_none() {
            if next >= bounds.len() {
                return Ok(());
            }
            let cmpval = self.rbound_cmp(types, &bounds[next], &high)?;
            if cmpval >= 0 {
                return Ok(());
            }
            (indexes[next + 1], datum_at(upper, cmpval))
        } else {
            let at = if cmpval == 0 {
                lower.first().and_then(|d| d.location)
            } else {
                datum_at(lower, cmpval)
            };
            (indexes[next], at)
        };
        let with = with.ok_or_else(|| Error::internal("an overlap with no partition"))?;
        Err(overlap(relname, &parts[with].name, at))
    }
}

/// The hash part of `check_new_partition_bound`. The moduli of the partitions must each be a factor of the next larger one, and no partition can take a remainder of the new one.
fn check_hash(
    relname: &str,
    parts: &[Part],
    modulus: i32,
    remainder: i32,
    at: Option<usize>,
) -> Result<()> {
    let mut bounds: Vec<(i32, i32, &str)> = parts
        .iter()
        .filter_map(|p| match p.bound {
            PartitionBound::Hash { modulus, remainder } => {
                Some((modulus, remainder, p.name.as_str()))
            }
            _ => None,
        })
        .collect();
    if bounds.is_empty() {
        return Ok(());
    }
    bounds.sort_unstable();
    let factor = |next: (i32, i32, &str)| {
        Error::new(
            SqlState::INVALID_OBJECT_DEFINITION,
            "every hash partition modulus must be a factor of the next larger modulus",
        )
        .with_detail(format!(
            "The new modulus {modulus} is not a factor of {}, the modulus of existing partition \"{}\".",
            next.0, next.2
        ))
    };
    // `partition_hash_bsearch`: the last bound that is not above the new one.
    match bounds.iter().rposition(|&(m, r, _)| (m, r) <= (modulus, remainder)) {
        None => {
            if bounds[0].0 % modulus != 0 {
                return Err(factor(bounds[0]));
            }
        }
        Some(offset) => {
            let prev = bounds[offset];
            if modulus % prev.0 != 0 {
                return Err(Error::new(
                    SqlState::INVALID_OBJECT_DEFINITION,
                    "every hash partition modulus must be a factor of the next larger modulus",
                )
                .with_detail(format!(
                    "The new modulus {modulus} is not divisible by {}, the modulus of existing partition \"{}\".",
                    prev.0, prev.2
                )));
            }
            if let Some(&next) = bounds.get(offset + 1)
                && next.0 % modulus != 0
            {
                return Err(factor(next));
            }
        }
    }
    let greatest = bounds.last().map_or(0, |b| b.0);
    let mut indexes: Vec<Option<&str>> = vec![None; usize::try_from(greatest).unwrap_or(0)];
    for &(m, r, name) in &bounds {
        let mut r = r;
        while r < greatest {
            indexes[usize::try_from(r).unwrap_or(0)] = Some(name);
            r += m;
        }
    }
    let mut r = if remainder >= greatest { remainder % greatest } else { remainder };
    while r < greatest {
        if let Some(name) = indexes[usize::try_from(r).unwrap_or(0)] {
            return Err(overlap(relname, name, at));
        }
        r += modulus;
    }
    Ok(())
}
