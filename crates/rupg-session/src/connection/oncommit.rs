//! The actions of `ON COMMIT DELETE ROWS` and `ON COMMIT DROP`, as `tablecmds.c` keeps them in `on_commits`. The session keeps each table with an action until the table goes, and at each commit `PreCommit_on_commit_actions` empties or drops the tables.

use rupg_catalog::{Catalog, RelKind};
use rupg_common::{Error, SqlState};
use rupg_sql::nodes::{DropBehavior, DropStmt, Node, ObjectType, OnCommitAction};

use super::Connection;
use crate::heap::View;

/// `BOOTSTRAP_SUPERUSERID`, the role of the drop of `ON COMMIT DROP`, which checks no privileges.
const BOOTSTRAP_SUPERUSER: u32 = 10;

/// `OnCommitItem`: a table with an action at the commit.
#[derive(Clone, Copy, Debug)]
pub(super) struct OnCommit {
    /// The OID of the table.
    table: u32,
    /// `ON COMMIT DELETE ROWS` or `ON COMMIT DROP`.
    action: OnCommitAction,
    /// True when the current transaction made the table, which accessed the temporary schema.
    new: bool,
}

impl Connection {
    /// `register_on_commit_action` for the new tables of a statement. `lcons` puts each one first, so the actions go in the reverse order of the tables.
    pub(super) fn register_on_commit(&self, tables: &[(u32, OnCommitAction)]) {
        let mut on_commits = self.on_commits.borrow_mut();
        for &(table, action) in tables {
            on_commits.insert(0, OnCommit { table, action, new: true });
        }
    }

    /// `PreCommit_on_commit_actions`: the tables with `ON COMMIT DELETE ROWS` lose their rows, then the tables with `ON COMMIT DROP` go, with the objects that depend on them. A table that is gone, as a drop or a rollback removed it, leaves the list, as `remove_on_commit_action` and `AtEOXact_on_commit_actions` remove it.
    ///
    /// # Errors
    ///
    /// `0A000` when a table that loses its rows has a foreign key from a table without the same action, and the errors of the drop.
    pub(super) fn on_commit_actions(&self) -> Result<(), Error> {
        if self.on_commits.borrow().is_empty() {
            return Ok(());
        }
        let catalog = self.catalog();
        self.on_commits.borrow_mut().retain(|item| catalog.relation(item.table).is_some());
        let items = self.on_commits.replace(Vec::new());
        let result = self.on_commit_items(&items, &catalog);
        *self.on_commits.borrow_mut() =
            items.into_iter().map(|item| OnCommit { new: false, ..item }).collect();
        result
    }

    /// The truncation and the drop of [`Connection::on_commit_actions`].
    fn on_commit_items(&self, items: &[OnCommit], catalog: &Catalog) -> Result<(), Error> {
        let database = self.database_oid;
        let heap = self.store.heap();
        let deleting: Vec<u32> = items
            .iter()
            .filter(|item| item.action == OnCommitAction::ONCOMMIT_DELETE_ROWS)
            .map(|item| item.table)
            .collect();
        // `XACT_FLAGS_ACCESSEDTEMPNAMESPACE`: a transaction that did not use the temporary schema leaves the tables, which are still empty.
        let accessed = self.transaction.accessed_temp()
            || items.iter().any(|item| item.new)
            || deleting.iter().any(|&table| heap.has_rows(database, table));
        if accessed && !deleting.is_empty() {
            check_on_commit_keys(&deleting, catalog)?;
            // `heap_truncate_one_rel`: a table with no storage, as a partitioned table, has no rows to remove.
            let truncated: Vec<u32> = deleting
                .iter()
                .copied()
                .filter(|&table| {
                    catalog.relation(table).is_some_and(|r| r.kind != RelKind::Partitioned)
                        && heap.has_rows(database, table)
                })
                .collect();
            if !truncated.is_empty() {
                if self.xid.get() == 0 {
                    self.xid.set(heap.begin(self.pid));
                }
                let view = View { xid: self.xid.get(), ..self.view() };
                for &table in &truncated {
                    // The table is temporary, so no other transaction changes it.
                    let _ = heap.truncate(table, &view);
                }
                self.command.set(view.command + 1);
            }
        }
        let dropped: Vec<Option<Node>> = items
            .iter()
            .filter(|item| item.action == OnCommitAction::ONCOMMIT_DROP)
            .filter_map(|item| {
                let rel = catalog.relation(item.table)?;
                let schema = catalog.schema(rel.namespace)?;
                Some(Some(Node::List(vec![
                    Some(Node::String(schema.name.as_str().into())),
                    Some(Node::String(rel.name.as_str().into())),
                ])))
            })
            .collect();
        if dropped.is_empty() {
            return Ok(());
        }
        // `performMultipleDeletions` with `DROP_CASCADE` and `PERFORM_DELETION_QUIETLY`: the drop gives no notices. A table that the drop of another one removed is gone, as `missing_ok` allows.
        let drop = Node::DropStmt(Box::new(DropStmt {
            objects: dropped,
            removeType: ObjectType::OBJECT_TABLE,
            behavior: DropBehavior::DROP_CASCADE,
            missing_ok: true,
            concurrent: false,
        }));
        self.define_with(&drop, "", &mut Vec::new(), BOOTSTRAP_SUPERUSER, false)?;
        Ok(())
    }
}

/// `heap_truncate_check_FKs` with `tempTables`: each table with a foreign key to a table that loses its rows must lose its rows too. Only the tables with `relhastriggers` and the partitioned tables count, and the error names the first such pair.
fn check_on_commit_keys(tables: &[u32], catalog: &Catalog) -> Result<(), Error> {
    let counted: Vec<u32> = tables
        .iter()
        .copied()
        .filter(|&t| {
            catalog.relation(t).is_some_and(|r| r.has_triggers || r.kind == RelKind::Partitioned)
        })
        .collect();
    for &table in &counted {
        // `heap_truncate_find_FKs`: the tables with a foreign key to the table, in the order of the OIDs.
        let mut referencing: Vec<u32> = catalog
            .constraints()
            .filter(|c| c.foreign.as_ref().is_some_and(|f| f.table == table))
            .map(|c| c.relation)
            .collect();
        referencing.sort_unstable();
        let Some(other) = referencing.into_iter().find(|t| !counted.contains(t)) else {
            continue;
        };
        let name = |oid: u32| catalog.relation(oid).map(|r| r.name.clone()).unwrap_or_default();
        return Err(Error::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "unsupported ON COMMIT and foreign key combination",
        )
        .with_detail(format!(
            "Table \"{}\" references \"{}\", but they do not have the same ON COMMIT setting.",
            name(other),
            name(table)
        )));
    }
    Ok(())
}
