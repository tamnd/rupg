//! `INSERT` into a partitioned table: `ExecFindPartition` routes each row to the leaf partition that takes it, and the leaf checks and writes the row.

use rupg_analyze::{CopyTarget, Derived, Params, Write, WriteKind};
use rupg_catalog::{Catalog, ConKind, TRIGGER_DELETE, TRIGGER_INSERT, TRIGGER_UPDATE};
use rupg_common::{Error, SqlState};
use rupg_exec::{Plan, Router};
use rupg_pgcatalog::builtin::{self, Named};
use rupg_sql::nodes::RangeVar;
use rupg_types::Value;
use std::rc::Rc;

/// The row security check of a partitioned root, with the name of the root table.
type RootPolicy = Rc<(rupg_analyze::PolicyCheck, String)>;

use super::foreign::Batch;
use super::trigger::{RowPair, Triggers, armed};
use super::{Connection, put_tids};
use crate::heap::{Change, Changed, View, tid_text};
use crate::query::{self, Reader};

/// A leaf partition of the rows of a statement, as `ExecInitPartitionInfo` gives its `ResultRelInfo`.
#[derive(Debug)]
struct Leaf {
    oid: u32,
    name: String,
    /// The `INSERT` into the leaf, from [`rupg_analyze::copy_insert`], with a parameter for each column of `live`.
    plan: Plan,
    /// The columns of the leaf that are not dropped and not generated. The leaf computes its generated columns.
    live: Vec<usize>,
    /// For each column of the leaf, the column of the partitioned table of the statement with the same name. A dropped column has none.
    up: Vec<Option<usize>>,
    /// The checked rows for the leaf.
    rows: Vec<Vec<Value>>,
    /// The line of each row of a `COPY FROM`.
    lines: Vec<u64>,
    /// The ID of the version of each row of the last write of `rows`, for `ctid` of `RETURNING`.
    ids: Vec<u64>,
    /// The triggers `FOR EACH ROW` of the leaf for the event of the statement.
    fire: Triggers,
    /// The event of `fire`.
    event: i16,
    /// The triggers of the leaf for the other events of the statement, with each event.
    armed: Vec<(i16, Triggers)>,
    /// The name of the leaf with its schema.
    qualified: String,
    /// The type of each column of the partitioned table of the statement, for the detail of an error.
    root_types: Vec<u32>,
    /// The partitioned table of the statement.
    root: u32,
    /// The columns of the partitioned table that the statement gives values, for the detail of an error.
    modified: Vec<i16>,
    /// The checks of the policies of the partitioned table, with its name, which `ExecInsert` maps to the leaf.
    policy: Option<RootPolicy>,
}

/// The routing of the rows of a statement into the leaf partitions of a partitioned table, and the rows of each leaf that the statement did not write yet.
#[derive(Debug, Default)]
pub(super) struct Routing {
    router: Router,
    leaves: Vec<Leaf>,
    /// The bit in `tgtype` of the event whose triggers of the leaves fire, or 0 when a leaf with triggers gives an error.
    event: i16,
    /// The columns of the partitioned table that the statement gives values, as `ExecGetInsertedCols` and `ExecGetUpdatedCols` give them.
    modified: Vec<i16>,
    /// The checks of the policies of the partitioned table for its leaves.
    policy: Option<RootPolicy>,
    /// The checks of the policies of the partitioned table for the new rows of `ON CONFLICT DO UPDATE`.
    update_policy: Option<RootPolicy>,
}

impl Leaf {
    /// The triggers of the leaf for the event `event`, after [`Leaf::arm`] for an event that is not the event of the statement.
    fn fire_of(&self, event: i16) -> Option<&Triggers> {
        if event == self.event {
            return Some(&self.fire);
        }
        self.armed.iter().find(|(e, _)| *e == event).map(|(_, triggers)| triggers)
    }

    /// Reads the triggers of the leaf for the event `event` from the catalog, at the first call for the event.
    fn arm(&mut self, event: i16, catalog: &Catalog, reader: &Reader<'_>) -> Result<(), Error> {
        if self.fire_of(event).is_none() {
            self.armed.push((event, armed(catalog, reader, self.oid, event)?));
        }
        Ok(())
    }
}

impl Routing {
    /// A routing whose leaves fire their triggers for the event `event`, a bit of `tgtype`.
    pub(super) fn of(event: i16, modified: &[i16]) -> Routing {
        Routing {
            router: Router::of(modified),
            event,
            modified: modified.to_vec(),
            ..Routing::default()
        }
    }

    /// The routing with the checks of the policies of the write `info`, which each leaf checks on its new rows.
    pub(super) fn with_policy(mut self, info: &Write) -> Routing {
        self.policy = info.policy_check.clone().map(|check| Rc::new((check, info.name.clone())));
        let update = info.on_conflict.as_ref().and_then(|c| c.update_check.clone());
        self.update_policy = update.map(|check| Rc::new((check, info.name.clone())));
        self
    }

    /// `ExecFindPartition` for a new row of the partitioned table `root`, then the triggers `BEFORE INSERT ... FOR EACH ROW` of the leaf, which fire before `ExecConstraints` checks the row with the constraints of the leaf. After the triggers, `ExecInsert` also checks the partition constraint of the leaf. The result is the index of the leaf and the checked row with the columns of the leaf, or `None` when a trigger skips the row.
    ///
    /// # Errors
    ///
    /// `23514` for a row that no partition takes or that a trigger takes out of the bounds of the leaf, and the errors of the triggers and of the constraints of the leaf.
    pub(super) fn route_fired(
        &mut self,
        conn: &Connection,
        (root, row): (u32, Vec<Value>),
        catalog: &Catalog,
        reader: &Reader<'_>,
    ) -> Result<Option<(usize, Vec<Value>)>, Error> {
        let (oid, values) = self.router.route(root, row, reader)?;
        let at = leaf_at(
            &mut self.leaves,
            catalog,
            (root, oid, self.event),
            (&self.modified, &self.policy),
            reader,
        )?;
        let Self { router, leaves, .. } = self;
        let leaf = &leaves[at];
        if !leaf.fire.before_rows() {
            let checked = leaf_row(leaf, &values, reader)?;
            return Ok(Some((at, checked)));
        }
        let shape = (&[][..], leaf.up.len());
        let mut stays = |row: &[Value]| router.check(oid, row, reader);
        let rows = (None, Some(values));
        let fired = conn.leaf_before((&leaf.fire, shape), rows, &leaf.qualified, &mut stays)?;
        let Some(values) = fired else { return Ok(None) };
        let checked = leaf_row(leaf, &values, reader)?;
        router
            .check_row(oid, &checked, reader)
            .map_err(|e| root_detail(leaf, &checked, e, reader))?;
        Ok(Some((at, checked)))
    }

