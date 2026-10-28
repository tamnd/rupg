//! The rows of a table after a statement that changes the table, as `ATRewriteTables` makes them: a new column gets its default in each row, a column with a new type gets the value of its `USING` expression or its old value with the new type, and the rows must pass the new not-null constraints, the new check constraints, the new unique indexes and the new foreign keys.
//!
//! PostgreSQL writes the rows to a new file, or it only reads them when no row changes. rupg deletes each row and inserts its new version in the same command, so a rollback brings back the old rows.

use std::sync::Arc;

use rupg_analyze::{CopyTarget, Derived, Fill, Params, PartitionCheck, Repack, Transform};
use rupg_catalog::{Catalog, ConKind, FIRST_NORMAL_OID, RelKind, Relation};
use rupg_common::{Error, Level, SqlState};
use rupg_pgcatalog::builtin::{self, Named};
use rupg_sql::Severity;
use rupg_sql::nodes::{Node, RangeVar};
use rupg_types::{Record, Value};

use super::Connection;
use super::foreign::Batch;
use crate::heap::{Changed, View};
use crate::query;
use crate::utility::Notice;

/// An `INFO` of `VERBOSE`.
fn info(message: String) -> Notice {
    let error = Error::new(SqlState::SUCCESSFUL_COMPLETION, message).with_level(Level::Info);
    Notice { severity: Severity::Notice, error }
}

