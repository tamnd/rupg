//! The heavyweight locks of the sessions of a server, as `lock.c` keeps them: the advisory locks of the lock method `USER_LOCKMETHOD` and the locks that `LOCK TABLE` takes on relations. Both use the conflicts of `LockConflicts`. A session is the owner of its locks by its process ID.

use std::collections::{HashMap, HashSet};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use rupg_func::AdvisoryKey;
use rupg_platform::Tasks;

/// The tag of a lock: a relation of a database, or the key of an advisory lock.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Tag {
    Relation { database: u32, relation: u32 },
    Advisory { database: u32, key: AdvisoryKey },
}

impl std::fmt::Display for Tag {
    /// `DescribeLockTag`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Tag::Relation { database, relation } => {
                write!(f, "relation {relation} of database {database}")
            }
            Tag::Advisory { database, key } => {
                write!(f, "advisory lock [{database},{},{},{}]", key.high, key.low, key.kind)
            }
        }
    }
}

/// A lock mode, from `AccessShareLock`, which is 1, to `AccessExclusiveLock`, which is 8.
pub(crate) type Mode = u8;

/// `ShareLock`, the mode of a shared advisory lock.
pub(crate) const SHARE: Mode = 5;

/// `ExclusiveLock`, the mode of an exclusive advisory lock.
pub(crate) const EXCLUSIVE: Mode = 7;

/// The names of the modes in the messages, from mode 1.
const NAMES: [&str; 8] = [
    "AccessShareLock",
    "RowShareLock",
    "RowExclusiveLock",
    "ShareUpdateExclusiveLock",
    "ShareLock",
    "ShareRowExclusiveLock",
    "ExclusiveLock",
    "AccessExclusiveLock",
];

/// `LockConflicts`: bit `b` of the entry of mode `a` is set when mode `a` conflicts with mode `b`.
const CONFLICTS: [u16; 9] = [
    0,
    1 << 8,
    1 << 7 | 1 << 8,
    1 << 5 | 1 << 6 | 1 << 7 | 1 << 8,
    1 << 4 | 1 << 5 | 1 << 6 | 1 << 7 | 1 << 8,
    1 << 3 | 1 << 4 | 1 << 6 | 1 << 7 | 1 << 8,
    1 << 3 | 1 << 4 | 1 << 5 | 1 << 6 | 1 << 7 | 1 << 8,
    1 << 2 | 1 << 3 | 1 << 4 | 1 << 5 | 1 << 6 | 1 << 7 | 1 << 8,
    1 << 1 | 1 << 2 | 1 << 3 | 1 << 4 | 1 << 5 | 1 << 6 | 1 << 7 | 1 << 8,
];

/// The name of a mode in the messages.
pub(crate) fn mode_name(mode: Mode) -> &'static str {
    NAMES[usize::from(mode - 1)]
}

/// True when mode `a` conflicts with mode `b`. The table is symmetric.
fn conflicts(a: Mode, b: Mode) -> bool {
    CONFLICTS[usize::from(a)] & 1 << b != 0
}

/// The holds of one owner in one mode, with a count for each level.
#[derive(Debug)]
struct Hold {
    owner: i32,
    mode: Mode,
    session: u32,
    xact: u32,
}

/// A request in the queue of a lock.
#[derive(Debug, Clone, Copy)]
struct Wait {
    owner: i32,
    mode: Mode,
}

/// The holds and the queue of one tag.
#[derive(Debug, Default)]
struct Lock {
    holds: Vec<Hold>,
    queue: Vec<Wait>,
}

impl Lock {
    /// True when another owner holds a mode that conflicts with `mode`.
    fn held_against(&self, owner: i32, mode: Mode) -> bool {
        self.holds.iter().any(|hold| hold.owner != owner && conflicts(mode, hold.mode))
    }

    /// True when the lock in mode `mode` is free for `owner` with the requests of `ahead` before it.
    fn free_for(&self, owner: i32, mode: Mode, ahead: &[Wait]) -> bool {
        !self.held_against(owner, mode)
            && !ahead.iter().any(|wait| wait.owner != owner && conflicts(mode, wait.mode))
    }

