//! `INSERT ... ON CONFLICT`, as `ExecInsert` runs it with a speculative insertion and `ExecOnConflictUpdate` runs `DO UPDATE`. Each row goes in, or it meets a row of the table with its key in an arbiter index. `DO NOTHING` then skips the row, and `DO UPDATE` changes the row of the table.
//!
//! The query of the statement gives all the new rows before the first row goes in, so the checks of the constraints of the new rows come before the writes, and a function of the query does not see the rows that the statement wrote. The triggers `BEFORE ... FOR EACH ROW` fire for each row in turn, as `ExecInsert` fires them: the triggers `INSERT` before the row goes in, and the triggers `UPDATE` before `DO UPDATE` changes the row of the table.

use rupg_analyze::{Derived, OnConflict, Unique, Write};
use rupg_catalog::{Catalog, TRIGGER_INSERT, TRIGGER_UPDATE};
use rupg_common::{Error, SqlState};
use rupg_exec::Plan;
use rupg_func::Writes;
use rupg_types::Value;

use super::Connection;
use super::route::{self, Routing};
use super::trigger::{RowPair, Triggers};
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
    /// The rows of the leaf partitions for their triggers `AFTER ... FOR EACH ROW`, in the order of the write: the place of the leaf, the event, and the old and the new row of the leaf.
    leaves: Vec<(usize, i16, RowPair)>,
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
    /// `ExecCheckIndexConstraints`: an arbiter of the table `table` must not be a deferrable unique constraint. `arbiters` has the places of the arbiters in the unique keys `unique`.
    fn deferrable_arbiter(
        &self,
        table: u32,
        unique: &[Unique],
        arbiters: &[usize],
    ) -> Result<(), Error> {
        let Some(key) =
            arbiters.iter().filter_map(|&at| unique.get(at)).find(|key| key.deferrable.is_some())
        else {
            return Ok(());
        };
        let error = Error::new(
            SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
            "ON CONFLICT does not support deferrable unique constraints/exclusion constraints as arbiters",
        );
        Err(rupg_func::with_table_constraint(error, table, &key.name, &self.reader()))
    }

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
            self.deferrable_arbiter(info.table, &info.unique, &conflict.arbiters)?;
            let mut kept = None;
            loop {
                let result = heap.speculative(
                    info.table,
                    view,
                    row.clone(),
                    (&info.unique, &|d: &Derived, r: &[Value]| self.index_key(d, r)),
                    &conflict.arbiters,
                    kept,
                )?;
                match result {
                    Speculative::Inserted => {
                        done.count += 1;
                        self.count_inserts(info.table, 1);
                        if info.keeps_written() {
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
                        if !to_update(view, conflict, (own, command, visible))? {
                            break;
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

    /// `ExecInsert` with `ExecFindPartition` for `INSERT ... ON CONFLICT` into a partitioned table. Each row goes to its leaf partition, the triggers `BEFORE INSERT ... FOR EACH ROW` of the leaf fire, and the leaf checks the row. Then the speculative insertion runs in the leaf, with the indexes of the leaf whose partitioned index is an arbiter, as `ExecInitPartitionInfo` maps them. `DO UPDATE` changes the row in the leaf. The events of the triggers `AFTER ... FOR EACH ROW` of the leaves go in the queue in the order of the rows, and the result has the rows for the transition tables of the partitioned table when `fire` needs them.
    ///
    /// # Errors
    ///
    /// The errors of [`Connection::upsert`], the errors of the routing, of the triggers and of the constraints of the leaves, `0A000` for a new row of `DO UPDATE` that the leaf does not take, and `0A000` for a leaf with foreign keys.
    pub(super) fn upsert_routed(
        &self,
        plan: &Plan,
        (info, conflict, fire): (&Write, &OnConflict, &Upsert),
        params: &[Value],
        view: &View,
        rows: Vec<Vec<Value>>,
    ) -> Result<Upserted, Error> {
        let catalog = self.catalog();
        let heap = self.store.heap();
        let mut routing = Routing::of(TRIGGER_INSERT, &info.modified).with_policy(info);
        // The places of the arbiters in the unique keys of each leaf, at the place of the leaf.
        let mut arbiters: Vec<Vec<usize>> = Vec::new();
        let mut done = Upserted::default();
        let keep = fire.after_rows();
        for row in rows {
            let fired =
                routing.route_fired(self, (info.table, row.clone()), &catalog, &self.reader())?;
            let Some((at, checked)) = fired else { continue };
            // `EXCLUDED` is the row that the triggers gave.
            let row = if routing.fires_before(at) {
                routing.to_root(at, &checked, info.width)
            } else {
                row
            };
            if at == arbiters.len() {
                let oid = routing.oid(at);
                if route::has_foreign(&catalog, oid) {
                    return Err(route::not_yet(
                        "ON CONFLICT on a partitioned table with foreign keys",
                    ));
                }
                arbiters.push(leaf_arbiters(&catalog, (info, conflict), routing.write(at)?)?);
            }
            let oid = routing.oid(at);
            self.deferrable_arbiter(oid, &routing.write(at)?.unique, &arbiters[at])?;
            let mut kept = None;
            loop {
                let leaf = routing.write(at)?;
                let result = heap.speculative(
                    oid,
                    view,
                    checked.clone(),
                    (&leaf.unique, &|d: &Derived, r: &[Value]| self.index_key(d, r)),
                    &arbiters[at],
                    kept,
                )?;
                match result {
                    Speculative::Inserted => {
                        done.count += 1;
                        self.count_inserts(oid, 1);
                        if info.keeps_written() {
                            let mut written = routing.to_root(at, &checked, info.width);
                            written.push(Value::Oid(oid));
                            written.resize(info.width + 1 + conflict.carried, Value::Null);
                            done.written.push(written);
                        }
                        if keep {
                            done.inserted.push(routing.to_root(at, &checked, info.width));
                            done.steps.push(true);
                        }
                        if routing.fires_after(at, TRIGGER_INSERT) {
                            done.leaves.push((at, TRIGGER_INSERT, (None, Some(checked.clone()))));
                        }
                        break;
                    }
                    Speculative::Wait { holder, tid } => {
                        let context = format!(
                            "while checking exclusion constraint on tuple {} in relation \"{}\"",
                            tid_text(tid),
                            routing.name(at)
                        );
                        self.reader().wait_xact(view.xid, holder, &context)?;
                    }
                    Speculative::Key(changed) => {
                        kept = self.key_conflict(leaf, view, changed)?.last().copied();
                    }
                    Speculative::Conflict { tid, values, own, command, visible } => {
                        if !to_update(view, conflict, (own, command, visible))? {
                            break;
                        }
                        let target = Target { tid, old: values, excluded: &row };
                        if self.update_routed(
                            plan,
                            (info, keep, &mut routing, at),
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
        for (at, event, rows) in std::mem::take(&mut done.leaves) {
            routing.after_leaf(self, (at, event), &[rows], &info.updated)?;
        }
        Ok(done)
    }

    /// `ExecOnConflictUpdate` for one row of the leaf partition at the place `at`. The query of `DO UPDATE` reads the row with the columns of the partitioned table, and then the triggers `BEFORE UPDATE ... FOR EACH ROW` of the leaf fire. A new row that the leaf does not take is an error, as `ExecCrossPartitionUpdate` gives it for `ON CONFLICT`. With `keep`, the result has the rows for the transition tables. True when the row changed or when the condition of `WHERE` or a trigger skipped it, and false when another transaction changed the row first.
    fn update_routed(
        &self,
        plan: &Plan,
        (info, keep, routing, at): (&Write, bool, &mut Routing, usize),
        params: &[Value],
        view: &View,
        target: Target<'_>,
        done: &mut Upserted,
    ) -> Result<bool, Error> {
        let Target { tid, old, excluded } = target;
        let mut existing = routing.to_root(at, &old, info.width);
        existing.push(Value::Int8(tid.cast_signed()));
        let (checked, carried) = {
            let reader = self.reader().with_view(view.clone());
            let new = plan.conflict_update(existing, excluded.to_vec(), params, &reader);
            self.trigger_warnings.borrow_mut().extend(reader.take_warnings());
            let Some(mut new) = new? else {
                return Ok(true);
            };
            let carried = new.split_off(info.width.min(new.len()));
            // `ExecOnConflictSelect`: the row that the leaf has goes to `RETURNING`, and the statement writes no row.
            if info.on_conflict.as_ref().is_some_and(|c| c.select.is_some()) {
                done.count += 1;
                if info.keeps_written() {
                    new.push(Value::Oid(routing.oid(at)));
                    new.extend(carried);
                    done.written.push(new);
                }
                return Ok(true);
            }
            let catalog = self.catalog();
            let fired = routing.before_update(
                self,
                (at, &info.updated),
                (&old, new),
                (&catalog, &reader),
            )?;
            let Some(new) = fired else { return Ok(true) };
            let Some(checked) = routing.check(at, &new, &reader)? else {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    "invalid ON UPDATE specification",
                )
                .with_detail(
                    "The result tuple would appear in a different partition than the original tuple.",
                ));
            };
            (checked, carried)
        };
        let leaf = routing.write(at)?;
        let oid = routing.oid(at);
        let heap = self.store.heap();
        let (mut kept, mut applied) = (Vec::new(), Vec::new());
        loop {
            let change = vec![(tid, Some(checked.clone()))];
            match heap.change(
                oid,
                view,
                change,
                (&leaf.unique, &|d: &Derived, r: &[Value]| self.index_key(d, r)),
                &kept,
                &mut applied,
            )? {
                Changed::Done(0) | Changed::Again => return Ok(false),
                Changed::Done(_) => {
                    self.count_writes(oid, Writes { updated: 1, ..Writes::default() });
                    break;
                }
                Changed::Wait { holder, tid } => {
                    let context = format!(
                        "while locking tuple {} in relation \"{}\"",
                        tid_text(tid),
                        routing.name(at)
                    );
                    self.reader().wait_xact(view.xid, holder, &context)?;
                    return Ok(false);
                }
                changed => kept = self.key_conflict(leaf, view, changed)?,
            }
        }
        done.count += 1;
        if info.keeps_written() {
            let mut written = routing.to_root(at, &checked, info.width);
            written.push(Value::Oid(oid));
            written.extend(carried);
            done.written.push(written);
        }
        if keep {
            done.new.push(routing.to_root(at, &checked, info.width));
            done.old.push(routing.to_root(at, &old, info.width));
            done.steps.push(false);
        }
        if routing.fires_after(at, TRIGGER_UPDATE) {
            done.leaves.push((at, TRIGGER_UPDATE, (Some(old), Some(checked))));
        }
        Ok(true)
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
            let new = plan.conflict_update(existing, excluded.to_vec(), params, &reader);
            // The messages of `RAISE` of the functions of the policies go with the messages of the triggers.
            self.trigger_warnings.borrow_mut().extend(reader.take_warnings());
            new?
        };
        let Some(mut new) = new else {
            return Ok(true);
        };
        // `ExecOnConflictSelect`: the row that the table has goes to `RETURNING`, and the statement writes no row.
        if info.on_conflict.as_ref().is_some_and(|c| c.select.is_some()) {
            done.count += 1;
            if info.keeps_written() {
                done.written.push(new);
            }
            return Ok(true);
        }
        // With triggers `BEFORE ... FOR EACH ROW`, the conditions of the checks follow the columns.
        let conds = if info.recheck.is_some() { info.checks.len() } else { 0 };
        let carried = new.split_off((info.width + conds).min(new.len()));
        if info.recheck.is_some() {
            match self.before_row(plan, info, &fire.update, Some(old.as_slice()), new)? {
                Some(row) => new = row,
                None => return Ok(true),
            }
        }
        let heap = self.store.heap();
        let mut kept = Vec::new();
        let mut applied = Vec::new();
        loop {
            let change = vec![(tid, Some(new.clone()))];
            match heap.change(
                info.table,
                view,
                change,
                (&info.unique, &|d: &Derived, r: &[Value]| self.index_key(d, r)),
                &kept,
                &mut applied,
            )? {
                Changed::Done(0) | Changed::Again => return Ok(false),
                Changed::Done(_) => {
                    self.count_writes(info.table, Writes { updated: 1, ..Writes::default() });
                    break;
                }
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
        if info.keeps_written() {
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

/// `ExecCheckTupleVisible` and the lock of `ExecOnConflictUpdate` for the row of the table that a row of the statement meets: true for `DO UPDATE`, and false for `DO NOTHING`, which skips the row. The flags tell if the version is of the transaction, if it is of the command, and if the snapshot sees it.
///
/// # Errors
///
/// `40001` in `REPEATABLE READ` for a row that the snapshot does not see, and `21000` for `DO UPDATE` of a row that the statement wrote.
fn to_update(
    view: &View,
    conflict: &OnConflict,
    (own, command, visible): (bool, bool, bool),
) -> Result<bool, Error> {
    // `ExecCheckTupleVisible`: in `REPEATABLE READ` the row of the table must be in the snapshot.
    if view.serializable && !visible && !own {
        return Err(Error::new(
            SqlState::T_R_SERIALIZATION_FAILURE,
            "could not serialize access due to concurrent update",
        ));
    }
    if conflict.update.is_none() {
        return Ok(false);
    }
    // `heap_lock_tuple` gives `TM_Invisible` for a version of the command. `DO SELECT` with no lock only fetches the row.
    if command && (conflict.select.is_none() || conflict.locks()) {
        let what = if conflict.select.is_some() { "SELECT" } else { "UPDATE" };
        return Err(Error::new(
            SqlState::CARDINALITY_VIOLATION,
            format!("ON CONFLICT DO {what} command cannot affect row a second time"),
        )
        .with_hint(
            "Ensure that no rows proposed for insertion within the same command have duplicate constrained values.",
        ));
    }
    Ok(true)
}

/// `ExecInitPartitionInfo`: the places in the unique keys of the leaf partition `leaf` of its arbiter indexes. An index of the leaf is an arbiter when an arbiter index of the partitioned table is above it. With no inference, each unique index of the leaf is an arbiter.
fn leaf_arbiters(
    catalog: &Catalog,
    (info, conflict): (&Write, &OnConflict),
    leaf: &Write,
) -> Result<Vec<usize>, Error> {
    if conflict.all {
        return Ok((0..leaf.unique.len()).collect());
    }
    let roots: Vec<u32> =
        conflict.arbiters.iter().filter_map(|&at| info.unique.get(at)).map(|u| u.oid).collect();
    let above = |mut index: u32| {
        while let Some(parent) = catalog
            .relation(index)
            .and_then(|r| r.index.as_ref())
            .map(|i| i.parent)
            .filter(|&p| p != 0)
        {
            if roots.contains(&parent) {
                return true;
            }
            index = parent;
        }
        false
    };
    let places: Vec<usize> = leaf
        .unique
        .iter()
        .enumerate()
        .filter(|(_, key)| above(key.oid))
        .map(|(at, _)| at)
        .collect();
    if places.len() != roots.len() {
        return Err(Error::internal("invalid arbiter index list"));
    }
    Ok(places)
}

/// The row of the table that a row of the statement meets: the ID of its version, its values, and the row of `EXCLUDED`.
#[derive(Debug)]
struct Target<'a> {
    tid: u64,
    old: Vec<Value>,
    excluded: &'a [Value],
}
