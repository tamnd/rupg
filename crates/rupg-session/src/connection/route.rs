//! `INSERT` into a partitioned table: `ExecFindPartition` routes each row to the leaf partition that takes it, and the leaf checks and writes the row.

use rupg_analyze::{CopyTarget, Params, Write, WriteKind};
use rupg_catalog::{Catalog, ConKind};
use rupg_common::{Error, SqlState};
use rupg_exec::{Plan, Router};
use rupg_pgcatalog::builtin::{self, Named};
use rupg_sql::nodes::RangeVar;
use rupg_types::Value;

use super::Connection;
use super::foreign::Batch;
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
}

/// The routing of the rows of a statement into the leaf partitions of a partitioned table, and the rows of each leaf that the statement did not write yet.
#[derive(Debug, Default)]
pub(super) struct Routing {
    router: Router,
    leaves: Vec<Leaf>,
}

impl Routing {
    /// `ExecFindPartition` and `ExecConstraints` for a new row of the partitioned table `root`. The leaf keeps the checked row, and the result is the OID of the leaf.
    ///
    /// # Errors
    ///
    /// `23514` for a row that no partition takes, the errors of the constraints of the leaf, and `0A000` for a leaf with triggers.
    pub(super) fn add(
        &mut self,
        root: u32,
        row: Vec<Value>,
        catalog: &Catalog,
        reader: &Reader<'_>,
    ) -> Result<usize, Error> {
        let (at, checked) = self.route(root, row, catalog, reader)?;
        self.leaves[at].rows.push(checked);
        Ok(at)
    }

    /// `ExecFindPartition` and `ExecConstraints` for a new row of the partitioned table `root`. The result is the index of the leaf and the checked row with the columns of the leaf.
    ///
    /// # Errors
    ///
    /// `23514` for a row that no partition takes, the errors of the constraints of the leaf, and `0A000` for a leaf with triggers.
    pub(super) fn route(
        &mut self,
        root: u32,
        row: Vec<Value>,
        catalog: &Catalog,
        reader: &Reader<'_>,
    ) -> Result<(usize, Vec<Value>), Error> {
        let (oid, values) = self.router.route(root, row, reader)?;
        let at = leaf_at(&mut self.leaves, catalog, root, oid, reader)?;
        let checked = leaf_row(&self.leaves[at], &values, reader)?;
        Ok((at, checked))
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
        let values: Vec<Value> = leaf
            .up
            .iter()
            .map(|i| i.and_then(|i| row.get(i).cloned()).unwrap_or(Value::Null))
            .collect();
        if !self.router.check(leaf.oid, &values, reader)? {
            return Ok(None);
        }
        leaf_row(leaf, &values, reader).map(Some)
    }