    /// True when the leaf at the place `at` fires triggers `BEFORE ... FOR EACH ROW` for the event of the statement.
    pub(super) fn fires_before(&self, at: usize) -> bool {
        self.leaves.get(at).is_some_and(|leaf| leaf.fire.before_rows())
    }

    /// True when the leaf at the place `at` fires triggers `AFTER ... FOR EACH ROW` for the event `event`, after [`Routing::before_update`] for `UPDATE`.
    pub(super) fn fires_after(&self, at: usize, event: i16) -> bool {
        self.leaves.get(at).and_then(|leaf| leaf.fire_of(event)).is_some_and(Triggers::after_rows)
    }

    /// `ExecBRUpdateTriggers` of the leaf at the place `at` for `ON CONFLICT DO UPDATE`: the old row `old` of the leaf and the new row `new` of the partitioned table, which sets the columns `updated`. The result is the new row of the partitioned table that the triggers give, or `None` when a trigger skips the row.
    ///
    /// # Errors
    ///
    /// The errors of the triggers.
    pub(super) fn before_update(
        &mut self,
        conn: &Connection,
        (at, updated): (usize, &[i16]),
        (old, new): (&[Value], Vec<Value>),
        (catalog, reader): (&Catalog, &Reader<'_>),
    ) -> Result<Option<Vec<Value>>, Error> {
        let Some(leaf) = self.leaves.get_mut(at) else { return Ok(Some(new)) };
        leaf.arm(TRIGGER_UPDATE, catalog, reader)?;
        let leaf = &*leaf;
        let Some(fire) = leaf.fire_of(TRIGGER_UPDATE).filter(|t| t.before_rows()) else {
            return Ok(Some(new));
        };
        let updated = leaf_updated(leaf, updated);
        let shape = (&updated[..], leaf.up.len());
        let mut stays = |_: &[Value]| Ok(true);
        let rows = (Some(old.to_vec()), Some(down(leaf, &new)));
        let fired = conn.leaf_before((fire, shape), rows, &leaf.qualified, &mut stays)?;
        Ok(fired.map(|row| up(leaf, &row, new.len())))
    }

    /// `ExecARInsertTriggers` and `ExecARUpdateTriggers` of the leaf at the place `at` for the event `event`: the events of its triggers `AFTER ... FOR EACH ROW` for the rows `rows` of the leaf, at the end of the queue. An `UPDATE` sets the columns `updated` of the partitioned table.
    ///
    /// # Errors
    ///
    /// The error of the condition of `WHEN` of a trigger.
    pub(super) fn after_leaf(
        &self,
        conn: &Connection,
        (at, event): (usize, i16),
        rows: &[RowPair],
        updated: &[i16],
    ) -> Result<(), Error> {
        let Some(leaf) = self.leaves.get(at) else { return Ok(()) };
        let Some(fire) = leaf.fire_of(event) else { return Ok(()) };
        let updated = leaf_updated(leaf, updated);
        conn.leaf_after((fire, (&updated, leaf.up.len())), rows)
    }

    /// `ExecPartitionCheck` and `ExecConstraints` for a new row of the partitioned table that changes a row of the leaf at the place `at`. The leaf checks the row with the policies of `ON CONFLICT DO UPDATE` of the partitioned table. The result is the checked row with the columns of the leaf, or `None` when the leaf does not take the row.
    ///
    /// # Errors
    ///
    /// The errors of the expressions of the keys and of the constraints of the leaf.
    pub(super) fn check(
        &mut self,
        at: usize,
        row: &[Value],
        reader: &Reader<'_>,
    ) -> Result<Option<Vec<Value>>, Error> {
        let leaf = &self.leaves[at];
        let values = down(leaf, row);
        if !self.router.check(leaf.oid, &values, reader)? {
            return Ok(None);
        }
        leaf_row_with(leaf, &values, self.update_policy.as_ref(), reader).map(Some)
    }

    /// A row of the leaf at the place `at` with the `width` columns of the partitioned table, as `execute_attr_map_slot` gives it.
    pub(super) fn to_root(&self, at: usize, row: &[Value], width: usize) -> Vec<Value> {
        match self.leaves.get(at) {
            Some(leaf) => up(leaf, row, width),
            None => vec![Value::Null; width],
        }
    }

    /// The name of the leaf at the place `at`.
    pub(super) fn name(&self, at: usize) -> &str {
        self.leaves.get(at).map_or("", |leaf| leaf.name.as_str())
    }

    /// The write of the leaf at the place `at`, with its unique keys.
    pub(super) fn write(&self, at: usize) -> Result<&Write, Error> {
        self.leaves
            .get(at)
            .and_then(|leaf| leaf.plan.write())
            .ok_or_else(|| Error::internal("a partition with no plan of its rows"))
    }

    /// The OID of the leaf at the place `at`.
    pub(super) fn oid(&self, at: usize) -> u32 {
        self.leaves.get(at).map_or(0, |leaf| leaf.oid)
    }

    /// Keeps the checked row `row` of a `COPY FROM` in the leaf at the place `at`, with the line of the copy.
    pub(super) fn push(&mut self, at: usize, row: Vec<Value>, line: u64) {
        if let Some(leaf) = self.leaves.get_mut(at) {
            leaf.rows.push(row);
            leaf.lines.push(line);
        }
    }

    /// The number of rows that the leaves keep.
    pub(super) fn pending(&self) -> usize {
        self.leaves.iter().map(|leaf| leaf.rows.len()).sum()
    }
}

/// The rows of an `INSERT` into a partitioned table that the leaves wrote.
pub(super) struct Routed {
    /// The number of the rows that the leaves wrote.
    pub(super) count: u64,
    /// The rows for `RETURNING`: the columns of the partitioned table, then the OID of the leaf.
    pub(super) written: Vec<Vec<Value>>,
    /// The new rows with the columns of the partitioned table, for its transition tables.
    pub(super) inserted: Vec<Vec<Value>>,
    /// The new rows that the foreign keys of the leaves check.
    pub(super) batches: Vec<Batch>,
}

impl Connection {
    /// `ExecInsert` with `ExecFindPartition`: each row goes to its leaf partition, the triggers `BEFORE ... FOR EACH ROW` of the leaf fire, and the not-null and check constraints of the leaf check the row, in the order of the rows. Then each leaf writes its rows and checks its unique keys, and the events of the triggers `AFTER ... FOR EACH ROW` of the leaves go in the queue in the order of the rows. With `keep`, the result has the new rows for the transition tables of the partitioned table.
    ///
    /// # Errors
    ///
    /// `23514` for a row that no partition takes, the errors of the triggers and of the constraints of the leaves.
    pub(super) fn insert_routed(
        &self,
        info: &Write,
        view: &View,
        rows: Vec<Vec<Value>>,
        keep: bool,
    ) -> Result<Routed, Error> {
        let catalog = self.catalog();
        let reader = self.reader();
        let mut routing = Routing::of(TRIGGER_INSERT, &info.modified).with_policy(info);
        let (mut written, mut inserted) = (Vec::new(), Vec::new());
        // The new rows of the leaves with triggers `AFTER ... FOR EACH ROW`.
        let mut after = Vec::new();
        // The leaf and the place in the rows of the leaf of each row of `written`.
        let mut places = Vec::new();
        let mut count = 0;
        for row in rows {
            let fired = routing.route_fired(self, (info.table, row), &catalog, &reader)?;
            let Some((at, checked)) = fired else { continue };
            count += 1;
            if info.keeps_written() || keep {
                let mut shown = routing.to_root(at, &checked, info.width);
                if keep {
                    inserted.push(shown.clone());
                }
                if info.keeps_written() {
                    shown.push(Value::Oid(routing.oid(at)));
                    written.push(shown);
                    places.push((at, routing.leaves[at].rows.len()));
                }
            }
            if routing.leaves[at].fire.after_rows() {
                after.push((at, checked.clone()));
            }
            routing.leaves[at].rows.push(checked);
        }
        drop(reader);
        let batches = self.insert_leaves(&mut routing, view, None)?;
        if let Some(tid_at) = info.returning_tid {
            for (row, (at, place)) in written.iter_mut().zip(places) {
                let id = routing.leaves[at].ids.get(place).copied().unwrap_or(0);
                put_tids(std::slice::from_mut(row), tid_at, &[id]);
            }
        }
        for (at, row) in after {
            let leaf = &routing.leaves[at];
            self.leaf_after((&leaf.fire, (&[], leaf.up.len())), &[(None, Some(row))])?;
        }
        Ok(Routed { count, written, inserted, batches })
    }

