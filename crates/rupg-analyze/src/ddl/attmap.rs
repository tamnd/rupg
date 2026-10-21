//! `build_attrmap_by_name` of `attmap.c` for the DDL of partitions. A partition can have its columns in another order than its parent table, and dropped columns, so a column of the parent can have another number in the partition. The expressions, the keys of the constraints and the columns of the indexes and the triggers that a partition takes from its parent get the numbers of the partition, as `map_variable_attnos` gives them.

use rupg_catalog::{IndexInfo, Relation};
use rupg_common::{Error, Result};

use super::Definer;
use crate::node;

/// For each column of a parent table, the number of the column of the partition with the same name, or 0 for a dropped column.
#[derive(Debug)]
pub(super) struct AttMap(Vec<i16>);

impl AttMap {
    /// `build_attrmap_by_name`: the map of the columns of `parent` to the columns of `child`.
    pub(super) fn by_name(parent: &Relation, child: &Relation) -> AttMap {
        let numbers = parent.columns.iter().map(|column| {
            if column.dropped {
                return 0;
            }
            let found = child.columns.iter().position(|c| !c.dropped && c.name == column.name);
            found.map_or(0, |i| i16::try_from(i + 1).unwrap_or(i16::MAX))
        });
        AttMap(numbers.collect())
    }

    /// True when each column has the same number in the two tables.
    pub(super) fn is_same(&self) -> bool {
        self.0.iter().enumerate().all(|(i, &n)| usize::try_from(n).ok() == Some(i + 1))
    }

    /// The number in the partition of the column `number` of the parent. A system column keeps its number, and so does 0, the number of an expression in the columns of an index.
    pub(super) fn column(&self, number: i16) -> i16 {
        if number <= 0 {
            return number;
        }
        usize::try_from(number - 1).ok().and_then(|i| self.0.get(i)).copied().unwrap_or(0)
    }

    /// The numbers in the partition of the columns `numbers` of the parent.
    pub(super) fn columns(&self, numbers: &[i16]) -> Vec<i16> {
        numbers.iter().map(|&n| self.column(n)).collect()
    }

    /// The stored text of an expression of the parent, with the numbers of the partition.
    pub(super) fn text(&self, text: &str) -> Result<String> {
        if self.is_same() {
            return Ok(text.to_owned());
        }
        let mut expr = node::read(text)?;
        expr.map_attnos(&self.0);
        node::write(&expr)
    }

    /// The stored text of a list of expressions of the parent, with the numbers of the partition.
    pub(super) fn list_text(&self, text: &str) -> Result<String> {
        if self.is_same() {
            return Ok(text.to_owned());
        }
        let mut exprs = node::read_list(text)?;
        for expr in &mut exprs {
            expr.map_attnos(&self.0);
        }
        node::write_list(&exprs)
    }

    /// The facts of an index of the parent, with the numbers of the columns of the partition in its keys, its expressions and its predicate, as `CompareIndexInfo` maps them.
    pub(super) fn index(&self, info: &IndexInfo) -> Result<IndexInfo> {
        let mut info = info.clone();
        info.keys = self.columns(&info.keys);
        info.exprs = info.exprs.as_deref().map(|text| self.list_text(text)).transpose()?;
        info.predicate = info.predicate.as_deref().map(|text| self.text(text)).transpose()?;
        Ok(info)
    }
}

impl Definer<'_, '_> {
    /// The map of the columns of the table `parent` to the columns of its partition `child`.
    pub(super) fn att_map(&self, parent: u32, child: u32) -> Result<AttMap> {
        let relation = |oid: u32| {
            self.catalog
                .relation(oid)
                .ok_or_else(|| Error::internal(format!("cache lookup failed for relation {oid}")))
        };
        Ok(AttMap::by_name(relation(parent)?, relation(child)?))
    }
}
