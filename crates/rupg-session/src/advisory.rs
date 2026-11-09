//! The heavyweight locks of the sessions of a server, as `lock.c` keeps them: the advisory locks of the lock method `USER_LOCKMETHOD` and the locks that statements and `LOCK TABLE` take on relations. All use the conflicts of `LockConflicts`. A session is the owner of its locks by its process ID.
//!
//! The weak modes of a relation, `AccessShareLock`, `RowShareLock` and `RowExclusiveLock`, conflict only with the strong modes. When no owner holds or waits for a strong mode on a relation of the same partition, a weak lock goes in the [`Fast`] path of its owner and touches no shared state. A strong request first counts itself in its partition and moves the weak locks of the relation from all the fast paths to the table, as `FastPathTransferRelationLocks` does.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
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

/// `RowExclusiveLock`, the mode of the table that a statement writes.
pub(crate) const ROW_EXCLUSIVE: Mode = 3;

/// `ShareLock`, the mode of a shared advisory lock.
pub(crate) const SHARE: Mode = 5;

/// `ExclusiveLock`, the mode of an exclusive advisory lock.
pub(crate) const EXCLUSIVE: Mode = 7;

/// `ShareUpdateExclusiveLock`, the first strong mode. The modes before it go on the fast path.
const STRONG: Mode = 4;

/// `FAST_PATH_STRONG_LOCK_HASH_PARTITIONS`.
const PARTITIONS: usize = 1024;