    /// `table_tuple_insert` of the checked rows that each leaf keeps, with the checks of its unique keys. It gives the rows that the foreign keys of the leaves check. For a `COPY FROM` of the table `copy`, a duplicate key has the context of the line of its row, or the context of the one row of `CIM_SINGLE`, and the events of the triggers `AFTER ... FOR EACH ROW` of each leaf go in the queue after the leaf writes its rows, as `CopyMultiInsertBufferFlush` queues them.
    pub(super) fn insert_leaves(
        &self,
        routing: &mut Routing,
        view: &View,
        copy: Option<(&str, Option<&str>)>,
    ) -> Result<Vec<Batch>, Error> {
        let heap = self.store.heap();
        let mut batches = Vec::new();
        for leaf in &mut routing.leaves {
            if leaf.rows.is_empty() {
                continue;
            }
            let rows = std::mem::take(&mut leaf.rows);
            let lines = std::mem::take(&mut leaf.lines);
            let Some(leaf_info) = leaf.plan.write() else {
                return Err(Error::internal("a partition with no plan of its rows"));
            };
            let checked = if leaf_info.foreign.is_empty() { Vec::new() } else { rows.clone() };
            self.heap_layout(leaf.oid);
            let mut kept = Vec::new();
            loop {
                match heap.insert_ids(
                    leaf.oid,
                    view,
                    rows.clone(),
                    (&leaf_info.unique, &|d: &Derived, r: &[Value]| self.index_key(d, r)),
                    &kept,
                    &mut leaf.ids,
                )? {
                    Changed::Done(_) => break,
                    Changed::Duplicate { index, key } if copy.is_some() => {
                        let derive = |d: &Derived, r: &[Value]| self.index_key(d, r);
                        let line = super::copy_from::duplicate_line(
                            (leaf_info, index),
                            &key,
                            &rows,
                            &lines,
                            &derive,
                        );
                        let conflict = Changed::Duplicate { index, key };
                        let error = match self.key_conflict(leaf_info, view, conflict) {
                            Ok(_) => Error::internal("a duplicate key with no error"),
                            Err(error) => error,
                        };
                        let context = match copy {
                            Some((_, Some(context))) => context.to_string(),
                            Some((name, None)) => format!("COPY {name}, line {line}"),
                            None => String::new(),
                        };
                        return Err(error.with_context(context));
                    }
                    conflict => kept = self.key_conflict(leaf_info, view, conflict)?,
                }
            }
            self.count_inserts(leaf.oid, rows.len());
            if copy.is_some() && leaf.fire.after_rows() {
                let rows: Vec<RowPair> = rows.into_iter().map(|row| (None, Some(row))).collect();
                self.leaf_after((&leaf.fire, (&[], leaf.up.len())), &rows)?;
            }
            if !checked.is_empty() {
                batches.push(Batch::new(
                    leaf_info,
                    &leaf_info.foreign,
                    view.command,
                    checked,
                    Vec::new(),
                ));
            }
        }
        Ok(batches)
    }