impl Connection {
    /// The tables and the materialized views whose rows the statement `node` changes, from the catalog before the statement to `work`. A table with no rows needs no change. A table with an expression of `transforms` or a default of `fills` always gets its rows again.
    ///
    /// # Errors
    ///
    /// `0A000` for a change that the rows cannot get yet: `ALTER DOMAIN` on a table with rows, and a change of the type of a column by a statement that is not `ALTER TABLE`.
    pub(super) fn rows_to_rewrite(
        &self,
        node: &Node,
        work: &Catalog,
        transforms: &[Transform],
        fills: &[Fill],
    ) -> Result<Vec<u32>, Error> {
        let heap = self.store.heap();
        let old = self.catalog();
        let tables: Vec<&Relation> = old
            .relations()
            .filter(|rel| {
                matches!(rel.kind, RelKind::Table | RelKind::MatView)
                    && rel.oid >= FIRST_NORMAL_OID
                    && heap.has_rows(self.database_oid, rel.oid)
            })
            .collect();
        let not_yet = |statement: &str| {
            Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!("{statement} on a table with rows is not supported yet"),
            )
        };
        if matches!(node, Node::AlterDomainStmt(_)) {
            return match tables.first() {
                Some(_) => Err(not_yet("ALTER DOMAIN")),
                None => Ok(Vec::new()),
            };
        }
        // A value of a row type keeps a field for each column of its relation, so a table with such values in its rows gets them again when the relation gets or loses a column.
        let reshaped = reshaped_row_types(&old, work);
        let holds = |rel: &Relation| {
            !reshaped.is_empty()
                && rel.columns.iter().any(|c| holds_row_type(&old, c.ty, &reshaped, 0))
        };
        let mut out = Vec::new();
        let transformed = |rel: &Relation| {
            transforms.iter().any(|t| t.table == rel.oid)
                || fills.iter().any(|f| f.table == rel.oid)
        };
        for rel in tables
            .into_iter()
            .filter(|rel| transformed(rel) || changes_rows(&old, work, rel) || holds(rel))
        {
            let Some(now) = work.relation(rel.oid) else { continue };
            let same = now.columns.len() >= rel.columns.len()
                && (matches!(node, Node::AlterTableStmt(_))
                    || rel.columns.iter().zip(&now.columns).all(|(before, after)| {
                        after.dropped
                            || before.ty == after.ty
                                && before.typmod == after.typmod
                                && before.collation == after.collation
                    }));
            if !same {
                return Err(not_yet(match node {
                    Node::IndexStmt(_) => "CREATE INDEX",
                    Node::AlterTableStmt(_) => "ALTER TABLE",
                    _ => "this statement",
                }));
            }
            out.push(rel.oid);
        }
        Ok(out)
    }

    /// `ATRewriteTable` for the tables: each row gets the new columns, then the checks of the new constraints. The first write of a transaction gives it a transaction ID, and the statement ends a command. When another transaction in progress changed a table, the statement waits for its end, as the lock of `ALTER TABLE` waits.
    /// The part of `ATRewriteTable` for the partition constraints that `ATTACH PARTITION` queues: each row of each leaf partition of each partition of `checks` must be in the bounds of its partition, and of each partition above it, in the catalog `work`.
    ///
    /// # Errors
    ///
    /// `23514` for a row that is not in the bounds, and the errors of the expressions of the partition keys.
    pub(super) fn check_partitions(
        &self,
        checks: &[PartitionCheck],
        work: &Arc<Catalog>,
    ) -> Result<(), Error> {
        if checks.is_empty() {
            return Ok(());
        }
        let heap = self.store.heap();
        let reader = self.reader_on(Arc::clone(work));
        let view = View { xid: self.xid.get(), ..self.view() };
        let mut router = rupg_exec::Router::default();
        for check in checks {
            let partitioned =
                work.relation(check.table).is_some_and(|r| r.kind == RelKind::Partitioned);
            let leaves = if partitioned {
                rupg_exec::partition_leaves(check.table, &reader)?
            } else {
                vec![check.table]
            };
            for leaf in leaves {
                for row in heap.rows(leaf, &view) {
                    if router.check(leaf, &row, &reader)? {
                        continue;
                    }
                    let name = work.relation(leaf).map_or("", |r| r.name.as_str());
                    let message = if check.default {
                        format!(
                            "updated partition constraint for default partition \"{name}\" would be violated by some row"
                        )
                    } else {
                        format!(
                            "partition constraint of relation \"{name}\" is violated by some row"
                        )
                    };
                    let error = Error::new(SqlState::CHECK_VIOLATION, message);
                    return Err(rupg_func::with_table(error, leaf, &self.reader()));
                }
            }
        }
        Ok(())
    }

    pub(super) fn rewrite_rows(
        &self,
        tables: &[u32],
        work: &Arc<Catalog>,
        transforms: &[Transform],
        fills: &[Fill],
    ) -> Result<(), Error> {
        if tables.is_empty() {
            return Ok(());
        }
        let heap = self.store.heap();
        if self.xid.get() == 0 {
            self.xid.set(heap.begin(self.pid));
        }
        let old = self.catalog();
        let command = self.command.get();
        for &table in tables {
            let Some(rel) = old.relation(table) else { continue };
            let in_place = keeps_rows(&old, work, rel, transforms, fills);
            loop {
                let view = View { xid: self.xid.get(), ..self.view() };
                let holder =
                    if in_place { heap.holder(table, &view) } else { heap.truncate(table, &view) };
                let Some(holder) = holder else { break };
                self.reader().wait_xact(view.xid, holder, "")?;
            }
            // The rows that the truncate deleted stay visible to the command that deleted them.
            let view = View { xid: self.xid.get(), ..self.view() };
            let rows = heap.rows(table, &view);
            // The `INSERT` of the rows gives a new column of `fills` the default that it had when `ADD COLUMN` ran.
            let own: Vec<&Fill> = fills.iter().filter(|f| f.table == table).collect();
            let filled;
            let work = if own.is_empty() {
                work
            } else {
                let mut catalog = Catalog::clone(work);
                for fill in own {
                    catalog.remove_default(table, fill.column);
                    if let Some(expr) = &fill.default {
                        catalog.add_default(table, fill.column, expr.clone(), &[])?;
                    }
                }
                filled = Arc::new(catalog);
                &filled
            };
            self.rewrite_table(&old, rel, work, (&view, in_place), rows, transforms)?;
        }
        // A function in an index expression can end commands after the command of the rewrite.
        self.command.set(self.command.get().max(command + 1));
        Ok(())
    }

    /// `copy_table_data` of `CLUSTER` and `REPACK`: the rows of each table of `repacked` go again into the table, in the order of the keys of the index or in their physical order, with the messages of `VERBOSE`. rupg removes no dead row version, as each one is gone already or visible to a transaction.
    pub(super) fn repack_rows(
        &self,
        repacked: &[Repack],
        work: &Arc<Catalog>,
        notices: &mut Vec<Notice>,
    ) -> Result<(), Error> {
        if repacked.is_empty() {
            return Ok(());
        }
        let heap = self.store.heap();
        if self.xid.get() == 0 {
            self.xid.set(heap.begin(self.pid));
        }
        let old = self.catalog();
        let command = self.command.get();
        for repack in repacked {
            let table = repack.table;
            let Some(rel) = old.relation(table) else { continue };
            let pages = heap.size(self.database_oid, table).blocks;
            loop {
                let view = View { xid: self.xid.get(), ..self.view() };
                let Some(holder) = heap.truncate(table, &view) else { break };
                self.reader().wait_xact(view.xid, holder, "")?;
            }
            let view = View { xid: self.xid.get(), ..self.view() };
            let mut rows = heap.rows(table, &view);
            if let Some(order) = &repack.order {
                rows = rupg_exec::cluster_order(
                    order,
                    rows,
                    &self.reader_without_view(Arc::clone(work)),
                )?;
            }
            let count = rows.len();
            if let Some(message) = &repack.verbose {
                notices.push(info(message.clone()));
            }
            self.rewrite_table(&old, rel, work, (&view, false), rows, &[])?;
            if repack.verbose.is_some() {
                let mut notice = info(format!(
                    "\"{}\": found 0 removable, {count} nonremovable row versions in {pages} pages",
                    repack.shown
                ));
                notice.error = notice.error.with_detail(
                    "0 dead row versions cannot be removed yet.\nCPU: user: 0.00 s, system: 0.00 s, elapsed: 0.00 s.",
                );
                notices.push(notice);
            }
        }
        self.command.set(self.command.get().max(command + 1));
        Ok(())
    }

    /// The new versions of the rows `rows` of the table `rel`, with the ID of the old version at the end of each row. The `INSERT` of the old columns gives the new columns their defaults, as the expressions of `ATRewriteTable` do. When a column has a new type or an expression of `transforms`, the `INSERT` reads the old values with their old types, and the expressions give the new values. With `in_place`, the rows stay and the statement only checks them, as `ATRewriteTable` does with no new file and as `index_build` checks the keys of a new unique index.
    fn rewrite_table(
        &self,
        old: &Catalog,
        rel: &Relation,
        work: &Arc<Catalog>,
        (view, in_place): (&View, bool),
        rows: Vec<Vec<Value>>,
        transforms: &[Transform],
    ) -> Result<(), Error> {
        let Some(now) = work.relation(rel.oid) else { return Ok(()) };
        let width = rel.columns.len();
        // A dropped column has no value in the new rows. The `INSERT` gives it a null, and it computes a generated column again.
        let live: Vec<usize> = (0..width)
            .filter(|&i| {
                !rel.columns[i].dropped
                    && now.columns.get(i).is_some_and(|c| !c.dropped && c.generated == 0)
            })
            .collect();
        let schemaname = match work.schema(now.namespace) {
            Some(schema) => schema.name.clone(),
            None => builtin::named_by_oid(Named::Namespace, now.namespace)
                .map_or_else(String::new, |n| n.name.to_string()),
        };
        let target = CopyTarget {
            relid: rel.oid,
            name: now.name.clone(),
            kind: b'r',
            columns: now.columns.iter().map(|c| c.name.clone()).collect(),
            selected: live.clone(),
        };
        let rv = RangeVar {
            schemaname: Some(schemaname.as_str().into()),
            relname: Some(now.name.as_str().into()),
            inh: false,
            location: -1,
            ..RangeVar::default()
        };
        let changed = live.iter().any(|&i| {
            let (before, after) = (&rel.columns[i], &now.columns[i]);
            before.ty != after.ty
                || before.typmod != after.typmod
                || before.collation != after.collation
        });
        let own: Vec<&Transform> = transforms.iter().filter(|t| t.table == rel.oid).collect();
        let (node, types) = if !live.is_empty() && (changed || !own.is_empty()) {
            // The last expression for a column wins, as the last entry of `tab->newvals` does.
            let exprs: Vec<Option<&Node>> = live
                .iter()
                .map(|&i| {
                    let name = &rel.columns[i].name;
                    own.iter().rev().find(|t| t.column == *name).map(|t| &t.expr)
                })
                .collect();
            let types = live.iter().map(|&i| rel.columns[i].ty).collect();
            (rupg_analyze::rewrite_insert(&target, &rv, &exprs), types)
        } else {
            (rupg_analyze::copy_insert(&target, &rv, &[]), vec![0; live.len()])
        };
        let reader = self.reader_on(Arc::clone(work));
        let params = Params { types, variable: true };
        // A materialized view takes the `INSERT` of its own rows, as the fill of `REFRESH MATERIALIZED VIEW` does.
        if now.kind == RelKind::MatView {
            self.filling.set(rel.oid);
        }
        let plan =
            query::plan_internal(&node, &reader, &params).and_then(|plan| plan.fold(None, &reader));
        self.filling.set(0);
        let plan = plan?;
        let Some(info) = plan.write() else {
            return Err(Error::internal("the rewrite of a table with no plan of its rows"));
        };
        // `ATRewriteTable` checks only the new check constraints and the ones that `VALIDATE CONSTRAINT` validates, and the not-null constraints of all the columns.
        let checks: Vec<(usize, &str)> = info
            .checks
            .iter()
            .enumerate()
            .filter(|&(_, name)| {
                work.constraints_of(rel.oid).any(|c| {
                    c.kind == ConKind::Check
                        && c.name == *name
                        && c.validated
                        && old.constraint(c.oid).is_none_or(|o| !o.validated)
                })
            })
            .map(|(at, name)| (at, name.as_str()))
            .collect();
        let reshaped = reshaped_row_types(old, work);
        let reshape: Vec<bool> = live
            .iter()
            .map(|&i| !reshaped.is_empty() && holds_row_type(old, rel.columns[i].ty, &reshaped, 0))
            .collect();
        let mut new_rows = Vec::with_capacity(rows.len());
        for row in rows {
            // A row from before an `ADD COLUMN` has no value for the new columns, which are null, and the ID of the version is at the end of the row.
            let stored = row.len().saturating_sub(1);
            let row: Vec<Value> = live
                .iter()
                .zip(&reshape)
                .map(|(&i, &reshape)| {
                    let value = if i < stored { row[i].clone() } else { Value::Null };
                    if reshape {
                        reshape_value(old, work, rel.columns[i].ty, value, 0)
                    } else {
                        value
                    }
                })
                .collect();
            for mut values in plan.run_unchecked(&row, &reader)? {
                for (column, value) in now.columns.iter().zip(&values) {
                    // `ATRewriteTable` checks only the valid not-null constraints.
                    if column.not_null && !column.not_null_invalid && value.is_null() {
                        let error = Error::new(
                            SqlState::NOT_NULL_VIOLATION,
                            format!(
                                "column \"{}\" of relation \"{}\" contains null values",
                                column.name, now.name
                            ),
                        );
                        return Err(rupg_func::with_table_column(
                            error,
                            now.oid,
                            &column.name,
                            &reader,
                        ));
                    }
                }
                for &(at, name) in &checks {
                    if matches!(values.get(info.width + at), Some(Value::Bool(false))) {
                        let error = Error::new(
                            SqlState::CHECK_VIOLATION,
                            format!(
                                "check constraint \"{name}\" of relation \"{}\" is violated by some row",
                                now.name
                            ),
                        );
                        return Err(rupg_func::with_table_constraint(
                            error, now.oid, name, &reader,
                        ));
                    }
                }
                values.truncate(info.width);
                new_rows.push(values);
            }
        }
        drop(reader);
        let heap = self.store.heap();
        // `ATPostAlterTypeCleanup` makes a foreign key on a generated column with a new expression again, so the new values are checked.
        let regenerated = |constraint: u32| {
            work.constraint(constraint).is_some_and(|c| {
                c.keys.iter().any(|&k| {
                    now.column(k).is_some_and(|col| col.generated != 0)
                        && old.default_of(rel.oid, k).map(|d| &d.expr)
                            != work.default_of(rel.oid, k).map(|d| &d.expr)
                })
            })
        };
        let foreign: Vec<_> = info
            .foreign
            .iter()
            .filter(|f| {
                // A foreign key that becomes valid checks the rows, and a `NOT VALID` one does not.
                let validated = work.constraint(f.constraint).is_some_and(|c| c.validated)
                    && old.constraint(f.constraint).is_none_or(|o| !o.validated);
                f.referencing && (validated || regenerated(f.constraint))
            })
            .map(|f| rupg_analyze::Foreign { deferrable: false, deferred: false, ..f.clone() })
            .collect();
        let checked = if foreign.is_empty() { Vec::new() } else { new_rows.clone() };
        // The rows of a new index are checked at once, also when the index is deferrable.
        let unique: Vec<_> = info
            .unique
            .iter()
            .map(|u| rupg_analyze::Unique { deferrable: None, ..u.clone() })
            .collect();
        if in_place {
            self.check_keys_in_place(rel.oid, view, &info.unique, work)?;
            self.check_exclusion_build(old, rel.oid, view, work)?;
            if !checked.is_empty() {
                self.fire(vec![Batch::new(info, &foreign, view.command, checked, Vec::new())])?;
            }
            return Ok(());
        }
        // `ATRewriteTable` writes the rows with a `BulkInsertState`.
        heap.start_bulk(rel.oid, view);
        if let Some(new) = work.relation(rel.oid) {
            self.relation_layout(new);
        }
        let mut kept = Vec::new();
        loop {
            match heap.insert(
                rel.oid,
                view,
                new_rows.clone(),
                (&unique, &|d: &Derived, r: &[Value]| {
                    rupg_exec::index_key(d, r, &self.reader_without_view(Arc::clone(work)))
                }),
                &kept,
            )? {
                Changed::Done(_) => break,
                Changed::Duplicate { index, key } => {
                    return Err(self.duplicated(rel.oid, info.unique.get(index), &key, work));
                }
                conflict => kept = self.key_conflict(info, view, conflict)?,
            }
        }
        self.check_exclusion_build(old, rel.oid, view, work)?;
        if !checked.is_empty() {
            self.fire(vec![Batch::new(info, &foreign, view.command, checked, Vec::new())])?;
        }
        Ok(())
    }

    /// `_bt_load` of the new unique indexes of the table `table` on the rows in place: the first duplicate key is an error, and a transaction in progress that holds a version with the key is a wait.
    fn check_keys_in_place(
        &self,
        table: u32,
        view: &View,
        unique: &[rupg_analyze::Unique],
        work: &Arc<Catalog>,
    ) -> Result<(), Error> {
        if unique.is_empty() {
            return Ok(());
        }
        let checked: Vec<_> =
            unique.iter().map(|u| rupg_analyze::Unique { deferrable: None, ..u.clone() }).collect();
        let reader = self.reader_without_view(Arc::clone(work));
        let derive = |d: &Derived, r: &[Value]| rupg_exec::index_key(d, r, &reader);
        loop {
            match self.store.heap().check_build(table, view, &checked, &derive)? {
                Some(Changed::WaitKey { holder, .. }) => {
                    self.reader().wait_xact(view.xid, holder, "")?;
                }
                Some(Changed::Duplicate { index, key }) => {
                    return Err(self.duplicated(table, unique.get(index), &key, work));
                }
                _ => return Ok(()),
            }
        }
    }

    /// The error of `index_build` for the key `key` that two rows of the table `table` have in the new unique index `unique`.
    fn duplicated(
        &self,
        table: u32,
        unique: Option<&rupg_analyze::Unique>,
        key: &[Value],
        work: &Arc<Catalog>,
    ) -> Error {
        let Some(unique) = unique else {
            return Error::internal("a duplicate key of an index that is not in the list");
        };
        let shown = self
            .index_key_text(&unique.types, key)
            .and_then(|values| Ok((self.key_shown(unique, Arc::clone(work))?, values)));
        let (columns, values) = match shown {
            Ok(shown) => shown,
            Err(error) => return error,
        };
        let error = Error::new(
            SqlState::UNIQUE_VIOLATION,
            format!("could not create unique index \"{}\"", unique.name),
        )
        .with_detail(format!("Key ({columns})=({values}) is duplicated."));
        rupg_func::with_table_constraint(error, table, &unique.name, &self.reader())
    }
}

