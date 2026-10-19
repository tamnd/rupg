//! `ALTER TABLE ... DROP CONSTRAINT` and `ALTER COLUMN ... DROP NOT NULL`, as `ATExecDropConstraint`, `ATExecDropNotNull` and `dropconstraint_internal` run them in the pass of the drops.

use rupg_catalog::{ConKind, ObjRef, PG_CONSTRAINT};
use rupg_common::{Error, Result, SqlState};

use super::Definer;
use super::alter::Target;
use crate::from::system_column;

impl Definer<'_, '_> {
    /// `ATExecDropConstraint`: finds the constraint of the table by its name, then removes it and the objects that depend on it.
    pub(super) fn drop_constraint(
        &mut self,
        target: &Target,
        name: &str,
        missing_ok: bool,
        cascade: bool,
    ) -> Result<()> {
        let found = self.catalog.constraints_of(target.oid).find(|c| c.name == name);
        let Some((oid, kind, keys)) = found.map(|c| (c.oid, c.kind, c.keys.clone())) else {
            let message =
                format!("constraint \"{name}\" of relation \"{}\" does not exist", target.name);
            if !missing_ok {
                return Err(Error::new(SqlState::UNDEFINED_OBJECT, message));
            }
            self.notice(SqlState::SUCCESSFUL_COMPLETION, format!("{message}, skipping"));
            return Ok(());
        };
        if kind == ConKind::NotNull
            && let [number] = keys[..]
        {
            self.check_drop_not_null(target, number)?;
        }
        self.perform(&[ObjRef::new(PG_CONSTRAINT, oid)], cascade)
    }

    /// `ATExecDropNotNull`: removes the not-null constraint of the column. A column with no constraint needs no change.
    pub(super) fn drop_not_null(&mut self, target: &Target, column: &str) -> Result<()> {
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
        self.check_drop_not_null(target, number)?;
        let found = self
            .catalog
            .constraints_of(target.oid)
            .find(|c| c.kind == ConKind::NotNull && c.keys == [number])
            .map(|c| c.oid);
        match found {
            Some(oid) => self.perform(&[ObjRef::new(PG_CONSTRAINT, oid)], false),
            None => Err(Error::internal(format!(
                "could not find not-null constraint on column \"{column}\", relation \"{}\"",
                target.name
            ))),
        }
    }

    /// The checks of `dropconstraint_internal` for a not-null constraint: a column of the primary key and an identity column keep their constraint.
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
