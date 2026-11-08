//! Serializable snapshot isolation, as `predicate.c` of PostgreSQL does it (spec/11 section 11.5.3).
//!
//! Each transaction in `SERIALIZABLE` has a record from its first snapshot. A read of a table takes a `SIREAD` lock on the table, as the sequential scan of `heap_beginscan` takes it with `PredicateLockRelation`. The lock stays after the commit while a transaction that overlaps it runs. A write into a table that another serializable transaction read records a read-write conflict from the reader to the writer, as `CheckForSerializableConflictIn` does. A read of a table that a concurrent serializable transaction wrote records a conflict from the reader to the writer, as `CheckForSerializableConflictOut` does for the versions of the writer that the scan finds.
//!
//! Two conflicts in a row, `T0 -> T1 -> T2`, where `T2` commits first, can make a cycle. The checks of `OnConflict_CheckForSerializationFailure` at each new conflict and of `PreCommit_CheckForSerializationFailure` at each commit choose the transaction that fails with `40001`, with the same rules and in the same steps as PostgreSQL.

use std::collections::{BTreeSet, HashMap};

use rupg_common::{Error, SqlState};

/// A table in a database: the target of a `SIREAD` lock and of a write.
pub(crate) type Target = (u32, u32);

/// `SerCommitSeqNo`: the order of the commits of the serializable transactions.
type SeqNo = u64;

/// `FirstNormalSerCommitSeqNo`.
const FIRST_NORMAL_SEQ_NO: SeqNo = 2;

/// `SERIALIZABLEXACT`: the record of a serializable transaction.
#[derive(Debug, Default)]
struct Sxact {
    /// `topXid`: the transaction ID, or 0 before the first write.
    xid: u32,
    /// `xmin` of the snapshot of the transaction.
    xmin: u32,
    /// `SeqNo.lastCommitBeforeSnapshot`.
    last_commit_before_snapshot: SeqNo,
    /// `prepareSeqNo`, from `PreCommit_CheckForSerializationFailure`, or `None` before it.
    prepare: Option<SeqNo>,
    /// `commitSeqNo`, or `None` before the commit.
    commit: Option<SeqNo>,
    /// `SeqNo.earliestOutConflictCommit`.
    earliest_out_conflict_commit: SeqNo,
    /// `finishedBefore`: `nextXid` at the end of the transaction.
    finished_before: u32,
    /// `SXACT_FLAG_READ_ONLY`: declared `READ ONLY`, or committed with no write.
    read_only: bool,
    /// The transaction was declared `READ ONLY` at its start.
    declared_read_only: bool,
    /// `SXACT_FLAG_DOOMED`.
    doomed: bool,
    /// `SXACT_FLAG_CONFLICT_OUT`: a conflict out to a transaction that committed before this one.
    conflict_out: bool,
    /// `MyXactDidWrite`.
    did_write: bool,
    /// `inConflicts`: the transactions that read what this one wrote.
    in_conflicts: BTreeSet<u64>,
    /// `outConflicts`: the transactions that wrote what this one read.
    out_conflicts: BTreeSet<u64>,
    /// The tables of the `SIREAD` locks of the transaction.
    reads: BTreeSet<Target>,
    /// The tables that the transaction wrote.
    writes: BTreeSet<Target>,
}

impl Sxact {
    /// `SxactIsCommitted`.
    fn committed(&self) -> bool {
        self.commit.is_some()
    }

    /// `SxactIsPrepared`: the transaction passed `PreCommit_CheckForSerializationFailure`.
    fn prepared(&self) -> bool {
        self.prepare.is_some()
    }

    /// `SxactIsCommitted` and not ended: the transaction runs.
    fn active(&self) -> bool {
        !self.committed()
    }
}

