# 11. Transactions and the log

Written 7 October 2026.

This document specifies how rupg runs transactions and makes them durable. It covers the client surface, versions and undo, visibility, commit, the isolation levels, savepoints, locks, the exposure of `xid`, `xid8`, `xmin` and `xmax`, two-phase commit, sequences, the LSN, the log rings, checkpoints, recovery, replication streams and backup. The client rules come from `../2140/compat/postgres/10-transactions.md`, which read the PostgreSQL source and the isolation suite. The engine follows three papers: Alhomssi and Leis on scalable snapshot isolation (PVLDB 16, 2023), Nguyen, Alhomssi, Ziegler and Leis on per-worker logs (SIGMOD 2025), and Lee, Ziegler and Leis on out-of-place page writes (PVLDB 19, 2026). The crates are `rupg-log` (rank 3) and `rupg-txn` (rank 5).

## 11.1 What a client sees

A client must see no difference from PostgreSQL 19 in the transaction surface.

**Status bytes.** `ReadyForQuery` carries `I` when idle, `T` in a transaction block and `E` in a failed block. In the failed state, every statement except `ROLLBACK`, `ROLLBACK TO SAVEPOINT` and `COMMIT` fails with `25P02`. `COMMIT` in the failed state rolls back and returns the tag `ROLLBACK`.

**Warnings.** `BEGIN` inside a block gives the warning `25001`. `COMMIT` or `ROLLBACK` outside a block gives the warning `25P01`. Statements that cannot run in a block, for example `VACUUM` and `CREATE INDEX CONCURRENTLY`, fail with `25001` inside one.

**Modes.** `transaction_isolation`, `default_transaction_isolation` (default `read committed`), `transaction_read_only` and `transaction_deferrable` keep their meaning. `SET TRANSACTION ISOLATION LEVEL` after the first query fails with `25001`. A write in a `READ ONLY` transaction fails with `25006`.

**Transactional DDL.** DDL rolls back with its transaction, because the rupg catalog is stored in tables with the same version headers as user data. A concurrent `CREATE` of the same name waits for the first transaction and then fails with `42P07` or succeeds, as PostgreSQL waits on the catalog's unique index. A use of an enum value added by `ALTER TYPE ... ADD VALUE` in the same block fails with `55P04`.

**Implicit transactions.** In the extended protocol, an implicit transaction spans the messages up to `Sync`, and an error discards the messages up to `Sync`, as in PostgreSQL.

## 11.2 Versions and undo

rupg updates rows in place and keeps old values in undo records. A reader that needs an older version follows the undo chain from the row. This is the design of LeanStore, HyPer and rudb.

### 11.2.1 The version header

Each row in a hot page has the 16-byte version header of document 10. This is its encoding.

| Field | Bits | Meaning |
|---|---|---|
| `stamp` | 64 | the commit timestamp of the newest version or, with the top bit set, the slot of the uncommitted owner |
| `undo` | 48 | the position of the newest undo record, or 0 |
| `flags` | 16 | the flags of document 10, lock only, lock strength (2 bits), lock group |

**Two hidden minipages.** A hot leaf also has a minipage for `xmin`, 4 bytes for each row, with the low 32 bits of the id of the transaction that wrote the newest version. It has a minipage for `xmax` only when some row in the leaf has a deleter or locker, and a column directory entry of 0 means that every `xmax` in the leaf is 0. So `xmin` costs 4 bytes for each hot row, and `xmax` costs nothing in most leaves. `xmin` is stored because the Entity Framework concurrency token reads `xmin` and later updates `WHERE xmin = $1`. If `xmin` came from a map of limited size, it would change when the entry left the map, and that update would report a false conflict. Document 24 asks document 10 to list these minipages.

A cold segment stores the commit timestamp and the `xmin` of each row as two compressed columns. A bulk load is one transaction, so both columns are constant and cost a few bytes. A delete or update of a cold row puts a delete mark with its commit timestamp in the segment, and the new version goes to the hot store under the same row id (document 10 section 10.5).

### 11.2.2 Undo records

Each transaction has an undo buffer in the memory budget. An update writes a record with the previous header fields, the command number, and the old values of the changed columns only. An insert or a delete writes a record with no values. Above 4 MiB for one transaction, which is a budget, the older undo spills to undo extents in the file. Undo is never logged and never read by recovery, because no unsafe change reaches a page on disk (section 11.15).

Undo stays after commit while a snapshot can need it (section 11.4.3).

### 11.2.3 Why in place

