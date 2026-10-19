//! The statements of `WITH` that change data, as the `ModifyTable` nodes of the `WITH` queries and `ExecPostprocessPlan` run them.
//!
//! The statements and the main query run in one command with one snapshot, so that no part sees the changes of another part. A statement whose rows the main query or another statement reads runs before the main query, and the references read the rows of its `RETURNING`. A statement that no part reads runs after the main query, as `ExecPostprocessPlan` runs it to its end. A row that the command changed before stays as it is, so of two parts that change one row, the first part changes it.

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