    /// `ExecUpdate` and `ExecDelete` for a partitioned table, or for a table with children of `INHERITS`: each row of the query gives the OID of its leaf and the ID of its version, and each leaf changes its rows. The triggers `BEFORE ... FOR EACH ROW` of the leaf fire for each row first. A new row that its leaf does not take moves, as `ExecCrossPartitionUpdate` moves it: the triggers `BEFORE DELETE` of the old leaf and `BEFORE INSERT` of the new leaf fire, the old leaf deletes the old row, and the row goes in the leaf that `ExecFindPartition` gives. The events of the triggers `AFTER ... FOR EACH ROW` of the leaves go in the queue in the order of the rows. With `keep`, the result has the old and the new rows with the columns of the partitioned table, for its transition tables.
    ///
    /// # Errors
    ///
    /// The errors of the routing, of the triggers and of the constraints of the leaves.
    pub(super) fn change_routed(
        &self,
        (plan, info): (&Plan, &Write),
        params: &[Value],
        view: &View,
        rows: Vec<Vec<Value>>,
        keep: bool,
    ) -> Result<ChangedRows, Error> {
        let catalog = self.catalog();
        let Some(rel) = catalog.relation(info.table) else {
            return Err(Error::internal(format!("relation {} is not in the catalog", info.table)));
        };
        let width = rel.columns.len();
        let update = info.kind == WriteKind::Update;
        let returning = info.keeps_written();
        let mut done = ChangedRows { view: view.clone(), ..ChangedRows::default() };
        let mut routing =
            Routing::of(if update { TRIGGER_UPDATE } else { TRIGGER_DELETE }, &info.modified)
                .with_policy(info);
        // The new rows that move to another leaf, after the old leaf deleted the old rows, with the place of each row in the statement.
        let mut moved: Vec<(usize, usize, Vec<Value>)> = Vec::new();
        // The events of the triggers `AFTER ... FOR EACH ROW` and the rows of the transition tables, with the place of each row in the statement.
        let mut after: Vec<AfterRow> = Vec::new();
        let (mut old_rows, mut new_rows) = (Vec::new(), Vec::new());
        let mut rows = rows;
        let mut seq = 0;
        let mut applied = Vec::new();
        let foreign = tree(&catalog, info.table).into_iter().any(|oid| has_foreign(&catalog, oid));
        loop {
            let reader = self.reader();
            // The changes of each old leaf, in the order of its first row, with the item of each change.
            let mut groups: Vec<Group> = Vec::new();
            for mut row in std::mem::take(&mut rows) {
                seq += 1;
                let (Some(Value::Int8(tid)), Some(Value::Oid(old))) = (row.pop(), row.pop()) else {
                    return Err(Error::internal("a row to change with no partition or version"));
                };
                let place = (old, tid.cast_unsigned(), seq);
                let shape = (info, width, update);
                let routed =
                    self.route_change(&mut routing, shape, place, &mut row, (&catalog, &reader))?;
                let Some((at, mut item)) = routed else { continue };
                if returning && (!item.moves || item.to.is_some()) {
                    let (to, mut shown) = match &item.to {
                        Some((to, checked)) => (*to, up(&routing.leaves[*to], checked, width)),
                        None => (at, row[..width].to_vec()),
                    };
                    shown.push(Value::Oid(routing.oid(to)));
                    shown.extend_from_slice(&row[width..]);
                    item.shown = Some(shown);
                }
                group(&mut groups, at, item);
            }
            drop(reader);
            if !self.change_leaves(&routing, groups, &done.view, keep || foreign, &mut applied)? {
                break;
            }
            // The rows that this command changed stay as they are in the new run.
            let reader = self.reader();
            rows = plan.run(params, &reader)?;
            done.view = reader.view().cloned().unwrap_or_default();
        }
        // The old and the new rows of the leaves for the triggers of the foreign keys, for each leaf and change, and the rows that move with the columns of the partitioned table.
        let mut changes: Vec<FkRows> = Vec::new();
        let (mut moved_new, mut moved_old) = (Vec::new(), Vec::new());
        // The place in `written` and in the statement of each row that moves, for `ctid` of `RETURNING`.
        let mut moved_shown = Vec::new();
        let heap = self.store.heap();
        for (at, item, old) in applied {
            let leaf = &routing.leaves[at];
            if update && item.to.is_some() {
                check_ancestor_keys(&catalog, leaf.oid, info.table)?;
            }
            if !item.moves || item.to.is_some() {
                done.count += 1;
            }
            if foreign && let Some(old) = &old {
                let change = match (update, item.moves) {
                    (true, false) => FkChange::Update,
                    (true, true) => FkChange::Moved,
                    (false, _) => FkChange::Delete,
                };
                let at_change = changes.iter().position(|c| c.0 == at && c.1 == change);
                let at_change = at_change.unwrap_or_else(|| {
                    changes.push((at, change, Vec::new(), Vec::new()));
                    changes.len() - 1
                });
                let rows = &mut changes[at_change];
                rows.3.push(old.clone());
                if let Some(new) = item.new.as_ref().filter(|_| change == FkChange::Update) {
                    rows.2.push(new.clone());
                }
                if let Some((to, checked)) = &item.to {
                    moved_old.push(up(leaf, old, width));
                    moved_new.push(up(&routing.leaves[*to], checked, width));
                }
            }
            if let Some(mut shown) = item.shown.clone() {
                if let Some(tid_at) = info.returning_tid {
                    if item.moves {
                        moved_shown.push((done.written.len(), item.seq));
                    } else {
                        let next = heap.next_versions(view.database, leaf.oid, &[item.tid]);
                        put_tids(std::slice::from_mut(&mut shown), tid_at, &next);
                    }
                }
                done.written.push(shown);
            }
            if keep && let Some(old) = &old {
                old_rows.push((item.seq, up(leaf, old, width)));
                if let Some(new) = &item.new {
                    new_rows.push((item.seq, up(leaf, new, width)));
                }
            }
            if leaf.fire_of(item.event()).is_some_and(Triggers::after_rows) {
                after.push((item.seq, at, item.event(), (old, item.new.clone())));
            }
            if let Some((to, checked)) = item.to {
                if keep {
                    new_rows.push((item.seq, up(&routing.leaves[to], &checked, width)));
                }
                moved.push((item.seq, to, checked));
            }
        }
        // `AfterTriggerSaveEvent`: the triggers of the foreign keys of each leaf fire for its rows. For a row that moves, a clone of a foreign key that refers to the old leaf does not fire, as the foreign key of the partitioned table fires for the update, as `ExecCrossPartitionUpdateForeignKey` queues it.
        let reader = self.reader();
        for (at, change, new, old) in changes {
            let leaf = &routing.leaves[at];
            let Some(leaf_info) = leaf.plan.write() else {
                return Err(Error::internal("a partition with no plan of its rows"));
            };
            let kind =
                if change == FkChange::Update { WriteKind::Update } else { WriteKind::Delete };
            let mut keys = rupg_analyze::foreign_keys(&reader, leaf.oid, kind)?;
            if change == FkChange::Moved {
                keys.retain(|f| !f.cloned);
            }
            if !keys.is_empty() {
                done.batches.push(Batch::new(leaf_info, &keys, done.view.command, new, old));
            }
        }
        drop(reader);
        let (batches, ids) =
            self.insert_moved(&mut routing, &done.view, moved, after, &info.updated)?;
        done.batches.extend(batches);
        if let Some(tid_at) = info.returning_tid {
            for (place, seq) in moved_shown {
                let id = ids.iter().find(|row| row.0 == seq).map_or(0, |row| row.1);
                if let Some(row) = done.written.get_mut(place) {
                    put_tids(std::slice::from_mut(row), tid_at, &[id]);
                }
            }
        }
        // `ExecARUpdateTriggers` of the target table: all its triggers fire for the rows that move, the clones too when the target is a partition.
        if !info.foreign.is_empty() && !moved_old.is_empty() {
            done.batches.push(Batch::new(
                info,
                &info.foreign,
                done.view.command,
                moved_new,
                moved_old,
            ));
        }
        old_rows.sort_by_key(|row| row.0);
        new_rows.sort_by_key(|row| row.0);
        done.old = old_rows.into_iter().map(|row| row.1).collect();
        done.new = new_rows.into_iter().map(|row| row.1).collect();
        Ok(done)
    }