An in-place update keeps the row id and the position of the row, so an index changes only when an indexed column changes. rupg needs no VACUUM. `VACUUM` is accepted and does no work on the hot store. `VACUUM FULL` rewrites the table's segments. The cost of in place is that an old snapshot must follow undo chains, and section 11.4.3 bounds it.

## 11.3 Visibility

A snapshot is a commit timestamp `S`, the reading transaction and its command number. A version is visible when its `stamp` is a commit timestamp at or below `S`, or when it belongs to the reader with a command number below the current one. Otherwise the reader follows the undo chain.

**Taking a snapshot.** A snapshot must not miss a commit with a lower timestamp that is still being installed. One global word, `visible`, holds the highest timestamp below which every commit is installed. A snapshot reads it. A commit moves it forward over itself and every installed commit after it. This is the ordered commit of Alhomssi and Leis, and a snapshot costs one load of a shared cache line. A short lock orders the timestamps as commits take them, so the lowest timestamp that is not installed only goes up. The word and the clock are in `crates/rupg-txn/src/visible.rs` and `crates/rupg-txn/src/clock.rs`.

`READ COMMITTED` takes a snapshot for each statement. `REPEATABLE READ` and `SERIALIZABLE` take one at the first statement.

**Visibility before durability.** A commit is visible when it is installed, before its log block is durable. Otherwise the next writer of a hot row waits for a log sync. A transaction that reads or overwrites a version that is not yet safe records a dependency on it (section 11.4.1). A statement that only reads, and saw an unsafe version, waits before `CommandComplete` until that version is safe. The check is one comparison for each version read. A crash can still lose a commit whose rows a client received. Document 24 keeps open whether the wait moves to the first `DataRow`.

## 11.4 The commit protocol and cleanup

### 11.4.1 Dependencies

A transaction keeps one entry for each log ring: the highest position in that ring of a commit whose version it read or overwrote while that version was unsafe. Most TPC-C transactions have no entry or one. The set is a small array, cleared at the start of each transaction.

### 11.4.2 Commit steps

A writing transaction commits on its worker in five steps:

1. Take a commit timestamp from the node's hybrid logical clock.
2. Encode a commit block with the records, the timestamp, the full transaction id and the dependencies, and append it to the worker's ring (section 11.13).
3. Install. Write the timestamp into the `stamp` of every owned row, release the row locks, wake the waiters and move `visible` forward.
4. Release the table locks.
5. Wait until the commit is safe (section 11.14), then send `CommandComplete`.

Steps 3 and 4 before step 5 are early lock release. The next writer of a hot row runs before the first commit is durable, and it records a dependency. The SIGMOD 2025 paper shows that this, with dependencies across per-worker logs, removes the global flush order of group commit. Document 03 must measure our numbers.

A read-only transaction writes no block. A rollback applies its undo in reverse, releases its locks and writes an abort block only if it already wrote part blocks (section 11.13.3).

### 11.4.3 Cleanup of old versions

The usual rule keeps every version newer than the oldest active snapshot. Under that rule one long analytic query stops cleanup for the whole node. Alhomssi and Leis show this collapse in PostgreSQL, InnoDB and WiredTiger. rupg must not collapse, because document 02 requires TPC-C and ClickBench on one node at the same time.

rupg uses a precise rule. Active snapshots are kept in a sorted list. An old version with commit timestamp `a`, replaced at `b`, is needed only if an active snapshot `S` exists with `a <= S < b`. A cleanup task on each worker walks committed undo in commit order and removes every record whose interval holds no active snapshot. A long query keeps at most one old version for each row changed since its snapshot.

A deleted row that an older snapshot can still see moves from its page to a graveyard index keyed by row id, so that new scans do not step over it. Old snapshots also read the graveyard for the row id ranges they scan. This is the Graveyard Index of the same paper. Without an older snapshot, the row leaves its page at commit.

An analytic query reads mostly cold segments, which are immutable and carry their own timestamps and delete marks. So it holds hot undo only for rows updated after its snapshot.

**Budget.** The undo that long snapshots hold must stay below 10 percent of `rupg.memory_limit`. Above that, the oldest undo spills to the file. No transaction fails because of the age of its snapshot. PostgreSQL 17 removed `old_snapshot_threshold`. The 10 percent is a budget.

## 11.5 Isolation levels

PostgreSQL has three levels, and `READ UNCOMMITTED` is `READ COMMITTED`. rupg prevents and allows the same anomalies, so that the Hermitage table of `../2140/compat/postgres/10-transactions.md` holds row by row: G0, G1a, G1b, G1c and OTV are prevented at every level; PMP, lost update and read skew are possible only at `READ COMMITTED`; write skew and G2 are prevented only at `SERIALIZABLE`.

