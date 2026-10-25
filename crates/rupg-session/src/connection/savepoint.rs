//! `SAVEPOINT`, `RELEASE SAVEPOINT` and `ROLLBACK TO SAVEPOINT`, as `DefineSavepoint`, `ReleaseSavepoint` and `RollbackToSavepoint` of `xact.c` run them. A subtransaction has no transaction ID of its own. Each savepoint keeps the state of the transaction at its start, and a rollback of the subtransaction brings that state back: the rows that the later commands wrote, the catalog, the settings, the queue of the deferred triggers, the modes of `SET CONSTRAINTS`, the advisory locks of the transaction level, the counts of the rows that the transaction wrote, its `LISTEN`, `UNLISTEN` and `NOTIFY`, and the portals of the subtransaction.

use std::collections::BTreeMap;
use std::sync::Arc;

use rupg_catalog::Catalog;
use rupg_common::Error;
use rupg_pgcatalog::roles::Roles;
use rupg_sql::nodes::TransactionStmt;
use rupg_wire::CommandTag;

use super::deferred::Modes;
use super::foreign::Batch;
use super::trigger::Event;
use super::{Connection, Done, Pending};
use crate::advisory::{Mode, Tag};
use crate::guc::Savepoint;
use crate::heap::Recheck;
use crate::utility;

/// The state of the transaction at a savepoint.
#[derive(Debug)]
pub(super) struct Mark {
    /// `currentCommandId` at the savepoint. The rows that the transaction wrote from this command on belong to the subtransaction.
    command: u32,
    /// The catalog and the roles of the transaction, or `None` when the transaction did not hold the lock of the catalog.
    catalog: Option<(Arc<Catalog>, Arc<Roles>)>,
    settings: Savepoint,
    deferred: Vec<Batch>,
    deferred_events: Vec<Event>,
    deferred_unique: Vec<Recheck>,
    modes: Modes,
    /// The counts of the locks of the transaction level.
    locks: Vec<(Tag, Mode, u32)>,
    /// The rows that the transaction wrote in each relation, for the statistics.
    writes: BTreeMap<u32, Pending>,
    /// The `LISTEN`, `UNLISTEN` and `NOTIFY` of the transaction.
    notify: crate::notify::Pending,
}

impl Connection {
    /// Runs `SAVEPOINT`, `RELEASE SAVEPOINT` or `ROLLBACK TO SAVEPOINT`. `RELEASE SAVEPOINT` keeps the changes of the subtransactions that it ends, and their portals go to the level that stays. `ROLLBACK TO SAVEPOINT` removes the changes and the portals of the subtransactions after the savepoint, and keeps the savepoint.
    ///
    /// # Errors
    ///
    /// The errors of [`utility::savepoint`].
    pub(super) fn savepoint(&mut self, stmt: &TransactionStmt) -> Result<Done, Error> {
        let (tag, depth) = utility::savepoint(stmt, &mut self.transaction)?;
        match tag {
            CommandTag::Savepoint => {
                let mark = self.mark();
                self.marks.push(mark);
            }
            CommandTag::Release => {
                if let Some(mark) = self.marks.get(depth) {
                    self.release(mark);
                }
                self.marks.truncate(depth);
                self.portals.get_mut().retain(|_, portal| {
                    portal.level = portal.level.min(depth);
                    true
                });
            }
            _ => {
                self.marks.truncate(depth);
                self.roll_back_subtransaction();
                self.portals.get_mut().retain(|_, portal| portal.level < depth);
            }
        }
        self.settings.get_mut().set_subtransaction(depth > 0);
        Ok(Done::Tag(tag))
    }

    /// The state of the transaction for a new savepoint.
    pub(super) fn mark(&self) -> Mark {
        Mark {
            command: self.command.get(),
            catalog: self
                .lease
                .borrow()
                .as_ref()
                .map(|lease| (Arc::clone(&lease.catalog), Arc::clone(&lease.roles))),
            settings: self.settings.borrow().savepoint(),
            deferred: self.deferred.borrow().clone(),
            deferred_events: self.deferred_events.borrow().clone(),
            deferred_unique: self.deferred_unique.borrow().clone(),
            modes: self.modes.borrow().clone(),
            locks: self.store.locks().xact_holds(self.pid),
            writes: self.activity.writes(),
            notify: self.notify.borrow().clone(),
        }
    }

    /// `CommitSubTransaction`: the settings of the subtransaction that ends at the mark `mark` go to the level that stays.
    pub(super) fn release(&self, mark: &Mark) {
        self.settings.borrow_mut().release(&mark.settings);
    }

    /// `AbortSubTransaction`: the transaction goes back to its state at the innermost savepoint. The OIDs that the subtransaction used stay used. When the transaction did not hold the lock of the catalog at the savepoint, it releases the lock.
    pub(super) fn roll_back_subtransaction(&mut self) {
        if let Some(mark) = self.marks.last() {
            self.restore(mark);
        }
    }

    /// The part of [`Connection::roll_back_subtransaction`] that brings back the state of the transaction at the mark `mark`. A block with `EXCEPTION` of PL/pgSQL uses it too.
    pub(super) fn restore(&self, mark: &Mark) {
        let xid = self.xid.get();
        if xid != 0 {
            self.store.heap().rollback_to(xid, mark.command);
        }
        match &mark.catalog {
            Some((catalog, roles)) => {
                if let Some(lease) = self.lease.borrow_mut().as_mut() {
                    lease.roles = Arc::clone(roles);
                    let next = lease.catalog.next_oid();
                    let mut catalog = Arc::clone(catalog);
                    if catalog.next_oid() < next {
                        Arc::make_mut(&mut catalog).advance_oid(next);
                    }
                    lease.catalog = catalog;
                }
            }
            None => *self.lease.borrow_mut() = None,
        }
        self.settings.borrow_mut().rollback_to(&mark.settings);
        self.deferred.borrow_mut().clone_from(&mark.deferred);
        self.deferred_events.borrow_mut().clone_from(&mark.deferred_events);
        self.deferred_unique.borrow_mut().clone_from(&mark.deferred_unique);
        self.modes.borrow_mut().clone_from(&mark.modes);
        self.store.locks().release_since(self.pid, &mark.locks);
        self.activity.roll_back_writes(&mark.writes);
        self.notify.borrow_mut().clone_from(&mark.notify);
    }
}