/// The serializable transactions, `PredXact`, and the `SIREAD` locks.
#[derive(Debug, Default)]
pub(crate) struct Ssi {
    /// The ID of the next record.
    next_id: u64,
    /// `LastSxactCommitSeqNo`.
    last_commit: SeqNo,
    /// `CanPartialClearThrough`.
    can_partial_clear_through: SeqNo,
    /// `HavePartialClearedThrough`.
    have_partial_cleared_through: SeqNo,
    /// The records by ID: the transactions that run and the committed ones that a running one overlaps.
    xacts: HashMap<u64, Sxact>,
    /// `SerializableXidHash`: the record of each transaction ID.
    by_xid: HashMap<u32, u64>,
    /// The holders of the `SIREAD` locks of each table.
    locks: HashMap<Target, BTreeSet<u64>>,
}

/// `40001` "could not serialize access due to read/write dependencies among transactions" with the reason of PostgreSQL.
fn failure(reason: &str) -> Error {
    Error::new(
        SqlState::T_R_SERIALIZATION_FAILURE,
        "could not serialize access due to read/write dependencies among transactions",
    )
    .with_detail(format!("Reason code: {reason}."))
    .with_hint("The transaction might succeed if retried.")
}

/// `XidIsConcurrent`: true when the snapshot of `xmin`, `xmax` and the running transactions `running` does not see the transaction `xid`.
pub(crate) type Concurrent<'a> = &'a dyn Fn(u32) -> bool;