### 11.5.1 Read committed and the recheck

An `UPDATE`, `DELETE`, `SELECT FOR UPDATE` or `MERGE` that finds a row changed by an uncommitted transaction waits for it. If it rolled back, the statement goes on. If it deleted the row, the statement skips it. If it updated the row, the statement evaluates its `WHERE` clause again on the newest version and goes on only if the clause holds. This is PostgreSQL's EvalPlanQual. The newest version is the row in the page, so the recheck walks no chain.

For a join, PostgreSQL rechecks with the newest version of the locked row and the other tables' rows from the snapshot, except that rows with their own row mark are fetched again by `EvalPlanQualFetchRowMark`. rupg must do the same, because the specs `eval-plan-qual` and `eval-plan-qual-trigger` check the exact rows. The locking operator keeps the join inputs of each row it locks. Document 14 owns that operator.

### 11.5.2 Repeatable read

`REPEATABLE READ` is snapshot isolation with first-updater-wins. A row whose newest committed version is newer than the snapshot gives `40001` "could not serialize access due to concurrent update", or "concurrent delete" for a delete. A row owned by an uncommitted transaction makes the second writer wait, and it fails only if the first commits.

### 11.5.3 Serializable

`SERIALIZABLE` is serializable snapshot isolation (Cahill, Röhm and Fekete, SIGMOD 2008; Ports and Grittner, PVLDB 5, 2012). Reads are recorded, writes that overwrite a concurrent read record a read-write conflict, and a pivot of two consecutive conflicts whose end committed first fails with `40001` "could not serialize access due to read/write dependencies among transactions".

**What a read records.** A point read records a row id. A range scan records a key range on the index it used, which is precision locking as in `../2140/engine-v4/08-concurrency.md` (W8). A scan of hot pages or a cold segment records a row id range. When one transaction's records on one table pass `max_pred_locks_per_relation`, they become one table record. A writer looks up a hash table by row id and an interval tree for each index and for row id ranges.

**Victim choice.** rupg must use the rules of PostgreSQL's `OnConflict_CheckForSerializationFailure` and `PreCommit_CheckForSerializationFailure` to choose the failing transaction and the step. The isolation specs encode both.

**Expected differences.** PostgreSQL's page and relation granularity gives false positives that some expected outputs contain. rupg records keys and row ids. We expect differences in `predicate-gin`, `predicate-gist` and `predicate-hash`, and possibly where PostgreSQL promotes locks. We do not fake a false positive. Each difference gets an alternate expected file with a comment that names the rule, and document 05 counts these files. The real list is known when M5 runs the suite.

**Deferrable.** `SERIALIZABLE READ ONLY DEFERRABLE` waits for a safe snapshot and then records no reads. A read-only transaction whose snapshot becomes safe stops recording, as in PostgreSQL.

**Cost.** Only serializable transactions record reads. G5 runs TPC-C at `READ COMMITTED`, with a `SERIALIZABLE` row published beside it.

## 11.6 Savepoints

A savepoint is a mark on three stacks: the undo position, the lock list length and the command number. `ROLLBACK TO SAVEPOINT` applies the undo after the mark, releases the locks taken after it and keeps the savepoint. `RELEASE SAVEPOINT` removes the savepoint and every later one. A repeated name refers to the newest. An unknown name fails with `3B001`. `ROLLBACK TO SAVEPOINT` is valid in the failed state and returns the session to `T`.

pgJDBC with `autosave=always` and psql with `ON_ERROR_ROLLBACK` put a savepoint around each statement, so a savepoint must cost about a function call and must allocate nothing. PostgreSQL slows down past 64 subtransactions in a backend, and rupg has no such limit.

A subtransaction that writes gets an id from the same counter, so `xmin` of a row inserted after a savepoint shows that id, as in PostgreSQL. The commit map maps it to the top transaction.

## 11.7 Row locks

The four strengths and their conflicts are PostgreSQL's.

| Held \ Requested | KEY SHARE | SHARE | NO KEY UPDATE | UPDATE |
|---|---|---|---|---|
| KEY SHARE | | | | conflict |
| SHARE | | | conflict | conflict |
| NO KEY UPDATE | | conflict | conflict | conflict |
| UPDATE | conflict | conflict | conflict | conflict |

An `UPDATE` that changes no column of a unique index takes `NO KEY UPDATE`. Other updates and deletes take `UPDATE`. A foreign key check takes `KEY SHARE`.

