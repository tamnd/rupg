//! The deferred triggers of the foreign keys, the deferred constraint triggers and the checks of the deferred unique constraints, as `trigger.c` keeps them. A trigger that is deferred does not fire at the end of its statement. Its rows go to the queue of the transaction, and they fire at the commit or when `SET CONSTRAINTS` makes the trigger immediate. A rollback removes the queue, and a rollback to a savepoint brings back the queue and the modes of the savepoint.

use std::collections::HashMap;

use rupg_analyze::{Derived, Foreign};
use rupg_catalog::Trigger;
use rupg_common::{Error, SqlState};
use rupg_sql::nodes::ConstraintsSetStmt;
use rupg_types::Value;
use rupg_wire::CommandTag;

use super::{Connection, Done};
use crate::heap::{Changed, Recheck, tid_text};
use crate::utility::Notice;

/// `SetConstraintStateData`: the modes that `SET CONSTRAINTS` gave in the transaction. True is `DEFERRED`.
#[derive(Clone, Debug, Default)]
pub(super) struct Modes {
    /// The mode of the last `SET CONSTRAINTS ALL`.
    all: Option<bool>,
    /// The mode of each constraint that a `SET CONSTRAINTS` named after the last `SET CONSTRAINTS ALL`, by the OID of the constraint.
    each: HashMap<u32, bool>,
}

impl Modes {
    /// `afterTriggerCheckState`: true when the trigger of the foreign key fires at the commit.
    pub(super) fn deferred(&self, foreign: &Foreign) -> bool {
        self.state(foreign.constraint, foreign.deferrable, foreign.deferred)
    }

    /// `afterTriggerCheckState` for a constraint trigger: true when the trigger fires at the commit.
    pub(super) fn trigger_deferred(&self, trigger: &Trigger) -> bool {
        self.state(trigger.constraint, trigger.deferrable, trigger.deferred)
    }

    /// `afterTriggerCheckState`: the mode of the constraint with the OID `constraint`, which is `DEFERRABLE` when `deferrable` is true and `INITIALLY DEFERRED` when `deferred` is true.
    pub(super) fn state(&self, constraint: u32, deferrable: bool, deferred: bool) -> bool {
        if !deferrable {
            return false;
        }
        if let Some(&deferred) = self.each.get(&constraint) {
            return deferred;
        }
        self.all.unwrap_or(deferred)
    }
}

impl Connection {
    /// `AfterTriggerSetState`: `SET CONSTRAINTS`. Outside of a block it gives the warning of `WarnNoTransactionBlock`. `IMMEDIATE` fires the deferred rows of the constraints at once.
    ///
    /// # Errors
    ///
    /// The errors of [`rupg_analyze::set_constraints`], and the errors of the triggers that fire.
    pub(super) fn set_constraints(
        &self,
        stmt: &ConstraintsSetStmt,
        notices: &mut Vec<Notice>,
    ) -> Result<Done, Error> {
        if !self.transaction.in_block() {
            notices.push(Notice::warning(Error::new(
                SqlState::NO_ACTIVE_SQL_TRANSACTION,
                "SET CONSTRAINTS can only be used in transaction blocks",
            )));
        }
        let oids = rupg_analyze::set_constraints(stmt, &self.reader())?;
        {
            let mut modes = self.modes.borrow_mut();
            match oids {
                None => {
                    modes.all = Some(stmt.deferred);
                    modes.each.clear();
                }
                Some(oids) => {
                    for oid in oids {
                        modes.each.insert(oid, stmt.deferred);
                    }
                }
            }
        }
        if !stmt.deferred {
            let fired = self.fire_deferred(false);
            notices.extend(self.trigger_notices());
            fired?;
        }
        Ok(Done::Tag(CommandTag::SetConstraints))
    }

    /// The rows of the queue of the transaction whose triggers are no longer deferred, as `afterTriggerMarkEvents` and `afterTriggerInvokeEvents` fire them, first the rows of the foreign keys, then the checks of the unique constraints and then the events of the constraint triggers. The other rows stay in the queue. With `all`, as `AfterTriggerFireDeferred` fires them, all the rows of the queue fire, and the rows that the statements of the triggers queue follow the modes of `SET CONSTRAINTS`.
    fn fire_deferred(&self, all: bool) -> Result<(), Error> {
        let pending = std::mem::take(&mut *self.deferred.borrow_mut());
        if !pending.is_empty() {
            let forced = if all { pending.len() } else { 0 };
            self.fire_forced(pending, forced)?;
        }
        let rechecks = std::mem::take(&mut *self.deferred_unique.borrow_mut());
        if !rechecks.is_empty() {
            let now = if all { rechecks } else { self.defer_unique(rechecks) };
            self.check_unique(&now)?;
        }
        self.fire_deferred_exclusions(all)?;
        self.fire_deferred_events(all)
    }

