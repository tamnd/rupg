//! `ALTER TABLE ... ADD CONSTRAINT ... NOT VALID` and `ALTER TABLE ... VALIDATE CONSTRAINT` of check constraints and foreign keys, as `ATExecValidateConstraint` and `QueueCheckConstraintValidation` run them.
//!
//! A `NOT VALID` check constraint has `convalidated` false: the new rows must satisfy it, but the rows from before it are not checked. `VALIDATE CONSTRAINT` sets `convalidated`, and the session then checks the rows of the table against the constraint, as it does for a new constraint.

use rupg_catalog::ConKind;
use rupg_common::{Error, Result, SqlState};

use super::Definer;
use super::alter::Target;

impl Definer<'_, '_> {
    /// `ATExecValidateConstraint`: marks the check constraint `name` of the table as validated, and the same constraint of each table that inherits from it first. A not-null constraint is always valid.
    pub(super) fn validate_constraint(
        &mut self,
        target: &Target,
        name: &str,
        recurse: bool,
        recursing: bool,
    ) -> Result<()> {
        let found = self.catalog.constraints_of(target.oid).find(|c| c.name == name);
        let Some(con) = found.map(|c| (c.oid, c.kind, c.validated, c.no_inherit)) else {
            return Err(Error::new(
                SqlState::UNDEFINED_OBJECT,
                format!("constraint \"{name}\" of relation \"{}\" does not exist", target.name),
            ));
        };
        let (oid, kind, validated, no_inherit) = con;
        if !matches!(kind, ConKind::Foreign | ConKind::Check | ConKind::NotNull) {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("cannot validate constraint \"{name}\" of relation \"{}\"", target.name),
            )
            .with_detail("This operation is not supported for this type of constraint."));
        }
        if validated || kind == ConKind::NotNull {
            return Ok(());
        }
        if kind == ConKind::Foreign {
            return self.validate_foreign_key(oid);
        }
        // `QueueCheckConstraintValidation` marks the constraint of the parent valid only after the constraint of each child.
        if !recursing && !no_inherit {
            for child in self.all_inheritors(target.oid) {
                if !recurse {
                    return Err(Error::new(
                        SqlState::INVALID_TABLE_DEFINITION,
                        "constraint must be validated on child tables too",
                    ));
                }
                self.validate_constraint(&child, name, false, true)?;
            }
        }
        self.catalog.set_constraint_validated(oid, true)
    }

    /// `QueueFKConstraintValidation`: marks the foreign key `oid` as validated, and each clone of it that is not valid. The session then checks the rows of each table whose constraint becomes valid and has the check triggers.
    pub(super) fn validate_foreign_key(&mut self, oid: u32) -> Result<()> {
        let clones: Vec<u32> = self
            .catalog
            .constraints()
            .filter(|c| c.parent == oid && c.kind == ConKind::Foreign && !c.validated)
            .map(|c| c.oid)
            .collect();
        for clone in clones {
            self.validate_foreign_key(clone)?;
        }
        self.catalog.set_constraint_validated(oid, true)
    }
}
