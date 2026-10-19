//! `ALTER TABLE ... ALTER COLUMN` with `ADD GENERATED ... AS IDENTITY`, `SET GENERATED`, the `SET` options of the sequence and `DROP IDENTITY`, as `ATExecAddIdentity`, `ATExecSetIdentity` and `ATExecDropIdentity` run them after the transform of `transformAlterTableStmt`.

use rupg_catalog::{DepKind, ObjRef, PG_CLASS};
use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{AlterSeqStmt, Constraint, DefElem, Node, RangeVar};

use super::Definer;
use super::alter::Target;
use crate::from::system_column;

/// The error for a column that the table does not have.
fn no_column(column: &str, table: &str) -> Error {
    Error::new(
        SqlState::UNDEFINED_COLUMN,
        format!("column \"{column}\" of relation \"{table}\" does not exist"),
    )
}

/// The error of `ATExecAddIdentity`, `ATExecSetIdentity` and `ATExecDropIdentity` for a system column.
fn system(column: &str) -> Error {
    Error::new(SqlState::FEATURE_NOT_SUPPORTED, format!("cannot alter system column \"{column}\""))
}

/// The error for a column that has no identity.
fn not_identity(column: &str, table: &str) -> Error {
    Error::new(
        SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
        format!("column \"{column}\" of relation \"{table}\" is not an identity column"),
    )
}

impl Definer<'_, '_> {
    /// `get_attnum` and `get_atttype`: the number and the type of a column of the table. A system column has a negative number.
    fn identity_column(&self, target: &Target, column: &str) -> Result<(i16, u32)> {
        let rel = self.catalog.relation(target.oid);
        if let Some(number) = rel.and_then(|r| r.column_number(column)) {
            let ty = rel.and_then(|r| r.column(number)).map_or(0, |c| c.ty);
            return Ok((number, ty));
        }
        system_column(column).ok_or_else(|| no_column(column, &target.name))
    }

    /// `ADD GENERATED ... AS IDENTITY`. The transform makes the sequence with the type of the column and gives it to the column at once. Then `ATExecAddIdentity` checks the column.
    pub(super) fn add_identity(
        &mut self,
        target: &Target,
        column: &str,
        con: &Constraint,
    ) -> Result<()> {
        let (number, ty) = self.identity_column(target, column)?;
        let sequence =
            self.identity_sequence(&target.name, target.namespace, column, ty, &con.options)?;
        let info = self.identity_sequence_info(&sequence.options)?;
        let oid = self.new_sequence(sequence.namespace, &sequence.name, info)?;
        let table = (target.oid, target.namespace, target.name.as_str());
        self.own_identity_sequence(oid, sequence.namespace, table, number)?;
        let Some(attr) = self.catalog.relation(target.oid).and_then(|r| r.column(number)) else {
            return Err(system(column));
        };
        let prerequisite = |message: String| {
            Error::new(
                SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                format!("column \"{column}\" of relation \"{}\" {message}", target.name),
            )
        };
        if !attr.not_null {
            return Err(prerequisite(
                "must be declared NOT NULL before identity can be added".into(),
            ));
        }
        if attr.identity != 0 {
            return Err(prerequisite("is already an identity column".into()));
        }
        if attr.has_default {
            return Err(prerequisite("already has a default value".into()));
        }
        self.catalog.set_identity(target.oid, number, con.generated_when)
    }

    /// The `SET` options of an identity column. The transform gives the options of the sequence to `ALTER SEQUENCE` first. Then `ATExecSetIdentity` checks the column and changes `SET GENERATED`.
    pub(super) fn set_identity(
        &mut self,
        target: &Target,
        column: &str,
        options: &[Option<Node>],
    ) -> Result<()> {
        let (generated, rest): (Vec<_>, Vec<_>) = options.iter().cloned().partition(|option| {
            matches!(option, Some(Node::DefElem(def)) if def.defname.as_deref() == Some("generated"))
        });
        let (number, _) = self.identity_column(target, column)?;
        if number <= 0 {
            return Err(system(column));
        }
        if !rest.is_empty() {
            let Some(sequence) = self.catalog.identity_sequence(target.oid, number) else {
                return Err(not_identity(column, &target.name));
            };
            let (namespace, name) = self
                .catalog
                .relation(sequence)
                .map(|r| (r.namespace, r.name.clone()))
                .unwrap_or_default();
            let rv = RangeVar {
                schemaname: Some(self.schema_name(namespace).into()),
                relname: Some(name.into()),
                inh: true,
                location: -1,
                ..RangeVar::default()
            };
            let stmt = AlterSeqStmt {
                sequence: Some(Box::new(rv)),
                options: rest,
                for_identity: true,
                missing_ok: false,
            };
            self.alter_sequence(&stmt)?;
        }
        let mut when: Option<&DefElem> = None;
        for option in &generated {
            let Some(Node::DefElem(def)) = option else { continue };
            if when.is_some() {
                return Err(Error::new(SqlState::SYNTAX_ERROR, "conflicting or redundant options"));
            }
            when = Some(def);
        }
        let identity = self
            .catalog
            .relation(target.oid)
            .and_then(|r| r.column(number))
            .map_or(0, |c| c.identity);
        if identity == 0 {
            return Err(not_identity(column, &target.name));
        }
        if let Some(def) = when {
            let value = match &def.arg {
                Some(Node::Integer(value)) => u8::try_from(*value).unwrap_or(identity),
                _ => identity,
            };
            self.catalog.set_identity(target.oid, number, value)?;
        }
        Ok(())
    }

    /// `ATExecDropIdentity`: the column loses its identity, and its sequence goes. The column keeps its not-null constraint.
    pub(super) fn drop_identity(
        &mut self,
        target: &Target,
        column: &str,
        missing_ok: bool,
    ) -> Result<()> {
        let (number, _) = self.identity_column(target, column)?;
        if number <= 0 {
            return Err(system(column));
        }
        let identity = self
            .catalog
            .relation(target.oid)
            .and_then(|r| r.column(number))
            .map_or(0, |c| c.identity);
        if identity == 0 {
            let error = not_identity(column, &target.name);
            if !missing_ok {
                return Err(error);
            }
            self.notice(SqlState::SUCCESSFUL_COMPLETION, format!("{}, skipping", error.message()));
            return Ok(());
        }
        self.catalog.set_identity(target.oid, number, 0)?;
        let Some(sequence) = self.catalog.identity_sequence(target.oid, number) else {
            return Ok(());
        };
        self.catalog.set_owned_by(sequence, None, DepKind::Internal);
        self.perform(&[ObjRef::new(PG_CLASS, sequence)], false)
    }
}
