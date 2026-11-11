//! `PREPARE TRANSACTION`, `COMMIT PREPARED` and `ROLLBACK PREPARED` (spec/11 section 11.10). A prepared transaction that changed the catalog keeps the lock of the catalog of its database and its changes of the catalog until its end, so another transaction of the database that changes the catalog waits for `COMMIT PREPARED` or `ROLLBACK PREPARED`.

use rupg_common::{Error, SqlState};
use rupg_sql::nodes::{TransactionStmt, TransactionStmtKind};
use rupg_wire::CommandTag;

use super::{Connection, Done};
use crate::twophase::{HeldLock, Prepared};

impl Connection {
    /// The checks of `PrepareTransaction` after `PreCommit_CheckForSerializationFailure`, and `MarkAsPreparing`, which reserves the global identifier for the session until [`Connection::end_prepared`]. The transaction gets a transaction ID if it has none, as `GetCurrentTransactionId` gives it.
    ///
    /// # Errors
    ///
    /// The errors of [`crate::twophase::PreparedXacts::reserve`], and `0A000` for a transaction that used `LISTEN`, `UNLISTEN` or `NOTIFY` or that holds a lock at both levels.
    pub(super) fn prepare_checks(&self, gid: &str) -> Result<(), Error> {
        let max = self
            .settings
            .borrow()
            .get("max_prepared_transactions")
            .and_then(|max| max.parse().ok())
            .unwrap_or(0);
        if !self.notify.borrow().is_empty() {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "cannot PREPARE a transaction that has executed LISTEN, UNLISTEN, or NOTIFY",
            ));
        }
        if self.store.locks().holds_both_levels(self.pid) {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "cannot PREPARE while holding both session-level and transaction-level locks on the same object",
            ));
        }
        let xid = self.transaction_id(true).unwrap_or_default();
        let entry = Prepared {
            gid: gid.to_owned(),
            xid,
            database: self.database_oid,
            owner: self.current_role(),
            prepared: crate::query::now(&*self.clock),
            sxact: self.sxact.get(),
            writes: std::collections::BTreeMap::new(),
            number: self.activity.number.unwrap_or_default(),
            busy: Some(self.pid),
            lease: None,
            recovered: None,
        };
        self.store.prepared().reserve(entry, max)
    }

    /// `PostPrepare_Locks` and the rest of `PrepareTransaction` after the reservation: the transaction ID, its row locks, its heavyweight locks and its record of serializable snapshot isolation go to the prepared transaction, and the session ends its transaction with no commit and no abort.
    pub(super) fn end_prepared(&mut self) {
        let lease = self.lease.get_mut().take();
        let xid = self.xid.replace(0);
        self.stable_xid.set(0);
        self.end_notify(false);
        let owner = Prepared::lock_owner(xid);
        if let Some(lease) = &lease {
            lease.hand_over(self.pid, owner);
        }
        self.store.heap().hand_over(xid, owner);
        self.store.locks().hand_over(self.pid, self.activity.fast.as_deref(), owner);
        let sxact = self.sxact.replace(0);
        // The log gets the prepare, with the changes of the catalog, before another session can end the transaction.
        let locks = self.store.locks().xact_holds(owner, None);
        let locks = locks.into_iter().map(|(tag, mode, count)| HeldLock::of(tag, mode, count));
        let durable = lease.as_ref().map(|lease| lease.durable(owner));
        let ssi = (sxact != 0).then(|| self.store.heap().serializable_durable(sxact)).flatten();
        if let Some(durable) = self.store.prepared().durable(xid, locks.collect(), durable, ssi) {
            self.store.prepare(xid, &durable.bytes());
        }
        self.store.prepared().prepared(self.pid, self.activity.take_writes(), lease);
        self.store.commit(0, false, true);
        self.end_heap_state();
        self.marks.clear();
    }

    /// `FinishPreparedTransaction` for `COMMIT PREPARED` and `ROLLBACK PREPARED`, which cannot run in a transaction block.
    ///
    /// # Errors
    ///
    /// `25001` in a block, and the errors of [`crate::twophase::PreparedXacts::take`].
    pub(super) fn finish_prepared(&mut self, stmt: &TransactionStmt) -> Result<Done, Error> {
        let commit = stmt.kind == TransactionStmtKind::TRANS_STMT_COMMIT_PREPARED;
        let (statement, tag) = if commit {
            ("COMMIT PREPARED", CommandTag::CommitPrepared)
        } else {
            ("ROLLBACK PREPARED", CommandTag::RollbackPrepared)
        };
        if self.transaction.in_block() {
            return Err(Error::new(
                SqlState::ACTIVE_SQL_TRANSACTION,
                format!("{statement} cannot run inside a transaction block"),
            ));
        }
        let gid = stmt.gid.as_deref().unwrap_or_default();
        let mut entry = self.store.prepared().take(
            gid,
            self.current_role(),
            self.superuser(),
            self.database_oid,
        )?;
        let heap = self.store.heap();
        let recovered =
            entry.recovered.take().map(|lease| self.store.recovered_lease(entry.database, lease));
        let lease = entry.lease.take().or(recovered);
        let defined = lease.is_some();
        if commit {
            if let Some(lease) = lease {
                lease.commit();
            }
            self.store.finish_prepared(entry.xid, true, defined, self.synchronous_commit());
        } else {
            drop(lease);
            heap.abort(entry.xid);
            self.store.finish_prepared(entry.xid, false, false, true);
        }
        if entry.sxact != 0 {
            heap.serializable_end(entry.sxact, commit);
        }
        self.store.locks().release_all(Prepared::lock_owner(entry.xid), false);
        if !entry.writes.is_empty() {
            self.store.report_writes(entry.database, &entry.writes, commit);
        }
        if commit && defined {
            let catalog = self.store.committed(entry.database);
            heap.drop_tables(entry.database, |relation| catalog.relation(relation).is_some());
            self.store.drop_counters(entry.database, &catalog);
        }
        Ok(Done::Tag(tag))
    }

    /// The prepared transaction statements that the session runs itself: `COMMIT PREPARED` and `ROLLBACK PREPARED`.
    pub(super) fn is_finish_prepared(stmt: &TransactionStmt) -> bool {
        matches!(
            stmt.kind,
            TransactionStmtKind::TRANS_STMT_COMMIT_PREPARED
                | TransactionStmtKind::TRANS_STMT_ROLLBACK_PREPARED
        )
    }
}
