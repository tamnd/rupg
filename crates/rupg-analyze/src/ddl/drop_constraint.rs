//! `ALTER TABLE ... DROP CONSTRAINT`, `DROP COLUMN` and `ALTER COLUMN ... DROP NOT NULL`, as `ATExecDropConstraint`, `ATExecDropColumn`, `ATExecDropNotNull` and `dropconstraint_internal` run them in the pass of the drops.
//!
//! A drop on a partitioned table goes on to its partitions. A partition loses the column or the constraint when it has it only from its parent table, and else it keeps it with a lower `attinhcount` or `coninhcount`. With `ONLY`, a partitioned table with partitions cannot lose a column, and its partitions keep the constraint as their own.

use rupg_catalog::{ConKind, ObjRef, PG_CONSTRAINT};
use rupg_common::{Error, Result, SqlState};

use super::Definer;
use super::alter::Target;
use crate::from::system_column;

impl Definer<'_, '_> {
    /// `ATExecDropConstraint`: finds the constraint of the table by its name, then removes it and the objects that depend on it. `recurse` is false for `ONLY`.
    pub(super) fn drop_constraint(
        &mut self,
        target: &Target,
        name: &str,
        missing_ok: bool,
        cascade: bool,
        recurse: bool,
    ) -> Result<()> {
        let found = self.catalog.constraints_of(target.oid).find(|c| c.name == name);
        let Some(oid) = found.map(|c| c.oid) else {
            let message =
                format!("constraint \"{name}\" of relation \"{}\" does not exist", target.name);
            if !missing_ok {
                return Err(Error::new(SqlState::UNDEFINED_OBJECT, message));
            }
            self.notice(SqlState::SUCCESSFUL_COMPLETION, format!("{message}, skipping"));
            return Ok(());
        };
        self.drop_constraint_internal(target, oid, cascade, recurse, false)
    }

    /// `dropconstraint_internal`: removes the constraint `oid` of the table and the objects that depend on it. A check constraint or a not-null constraint then goes on to the partitions, unless it is `NO INHERIT`: a partition loses the constraint when it has it only from the table, and else its `coninhcount` goes down. With `ONLY`, the partitions keep the constraint as their own when the table was its only source. The partitions of a primary key, a unique constraint or a foreign key go with the constraint as objects that depend on it.
    fn drop_constraint_internal(
        &mut self,
        target: &Target,
        oid: u32,
        cascade: bool,
        recurse: bool,
        recursing: bool,
    ) -> Result<()> {
        let Some(con) = self.catalog.constraint(oid).cloned() else {
            return Err(Error::internal(format!("cache lookup failed for constraint {oid}")));
        };
        if con.inhcount > 0 && !recursing {
            return Err(Error::new(
                SqlState::INVALID_TABLE_DEFINITION,
                format!(
                    "cannot drop inherited constraint \"{}\" of relation \"{}\"",
                    con.name, target.name
                ),
            ));
        }
        let column = match (con.kind, &con.keys[..]) {
            (ConKind::NotNull, &[number]) => {
                self.check_drop_not_null(target, number)?;
                let rel = self.catalog.relation(target.oid);
                Some(
                    rel.and_then(|r| r.column(number)).map_or_else(String::new, |c| c.name.clone()),
                )
            }
            _ => None,
        };
        self.perform(&[ObjRef::new(PG_CONSTRAINT, oid)], cascade)?;
        if !matches!(con.kind, ConKind::Check | ConKind::NotNull) && target.kind == b'p' {
            return Ok(());
        }
        if con.no_inherit {
            return Ok(());
        }
        for child in self.child_targets(target.oid) {
            let found = match &column {
                Some(column) => {
                    let rel = self.catalog.relation(child.oid);
                    let number = rel.and_then(|r| r.column_number(column));
                    let found = self.catalog.constraints_of(child.oid).find(|c| {
                        c.kind == ConKind::NotNull && number.is_some_and(|n| c.keys == [n])
                    });
                    let Some(found) = found else {
                        return Err(Error::internal(format!(
                            "cache lookup failed for not-null constraint on column \"{column}\" of relation {}",
                            child.oid
                        )));
                    };
                    found
                }
                None => {
                    let found = self.catalog.constraints_of(child.oid).find(|c| c.name == con.name);
                    let Some(found) = found else {
                        return Err(Error::new(
                            SqlState::UNDEFINED_OBJECT,
                            format!(
                                "constraint \"{}\" of relation \"{}\" does not exist",
                                con.name, child.name
                            ),
                        ));
                    };
                    found
                }
            };
            let (found, kind, local, inhcount) =
                (found.oid, found.kind, found.local, found.inhcount);
            if !matches!(kind, ConKind::Check | ConKind::NotNull) {
                return Err(Error::internal(
                    "inherited constraint is not a CHECK or not-null constraint",
                ));
            }
            if inhcount <= 0 {
                return Err(Error::internal(format!(
                    "relation {} has non-inherited constraint \"{}\"",
                    child.oid, con.name
                )));
            }
            if !recurse {
                self.catalog.set_constraint_inherited(
                    found,
                    local || inhcount == 1,
                    inhcount - 1,
                )?;
            } else if inhcount == 1 && !local {
                self.drop_constraint_internal(&child, found, cascade, true, true)?;
            } else {
                self.catalog.set_constraint_inherited(found, local, inhcount - 1)?;
            }
        }
        Ok(())
    }