/// The partition of the count of strong locks of a relation tag, or `None` for an advisory tag.
fn partition(tag: Tag) -> Option<usize> {
    match tag {
        Tag::Relation { relation, .. } => Some(relation as usize % PARTITIONS),
        Tag::Advisory { .. } => None,
    }
}

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

    /// True when an owner holds or waits for a strong mode.
    fn strong(&self) -> bool {
        self.holds.iter().any(|hold| hold.mode >= STRONG)
            || self.queue.iter().any(|wait| wait.mode >= STRONG)
    }

    /// The hold of `owner` in mode `mode`, which is new with no counts when the owner has none.
    fn hold(&mut self, owner: i32, mode: Mode) -> &mut Hold {
        let at = match self.holds.iter().position(|h| h.owner == owner && h.mode == mode) {
            Some(at) => at,
            None => {
                self.holds.push(Hold { owner, mode, session: 0, xact: 0 });
                self.holds.len() - 1
            }
        };
        &mut self.holds[at]
    }

    /// `GrantLock`: one more hold of `owner` in mode `mode` at the level.
    fn grant(&mut self, owner: i32, mode: Mode, session: bool) {
        let hold = self.hold(owner, mode);
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

/// The weak locks of the transaction of one owner on relations that took the fast path, as the `fpRelId` and the `fpLockBits` of a `PGPROC` keep them: the tag, the mode and the count.
#[derive(Debug)]
pub(crate) struct Fast {
    owner: i32,
    holds: Mutex<Vec<(Tag, Mode, u32)>>,
}

impl Fast {
    fn holds(&self) -> MutexGuard<'_, Vec<(Tag, Mode, u32)>> {
        self.holds.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The heavyweight locks of a server.
#[derive(Debug)]
pub(crate) struct Locks {
    table: Mutex<HashMap<Tag, Lock>>,
    /// False while the table is empty. A request sets it under the lock of the table, and [`Locks::release_all`] clears it when the table becomes empty, so that the end of a transaction with no locks takes no lock.
    used: AtomicBool,
    /// The signal of a release, or of a request that leaves a queue.
    changed: Condvar,
    /// `FastPathStrongRelationLocks`: for each partition, the number of the relation tags in the table with a hold or a request of a strong mode, and the strong requests that have not yet looked at the table.
    strong: Box<[AtomicU32]>,
    /// The fast paths of the owners.
    fast: Mutex<Vec<Arc<Fast>>>,
    /// `WaitForLockers`: for each owner that waits for the transactions of other owners to end, as `VirtualXactLock` waits for a virtual transaction ID, each of those owners and true once its transaction ended.
    watches: Mutex<HashMap<i32, Vec<(i32, bool)>>>,
    /// The number of the owners in `watches`, so that the end of a transaction takes no lock while no owner waits.
    watching: AtomicUsize,
}

impl Default for Locks {
    fn default() -> Locks {
        Locks {
            table: Mutex::default(),
            used: AtomicBool::default(),
            changed: Condvar::default(),
            strong: (0..PARTITIONS).map(|_| AtomicU32::new(0)).collect(),
            fast: Mutex::default(),
            watches: Mutex::default(),
            watching: AtomicUsize::new(0),
        }
    }
}

impl Locks {
    fn table(&self) -> MutexGuard<'_, HashMap<Tag, Lock>> {
        self.table.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn fasts(&self) -> MutexGuard<'_, Vec<Arc<Fast>>> {
        self.fast.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn watches(&self) -> MutexGuard<'_, HashMap<i32, Vec<(i32, bool)>>> {
        self.watches.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `GetLockConflicts` and the start of `WaitForLockersMultiple`: `owner` starts to wait for the transactions of the other owners that hold a mode that conflicts with `mode` on a tag of `tags`, on a fast path or in the table. False when there is none, and then the owner does not wait.
    pub(crate) fn watch(&self, owner: i32, tags: &[Tag], mode: Mode) -> bool {
        let mut watches = self.watches();
        self.watching.fetch_add(1, Ordering::SeqCst);
        let mut lockers: Vec<(i32, bool)> = Vec::new();
        for (tag, pid, held, granted, _) in self.status() {
            if granted
                && pid != owner
                && tags.contains(&tag)
                && conflicts(mode, held)
                && !lockers.iter().any(|&(p, _)| p == pid)
            {
                lockers.push((pid, false));
            }
        }
        if lockers.is_empty() {
            self.watching.fetch_sub(1, Ordering::SeqCst);
            return false;
        }
        if watches.insert(owner, lockers).is_some() {
            self.watching.fetch_sub(1, Ordering::SeqCst);
        }
        true
    }

    /// The owners whose transactions `owner` still waits for since [`Locks::watch`].
    pub(crate) fn watched(&self, owner: i32) -> Vec<i32> {
        if self.watching.load(Ordering::SeqCst) == 0 {
            return Vec::new();
        }
        let watches = self.watches();
        watches.get(&owner).map_or_else(Vec::new, |lockers| {
            lockers.iter().filter(|&&(_, ended)| !ended).map(|&(pid, _)| pid).collect()
        })
    }

    /// The end of the wait of `owner` for the transactions of other owners.
    pub(crate) fn unwatch(&self, owner: i32) {
        if self.watches().remove(&owner).is_some() {
            self.watching.fetch_sub(1, Ordering::SeqCst);
        }
    }

    /// The end of the transaction of `owner`, after it released its locks, for the owners that wait for it.
    fn xact_ended(&self, owner: i32) {
        if self.watching.load(Ordering::SeqCst) == 0 {
            return;
        }
        for lockers in self.watches().values_mut() {
            for (pid, ended) in lockers.iter_mut() {
                if *pid == owner {
                    *ended = true;
                }
            }
        }
    }

    /// The fast path of a new owner.
    pub(crate) fn join(&self, owner: i32) -> Arc<Fast> {
        let fast = Arc::new(Fast { owner, holds: Mutex::default() });
        self.fasts().push(Arc::clone(&fast));
        fast
    }

    /// The end of an owner: its fast path goes.
    pub(crate) fn leave(&self, fast: &Arc<Fast>) {
        self.fasts().retain(|other| !Arc::ptr_eq(other, fast));
    }

    /// `FastPathGrantRelationLock`: true when the weak lock of `tag` in `mode` goes on the fast path of the owner, because no owner holds or waits for a strong mode on a relation of its partition. False sends the request to [`Locks::request`].
    pub(crate) fn weak(&self, fast: &Fast, tag: Tag, mode: Mode) -> bool {
        let Some(part) = partition(tag).filter(|_| mode < STRONG) else { return false };
        let mut holds = fast.holds();
        // A strong request counts itself before it takes this lock to move the holds, so it sees this hold, or this load sees its count.
        if self.strong[part].load(Ordering::SeqCst) != 0 {
            return false;
        }
        match holds.iter_mut().find(|hold| hold.0 == tag && hold.1 == mode) {
            Some(hold) => hold.2 += 1,
            None => holds.push((tag, mode, 1)),
        }
        true
    }

    /// The end of the transaction of the owner of `fast`: its weak locks on the fast path go.
    pub(crate) fn release_fast(&self, fast: &Fast) {
        let mut holds = fast.holds();
        if !holds.is_empty() {
            holds.clear();
        }
    }

    /// `FastPathTransferRelationLocks`: the weak locks of `tag` on the fast paths of all the owners go to the table.
    fn transfer(&self, tag: Tag) {
        let fasts = self.fasts();
        for fast in fasts.iter() {
            let mut holds = fast.holds();
            if !holds.iter().any(|hold| hold.0 == tag) {
                continue;
            }
            let mut table = self.table();
            self.used.store(true, Ordering::Relaxed);
            holds.retain(|&(held, mode, count)| {
                if held != tag {
                    return true;
                }
                table.entry(tag).or_default().hold(fast.owner, mode).xact += count;
                false
            });
        }
    }

    /// The count of the partition of `tag` after a change of the table that took away the last strong hold or request of the tag.
    fn settle(&self, tag: Tag, before: bool, after: bool) {
        if before
            && !after
            && let Some(part) = partition(tag)
        {
            self.strong[part].fetch_sub(1, Ordering::SeqCst);
        }
    }

    /// `LockAcquire`: a hold that the owner has in this mode gets one more count. Otherwise the lock is free when no other owner holds or waits for a mode that conflicts. A lock that is not free gives [`Request::Busy`] without `wait`, and puts the request in the queue with `wait`, as `ProcSleep` does. A strong request on a relation first counts itself in its partition and moves the weak locks of the relation to the table.
    pub(crate) fn request(
        &self,
        tag: Tag,
        owner: i32,
        mode: Mode,
        session: bool,
        wait: bool,
    ) -> Request {
        let part = partition(tag).filter(|_| mode >= STRONG);
        if let Some(part) = part {
            self.strong[part].fetch_add(1, Ordering::SeqCst);
            self.transfer(tag);
        }
        let mut table = self.table();
        let before = part.is_some() && table.get(&tag).is_some_and(Lock::strong);
        let request = self.request_in(&mut table, tag, owner, mode, session, wait);
        // The count of the request stays when the tag has its first strong hold or request.
        if let Some(part) = part
            && (before || !table.get(&tag).is_some_and(Lock::strong))
        {
            self.strong[part].fetch_sub(1, Ordering::SeqCst);
        }
        request
    }

    /// [`Locks::request`] in the table.
    fn request_in(
        &self,
        table: &mut HashMap<Tag, Lock>,
        tag: Tag,
        owner: i32,
        mode: Mode,
        session: bool,
        wait: bool,
    ) -> Request {
        self.used.store(true, Ordering::Relaxed);
        let lock = table.entry(tag).or_default();
        if lock.holds.iter().any(|h| h.owner == owner && h.mode == mode) {
            lock.grant(owner, mode, session);
            return Request::Granted;
        }
        if lock.free_for(owner, mode, &lock.queue) {
            lock.grant(owner, mode, session);
            return Request::Granted;
        }
        // `JoinWaitQueue`: a session that holds a mode on the tag goes before the first request that waits for a mode that conflicts with it, and gets the lock there when nothing before it conflicts, also with `NOWAIT`.
        let held: Vec<Mode> =
            lock.holds.iter().filter(|h| h.owner == owner).map(|h| h.mode).collect();
        let mut at = lock.queue.len();
        let mut deadlock = None;
        for (i, other) in lock.queue.iter().enumerate() {
            if !held.iter().any(|&mine| conflicts(mine, other.mode)) {
                continue;
            }
            if lock.holds.iter().any(|h| h.owner == other.owner && conflicts(mode, h.mode)) {
                deadlock = Some(vec![
                    Edge { pid: owner, mode, tag },
                    Edge { pid: other.owner, mode: other.mode, tag },
                ]);
                break;
            }
            if lock.free_for(owner, mode, &lock.queue[..i]) {
                lock.grant(owner, mode, session);
                return Request::Granted;
            }
            at = i;
            break;
        }
        if !wait {
            if lock.holds.is_empty() && lock.queue.is_empty() {
                table.remove(&tag);
            }
            return Request::Busy;
        }
        if let Some(cycle) = deadlock {
            return Request::Deadlock(cycle);
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
            let before = lock.strong();
            lock.queue.retain(|wait| wait.owner != owner);
            self.settle(tag, before, lock.strong());
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

    /// `GetBlockerStatusData` for the request of `owner`: each other owner that holds a mode that conflicts with the mode of the request, and each other owner before it in the queue with a mode that conflicts.
    pub(crate) fn blockers(&self, owner: i32) -> Vec<i32> {
        let table = self.table();
        let mut pids = Vec::new();
        for lock in table.values() {
            let Some(at) = lock.waiting(owner) else { continue };
            let mode = lock.queue[at].mode;
            let holds = lock.holds.iter().map(|hold| (hold.owner, hold.mode));
            let ahead = lock.queue[..at].iter().map(|wait| (wait.owner, wait.mode));
            for (pid, held) in holds.chain(ahead) {
                if pid != owner && conflicts(mode, held) && !pids.contains(&pid) {
                    pids.push(pid);
                }
            }
        }
        drop(table);
        for pid in self.watched(owner) {
            if !pids.contains(&pid) {
                pids.push(pid);
            }
        }
        pids
    }

    /// `GetLockStatusData`: each hold on a fast path, and each hold and each request in a queue of the table, as the tag, the owner, the mode, true for a hold and true for a hold on a fast path.
    pub(crate) fn status(&self) -> Vec<(Tag, i32, Mode, bool, bool)> {
        let mut rows = Vec::new();
        for fast in self.fasts().iter() {
            let holds = fast.holds();
            rows.extend(holds.iter().map(|&(tag, mode, _)| (tag, fast.owner, mode, true, true)));
        }
        let table = self.table();
        for (tag, lock) in table.iter() {
            rows.extend(lock.holds.iter().map(|hold| (*tag, hold.owner, hold.mode, true, false)));
            rows.extend(lock.queue.iter().map(|wait| (*tag, wait.owner, wait.mode, false, false)));
        }
        rows
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
            let before = lock.strong();
            lock.holds.remove(at);
            self.settle(tag, before, lock.strong());
            if lock.holds.is_empty() && lock.queue.is_empty() {
                table.remove(&tag);
            }
            drop(table);
            self.changed.notify_all();
        }
        true
    }

    /// The counts of the holds of the transaction level of the owner, in the table and on its fast path, which a savepoint keeps for [`Locks::release_since`].
    pub(crate) fn xact_holds(&self, owner: i32, fast: Option<&Fast>) -> Vec<(Tag, Mode, u32)> {
        let mut holds = fast.map_or_else(Vec::new, |fast| fast.holds().clone());
        let table = self.table();
        for (tag, lock) in table.iter() {
            for hold in lock.holds.iter().filter(|h| h.owner == owner && h.xact > 0) {
                match holds.iter_mut().find(|k| k.0 == *tag && k.1 == hold.mode) {
                    Some(kept) => kept.2 += hold.xact,
                    None => holds.push((*tag, hold.mode, hold.xact)),
                }
            }
        }
        holds
    }

    /// `LockReleaseCurrentOwner` at the abort of a subtransaction: the holds of the transaction level of the owner, in the table and on its fast path, go back to the counts of `kept`, so the locks that the subtransaction took go.
    pub(crate) fn release_since(&self, owner: i32, fast: Option<&Fast>, kept: &[(Tag, Mode, u32)]) {
        let count = |tag: Tag, mode: Mode| {
            kept.iter().find(|k| k.0 == tag && k.1 == mode).map_or(0, |k| k.2)
        };
        if let Some(fast) = fast {
            fast.holds().retain_mut(|hold| {
                hold.2 = hold.2.min(count(hold.0, hold.1));
                hold.2 > 0
            });
        }
        let mut table = self.table();
        let mut freed = false;
        table.retain(|tag, lock| {
            let before = lock.strong();
            lock.holds.retain_mut(|hold| {
                if hold.owner != owner || hold.xact == 0 {
                    return true;
                }
                hold.xact = hold.xact.min(count(*tag, hold.mode));
                let kept = hold.session > 0 || hold.xact > 0;
                freed |= !kept;
                kept
            });
            self.settle(*tag, before, lock.strong());
            !lock.holds.is_empty() || !lock.queue.is_empty()
        });
        drop(table);
        if freed {
            self.changed.notify_all();
        }
    }

    /// `CheckForSessionAndXactLocks`: true when the owner holds a tag at the session level and at the transaction level, in any modes, which a prepared transaction cannot keep apart.
    pub(crate) fn holds_both_levels(&self, owner: i32) -> bool {
        self.table().values().any(|lock| {
            let mine = || lock.holds.iter().filter(|h| h.owner == owner);
            mine().any(|h| h.session > 0) && mine().any(|h| h.xact > 0)
        })
    }

    /// `PostPrepare_Locks`: the holds of the transaction level of `owner`, in the table and on its fast path, go to the owner `to` of a prepared transaction, which keeps them in the table until it ends.
    pub(crate) fn hand_over(&self, owner: i32, fast: Option<&Fast>, to: i32) {
        let moved = fast.map_or_else(Vec::new, |fast| std::mem::take(&mut *fast.holds()));
        let mut table = self.table();
        let mut held = Vec::new();
        table.retain(|tag, lock| {
            lock.holds.retain_mut(|hold| {
                if hold.owner != owner || hold.xact == 0 {
                    return true;
                }
                held.push((*tag, hold.mode, std::mem::take(&mut hold.xact)));
                hold.session > 0
            });
            !lock.holds.is_empty() || !lock.queue.is_empty()
        });
        for (tag, mode, count) in moved.into_iter().chain(held) {
            table.entry(tag).or_default().hold(to, mode).xact += count;
        }
        if !table.is_empty() {
            self.used.store(true, Ordering::Relaxed);
        }
    }

    /// `LockReleaseAll` for one level: releases all the holds of the owner at the session level or at the transaction level.
    pub(crate) fn release_all(&self, owner: i32, session: bool) {
        self.release_holds(owner, session);
        if !session {
            self.xact_ended(owner);
        }
    }

    /// The release of the holds of `owner` at one level for [`Locks::release_all`].
    fn release_holds(&self, owner: i32, session: bool) {
        // The owner set the flag itself before it got any hold, so a clear flag means that it holds nothing.
        if !self.used.load(Ordering::Relaxed) {
            return;
        }
        let mut table = self.table();
        let mut freed = false;
        table.retain(|tag, lock| {
            let before = lock.strong();
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
            self.settle(*tag, before, lock.strong());
            !lock.holds.is_empty() || !lock.queue.is_empty()
        });
        if table.is_empty() {
            self.used.store(false, Ordering::Relaxed);
        }
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
        let kept = locks.xact_holds(1, None);
        assert_eq!(locks.request(tag(1), 1, EXCLUSIVE, false, false), Request::Granted);
        assert_eq!(locks.request(tag(2), 1, EXCLUSIVE, false, false), Request::Granted);
        locks.release_since(1, None, &kept);
        assert_eq!(locks.xact_holds(1, None), kept);
        assert_eq!(locks.request(tag(2), 2, EXCLUSIVE, true, false), Request::Granted);
        assert_eq!(locks.request(tag(1), 2, EXCLUSIVE, true, false), Request::Busy);
        locks.release_all(1, false);
        assert!(locks.release(tag(1), 1, EXCLUSIVE));
        assert_eq!(locks.request(tag(1), 2, EXCLUSIVE, true, false), Request::Granted);
    }

    #[test]
    fn fast_path() {
        let locks = Locks::default();
        let rel = Tag::Relation { database: 5, relation: 16384 };
        let (one, two) = (locks.join(1), locks.join(2));
        assert!(locks.weak(&one, rel, 1));
        assert!(locks.weak(&one, rel, 1));
        assert!(!locks.weak(&one, tag(1), 1));
        assert!(locks.table().is_empty());
        assert_eq!(locks.status(), [(rel, 1, 1, true, true)]);
        // A strong request moves the weak lock to the table, where it conflicts.
        assert_eq!(locks.request(rel, 2, 8, false, false), Request::Busy);
        assert!(one.holds().is_empty());
        assert_eq!(locks.xact_holds(1, Some(&one)), [(rel, 1, 2)]);
        assert_eq!(locks.request(rel, 2, 8, false, true), Request::Queued);
        // While a strong request waits, a weak lock does not take the fast path.
        assert!(!locks.weak(&two, Tag::Relation { database: 5, relation: 16384 + 1024 }, 1));
        locks.release_fast(&one);
        locks.release_all(1, false);
        assert!(locks.wait(rel, 2, false, Duration::ZERO, None));
        locks.release_all(2, false);
        assert!(locks.weak(&two, rel, 3));
        let kept = locks.xact_holds(2, Some(&two));
        assert!(locks.weak(&two, rel, 3));
        assert!(locks.weak(&two, rel, 1));
        locks.release_since(2, Some(&two), &kept);
        assert_eq!(locks.xact_holds(2, Some(&two)), [(rel, 3, 1)]);
        locks.leave(&one);
        assert_eq!(locks.status().len(), 1);
    }

    #[test]
    fn a_holder_goes_before_a_request_that_waits_for_it_also_with_nowait() {
        let locks = Locks::default();
        assert_eq!(locks.request(tag(1), 1, 8, true, true), Request::Granted);
        assert_eq!(locks.request(tag(1), 2, EXCLUSIVE, true, true), Request::Queued);
        // `LOCK TABLE ... IN SHARE ROW EXCLUSIVE MODE NOWAIT` of the holder conflicts only with the queued request.
        assert_eq!(locks.request(tag(1), 1, 6, true, false), Request::Granted);
        // A session that holds nothing waits behind the queue.
        assert_eq!(locks.request(tag(1), 3, 1, true, false), Request::Busy);
    }

    #[test]
    fn queue_order() {
        let locks = Locks::default();
        assert_eq!(locks.request(tag(1), 1, SHARE, true, true), Request::Granted);
        assert_eq!(locks.request(tag(1), 2, EXCLUSIVE, true, true), Request::Queued);
        // A share request waits behind the exclusive request.
        assert_eq!(locks.request(tag(1), 3, SHARE, true, false), Request::Busy);
        assert_eq!(locks.request(tag(1), 3, SHARE, true, true), Request::Queued);
        // The share request waits for no hold, but for the exclusive request before it.
        assert_eq!(locks.blockers(1), [] as [i32; 0]);
        assert_eq!(locks.blockers(2), [1]);
        assert_eq!(locks.blockers(3), [2]);
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
