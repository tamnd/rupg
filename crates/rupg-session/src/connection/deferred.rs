//! The deferred triggers of the foreign keys and the deferred constraint triggers, as `trigger.c` keeps them. A trigger that is deferred does not fire at the end of its statement. Its rows go to the queue of the transaction, and they fire at the commit or when `SET CONSTRAINTS` makes the trigger immediate. A rollback removes the queue, and a rollback to a savepoint brings back the queue and the modes of the savepoint.

use std::collections::HashMap;

use rupg_analyze::Foreign;
use rupg_catalog::Trigger;
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
        self.state(foreign.constraint, foreign.deferrable, foreign.deferred)
    }

    /// `afterTriggerCheckState` for a constraint trigger: true when the trigger fires at the commit.
    pub(super) fn trigger_deferred(&self, trigger: &Trigger) -> bool {
        self.state(trigger.constraint, trigger.deferrable, trigger.deferred)
    }

    /// `afterTriggerCheckState`: the mode of the constraint with the OID `constraint`, which is `DEFERRABLE` when `deferrable` is true and `INITIALLY DEFERRED` when `deferred` is true.
    fn state(&self, constraint: u32, deferrable: bool, deferred: bool) -> bool {
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
            let fired = self.fire_deferred();
            notices.extend(self.trigger_notices());
            fired?;
        }
        Ok(Done::Tag(CommandTag::SetConstraints))
    }

    /// The rows of the queue of the transaction whose triggers are no longer deferred, as `afterTriggerMarkEvents` and `afterTriggerInvokeEvents` fire them, first the rows of the foreign keys and then the events of the constraint triggers. The other rows stay in the queue.
    fn fire_deferred(&self) -> Result<(), Error> {
        let pending = std::mem::take(&mut *self.deferred.borrow_mut());
        if !pending.is_empty() {
            self.fire(pending)?;
        }
        self.fire_deferred_events()
    }

    /// `AfterTriggerFireDeferred`: before a commit, all the deferred triggers fire. It does nothing when the end of the statement does not commit.
    ///
    /// # Errors
    ///
    /// The errors of the triggers.
    pub(super) fn pre_commit(&self) -> Result<(), Error> {
        if !self.transaction.commits()
            || (self.deferred.borrow().is_empty() && self.deferred_events.borrow().is_empty())
        {
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
        self.deferred_events.borrow_mut().clear();
        *self.modes.borrow_mut() = Modes::default();
    }
}