    /// The part of `ExecUpdate` and `ExecDelete` of a partitioned table before the write of one row of the write `info`: the old row is the version `tid` in the leaf `old`, and the row `row` of the partitioned table has the new row of an `UPDATE` in its first `width` columns. The triggers `BEFORE UPDATE` or `BEFORE DELETE ... FOR EACH ROW` of the leaf fire, and the row of a trigger goes in `row`. A new row that its leaf does not take moves: the triggers `BEFORE DELETE` of the old leaf and `BEFORE INSERT` of the new leaf fire, as `ExecCrossPartitionUpdate` fires them. A row of a table of the tree of `INHERITS` stays in its table, and the columns of the child that the parent table does not have keep their values. The result is the index of the old leaf and the change, or `None` when a trigger skips the row.
    ///
    /// # Errors
    ///
    /// The errors of the routing, of the triggers and of the constraints of the leaves.
    pub(super) fn route_change(
        &self,
        routing: &mut Routing,
        (info, width, update): (&Write, usize, bool),
        (old, tid, seq): (u32, u64, usize),
        row: &mut Vec<Value>,
        (catalog, reader): (&Catalog, &Reader<'_>),
    ) -> Result<Option<(usize, Moved)>, Error> {
        let heap = self.store.heap();
        let table = info.table;
        let event = if update { TRIGGER_UPDATE } else { TRIGGER_DELETE };
        let Routing { router, leaves, event: first, policy, .. } = routing;
        let at = leaf_at(leaves, catalog, (table, old, *first), (&info.modified, policy), reader)?;
        leaves[at].arm(event, catalog, reader)?;
        let leaf = &leaves[at];
        // The old row of the leaf for the triggers.
        let mut old_row = None;
        // The new row of a table of the tree of `INHERITS`: its old row with the values of the columns of the parent table, which keeps the values of the columns of the child.
        let mut child_row = None;
        if update && info.inherit {
            let versions = heap.versions(self.database_oid, leaf.oid, &[tid]);
            old_row = versions.into_iter().next();
            child_row = Some(over(leaf, old_row.clone().unwrap_or_default(), &row[..width]));
        }
        if let Some(fire) = leaf.fire_of(event).filter(|t| t.before_rows()) {
            if old_row.is_none() {
                let versions = heap.versions(self.database_oid, leaf.oid, &[tid]);
                old_row = versions.into_iter().next();
            }
            let new = match &child_row {
                Some(child) => Some(child.clone()),
                None => update.then(|| down(leaf, &row[..width])),
            };
            let updated = if update { leaf_updated(leaf, &info.updated) } else { Vec::new() };
            let shape = (&updated[..], leaf.up.len());
            // `ExecBRUpdateTriggers`: the new row can move to another partition after the triggers.
            let mut stays = |_: &[Value]| Ok(true);
            let rows = (old_row.clone(), new);
            let Some(new) = self.leaf_before((fire, shape), rows, &leaf.qualified, &mut stays)?
            else {
                return Ok(None);
            };
            if update {
                row.splice(..width, up(leaf, &new, width));
            }
            if child_row.is_some() {
                child_row = Some(new);
            }
        }
        let mut item = Moved { seq, tid, shown: None, to: None, new: None, moves: false, update };
        if let Some(values) = child_row {
            // A row of a table of the tree of `INHERITS` stays in its table.
            item.new = Some(leaf_row(&leaves[at], &values, reader)?);
        } else if update {
            let (to, values) = router.route(table, row[..width].to_vec(), reader)?;
            let to =
                leaf_at(leaves, catalog, (table, to, *first), (&info.modified, policy), reader)?;
            if to == at {
                item.new = Some(leaf_row(&leaves[to], &values, reader)?);
            } else {
                item.moves = true;
                // `ExecCrossPartitionUpdate`: `ExecDelete` of the old row fires the triggers `BEFORE DELETE` of the old leaf, and a trigger can skip the row.
                leaves[at].arm(TRIGGER_DELETE, catalog, reader)?;
                leaves[to].arm(TRIGGER_INSERT, catalog, reader)?;
                let source = &leaves[at];
                if let Some(fire) = source.fire_of(TRIGGER_DELETE).filter(|t| t.before_rows()) {
                    let old_row = match old_row.take() {
                        Some(row) => row,
                        None => heap
                            .versions(self.database_oid, source.oid, &[tid])
                            .into_iter()
                            .next()
                            .unwrap_or_default(),
                    };
                    let mut stays = |_: &[Value]| Ok(true);
                    let shape = (&[][..], source.up.len());
                    let rows = (Some(old_row), None);
                    let fired =
                        self.leaf_before((fire, shape), rows, &source.qualified, &mut stays)?;
                    if fired.is_none() {
                        return Ok(None);
                    }
                }
                // `ExecInsert` of the new row fires the triggers `BEFORE INSERT` of the new leaf. A trigger that skips the row leaves the row deleted.
                let target = &leaves[to];
                let values = match target.fire_of(TRIGGER_INSERT).filter(|t| t.before_rows()) {
                    Some(fire) => {
                        let oid = target.oid;
                        let mut stays = |row: &[Value]| router.check(oid, row, reader);
                        let shape = (&[][..], target.up.len());
                        let rows = (None, Some(values));
                        self.leaf_before((fire, shape), rows, &target.qualified, &mut stays)?
                    }
                    None => Some(values),
                };
                if let Some(values) = values {
                    let checked = leaf_row(target, &values, reader)?;
                    if target.fire_of(TRIGGER_INSERT).is_some_and(Triggers::before_rows) {
                        router
                            .check_row(target.oid, &checked, reader)
                            .map_err(|e| root_detail(target, &checked, e, reader))?;
                    }
                    item.to = Some((to, checked));
                }
            }
        }
        Ok(Some((at, item)))
    }

    /// `table_tuple_update` and `table_tuple_delete` of the changes of each leaf in `groups`, with the checks of the unique keys of the leaf. A row that a transaction in progress changed waits for the end of that transaction. Each change that a leaf made goes in `applied` with the index of the leaf and the old row of the leaf, when `keep` or a trigger `AFTER ... FOR EACH ROW` of the leaf needs it. The result is true when a transaction that committed after the snapshot changed a row, and the statement must run again with a new snapshot.
    ///
    /// # Errors
    ///
    /// `23505` for a duplicate key, and the error of a wait.
    pub(super) fn change_leaves(
        &self,
        routing: &Routing,
        groups: Vec<Group>,
        view: &View,
        keep: bool,
        applied: &mut Vec<(usize, Moved, Option<Vec<Value>>)>,
    ) -> Result<bool, Error> {
        let heap = self.store.heap();
        for (at, items) in groups {
            let leaf = &routing.leaves[at];
            let Some(leaf_info) = leaf.plan.write() else {
                return Err(Error::internal("a partition with no plan of its rows"));
            };
            let changes: Vec<Change> =
                items.iter().map(|item| (item.tid, item.new.clone())).collect();
            self.heap_layout(leaf.oid);
            let (mut kept, mut made) = (Vec::new(), Vec::new());
            loop {
                let changed = heap.change(
                    leaf.oid,
                    view,
                    changes.clone(),
                    (&leaf_info.unique, &|d: &Derived, r: &[Value]| self.index_key(d, r)),
                    &kept,
                    &mut made,
                )?;
                match changed {
                    Changed::Done(_) => break,
                    Changed::Wait { holder, tid } => {
                        let update = items.iter().any(|item| item.tid == tid && item.update);
                        let what = if update { "updating" } else { "deleting" };
                        let context = format!(
                            "while {what} tuple {} in relation \"{}\"",
                            tid_text(tid),
                            leaf.name
                        );
                        self.reader().wait_xact(view.xid, holder, &context)?;
                    }
                    Changed::Again => return Ok(true),
                    conflict => kept = self.key_conflict(leaf_info, view, conflict)?,
                }
            }
            self.count_changes(leaf.oid, &changes, &made);
            let items: Vec<Moved> = made.iter().filter_map(|&i| items.get(i).cloned()).collect();
            let fired = |item: &Moved| leaf.fire_of(item.event()).is_some_and(Triggers::after_rows);
            let olds = if keep || items.iter().any(fired) {
                let tids: Vec<u64> = items.iter().map(|item| item.tid).collect();
                heap.versions(view.database, leaf.oid, &tids)
            } else {
                Vec::new()
            };
            let mut olds = olds.into_iter();
            applied.extend(items.into_iter().map(|item| (at, item, olds.next())));
        }
        Ok(false)
    }

    /// `ExecInsert` of the rows `rows` in their leaves after the changes of a statement, each with its place in the statement and the index of its leaf, with the checks of the unique keys of the leaves. Then the events `after` of the triggers `AFTER ... FOR EACH ROW` of the changes and the events of the triggers `AFTER INSERT ... FOR EACH ROW` of the new rows go in the queue in the order of the rows. The statement sets the columns `updated` of the partitioned table. The result has the new rows that the foreign keys of the leaves check, and the ID of the new version of each row with its place in the statement.
    ///
    /// # Errors
    ///
    /// `23505` for a duplicate key, and the error of the condition of `WHEN` of a trigger.
    pub(super) fn insert_moved(
        &self,
        routing: &mut Routing,
        view: &View,
        rows: Vec<(usize, usize, Vec<Value>)>,
        mut after: Vec<AfterRow>,
        updated: &[i16],
    ) -> Result<(Vec<Batch>, Vec<NewId>), Error> {
        // Each leaf writes its new rows in the order of the statement.
        let mut rows = rows;
        rows.sort_by_key(|row| row.0);
        let mut places = Vec::with_capacity(rows.len());
        for (seq, to, row) in rows {
            if routing.leaves[to].fire_of(TRIGGER_INSERT).is_some_and(Triggers::after_rows) {
                after.push((seq, to, TRIGGER_INSERT, (None, Some(row.clone()))));
            }
            places.push((seq, to, routing.leaves[to].rows.len()));
            routing.leaves[to].rows.push(row);
        }
        let batches = self.insert_leaves(routing, view, None)?;
        let ids = places
            .into_iter()
            .map(|(seq, to, place)| (seq, routing.leaves[to].ids.get(place).copied().unwrap_or(0)))
            .collect();
        // A sort that keeps the order of the events of a row: the event of the old leaf, then the event of the new leaf.
        after.sort_by_key(|event| event.0);
        for (_, at, event, rows) in after {
            routing.after_leaf(self, (at, event), &[rows], updated)?;
        }
        Ok((batches, ids))
    }
}

/// The rows of an `UPDATE` or a `DELETE` of a partitioned table that the leaves changed.
#[derive(Debug, Default)]
pub(super) struct ChangedRows {
    /// The number of the rows that changed.
    pub(super) count: u64,
    /// The rows for `RETURNING`: the columns of the partitioned table, the OID of the leaf, then the carried values.
    pub(super) written: Vec<Vec<Value>>,
    /// The old and the new rows with the columns of the partitioned table, for its transition tables.
    pub(super) old: Vec<Vec<Value>>,
    pub(super) new: Vec<Vec<Value>>,
    /// The view of the last run of the query.
    pub(super) view: View,
    /// The rows that the foreign keys of the leaves and of the partitioned table check.
    pub(super) batches: Vec<Batch>,
}

/// The rows of a leaf for the triggers of its foreign keys: the index of the leaf, the change, and the new and the old rows of the leaf.
type FkRows = (usize, FkChange, Vec<Vec<Value>>, Vec<Vec<Value>>);

/// The kind of a change of a row of a leaf for the triggers of its foreign keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FkChange {
    /// An `UPDATE` that keeps the row in its leaf.
    Update,
    /// A `DELETE`.
    Delete,
    /// An `UPDATE` that moves the row to another leaf, with a delete in the old leaf.
    Moved,
}

/// A change of a row of a partitioned table.
#[derive(Debug, Clone)]
pub(super) struct Moved {
    /// The place of the row in the statement.
    pub(super) seq: usize,
    /// The ID of the version of the old row in its leaf.
    tid: u64,
    /// The row for `RETURNING`: the new row of an `UPDATE` or the old row of a `DELETE`, the OID of its leaf, then the carried values.
    pub(super) shown: Option<Vec<Value>>,
    /// The new leaf and the checked new row with the columns of the leaf, for a row that moves. It is `None` for a row that moves when a trigger of the new leaf skips it.
    pub(super) to: Option<(usize, Vec<Value>)>,
    /// The checked new row of an `UPDATE` in the leaf.
    pub(super) new: Option<Vec<Value>>,
    /// True for a row that moves to another leaf.
    pub(super) moves: bool,
    /// True for an `UPDATE`, and false for a `DELETE`.
    update: bool,
}

impl Moved {
    /// The event of the triggers `AFTER` of the old leaf for the change: `UPDATE` in the leaf, or `DELETE` for a row that moves or that the statement deletes.
    pub(super) fn event(&self) -> i16 {
        if self.update && !self.moves { TRIGGER_UPDATE } else { TRIGGER_DELETE }
    }
}

/// The place of a row in the statement and the ID of its new version.
type NewId = (usize, u64);

/// The changes of the rows of one leaf, with the index of the leaf, in the order of the rows.
pub(super) type Group = (usize, Vec<Moved>);

/// An event of a trigger `AFTER ... FOR EACH ROW` of a leaf: the place of the row in the statement, the index of the leaf, the event and the old and the new rows of the leaf.
pub(super) type AfterRow = (usize, usize, i16, RowPair);

/// Adds the change `item` of the leaf at the place `at` to its group.
pub(super) fn group(groups: &mut Vec<Group>, at: usize, item: Moved) {
    match groups.iter_mut().find(|g| g.0 == at) {
        Some(group) => group.1.push(item),
        None => groups.push((at, vec![item])),
    }
}

/// The index in `leaves` of the leaf partition `oid` of the partitioned table `root`, with its plan made at the first row of the leaf.
fn leaf_at(
    leaves: &mut Vec<Leaf>,
    catalog: &Catalog,
    (root, oid, event): (u32, u32, i16),
    (modified, policy): (&[i16], &Option<RootPolicy>),
    reader: &Reader<'_>,
) -> Result<usize, Error> {
    if let Some(at) = leaves.iter().position(|leaf| leaf.oid == oid) {
        return Ok(at);
    }
    let mut new = leaf(catalog, (root, oid, event), reader)?;
    new.modified = modified.to_vec();
    new.policy.clone_from(policy);
    leaves.push(new);
    Ok(leaves.len() - 1)
}

/// A row of the partitioned table with the columns of the leaf `leaf`, as `execute_attr_map_slot` gives it.
fn down(leaf: &Leaf, row: &[Value]) -> Vec<Value> {
    leaf.up.iter().map(|i| i.and_then(|i| row.get(i).cloned()).unwrap_or(Value::Null)).collect()
}

/// The row `old` of the leaf `leaf` with the values of the columns of the row `row` of the parent table in place of its own values.
fn over(leaf: &Leaf, mut old: Vec<Value>, row: &[Value]) -> Vec<Value> {
    old.resize(leaf.up.len(), Value::Null);
    for (slot, to) in old.iter_mut().zip(&leaf.up) {
        if let Some(value) = to.and_then(|to| row.get(to)) {
            *slot = value.clone();
        }
    }
    old
}

/// A row of the leaf `leaf` with the `width` columns of the partitioned table.
fn up(leaf: &Leaf, row: &[Value], width: usize) -> Vec<Value> {
    let mut up = vec![Value::Null; width];
    for (value, &to) in row.iter().zip(&leaf.up) {
        if let Some(slot) = to.and_then(|to| up.get_mut(to)) {
            *slot = value.clone();
        }
    }
    up
}

/// The numbers of the columns of the leaf `leaf` that an `UPDATE` of the partitioned table sets in its columns `updated`.
fn leaf_updated(leaf: &Leaf, updated: &[i16]) -> Vec<i16> {
    (1..)
        .zip(&leaf.up)
        .filter(|(_, to)| {
            to.and_then(|to| i16::try_from(to + 1).ok()).is_some_and(|n| updated.contains(&n))
        })
        .map(|(number, _)| number)
        .collect()
}

/// The new row of the leaf partition `leaf` after the not-null and check constraints of the leaf, as `ExecConstraints` checks it.
fn leaf_row(leaf: &Leaf, values: &[Value], reader: &Reader<'_>) -> Result<Vec<Value>, Error> {
    leaf_row_with(leaf, values, leaf.policy.as_ref(), reader)
}

/// [`leaf_row`] with the checks `policy` of the policies of the partitioned table.
fn leaf_row_with(
    leaf: &Leaf,
    values: &[Value],
    policy: Option<&RootPolicy>,
    reader: &Reader<'_>,
) -> Result<Vec<Value>, Error> {
    // `ExecInsert` and `ExecUpdateAct` check the policies of the partitioned table on the row of the leaf before the constraints of the leaf.
    if let Some(policy) = policy {
        let row = up(leaf, values, leaf.root_types.len());
        let (check, name) = &**policy;
        if let Some(Some(error)) = rupg_exec::policy_failures(check, name, vec![row], reader)?.pop()
        {
            return Err(error);
        }
    }
    let params: Vec<Value> =
        leaf.live.iter().map(|&i| values.get(i).cloned().unwrap_or(Value::Null)).collect();
    let checked = leaf.plan.run_unchecked(&params, reader)?;
    // The row comes after the triggers of the leaf, so the checks run also for a leaf with triggers `BEFORE ... FOR EACH ROW`, which [`Plan::check`] leaves to [`Plan::recheck`].
    let mut rows =
        leaf.plan.check_rows(checked, reader).map_err(|e| root_detail(leaf, values, e, reader))?;
    rows.pop().ok_or_else(|| Error::internal("a check of a row with no row"))
}

/// `ExecConstraints` and `ExecPartitionCheckEmitError` for a row of a leaf that a statement on the partitioned table writes: the detail of the error shows the row with the columns of the partitioned table, as `execute_attr_map_slot` gives it.
fn root_detail(leaf: &Leaf, row: &[Value], error: Error, reader: &Reader<'_>) -> Error {
    let state = error.state();
    if !matches!(state, SqlState::CHECK_VIOLATION | SqlState::NOT_NULL_VIOLATION)
        || !error.message().contains(" violates ")
    {
        return error;
    }
    let shown = up(leaf, row, leaf.root_types.len());
    let of = (leaf.root, leaf.modified.as_slice());
    match rupg_exec::failing_row(&shown, &leaf.root_types, of, reader) {
        Ok(detail) => error.with_detail_opt(detail),
        Err(_) => error,
    }
}

/// The table `table` and all its partitions or its children of `INHERITS`, at each level.
fn tree(catalog: &Catalog, table: u32) -> Vec<u32> {
    let mut tables = vec![table];
    tables.extend(catalog.all_inheritors(table));
    tables
}

/// `ExecCrossPartitionUpdateForeignKey`: a row that moves out of the leaf `leaf` can not keep a foreign key that refers to an ancestor of the leaf below the table `root` of the statement, unless the foreign key is a clone of one on a table above.
///
/// # Errors
///
/// `0A000` for the first such ancestor, from the parent of the leaf up.
fn check_ancestor_keys(catalog: &Catalog, leaf: u32, root: u32) -> Result<(), Error> {
    for ancestor in catalog.partition_ancestors(leaf).into_iter().take_while(|&a| a != root) {
        if !catalog
            .constraints()
            .any(|c| c.parent == 0 && c.foreign.as_ref().is_some_and(|f| f.table == ancestor))
        {
            continue;
        }
        let name = |oid: u32| catalog.relation(oid).map(|r| r.name.clone()).unwrap_or_default();
        return Err(Error::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "cannot move tuple across partitions when a non-root ancestor of the source partition is directly referenced in a foreign key",
        )
        .with_detail(format!(
            "A foreign key points to ancestor \"{}\" but not the root ancestor \"{}\".",
            name(ancestor),
            name(root)
        ))
        .with_hint(format!("Consider defining the foreign key on table \"{}\".", name(root))));
    }
    Ok(())
}

