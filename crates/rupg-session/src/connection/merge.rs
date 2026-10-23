//! `MERGE`, as `ExecMerge` runs it: each row of the join of the source and the target table takes the first `WHEN` clause of its kind whose condition is true, and the action of that clause inserts a row, updates or deletes the row of the target table, or does nothing.
//!
//! The query of the statement gives all the rows of the join before the first write, and the clauses run on all the rows before the first write. The triggers `BEFORE ... FOR EACH ROW` of an action fire when its clause takes the row, before the checks of the constraints, so a trigger does not see the rows that its statement wrote before. The updates and the deletes then go in one change of the heap, and the inserts come after them. The events of the triggers `AFTER` go in the queue in the order of the actions.

use std::collections::HashSet;

use rupg_analyze::{Derived, MatchKind, Merge, MergeCommand, Write};
use rupg_catalog::{RelKind, TRIGGER_INSERT};
use rupg_common::{Error, SqlState};
use rupg_exec::Plan;
use rupg_types::Value;

use super::Connection;
use super::foreign::Batch;
use super::route::{self, AfterRow, Routing, group};
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
    /// The OID of the leaf partition of the row of a partitioned table, or 0.
    leaf: u32,
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
        // The rows of a table with children of `INHERITS` go to their tables as the rows of a partitioned table go to their leaves. An insert goes to the table of the statement.
        if info.inherit || self.partitioned(info.table) {
            return self.merge_routed(plan, info, (merge, fire), params, view, rows);
        }
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
                        (&info.unique, &|d: &Derived, r: &[Value]| self.index_key(d, r)),
                        &kept,
                        &mut applied,
                    )?;
                    match changed {
                        Changed::Done(_) => {
                            self.count_changes(info.table, &changes, &applied);
                            break false;
                        }
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
                heap.insert(
                    info.table,
                    &view,
                    inserts.clone(),
                    (&[], &|d: &Derived, r: &[Value]| self.index_key(d, r)),
                    &[],
                )?;
            } else {
                let mut kept = Vec::new();
                loop {
                    match heap.insert(
                        info.table,
                        &view,
                        inserts.clone(),
                        (&info.unique, &|d: &Derived, r: &[Value]| self.index_key(d, r)),
                        &kept,
                    )? {
                        Changed::Done(_) => break,
                        conflict => kept = self.key_conflict(info, &view, conflict)?,
                    }
                }
            }
            self.count_inserts(info.table, inserts.len());
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
        let mut written = Vec::new();
        if info.returning.is_some() {
            written = done.iter().map(|action| returned(action, info.width, None)).collect();
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
        // A row of a partitioned table, or of a table with children of `INHERITS`, has the OID of its table before the ID of its version.
        let partitioned = usize::from(info.inherit || self.partitioned(info.table));
        // A subquery of a clause reads with the snapshot of the statement.
        let reader = self.reader().with_view(view.clone());
        let mut actions = Vec::new();
        let mut seen = HashSet::new();
        for mut row in rows {
            if row.len() < width + 2 + partitioned {
                return Err(Error::internal("a row of MERGE with no ID of its version"));
            }
            let carried = row.split_off(width + 2 + partitioned);
            let marker = row.pop().unwrap_or(Value::Null);
            let leaf = match partitioned {
                0 => 0,
                _ => match row.remove(width) {
                    Value::Oid(oid) => oid,
                    _ => 0,
                },
            };
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
                if clause.command != MergeCommand::Insert && seen.contains(&(leaf, tid)) {
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
                    seen.insert((leaf, tid));
                }
                actions.push(Action {
                    command: clause.command,
                    tid,
                    leaf,
                    old,
                    new,
                    carried: carried.clone(),
                });
                break;
            }
        }
        Ok(actions)
    }

    /// `ExecMerge` for a partitioned table. The action of each row of the join goes to the leaf partitions: an update or a delete changes the row in its leaf as `ExecUpdate` and `ExecDelete` do, and an update can move the row to another leaf as `ExecCrossPartitionUpdate` moves it. An insert goes to the leaf that `ExecFindPartition` gives. The triggers `BEFORE ... FOR EACH ROW` of the leaves fire for each action in the order of the join, and the events of their triggers `AFTER ... FOR EACH ROW` go in the queue in the same order. A row that moves is in the transition tables of `DELETE` and of `INSERT`.
    ///
    /// # Errors
    ///
    /// `21000` when two rows of the join update or delete the same row, `23514` for a row that no partition takes, `23505` for a duplicate key, and the errors of the clauses, of the triggers and of the constraints of the leaves.
    fn merge_routed(
        &self,
        plan: &Plan,
        info: &Write,
        (merge, fire): (&Merge, &MergeTriggers),
        params: &[Value],
        view: &View,
        rows: Vec<Vec<Value>>,
    ) -> Result<Merged, Error> {
        let catalog = self.catalog();
        let width = info.width;
        let mut routing = Routing::of(TRIGGER_INSERT);
        let mut view = view.clone();
        let mut rows = rows;
        // The actions of the join, and for each action the new row and the leaf of the write, or `None` when the leaves did not write the row.
        let mut actions: Vec<Action> = Vec::new();
        let mut results: Vec<Option<Written>> = Vec::new();
        // The checked new rows of the inserts and of the rows that move, with the place of the action and the index of the leaf.
        let mut inserts: Vec<(usize, usize, Vec<Value>)> = Vec::new();
        let mut applied = Vec::new();
        loop {
            let found = self.merge_actions(plan, info, (merge, fire), params, &view, rows)?;
            let reader = self.reader();
            let mut groups = Vec::new();
            for action in found {
                let seq = actions.len();
                if action.command == MergeCommand::Insert {
                    let Some(new) = action.new.clone() else { continue };
                    let fired = routing.route_fired(self, (info.table, new), &catalog, &reader)?;
                    let Some((at, checked)) = fired else { continue };
                    let new = routing.to_root(at, &checked, width);
                    results.push(Some(Written {
                        new: Some(new),
                        oid: routing.oid(at),
                        moves: false,
                    }));
                    inserts.push((seq, at, checked));
                } else {
                    let update = action.command == MergeCommand::Update;
                    let mut row = action.new.clone().unwrap_or_else(|| action.old.clone());
                    let place = (action.leaf, action.tid, seq);
                    let shape = (info, width, update);
                    let routed = self.route_change(
                        &mut routing,
                        shape,
                        place,
                        &mut row,
                        (&catalog, &reader),
                    )?;
                    let Some((at, mut item)) = routed else { continue };
                    let to = item.to.as_ref().map_or(at, |(to, _)| *to);
                    if route::has_foreign(&catalog, routing.oid(at))
                        || route::has_foreign(&catalog, routing.oid(to))
                    {
                        return Err(route::not_yet(
                            "MERGE into a partitioned table with foreign keys",
                        ));
                    }
                    // The new row of an update after the triggers, with the columns of the partitioned table.
                    item.shown = match &item.to {
                        Some((to, checked)) => Some(routing.to_root(*to, checked, width)),
                        None => update.then_some(row),
                    };
                    group(&mut groups, at, item);
                    results.push(None);
                }
                actions.push(action);
            }
            drop(reader);
            if !self.change_leaves(&routing, groups, &view, false, &mut applied)? {
                break;
            }
            // The rows that this command changed stay as they are in the new run.
            let reader = self.reader();
            rows = plan.run(params, &reader)?;
            view = reader.view().cloned().unwrap_or_default();
        }
        let mut after: Vec<AfterRow> = Vec::new();
        for (at, item, old) in applied {
            let event = item.event();
            if routing.fires_after(at, event) {
                after.push((item.seq, at, event, (old, item.new.clone())));
            }
            if item.moves && item.to.is_none() {
                continue;
            }
            let oid = routing.oid(item.to.as_ref().map_or(at, |to| to.0));
            let written = Written { new: item.shown, oid, moves: item.moves };
            if let Some(slot) = results.get_mut(item.seq) {
                *slot = Some(written);
            }
            if let Some((to, checked)) = item.to {
                inserts.push((item.seq, to, checked));
            }
        }
        let batches = self.insert_moved(&mut routing, &view, inserts, after, &info.updated)?;
        let mut done = Vec::new();
        let mut oids = Vec::new();
        // The actions for the transition tables: a row that moves is a delete and an insert.
        let mut captured = Vec::new();
        for (action, result) in actions.into_iter().zip(results) {
            let Some(Written { new, oid, moves }) = result else { continue };
            let action = Action { new, ..action };
            if !fire.is_empty() {
                let row = |command, old: Vec<Value>, new| Action {
                    command,
                    tid: 0,
                    leaf: 0,
                    old,
                    new,
                    carried: Vec::new(),
                };
                if moves {
                    captured.push(row(MergeCommand::Delete, action.old.clone(), None));
                    let nulls = vec![Value::Null; width];
                    captured.push(row(MergeCommand::Insert, nulls, action.new.clone()));
                } else {
                    captured.push(row(action.command, action.old.clone(), action.new.clone()));
                }
            }
            oids.push(oid);
            done.push(action);
        }
        if !fire.is_empty() {
            self.after_merge(info, fire, &captured)?;
        }
        let mut written = Vec::new();
        if info.returning.is_some() {
            let pairs = done.iter().zip(&oids);
            written = pairs.map(|(action, &oid)| returned(action, width, Some(oid))).collect();
        }
        Ok(Merged { count: done.len() as u64, written, batches, view })
    }

    /// True when the relation `table` is a partitioned table.
    fn partitioned(&self, table: u32) -> bool {
        self.catalog().relation(table).is_some_and(|r| r.kind == RelKind::Partitioned)
    }
}

/// The write of the action of a `MERGE` in a partitioned table.
#[derive(Debug)]
struct Written {
    /// The new row of an insert or an update, with the columns of the partitioned table.
    new: Option<Vec<Value>>,
    /// The OID of the leaf that has the new row, or the old row of a delete.
    oid: u32,
    /// True for an update that moves the row to another leaf.
    moves: bool,
}

/// The row of `RETURNING` for the action `action` of a table with `width` columns: the row that the action wrote, or the old row for a delete, then the old row, the new row and the name of the action. For a partitioned table, the OID `oid` of the leaf comes next. The carried values are last.
fn returned(action: &Action, width: usize, oid: Option<u32>) -> Vec<Value> {
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
    out.extend(oid.map(Value::Oid));
    out.extend(action.carried.iter().cloned());
    out
}
