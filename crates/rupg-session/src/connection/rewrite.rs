//! The rows of a table after a statement that changes the table, as `ATRewriteTables` makes them: a new column gets its default in each row, a column with a new type gets the value of its `USING` expression or its old value with the new type, and the rows must pass the new not-null constraints, the new check constraints, the new unique indexes and the new foreign keys.
//!
//! PostgreSQL writes the rows to a new file, or it only reads them when no row changes. rupg deletes each row and inserts its new version in the same command, so a rollback brings back the old rows.

use std::sync::Arc;

use rupg_analyze::{CopyTarget, Params, Transform};
use rupg_catalog::{Catalog, ConKind, FIRST_NORMAL_OID, RelKind, Relation};
use rupg_common::{Error, SqlState};
use rupg_pgcatalog::builtin::{self, Named};
use rupg_sql::nodes::{Node, RangeVar};
use rupg_types::{Record, Value};

use super::Connection;
use super::foreign::Batch;
use crate::heap::{Changed, View};
use crate::query;

impl Connection {
    /// The tables whose rows the statement `node` changes, from the catalog before the statement to `work`. A table with no rows needs no change. A table with an expression of `transforms` always gets its rows again.
    ///
    /// # Errors
    ///
    /// `0A000` for a change that the rows cannot get yet: `ALTER DOMAIN` on a table with rows, and a change of the type of a column by a statement that is not `ALTER TABLE`.
    pub(super) fn rows_to_rewrite(
        &self,
        node: &Node,
        work: &Catalog,
        transforms: &[Transform],
    ) -> Result<Vec<u32>, Error> {
        let heap = self.store.heap();
        let old = self.catalog();
        let tables: Vec<&Relation> = old
            .relations()
            .filter(|rel| {
                rel.kind == RelKind::Table
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
        let transformed = |rel: &Relation| transforms.iter().any(|t| t.table == rel.oid);
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
    pub(super) fn rewrite_rows(
        &self,
        tables: &[u32],
        work: &Arc<Catalog>,
        transforms: &[Transform],
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
            loop {
                let view = View { xid: self.xid.get(), ..self.view() };
                let Some(holder) = heap.truncate(table, &view) else { break };
                self.reader().wait_xact(view.xid, holder, "")?;
            }
            // The rows that the truncate deleted stay visible to the command that deleted them.
            let view = View { xid: self.xid.get(), ..self.view() };
            let rows = heap.rows(table, &view);
            self.rewrite_table(&old, rel, work, &view, rows, transforms)?;
        }
        self.command.set(command + 1);
        Ok(())
    }

    /// The new versions of the rows `rows` of the table `rel`, with the ID of the old version at the end of each row. The `INSERT` of the old columns gives the new columns their defaults, as the expressions of `ATRewriteTable` do. When a column has a new type or an expression of `transforms`, the `INSERT` reads the old values with their old types, and the expressions give the new values.
    fn rewrite_table(
        &self,
        old: &Catalog,
        rel: &Relation,
        work: &Arc<Catalog>,
        view: &View,
        rows: Vec<Vec<Value>>,
        transforms: &[Transform],
    ) -> Result<(), Error> {
        let Some(now) = work.relation(rel.oid) else { return Ok(()) };
        let width = rel.columns.len();
        // A dropped column has no value in the new rows. The `INSERT` gives it a null.
        let live: Vec<usize> = (0..width)
            .filter(|&i| !rel.columns[i].dropped && now.columns.get(i).is_some_and(|c| !c.dropped))
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
        let plan = query::plan(&node, &reader, &params)?.fold(None, &reader)?;
        let Some(info) = plan.write() else {
            return Err(Error::internal("the rewrite of a table with no plan of its rows"));
        };
        // `ATRewriteTable` checks only the new check constraints, and the not-null constraints of all the columns.
        let checks: Vec<(usize, &str)> = info
            .checks
            .iter()
            .enumerate()
            .filter(|&(_, name)| {
                work.constraints_of(rel.oid).any(|c| {
                    c.kind == ConKind::Check
                        && c.name == *name
                        && c.validated
                        && old.constraint(c.oid).is_none()
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
            let row: Vec<Value> = live
                .iter()
                .zip(&reshape)
                .map(|(&i, &reshape)| {
                    let value = row.get(i).cloned().unwrap_or(Value::Null);
                    if reshape {
                        reshape_value(old, work, rel.columns[i].ty, value, 0)
                    } else {
                        value
                    }
                })
                .collect();
            for mut values in plan.run_unchecked(&row, &reader)? {
                for (column, value) in now.columns.iter().zip(&values) {
                    if column.not_null && value.is_null() {
                        return Err(Error::new(
                            SqlState::NOT_NULL_VIOLATION,
                            format!(
                                "column \"{}\" of relation \"{}\" contains null values",
                                column.name, now.name
                            ),
                        ));
                    }
                }
                for &(at, name) in &checks {
                    if matches!(values.get(info.width + at), Some(Value::Bool(false))) {
                        return Err(Error::new(
                            SqlState::CHECK_VIOLATION,
                            format!(
                                "check constraint \"{name}\" of relation \"{}\" is violated by some row",
                                now.name
                            ),
                        ));
                    }
                }
                values.truncate(info.width);
                new_rows.push(values);
            }
        }
        drop(reader);
        let heap = self.store.heap();
        let foreign: Vec<_> = info
            .foreign
            .iter()
            .filter(|f| f.referencing && old.constraint(f.constraint).is_none())
            .map(|f| rupg_analyze::Foreign { deferrable: false, deferred: false, ..f.clone() })
            .collect();
        let checked = if foreign.is_empty() { Vec::new() } else { new_rows.clone() };
        let mut kept = Vec::new();
        loop {
            match heap.insert(rel.oid, view, new_rows.clone(), &info.unique, &kept)? {
                Changed::Done(_) => break,
                Changed::Duplicate { index, key } => {
                    let Some(unique) = info.unique.get(index) else {
                        return Err(Error::internal(
                            "a duplicate key of an index that is not in the list",
                        ));
                    };
                    let values = self.index_key_text(&unique.types, &key)?;
                    return Err(Error::new(
                        SqlState::UNIQUE_VIOLATION,
                        format!("could not create unique index \"{}\"", unique.name),
                    )
                    .with_detail(format!("Key ({})=({values}) is duplicated.", unique.shown)));
                }
                conflict => kept = self.key_conflict(info, view, conflict)?,
            }
        }
        if !checked.is_empty() {
            self.fire(vec![Batch::new(info, &foreign, view.command, checked, Vec::new())])?;
        }
        Ok(())
    }
}

/// True if the change of the catalog from `old` to `new` changes the rows of the table `rel`: a change of the type of a column, a new not-null constraint, a new column with a default, a not-null constraint or a domain with constraints, a new constraint, or a new unique index. A dropped table and a new column with no default keep the rows.
fn changes_rows(old: &Catalog, new: &Catalog, rel: &Relation) -> bool {
    let Some(now) = new.relation(rel.oid) else {
        return false;
    };
    if now.columns.len() < rel.columns.len() {
        return true;
    }
    // A dropped column keeps its value in the rows, as no query can read it.
    let same = rel.columns.iter().zip(&now.columns).all(|(before, after)| {
        after.dropped
            || before.ty == after.ty
                && before.typmod == after.typmod
                && before.collation == after.collation
                && (before.not_null || !after.not_null)
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
    let constraint = new
        .constraints_of(rel.oid)
        .any(|c| c.kind != ConKind::NotNull && c.validated && old.constraint(c.oid).is_none());
    let index = new.relations().any(|index| {
        old.relation(index.oid).is_none()
            && index.index.as_ref().is_some_and(|info| info.table == rel.oid && info.unique)
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
