//! `INSERT ... ON CONFLICT`, as `ExecInsert` runs it with a speculative insertion and `ExecOnConflictUpdate` runs `DO UPDATE`. Each row goes in, or it meets a row of the table with its key in an arbiter index. `DO NOTHING` then skips the row, and `DO UPDATE` changes the row of the table.
//!
//! The query of the statement gives all the new rows before the first row goes in, so the checks of the constraints of the new rows come before the writes, and a function of the query does not see the rows that the statement wrote. The triggers `BEFORE ... FOR EACH ROW` fire for each row in turn, as `ExecInsert` fires them: the triggers `INSERT` before the row goes in, and the triggers `UPDATE` before `DO UPDATE` changes the row of the table.

use rupg_analyze::{OnConflict, Write};
use rupg_common::{Error, SqlState};
use rupg_exec::Plan;
use rupg_types::Value;

use super::Connection;
use super::trigger::Triggers;
use crate::heap::{Changed, Speculative, View, tid_text};

/// The rows that `INSERT ... ON CONFLICT` wrote.
#[derive(Debug, Default)]
pub(super) struct Upserted {
    /// The number of rows that went in or changed.
    pub(super) count: u64,
    /// The rows of `RETURNING`, in the order of the write. Each row has the columns of the table, then the carried values, which are null for a row that went in.
    pub(super) written: Vec<Vec<Value>>,
    /// The rows that went in, for the foreign keys of an insert.
    pub(super) inserted: Vec<Vec<Value>>,
    /// The new rows of the rows that changed, for the foreign keys of an update.
    pub(super) new: Vec<Vec<Value>>,
    /// The old rows of the rows that changed, in the same order.
    pub(super) old: Vec<Vec<Value>>,
    /// For each row of [`Upserted::inserted`] and [`Upserted::new`] in the order of the write, true when the row went in and false when it changed, for the triggers `AFTER ... FOR EACH ROW`.
    pub(super) steps: Vec<bool>,
}

/// The triggers of `INSERT ... ON CONFLICT`: the triggers `INSERT`, and the triggers `UPDATE` of `DO UPDATE`.
#[derive(Default)]
pub(super) struct Upsert {
    pub(super) insert: Triggers,
    pub(super) update: Triggers,
}

impl Upsert {
    /// True when a trigger `AFTER` gets the rows of the write.
    pub(super) fn after_rows(&self) -> bool {
        self.insert.after_rows() || self.update.after_rows()
    }
}

impl Connection {
    /// Writes the rows of `INSERT ... ON CONFLICT` one by one, in the command of the view. A row that meets a version that a transaction in progress made or deleted waits for the end of that transaction, and then the row tries again.
    ///
    /// # Errors
    ///
    /// `21000` for `DO UPDATE` of a row that the statement wrote, `40001` in `REPEATABLE READ` for a row that the snapshot does not see, `23505` for a key of an index that is not an arbiter, and the errors of `SET` and `WHERE` and of the constraints of the new row of `DO UPDATE`.
    pub(super) fn upsert(
        &self,
        plan: &Plan,
        (info, conflict): (&Write, &OnConflict),
        params: &[Value],
        view: &View,
        rows: Vec<Vec<Value>>,
        fire: &Upsert,
    ) -> Result<Upserted, Error> {
        let heap = self.store.heap();
        let mut done = Upserted::default();
        let keep = fire.after_rows();
        let before = !fire.insert.is_empty() || info.recheck.is_some();
        for row in rows {
            let row = if before {
                match self.before_rows(plan, info, &fire.insert, vec![row])?.pop() {
                    Some(row) => row,
                    None => continue,
                }
            } else {
                row
            };
            let mut kept = None;
            loop {
                let result = heap.speculative(
                    info.table,
                    view,
                    row.clone(),
                    &info.unique,
                    &conflict.arbiters,
                    kept,
                )?;
                match result {
                    Speculative::Inserted => {
                        done.count += 1;
                        if info.returning.is_some() {
                            let mut written = row.clone();
                            written.resize(row.len() + conflict.carried, Value::Null);
                            done.written.push(written);
                        }
                        if keep || !info.foreign.is_empty() {
                            done.inserted.push(row);
                        }
                        if keep {
                            done.steps.push(true);
                        }
                        break;
                    }
                    Speculative::Wait { holder, tid } => {
                        let context = format!(
                            "while checking exclusion constraint on tuple {} in relation \"{}\"",
                            tid_text(tid),
                            info.name
                        );
                        self.reader().wait_xact(view.xid, holder, &context)?;
                    }
                    Speculative::Key(changed) => {
                        kept = self.key_conflict(info, view, changed)?.last().copied();
                    }
                    Speculative::Conflict { tid, values, own, command, visible } => {
                        // `ExecCheckTupleVisible`: in `REPEATABLE READ` the row of the table must be in the snapshot.
                        if view.serializable && !visible && !own {
                            return Err(Error::new(
                                SqlState::T_R_SERIALIZATION_FAILURE,
                                "could not serialize access due to concurrent update",
                            ));
                        }
                        if conflict.update.is_none() {
                            break;
                        }
                        // `heap_lock_tuple` gives `TM_Invisible` for a version of the command.
                        if command {
                            return Err(Error::new(
                                SqlState::CARDINALITY_VIOLATION,
                                "ON CONFLICT DO UPDATE command cannot affect row a second time",
                            )
                            .with_hint("Ensure that no rows proposed for insertion within the same command have duplicate constrained values."));
                        }
                        let target = Target { tid, old: values, excluded: &row };
                        if self.conflict_update(
                            plan,
                            (info, fire),
                            params,
                            view,
                            target,
                            &mut done,
                        )? {
                            break;
                        }
                    }
                }
            }
        }
        Ok(done)
    }