/// True if the rows of the table `rel` keep their values after the change of the catalog from `old` to `new`, so that the statement only checks them: no column gets a new type, a value or a new expression of a generated column, and no value of a row type gets a new shape.
fn keeps_rows(
    old: &Catalog,
    new: &Catalog,
    rel: &Relation,
    transforms: &[Transform],
    fills: &[Fill],
) -> bool {
    let Some(now) = new.relation(rel.oid) else {
        return false;
    };
    if now.columns.len() != rel.columns.len()
        || transforms.iter().any(|t| t.table == rel.oid)
        || fills.iter().any(|f| f.table == rel.oid)
    {
        return false;
    }
    let reshaped = reshaped_row_types(old, new);
    rel.columns.iter().zip(&now.columns).enumerate().all(|(i, (before, after))| {
        let attnum = i16::try_from(i + 1).unwrap_or(i16::MAX);
        before.dropped == after.dropped
            && (before.dropped
                || before.ty == after.ty
                    && before.typmod == after.typmod
                    && before.collation == after.collation
                    && before.generated == after.generated
                    && (after.generated == 0
                        || old.default_of(rel.oid, attnum).map(|d| &d.expr)
                            == new.default_of(rel.oid, attnum).map(|d| &d.expr))
                    && (reshaped.is_empty() || !holds_row_type(old, before.ty, &reshaped, 0)))
    })
}