    /// `GrantLock`: one more hold of `owner` in mode `mode` at the level.
    fn grant(&mut self, owner: i32, mode: Mode, session: bool) {
        let at = match self.holds.iter().position(|h| h.owner == owner && h.mode == mode) {
            Some(at) => at,
            None => {
                self.holds.push(Hold { owner, mode, session: 0, xact: 0 });
                self.holds.len() - 1
            }
        };
        let hold = &mut self.holds[at];
        if session {
            hold.session += 1;
        } else {
            hold.xact += 1;
        }
    }

    /// The position of the request of `owner` in the queue.
    fn waiting(&self, owner: i32) -> Option<usize> {
        self.queue.iter().position(|wait| wait.owner == owner)
    }
}

/// The result of a request.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Request {
    /// The session holds the lock.
    Granted,
    /// The lock is not free, and the request did not wait.
    Busy,
    /// The request is in the queue of the lock.
    Queued,
    /// `RememberSimpleDeadLock`: the request must go before a request in the queue that waits for a lock that the session holds, but that request holds a lock that conflicts. The cycle is as [`Locks::deadlock`] gives it.
    Deadlock(Vec<Edge>),
}

/// One step of a cycle of waits: the process, the mode that it waits for, and the tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Edge {
    pub(crate) pid: i32,
    pub(crate) mode: Mode,
    pub(crate) tag: Tag,
}

/// The heavyweight locks of a server.
#[derive(Debug, Default)]
pub(crate) struct Locks {
    table: Mutex<HashMap<Tag, Lock>>,
    /// The signal of a release, or of a request that leaves a queue.
    changed: Condvar,
}

impl Locks {
    fn table(&self) -> MutexGuard<'_, HashMap<Tag, Lock>> {
        self.table.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `LockAcquire`: a hold that the owner has in this mode gets one more count. Otherwise the lock is free when no other owner holds or waits for a mode that conflicts. A lock that is not free gives [`Request::Busy`] without `wait`, and puts the request in the queue with `wait`, as `ProcSleep` does.
    pub(crate) fn request(
        &self,
        tag: Tag,
        owner: i32,
        mode: Mode,
        session: bool,
        wait: bool,
    ) -> Request {
        let mut table = self.table();
        let lock = table.entry(tag).or_default();
        if lock.holds.iter().any(|h| h.owner == owner && h.mode == mode) {
            lock.grant(owner, mode, session);
            return Request::Granted;
        }
        if lock.free_for(owner, mode, &lock.queue) {
            lock.grant(owner, mode, session);
            return Request::Granted;
        }
        if !wait {
            if lock.holds.is_empty() && lock.queue.is_empty() {
                table.remove(&tag);
            }
            return Request::Busy;
        }
        // A session that holds a mode on the tag goes before the first request that waits for a mode that conflicts with it.
        let held: Vec<Mode> =
            lock.holds.iter().filter(|h| h.owner == owner).map(|h| h.mode).collect();
        let mut at = lock.queue.len();
        for (i, other) in lock.queue.iter().enumerate() {
            if !held.iter().any(|&mine| conflicts(mine, other.mode)) {
                continue;
            }
            if lock.holds.iter().any(|h| h.owner == other.owner && conflicts(mode, h.mode)) {
                return Request::Deadlock(vec![
                    Edge { pid: owner, mode, tag },
                    Edge { pid: other.owner, mode: other.mode, tag },
                ]);
            }
            if lock.free_for(owner, mode, &lock.queue[..i]) {
                lock.grant(owner, mode, session);
                return Request::Granted;
            }
            at = i;
            break;
        }
        lock.queue.insert(at, Wait { owner, mode });
        Request::Queued
    }

