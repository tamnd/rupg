//! The prepared transactions of two phase commit, as `twophase.c` keeps them in `TwoPhaseState`. A prepared transaction keeps its transaction ID in progress, its row locks, its heavyweight locks and its record of serializable snapshot isolation until `COMMIT PREPARED` or `ROLLBACK PREPARED` ends it, from any session. The log and the image keep each prepared transaction with its changes and its heavyweight locks, so a restart after a crash finds it prepared again, as `RecoverPreparedTransactions` does. A transaction that changed the catalog comes back with the lock of the catalog of its database and its changes of the catalog. A restart does not keep a transaction that truncated a table or loaded rows, or its record of serializable snapshot isolation.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use rupg_catalog::Catalog;
use rupg_common::persist::{Persist, Reader, corrupt};
use rupg_common::{Error, Result, SqlState};
use rupg_func::AdvisoryKey;

use crate::advisory::{Mode, Tag};
use crate::connection::Pending;
use crate::store::Lease;
use rupg_pgcatalog::roles::Roles;

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
    /// The changes of the catalog of a transaction that changed the catalog and that a restart prepared again. The lock of the catalog of its database is the lock of the prepared transaction, and `COMMIT PREPARED` or `ROLLBACK PREPARED` makes a lease of them.
    pub(crate) recovered: Option<DurableLease>,
}

impl Prepared {
    /// The owner of the heavyweight locks of the transaction, as the dummy `PGPROC` of a prepared transaction holds them: a number that no session has.
    pub(crate) fn lock_owner(xid: u32) -> i32 {
        -xid.cast_signed()
    }
}

/// What the log and the image keep of a prepared transaction, as the state file of `pg_twophase` keeps it: the global identifier, the database, the owner, the time of the prepare, the heavyweight locks, the number of the session that prepared it, and the changes of the catalog of a transaction that changed the catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Durable {
    pub(crate) gid: String,
    pub(crate) database: u32,
    pub(crate) owner: u32,
    pub(crate) prepared: i64,
    pub(crate) locks: Vec<HeldLock>,
    pub(crate) number: i32,
    pub(crate) lease: Option<DurableLease>,
}

/// The changes of the catalog of a prepared transaction, as the record of a lease keeps them: the catalog with the changes of the transaction, the roles of the last commit when the transaction took the lease and the roles with its changes, and the rows of `pg_database` that it locked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DurableLease {
    pub(crate) catalog: Arc<Catalog>,
    pub(crate) base: Arc<Roles>,
    pub(crate) roles: Arc<Roles>,
    pub(crate) rows: Vec<u32>,
}

/// A heavyweight lock of a prepared transaction, as the record `TWOPHASE_RM_LOCK_ID` keeps it: the tag, the mode and the number of the holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HeldLock {
    /// 0 for a relation, 1 for an advisory lock and 2 for an object.
    kind: u8,
    database: u32,
    /// The relation, the high half of the key, or the class.
    first: u32,
    /// The low half of the key, or the OID of the object.
    second: u32,
    /// The kind of the key of an advisory lock.
    field: u16,
    mode: Mode,
    count: u32,
}

rupg_common::persist_struct!(HeldLock { kind, database, first, second, field, mode, count });

impl HeldLock {
    /// The record of `count` holds of `tag` in `mode`.
    pub(crate) fn of(tag: Tag, mode: Mode, count: u32) -> HeldLock {
        let (kind, database, first, second, field) = match tag {
            Tag::Relation { database, relation } => (0, database, relation, 0, 0),
            Tag::Advisory { database, key } => (1, database, key.high, key.low, key.kind),
            Tag::Object { database, class, oid } => (2, database, class, oid, 0),
        };
        HeldLock { kind, database, first, second, field, mode, count }
    }

    /// The tag, the mode and the number of the holds of the record.
    ///
    /// # Errors
    ///
    /// `XX001` for a record with a bad kind.
    pub(crate) fn lock(&self) -> Result<(Tag, Mode, u32)> {
        let HeldLock { kind, database, first, second, field, mode, count } = *self;
        let tag = match kind {
            0 => Tag::Relation { database, relation: first },
            1 => Tag::Advisory {
                database,
                key: AdvisoryKey { high: first, low: second, kind: field },
            },
            2 => Tag::Object { database, class: first, oid: second },
            _ => return Err(corrupt("a lock of a prepared transaction has a bad kind")),
        };
        Ok((tag, mode, count))
    }
}