**Where a lock lives.** One locker lives in the header: the `stamp` names the slot, the flags hold the strength and the lock-only bit, and the `xmax` minipage holds the locker's `xid`. A second shared locker creates a lock group in memory, and `xmax` then holds a group number from a separate 32-bit counter, which `pg_get_multixact_members()` reads. Groups are not stored on disk, because the locks of a prepared transaction are in its prepare block (section 11.10). After the locker ends, `xmax` keeps its `xid` until the row changes, as in PostgreSQL.

**Waiting.** A waiter waits for the holding transaction. A queue for the row, keyed by row id, is created on the first waiter and is first come first served. `pg_locks` shows a `transactionid` lock in `ShareLock` mode and a `tuple` lock, as in PostgreSQL.

**NOWAIT and SKIP LOCKED.** `NOWAIT` fails with `55P03` at the first conflict. `SKIP LOCKED` skips a conflicting row and never waits. With `LIMIT`, the locking operator sits below the limit and locks rows in scan order until it has enough, as PostgreSQL's `LockRows` node does. Job queues such as Oban, Que and pg-boss depend on it. A locking clause with an aggregate, `DISTINCT`, `GROUP BY` or a window fails with `0A000`.

## 11.8 Table locks, advisory locks, deadlocks and timeouts

### 11.8.1 Table locks

The eight modes and their conflict table are PostgreSQL's, and each statement takes the mode that PostgreSQL takes. Locks are held to the end of the transaction.

The three weak modes, `ACCESS SHARE`, `ROW SHARE` and `ROW EXCLUSIVE`, conflict only with the strong modes. Each table has a strong lock count. When it is zero, a weak lock increments a counter in the worker's own array and touches no shared line. A strong locker increments the strong count and waits until every worker's weak counter for the table is zero. PostgreSQL's fast path uses the same split. TPC-C takes only weak locks, so G5 touches no shared lock state.

### 11.8.2 Advisory locks and pg_locks

The 21 `pg_proc` rows over 11 names are implemented: session and transaction forms, shared and exclusive, and `pg_try_` forms that return false. They are reentrant. Unlocking a lock not held returns false with the warning "you don't own a lock of type ExclusiveLock". The `bigint` key and the `integer` pair are separate spaces, shown in `pg_locks` with `objsubid` 1 and 2.

`pg_locks` is built from the lock manager, the row queues, the advisory locks and the transaction slots. Each transaction shows its `virtualxid` lock and, with an id, its `transactionid` lock. `pg_blocking_pids()` and `pg_isolation_test_session_is_blocked()` read the same state.

### 11.8.3 Deadlocks

A waiter sleeps for `deadlock_timeout`, default 1,000 ms. Then it searches the wait-for graph of row, table and advisory waits. A cycle through it fails it with `40P01` "deadlock detected" and a `DETAIL` line for each edge, for example "Process 123 waits for ShareLock on transaction 456; blocked by process 789."

The session that searches is the one that fails. The specs choose the victim with different `deadlock_timeout` values. rupg must also reorder a table lock queue to break a soft deadlock, as PostgreSQL does, because `deadlock-soft` and `deadlock-soft-2` test it.

### 11.8.4 Timeouts and cancel

| Setting or event | Result |
|---|---|
| `statement_timeout` | `57014` |
| `lock_timeout`, default 0 | `55P03` "canceling statement due to lock timeout" |
| `idle_in_transaction_session_timeout` | `25P03`, FATAL |
| `transaction_timeout` | `25P04`, FATAL |
| `idle_session_timeout` | `57P05`, FATAL |
| a cancel request | `57014` "canceling statement due to user request" |

A cancel or timeout sets a flag on the session task. The executor checks it between morsels of about 1 ms (document 04) and at every wait.

## 11.9 xid, xid8, xmin and xmax

The transaction id is 64 bits, local to the node and assigned at the first write. rupg shows the low 32 bits as `xid` and the full value as `xid8`.

**Skipped values.** PostgreSQL reserves the 32-bit values 0, 1 and 2. The counter skips every value whose low 32 bits are 0, 1 or 2. The first id is 3.

**Functions.** `txid_current()` and `pg_current_xact_id()` return the full id and assign one if needed. `txid_current_if_assigned()` and `pg_current_xact_id_if_assigned()` return NULL before the first write. `pg_xact_status(xid8)` reads the commit map, returns NULL for an id older than the map, and fails for a future id, as in PostgreSQL.