    /// `ATExecDropColumn`: removes the column with the objects that depend on it, as `performDeletion` does. The column keeps its place as a dropped column, so the other columns keep their numbers. The column of a partitioned table goes from its partitions too, in the same deletion. `recurse` is false for `ONLY`.
    pub(super) fn drop_column(
        &mut self,
        target: &Target,
        column: &str,
        missing_ok: bool,
        cascade: bool,
        recurse: bool,
    ) -> Result<()> {
        let mut drops = Vec::new();
        if !self.collect_drop_column(target, column, missing_ok, recurse, false, &mut drops)? {
            return Ok(());
        }
        let objects: Vec<ObjRef> = drops.iter().map(|&(t, n, _)| ObjRef::column(t, n)).collect();
        self.perform(&objects, cascade)?;
        for (table, _, at) in drops {
            self.drop_field(table, at)?;
        }
        Ok(())
    }

    /// The checks and the recursion of `ATExecDropColumn`: the columns to drop go to `drops` with the table, the number of the column and the place of the field among the fields of the row type, the columns of the partitions first. The result is false for a column that the table does not have, with `IF EXISTS`.
    fn collect_drop_column(
        &mut self,
        target: &Target,
        column: &str,
        missing_ok: bool,
        recurse: bool,
        recursing: bool,
        drops: &mut Vec<(u32, i16, usize)>,
    ) -> Result<bool> {
        let rel = self.catalog.relation(target.oid);
        let Some(number) = rel.and_then(|r| r.column_number(column)) else {
            // A composite type has no system columns.
            if target.kind != b'c' && system_column(column).is_some() {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    format!("cannot drop system column \"{column}\""),
                ));
            }
            let message =
                format!("column \"{column}\" of relation \"{}\" does not exist", target.name);
            if !missing_ok {
                return Err(Error::new(SqlState::UNDEFINED_COLUMN, message));
            }
            self.notice(SqlState::SUCCESSFUL_COMPLETION, format!("{message}, skipping"));
            return Ok(false);
        };
        if rel.and_then(|r| r.column(number)).is_some_and(|c| c.inhcount > 0) && !recursing {
            return Err(Error::new(
                SqlState::INVALID_TABLE_DEFINITION,
                format!("cannot drop inherited column \"{column}\""),
            ));
        }
        if self.in_partition_key(target.oid, number)? {
            return Err(Error::new(
                SqlState::INVALID_TABLE_DEFINITION,
                format!(
                    "cannot drop column \"{column}\" because it is part of the partition key of relation \"{}\"",
                    target.name
                ),
            ));
        }
        let children = self.child_targets(target.oid);
        if !children.is_empty() && target.kind == b'p' && !recurse {
            return Err(Error::new(
                SqlState::INVALID_TABLE_DEFINITION,
                "cannot drop column from only the partitioned table when partitions exist",
            )
            .with_hint("Do not specify the ONLY keyword."));
        }
        for child in children {
            let rel = self.catalog.relation(child.oid);
            let found = rel.and_then(|r| {
                let n = r.column_number(column)?;
                r.column(n).map(|c| (n, c.local, c.inhcount))
            });
            let Some((n, local, inhcount)) = found else {
                return Err(Error::internal(format!(
                    "cache lookup failed for attribute \"{column}\" of relation {}",
                    child.oid
                )));
            };
            if inhcount <= 0 {
                return Err(Error::internal(format!(
                    "relation {} has non-inherited attribute \"{column}\"",
                    child.oid
                )));
            }
            if !recurse {
                self.catalog.set_column_inherited(child.oid, n, true, inhcount - 1)?;
            } else if inhcount == 1 && !local {
                self.collect_drop_column(&child, column, false, true, true, drops)?;
            } else {
                self.catalog.set_column_inherited(child.oid, n, local, inhcount - 1)?;
            }
        }
        // The place of the field among the fields of the row type, which have no dropped columns.
        let at = self.catalog.relation(target.oid).map_or(0, |r| {
            r.columns
                .iter()
                .take(usize::try_from(number - 1).unwrap_or(0))
                .filter(|c| !c.dropped)
                .count()
        });
        drops.push((target.oid, number, at));
        Ok(true)
    }

    /// `ATExecDropNotNull`: removes the not-null constraint of the column. A column with no constraint needs no change. A partition keeps the constraint of a column that is not null in its parent table. `recurse` is false for `ONLY`.
    pub(super) fn drop_not_null(
        &mut self,
        target: &Target,
        column: &str,
        recurse: bool,
    ) -> Result<()> {
        let rel = self.catalog.relation(target.oid);
        let Some(number) = rel.and_then(|r| r.column_number(column)) else {
            if system_column(column).is_some() {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    format!("cannot alter system column \"{column}\""),
                ));
            }
            return Err(Error::new(
                SqlState::UNDEFINED_COLUMN,
                format!("column \"{column}\" of relation \"{}\" does not exist", target.name),
            ));
        };
        let attr = rel.and_then(|r| r.column(number));
        if attr.is_some_and(|c| c.identity != 0) {
            return Err(Error::new(
                SqlState::SYNTAX_ERROR,
                format!(
                    "column \"{column}\" of relation \"{}\" is an identity column",
                    target.name
                ),
            ));
        }
        if !attr.is_some_and(|c| c.not_null) {
            return Ok(());
        }
        let parent =
            rel.and_then(|r| r.partition_of.as_ref()).and_then(|p| self.catalog.relation(p.parent));
        let in_parent = parent.and_then(|p| p.column(p.column_number(column)?));
        if in_parent.is_some_and(|c| c.not_null) {
            return Err(Error::new(
                SqlState::INVALID_TABLE_DEFINITION,
                format!("column \"{column}\" is marked NOT NULL in parent table"),
            ));
        }
        let found = self
            .catalog
            .constraints_of(target.oid)
            .find(|c| c.kind == ConKind::NotNull && c.keys == [number])
            .map(|c| c.oid);
        match found {
            Some(oid) => self.drop_constraint_internal(target, oid, false, recurse, false),
            None => Err(Error::internal(format!(
                "could not find not-null constraint on column \"{column}\", relation \"{}\"",
                target.name
            ))),
        }
    }

    /// The checks of `dropconstraint_internal` for a not-null constraint: a column of the primary key, a key column of the replica identity index and an identity column keep their constraint.
    fn check_drop_not_null(&self, target: &Target, number: i16) -> Result<()> {
        let attr = self.catalog.relation(target.oid).and_then(|r| r.column(number));
        let column = attr.map_or("", |c| c.name.as_str());
        let primary = self
            .catalog
            .constraints_of(target.oid)
            .any(|c| c.kind == ConKind::Primary && c.keys.contains(&number));
        if primary {
            return Err(Error::new(
                SqlState::INVALID_TABLE_DEFINITION,
                format!("column \"{column}\" is in a primary key"),
            ));
        }
        let replica = self.catalog.relation(target.oid).is_some_and(|r| r.replica_identity == b'i')
            && self.catalog.relations().any(|r| {
                r.index.as_ref().is_some_and(|i| {
                    i.table == target.oid
                        && i.replica_identity
                        && i.keys
                            .iter()
                            .take(usize::try_from(i.key_count).unwrap_or_default())
                            .any(|&k| k == number)
                })
            });
        if replica {
            return Err(Error::new(
                SqlState::INVALID_TABLE_DEFINITION,
                format!("column \"{column}\" is in index used as replica identity"),
            ));
        }
        if attr.is_some_and(|c| c.identity != 0) {
            return Err(Error::new(
                SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                format!(
                    "column \"{column}\" of relation \"{}\" is an identity column",
                    target.name
                ),
            ));
        }
        Ok(())
    }
}