impl Durable {
    /// The bytes of the record. The number of the session and the lease come after the fields of a record of an earlier server, so a record of an earlier server still reads.
    pub(crate) fn bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.gid.put(&mut out);
        self.database.put(&mut out);
        self.owner.put(&mut out);
        self.prepared.put(&mut out);
        self.locks.put(&mut out);
        self.number.put(&mut out);
        self.lease.is_some().put(&mut out);
        if let Some(lease) = &self.lease {
            lease.catalog.put(&mut out);
            lease.base.put(&mut out);
            // Roles that the transaction did not change are in the record once.
            let changed = lease.roles != lease.base;
            changed.put(&mut out);
            if changed {
                lease.roles.put(&mut out);
            }
            lease.rows.put(&mut out);
        }
        out
    }

    /// The record of the bytes of [`Durable::bytes`].
    ///
    /// # Errors
    ///
    /// `XX001` for bytes that are not a record.
    pub(crate) fn of_bytes(bytes: &[u8]) -> Result<Durable> {
        let mut input = Reader::new(bytes);
        let mut durable = Durable {
            gid: Persist::take(&mut input)?,
            database: Persist::take(&mut input)?,
            owner: Persist::take(&mut input)?,
            prepared: Persist::take(&mut input)?,
            locks: Persist::take(&mut input)?,
            number: 0,
            lease: None,
        };
        if !input.is_empty() {
            durable.number = Persist::take(&mut input)?;
            if bool::take(&mut input)? {
                let catalog = Arc::new(Catalog::take(&mut input)?);
                let base = Arc::new(Roles::take(&mut input)?);
                let roles = if bool::take(&mut input)? {
                    Arc::new(Roles::take(&mut input)?)
                } else {
                    Arc::clone(&base)
                };
                let rows = Persist::take(&mut input)?;
                durable.lease = Some(DurableLease { catalog, base, roles, rows });
            }
        }
        if !input.is_empty() {
            return Err(corrupt("a prepared transaction has bytes after its end"));
        }
        Ok(durable)
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

    /// The record that the log keeps of the transaction `xid` that the session prepares, with the heavyweight locks `locks` and the changes of the catalog `lease`.
    pub(crate) fn durable(
        &self,
        xid: u32,
        locks: Vec<HeldLock>,
        lease: Option<DurableLease>,
    ) -> Option<Durable> {
        let list = self.list();
        let entry = list.iter().find(|entry| entry.xid == xid)?;
        Some(Durable {
            gid: entry.gid.clone(),
            database: entry.database,
            owner: entry.owner,
            prepared: entry.prepared,
            locks,
            number: entry.number,
            lease,
        })
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

    /// `RecoverPreparedTransactions`: the prepared transaction `xid` of the log or the image is prepared again, with its changes of the catalog, and with no writes to count and no record of serializable snapshot isolation.
    pub(crate) fn restore(&self, xid: u32, durable: Durable) {
        self.list().push(Prepared {
            gid: durable.gid,
            xid,
            database: durable.database,
            owner: durable.owner,
            prepared: durable.prepared,
            sxact: 0,
            writes: BTreeMap::new(),
            number: durable.number,
            busy: None,
            lease: None,
            recovered: durable.lease,
        });
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A record keeps the number of the session and the lease, and a record of a server before them still reads, with the session 0 and no lease.
    #[test]
    fn a_record_keeps_the_session_and_the_lease() {
        let base = Arc::new(Roles::default());
        let mut changed = Roles::default();
        changed.comments.push(rupg_pgcatalog::roles::SharedComment {
            class: 1262,
            oid: 5,
            text: Some("c".into()),
        });
        let lock = HeldLock::of(
            Tag::Relation { database: 5, relation: 16384 },
            crate::advisory::ROW_EXCLUSIVE,
            2,
        );
        let mut durable = Durable {
            gid: "g".into(),
            database: 5,
            owner: 10,
            prepared: 77,
            locks: vec![lock],
            number: 3,
            lease: Some(DurableLease {
                catalog: Arc::new(Catalog::new()),
                base: Arc::clone(&base),
                roles: Arc::new(changed),
                rows: vec![5],
            }),
        };
        assert_eq!(Durable::of_bytes(&durable.bytes()).unwrap(), durable);
        durable.lease.as_mut().unwrap().roles = Arc::clone(&base);
        assert_eq!(Durable::of_bytes(&durable.bytes()).unwrap(), durable);
        let mut old = Vec::new();
        (durable.gid.clone(), durable.database, durable.owner, durable.prepared).put(&mut old);
        durable.locks.put(&mut old);
        let read = Durable::of_bytes(&old).unwrap();
        assert_eq!((read.number, read.lease), (0, None));
        assert_eq!(read.locks, durable.locks);
    }
}