    /// A row of the leaf at the place `at` with the `width` columns of the partitioned table, as `execute_attr_map_slot` gives it.
    pub(super) fn to_root(&self, at: usize, row: &[Value], width: usize) -> Vec<Value> {
        let mut up = vec![Value::Null; width];
        if let Some(leaf) = self.leaves.get(at) {
            for (value, &to) in row.iter().zip(&leaf.up) {
                if let Some(slot) = to.and_then(|to| up.get_mut(to)) {
                    *slot = value.clone();
                }
            }
        }
        up
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

    /// Keeps the line of the copy for the last row of the leaf at the place `at`.
    pub(super) fn add_line(&mut self, at: usize, line: u64) {
        if let Some(leaf) = self.leaves.get_mut(at) {
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
    /// The rows for `RETURNING`: the columns of the partitioned table, then the OID of the leaf.
    pub(super) written: Vec<Vec<Value>>,
    /// The new rows that the foreign keys of the leaves check.
    pub(super) batches: Vec<Batch>,
}

impl Connection {
    /// `ExecInsert` with `ExecFindPartition`: each row goes to its leaf partition, and the not-null and check constraints of the leaf check it, in the order of the rows. Then each leaf writes its rows and checks its unique keys.
    ///
    /// # Errors
    ///
    /// `23514` for a row that no partition takes, the errors of the constraints of the leaves, and `0A000` for a leaf with triggers.
    pub(super) fn insert_routed(
        &self,
        info: &Write,
        view: &View,
        rows: Vec<Vec<Value>>,
    ) -> Result<Routed, Error> {
        let catalog = self.catalog();
        let reader = self.reader();
        let mut routing = Routing::default();
        let mut written = Vec::new();
        for row in rows {
            let shown = info.returning.is_some().then(|| row[..info.width].to_vec());
            let at = routing.add(info.table, row, &catalog, &reader)?;
            if let Some(mut shown) = shown {
                shown.push(Value::Oid(routing.oid(at)));
                written.push(shown);
            }
        }
        drop(reader);
        let batches = self.insert_leaves(&mut routing, view, None)?;
        Ok(Routed { written, batches })
    }

    /// `table_tuple_insert` of the checked rows that each leaf keeps, with the checks of its unique keys. It gives the rows that the foreign keys of the leaves check. For a `COPY FROM` of the table `copy`, a duplicate key has the context of the line of its row.
    pub(super) fn insert_leaves(
        &self,
        routing: &mut Routing,
        view: &View,
        copy: Option<&str>,
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
                        let name = copy.unwrap_or_default();
                        return Err(error.with_context(format!("COPY {name}, line {line}")));
                    }
                    conflict => kept = self.key_conflict(leaf_info, view, conflict)?,
                }
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

    /// `ExecUpdate` and `ExecDelete` for a partitioned table: each row of the query gives the OID of its leaf and the ID of its version, and each leaf changes its rows. A new row that its leaf does not take moves, as `ExecCrossPartitionUpdate` moves it: the old leaf deletes the old row, and the row goes in the leaf that `ExecFindPartition` gives. It gives the count of the rows, the rows for `RETURNING` and the view of the last run of the query.
    ///
    /// # Errors
    ///
    /// The errors of the routing and of the constraints of the leaves, and `0A000` for a leaf with triggers or foreign keys.
    pub(super) fn change_routed(
        &self,
        (plan, info): (&Plan, &Write),
        params: &[Value],
        view: &View,
        rows: Vec<Vec<Value>>,
    ) -> Result<(u64, Vec<Vec<Value>>, View), Error> {
        let catalog = self.catalog();
        let Some(rel) = catalog.relation(info.table) else {
            return Err(Error::internal(format!("relation {} is not in the catalog", info.table)));
        };
        let width = rel.columns.len();
        let update = info.kind == WriteKind::Update;
        let returning = info.returning.is_some();
        let heap = self.store.heap();
        let mut view = view.clone();
        let mut routing = Routing::default();
        let Routing { router, leaves } = &mut routing;
        let (mut count, mut written) = (0, Vec::new());
        // The new rows that move to another leaf, after the old leaf deleted the old rows.
        let mut moved: Vec<(usize, Vec<Value>)> = Vec::new();
        let mut rows = rows;
        'run: loop {
            let reader = self.reader();
            // The changes of each old leaf, in the order of its first row, with the row for `RETURNING` and the moved row of each change.
            let mut groups: Vec<(usize, Vec<Change>, Vec<Moved>)> = Vec::new();
            for mut row in std::mem::take(&mut rows) {
                let (Some(Value::Int8(tid)), Some(Value::Oid(old))) = (row.pop(), row.pop()) else {
                    return Err(Error::internal("a row to change with no partition or version"));
                };
                let at = leaf_at(leaves, &catalog, info.table, old, &reader)?;
                let tid = tid.cast_unsigned();
                let (to, change, item) = if update {
                    let (to, values) = router.route(info.table, row[..width].to_vec(), &reader)?;
                    let to = leaf_at(leaves, &catalog, info.table, to, &reader)?;
                    let checked = leaf_row(&leaves[to], &values, &reader)?;
                    if to == at {
                        (to, (tid, Some(checked)), None)
                    } else {
                        (to, (tid, None), Some((to, checked)))
                    }
                } else {
                    (at, (tid, None), None)
                };
                if has_foreign(&catalog, leaves[at].oid) || has_foreign(&catalog, leaves[to].oid) {
                    return Err(not_yet(if update {
                        "UPDATE of a partitioned table with foreign keys"
                    } else {
                        "DELETE from a partitioned table with foreign keys"
                    }));
                }
                let shown = returning.then(|| {
                    let mut shown = row[..width].to_vec();
                    shown.push(Value::Oid(leaves[to].oid));
                    shown.extend_from_slice(&row[width..]);
                    shown
                });
                let item = Moved { shown, to: item };
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
                        &view,
                        changes.clone(),
                        &leaf_info.unique,
                        &kept,
                        &mut applied,
                    )?;
                    match changed {
                        Changed::Done(n) => {
                            count += n;
                            break;
                        }
                        Changed::Wait { holder, tid } => {
                            let what = if update { "updating" } else { "deleting" };
                            let context = format!(
                                "while {what} tuple {} in relation \"{}\"",
                                tid_text(tid),
                                leaf.name
                            );
                            self.reader().wait_xact(view.xid, holder, &context)?;
                        }
                        Changed::Again => {
                            // The rows that this command changed stay as they are in the new run.
                            let reader = self.reader();
                            rows = plan.run(params, &reader)?;
                            view = reader.view().cloned().unwrap_or_default();
                            continue 'run;
                        }
                        conflict => kept = self.key_conflict(leaf_info, &view, conflict)?,
                    }
                }
                for &i in &applied {
                    let Some(item) = items.get(i) else { continue };
                    written.extend(item.shown.clone());
                    moved.extend(item.to.clone());
                }
            }
            break;
        }
        // `ExecInsert` of the moved rows in their new leaves.
        for (to, row) in moved {
            leaves[to].rows.push(row);
        }
        self.insert_leaves(&mut routing, &view, None)?;
        Ok((count, written, view))
    }
}

