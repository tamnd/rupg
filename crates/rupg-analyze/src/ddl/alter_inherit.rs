//! `ALTER TABLE ... INHERIT` and `NO INHERIT`, as `ATExecAddInherit` and `ATExecDropInherit` run them. rupg has no typed tables, so the check of `ATPrepAddInherit` for a typed table has no work.

use rupg_catalog::RelKind;
use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{AlterTableCmd, Node, RangeVar};

use super::alter::Target;
use super::{Definer, relname};

impl Definer<'_, '_> {
    /// `ATPrepAddInherit` and the first check of `ATExecDropInherit`: a partition cannot get or lose a parent of `INHERITS`. `ATSimplePermissions` keeps out a partitioned table before this check.
    pub(super) fn prep_inherit(&self, target: &Target) -> Result<()> {
        if self.catalog.relation(target.oid).is_some_and(|r| r.partition_of.is_some()) {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                "cannot change inheritance of a partition",
            ));
        }
        Ok(())
    }

    /// `ATExecAddInherit` and `CreateInheritance`: the table `target` becomes a child of the table of the command. The child must have each column and each inherited constraint of the parent already, and they then count the parent.
    pub(super) fn add_inherit(&mut self, target: &Target, cmd: &AlterTableCmd) -> Result<()> {
        let Some(Node::RangeVar(rv)) = &cmd.def else {
            return Err(Error::internal("INHERIT with no parent"));
        };
        let parent = self.open_attached(rv, cmd)?;
        let lookup = |oid: u32| Error::internal(format!("cache lookup failed for relation {oid}"));
        let rel = self.catalog.relation(target.oid).cloned().ok_or_else(|| lookup(target.oid))?;
        let parent_rel = self.catalog.relation(parent).cloned().ok_or_else(|| lookup(parent))?;
        if self.catalog.is_temp_schema(parent_rel.namespace)
            && !self.catalog.is_temp_schema(rel.namespace)
        {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("cannot inherit from temporary relation \"{}\"", parent_rel.name),
            ));
        }
        if parent_rel.kind == RelKind::Partitioned {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("cannot inherit from partitioned table \"{}\"", relname(rv)),
            ));
        }
        if parent_rel.partition_of.is_some() {
            return Err(Error::new(SqlState::WRONG_OBJECT_TYPE, "cannot inherit from a partition"));
        }
        // `find_all_inheritors` gives the table itself too, so a table cannot inherit from itself.
        if parent == rel.oid || self.catalog.all_inheritors(rel.oid).contains(&parent) {
            return Err(Error::new(SqlState::DUPLICATE_TABLE, "circular inheritance not allowed")
                .with_detail(format!(
                    "\"{}\" is already a child of \"{}\".",
                    relname(rv),
                    rel.name
                )));
        }
        self.check_transition_triggers(&rel, false)?;
        if rel.inherits.contains(&parent) {
            return Err(Error::new(
                SqlState::DUPLICATE_TABLE,
                format!("relation \"{}\" would be inherited from more than once", parent_rel.name),
            ));
        }
        self.merge_attributes(&rel, &parent_rel)?;
        self.merge_constraints(&rel, &parent_rel)?;
        self.catalog.add_inherit(rel.oid, parent)
    }

    /// `ATExecDropInherit`: the table `target` is no longer a child of the table `rv`. The columns and the constraints that it has from the parent stay, and they become local when they have no other parent.
    pub(super) fn drop_inherit(&mut self, target: &Target, rv: &RangeVar) -> Result<()> {
        self.prep_inherit(target)?;
        let parent = self.open_table(rv)?;
        let inherits =
            self.catalog.relation(target.oid).is_some_and(|r| r.inherits.contains(&parent));
        if !inherits {
            let name =
                self.catalog.relation(parent).map_or_else(|| relname(rv), |r| r.name.as_str());
            return Err(Error::new(
                SqlState::UNDEFINED_TABLE,
                format!("relation \"{name}\" is not a parent of relation \"{}\"", target.name),
            ));
        }
        self.remove_inheritance(target.oid, parent)
    }
}
