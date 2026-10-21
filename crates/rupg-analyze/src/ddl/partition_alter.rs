//! The recursion of `ALTER TABLE` to the partitions of a partitioned table: `ATSimpleRecursion` and the recursion of `ATPrepAlterColumnType` in the checks of the subcommands, and the recursion of `ATExecAddColumn` and `ATAddCheckNNConstraint` when the subcommands run. The recursion of `ATExecSetNotNull` is in `alter`, and the recursion of `ATExecDropColumn` and `dropconstraint_internal` is in `drop_constraint`.
//!
//! A partition can have the columns of its parent table in another order, so a column can have another number in each table. The recursion finds the column of a partition by name, and an expression of the parent table gets the numbers of the partition.

use rupg_catalog::{Column, ConKind};
use rupg_common::{Error, Result, SqlState};

use super::alter::Target;
use super::partition_attach::without_locations;
use super::table::collation_name;
use super::{Definer, Fill, Found, column_numbers, references};
use crate::expr::Expr;

/// The error of a relation that is not in the catalog.
fn lookup(oid: u32) -> Error {
    Error::internal(format!("cache lookup failed for relation {oid}"))
}

impl Definer<'_, '_> {
    /// The target of a partition that the recursion reaches.
    fn target_of(&self, oid: u32) -> Target {
        let (name, namespace) =
            self.catalog.relation(oid).map(|r| (r.name.clone(), r.namespace)).unwrap_or_default();
        Target { oid, kind: self.kind_of(Found::User(oid)), name, namespace }
    }

    /// `find_inheritance_children`: the partitions of a table and its children of `INHERITS`, in the order of their OIDs.
    pub(super) fn child_targets(&self, table: u32) -> Vec<Target> {
        let children: Vec<u32> = self.catalog.children(table).iter().map(|r| r.oid).collect();
        children.into_iter().map(|oid| self.target_of(oid)).collect()
    }

    /// `find_all_inheritors` with no table itself: the children of the table, then the children of each of them in the same order, and so on.
    pub(super) fn all_inheritors(&self, table: u32) -> Vec<Target> {
        self.catalog.all_inheritors(table).into_iter().map(|oid| self.target_of(oid)).collect()
    }

    /// `has_partition_attrs` for one column: the column is a column of the partition key of the table, or an expression of the key uses it.
    pub(super) fn in_partition_key(&self, table: u32, column: i16) -> Result<bool> {
        let rel = self.catalog.relation(table);
        let Some(key) = rel.and_then(|r| r.partition_key.as_ref()) else { return Ok(false) };
        if key.attrs.contains(&column) {
            return Ok(true);
        }
        let exprs: Vec<Expr> = match &key.exprs {
            Some(text) => crate::node::read_list(text)?,
            None => Vec::new(),
        };
        Ok(column_numbers(&exprs.iter().collect::<Vec<_>>()).contains(&column))
    }

    /// The recursion of `ATExecAddColumn`: each partition of `parent` gets the new column `number` of `parent` as an inherited column, at the end of its own columns, with the default and the not-null constraint of the column, and then the partitions of a partition get the column in turn. A child of `INHERITS` with a column of the same name merges the new column with its own column.
    pub(super) fn add_column_to_partitions(&mut self, parent: u32, number: i16) -> Result<()> {
        let rel = self.catalog.relation(parent).ok_or_else(|| lookup(parent))?;
        let Some(column) = rel.column(number).cloned() else {
            return Err(Error::internal(format!(
                "cache lookup failed for attribute {number} of relation {parent}"
            )));
        };
        let default = self.catalog.default_of(parent, number).map(|d| d.expr.clone());
        let not_null = self
            .catalog
            .constraints_of(parent)
            .find(|c| c.kind == ConKind::NotNull && c.keys == [number] && !c.no_inherit)
            .map(|c| c.name.clone());
        for child in self.child_targets(parent) {
            let rel = self.catalog.relation(child.oid);
            let own = rel.and_then(|r| r.column_number(&column.name));
            if let Some(own) = own {
                self.merge_child_column(&child, own, &column)?;
                continue;
            }
            let mut new = column.clone();
            new.not_null = false;
            new.has_default = false;
            new.local = false;
            new.inhcount = 1;
            let own = self.catalog.add_column(child.oid, new)?;
            if let Some(text) = &default {
                let expr = crate::node::read(text)?;
                let refs = references(self.catalog, child.oid, &[&expr]);
                self.catalog.add_default(child.oid, own, text.clone(), &refs)?;
            }
            self.added.push(Fill { table: child.oid, column: own, default: default.clone() });
            if let Some(name) = &not_null {
                let used = self.catalog.constraints_of(child.oid).any(|c| c.name == *name);
                let given = (!used).then_some(name.as_str());
                let oid = self.catalog.add_not_null(child.oid, given, own, false)?;
                self.catalog.set_constraint_inherited(oid, false, 1)?;
            }
            self.toast_after_columns(child.oid)?;
            self.add_column_to_partitions(child.oid, own)?;
        }
        Ok(())
    }