**Snapshots.** `pg_current_snapshot()` builds `xmin:xmax:xip_list` at the call. `xmax` is the next id. `xip_list` holds the ids that were running at the snapshot: uncommitted slots, and commit map entries with a timestamp above the snapshot. `xmin` is the smallest of them. `pg_visible_in_snapshot()` uses the same rule.

**The commit map.** One entry of 16 bytes for each writing transaction: commit timestamp and full id. It is a ring in memory, saved at each checkpoint, sized by `rupg.commit_map_size`, default 64 MiB or about 4 million transactions, which is a budget. A sample every 2^20 ids never expires and gives the age of any `xid` within 2^20.

**`xmin` and `xmax` of a row.** These are the stored values of the minipages of section 11.2.1 or of the cold columns. They do not change when the map moves on, so the Entity Framework token works. A row whose writer is more than 2,000,000,000 ids behind the next id shows `xmin` 2, which is PostgreSQL's frozen id, so that the 32-bit value is never read as a future id. A live row that was never locked shows `xmax` 0.

**`age()`.** `age(xid)` uses 32-bit arithmetic, and a frozen row gives 2,147,483,647. `age(datfrozenxid)` and `age(relfrozenxid)` give the age of the oldest `xmin` not shown as frozen, capped at 200,000,000, the default `autovacuum_freeze_max_age`, so that monitoring tools do not alarm.

**`cmin` and `cmax`.** These show the command number for rows of the current transaction, from its undo, and 0 for other rows. PostgreSQL shows the stored command id of a committed row. We expect nothing to depend on it, and document 05 lists it as a known difference.

**`pg_xact_commit_timestamp()`.** It follows `track_commit_timestamp`, default off, and fails with `55000` when off, as in PostgreSQL. When on, it returns the physical part of the commit timestamp.

## 11.10 Two-phase commit

`max_prepared_transactions` defaults to 0, and `PREPARE TRANSACTION` then fails with `55000` "prepared transactions are disabled". The test `prepared_xacts` has the alternate file `prepared_xacts_1.out` for this, and rupg must match it with the default and the main file otherwise.

`PREPARE TRANSACTION 'gid'` writes a prepare block with the records, the full id, the gid, the owner, the database, the row locks and the table locks. It waits until the block is safe. Then it detaches the transaction, with its locks, rows and undo, from the session. `COMMIT PREPARED` writes a commit block that names the prepare block and installs as in section 11.4.2. `ROLLBACK PREPARED` applies the undo and writes an abort block. `pg_prepared_xacts` lists them.

Recovery rebuilds every prepare block with no decision: it applies its changes in memory as uncommitted rows, builds its undo and takes its locks again. A checkpoint copies the state of each prepared transaction, so the ring can wrap past the prepare block. The cluster puts its prepare records in Raft logs (document 18).

## 11.11 Sequences

Sequences are not transactional. `CACHE` defaults to 1. `currval()` and `lastval()` fail with `55000` before the first `nextval()`, and passing the limit fails with `2200H`. A sequence is one hot row changed under a latch with no row lock. A sequence record is logged for every 32 values ahead, as PostgreSQL does, so a crash can skip up to 32 values. A transaction that commits after `nextval()` makes the sequence record safe with its commit.

## 11.12 The LSN

PostgreSQL clients read `pg_lsn` values, compare them and subtract them. rupg has one ring for each worker and no byte position that orders all commits. It shows a commit timestamp as the LSN. That is 64 bits, monotonic and ordered with commits. The text form is PostgreSQL's: the high and low 32 bits in hexadecimal with a slash between.

| Function | rupg value |
|---|---|
| `pg_current_wal_lsn()`, `pg_current_wal_flush_lsn()` | the safe horizon: the highest timestamp below which every commit is safe |
| `pg_current_wal_insert_lsn()` | `visible`, the newest installed commit |
| `pg_last_wal_receive_lsn()` on a replica | the newest commit timestamp received |
| `pg_last_wal_replay_lsn()` on a replica | the newest commit timestamp applied |
| `pg_wal_lsn_diff(a, b)` | `a` minus `b` in timestamp units |
| `pg_walfile_name(lsn)` | computed from the number and `wal_segment_size`, as in PostgreSQL |

**Units.** One millisecond is 65,536 units. A replica one second behind shows 65,536,000, which a dashboard shows as about 62.5 MiB. The number is not bytes, but it is monotonic and grows with time, so alarms on growth work. The interval columns `write_lag`, `flush_lag` and `replay_lag` of `pg_stat_replication` are real times. Document 24 keeps open whether a byte count over rings serves tools better.