/// True if the change of the catalog from `old` to `new` changes the rows of the table `rel`: a change of the type of a column, a new not-null constraint, a new column with a default, a not-null constraint or a domain with constraints, a new constraint that checks the rows of the table or one that becomes valid, or a new unique index or one that becomes ready. A dropped table and a new column with no default keep the rows.
fn changes_rows(old: &Catalog, new: &Catalog, rel: &Relation) -> bool {
    let Some(now) = new.relation(rel.oid) else {
        return false;
    };
    if now.columns.len() < rel.columns.len() {
        return true;
    }
    // A `NOT VALID` not-null constraint does not check the rows, and one that becomes valid does.
    let proven = |c: &rupg_catalog::Column| c.not_null && !c.not_null_invalid;
    // A dropped column keeps its value in the rows, as no query can read it.
    let same = rel.columns.iter().zip(&now.columns).all(|(before, after)| {
        after.dropped
            || before.ty == after.ty
                && before.typmod == after.typmod
                && before.collation == after.collation
                && (proven(before) || !proven(after))
    });
    if !same {
        return true;
    }
    let added = (rel.columns.len()..now.columns.len()).any(|i| {
        let attnum = i16::try_from(i + 1).unwrap_or(i16::MAX);
        let column = &now.columns[i];
        column.not_null
            || column.identity != 0
            || new.default_of(rel.oid, attnum).is_some()
            || domain_has_constraints(new, column.ty)
    });
    if added {
        return true;
    }
    // A clone of a foreign key for a partition of the referenced table has no check on the rows of its table.
    let constraint = new.constraints_of(rel.oid).any(|c| {
        !matches!(c.kind, ConKind::NotNull | ConKind::Trigger)
            && c.validated
            && old.constraint(c.oid).is_none_or(|o| !o.validated)
            && (c.kind != ConKind::Foreign
                || new.triggers_of(rel.oid).iter().any(|t| t.constraint == c.oid))
    });
    // An index that `REINDEX` makes ready again checks the rows as a new index does.
    let index = new.relations().any(|index| {
        let was_ready = old.relation(index.oid).and_then(|o| o.index.as_ref()).map(|i| i.ready);
        was_ready != Some(true)
            && index
                .index
                .as_ref()
                .is_some_and(|info| info.table == rel.oid && info.unique && info.ready)
    });
    constraint || index
}

