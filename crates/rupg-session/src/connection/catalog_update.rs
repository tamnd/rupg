//! `UPDATE pg_class` of the statistics of a relation, `relpages` and `reltuples`, which the regression tests of PostgreSQL use to give the planner other row counts. The engine keeps the statistics outside of the catalog, so the statement does not go through the executor.

use rupg_analyze::Params;
use rupg_common::{Error, SqlState};
use rupg_sql::nodes::{ColumnRef, Node, ResTarget, SelectStmt, TypeCast, TypeName, UpdateStmt};
use rupg_types::{Value, oid};
use rupg_wire::CommandTag;

use super::{Connection, Done};
use crate::query;
use crate::utility::Notice;

/// The columns of `pg_class` that the statement can set, with their types.
const COLUMNS: [(&str, u32); 2] = [("relpages", oid::INT4), ("reltuples", oid::FLOAT4)];

impl Connection {
    /// `UPDATE pg_class` that sets only `relpages` and `reltuples`: the statistics of each relation that the condition takes, as `vac_update_relstats` sets them. A role that is not a superuser gets the error of the privilege check for any `UPDATE pg_class`. `None` for another statement.
    pub(super) fn update_statistics(
        &self,
        node: &Node,
        notices: &mut Vec<Notice>,
    ) -> Option<Result<Done, Error>> {
        let Node::UpdateStmt(stmt) = node else { return None };
        let relation = stmt.relation.as_deref()?;
        if relation.relname.as_deref() != Some("pg_class")
            || relation.schemaname.as_deref().is_some_and(|schema| schema != "pg_catalog")
            || !stmt.fromClause.is_empty()
            || stmt.returningClause.is_some()
            || stmt.withClause.is_some()
        {
            return None;
        }
        if !self.superuser() {
            // Only the superusers have `UPDATE` on `pg_class`.
            return Some(Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                "permission denied for table pg_class",
            )));
        }
        let mut columns = Vec::new();
        for target in stmt.targetList.iter().flatten() {
            let Node::ResTarget(target) = target else { return None };
            let (_, ty) = COLUMNS.iter().find(|(name, _)| target.name.as_deref() == Some(*name))?;
            if !target.indirection.is_empty() || matches!(target.val, Some(Node::SetToDefault(_))) {
                return None;
            }
            columns.push((target.name.as_deref()?, *ty, target.val.clone()?));
        }
        Some(self.set_statistics(stmt, &columns, notices))
    }

    /// The `SELECT` of the OID and the new values of each row of `pg_class` that the condition takes, and the statistics of the relations.
    fn set_statistics(
        &self,
        stmt: &UpdateStmt,
        columns: &[(&str, u32, Node)],
        notices: &mut Vec<Notice>,
    ) -> Result<Done, Error> {
        let oid_column = ColumnRef { fields: vec![Some(Node::String("oid".into()))], location: -1 };
        let mut targets = vec![Some(
            ResTarget { val: Some(oid_column.into()), location: -1, ..ResTarget::default() }.into(),
        )];
        for (_, ty, value) in columns {
            let cast = TypeCast {
                arg: Some(value.clone()),
                typeName: Some(Box::new(TypeName { typeOid: *ty, ..TypeName::default() })),
                location: -1,
            };
            targets.push(Some(
                ResTarget { val: Some(cast.into()), location: -1, ..ResTarget::default() }.into(),
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
        let heap = self.heap();
        for row in &rows {
            let Some(Value::Oid(relation)) = row.first() else {
                return Err(Error::internal("pg_class has a row with no OID"));
            };
            let size = heap.size(self.database_oid, *relation);
            let (mut relpages, mut reltuples) = (size.relpages, size.reltuples);
            for ((name, _, _), value) in columns.iter().zip(&row[1..]) {
                match (*name, value) {
                    ("relpages", Value::Int4(pages)) => relpages = *pages,
                    ("reltuples", Value::Float4(tuples)) => reltuples = *tuples,
                    _ => {
                        return Err(Error::new(
                            SqlState::NOT_NULL_VIOLATION,
                            format!(
                                "null value in column \"{name}\" of relation \"pg_class\" violates not-null constraint"
                            ),
                        ));
                    }
                }
            }
            heap.set_stats(self.database_oid, *relation, relpages, reltuples);
        }
        Ok(Done::Count(CommandTag::Update, rows.len() as u64))
    }
}