**WAIT FOR LSN.** PostgreSQL 19 adds `WAIT FOR LSN`, which waits until a replica has replayed an LSN, with a timeout. rupg implements the statement with the grammar of the pin. On a primary it returns when the safe horizon reaches the LSN. With `rupg.report_commit_lsn` on, rupg also sends the commit timestamp as the `ParameterStatus` value `rupg_commit_lsn` after each commit. The default is off, because a strict driver could reject an unknown parameter.

## 11.13 The log rings

### 11.13.1 Rings in the file

The log is inside the `.rupg` file. Each worker has one ring. Document 08 owns the ring region: a ring is a list of extents that grows and shrinks, and a ring position is a 64-bit byte offset that only increases. The ring directory, with the start position of each ring, is in the checkpoint. A node that starts with fewer workers leaves extra rings idle after recovery, and one with more workers adds rings. In the library, application threads run sessions, so the library keeps `rupg.log_rings` rings, default the number of cores capped at 4, and a committing thread takes a free one. The library allocates a ring at its first commit and releases every ring at a clean close, so a file that is only read has no ring space (document 17).

### 11.13.2 Blocks

A ring holds packed blocks. A flush writes the new blocks and pads the last 4 KiB unit with a fill block, so a flush never writes a unit that is already durable. The format is in `rupg-log/src/block.rs`. All numbers are little endian.

| Offset | Field | Bytes | Meaning |
|---|---|---|---|
| 0 | `length` | 4 | block length, a multiple of 8, at most 16 MiB |
| 4 | `kind` | 1 | 1 commit, 2 part, 3 abort, 4 prepare, 5 commit prepared, 6 abort prepared, 7 sequence, 8 fill |
| 5 | `flags` | 1 | bit 0 compressed, bit 1 has dependencies |
| 6 | `ring` | 2 | ring number |
| 8 | `checksum` | 4 | CRC32C of the block with this field at zero, seeded with the ring number, over the header only for a fill block |
| 12 | `deps` | 2 | number of dependencies |
| 14 | reserved | 2 | zero |
| 16 | `position` | 8 | the ring position of the block |
| 24 | `commit_ts` | 8 | HLC commit timestamp |
| 32 | `xid` | 8 | full transaction id |
| 40 | dependencies | 10 each | ring (2) and the position just after the block that must be safe first (8) |
| after | records | rest | the records, padded with zero bytes to a multiple of 8 |

A fill block has only the header. Its checksum covers the header only, so a fill to the end of an extent writes 40 bytes and not up to 16 MiB. Replay reads the header and goes on after `length` bytes. When fewer than 40 bytes are left in a 4 KiB unit, the writer leaves them and starts the next block at the next unit, and replay does the same. No version writes the compressed flag yet, and a block with it set gives `XX001`.

A block left from an earlier use of an extent holds a lower position than replay expects, or fails its checksum, so replay stops at the first such block. A length that is not a multiple of 8 or is out of range also ends the ring. A block that passes its checksum and its position and still is not valid, for example with an unknown kind, is damage and gives `XX001`. rudb's lanes find the end of the log in a similar way (`rudb-txn/src/log/lane.rs`). A block never spans two extents. When it does not fit, the rest of the extent gets a fill block.

### 11.13.3 Records

A record changes one row: kind, table OID, row id, a bitmap of changed columns and the new values in storage encoding. The shard is in the row id. The kinds are 1 insert, 2 update, 3 delete, 4 catalog change (which also invalidates caches), 5 truncate, and 6 segment. The format is in `rupg-log/src/record.rs`.

A record is the kind byte, the table OID as a zigzag varint delta from the OID of the record before it, the row id as a zigzag varint delta from the row id before it, the bitmap width as a varint, the bitmap, and one value for each set bit. The first record of a block takes its deltas from zero. The width is the last changed column plus 1, so the last bit of the bitmap is always set and a record has one form only. A value is a varint of its length plus 1 and then its bytes, or the varint 0 for NULL. No record starts with a zero byte, so the zero padding of a block ends its records. A segment record names a new cold segment's extents and row id range. A bulk load writes and syncs its segments before the commit block, so it logs only segment records and does not write its data twice.

A transaction keeps its records in a private buffer for its commit block. Past 1 MiB, a budget, it writes a part block to its ring and goes on, and its commit block lists its part blocks as dependencies. A rollback of such a transaction writes an abort block.

The log is redo only (section 11.15).

## 11.14 Commit durability

### 11.14.1 Flushing a ring

