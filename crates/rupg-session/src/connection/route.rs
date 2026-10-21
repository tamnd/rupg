//! `INSERT` into a partitioned table: `ExecFindPartition` routes each row to the leaf partition that takes it, and the leaf checks and writes the row.

use rupg_analyze::{CopyTarget, Params, Write, WriteKind};
use rupg_catalog::{Catalog, ConKind, TRIGGER_DELETE, TRIGGER_INSERT, TRIGGER_UPDATE};
use rupg_common::{Error, SqlState};
use rupg_exec::{Plan, Router};
use rupg_pgcatalog::builtin::{self, Named};
use rupg_sql::nodes::RangeVar;
use rupg_types::Value;

use super::Connection;
use super::foreign::Batch;
use super::trigger::{RowPair, Triggers, armed};
use crate::heap::{Change, Changed, View, tid_text};
use crate::query::{self, Reader};

/// A leaf partition of the rows of a statement, as `ExecInitPartitionInfo` gives its `ResultRelInfo`.
#[derive(Debug)]
struct Leaf {
    oid: u32,
    name: String,
    /// The `INSERT` into the leaf, from [`rupg_analyze::copy_insert`], with a parameter for each column that is not dropped.
    plan: Plan,
    /// The columns of the leaf that are not dropped.
    live: Vec<usize>,
    /// For each column of the leaf, the column of the partitioned table of the statement with the same name. A dropped column has none.
    up: Vec<Option<usize>>,
    /// The checked rows for the leaf.
    rows: Vec<Vec<Value>>,
    /// The line of each row of a `COPY FROM`.
    lines: Vec<u64>,
    /// The triggers `FOR EACH ROW` of the leaf for the event of the statement.
    fire: Triggers,
    /// The event of `fire`.
    event: i16,
    /// The triggers of the leaf for the other events of the statement, with each event.
    armed: Vec<(i16, Triggers)>,
    /// The name of the leaf with its schema.
    qualified: String,
}

