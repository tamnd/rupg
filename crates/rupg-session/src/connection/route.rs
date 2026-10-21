//! `INSERT` into a partitioned table: `ExecFindPartition` routes each row to the leaf partition that takes it, and the leaf checks and writes the row.

use rupg_analyze::{CopyTarget, Params, Write};
use rupg_catalog::Catalog;
use rupg_common::{Error, SqlState};
use rupg_exec::{Plan, Router};
use rupg_pgcatalog::builtin::{self, Named};
use rupg_sql::nodes::RangeVar;
use rupg_types::Value;

use super::Connection;
use super::foreign::Batch;
use crate::heap::{Changed, View};
use crate::query::{self, Reader};

/// A leaf partition of the rows of a statement, as `ExecInitPartitionInfo` gives its `ResultRelInfo`.
struct Leaf {
    oid: u32,
    /// The `INSERT` into the leaf, from [`rupg_analyze::copy_insert`], with a parameter for each column that is not dropped.
    plan: Plan,
    /// The columns of the leaf that are not dropped.
    live: Vec<usize>,
    /// The checked rows for the leaf.
    rows: Vec<Vec<Value>>,
}

/// The rows of an `INSERT` into a partitioned table that the leaves wrote.
pub(super) struct Routed {
    /// The rows for `RETURNING`: the columns of the partitioned table, a null `ctid` and the OID of the leaf as `tableoid`.
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
        let mut router = Router::default();
        let mut leaves: Vec<Leaf> = Vec::new();
        let mut written = Vec::new();
        for row in rows {
            let (oid, values) = router.route(info.table, row.clone(), &reader)?;
            let at = match leaves.iter().position(|leaf| leaf.oid == oid) {
                Some(at) => at,
                None => {
                    leaves.push(leaf(&catalog, oid, &reader)?);
                    leaves.len() - 1
                }
            };
            let leaf = &mut leaves[at];
            let params: Vec<Value> =
                leaf.live.iter().map(|&i| values.get(i).cloned().unwrap_or(Value::Null)).collect();
            let checked = leaf.plan.run_unchecked(&params, &reader)?;
            leaf.rows.extend(leaf.plan.check(checked, &reader)?);
            if info.returning.is_some() {
                let mut row = row;
                row.truncate(info.width);
                row.extend([Value::Null, Value::Oid(oid)]);
                written.push(row);
            }
        }
        drop(reader);
        let heap = self.store.heap();
        let mut batches = Vec::new();
        for leaf in leaves {
            let Some(leaf_info) = leaf.plan.write() else {
                return Err(Error::internal("a partition with no plan of its rows"));
            };
            if info.on_conflict.is_some() && !leaf_info.unique.is_empty() {
                return Err(not_yet("ON CONFLICT on a partitioned table with unique keys"));
            }
            let checked = if leaf_info.foreign.is_empty() { Vec::new() } else { leaf.rows.clone() };
            let mut kept = Vec::new();
            loop {
                match heap.insert(leaf.oid, view, leaf.rows.clone(), &leaf_info.unique, &kept)? {
                    Changed::Done(_) => break,
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
        Ok(Routed { written, batches })
    }
}

/// The plan of the `INSERT` of the rows of the leaf partition `oid`.
fn leaf(catalog: &Catalog, oid: u32, reader: &Reader<'_>) -> Result<Leaf, Error> {
    let Some(rel) = catalog.relation(oid) else {
        return Err(Error::internal(format!("partition {oid} is not in the catalog")));
    };
    if catalog.triggers_of(oid).iter().any(|t| !t.internal) {
        return Err(not_yet("INSERT into a partitioned table whose partitions have triggers"));
    }
    let live: Vec<usize> = (0..rel.columns.len()).filter(|&i| !rel.columns[i].dropped).collect();
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
    Ok(Leaf { oid, plan, live, rows: Vec::new() })
}

/// The error of a feature of partitioned tables that the engine does not have yet.
fn not_yet(what: &str) -> Error {
    Error::new(SqlState::FEATURE_NOT_SUPPORTED, format!("{what} is not supported yet"))
}