A worker flushes its ring when a commit waits and no flush runs. When the next block does not fit between the end of the ring and the redo position plus the ring size, the append fails with `53100`, and a checkpoint must move the redo position first. The ring writer is in `rupg-log/src/ring.rs`. Commits that arrive during a flush go in the next one. This is group commit inside a worker, with no order between workers. On Linux the file is opened with `O_DIRECT`, and a flush is one `io_uring` write with `RWF_DSYNC`, which is a Force Unit Access write on a device that supports it, and otherwise a write and `fdatasync`. macOS uses `F_FULLFSYNC`.

On a device with a slow cache flush, flushes from many workers can cost more than one shared flush. `rupg.log_flush` selects `per_worker`, the default, or `grouped`, where one flusher writes the new pages of every ring and syncs once. macOS defaults to `grouped`, because `F_FULLFSYNC` flushes the whole device. gpc measured `fdatasync` at 2,247 µs p50 on WSL2 (`../2140/bench/tpc-c/`), and there we expect `grouped` to win. Document 03 measures both.

### 11.14.2 When a commit is safe

Each ring has a durable position and a safe position. The safe position moves over a commit block when the block is durable and every dependency `(r, p)` has ring `r` safe at or above `p`. A commit is acknowledged when its ring's safe position reaches it. A commit that waits flushes its own ring and then each ring that a block before it waits for, so no other thread has to flush for it. The safe positions are in `rupg-log/src/rings.rs`.

The safe position is a prefix. A commit behind a block with an unsafe dependency waits for it, which costs at most one flush of another ring and gives recovery a simple rule. A worker takes the timestamp and appends in one step, so blocks in a ring are in timestamp order. A dependency always points to a lower timestamp, so no cycle of waits forms.

### 11.14.3 synchronous_commit

| Setting | Single node | Cluster (document 18) |
|---|---|---|
| `on` | wait until safe | a quorum has the block durable |
| `remote_apply` | wait until safe | a quorum has applied the block |
| `remote_write` | wait until safe | a quorum has written the block to the operating system |
| `local` | wait until safe | safe on the leader only |
| `off` | no wait | no wait |

With `off`, each ring is flushed at least once in each `wal_writer_delay`, default 200 ms. PostgreSQL can lose up to three `wal_writer_delay` periods. `commit_delay` and `commit_siblings` delay a ring flush in the same way. `fsync = off` is as unsafe as in PostgreSQL. Document 02 requires the same setting on both sides of a comparison.

## 11.15 Checkpoints

**Out-of-place writes.** A page writer writes dirty pages to new locations and records them in the page table (document 08 section 8.6). Lee, Ziegler and Leis report 1.65x to 2.24x higher throughput and 6.2x to 9.8x fewer flash writes.

**No uncommitted data on disk.** The writer reads `visible`, then copies the page under its latch. On the copy only, it applies the undo of every version that a running transaction owns. A page with a committed version that is not yet safe waits until the rings are durable for it, as document 08 section 8.6 requires. So the log needs no undo, recovery never reads undo extents, and recovery has nothing to undo in pages. It only drops part blocks that have no safe commit.

**Two stamps decide replay.** The page timestamp is the value of `visible` that the writer read before the copy. Every commit at or below it was installed before the copy, so its changes are in the page, and recovery skips the page for those records. The `stamp` of a written row is the timestamp of the newest committed version in the copy. For a record above the page timestamp, recovery applies it only if its commit timestamp is above the row's `stamp`. The row lock orders writes to one row, so the test is exact.

**A checkpoint** is incremental, as document 08 section 8.6 specifies. It notes the safe position of every ring as its redo position, lets the writer write the dirty pages older than those positions at a steady rate, writes the page table root, the ring directory with the noted positions, the prepared transactions, the sequences, the HLC high-water mark, the next transaction id and the commit map, syncs, and points a header slot at the result (document 08 section 8.6). Ring space before the noted positions and pages that only the previous checkpoint referenced are then free. A redo position must not pass the first part block of a running transaction, because recovery reads no block before the redo position and the commit block needs its part blocks.

**When.** At `checkpoint_timeout`, default 5 min, when any one ring holds more than `max_wal_size`, default 1 GB, or at `CHECKPOINT`. LeanStore reports incremental checkpoints writing about 1 percent of the buffer pool for each GB of log (PVLDB 17, 2024). We expect the same order, and document 19 counts the bytes.

**A clean close** writes a checkpoint after which every ring is empty, so the file can be copied alone (document 17).

## 11.16 Recovery

On open after a crash:

