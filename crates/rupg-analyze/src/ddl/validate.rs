//! `ALTER TABLE ... ADD CONSTRAINT ... NOT VALID` and `ALTER TABLE ... VALIDATE CONSTRAINT` of check constraints, as `ATExecValidateConstraint` and `QueueCheckConstraintValidation` run them.
//!
//! A `NOT VALID` check constraint has `convalidated` false: the new rows must satisfy it, but the rows from before it are not checked. `VALIDATE CONSTRAINT` sets `convalidated`, and the session then checks the rows of the table against the constraint, as it does for a new constraint.

use rupg_catalog::ConKind;
use rupg_common::{Error, Result, SqlState};

use super::Definer;
use super::alter::Target;

impl Definer<'_, '_> {
    /// `ATExecValidateConstraint`: marks the check constraint `name` of the table as validated, and the same constraint of each table that inherits from it first. A foreign key and a not-null constraint are always valid.
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
        if validated || kind != ConKind::Check {
            return Ok(());
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
}