    /// The end of a statement for the deferrable unique constraints and the exclusion constraints: the new versions that the commands from `command` on wrote with the key of another version, as `unique_key_recheck` checks them when the constraint is immediate. The versions of a deferred constraint go to the queue of the transaction.
    ///
    /// # Errors
    ///
    /// The duplicate keys, and the errors of a wait.
    pub(super) fn recheck_unique(&self, command: u32) -> Result<(), Error> {
        self.recheck_unique_from(command, command)
    }

    /// [`Connection::recheck_unique`] when the exclusion constraints that are not deferrable checked the commands before `immediate`.
    ///
    /// # Errors
    ///
    /// The duplicate keys, and the errors of a wait.
    pub(super) fn recheck_unique_from(&self, command: u32, immediate: u32) -> Result<(), Error> {
        let xid = self.xid.get();
        if xid == 0 {
            return Ok(());
        }
        let rechecks = self.store.heap().take_rechecks(xid, command);
        if !rechecks.is_empty() {
            let now = self.defer_unique(rechecks);
            self.check_unique(&now)?;
        }
        self.check_exclusions(command, immediate, true)
    }

    /// The rechecks of a deferred constraint go to the queue of the transaction. The result is the other rechecks.
    fn defer_unique(&self, rechecks: Vec<Recheck>) -> Vec<Recheck> {
        let modes = self.modes.borrow();
        let mut queue = self.deferred_unique.borrow_mut();
        let mut now = Vec::with_capacity(rechecks.len());
        for recheck in rechecks {
            match recheck.unique.deferrable {
                Some(mode) if modes.state(mode.constraint, true, mode.deferred) => {
                    queue.push(recheck);
                }
                _ => now.push(recheck),
            }
        }
        now
    }

    /// `unique_key_recheck` of each version of `rechecks`: a live version with the same key is a duplicate, and the check waits for a transaction in progress that holds the key.
    fn check_unique(&self, rechecks: &[Recheck]) -> Result<(), Error> {
        let heap = self.store.heap();
        let xid = self.xid.get();
        let derive = |d: &Derived, r: &[Value]| self.index_key(d, r);
        for recheck in rechecks {
            loop {
                match heap.recheck(xid, recheck, &derive)? {
                    Some(Changed::WaitKey { holder, .. }) => {
                        let tid = tid_text(recheck.tid);
                        let name = &recheck.unique.name;
                        let context =
                            format!("while inserting index tuple {tid} in relation \"{name}\"");
                        self.reader().wait_xact(xid, holder, &context)?;
                    }
                    Some(Changed::Duplicate { key, .. }) => {
                        return Err(self.duplicate_key(recheck.table, &recheck.unique, &key));
                    }
                    _ => break,
                }
            }
        }
        Ok(())
    }

    /// `AfterTriggerFireDeferred`: before a commit, all the deferred triggers fire, then the actions of `ON COMMIT` run. It does nothing when the end of the statement does not commit.
    ///
    /// # Errors
    ///
    /// The errors of the triggers and of the actions of `ON COMMIT`.
    pub(super) fn pre_commit(&self) -> Result<(), Error> {
        if !self.transaction.commits() {
            return Ok(());
        }
        if !self.deferred.borrow().is_empty()
            || !self.deferred_events.borrow().is_empty()
            || !self.deferred_unique.borrow().is_empty()
            || !self.deferred_exclusions.borrow().is_empty()
        {
            // `AfterTriggerFireDeferred` repeats until the statements of the triggers queue no more rows.
            while !self.deferred.borrow().is_empty()
                || !self.deferred_events.borrow().is_empty()
                || !self.deferred_unique.borrow().is_empty()
                || !self.deferred_exclusions.borrow().is_empty()
            {
                self.fire_deferred(true)?;
            }
        }
        // `PreCommit_on_commit_actions` comes after `AfterTriggerFireDeferred` in `CommitTransaction`.
        self.on_commit_actions()
    }

    /// `AfterTriggerPendingOnRel`: true when the queue of the transaction has rows of the triggers of the table `table`, as the deferred checks of its foreign keys and of its unique constraints and the events of its constraint triggers.
    pub(crate) fn trigger_events_pending(&self, table: u32) -> bool {
        self.deferred.borrow().iter().any(|batch| batch.table() == table)
            || self.deferred_events.borrow().iter().any(|event| event.table() == table)
            || self.deferred_unique.borrow().iter().any(|recheck| recheck.table == table)
            || self.deferred_exclusions.borrow().iter().any(|&(t, _)| t == table)
    }

    /// The end of a transaction: the queue and the modes go.
    pub(super) fn end_deferred(&self) {
        self.deferred.borrow_mut().clear();
        self.deferred_events.borrow_mut().clear();
        self.deferred_unique.borrow_mut().clear();
        self.deferred_exclusions.borrow_mut().clear();
        *self.modes.borrow_mut() = Modes::default();
    }
}
