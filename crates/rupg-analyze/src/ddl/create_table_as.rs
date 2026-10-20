//! `CREATE TABLE AS` and `SELECT INTO`, as `transformCreateTableAsStmt`, `ExecCreateTableAs` and `create_ctas_nodata` of `createas.c` run them. The statement makes the table here, and the caller then fills it with the rows of the query, as `intorel_receive` writes them.

use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{
    CreateTableAsStmt, IntoClause, Node, ObjectType, OnCommitAction, SelectStmt,
};

use super::{Definer, not_yet, relname};
use crate::collate::target_collation;
use crate::typename::{names, place};
use crate::types;

impl Definer<'_, '_> {
    /// `ExecCreateTableAs` up to `create_ctas_internal`: a table with a column for each column of the query. The result is the OID of the new table, or `None` when the table exists and the statement has `IF NOT EXISTS`.
    pub(super) fn create_table_as(&mut self, stmt: &CreateTableAsStmt) -> Result<Option<u32>> {
        if stmt.objtype == ObjectType::OBJECT_MATVIEW {
            return Err(not_yet("CREATE MATERIALIZED VIEW", None));
        }
        let Some(into) = stmt.into.as_deref() else {
            return Err(Error::internal("CREATE TABLE AS with no INTO clause"));
        };
        match stmt.query.as_ref() {
            Some(Node::SelectStmt(select)) => self.query_table(select, into, stmt.if_not_exists),
            _ => Err(not_yet("CREATE TABLE AS EXECUTE", None)),
        }
    }

    /// `SELECT ... INTO`, which the grammar of PostgreSQL makes a `CREATE TABLE AS` with the same query and no `INTO`.
    pub(super) fn select_into(&mut self, stmt: &SelectStmt) -> Result<Option<u32>> {
        let Some(into) = stmt.intoClause.as_deref() else {
            return Err(Error::internal("SELECT INTO with no INTO clause"));
        };
        let query = SelectStmt { intoClause: None, ..stmt.clone() };
        self.query_table(&query, into, false)
    }

    /// The table of `CREATE TABLE AS` with the query `select` and the target `into`.
    fn query_table(
        &mut self,
        select: &SelectStmt,
        into: &IntoClause,
        if_not_exists: bool,
    ) -> Result<Option<u32>> {
        let query = self.an.select(select)?;
        if query.ctes.iter().any(|c| c.write.is_some()) {
            return Err(not_yet("CREATE TABLE AS with a data-modifying statement in WITH", None));
        }
        let rv = into.rel.as_deref().cloned().unwrap_or_default();
        let at = place(rv.location);
        // `CreateTableAsRelExists` looks up the schema with no parse state, so its errors have no position.
        let namespace = self.creation_namespace(&rv).map_err(|e| e.with_position(None))?;
        let name = relname(&rv).to_string();
        if if_not_exists && self.relation_in(namespace, &name).is_some() {
            self.notice(
                SqlState::DUPLICATE_TABLE,
                format!("relation \"{name}\" already exists, skipping"),
            );
            return Ok(None);
        }
        let targets: Vec<_> = query.targets.iter().filter(|t| !t.junk).collect();
        let aliases = names(&into.colNames);
        if aliases.len() > targets.len() {
            return Err(Error::new(SqlState::SYNTAX_ERROR, "too many column names were specified"));
        }
        let mut columns = Vec::with_capacity(targets.len());
        for (i, target) in targets.iter().enumerate() {
            let column = aliases.get(i).copied().unwrap_or(target.name.as_str());
            let expr = &target.expr;
            let collation = target_collation(expr, &query, Some(&*self.catalog));
            if types::collatable(expr.ty) && collation == 0 {
                return Err(Error::new(
                    SqlState::INDETERMINATE_COLLATION,
                    format!(
                        "no collation was derived for column \"{column}\" with collatable type {}",
                        types::name(expr.ty)
                    ),
                )
                .with_hint("Use the COLLATE clause to set the collation explicitly."));
            }
            columns.push((column.to_string(), expr.ty, expr.typmod, collation));
        }
        if into.tableSpaceName.is_some() {
            return Err(not_yet("TABLESPACE", at));
        }
        if into.accessMethod.is_some() {
            return Err(not_yet("USING", at));
        }
        if into.onCommit != OnCommitAction::ONCOMMIT_NOOP {
            return Err(Error::new(
                SqlState::INVALID_TABLE_DEFINITION,
                "ON COMMIT can only be used on temporary tables",
            ));
        }
        let table = self.create_query_table(namespace, &name, columns, &into.options)?;
        Ok((!into.skipData).then_some(table))
    }
}