/// The routing of the rows of a statement into the leaf partitions of a partitioned table, and the rows of each leaf that the statement did not write yet.
#[derive(Debug, Default)]
pub(super) struct Routing {
    router: Router,
    leaves: Vec<Leaf>,
    /// The bit in `tgtype` of the event whose triggers of the leaves fire, or 0 when a leaf with triggers gives an error.
    event: i16,
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
    pub(super) fn of(event: i16) -> Routing {
        Routing { event, ..Routing::default() }
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
        let at = leaf_at(&mut self.leaves, catalog, (root, oid, self.event), reader)?;
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
        router.check_row(oid, &checked, reader)?;
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

    /// `ExecPartitionCheck` and `ExecConstraints` for a new row of the partitioned table that changes a row of the leaf at the place `at`. The result is the checked row with the columns of the leaf, or `None` when the leaf does not take the row.
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
        leaf_row(leaf, &values, reader).map(Some)
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
        let mut routing = Routing::of(TRIGGER_INSERT);
        let (mut written, mut inserted) = (Vec::new(), Vec::new());
        // The new rows of the leaves with triggers `AFTER ... FOR EACH ROW`.
        let mut after = Vec::new();
        let mut count = 0;
        for row in rows {
            let fired = routing.route_fired(self, (info.table, row), &catalog, &reader)?;
            let Some((at, checked)) = fired else { continue };
            count += 1;
            if info.returning.is_some() || keep {
                let mut shown = routing.to_root(at, &checked, info.width);
                if keep {
                    inserted.push(shown.clone());
                }
                if info.returning.is_some() {
                    shown.push(Value::Oid(routing.oid(at)));
                    written.push(shown);
                }
            }
            if routing.leaves[at].fire.after_rows() {
                after.push((at, checked.clone()));
            }
            routing.leaves[at].rows.push(checked);
        }
        drop(reader);
        let batches = self.insert_leaves(&mut routing, view, None)?;
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
            let mut kept = Vec::new();
            loop {
                match heap.insert(leaf.oid, view, rows.clone(), &leaf_info.unique, &kept)? {
                    Changed::Done(_) => break,
                    Changed::Duplicate { index, key } if copy.is_some() => {
                        let line =
                            super::copy_from::duplicate_line(leaf_info, index, &key, &rows, &lines);
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

    /// `ExecUpdate` and `ExecDelete` for a partitioned table: each row of the query gives the OID of its leaf and the ID of its version, and each leaf changes its rows. The triggers `BEFORE ... FOR EACH ROW` of the leaf fire for each row first. A new row that its leaf does not take moves, as `ExecCrossPartitionUpdate` moves it: the triggers `BEFORE DELETE` of the old leaf and `BEFORE INSERT` of the new leaf fire, the old leaf deletes the old row, and the row goes in the leaf that `ExecFindPartition` gives. The events of the triggers `AFTER ... FOR EACH ROW` of the leaves go in the queue in the order of the rows. With `keep`, the result has the old and the new rows with the columns of the partitioned table, for its transition tables.
    ///
    /// # Errors
    ///
    /// The errors of the routing, of the triggers and of the constraints of the leaves, and `0A000` for a leaf with foreign keys.
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
        let returning = info.returning.is_some();
        let heap = self.store.heap();
        let mut done = ChangedRows { view: view.clone(), ..ChangedRows::default() };
        let event = if update { TRIGGER_UPDATE } else { TRIGGER_DELETE };
        let mut routing = Routing::of(event);
        let Routing { router, leaves, .. } = &mut routing;
        // The new rows that move to another leaf, after the old leaf deleted the old rows, with the place of each row in the statement.
        let mut moved: Vec<(usize, usize, Vec<Value>)> = Vec::new();
        // The events of the triggers `AFTER ... FOR EACH ROW` and the rows of the transition tables, with the place of each row in the statement.
        let mut after: Vec<(usize, usize, i16, RowPair)> = Vec::new();
        let (mut old_rows, mut new_rows) = (Vec::new(), Vec::new());
        let mut rows = rows;
        let mut seq = 0;
        'run: loop {
            let reader = self.reader();
            // The changes of each old leaf, in the order of its first row, with the item of each change.
            let mut groups: Vec<(usize, Vec<Change>, Vec<Moved>)> = Vec::new();
            for mut row in std::mem::take(&mut rows) {
                seq += 1;
                let (Some(Value::Int8(tid)), Some(Value::Oid(old))) = (row.pop(), row.pop()) else {
                    return Err(Error::internal("a row to change with no partition or version"));
                };
                let at = leaf_at(leaves, &catalog, (info.table, old, event), &reader)?;
                let tid = tid.cast_unsigned();
                let leaf = &leaves[at];
                // The old row of the leaf for the triggers.
                let mut old_row = None;
                if leaf.fire.before_rows() {
                    let versions = heap.versions(self.database_oid, leaf.oid, &[tid]);
                    old_row = versions.into_iter().next();
                    let new = update.then(|| down(leaf, &row[..width]));
                    let updated = leaf_updated(leaf, &info.updated);
                    let shape = (&updated[..], leaf.up.len());
                    // `ExecBRUpdateTriggers`: the new row can move to another partition after the triggers.
                    let mut stays = |_: &[Value]| Ok(true);
                    let rows = (old_row.clone(), new);
                    let fire = (&leaf.fire, shape);
                    let Some(new) = self.leaf_before(fire, rows, &leaf.qualified, &mut stays)?
                    else {
                        continue;
                    };
                    if update {
                        row.splice(..width, up(leaf, &new, width));
                    }
                }
                let mut item = Moved { seq, shown: None, to: None, new: None, moves: false };
                let to = if update {
                    let (to, values) = router.route(info.table, row[..width].to_vec(), &reader)?;
                    let to = leaf_at(leaves, &catalog, (info.table, to, event), &reader)?;
                    if to == at {
                        item.new = Some(leaf_row(&leaves[to], &values, &reader)?);
                    } else {
                        item.moves = true;
                        // `ExecCrossPartitionUpdate`: `ExecDelete` of the old row fires the triggers `BEFORE DELETE` of the old leaf, and a trigger can skip the row.
                        leaves[at].arm(TRIGGER_DELETE, &catalog, &reader)?;
                        leaves[to].arm(TRIGGER_INSERT, &catalog, &reader)?;
                        let source = &leaves[at];
                        if let Some(fire) =
                            source.fire_of(TRIGGER_DELETE).filter(|t| t.before_rows())
                        {
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
                            let fired = self.leaf_before(
                                (fire, shape),
                                rows,
                                &source.qualified,
                                &mut stays,
                            )?;
                            if fired.is_none() {
                                continue;
                            }
                        }
                        // `ExecInsert` of the new row fires the triggers `BEFORE INSERT` of the new leaf. A trigger that skips the row leaves the row deleted.
                        let target = &leaves[to];
                        let values =
                            match target.fire_of(TRIGGER_INSERT).filter(|t| t.before_rows()) {
                                Some(fire) => {
                                    let oid = target.oid;
                                    let mut stays = |row: &[Value]| router.check(oid, row, &reader);
                                    let shape = (&[][..], target.up.len());
                                    let rows = (None, Some(values));
                                    self.leaf_before(
                                        (fire, shape),
                                        rows,
                                        &target.qualified,
                                        &mut stays,
                                    )?
                                }
                                None => Some(values),
                            };
                        if let Some(values) = values {
                            let checked = leaf_row(target, &values, &reader)?;
                            if target.fire_of(TRIGGER_INSERT).is_some_and(Triggers::before_rows) {
                                router.check_row(target.oid, &checked, &reader)?;
                            }
                            item.to = Some((to, checked));
                        }
                    }
                    to
                } else {
                    at
                };
                if has_foreign(&catalog, leaves[at].oid) || has_foreign(&catalog, leaves[to].oid) {
                    return Err(not_yet(if update {
                        "UPDATE of a partitioned table with foreign keys"
                    } else {
                        "DELETE from a partitioned table with foreign keys"
                    }));
                }
                if returning && (!item.moves || item.to.is_some()) {
                    let mut shown = match &item.to {
                        Some((to, checked)) => up(&leaves[*to], checked, width),
                        None => row[..width].to_vec(),
                    };
                    shown.push(Value::Oid(leaves[to].oid));
                    shown.extend_from_slice(&row[width..]);
                    item.shown = Some(shown);
                }
                let change = (tid, item.new.clone());
                match groups.iter_mut().find(|g| g.0 == at) {
                    Some(group) => {
                        group.1.push(change);
                        group.2.push(item);
                    }
                    None => groups.push((at, vec![change], vec![item])),
                }
            }
            drop(reader);
            for (at, changes, items) in groups {
                let leaf = &leaves[at];
                let Some(leaf_info) = leaf.plan.write() else {
                    return Err(Error::internal("a partition with no plan of its rows"));
                };
                let (mut kept, mut applied) = (Vec::new(), Vec::new());
                loop {
                    let changed = heap.change(
                        leaf.oid,
                        &done.view,
                        changes.clone(),
                        &leaf_info.unique,
                        &kept,
                        &mut applied,
                    )?;
                    match changed {
                        Changed::Done(_) => break,
                        Changed::Wait { holder, tid } => {
                            let what = if update { "updating" } else { "deleting" };
                            let context = format!(
                                "while {what} tuple {} in relation \"{}\"",
                                tid_text(tid),
                                leaf.name
                            );
                            self.reader().wait_xact(done.view.xid, holder, &context)?;
                        }
                        Changed::Again => {
                            // The rows that this command changed stay as they are in the new run.
                            let reader = self.reader();
                            rows = plan.run(params, &reader)?;
                            done.view = reader.view().cloned().unwrap_or_default();
                            continue 'run;
                        }
                        conflict => kept = self.key_conflict(leaf_info, &done.view, conflict)?,
                    }
                }
                // The event of the triggers `AFTER` of the old leaf for each change: `UPDATE` in the leaf, or `DELETE` for a row that moves or that the statement deletes.
                let of = |item: &Moved| {
                    if update && !item.moves { TRIGGER_UPDATE } else { TRIGGER_DELETE }
                };
                let items: Vec<Moved> =
                    applied.iter().filter_map(|&i| items.get(i).cloned()).collect();
                let fired = |item: &Moved| leaf.fire_of(of(item)).is_some_and(Triggers::after_rows);
                let olds = if keep || items.iter().any(&fired) {
                    let tids: Vec<u64> =
                        applied.iter().filter_map(|&i| changes.get(i).map(|c| c.0)).collect();
                    heap.versions(done.view.database, leaf.oid, &tids)
                } else {
                    Vec::new()
                };
                let mut olds = olds.into_iter();
                for item in items {
                    let old = olds.next();
                    if !item.moves || item.to.is_some() {
                        done.count += 1;
                    }
                    done.written.extend(item.shown.clone());
                    if keep && let Some(old) = &old {
                        old_rows.push((item.seq, up(leaf, old, width)));
                        if let Some(new) = &item.new {
                            new_rows.push((item.seq, up(leaf, new, width)));
                        }
                    }
                    if fired(&item) {
                        after.push((item.seq, at, of(&item), (old, item.new.clone())));
                    }
                    if let Some((to, checked)) = item.to {
                        moved.push((item.seq, to, checked));
                    }
                }
            }
            break;
        }
        // `ExecInsert` of the moved rows in their new leaves.
        let mut inserted = Vec::new();
        for (seq, to, row) in moved {
            if keep {
                new_rows.push((seq, up(&leaves[to], &row, width)));
            }
            if leaves[to].fire_of(TRIGGER_INSERT).is_some_and(Triggers::after_rows) {
                inserted.push((seq, to, TRIGGER_INSERT, (None, Some(row.clone()))));
            }
            leaves[to].rows.push(row);
        }
        self.insert_leaves(&mut routing, &done.view, None)?;
        after.extend(inserted);
        // A sort that keeps the order of the events of a row: the event of the old leaf, then the event of the new leaf.
        after.sort_by_key(|event| event.0);
        for (_, at, event, rows) in after {
            routing.after_leaf(self, (at, event), &[rows], &info.updated)?;
        }
        old_rows.sort_by_key(|row| row.0);
        new_rows.sort_by_key(|row| row.0);
        done.old = old_rows.into_iter().map(|row| row.1).collect();
        done.new = new_rows.into_iter().map(|row| row.1).collect();
        Ok(done)
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
}

/// A change of a row of a partitioned table.
#[derive(Debug, Clone)]
struct Moved {
    /// The place of the row in the statement.
    seq: usize,
    /// The row for `RETURNING`: the new row of an `UPDATE` or the old row of a `DELETE`, the OID of its leaf, then the carried values.
    shown: Option<Vec<Value>>,
    /// The new leaf and the checked new row with the columns of the leaf, for a row that moves. It is `None` for a row that moves when a trigger of the new leaf skips it.
    to: Option<(usize, Vec<Value>)>,
    /// The checked new row of an `UPDATE` in the leaf.
    new: Option<Vec<Value>>,
    /// True for a row that moves to another leaf.
    moves: bool,
}

/// The index in `leaves` of the leaf partition `oid` of the partitioned table `root`, with its plan made at the first row of the leaf.
fn leaf_at(
    leaves: &mut Vec<Leaf>,
    catalog: &Catalog,
    (root, oid, event): (u32, u32, i16),
    reader: &Reader<'_>,
) -> Result<usize, Error> {
    if let Some(at) = leaves.iter().position(|leaf| leaf.oid == oid) {
        return Ok(at);
    }
    leaves.push(leaf(catalog, (root, oid, event), reader)?);
    Ok(leaves.len() - 1)
}

/// A row of the partitioned table with the columns of the leaf `leaf`, as `execute_attr_map_slot` gives it.
fn down(leaf: &Leaf, row: &[Value]) -> Vec<Value> {
    leaf.up.iter().map(|i| i.and_then(|i| row.get(i).cloned()).unwrap_or(Value::Null)).collect()
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
    let params: Vec<Value> =
        leaf.live.iter().map(|&i| values.get(i).cloned().unwrap_or(Value::Null)).collect();
    let checked = leaf.plan.run_unchecked(&params, reader)?;
    let mut rows = leaf.plan.check(checked, reader)?;
    rows.pop().ok_or_else(|| Error::internal("a check of a row with no row"))
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
    let live: Vec<usize> = (0..rel.columns.len()).filter(|&i| !rel.columns[i].dropped).collect();
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
    let plan = query::plan(&node, reader, &params)?.fold(None, reader)?;
    let qualified = format!("{schemaname}.{}", rel.name);
    Ok(Leaf {
        oid,
        name: rel.name.clone(),
        plan,
        live,
        up,
        rows: Vec::new(),
        lines: Vec::new(),
        fire,
        event,
        armed: Vec::new(),
        qualified,
    })
}

/// The error of a feature of partitioned tables that the engine does not have yet.
pub(super) fn not_yet(what: &str) -> Error {
    Error::new(SqlState::FEATURE_NOT_SUPPORTED, format!("{what} is not supported yet"))
}