impl Ssi {
    /// `GetSerializableTransactionSnapshotInt`: the record of a new serializable transaction whose snapshot has `xmin`. The caller takes the snapshot under the lock of this state.
    pub(crate) fn begin(&mut self, xmin: u32, read_only: bool) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        if self.last_commit < FIRST_NORMAL_SEQ_NO - 1 {
            self.last_commit = FIRST_NORMAL_SEQ_NO - 1;
        }
        self.xacts.insert(
            id,
            Sxact {
                xmin,
                last_commit_before_snapshot: self.last_commit,
                read_only,
                declared_read_only: read_only,
                ..Sxact::default()
            },
        );
        id
    }

    /// The record `id`, which a running transaction has.
    fn get(&self, id: u64) -> Option<&Sxact> {
        self.xacts.get(&id)
    }

    /// `CheckForSerializableConflictOutNeeded`: the error of a transaction that another one doomed.
    fn check_doomed(&self, id: u64, step: &str) -> Result<(), Error> {
        if self.get(id).is_some_and(|me| me.doomed) {
            return Err(failure(&format!("Canceled on identification as a pivot, {step}")));
        }
        Ok(())
    }

    /// `PredicateLockRelation` and `CheckForSerializableConflictOut` for a scan of the table `target` by the transaction `id`: the transaction takes a `SIREAD` lock on the table, and each concurrent serializable transaction that wrote the table gets a conflict from this one. `concurrent` is `XidIsConcurrent` for the snapshot of the transaction.
    ///
    /// # Errors
    ///
    /// `40001` when the transaction is doomed or the conflict makes a dangerous structure where this transaction must fail.
    pub(crate) fn read(
        &mut self,
        id: u64,
        target: Target,
        concurrent: Concurrent<'_>,
    ) -> Result<(), Error> {
        let Some(me) = self.xacts.get_mut(&id) else { return Ok(()) };
        if me.reads.insert(target) {
            self.locks.entry(target).or_default().insert(id);
        }
        let writers: Vec<u64> = self
            .xacts
            .iter()
            .filter(|&(&w, sxact)| w != id && sxact.xid != 0 && sxact.writes.contains(&target))
            .map(|(&w, _)| w)
            .collect();
        if writers.is_empty() {
            return Ok(());
        }
        self.check_doomed(id, "during conflict out checking")?;
        for writer in writers {
            self.conflict_out(id, writer, concurrent)?;
        }
        Ok(())
    }

    /// `CheckForSerializableConflictOut` of the reader `me` for a version of the transaction of the record `writer`.
    fn conflict_out(
        &mut self,
        me: u64,
        writer: u64,
        concurrent: Concurrent<'_>,
    ) -> Result<(), Error> {
        let (Some(reader), Some(sxact)) = (self.get(me), self.get(writer)) else { return Ok(()) };
        if sxact.doomed {
            // A transaction that rolls back makes no conflict.
            return Ok(());
        }
        // A read-only transaction appears to run first when the writer committed with no conflict out to a transaction that committed before the snapshot of the reader.
        if reader.read_only
            && sxact.committed()
            && (!sxact.conflict_out
                || reader.last_commit_before_snapshot < sxact.earliest_out_conflict_commit)
        {
            return Ok(());
        }
        if !concurrent(sxact.xid) || reader.out_conflicts.contains(&writer) {
            return Ok(());
        }
        self.flag_rw_conflict(me, writer, me)
    }

    /// `CheckForSerializableConflictIn` for a write of the table `target` by the transaction `id` with the transaction ID `xid`: each other transaction with a `SIREAD` lock on the table that overlaps this one gets a conflict to this one. `snapshot_xmin` is `xmin` of the snapshot of the transaction.
    ///
    /// # Errors
    ///
    /// `40001` when the transaction is doomed or a conflict makes a dangerous structure where this transaction must fail.
    pub(crate) fn write(
        &mut self,
        id: u64,
        xid: u32,
        target: Target,
        snapshot_xmin: u32,
    ) -> Result<(), Error> {
        if !self.xacts.contains_key(&id) {
            return Ok(());
        }
        self.check_doomed(id, "during conflict in checking")?;
        if let Some(me) = self.xacts.get_mut(&id) {
            me.did_write = true;
            if me.xid == 0 && xid != 0 {
                me.xid = xid;
                self.by_xid.insert(xid, id);
            }
            me.writes.insert(target);
        }
        let holders: Vec<u64> = self
            .locks
            .get(&target)
            .map(|holders| holders.iter().copied().filter(|&h| h != id).collect())
            .unwrap_or_default();
        for holder in holders {
            let Some(sxact) = self.get(holder) else { continue };
            // `CheckTargetForConflictsIn`: a committed reader overlaps the writer while it ended after `xmin` of the snapshot of the writer.
            if !sxact.doomed
                && (!sxact.committed() || snapshot_xmin < sxact.finished_before)
                && !sxact.out_conflicts.contains(&id)
            {
                self.flag_rw_conflict(holder, id, id)?;
            }
        }
        Ok(())
    }

    /// `FlagRWConflict`: the conflict from `reader` to `writer`, after the check of `OnConflict_CheckForSerializationFailure`. `me` is the transaction of the session.
    fn flag_rw_conflict(&mut self, reader: u64, writer: u64, me: u64) -> Result<(), Error> {
        self.on_conflict_check(reader, writer, me)?;
        if let Some(r) = self.xacts.get_mut(&reader) {
            r.out_conflicts.insert(writer);
        }
        if let Some(w) = self.xacts.get_mut(&writer) {
            w.in_conflicts.insert(reader);
        }
        Ok(())
    }

    /// `OnConflict_CheckForSerializationFailure`: whether the new conflict from `reader` to `writer` makes a dangerous structure, which fails the session of `me` or dooms the writer.
    fn on_conflict_check(&mut self, reader: u64, writer: u64, me: u64) -> Result<(), Error> {
        let (Some(r), Some(w)) = (self.get(reader), self.get(writer)) else { return Ok(()) };
        // `R -> W -> T2`, where `W` committed with a conflict out to a transaction that committed before it.
        let mut failed = w.committed() && w.conflict_out;
        if !failed {
            // `W` is a pivot with a conflict out to `T2`, which committed first.
            failed = w.out_conflicts.iter().filter_map(|t2| self.get(*t2)).any(|t2| {
                t2.prepare.is_some_and(|t2_prepare| {
                    r.commit.is_none_or(|c| t2_prepare <= c)
                        && w.commit.is_none_or(|c| t2_prepare <= c)
                        && (!r.read_only || t2_prepare <= r.last_commit_before_snapshot)
                })
            });
        }
        // `T0 -> R -> W`, where `W` committed first.
        if !failed
            && let Some(w_prepare) = w.prepare
            && !r.read_only
        {
            failed = r.in_conflicts.iter().filter_map(|t0| self.get(*t0)).any(|t0| {
                !t0.doomed
                    && t0.commit.is_none_or(|c| c >= w_prepare)
                    && (!t0.read_only || t0.last_commit_before_snapshot >= w_prepare)
            });
        }
        if !failed {
            return Ok(());
        }
        if me == writer {
            return Err(failure("Canceled on identification as a pivot, during write"));
        }
        if w.prepared() {
            return Err(failure(&format!(
                "Canceled on conflict out to pivot {}, during read",
                w.xid
            )));
        }
        if let Some(w) = self.xacts.get_mut(&writer) {
            w.doomed = true;
        }
        Ok(())
    }

    /// `PreCommit_CheckForSerializationFailure`: before the commit of the transaction `id`, a doomed transaction fails, and a pivot that a conflict to this one makes dangerous is doomed. The transaction then has its `prepareSeqNo`.
    ///
    /// # Errors
    ///
    /// `40001` when the transaction is doomed.
    pub(crate) fn pre_commit(&mut self, id: u64) -> Result<(), Error> {
        let Some(me) = self.get(id) else { return Ok(()) };
        if me.doomed {
            return Err(failure("Canceled on identification as a pivot, during commit attempt"));
        }
        let mut doom = Vec::new();
        for &near in &me.in_conflicts {
            let Some(pivot) = self.get(near) else { continue };
            if pivot.committed() || pivot.doomed {
                continue;
            }
            let dangerous = pivot.in_conflicts.iter().any(|&far| {
                far == id
                    || self.get(far).is_some_and(|t| !t.committed() && !t.read_only && !t.doomed)
            });
            if dangerous {
                if pivot.prepared() {
                    return Err(failure(
                        "Canceled on commit attempt with conflict in from prepared pivot",
                    ));
                }
                doom.push(near);
            }
        }
        for near in doom {
            if let Some(pivot) = self.xacts.get_mut(&near) {
                pivot.doomed = true;
            }
        }
        self.last_commit += 1;
        let seq = self.last_commit;
        if let Some(me) = self.xacts.get_mut(&id) {
            me.prepare = Some(seq);
        }
        Ok(())
    }

    /// The smallest `xmin` of the transactions that run, `SxactGlobalXmin`, or `None` when none runs.
    fn global_xmin(&self) -> Option<u32> {
        self.xacts.values().filter(|s| s.active()).map(|s| s.xmin).min()
    }

    /// `ReleasePredicateLocks`: the end of the transaction `id`, with `next_xid` as `nextXid`. A rollback removes its record. A commit keeps the record and its `SIREAD` locks while a transaction that overlaps it runs.
    pub(crate) fn end(&mut self, id: u64, commit: bool, next_xid: u32) {
        let Some(me) = self.get(id) else { return };
        let old_global = self.global_xmin();
        let my_xmin = me.xmin;
        let declared_read_only = me.declared_read_only;
        if !commit {
            self.release(id, false);
        } else {
            self.last_commit += 1;
            let seq = self.last_commit;
            let last_commit = self.last_commit;
            let Some(me) = self.xacts.get_mut(&id) else { return };
            me.finished_before = next_xid;
            me.commit = Some(seq);
            if !me.did_write {
                me.read_only = true;
            }
            let read_only = me.read_only;
            let outs: Vec<u64> = me.out_conflicts.iter().copied().collect();
            let ins: Vec<u64> = me.in_conflicts.iter().copied().collect();
            let mut earliest = me.earliest_out_conflict_commit;
            let mut conflict_out = me.conflict_out;
            for out in outs {
                let Some(t) = self.get(out) else { continue };
                if !read_only && t.committed() {
                    let t_prepare = t.prepare.unwrap_or(0);
                    if !conflict_out || t_prepare < earliest {
                        earliest = t_prepare;
                    }
                    conflict_out = true;
                }
                if t.committed() || t.last_commit_before_snapshot >= last_commit {
                    self.release_conflict(id, out);
                }
            }
            for into in ins {
                let Some(t) = self.get(into) else { continue };
                if t.committed() || t.read_only {
                    self.release_conflict(into, id);
                }
            }
            if let Some(me) = self.xacts.get_mut(&id) {
                me.earliest_out_conflict_commit = earliest;
                me.conflict_out = conflict_out;
            }
        }
        if !declared_read_only && !self.xacts.values().any(|s| s.active() && !s.declared_read_only)
        {
            self.can_partial_clear_through = self.last_commit;
        }
        let new_global = self.global_xmin();
        if old_global == Some(my_xmin) && new_global != old_global {
            self.clear_old(new_global);
        }
    }

    /// `ClearOldPredicateLocks`: the committed transactions that no running transaction overlaps go. While only read-only transactions run, a committed transaction loses its `SIREAD` locks and its conflicts in.
    fn clear_old(&mut self, global_xmin: Option<u32>) {
        let mut finished: Vec<(SeqNo, u64)> =
            self.xacts.iter().filter_map(|(&id, s)| s.commit.map(|c| (c, id))).collect();
        finished.sort_unstable();
        for (commit, id) in finished {
            let Some(sxact) = self.get(id) else { continue };
            if global_xmin.is_none_or(|g| sxact.finished_before <= g) {
                self.release(id, false);
            } else if commit > self.have_partial_cleared_through
                && commit <= self.can_partial_clear_through
            {
                let read_only = sxact.read_only;
                self.release(id, !read_only);
                self.have_partial_cleared_through = commit;
            }
        }
    }

    /// `ReleaseOneSerializableXact`: the `SIREAD` locks and the conflicts in of the transaction `id` go, and with `partial` false, its conflicts out and its record too.
    fn release(&mut self, id: u64, partial: bool) {
        let Some(sxact) = self.xacts.get_mut(&id) else { return };
        let reads = std::mem::take(&mut sxact.reads);
        let ins: Vec<u64> = sxact.in_conflicts.iter().copied().collect();
        let outs: Vec<u64> =
            if partial { Vec::new() } else { sxact.out_conflicts.iter().copied().collect() };
        for target in reads {
            if let Some(holders) = self.locks.get_mut(&target) {
                holders.remove(&id);
                if holders.is_empty() {
                    self.locks.remove(&target);
                }
            }
        }
        for into in ins {
            self.release_conflict(into, id);
        }
        for out in outs {
            self.release_conflict(id, out);
        }
        if !partial && let Some(sxact) = self.xacts.remove(&id) {
            self.by_xid.remove(&sxact.xid);
        }
    }

    /// `ReleaseRWConflict`: the conflict from `reader` to `writer` goes.
    fn release_conflict(&mut self, reader: u64, writer: u64) {
        if let Some(r) = self.xacts.get_mut(&reader) {
            r.out_conflicts.remove(&writer);
        }
        if let Some(w) = self.xacts.get_mut(&writer) {
            w.in_conflicts.remove(&reader);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: Target = (1, 100);
    const U: Target = (1, 101);

    /// Write skew: each transaction reads the table that the other writes, and the second commit fails.
    #[test]
    fn write_skew_fails_at_the_second_commit() {
        let mut ssi = Ssi::default();
        let a = ssi.begin(10, false);
        let b = ssi.begin(10, false);
        let none = |_: u32| true;
        ssi.read(a, T, &none).unwrap();
        ssi.read(b, U, &none).unwrap();
        ssi.read(a, U, &none).unwrap();
        ssi.read(b, T, &none).unwrap();
        ssi.write(a, 10, T, 10).unwrap();
        ssi.write(b, 11, U, 10).unwrap();
        ssi.pre_commit(a).unwrap();
        ssi.end(a, true, 12);
        let error = ssi.pre_commit(b).unwrap_err();
        assert_eq!(error.state(), SqlState::T_R_SERIALIZATION_FAILURE);
        ssi.end(b, false, 12);
        assert!(ssi.xacts.is_empty());
    }

    /// One conflict alone is no anomaly.
    #[test]
    fn one_conflict_commits() {
        let mut ssi = Ssi::default();
        let a = ssi.begin(10, false);
        let b = ssi.begin(10, false);
        let none = |_: u32| true;
        ssi.read(a, T, &none).unwrap();
        ssi.write(b, 11, T, 10).unwrap();
        ssi.pre_commit(b).unwrap();
        ssi.end(b, true, 12);
        ssi.pre_commit(a).unwrap();
        ssi.end(a, true, 12);
        assert!(ssi.xacts.is_empty() && ssi.locks.is_empty());
    }
}