    /// `ProcLockWakeup` for the request of `owner` in the queue of `tag`: grants the lock when no other owner holds a mode that conflicts and no request before it in the queue conflicts. Without the grant, it waits for a change or for `slice`, and then looks again. It gives true when the session holds the lock.
    pub(crate) fn wait(
        &self,
        tag: Tag,
        owner: i32,
        session: bool,
        slice: Duration,
        tasks: Option<&dyn Tasks>,
    ) -> bool {
        let mut table = self.table();
        for pass in 0..2 {
            let Some(lock) = table.get_mut(&tag) else { return false };
            let Some(at) = lock.waiting(owner) else { return false };
            let mode = lock.queue[at].mode;
            if lock.free_for(owner, mode, &lock.queue[..at]) {
                lock.queue.remove(at);
                lock.grant(owner, mode, session);
                drop(table);
                self.changed.notify_all();
                return true;
            }
            if pass == 1 {
                break;
            }
            table = match tasks {
                Some(tasks) => {
                    drop(table);
                    tasks.sleep(slice);
                    self.table()
                }
                None => {
                    self.changed
                        .wait_timeout(table, slice)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0
                }
            };
        }
        false
    }

    /// `RemoveFromWaitQueue`: the end of a wait with no lock, after a cancel, a timeout or a deadlock.
    pub(crate) fn leave_queue(&self, tag: Tag, owner: i32) {
        let mut table = self.table();
        if let Some(lock) = table.get_mut(&tag) {
            lock.queue.retain(|wait| wait.owner != owner);
            if lock.holds.is_empty() && lock.queue.is_empty() {
                table.remove(&tag);
            }
        }
        drop(table);
        self.changed.notify_all();
    }

    /// `DeadLockCheck` for the request of `owner`: a cycle of waits that starts at the owner and comes back to it, or `None`. A request waits for each other owner that holds a mode that conflicts, and for each other owner before it in the queue with a mode that conflicts.
    pub(crate) fn deadlock(&self, owner: i32) -> Option<Vec<Edge>> {
        let table = self.table();
        let mut waits: HashMap<i32, (Tag, usize)> = HashMap::new();
        for (tag, lock) in table.iter() {
            for (at, wait) in lock.queue.iter().enumerate() {
                waits.insert(wait.owner, (*tag, at));
            }
        }
        let mut path = Vec::new();
        let mut seen = HashSet::new();
        find_cycle(&table, &waits, owner, owner, &mut path, &mut seen).then_some(path)
    }

    /// `LockRelease` of a hold of the session level: false when the owner has no such hold.
    pub(crate) fn release(&self, tag: Tag, owner: i32, mode: Mode) -> bool {
        let mut table = self.table();
        let Some(lock) = table.get_mut(&tag) else { return false };
        let Some(at) =
            lock.holds.iter().position(|h| h.owner == owner && h.mode == mode && h.session > 0)
        else {
            return false;
        };
        lock.holds[at].session -= 1;
        if lock.holds[at].session == 0 && lock.holds[at].xact == 0 {
            lock.holds.remove(at);
            if lock.holds.is_empty() && lock.queue.is_empty() {
                table.remove(&tag);
            }
            drop(table);
            self.changed.notify_all();
        }
        true
    }

    /// The counts of the holds of the transaction level of the owner, which a savepoint keeps for [`Locks::release_since`].
    pub(crate) fn xact_holds(&self, owner: i32) -> Vec<(Tag, Mode, u32)> {
        let table = self.table();
        let mut holds = Vec::new();
        for (tag, lock) in table.iter() {
            for hold in lock.holds.iter().filter(|h| h.owner == owner && h.xact > 0) {
                holds.push((*tag, hold.mode, hold.xact));
            }
        }
        holds
    }

    /// `LockReleaseCurrentOwner` at the abort of a subtransaction: the holds of the transaction level of the owner go back to the counts of `kept`, so the locks that the subtransaction took go.
    pub(crate) fn release_since(&self, owner: i32, kept: &[(Tag, Mode, u32)]) {
        let mut table = self.table();
        let mut freed = false;
        table.retain(|tag, lock| {
            lock.holds.retain_mut(|hold| {
                if hold.owner != owner || hold.xact == 0 {
                    return true;
                }
                let count =
                    kept.iter().find(|k| k.0 == *tag && k.1 == hold.mode).map_or(0, |k| k.2);
                hold.xact = hold.xact.min(count);
                let kept = hold.session > 0 || hold.xact > 0;
                freed |= !kept;
                kept
            });
            !lock.holds.is_empty() || !lock.queue.is_empty()
        });
        drop(table);
        if freed {
            self.changed.notify_all();
        }
    }

