//! `ALTER TABLE ... ADD CONSTRAINT ... NOT VALID` and `ALTER TABLE ... VALIDATE CONSTRAINT` of check constraints, not-null constraints and foreign keys, as `ATExecValidateConstraint` and `QueueCheckConstraintValidation` run them.
//!
//! A `NOT VALID` check constraint has `convalidated` false: the new rows must satisfy it, but the rows from before it are not checked. `VALIDATE CONSTRAINT` sets `convalidated`, and the session then checks the rows of the table against the constraint, as it does for a new constraint.

use rupg_catalog::ConKind;
use rupg_common::{Error, Result, SqlState};

use super::Definer;
use super::alter::Target;

impl Definer<'_, '_> {
    /// `ATExecValidateConstraint`: marks the check constraint or the not-null constraint `name` of the table as validated, and the same constraint of each table that inherits from it first. For a foreign key, the clones become valid with it.
    pub(super) fn validate_constraint(
        &mut self,
        target: &Target,
        name: &str,
        recurse: bool,
        recursing: bool,
    ) -> Result<()> {
        let found = self.catalog.constraints_of(target.oid).find(|c| c.name == name);
        let Some(con) =
            found.map(|c| (c.oid, c.kind, (c.validated, c.enforced), c.no_inherit, c.keys.clone()))
        else {
            return Err(Error::new(
                SqlState::UNDEFINED_OBJECT,
                format!("constraint \"{name}\" of relation \"{}\" does not exist", target.name),
            ));
        };
        let (oid, kind, (validated, enforced), no_inherit, keys) = con;
        if !matches!(kind, ConKind::Foreign | ConKind::Check | ConKind::NotNull) {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("cannot validate constraint \"{name}\" of relation \"{}\"", target.name),
            )
            .with_detail("This operation is not supported for this type of constraint."));
        }
        if !enforced {
            return Err(Error::new(
                SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                "cannot validate NOT ENFORCED constraint",
            ));
        }
        if validated {
            return Ok(());
        }
        if kind == ConKind::Foreign {
            return self.validate_foreign_key(oid);
        }
        // `QueueCheckConstraintValidation` and `QueueNNConstraintValidation` mark the constraint of the parent valid only after the constraint of each child. The not-null constraint of a child is the one of the column with the same name.
        let column = keys.first().and_then(|&k| self.catalog.relation(target.oid)?.column(k));
        let column = column.map(|c| c.name.clone());
        if !recursing && !no_inherit {
            for child in self.all_inheritors(target.oid) {
                if !recurse {
                    return Err(Error::new(
                        SqlState::INVALID_TABLE_DEFINITION,
                        "constraint must be validated on child tables too",
                    ));
                }
                let child_name = match (kind, column.as_deref()) {
                    (ConKind::NotNull, Some(column)) => self.child_not_null(child.oid, column),
                    _ => Some(name.to_string()),
                };
                if let Some(child_name) = child_name {
                    self.validate_constraint(&child, &child_name, false, true)?;
                }
            }
        }
        self.catalog.set_constraint_validated(oid, true)
    }

    /// The name of the not-null constraint of the column `column` of the table `table`, as `findNotNullConstraint` finds it.
    fn child_not_null(&self, table: u32, column: &str) -> Option<String> {
        let number = self.catalog.relation(table)?.column_number(column)?;
        let found = self
            .catalog
            .constraints_of(table)
            .find(|c| c.kind == ConKind::NotNull && c.keys == [number]);
        found.map(|c| c.name.clone())
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