/// True when a foreign key references the table `oid` or is a constraint of it.
pub(super) fn has_foreign(catalog: &Catalog, oid: u32) -> bool {
    catalog.constraints().any(|c| {
        (c.relation == oid && c.kind == ConKind::Foreign)
            || c.foreign.as_ref().is_some_and(|f| f.table == oid)
    })
}

/// The plan of the `INSERT` of the rows of the leaf partition `oid` of the partitioned table `root`, with the triggers of the leaf for the event `event`.
fn leaf(
    catalog: &Catalog,
    (root, oid, event): (u32, u32, i16),
    reader: &Reader<'_>,
) -> Result<Leaf, Error> {
    let Some(rel) = catalog.relation(oid) else {
        return Err(Error::internal(format!("partition {oid} is not in the catalog")));
    };
    let fire = if event == 0 {
        if catalog.triggers_of(oid).iter().any(|t| !t.internal) {
            return Err(not_yet("INSERT into a partitioned table whose partitions have triggers"));
        }
        Triggers::default()
    } else {
        armed(catalog, reader, oid, event)?
    };
    let live: Vec<usize> = (0..rel.columns.len())
        .filter(|&i| !rel.columns[i].dropped && rel.columns[i].generated == 0)
        .collect();
    let root_types = catalog.relation(root).map(rupg_exec::row_types).unwrap_or_default();
    let up = match catalog.relation(root) {
        Some(parent) => rel
            .columns
            .iter()
            .map(|c| {
                if c.dropped {
                    return None;
                }
                parent.columns.iter().position(|p| !p.dropped && p.name == c.name)
            })
            .collect(),
        None => Vec::new(),
    };
    let schemaname = match catalog.schema(rel.namespace) {
        Some(schema) => schema.name.clone(),
        None => builtin::named_by_oid(Named::Namespace, rel.namespace)
            .map_or_else(String::new, |n| n.name.to_string()),
    };
    let target = CopyTarget {
        relid: oid,
        name: rel.name.clone(),
        kind: b'r',
        columns: rel.columns.iter().map(|c| c.name.clone()).collect(),
        selected: live.clone(),
    };
    let rv = RangeVar {
        schemaname: Some(schemaname.as_str().into()),
        relname: Some(rel.name.as_str().into()),
        inh: false,
        location: -1,
        ..RangeVar::default()
    };
    let node = rupg_analyze::copy_insert(&target, &rv, &[]);
    let params =
        Params { types: live.iter().map(|&i| rel.columns[i].ty).collect(), variable: true };
    // `ExecCheckPermissions` checks the parent table, not the partition.
    let plan = query::plan_internal(&node, reader, &params)?.fold(None, reader)?;
    let qualified = format!("{schemaname}.{}", rel.name);
    Ok(Leaf {
        oid,
        name: rel.name.clone(),
        plan,
        live,
        up,
        rows: Vec::new(),
        lines: Vec::new(),
        ids: Vec::new(),
        fire,
        event,
        armed: Vec::new(),
        qualified,
        root_types,
        root,
        modified: Vec::new(),
        policy: None,
    })
}

/// The error of a feature of partitioned tables that the engine does not have yet.
pub(super) fn not_yet(what: &str) -> Error {
    Error::new(SqlState::FEATURE_NOT_SUPPORTED, format!("{what} is not supported yet"))
}