1. Read the newest valid header slot and its checkpoint.
2. Read each ring from its redo position until a block fails its checksum or holds an unexpected position. That is the ring's durable end. An end before the durable position of the checkpoint gives `XX001`.
3. Cut each ring at its first block with a dependency `(r, p)` beyond the end or the cut of ring `r`, or on a ring that the checkpoint does not hold. Repeat until no cut moves. What is left is the safe prefix, and it holds every acknowledged commit.
4. Apply the records of the safe prefixes with the two stamps of section 11.15, on every worker in parallel, partitioned by page.
5. Drop part blocks that no block of a safe prefix names as a dependency. No page holds their changes.
6. Rebuild prepared transactions, sequences and the commit map, and set the HLC above the highest timestamp seen.
7. Write a checkpoint that starts each ring one ring size after its end, rounded up to a 4 KiB unit, before any new block. A flush that the crash stopped can leave valid blocks after a hole at the end. Their positions are one ring size below the positions that a later read expects in the same places, so no later recovery reads them again.

A block beyond a cut was never acknowledged, so dropping it is correct. The scan, the cut and step 7 are in `crates/rupg-log/src/recovery.rs`. The crash test there cuts the power at random points and checks that no acknowledged commit is lost, that no aborted or unknown block comes back, and that a kept commit keeps its part blocks and its dependencies.

**Budget.** Recovery must apply at least 1 GB of log per second with 16 cores. This is a budget, tested by the crash tests of document 21. Because the trigger is per ring, the worst case is every ring full: 16 rings of 1 GB cost about 16 s of replay plus page reads. Document 24 asks whether the trigger should be the sum over rings.

**Torn pages.** A page write never touches a page that the last checkpoint references, so a torn write only damages pages that recovery does not read. `full_page_writes` is accepted and has no effect.

## 11.17 Replication streams

**rupg to rupg.** A replica receives the primary's blocks and applies them as recovery does. At M10 this becomes the Raft log of shard 0 (document 18).

**PostgreSQL physical replication** is not served, because a rupg log is not PostgreSQL WAL. `pg_basebackup` against rupg fails with `0A000`, and document 05 lists it outside the counted surface.

**Logical replication out.** rupg serves `START_REPLICATION SLOT ... LOGICAL` with `pgoutput` and `test_decoding`. A walsender task merges the safe blocks of all rings in timestamp order and decodes them with the catalog as of each commit. A slot keeps `restart_lsn` and `confirmed_flush_lsn` as timestamps. A lagging slot holds ring space, and when the ring is full the needed blocks are copied to a retention extent in the file, bounded by `max_slot_wal_keep_size`, default no limit as in PostgreSQL. Debezium must work.

**Logical replication in.** `CREATE SUBSCRIPTION` to a PostgreSQL 14 to 19 publisher, with the initial copy. This is also the online path of the importer. Both directions are in M9.

## 11.18 Backup and point-in-time recovery

`pg_backup_start()` pins the current checkpoint. While pinned, no page that the checkpoint references and no ring space is reused. A plain copy of the `.rupg` file plus the blocks up to `pg_backup_stop()` is then a consistent backup, because out-of-place writes leave the pinned pages unchanged. `pg_backup_stop()` removes the pin and returns the LSN. `rupg backup` in the CLI does this and writes one file.

`archive_mode`, `archive_command` and `archive_library` archive runs of ring blocks, named as PostgreSQL names WAL segments. `restore_command` fetches them. `recovery_target_time` is exact, because the LSN is a time. `recovery_target_lsn`, `recovery_target_xid`, `recovery_target_inclusive` and `recovery_target_action` keep their meaning. Both are in M10.

## 11.19 LISTEN and NOTIFY

A notification is queued in the transaction and sent at install, without duplicates. A payload of 8,000 bytes or more fails with `22023`. Notifications are not durable. `pg_listening_channels()` and `pg_notification_queue_usage()` read the node's queue.

## 11.20 What is measured and when

| Milestone | Gate |
|---|---|
| M1 | rings, blocks, safe positions and recovery, with a crash test that kills the process at random points and checks that every acknowledged commit survives and no unacknowledged change appears |
| M3 | commit on the point path, inside G4 |
| M5 | the 133 scheduled isolation specs with the alternate files of section 11.5.3; savepoints, locks, deadlocks, `LISTEN`, two-phase commit; G5 |
| M7 | G6 |
| M9 | logical replication in and out |
| M10 | replicas, backup and point-in-time recovery |

The interference target of document 14 section 14.11 depends on section 11.4.3. A TPC-C run next to a long ClickBench query must stay within that bound and must not lose throughput as the query runs longer.