    /// `ExecOnConflictUpdate` for one row of the table. True when the row changed or when the condition of `WHERE` skipped it, and false when another transaction changed the row first, so the row of the statement must try again.
    fn conflict_update(
        &self,
        plan: &Plan,
        (info, fire): (&Write, &Upsert),
        params: &[Value],
        view: &View,
        target: Target<'_>,
        done: &mut Upserted,
    ) -> Result<bool, Error> {
        let Target { tid, old, excluded } = target;
        let mut existing = old.clone();
        existing.push(Value::Int8(tid.cast_signed()));
        let new = {
            // A subquery of `SET` reads with the snapshot of the statement.
            let reader = self.reader().with_view(view.clone());
            plan.conflict_update(existing, excluded.to_vec(), params, &reader)?
        };
        let Some(mut new) = new else {
            return Ok(true);
        };
        // With triggers `BEFORE ... FOR EACH ROW`, the conditions of the checks follow the columns.
        let conds = if info.recheck.is_some() { info.checks.len() } else { 0 };
        let carried = new.split_off((info.width + conds).min(new.len()));
        if info.recheck.is_some() {
            match self.conflict_before(plan, info, &fire.update, &old, new)? {
                Some(row) => new = row,
                None => return Ok(true),
            }
        }
        let heap = self.store.heap();
        let mut kept = Vec::new();
        let mut applied = Vec::new();
        loop {
            let change = vec![(tid, Some(new.clone()))];
            match heap.change(info.table, view, change, &info.unique, &kept, &mut applied)? {
                Changed::Done(0) | Changed::Again => return Ok(false),
                Changed::Done(_) => break,
                Changed::Wait { holder, tid } => {
                    let context = format!(
                        "while locking tuple {} in relation \"{}\"",
                        tid_text(tid),
                        info.name
                    );
                    self.reader().wait_xact(view.xid, holder, &context)?;
                    return Ok(false);
                }
                changed => kept = self.key_conflict(info, view, changed)?,
            }
        }
        done.count += 1;
        let keep = fire.after_rows();
        let foreign = keep || info.on_conflict.as_ref().is_some_and(|c| !c.foreign.is_empty());
        if info.returning.is_some() {
            let mut written = new.clone();
            written.extend(carried);
            done.written.push(written);
        }
        if foreign {
            done.new.push(new);
            done.old.push(old);
        }
        if keep {
            done.steps.push(false);
        }
        Ok(true)
    }
}

/// The row of the table that a row of the statement meets: the ID of its version, its values, and the row of `EXCLUDED`.
#[derive(Debug)]
struct Target<'a> {
    tid: u64,
    old: Vec<Value>,
    excluded: &'a [Value],
}
