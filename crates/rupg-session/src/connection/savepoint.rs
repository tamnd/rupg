//! `SAVEPOINT`, `RELEASE SAVEPOINT` and `ROLLBACK TO SAVEPOINT`, as `DefineSavepoint`, `ReleaseSavepoint` and `RollbackToSavepoint` of `xact.c` run them. A subtransaction has no transaction ID of its own. Each savepoint keeps the state of the transaction at its start, and a rollback of the subtransaction brings that state back: the rows that the later commands wrote, the catalog, the settings, the queue of the deferred triggers, the modes of `SET CONSTRAINTS`, the advisory locks of the transaction level and the portals of the subtransaction.

use std::sync::Arc;

use rupg_catalog::Catalog;
use rupg_common::Error;
use rupg_pgcatalog::roles::Roles;
use rupg_sql::nodes::TransactionStmt;
use rupg_wire::CommandTag;

use super::deferred::Modes;
use super::foreign::Batch;
use super::{Connection, Done};
use crate::advisory::Tag;
use crate::guc::Savepoint;
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
    modes: Modes,
    /// The counts of the advisory locks of the transaction level.
    locks: Vec<(Tag, bool, u32)>,
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
                    self.settings.get_mut().release(&mark.settings);
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
    fn mark(&self) -> Mark {
        Mark {
            command: self.command.get(),
            catalog: self
                .lease
                .as_ref()
                .map(|lease| (Arc::clone(&lease.catalog), Arc::clone(&lease.roles))),
            settings: self.settings.borrow().savepoint(),
            deferred: self.deferred.borrow().clone(),
            modes: self.modes.borrow().clone(),
            locks: self.store.locks().xact_holds(self.pid),
        }
    }

    /// `AbortSubTransaction`: the transaction goes back to its state at the innermost savepoint. The OIDs that the subtransaction used stay used. When the transaction did not hold the lock of the catalog at the savepoint, it releases the lock.
    pub(super) fn roll_back_subtransaction(&mut self) {
        let Some(mark) = self.marks.last() else { return };
        let xid = self.xid.get();
        if xid != 0 {
            self.store.heap().rollback_to(xid, mark.command);
        }
        match &mark.catalog {
            Some((catalog, roles)) => {
                if let Some(lease) = self.lease.as_mut() {
                    lease.roles = Arc::clone(roles);
                    let next = lease.catalog.next_oid();
                    let mut catalog = Arc::clone(catalog);
                    if catalog.next_oid() < next {
                        Arc::make_mut(&mut catalog).advance_oid(next);
                    }
                    lease.catalog = catalog;
                }
            }
            None => self.lease = None,
        }
        self.settings.get_mut().rollback_to(&mark.settings);
        self.deferred.get_mut().clone_from(&mark.deferred);
        self.modes.get_mut().clone_from(&mark.modes);
        self.store.locks().release_since(self.pid, &mark.locks);
    }
}
