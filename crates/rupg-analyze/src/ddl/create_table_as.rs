//! `CREATE TABLE AS`, `SELECT INTO` and `CREATE MATERIALIZED VIEW`, as `transformCreateTableAsStmt`, `ExecCreateTableAs` and `create_ctas_nodata` of `createas.c` run them. The statement makes the table here, and the caller then fills it with the rows of the query, as `intorel_receive` writes them.

use rupg_catalog::{ObjRef, PG_CLASS};
use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{CreateTableAsStmt, IntoClause, Node, ObjectType, SelectStmt};

use super::view::query_references;
use super::{Definer, not_yet, relname};
use crate::collate::target_collation;
use crate::typename::{names, place};
use crate::types;

impl Definer<'_, '_> {
    /// `ExecCreateTableAs` up to `create_ctas_internal`: a table with a column for each column of the query. The result is the OID of the new table, or `None` when the table exists and the statement has `IF NOT EXISTS` or the statement has `WITH NO DATA`. `text` is the text of the statement, which the catalog keeps for a materialized view.
    pub(super) fn create_table_as(
        &mut self,
        stmt: &CreateTableAsStmt,
        text: &str,
    ) -> Result<Option<u32>> {
        let matview = stmt.objtype == ObjectType::OBJECT_MATVIEW;
        if matview {
            let rel = stmt.into.as_deref().and_then(|i| i.rel.as_deref());
            if rel.is_some_and(|rv| rv.relpersistence == b'u') {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    "materialized views cannot be unlogged",
                ));
            }
        }
        let Some(into) = stmt.into.as_deref() else {
            return Err(Error::internal("CREATE TABLE AS with no INTO clause"));
        };
        match stmt.query.as_ref() {
            Some(Node::SelectStmt(select)) => {
                self.query_table(select, into, stmt.if_not_exists, matview.then_some(text))
            }
            _ => Err(not_yet("CREATE TABLE AS EXECUTE", None)),
        }
    }

    /// `SELECT ... INTO`, which the grammar of PostgreSQL makes a `CREATE TABLE AS` with the same query and no `INTO`.
    pub(super) fn select_into(&mut self, stmt: &SelectStmt) -> Result<Option<u32>> {
        let Some(into) = crate::select_into(stmt) else {
            return Err(Error::internal("SELECT INTO with no INTO clause"));
        };
        let query = crate::without_into(stmt);
        self.query_table(&query, into, false, None)
    }

    /// The table of `CREATE TABLE AS` with the query `select` and the target `into`. `matview` is the text of `CREATE MATERIALIZED VIEW`, and then the table becomes a materialized view.
    fn query_table(
        &mut self,
        select: &SelectStmt,
        into: &IntoClause,
        if_not_exists: bool,
        matview: Option<&str>,
    ) -> Result<Option<u32>> {
        let query = self.an.select(select)?;
        if query.ctes.iter().any(|c| c.write.is_some()) {
            if matview.is_some() {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    "materialized views must not use data-modifying statements in WITH",
                ));
            }
            return Err(not_yet("CREATE TABLE AS with a data-modifying statement in WITH", None));
        }
        let mut refs = Vec::new();
        if matview.is_some() {
            query_references(self.catalog, &query, &[], &mut refs);
            self.check_usage_on_types(&refs)?;
            // `query_uses_temp_object`.
            let temp = refs.iter().copied().find(|r| {
                r.class == PG_CLASS
                    && self
                        .catalog
                        .relation(r.oid)
                        .is_some_and(|rel| self.catalog.is_temp_schema(rel.namespace))
            });
            if let Some(temp) = temp {
                let temp = ObjRef { sub: 0, ..temp };
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    "materialized views must not use temporary objects",
                )
                .with_detail(format!("This view depends on temporary {}.", self.describe(temp))));
            }
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
        self.check_on_commit(into.onCommit, namespace)?;
        let table = self.create_query_table(namespace, &name, columns, &into.options)?;
        self.register_on_commit(table, into.onCommit);
        if rv.relpersistence == b'u' {
            self.catalog.set_unlogged(table, true)?;
        }
        if let Some(text) = matview {
            let path = self.an.path.clone();
            self.catalog.make_matview(table, text.to_string(), path, &refs, !into.skipData)?;
        }
        Ok((!into.skipData).then_some(table))
    }
}
