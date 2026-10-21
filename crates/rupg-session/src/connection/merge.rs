//! `MERGE`, as `ExecMerge` runs it: each row of the join of the source and the target table takes the first `WHEN` clause of its kind whose condition is true, and the action of that clause inserts a row, updates or deletes the row of the target table, or does nothing.
//!
//! The query of the statement gives all the rows of the join before the first write, and the clauses run on all the rows before the first write. The triggers `BEFORE ... FOR EACH ROW` of an action fire when its clause takes the row, before the checks of the constraints, so a trigger does not see the rows that its statement wrote before. The updates and the deletes then go in one change of the heap, and the inserts come after them. The events of the triggers `AFTER` go in the queue in the order of the actions.

use std::collections::HashSet;

use rupg_analyze::{MatchKind, Merge, MergeCommand, Write};
use rupg_common::{Error, SqlState};
use rupg_exec::Plan;
use rupg_types::Value;

use super::Connection;
use super::foreign::Batch;
use super::trigger::MergeTriggers;
use crate::heap::{Change, Changed, View, tid_text};

/// The rows that a `MERGE` wrote.
#[derive(Debug)]
pub(super) struct Merged {
    /// The number of rows that went in, changed or went out.
    pub(super) count: u64,
    /// The rows of `RETURNING`, in the order of the join. Each row has the row that the action wrote, or the old row for a delete, then the old row, the new row, the name of the action and the carried values.
    pub(super) written: Vec<Vec<Value>>,
    /// The rows of the inserts, the updates and the deletes for the foreign keys.
    pub(super) batches: Vec<Batch>,
    /// The view of the writes. In `READ COMMITTED` it is new when the statement ran again.
    pub(super) view: View,
}

/// The action that a `WHEN` clause took for a row of the join.
#[derive(Debug)]
pub(super) struct Action {
    pub(super) command: MergeCommand,
    /// The ID of the version of the row of the target table, or 0 for an insert.
    tid: u64,
    /// The row of the target table, with nulls for an insert.
    pub(super) old: Vec<Value>,
    /// The new row of an insert or an update.
    pub(super) new: Option<Vec<Value>>,
    /// The carried values of the row of the join.
    carried: Vec<Value>,
}

impl Connection {
    /// Runs the actions of a `MERGE` in the command of the view. A row of the target table that a transaction in progress changed waits for the end of that transaction. In `READ COMMITTED`, a row that another transaction changed first makes the statement run again with a new snapshot.
    ///
    /// # Errors
    ///
    /// `21000` when two rows of the join update or delete the same row of the target table, `23505` for a duplicate key, and the errors of the clauses, of the triggers and of the constraints of the new rows.
    pub(super) fn merge(
        &self,
        plan: &Plan,
        info: &Write,
        (merge, fire): (&Merge, &MergeTriggers),
        params: &[Value],
        view: &View,
        rows: Vec<Vec<Value>>,
    ) -> Result<Merged, Error> {
        let heap = self.store.heap();
        let mut view = view.clone();
        let mut rows = rows;
        let (actions, applied) = loop {
            let actions = self.merge_actions(plan, info, (merge, fire), params, &view, rows)?;
            let changes: Vec<Change> = actions
                .iter()
                .filter(|a| a.command != MergeCommand::Insert)
                .map(|a| (a.tid, a.new.clone()))
                .collect();
            let mut kept = Vec::new();
            let mut applied = Vec::new();
            let again = !changes.is_empty()
                && loop {
                    let changed = heap.change(
                        info.table,
                        &view,
                        changes.clone(),
                        &info.unique,
                        &kept,
                        &mut applied,
                    )?;
                    match changed {
                        Changed::Done(_) => break false,
                        Changed::Wait { holder, tid } => {
                            let update = changes.iter().any(|c| c.0 == tid && c.1.is_some());
                            let what = if update { "updating" } else { "deleting" };
                            let context = format!(
                                "while {what} tuple {} in relation \"{}\"",
                                tid_text(tid),
                                info.name
                            );
                            self.reader().wait_xact(view.xid, holder, &context)?;
                        }
                        Changed::Again => break true,
                        conflict => kept = self.key_conflict(info, &view, conflict)?,
                    }
                };
            if !again {
                break (actions, applied);
            }
            let reader = self.reader();
            rows = plan.run(params, &reader)?;
            view = reader.view().cloned().unwrap_or_default();
        };
        let inserts: Vec<Vec<Value>> = actions
            .iter()
            .filter(|a| a.command == MergeCommand::Insert)
            .filter_map(|a| a.new.clone())
            .collect();
        if !inserts.is_empty() {
            if info.unique.is_empty() {
                heap.insert(info.table, &view, inserts.clone(), &[], &[])?;
            } else {
                let mut kept = Vec::new();
                loop {
                    match heap.insert(info.table, &view, inserts.clone(), &info.unique, &kept)? {
                        Changed::Done(_) => break,
                        conflict => kept = self.key_conflict(info, &view, conflict)?,
                    }
                }
            }
        }
        // The actions that wrote a row: each insert, and each change that the heap made.
        let applied: HashSet<usize> = applied.into_iter().collect();
        let mut change = 0;
        let mut done = Vec::with_capacity(actions.len());
        for action in actions {
            if action.command != MergeCommand::Insert {
                change += 1;
                if !applied.contains(&(change - 1)) {
                    continue;
                }
            }
            done.push(action);
        }
        if !fire.is_empty() {
            self.after_merge(info, fire, &done)?;
        }
        let width = info.width;
        let mut written = Vec::new();
        if info.returning.is_some() {
            for action in &done {
                let nulls = vec![Value::Null; width];
                let new = action.new.clone().unwrap_or_else(|| nulls.clone());
                let (row, name) = match action.command {
                    MergeCommand::Insert => (new.clone(), "INSERT"),
                    MergeCommand::Update => (new.clone(), "UPDATE"),
                    _ => (action.old.clone(), "DELETE"),
                };
                let mut out = row;
                out.extend(action.old.iter().cloned());
                out.extend(new);
                out.push(Value::Text(name.into()));
                out.extend(action.carried.iter().cloned());
                written.push(out);
            }
        }
        let mut batches = Vec::new();
        let kinds = [
            (MergeCommand::Update, &merge.update_foreign),
            (MergeCommand::Delete, &merge.delete_foreign),
            (MergeCommand::Insert, &merge.insert_foreign),
        ];
        for (command, foreign) in kinds {
            if foreign.is_empty() {
                continue;
            }
            let (mut new, mut old) = (Vec::new(), Vec::new());
            for action in done.iter().filter(|a| a.command == command) {
                if let Some(row) = &action.new {
                    new.push(row.clone());
                }
                if command != MergeCommand::Insert {
                    old.push(action.old.clone());
                }
            }
            if !new.is_empty() || !old.is_empty() {
                batches.push(Batch::new(info, foreign, view.command, new, old));
            }
        }
        Ok(Merged { count: done.len() as u64, written, batches, view })
    }