/// The row types whose relations get or lose a column from the catalog `old` to the catalog `new`.
fn reshaped_row_types(old: &Catalog, new: &Catalog) -> Vec<u32> {
    old.relations()
        .filter(|rel| rel.row_type != 0)
        .filter(|rel| {
            new.relation(rel.oid).is_some_and(|now| {
                now.columns.len() != rel.columns.len()
                    || rel.columns.iter().zip(&now.columns).any(|(b, a)| b.dropped != a.dropped)
            })
        })
        .map(|rel| rel.row_type)
        .collect()
}

/// True when the type `ty` is one of the row types `targets`, or a domain, an array or a composite type that holds one.
fn holds_row_type(catalog: &Catalog, ty: u32, targets: &[u32], depth: usize) -> bool {
    if targets.contains(&ty) {
        return true;
    }
    let Some(found) = catalog.type_by_oid(ty).filter(|_| depth < 32) else { return false };
    if let Some(domain) = &found.domain {
        return holds_row_type(catalog, domain.base, targets, depth + 1);
    }
    if found.element != 0 {
        return holds_row_type(catalog, found.element, targets, depth + 1);
    }
    catalog.relation(found.relation).is_some_and(|rel| {
        rel.columns.iter().any(|c| !c.dropped && holds_row_type(catalog, c.ty, targets, depth + 1))
    })
}

