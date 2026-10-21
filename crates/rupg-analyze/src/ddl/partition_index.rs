//! The indexes of partitioned tables, as `DefineIndex` makes them: the index of a partitioned table has an index in each partition, and a new partition gets an index for each index of its parent table. A partition has the columns of its parent table in the same order, so the column numbers and the expressions of the two indexes are the same.

use rupg_catalog::{ConKind, IndexInfo, NewIndex, NewIndexColumn, PartStrategy, RelKind};
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin;

use super::index::HASH;
use super::{Definer, references};
use crate::expr::Expr;

/// `BTEqualStrategyNumber`.
const BTREE_EQUAL: i16 = 3;
/// `HTEqualStrategyNumber`.
const HASH_EQUAL: i16 = 1;

/// The equality operator of the family of the operator class for its input type, as `get_opfamily_member` gives it.
fn equality_operator(class: u32, strategy: i16) -> Option<u32> {
    let row = builtin::opclasses().iter().find(|c| c.oid == class)?;
    builtin::opfamily_member(row.family, row.intype, row.intype, strategy)
}

/// The operator family of an operator class.
fn family(class: u32) -> u32 {
    builtin::opclasses().iter().find(|c| c.oid == class).map_or(class, |c| c.family)
}

impl Definer<'_, '_> {
    /// The check of `DefineIndex` for a unique index of a partitioned table: each column of the partition key must be a key column of the index, with the same collation and the same equality operator. A partition key with an expression cannot have a unique index.
    pub(super) fn check_partition_key(
        &self,
        table: u32,
        keys: &[NewIndexColumn],
        primary: bool,
    ) -> Result<()> {
        let Some(rel) = self.catalog.relation(table) else { return Ok(()) };
        let Some(partkey) = &rel.partition_key else { return Ok(()) };
        let what = if primary { "PRIMARY KEY" } else { "UNIQUE" };
        let strategy =
            if partkey.strategy == PartStrategy::Hash { HASH_EQUAL } else { BTREE_EQUAL };
        for (i, &attr) in partkey.attrs.iter().enumerate() {
            let class = partkey.classes.get(i).copied().unwrap_or_default();
            let collation = partkey.collations.get(i).copied().unwrap_or_default();
            let operator = equality_operator(class, strategy);
            if attr == 0 {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    format!("unsupported {what} constraint with partition key definition"),
                )
                .with_detail(format!(
                    "{what} constraints cannot be used when partition keys include expressions."
                )));
            }
            let found = keys.iter().any(|column| {
                let method =
                    if family_method(column.class) == HASH { HASH_EQUAL } else { BTREE_EQUAL };
                column.key == attr
                    && column.collation == collation
                    && operator.is_some()
                    && equality_operator(column.class, method) == operator
            });
            if !found {
                let column = rel.column(attr).map_or("", |c| c.name.as_str());
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    format!("{what} constraint on partitioned table must include all partitioning columns"),
                )
                .with_detail(format!(
                    "{what} constraint on table \"{}\" lacks column \"{column}\" which is part of the partition key.",
                    rel.name
                )));
            }
        }
        Ok(())
    }

    /// The loop of `DefineIndex` over the partitions of a partitioned table for its new index `index`: each partition gets the index with a matching definition that is not a partition of another index yet, else a new index. The index of a constraint takes only an index with a constraint. The result is false when the index of a partition is not valid, and then the index is not valid either.
    pub(super) fn index_partitions(
        &mut self,
        table: u32,
        index: u32,
        constraint: Option<u32>,
        alter: bool,
    ) -> Result<bool> {
        let children: Vec<u32> = self.catalog.partitions(table).iter().map(|r| r.oid).collect();
        let mut valid = true;
        for child in children {
            match self.matching_index(child, index, constraint, false) {
                Some((found, found_constraint, found_valid)) => {
                    self.catalog.set_index_parent(found, index)?;
                    if let (Some(child), Some(parent)) = (found_constraint, constraint) {
                        self.catalog.set_constraint_parent(child, parent)?;
                    }
                    valid &= found_valid;
                }
                None => valid &= self.define_partition_index(child, index, constraint, alter)?,
            }
        }
        if !valid {
            self.catalog.set_index_valid(index, false)?;
        }
        Ok(valid)
    }

    /// `CompareIndexInfo` of the indexes of the partition `child` with the index `parent` of its parent table: the first index in the order of the OIDs with no parent index, the same kind of uniqueness, the same access method, the same columns with the same collations and operator families, and the same expressions and predicate. The result is the index, its constraint and its `indisvalid`. For `attach`, as `AttachPartitionEnsureIndexes` does, an index that is not valid does not match, and the constraint of the index must be of the same kind.
    pub(super) fn matching_index(
        &self,
        child: u32,
        parent: u32,
        constraint: Option<u32>,
        attach: bool,
    ) -> Option<(u32, Option<u32>, bool)> {
        let wanted = self.catalog.relation(parent)?.index.as_ref()?;
        let wanted_kind = constraint.and_then(|oid| self.catalog.constraint(oid)).map(|c| c.kind);
        for rel in self.catalog.relations() {
            let Some(info) = rel
                .index
                .as_ref()
                .filter(|i| i.table == child && i.parent == 0 && (i.valid || !attach))
            else {
                continue;
            };
            if !same_definition(info, wanted) {
                continue;
            }
            let found_constraint = match constraint {
                Some(_) => {
                    let found = self.catalog.constraints_of(child).find(|c| {
                        c.index == rel.oid && matches!(c.kind, ConKind::Primary | ConKind::Unique)
                    });
                    match found {
                        Some(con) if !attach || Some(con.kind) == wanted_kind => Some(con.oid),
                        _ => continue,
                    }
                }
                None => None,
            };
            return Some((rel.oid, found_constraint, info.valid));
        }
        None
    }

    /// `generateClonedIndexStmt` and `DefineIndex` for the partition `child` and the index `parent` of its parent table, with the constraint `constraint` of `parent`: a new index of the partition with the definition of `parent` and a name from the name of the partition, as a partition of `parent`. A partitioned partition then gets the indexes of its own partitions. The result is the `indisvalid` of the new index.
    pub(super) fn define_partition_index(
        &mut self,
        child: u32,
        parent: u32,
        constraint: Option<u32>,
        alter: bool,
    ) -> Result<bool> {
        let lookup = || Error::internal(format!("cache lookup failed for index {parent}"));
        let source = self.catalog.relation(parent).ok_or_else(lookup)?;
        let info = source.index.clone().ok_or_else(lookup)?;
        let options = source.options.clone();
        let key_count = usize::try_from(info.key_count).unwrap_or_default();
        let columns: Vec<NewIndexColumn> = source
            .columns
            .iter()
            .zip(&info.keys)
            .enumerate()
            .map(|(i, (column, &attnum))| NewIndexColumn {
                name: column.name.clone(),
                key: attnum,
                ty: column.ty,
                typmod: column.typmod,
                collation: if i < key_count { column.collation } else { 0 },
                class: info.classes.get(i).copied().unwrap_or_default(),
                option: info.options.get(i).copied().unwrap_or_default(),
            })
            .collect();
        let con = constraint.and_then(|oid| self.catalog.constraint(oid)).cloned();
        let (child_name, partitioned) = self
            .catalog
            .relation(child)
            .map(|r| (r.name.clone(), r.kind == RelKind::Partitioned))
            .ok_or_else(|| Error::internal(format!("cache lookup failed for relation {child}")))?;
        if info.primary {
            self.check_primary_key(child, &child_name, &columns[..key_count], alter)?;
        }
        if partitioned && info.unique {
            self.check_partition_key(child, &columns[..key_count], info.primary)?;
        }
        let mut exprs: Vec<Expr> = match &info.exprs {
            Some(text) => crate::node::read_list(text)?,
            None => Vec::new(),
        };
        if let Some(text) = &info.predicate {
            exprs.push(crate::node::read(text)?);
        }
        let refs = if exprs.is_empty() {
            Vec::new()
        } else {
            references(self.catalog, child, &exprs.iter().collect::<Vec<_>>())
        };
        let new = NewIndex {
            table: child,
            name: None,
            columns,
            key_count,
            method: info.method,
            unique: info.unique,
            nulls_not_distinct: info.nulls_not_distinct,
            constraint: con.as_ref().map(|c| c.kind),
            deferrable: con.as_ref().is_some_and(|c| c.deferrable),
            deferred: con.as_ref().is_some_and(|c| c.deferred),
            exprs: info.exprs.clone(),
            predicate: info.predicate.clone(),
            refs,
            pages: self.index_pages(child, info.method),
        };
        let (index, child_constraint) = self.catalog.create_index(new)?;
        if !options.is_empty() {
            self.catalog.set_options(index, options)?;
        }
        self.catalog.set_index_parent(index, parent)?;
        if let (Some(child_constraint), Some(constraint)) = (child_constraint, constraint) {
            self.catalog.set_constraint_parent(child_constraint, constraint)?;
        }
        if partitioned {
            return self.index_partitions(child, index, child_constraint, alter);
        }
        // `index_build` reads the rows of the partition with a sequential scan.
        self.scans.push(child);
        Ok(true)
    }

    /// The part of `DefineRelation` for a new partition of a partitioned table: the partition gets an index for each index of the parent table, in the order of their OIDs, with the constraint of the index.
    pub(super) fn clone_partition_indexes(&mut self, child: u32, parent: u32) -> Result<()> {
        let indexes: Vec<u32> = self
            .catalog
            .relations()
            .filter(|r| r.index.as_ref().is_some_and(|i| i.table == parent))
            .map(|r| r.oid)
            .collect();
        for index in indexes {
            let constraint = self
                .catalog
                .constraints_of(parent)
                .find(|c| c.index == index && matches!(c.kind, ConKind::Primary | ConKind::Unique))
                .map(|c| c.oid);
            self.define_partition_index(child, index, constraint, false)?;
        }
        Ok(())
    }
}

/// The access method of the family of an operator class.
fn family_method(class: u32) -> u32 {
    builtin::opclasses().iter().find(|c| c.oid == class).map_or(0, |c| c.method)
}

/// `CompareIndexInfo`: two indexes with the same kind of uniqueness, the same access method, the same columns with the same collations and operator families, and the same expressions and predicate.
pub(super) fn same_definition(a: &IndexInfo, b: &IndexInfo) -> bool {
    a.unique == b.unique
        && a.nulls_not_distinct == b.nulls_not_distinct
        && a.method == b.method
        && a.keys == b.keys
        && a.key_count == b.key_count
        && a.collations == b.collations
        && a.classes.len() == b.classes.len()
        && a.classes.iter().zip(&b.classes).all(|(&x, &y)| family(x) == family(y))
        && a.exprs == b.exprs
        && a.predicate == b.predicate
}