    /// The action of each row of the join: the first clause of the kind of the row whose condition is true. Each row of the join has the columns of the target table, the ID of the version of the row of the target table, the marker of the source row and the carried values. The triggers `BEFORE ... FOR EACH ROW` of the action fire for the row, as `ExecMergeMatched` and `ExecMergeNotMatched` fire them, and a trigger that gives a null row skips the action.
    fn merge_actions(
        &self,
        plan: &Plan,
        info: &Write,
        (merge, fire): (&Merge, &MergeTriggers),
        params: &[Value],
        view: &View,
        rows: Vec<Vec<Value>>,
    ) -> Result<Vec<Action>, Error> {
        let width = info.width;
        // A subquery of a clause reads with the snapshot of the statement.
        let reader = self.reader().with_view(view.clone());
        let mut actions = Vec::new();
        let mut seen = HashSet::new();
        for mut row in rows {
            if row.len() < width + 2 {
                return Err(Error::internal("a row of MERGE with no ID of its version"));
            }
            let carried = row.split_off(width + 2);
            let marker = row.pop().unwrap_or(Value::Null);
            let tid = match row.last() {
                Some(Value::Int8(tid)) => Some(tid.cast_unsigned()),
                _ => None,
            };
            let kind = match tid {
                None => MatchKind::NotMatchedByTarget,
                Some(_) if marker.is_null() => MatchKind::NotMatchedBySource,
                Some(_) => MatchKind::Matched,
            };
            for clause in merge.clauses.iter().filter(|c| c.kind == kind) {
                let out =
                    plan.merge_clause(clause, row.clone(), carried.clone(), params, &reader)?;
                let Some(mut out) = out else { continue };
                let mut old = row.clone();
                old.truncate(width);
                let tid = tid.unwrap_or_default();
                let Some(triggers) = fire.of(clause.command) else { break };
                if clause.command != MergeCommand::Insert && seen.contains(&tid) {
                    return Err(Error::new(
                        SqlState::CARDINALITY_VIOLATION,
                        "MERGE command cannot affect row a second time",
                    )
                    .with_hint(
                        "Ensure that not more than one source row matches any one target row.",
                    ));
                }
                let new = match clause.command {
                    MergeCommand::Delete => {
                        if !self.before_delete(info, triggers, &old)? {
                            break;
                        }
                        None
                    }
                    command => {
                        if command == MergeCommand::Insert {
                            old = vec![Value::Null; width];
                        }
                        if info.recheck.is_some() {
                            let given = (command == MergeCommand::Update).then_some(old.as_slice());
                            match self.before_row(plan, info, triggers, given, out)? {
                                Some(new) => out = new,
                                None => break,
                            }
                        }
                        out.truncate(width);
                        Some(out)
                    }
                };
                if clause.command != MergeCommand::Insert {
                    seen.insert(tid);
                }
                actions.push(Action {
                    command: clause.command,
                    tid,
                    old,
                    new,
                    carried: carried.clone(),
                });
                break;
            }
        }
        Ok(actions)
    }
}