    /// The merge of `ATExecAddColumn` when a child of `INHERITS` has a column `own` with the name of the new column of its parent: the column must have the same type and collation, and it counts one more parent. The children of the child have the column already, so the recursion stops.
    fn merge_child_column(&mut self, child: &Target, own: i16, column: &Column) -> Result<()> {
        let Some(found) = self.catalog.relation(child.oid).and_then(|r| r.column(own)) else {
            return Err(lookup(child.oid));
        };
        if found.ty != column.ty || found.typmod != column.typmod {
            return Err(Error::new(
                SqlState::DATATYPE_MISMATCH,
                format!(
                    "child table \"{}\" has different type for column \"{}\"",
                    child.name, column.name
                ),
            ));
        }
        if found.collation != column.collation {
            return Err(Error::new(
                SqlState::COLLATION_MISMATCH,
                format!(
                    "child table \"{}\" has different collation for column \"{}\"",
                    child.name, column.name
                ),
            )
            .with_detail(format!(
                "\"{}\" versus \"{}\"",
                collation_name(column.collation),
                collation_name(found.collation)
            )));
        }
        let Some(inhcount) = found.inhcount.checked_add(1) else {
            return Err(Error::new(
                SqlState::PROGRAM_LIMIT_EXCEEDED,
                "too many inheritance parents",
            ));
        };
        let local = found.local;
        self.catalog.set_column_inherited(child.oid, own, local, inhcount)?;
        self.notice(
            SqlState::SUCCESSFUL_COMPLETION,
            format!(
                "merging definition of column \"{}\" for child \"{}\"",
                column.name, child.name
            ),
        );
        Ok(())
    }

    /// The recursion of `ATAddCheckNNConstraint` for the new check constraint `oid` of `parent`: each partition gets a constraint with the same name and expression, with the numbers of the columns of the partition, or `MergeWithExistingConstraint` merges the constraint with a constraint of the partition that has the same name and expression. A partition with a new constraint passes it to its own partitions. With `ONLY`, a table with partitions cannot get the constraint.
    pub(super) fn add_check_to_partitions(
        &mut self,
        parent: &Target,
        oid: u32,
        recurse: bool,
    ) -> Result<()> {
        let Some(con) = self.catalog.constraint(oid).cloned() else {
            return Err(Error::internal(format!("cache lookup failed for constraint {oid}")));
        };
        if con.no_inherit {
            return Ok(());
        }
        let children = self.child_targets(parent.oid);
        if !recurse && !children.is_empty() {
            return Err(Error::new(
                SqlState::INVALID_TABLE_DEFINITION,
                "constraint must be added to child tables too",
            ));
        }
        let parent_text = con.expr.clone().unwrap_or_default();
        for child in children {
            let map = self.att_map(parent.oid, child.oid)?;
            let text = map.text(&parent_text)?;
            let existing =
                self.catalog.constraints_of(child.oid).find(|c| c.name == con.name).map(|c| {
                    (c.oid, c.kind, c.expr.clone(), c.no_inherit, c.validated, c.local, c.inhcount)
                });
            if let Some((found, kind, expr, no_inherit, validated, local, inhcount)) = existing {
                if kind != ConKind::Check
                    || without_locations(expr.as_deref()) != without_locations(Some(&text))
                {
                    return Err(Error::new(
                        SqlState::DUPLICATE_OBJECT,
                        format!(
                            "constraint \"{}\" for relation \"{}\" already exists",
                            con.name, child.name
                        ),
                    ));
                }
                let conflict = if no_inherit {
                    "non-inherited"
                } else if !validated {
                    "NOT VALID"
                } else {
                    ""
                };
                if !conflict.is_empty() {
                    return Err(Error::new(
                        SqlState::INVALID_OBJECT_DEFINITION,
                        format!(
                            "constraint \"{}\" conflicts with {conflict} constraint on relation \"{}\"",
                            con.name, child.name
                        ),
                    ));
                }
                self.notice(
                    SqlState::SUCCESSFUL_COMPLETION,
                    format!("merging constraint \"{}\" with inherited definition", con.name),
                );
                // A partition has the constraint only from its parent, and a child of `INHERITS` keeps its own constraint with one more parent.
                if self.catalog.relation(child.oid).is_some_and(|r| r.partition_of.is_some()) {
                    self.catalog.set_constraint_inherited(found, false, 1)?;
                } else {
                    self.catalog.set_constraint_inherited(
                        found,
                        local,
                        inhcount.saturating_add(1),
                    )?;
                }
                continue;
            }
            let expr = crate::node::read(&text)?;
            let refs = references(self.catalog, child.oid, &[&expr]);
            let new = self.catalog.add_check(
                child.oid,
                Some(&con.name),
                text.clone(),
                map.columns(&con.keys),
                &refs,
                false,
            )?;
            self.catalog.set_constraint_inherited(new, false, 1)?;
            // Phase 3 of `ALTER TABLE` reads the rows of a partition with a new constraint.
            if child.kind != b'p' {
                self.scans.push(child.oid);
            }
            self.add_check_to_partitions(&child, new, true)?;
        }
        Ok(())
    }
}