    /// `LockReleaseAll` for one level: releases all the holds of the owner at the session level or at the transaction level.
    pub(crate) fn release_all(&self, owner: i32, session: bool) {
        let mut table = self.table();
        let mut freed = false;
        table.retain(|_, lock| {
            lock.holds.retain_mut(|hold| {
                if hold.owner != owner {
                    return true;
                }
                let count = if session { &mut hold.session } else { &mut hold.xact };
                if *count == 0 {
                    return true;
                }
                *count = 0;
                let kept = hold.session > 0 || hold.xact > 0;
                freed |= !kept;
                kept
            });
            !lock.holds.is_empty() || !lock.queue.is_empty()
        });
        drop(table);
        if freed {
            self.changed.notify_all();
        }
    }
}

/// `FindLockCycleRecurse`: true when a path of waits from `from` comes back to `start`. The path gets the waits of the cycle in order.
fn find_cycle(
    table: &HashMap<Tag, Lock>,
    waits: &HashMap<i32, (Tag, usize)>,
    start: i32,
    from: i32,
    path: &mut Vec<Edge>,
    seen: &mut HashSet<i32>,
) -> bool {
    if !seen.insert(from) {
        return false;
    }
    let Some(&(tag, at)) = waits.get(&from) else { return false };
    let Some(lock) = table.get(&tag) else { return false };
    let mode = lock.queue[at].mode;
    path.push(Edge { pid: from, mode, tag });
    let holders = lock.holds.iter().filter(|h| conflicts(mode, h.mode)).map(|h| h.owner);
    let ahead = lock.queue[..at].iter().filter(|w| conflicts(mode, w.mode)).map(|w| w.owner);
    for blocker in holders.chain(ahead) {
        if blocker == from {
            continue;
        }
        if blocker == start || find_cycle(table, waits, start, blocker, path, seen) {
            return true;
        }
    }
    path.pop();
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag(n: u32) -> Tag {
        Tag::Advisory { database: 5, key: AdvisoryKey { high: 0, low: n, kind: 1 } }
    }

    #[test]
    fn conflict_table() {
        let all: Vec<Vec<Mode>> =
            (1..=8).map(|a| (1..=8).filter(|&b| conflicts(a, b)).collect()).collect();
        assert_eq!(
            all,
            [
                vec![8],
                vec![7, 8],
                vec![5, 6, 7, 8],
                vec![4, 5, 6, 7, 8],
                vec![3, 4, 6, 7, 8],
                vec![3, 4, 5, 6, 7, 8],
                vec![2, 3, 4, 5, 6, 7, 8],
                vec![1, 2, 3, 4, 5, 6, 7, 8],
            ]
        );
        assert_eq!(mode_name(1), "AccessShareLock");
        assert_eq!(mode_name(8), "AccessExclusiveLock");
        let rel = Tag::Relation { database: 5, relation: 16384 };
        assert_eq!(rel.to_string(), "relation 16384 of database 5");
    }

    #[test]
    fn modes_conflict() {
        let locks = Locks::default();
        assert_eq!(locks.request(tag(1), 1, SHARE, true, false), Request::Granted);
        assert_eq!(locks.request(tag(1), 2, SHARE, true, false), Request::Granted);
        assert_eq!(locks.request(tag(1), 2, EXCLUSIVE, true, false), Request::Busy);
        assert_eq!(locks.request(tag(1), 1, EXCLUSIVE, true, false), Request::Busy);
        assert!(locks.release(tag(1), 2, SHARE));
        assert_eq!(locks.request(tag(1), 1, EXCLUSIVE, true, false), Request::Granted);
        assert!(!locks.release(tag(1), 2, SHARE));
        assert!(locks.release(tag(1), 1, SHARE));
        assert!(locks.release(tag(1), 1, EXCLUSIVE));
        assert!(locks.table().is_empty());
    }

    #[test]
    fn levels_count_apart() {
        let locks = Locks::default();
        assert_eq!(locks.request(tag(1), 1, EXCLUSIVE, false, false), Request::Granted);
        assert!(!locks.release(tag(1), 1, EXCLUSIVE));
        assert_eq!(locks.request(tag(1), 1, EXCLUSIVE, true, false), Request::Granted);
        locks.release_all(1, false);
        assert_eq!(locks.request(tag(1), 2, EXCLUSIVE, true, false), Request::Busy);
        locks.release_all(1, true);
        assert_eq!(locks.request(tag(1), 2, EXCLUSIVE, true, false), Request::Granted);
    }

    #[test]
    fn a_subtransaction_releases_its_locks() {
        let locks = Locks::default();
        assert_eq!(locks.request(tag(1), 1, EXCLUSIVE, false, false), Request::Granted);
        assert_eq!(locks.request(tag(1), 1, EXCLUSIVE, true, false), Request::Granted);
        let kept = locks.xact_holds(1);
        assert_eq!(locks.request(tag(1), 1, EXCLUSIVE, false, false), Request::Granted);
        assert_eq!(locks.request(tag(2), 1, EXCLUSIVE, false, false), Request::Granted);
        locks.release_since(1, &kept);
        assert_eq!(locks.xact_holds(1), kept);
        assert_eq!(locks.request(tag(2), 2, EXCLUSIVE, true, false), Request::Granted);
        assert_eq!(locks.request(tag(1), 2, EXCLUSIVE, true, false), Request::Busy);
        locks.release_all(1, false);
        assert!(locks.release(tag(1), 1, EXCLUSIVE));
        assert_eq!(locks.request(tag(1), 2, EXCLUSIVE, true, false), Request::Granted);
    }

    #[test]
    fn queue_order() {
        let locks = Locks::default();
        assert_eq!(locks.request(tag(1), 1, SHARE, true, true), Request::Granted);
        assert_eq!(locks.request(tag(1), 2, EXCLUSIVE, true, true), Request::Queued);
        // A share request waits behind the exclusive request.
        assert_eq!(locks.request(tag(1), 3, SHARE, true, false), Request::Busy);
        assert_eq!(locks.request(tag(1), 3, SHARE, true, true), Request::Queued);
        assert!(!locks.wait(tag(1), 2, true, Duration::ZERO, None));
        assert!(!locks.wait(tag(1), 3, true, Duration::ZERO, None));
        locks.release_all(1, true);
        assert!(!locks.wait(tag(1), 3, true, Duration::ZERO, None));
        assert!(locks.wait(tag(1), 2, true, Duration::ZERO, None));
        assert!(!locks.wait(tag(1), 3, true, Duration::ZERO, None));
        locks.release_all(2, true);
        assert!(locks.wait(tag(1), 3, true, Duration::ZERO, None));
    }

    #[test]
    fn cycles() {
        let locks = Locks::default();
        assert_eq!(locks.request(tag(1), 1, EXCLUSIVE, true, true), Request::Granted);
        assert_eq!(locks.request(tag(2), 2, EXCLUSIVE, true, true), Request::Granted);
        assert_eq!(locks.request(tag(2), 1, EXCLUSIVE, true, true), Request::Queued);
        assert_eq!(locks.deadlock(1), None);
        assert_eq!(locks.request(tag(1), 2, SHARE, true, true), Request::Queued);
        let cycle = locks.deadlock(1).unwrap();
        assert_eq!(
            cycle,
            vec![
                Edge { pid: 1, mode: EXCLUSIVE, tag: tag(2) },
                Edge { pid: 2, mode: SHARE, tag: tag(1) }
            ]
        );
        assert_eq!(locks.deadlock(2).unwrap()[0].pid, 2);
        locks.leave_queue(tag(2), 1);
        assert_eq!(locks.deadlock(2), None);
    }

    #[test]
    fn early_deadlock() {
        let locks = Locks::default();
        assert_eq!(locks.request(tag(1), 1, SHARE, true, true), Request::Granted);
        assert_eq!(locks.request(tag(1), 2, SHARE, true, true), Request::Granted);
        assert_eq!(locks.request(tag(1), 2, EXCLUSIVE, true, true), Request::Queued);
        assert_eq!(
            locks.request(tag(1), 1, EXCLUSIVE, true, true),
            Request::Deadlock(vec![
                Edge { pid: 1, mode: EXCLUSIVE, tag: tag(1) },
                Edge { pid: 2, mode: EXCLUSIVE, tag: tag(1) },
            ])
        );
    }
}
