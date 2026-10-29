//! `UPDATE` of some columns of `pg_class` and `pg_database`, which the regression tests of PostgreSQL use to give the planner other row counts, to give a relation options that `ALTER TABLE` does not accept, and to give a database a large `datacl`. The engine keeps these facts outside of the heap of a catalog, so the statement does not go through the executor.

use rupg_analyze::Params;
use rupg_common::{Error, SqlState};
use rupg_sql::nodes::{ColumnRef, Node, ResTarget, SelectStmt, TypeCast, TypeName, UpdateStmt};
use rupg_types::{Value, oid};
use rupg_wire::CommandTag;

use super::{Connection, Done};
use crate::query;
use crate::utility::Notice;

/// The columns of `pg_class` that the statement can set, with the types that the new values get.
const CLASS_COLUMNS: [(&str, &[u32]); 3] =
    [("relpages", &[oid::INT4]), ("reltuples", &[oid::FLOAT4]), ("reloptions", &[oid::TEXT_ARRAY])];

/// The columns of `pg_database` that the statement can set. An `aclitem[]` value goes to its text form.
const DATABASE_COLUMNS: [(&str, &[u32]); 1] = [("datacl", &[oid::ACLITEM_ARRAY, oid::TEXT_ARRAY])];

impl Connection {
    /// `UPDATE pg_class` that sets only `relpages`, `reltuples` and `reloptions`, or `UPDATE pg_database` that sets only `datacl`: the new values of each row that the condition takes. A role that is not a superuser gets the error of the privilege check. `None` for another statement.
    pub(super) fn update_statistics(
        &self,
        node: &Node,
        notices: &mut Vec<Notice>,
    ) -> Option<Result<Done, Error>> {
        let Node::UpdateStmt(stmt) = node else { return None };
        let relation = stmt.relation.as_deref()?;
        let name = relation.relname.as_deref()?;
        let allowed: &[(&str, &[u32])] = match name {
            "pg_class" => &CLASS_COLUMNS,
            "pg_database" => &DATABASE_COLUMNS,
            _ => return None,
        };
        if relation.schemaname.as_deref().is_some_and(|schema| schema != "pg_catalog")
            || !stmt.fromClause.is_empty()
            || stmt.returningClause.is_some()
            || stmt.withClause.is_some()
        {
            return None;
        }
        let mut columns = Vec::new();
        for target in stmt.targetList.iter().flatten() {
            let Node::ResTarget(target) = target else { return None };
            let (_, types) =
                allowed.iter().find(|(column, _)| target.name.as_deref() == Some(*column))?;
            if !target.indirection.is_empty() || matches!(target.val, Some(Node::SetToDefault(_))) {
                return None;
            }
            columns.push((target.name.as_deref()?, *types, target.val.clone()?));
        }
        if !self.superuser() {
            // Only the superusers have `UPDATE` on a system catalog.
            return Some(Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("permission denied for table {name}"),
            )));
        }
        Some(self.set_catalog_values(stmt, name, &columns, notices))
    }

    /// The `SELECT` of the OID and the new values of each row that the condition takes, and the change of each row.
    fn set_catalog_values(
        &self,
        stmt: &UpdateStmt,
        catalog: &str,
        columns: &[(&str, &[u32], Node)],
        notices: &mut Vec<Notice>,
    ) -> Result<Done, Error> {
        let oid_column = ColumnRef { fields: vec![Some(Node::String("oid".into()))], location: -1 };
        let mut targets = vec![Some(
            ResTarget { val: Some(oid_column.into()), location: -1, ..ResTarget::default() }.into(),
        )];
        for (_, types, value) in columns {
            let mut value = value.clone();
            for ty in *types {
                value = TypeCast {
                    arg: Some(value),
                    typeName: Some(Box::new(TypeName { typeOid: *ty, ..TypeName::default() })),
                    location: -1,
                }
                .into();
            }
            targets.push(Some(
                ResTarget { val: Some(value), location: -1, ..ResTarget::default() }.into(),
            ));
        }
        let select = SelectStmt {
            targetList: targets,
            fromClause: vec![stmt.relation.clone().map(|relation| (*relation).into())],
            whereClause: stmt.whereClause.clone(),
            ..SelectStmt::default()
        };
        self.settings.borrow_mut().take_snapshot();
        let reader = self.reader();
        let plan = query::plan(&Node::SelectStmt(Box::new(select)), &reader, &Params::default());
        notices.extend(reader.take_warnings().into_iter().map(Notice::of_function));
        let plan = plan?.fold(None, &reader)?;
        plan.start(&reader)?;
        let rows = plan.run(&[], &reader);
        notices.extend(reader.take_warnings().into_iter().map(Notice::of_function));
        let rows = rows?;
        drop(plan);
        drop(reader);
        for row in &rows {
            let Some(Value::Oid(object)) = row.first() else {
                return Err(Error::internal(format!("{catalog} has a row with no OID")));
            };
            let values = columns.iter().map(|(name, _, _)| *name).zip(&row[1..]);
            if catalog == "pg_database" {
                self.set_database_values(*object, values)?;
            } else {
                self.set_class_values(*object, values)?;
            }
        }
        Ok(Done::Count(CommandTag::Update, rows.len() as u64))
    }

    /// `relpages` and `reltuples` as `vac_update_relstats` sets them, and `reloptions` with no check of the options.
    fn set_class_values<'a>(
        &self,
        relation: u32,
        values: impl Iterator<Item = (&'a str, &'a Value)>,
    ) -> Result<(), Error> {
        let heap = self.heap();
        let size = heap.size(self.database_oid, relation);
        let (mut relpages, mut reltuples) = (size.relpages, size.reltuples);
        let mut options = None;
        for (name, value) in values {
            match (name, value) {
                ("relpages", Value::Int4(pages)) => relpages = *pages,
                ("reltuples", Value::Float4(tuples)) => reltuples = *tuples,
                ("reloptions", value) => options = Some(texts(value)),
                _ => return Err(not_null(name, "pg_class")),
            }
        }
        heap.set_stats(self.database_oid, relation, relpages, reltuples);
        if let Some(options) = options {
            self.change_catalog(&mut |catalog| catalog.set_options(relation, options.clone()))?;
        }
        Ok(())
    }

    /// `datacl` of a database in the changes of the transaction, so a rollback discards it.
    fn set_database_values<'a>(
        &self,
        database: u32,
        values: impl Iterator<Item = (&'a str, &'a Value)>,
    ) -> Result<(), Error> {
        let Some(mut row) = self.database_by_oid(database) else {
            return Err(Error::internal(format!("pg_database has no row with the OID {database}")));
        };
        for (_, value) in values {
            row.acl = match value {
                Value::Null => None,
                value => Some(texts(value)),
            };
        }
        self.change_catalog(&mut |_| Ok(()))?;
        self.change_database(&row)
    }
}

/// The texts of the items of a `text[]` value. A null value or a null item gives no texts.
fn texts(value: &Value) -> Vec<String> {
    let Value::Array(array) = value else { return Vec::new() };
    array
        .values
        .iter()
        .filter_map(|item| match item {
            Some(Value::Text(text)) => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// The error of a null value in a column that is not null.
fn not_null(name: &str, catalog: &str) -> Error {
    Error::new(
        SqlState::NOT_NULL_VIOLATION,
        format!(
            "null value in column \"{name}\" of relation \"{catalog}\" violates not-null constraint"
        ),
    )
}