/// A stored value of the type `ty` with the fields of the catalog `new`: a record has a field for each column of its relation that is not dropped, with a null for a new column, also in the arrays, the domains and the other records that hold it. A value that the catalog `old` gave fewer fields gets nulls too.
fn reshape_value(old: &Catalog, new: &Catalog, ty: u32, value: Value, depth: usize) -> Value {
    let Some(found) = old.type_by_oid(ty).filter(|_| depth < 32) else { return value };
    if let Some(domain) = &found.domain {
        return reshape_value(old, new, domain.base, value, depth + 1);
    }
    match value {
        Value::Array(mut array) if found.element != 0 => {
            for element in array.values.iter_mut().flatten() {
                let taken = std::mem::replace(element, Value::Null);
                *element = reshape_value(old, new, found.element, taken, depth + 1);
            }
            Value::Array(array)
        }
        Value::Record(record) => {
            let (Some(before), Some(after)) =
                (old.relation(found.relation), new.relation(found.relation))
            else {
                return Value::Record(record);
            };
            let mut fields = record.values.into_iter();
            let mut by_column: Vec<Option<Value>> = before
                .columns
                .iter()
                .map(|c| if c.dropped { None } else { fields.next() })
                .collect();
            let mut types = Vec::with_capacity(after.columns.len());
            let mut values = Vec::with_capacity(after.columns.len());
            for (i, column) in after.columns.iter().enumerate().filter(|(_, c)| !c.dropped) {
                let value = match by_column.get_mut(i).and_then(Option::take) {
                    Some(value) => reshape_value(old, new, before.columns[i].ty, value, depth + 1),
                    None => Value::Null,
                };
                types.push(column.ty);
                values.push(value);
            }
            Value::Record(Box::new(Record::new(types, values)))
        }
        value => value,
    }
}

/// `DomainHasConstraints`: true for a domain with a not-null constraint or a check constraint, also from the domain that it is based on. A new column of such a domain with no default gets a null of the domain in each row, which the constraints check.
fn domain_has_constraints(catalog: &Catalog, ty: u32) -> bool {
    let mut ty = ty;
    while let Some(domain) = catalog.type_by_oid(ty).and_then(|t| t.domain.as_ref()) {
        if domain.not_null || catalog.constraints().any(|c| c.domain == ty) {
            return true;
        }
        ty = domain.base;
    }
    false
}
