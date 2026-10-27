//! The statements of `WITH` that change data, as the `ModifyTable` nodes of the `WITH` queries and `ExecPostprocessPlan` run them.
//!
//! The statements and the main query run in one command with one snapshot, so that no part sees the changes of another part. A statement whose rows the main query or another statement reads runs before the main query, and the references read the rows of its `RETURNING`. A statement that no part reads runs after the main query, as `ExecPostprocessPlan` runs it to its end. A row that the command changed before stays as it is, so of two parts that change one row, the first part changes it.

use rupg_analyze::{MergeCommand, Write, WriteKind};
use rupg_common::Error;
use rupg_exec::Plan;
use rupg_types::Value;

use super::foreign::Batch;
use super::{Applied, Connection};
use crate::heap::View;

/// The state of the statements of `WITH` that change data, from the start of the main query to its end.
#[derive(Debug)]
pub(super) struct Writes {
    /// The statements that no part reads, which run after the main query.
    after: Vec<Plan>,
    /// The rows of `RETURNING` of each statement, by its number. A statement that runs after the main query has no rows here.
    written: Vec<Vec<Vec<Value>>>,
    /// The rows of the writes for the triggers of the foreign keys, which fire at the end of the statement.
    batches: Vec<Batch>,
    /// The view of the statement.
    view: View,
}

impl Connection {
    /// Runs the statements of `WITH` that change data whose rows a part of the statement reads, and gives the plan of the main query with their rows. A plan with no such statement does not change.
    ///
    /// # Errors
    ///
    /// The errors of the statements, such as a duplicate key.
    pub(super) fn start_writes(
        &self,
        plan: Plan,
        params: &[Value],
        view: &View,
    ) -> Result<(Plan, Writes), Error> {
        self.start_triggers();
        let mut writes = Writes {
            after: Vec::new(),
            written: Vec::new(),
            batches: Vec::new(),
            view: view.clone(),
        };
        let list = plan.cte_writes();
        if list.is_empty() {
            return Ok((plan, writes));
        }
        for (cte, read) in list {
            if read {
                let cte = cte.with_written(writes.written.clone());
                let rows = self.write_cte(&cte, params, view, &mut writes.batches)?;
                writes.written.push(rows);
            } else {
                writes.after.push(cte);
                writes.written.push(Vec::new());
            }
        }
        Ok((plan.with_written(writes.written.clone()), writes))
    }

    /// Runs the statements of `WITH` that change data that no part reads, then fires the triggers of the foreign keys for the rows of all the statements. The batches `main` are the rows of the main query, whose triggers fire after the triggers of the statements that ran before it.
    ///
    /// # Errors
    ///
    /// The errors of the statements and of the triggers.
    pub(super) fn end_writes(
        &self,
        writes: Writes,
        params: &[Value],
        main: Vec<Batch>,
    ) -> Result<(), Error> {
        let Writes { after, written, mut batches, view } = writes;
        batches.extend(main);
        for cte in after {
            let cte = cte.with_written(written.clone());
            self.write_cte(&cte, params, &view, &mut batches)?;
        }
        if !batches.is_empty() {
            self.fire(batches)?;
        }
        self.recheck_unique(view.command)?;
        self.fire_events(0)
    }

    /// `CheckValidResultRel` for each result relation of the write, as `ExecInitModifyTable` opens them: the target table, and each child table that the plan keeps. Each one checks its replica identity for the actions of the write.
    ///
    /// # Errors
    ///
    /// The errors of `CheckCmdReplicaIdentity`.
    pub(super) fn check_result_relations(&self, info: &Write) -> Result<(), Error> {
        let mut actions = Vec::new();
        match info.kind {
            WriteKind::Update | WriteKind::Delete => actions.push(info.kind),
            WriteKind::Merge => {
                for clause in info.merge.iter().flat_map(|m| &m.clauses) {
                    match clause.command {
                        MergeCommand::Update => actions.push(WriteKind::Update),
                        MergeCommand::Delete => actions.push(WriteKind::Delete),
                        _ => {}
                    }
                }
            }
            // `INSERT ... ON CONFLICT DO UPDATE` also checks `UPDATE`.
            WriteKind::Insert => {
                if info.on_conflict.as_ref().is_some_and(|c| c.update.is_some()) {
                    actions.push(WriteKind::Update);
                }
            }
        }
        if actions.is_empty() || info.instead {
            return Ok(());
        }
        let catalog = self.catalog();
        let mut tables = vec![info.table];
        if !info.pruned && info.kind != WriteKind::Insert {
            tables.extend(catalog.all_inheritors(info.table));
        }
        for table in tables {
            for &action in &actions {
                rupg_analyze::check_cmd_replica_identity(&catalog, table, action)?;
            }
        }
        Ok(())
    }

    /// Runs one statement of `WITH` that changes data in the command of the view, and gives the rows of its `RETURNING`.
    fn write_cte(
        &self,
        plan: &Plan,
        params: &[Value],
        view: &View,
        batches: &mut Vec<Batch>,
    ) -> Result<Vec<Vec<Value>>, Error> {
        let reader = self.reader().with_view(view.clone());
        let rows = plan.run(params, &reader)?;
        drop(reader);
        let Applied { written, view, batches: more, .. } =
            self.apply_write(plan, params, view, rows)?;
        batches.extend(more);
        // A subquery of `RETURNING` reads with the snapshot of the statement.
        let reader = self.reader().with_view(view);
        plan.returning(written, params, &reader)
    }
}
