//! The deferred triggers of the foreign keys, as `trigger.c` keeps them. A trigger that is deferred does not fire at the end of its statement. Its rows go to the queue of the transaction, and they fire at the commit or when `SET CONSTRAINTS` makes the trigger immediate. A rollback removes the queue, and a rollback to a savepoint brings back the queue and the modes of the savepoint.

use std::collections::HashMap;

use rupg_analyze::Foreign;
use rupg_common::{Error, SqlState};
use rupg_sql::nodes::ConstraintsSetStmt;
use rupg_wire::CommandTag;

use super::{Connection, Done};
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
        if !foreign.deferrable {
            return false;
        }
        if let Some(&deferred) = self.each.get(&foreign.constraint) {
            return deferred;
        }
        self.all.unwrap_or(foreign.deferred)
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
            self.fire_deferred()?;
        }
        Ok(Done::Tag(CommandTag::SetConstraints))
    }

    /// The rows of the queue of the transaction whose triggers are no longer deferred, as `afterTriggerMarkEvents` and `afterTriggerInvokeEvents` fire them. The other rows stay in the queue.
    fn fire_deferred(&self) -> Result<(), Error> {
        let pending = std::mem::take(&mut *self.deferred.borrow_mut());
        if pending.is_empty() {
            return Ok(());
        }
        self.fire(pending)
    }

    /// `AfterTriggerFireDeferred`: before a commit, all the deferred triggers fire. It does nothing when the end of the statement does not commit.
    ///
    /// # Errors
    ///
    /// The errors of the triggers.
    pub(super) fn pre_commit(&self) -> Result<(), Error> {
        if !self.transaction.commits() || self.deferred.borrow().is_empty() {
            return Ok(());
        }
        {
            let mut modes = self.modes.borrow_mut();
            modes.all = Some(false);
            modes.each.clear();
        }
        self.fire_deferred()
    }

    /// The end of a transaction: the queue and the modes go.
    pub(super) fn end_deferred(&self) {
        self.deferred.borrow_mut().clear();
        *self.modes.borrow_mut() = Modes::default();
    }
}
