//! `ALTER TABLE ... ALTER COLUMN ... SET EXPRESSION` and `... DROP EXPRESSION`, as `ATExecSetExpression`, `ATPrepDropExpression` and `ATExecDropExpression` run them.
//!
//! `SET EXPRESSION` gives a stored generated column a new generation expression in the pass `AT_PASS_SET_EXPRESSION`. The session writes the rows of the table again, so each row gets the new value, and the check constraints on the column and on the whole row are made again, so the rows are checked against them. `DROP EXPRESSION` makes the column a plain column that keeps its values.

use rupg_catalog::ConKind;
use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{AlterTableCmd, Node};

use super::alter::Target;
use super::alter_type::Changed;
use super::table::SYSTEM_COLUMNS;
use super::{Definer, Fill, column_numbers};

/// The error for a column that the table does not have, or `cannot alter system column` for a system column.
fn no_column(column: &str, table: &str) -> Error {
    if SYSTEM_COLUMNS.contains(&column) {
        return Error::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            format!("cannot alter system column \"{column}\""),
        );
    }
    Error::new(
        SqlState::UNDEFINED_COLUMN,
        format!("column \"{column}\" of relation \"{table}\" does not exist"),
    )
}

/// The error of a subcommand on a column that is not generated.
fn not_generated(column: &str, table: &str) -> Error {
    Error::new(
        SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
        format!("column \"{column}\" of relation \"{table}\" is not a generated column"),
    )
}

impl Definer<'_, '_> {
    /// `ATExecSetExpression` for a stored generated column: the old expression goes, the new expression goes through the checks of `cookDefault`, and the check constraints that read the column or the whole row go to `changed`, to be made again. A table that is not partitioned gets its rows again with the new values.
    pub(super) fn set_expression(
        &mut self,
        target: &Target,
        column: &str,
        node: &Node,
        changed: &mut Changed,
    ) -> Result<()> {
        let table = target.oid;
        let rel = self.catalog.relation(table);
        let Some(number) = rel.and_then(|r| r.column_number(column)) else {
            return Err(no_column(column, &target.name));
        };
        let generated = rel.and_then(|r| r.column(number)).map_or(0, |c| c.generated);
        if generated == 0 {
            return Err(not_generated(column, &target.name));
        }
        // A new expression could change a row filter, and it could have what a row filter cannot have.
        if generated == b'v'
            && self.catalog.publication_rels().any(|r| r.relation == table && !r.except)
        {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "ALTER TABLE / SET EXPRESSION is not supported for virtual generated columns in tables that are part of a publication",
            )
            .with_detail(format!(
                "Column \"{column}\" of relation \"{}\" is a virtual generated column.",
                target.name
            )));
        }
        // `RememberAllDependentForRebuilding` and `RememberWholeRowDependentForRebuilding`.
        let mut checks = Vec::new();
        for con in self.catalog.constraints_of(table).filter(|c| c.kind == ConKind::Check) {
            let expr = crate::node::read(con.expr.as_deref().unwrap_or_default())?;
            let read = column_numbers(&[&expr]);
            if read.contains(&number) || read.contains(&0) {
                checks.push(con.oid);
            }
        }
        changed.add_checks(checks);
        self.catalog.remove_default(table, number);
        self.add_generation_expression(table, number, node)?;
        // `ATRewriteTables` writes the rows again, and a stored value of the row type would not get the new values.
        // A virtual generated column has no stored values, and the rows stay as they are.
        if target.kind != b'p' && generated == b's' {
            self.check_row_type_users(target)?;
            let default = self.catalog.default_of(table, number).map(|d| d.expr.clone());
            let identity = self.identity_of(table, number);
            self.added.push(Fill { table, column: number, default, identity });
        }
        Ok(())
    }

    /// `ATPrepDropExpression`: a table with children must give the subcommand to them too, and the expression of an inherited column cannot go.
    pub(super) fn prep_drop_expression(
        &self,
        target: &Target,
        cmd: &AlterTableCmd,
        recurse: bool,
        recursing: bool,
    ) -> Result<()> {
        if !recurse && !recursing && !self.catalog.children(target.oid).is_empty() {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "ALTER TABLE / DROP EXPRESSION must be applied to child tables too",
            ));
        }
        if recursing {
            return Ok(());
        }
        let column = cmd.name.as_deref().unwrap_or_default();
        let rel = self.catalog.relation(target.oid);
        let found = rel.and_then(|r| r.column(r.column_number(column)?));
        let Some(found) = found else {
            return Err(Error::new(
                SqlState::UNDEFINED_COLUMN,
                format!("column \"{column}\" of relation \"{}\" does not exist", target.name),
            ));
        };
        if found.inhcount > 0 {
            return Err(Error::new(
                SqlState::INVALID_TABLE_DEFINITION,
                "cannot drop generation expression from inherited column",
            ));
        }
        Ok(())
    }

    /// `ATExecDropExpression`: the column is not generated any more, and its expression goes. The rows keep their values. With `IF EXISTS`, a column that is not generated gives a notice.
    pub(super) fn drop_expression(
        &mut self,
        target: &Target,
        column: &str,
        missing_ok: bool,
    ) -> Result<()> {
        let table = target.oid;
        let rel = self.catalog.relation(table);
        let Some(number) = rel.and_then(|r| r.column_number(column)) else {
            return Err(no_column(column, &target.name));
        };
        if rel.and_then(|r| r.column(number)).is_none_or(|c| c.generated == 0) {
            if !missing_ok {
                return Err(not_generated(column, &target.name));
            }
            self.notice(
                SqlState::SUCCESSFUL_COMPLETION,
                format!(
                    "column \"{column}\" of relation \"{}\" is not a generated column, skipping",
                    target.name
                ),
            );
            return Ok(());
        }
        self.catalog.set_column_generated(table, number, 0)?;
        self.catalog.remove_default(table, number);
        Ok(())
    }
}
