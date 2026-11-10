//! The prepared transactions of two phase commit, as `twophase.c` keeps them in `TwoPhaseState`. A prepared transaction keeps its transaction ID in progress, its row locks, its heavyweight locks and its record of serializable snapshot isolation until `COMMIT PREPARED` or `ROLLBACK PREPARED` ends it, from any session. The list is in memory, so a restart of the server loses the prepared transactions.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use rupg_common::{Error, Result, SqlState};

use crate::connection::Pending;
use crate::store::Lease;

/// `GIDSIZE`: the size of the buffer of a global identifier with its terminating zero.
const GID_SIZE: usize = 200;

/// One prepared transaction, `GlobalTransactionData`.
#[derive(Debug)]
pub(crate) struct Prepared {
    /// The global identifier that `PREPARE TRANSACTION` gave.
    pub(crate) gid: String,
    pub(crate) xid: u32,
    /// The OID of the database of the transaction.
    pub(crate) database: u32,
    /// The OID of the role that prepared the transaction.
    pub(crate) owner: u32,
    /// The time of the prepare, in microseconds since 2000-01-01 UTC.
    pub(crate) prepared: i64,
    /// The record of the transaction in serializable snapshot isolation, or 0.
    pub(crate) sxact: u64,
    /// The writes of the transaction for the cumulative statistics, which count at its end.
    pub(crate) writes: BTreeMap<u32, Pending>,
    /// The number of the session that prepared the transaction, which the virtual transaction ID of its locks keeps.
    pub(crate) number: i32,
    /// `locking_backend`: the session that prepares the transaction or that ends it, which no other session can end meanwhile.
    pub(crate) busy: Option<i32>,
    /// The lock of the catalog of the database and the changes of the catalog, of a transaction that changed the catalog. `COMMIT PREPARED` publishes the changes, and `ROLLBACK PREPARED` discards them.
    pub(crate) lease: Option<Lease>,
}

impl Prepared {
    /// The owner of the heavyweight locks of the transaction, as the dummy `PGPROC` of a prepared transaction holds them: a number that no session has.
    pub(crate) fn lock_owner(xid: u32) -> i32 {
        -xid.cast_signed()
    }
}

/// The prepared transactions of the server.
#[derive(Debug, Default)]
pub(crate) struct PreparedXacts {
    list: Mutex<Vec<Prepared>>,
}

impl PreparedXacts {
    fn list(&self) -> MutexGuard<'_, Vec<Prepared>> {
        self.list.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `MarkAsPreparing`: the transaction takes `gid` and stays busy for the session `pid` until [`PreparedXacts::prepared`].
    ///
    /// # Errors
    ///
    /// `22023` for a global identifier that is too long, `53200` when `max` transactions are prepared, and `42710` for an identifier in use.
    pub(crate) fn reserve(&self, entry: Prepared, max: usize) -> Result<()> {
        if entry.gid.len() >= GID_SIZE {
            return Err(Error::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!("transaction identifier \"{}\" is too long", entry.gid),
            ));
        }
        let mut list = self.list();
        if list.iter().any(|other| other.gid == entry.gid) {
            return Err(Error::new(
                SqlState::DUPLICATE_OBJECT,
                format!("transaction identifier \"{}\" is already in use", entry.gid),
            ));
        }
        if list.len() >= max {
            return Err(Error::new(
                SqlState::OUT_OF_MEMORY,
                "maximum number of prepared transactions reached",
            )
            .with_hint(format!("Increase \"max_prepared_transactions\" (currently {max}).")));
        }
        list.push(entry);
        Ok(())
    }

    /// `MarkAsPrepared`: the transaction of the session `pid` is prepared, with the writes of the transaction and the lease of the catalog of a transaction that changed the catalog.
    pub(crate) fn prepared(&self, pid: i32, writes: BTreeMap<u32, Pending>, lease: Option<Lease>) {
        if let Some(entry) = self.list().iter_mut().find(|entry| entry.busy == Some(pid)) {
            entry.busy = None;
            entry.writes = writes;
            entry.lease = lease;
        }
    }

    /// `AtAbort_Twophase`: the reservation of the session `pid` goes when its transaction aborts before the end of the prepare.
    pub(crate) fn forget(&self, pid: i32) {
        self.list().retain(|entry| entry.busy != Some(pid));
    }

    /// `LockGXact`: takes the prepared transaction `gid` out of the list for its end by the session `pid`, as the role `role` with superuser rights when `superuser`.
    ///
    /// # Errors
    ///
    /// `42704` for an identifier that no transaction has, `55000` for a transaction that another session ends or prepares, `42501` for a role that is not the owner, and `0A000` for a transaction of another database.
    pub(crate) fn take(
        &self,
        gid: &str,
        role: u32,
        superuser: bool,
        database: u32,
    ) -> Result<Prepared> {
        let mut list = self.list();
        let Some(at) = list.iter().position(|entry| entry.gid == gid) else {
            return Err(Error::new(
                SqlState::UNDEFINED_OBJECT,
                format!("prepared transaction with identifier \"{gid}\" does not exist"),
            ));
        };
        let entry = &list[at];
        if entry.busy.is_some() {
            return Err(Error::new(
                SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                format!("prepared transaction with identifier \"{gid}\" is busy"),
            ));
        }
        if role != entry.owner && !superuser {
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                "permission denied to finish prepared transaction",
            )
            .with_hint("Must be superuser or the user that prepared the transaction."));
        }
        if database != entry.database {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "prepared transaction belongs to another database",
            )
            .with_hint(
                "Connect to the database where the transaction was prepared to finish it.",
            ));
        }
        Ok(list.remove(at))
    }

    /// The transaction ID and the number of the session that prepared it, of each prepared transaction, for `pg_lock_status`.
    pub(crate) fn lock_owners(&self) -> Vec<(u32, i32)> {
        self.list().iter().map(|entry| (entry.xid, entry.number)).collect()
    }

    /// The rows of `pg_prepared_xact`: the transaction ID, the global identifier, the time of the prepare, the owner and the database of each prepared transaction.
    pub(crate) fn rows(&self) -> Vec<(u32, String, i64, u32, u32)> {
        self.list()
            .iter()
            .filter(|entry| entry.busy.is_none())
            .map(|entry| {
                (entry.xid, entry.gid.clone(), entry.prepared, entry.owner, entry.database)
            })
            .collect()
    }
}