/// A change of a row of a partitioned table.
struct Moved {
    /// The row for `RETURNING`: the new row of an `UPDATE` or the old row of a `DELETE`, the OID of its leaf, then the carried values.
    shown: Option<Vec<Value>>,
    /// The new leaf and the checked new row with the columns of the leaf, for a row that moves.
    to: Option<(usize, Vec<Value>)>,
}

/// The index in `leaves` of the leaf partition `oid` of the partitioned table `root`, with its plan made at the first row of the leaf.
fn leaf_at(
    leaves: &mut Vec<Leaf>,
    catalog: &Catalog,
    root: u32,
    oid: u32,
    reader: &Reader<'_>,
) -> Result<usize, Error> {
    if let Some(at) = leaves.iter().position(|leaf| leaf.oid == oid) {
        return Ok(at);
    }
    leaves.push(leaf(catalog, root, oid, reader)?);
    Ok(leaves.len() - 1)
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

/// The plan of the `INSERT` of the rows of the leaf partition `oid` of the partitioned table `root`.
fn leaf(catalog: &Catalog, root: u32, oid: u32, reader: &Reader<'_>) -> Result<Leaf, Error> {
    let Some(rel) = catalog.relation(oid) else {
        return Err(Error::internal(format!("partition {oid} is not in the catalog")));
    };
    if catalog.triggers_of(oid).iter().any(|t| !t.internal) {
        return Err(not_yet("INSERT into a partitioned table whose partitions have triggers"));
    }
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
    Ok(Leaf { oid, name: rel.name.clone(), plan, live, up, rows: Vec::new(), lines: Vec::new() })
}

/// The error of a feature of partitioned tables that the engine does not have yet.
pub(super) fn not_yet(what: &str) -> Error {
    Error::new(SqlState::FEATURE_NOT_SUPPORTED, format!("{what} is not supported yet"))
}
