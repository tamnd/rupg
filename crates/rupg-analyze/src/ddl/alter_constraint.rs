//! `ALTER TABLE ... ALTER CONSTRAINT`, as `ATExecAlterConstraint` runs it: the deferrability of a foreign key and the inheritability of a not-null constraint.

use rupg_catalog::ConKind;
use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::ATAlterConstraint;

use super::alter::{Origin, Target};
use super::table::make_not_null;
use super::{Definer, not_yet};

impl Definer<'_, '_> {
    /// `ATExecAlterConstraint`: the checks of the constraint `cmd.conname` of the table, then the change of its deferrability or its inheritability. `recurse` is false with `ONLY`. A constraint is always `ENFORCED` here, so `ENFORCED` has no work.
    pub(super) fn alter_constraint(
        &mut self,
        target: &Target,
        cmd: &ATAlterConstraint,
        recurse: bool,
    ) -> Result<()> {
        let table = &target.name;
        if target.kind == b'p' && !recurse {
            return Err(Error::new(
                SqlState::INVALID_TABLE_DEFINITION,
                "constraint must be altered in child tables too",
            )
            .with_hint("Do not specify the ONLY keyword."));
        }
        let name = cmd.conname.as_deref().unwrap_or_default();
        let found = self.catalog.constraints_of(target.oid).find(|c| c.name == name);
        let Some(con) = found.map(|c| (c.oid, c.kind, c.inhcount, c.parent)) else {
            return Err(Error::new(
                SqlState::UNDEFINED_OBJECT,
                format!("constraint \"{name}\" of relation \"{table}\" does not exist"),
            ));
        };
        let (oid, kind, inhcount, parent) = con;
        let wrong = |message: String| Error::new(SqlState::WRONG_OBJECT_TYPE, message);
        if cmd.alterDeferrability && kind != ConKind::Foreign {
            return Err(wrong(format!(
                "constraint \"{name}\" of relation \"{table}\" is not a foreign key constraint"
            )));
        }
        if cmd.alterEnforceability && !matches!(kind, ConKind::Foreign | ConKind::Check) {
            return Err(wrong(format!(
                "cannot alter enforceability of constraint \"{name}\" of relation \"{table}\""
            ))
            .with_hint("Only foreign key and check constraints can change enforceability."));
        }
        if cmd.alterInheritability && kind != ConKind::NotNull {
            return Err(wrong(format!(
                "constraint \"{name}\" of relation \"{table}\" is not a not-null constraint"
            )));
        }
        if cmd.alterInheritability && cmd.noinherit && target.kind == b'p' {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!(
                    "not-null constraint \"{name}\" on partitioned table \"{table}\" cannot be NO INHERIT"
                ),
            ));
        }
        let prerequisite =
            |message: String| Error::new(SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE, message);
        if cmd.alterInheritability && cmd.noinherit && inhcount > 0 {
            return Err(prerequisite(format!(
                "cannot alter inherited constraint \"{name}\" on relation \"{table}\""
            )));
        }
        if parent != 0 {
            let error =
                prerequisite(format!("cannot alter constraint \"{name}\" on relation \"{table}\""))
                    .with_hint("You may alter the constraint it derives from instead.");
            return Err(match self.topmost_constraint(parent) {
                Some((top, relation)) => error.with_detail(format!(
                    "Constraint \"{name}\" is derived from constraint \"{top}\" of relation \"{relation}\"."
                )),
                None => error,
            });
        }
        if cmd.alterEnforceability {
            if !cmd.is_enforced {
                return Err(not_yet("NOT ENFORCED", None));
            }
        } else if cmd.alterDeferrability {
            self.alter_deferrability(oid, (cmd.deferrable, cmd.initdeferred), recurse)?;
        }
        if cmd.alterInheritability {
            self.alter_inheritability(target, oid, cmd.noinherit)?;
        }
        Ok(())
    }

    /// The name of the constraint at the top of the constraints that the constraint `parent` derives from, with the name of its table.
    fn topmost_constraint(&self, parent: u32) -> Option<(String, String)> {
        let mut con = self.catalog.constraint(parent)?;
        while con.parent != 0 {
            con = self.catalog.constraint(con.parent)?;
        }
        let relation = self.catalog.relation(con.relation)?;
        Some((con.name.clone(), relation.name.clone()))
    }

    /// `ATExecAlterConstrDeferrability`: the new `DEFERRABLE` and `INITIALLY DEFERRED` of the foreign key and of its triggers. A change goes on to the constraints of the partitions that derive from it, as `AlterConstrDeferrabilityRecurse` does. A foreign key that has the new state already has no work.
    fn alter_deferrability(
        &mut self,
        oid: u32,
        (deferrable, deferred): (bool, bool),
        recurse: bool,
    ) -> Result<()> {
        let Some(con) = self.catalog.constraint(oid) else {
            return Err(Error::internal(format!("cache lookup failed for constraint {oid}")));
        };
        if con.deferrable == deferrable && con.deferred == deferred {
            return Ok(());
        }
        self.catalog.set_constraint_deferrability(oid, deferrable, deferred)?;
        if !recurse {
            return Ok(());
        }
        let children: Vec<u32> =
            self.catalog.constraints().filter(|c| c.parent == oid).map(|c| c.oid).collect();
        for child in children {
            self.alter_deferrability(child, (deferrable, deferred), recurse)?;
        }
        Ok(())
    }

    /// `ATExecAlterConstrInheritability`: the new `NO INHERIT` of the not-null constraint. With `NO INHERIT`, the not-null constraint of the column of each child no longer counts this table and becomes a constraint of the child itself. With `INHERIT`, each child gets the constraint, as `ATExecSetNotNull` gives it. The change does not go on to the children of the children.
    fn alter_inheritability(&mut self, target: &Target, oid: u32, no_inherit: bool) -> Result<()> {
        let Some(con) = self.catalog.constraint(oid) else {
            return Err(Error::internal(format!("cache lookup failed for constraint {oid}")));
        };
        if con.no_inherit == no_inherit {
            return Ok(());
        }
        let (name, number) = (con.name.clone(), con.keys.first().copied().unwrap_or_default());
        let rel = self.catalog.relation(target.oid);
        let column = rel.and_then(|r| r.column(number)).map(|c| c.name.clone());
        let Some(column) = column else {
            return Err(Error::internal(format!("the column of constraint {oid} does not exist")));
        };
        self.catalog.set_constraint_no_inherit(oid, no_inherit)?;
        for child in self.child_targets(target.oid) {
            if no_inherit {
                let number =
                    self.catalog.relation(child.oid).and_then(|r| r.column_number(&column));
                let found = self
                    .catalog
                    .constraints_of(child.oid)
                    .find(|c| c.kind == ConKind::NotNull && number.is_some_and(|n| c.keys == [n]))
                    .map(|c| (c.oid, c.inhcount));
                let Some((child_con, inhcount)) = found else {
                    return Err(Error::internal(format!(
                        "cache lookup failed for not-null constraint on column \"{column}\" of relation {}",
                        child.oid
                    )));
                };
                self.catalog.set_constraint_inherited(child_con, true, inhcount - 1)?;
            } else {
                let mut con = make_not_null(&column);
                con.conname = Some(name.as_str().into());
                if self.alter_not_null(&child, &con, Origin::SetNotNull, true, true)?
                    && child.kind != b'p'
                {
                    self.scans.push(child.oid);
                }
            }
        }
        Ok(())
    }
}
