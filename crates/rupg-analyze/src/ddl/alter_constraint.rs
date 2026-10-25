//! `ALTER TABLE ... ALTER CONSTRAINT`, as `ATExecAlterConstraint` runs it: the enforceability of a foreign key or a check constraint, the deferrability of a foreign key and the inheritability of a not-null constraint.

use rupg_catalog::{ConKind, ObjRef, PG_TRIGGER};
use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::ATAlterConstraint;

use super::Definer;
use super::alter::{Origin, Target};
use super::table::make_not_null;

/// The two triggers of a foreign key on one table.
type Pair = (u32, u32);

impl Definer<'_, '_> {
    /// `ATExecAlterConstraint`: the checks of the constraint `cmd.conname` of the table, then the change of its enforceability, its deferrability or its inheritability. `recurse` is false with `ONLY`.
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
            if kind == ConKind::Foreign {
                if cmd.alterDeferrability {
                    self.alter_deferrability(oid, (cmd.deferrable, cmd.initdeferred), recurse)?;
                }
                self.alter_fk_enforceability(oid, cmd.is_enforced, (None, None))?;
            } else {
                self.alter_check_enforceability(target, name, cmd.is_enforced, recurse, None)?;
            }
        } else if cmd.alterDeferrability {
            self.alter_deferrability(oid, (cmd.deferrable, cmd.initdeferred), recurse)?;
        }
        if cmd.alterInheritability {
            self.alter_inheritability(target, oid, cmd.noinherit)?;
        }
        Ok(())
    }

    /// `ATExecAlterFKConstrEnforceability`: the new `conenforced` and `convalidated` of the foreign key `oid` and of the constraints that derive from it. `NOT ENFORCED` drops the triggers of the constraints, those of the clones first. `ENFORCED` gives a foreign key that was not enforced its action triggers and its check triggers, as clones of the triggers `parents` of the parent constraint, and the session then checks the rows of the referencing table. A clone for a partition of the referenced table has only action triggers, and a clone for a partition of the referencing table has only check triggers.
    fn alter_fk_enforceability(
        &mut self,
        oid: u32,
        enforced: bool,
        parents: (Option<Pair>, Option<Pair>),
    ) -> Result<()> {
        let Some(con) = self.catalog.constraint(oid) else {
            return Err(Error::internal(format!("cache lookup failed for constraint {oid}")));
        };
        let (relation, parent, changed) = (con.relation, con.parent, con.enforced != enforced);
        if changed {
            self.catalog.set_constraint_enforced(oid, enforced)?;
        }
        let children: Vec<(u32, u32)> = self
            .catalog
            .constraints()
            .filter(|c| c.parent == oid && c.kind == ConKind::Foreign)
            .map(|c| (c.oid, c.relation))
            .collect();
        if !enforced {
            for &(child, _) in &children {
                self.alter_fk_enforceability(child, false, (None, None))?;
            }
            let triggers: Vec<u32> =
                self.catalog.triggers().filter(|t| t.constraint == oid).map(|t| t.oid).collect();
            for trigger in triggers {
                let object = ObjRef::new(PG_TRIGGER, trigger);
                self.catalog.clear_trigger_parent(trigger);
                self.catalog.drop_depends_of(object);
                self.perform(&[object], false)?;
            }
            return Ok(());
        }
        if !changed {
            return Ok(());
        }
        // A clone on the same table as its parent is a clone for a partition of the referenced table.
        let referenced_side =
            parent == 0 || self.catalog.constraint(parent).is_some_and(|p| p.relation == relation);
        let referencing_side = parent == 0 || !referenced_side;
        let actions = if referenced_side {
            Some(self.catalog.add_fk_action_triggers(oid, parents.0)?)
        } else {
            None
        };
        let checks = if referencing_side {
            Some(self.catalog.add_fk_check_triggers(oid, parents.1)?)
        } else {
            None
        };
        if referencing_side {
            let leaves = self.leaves(relation);
            self.scans.extend(leaves);
        }
        for (child, child_relation) in children {
            let triggers =
                if child_relation == relation { (actions, None) } else { (None, checks) };
            self.alter_fk_enforceability(child, true, triggers)?;
        }
        Ok(())
    }

    /// `ATExecAlterCheckConstrEnforceability`: the new `conenforced` and `convalidated` of the check constraint `name` of the table and of the same constraint of each table that inherits from it. A constraint that becomes enforced gets the check of the rows of its table. An inherited constraint cannot become `NOT ENFORCED` while a parent enforces it, and in the recursion the constraint of a child stays enforced when a parent outside the tree of `root` enforces it.
    fn alter_check_enforceability(
        &mut self,
        target: &Target,
        name: &str,
        enforced: bool,
        recurse: bool,
        root: Option<u32>,
    ) -> Result<()> {
        let found = self.catalog.constraints_of(target.oid).find(|c| c.name == name);
        let Some((oid, current, inhcount, no_inherit)) =
            found.map(|c| (c.oid, c.enforced, c.inhcount, c.no_inherit))
        else {
            return Err(Error::new(
                SqlState::UNDEFINED_OBJECT,
                format!("constraint \"{name}\" of relation \"{}\" does not exist", target.name),
            ));
        };
        let mut target_enforced = enforced;
        let partition = self.catalog.relation(target.oid).is_some_and(|r| r.partition_of.is_some());
        if !enforced && inhcount > 0 && (root.is_none() || !partition) {
            let tree: Vec<u32> = match root {
                Some(root) => {
                    let mut tree = self.catalog.all_inheritors(root);
                    tree.push(root);
                    tree
                }
                None => Vec::new(),
            };
            if let Some(parent) = self.enforcing_parent(target.oid, name, &tree) {
                if root.is_none() {
                    return Err(Error::new(
                        SqlState::INVALID_OBJECT_DEFINITION,
                        format!("cannot mark inherited constraint \"{name}\" as NOT ENFORCED"),
                    )
                    .with_detail(format!(
                        "The matching constraint on parent table \"{parent}\" is ENFORCED."
                    )));
                }
                target_enforced = true;
            }
        }
        let changed = current != target_enforced;
        if changed {
            self.catalog.set_constraint_enforced(oid, target_enforced)?;
        }
        if (!enforced || changed) && root.is_none() && !no_inherit {
            let children = self.all_inheritors(target.oid);
            if !recurse && !children.is_empty() {
                return Err(Error::new(
                    SqlState::INVALID_TABLE_DEFINITION,
                    "constraint must be altered on child tables too",
                )
                .with_hint("Do not specify the ONLY keyword."));
            }
            for child in children {
                self.alter_check_enforceability(&child, name, enforced, false, Some(target.oid))?;
            }
        }
        // Phase 3 of `ALTER TABLE` reads the rows of a table whose constraint becomes enforced.
        if target.kind == b'r' && !current && target_enforced {
            self.scans.push(target.oid);
        }
        Ok(())
    }

    /// The name of a parent of the table, not in `tree`, whose check constraint `name` is enforced, as `ATCheckCheckConstrHasEnforcedParent` finds it.
    fn enforcing_parent(&self, table: u32, name: &str, tree: &[u32]) -> Option<String> {
        let rel = self.catalog.relation(table)?;
        let partition = rel.partition_of.as_ref().map(|p| p.parent);
        let parents = rel.inherits.iter().copied().chain(partition);
        for parent in parents.filter(|p| !tree.contains(p)) {
            let enforced = self
                .catalog
                .constraints_of(parent)
                .any(|c| c.kind == ConKind::Check && c.name == name && c.enforced);
            if enforced {
                return self.catalog.relation(parent).map(|r| r.name.clone());
            }
        }
        None
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
