//! The rows of the tables in memory, with the transaction IDs of `heapam_visibility.c`. Each version of a row has the transaction and the command that made it, and the transaction and the command that deleted it. A snapshot tells which versions a statement sees.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Mutex, MutexGuard, PoisonError};

use rupg_analyze::{Derived, Unique};
use rupg_common::{Error, Result, SqlState};
use rupg_func::{HeapSize, Statistic, XactState};
use rupg_types::{Value, oid};

pub(crate) use btree::{Shape, Size as BtreeSize};
pub(crate) use pages::{Fixed, Skip, Storage, datum_width, stored_widths};
use pages::{Pages, Pruned, generation_of, max_align, page_of, slot_of, toast};

mod btree;
mod fsm;
mod pages;

/// The key of a row in a unique index with an expression or a predicate, or `None` for a row that is not in the index, as [`rupg_exec::index_key`] gives it. A function in an expression can read and change the heap, so the heap calls it only when it is not locked.
pub(crate) type Derive<'a> = &'a dyn Fn(&Derived, &[Value]) -> Result<Option<Vec<Value>>>;

/// The keys of a new row in the unique indexes with an expression or a predicate, by the place of the index in the unique keys of the write. The place of an index with no expression, or of an index that does not have the row, has `None`. The list is empty when no index has an expression.
type Fresh = Vec<DerivedKey>;

/// The key of a row in an index with an expression or a predicate, or `None` when the index does not have the row.
type DerivedKey = Option<Box<[Value]>>;

/// The keys of the new row `values` in the indexes of `unique` with an expression or a predicate. The expressions run before the write locks the heap.
fn fresh(unique: &[Unique], values: &[Value], derive: Derive<'_>) -> Result<Fresh> {
    if unique.iter().all(|key| key.derived.is_none()) {
        return Ok(Vec::new());
    }
    let mut keys = Vec::with_capacity(unique.len());
    for key in unique {
        keys.push(match &key.derived {
            Some(derived) => derive(derived, values)?.map(Vec::into_boxed_slice),
            None => None,
        });
    }
    Ok(keys)
}

/// `nextXid` of a new cluster: `initdb` of PostgreSQL 19 uses the transaction IDs from `FirstNormalTransactionId` up to 661, so the first transaction gets 662.
const INITDB_NEXT_XID: u32 = 662;

/// The rows of the tables of all the databases, and the transactions that are in progress.
#[derive(Debug, Default)]
pub(crate) struct Heap {
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    /// `nextXid`: the transaction ID of the next transaction that writes.
    next_xid: u32,
    /// The transactions that are in progress, `ProcArray`.
    running: BTreeSet<u32>,
    /// `latestCompletedXid`: the last transaction that committed or aborted, or 0.
    latest_completed: u32,
    /// The transactions that aborted, the aborted entries of `pg_xact`. A transaction before `next_xid` that is not in progress and did not abort committed.
    aborted: BTreeSet<u32>,
    /// The process ID of the session of each transaction that is in progress.
    owners: HashMap<u32, i32>,
    /// `PGPROC->xmin`: the `xmin` of the oldest snapshot that each session holds, by the process ID.
    xmins: HashMap<i32, u32>,
    /// The versions of the rows of each table, by the OID of the database and the OID of the table.
    tables: HashMap<(u32, u32), Table>,
    /// The tables that each transaction in progress truncated, whose versions from before the `TRUNCATE` go at the commit.
    truncating: HashMap<u32, Vec<(u32, u32)>>,
    /// The transaction that each waiting transaction waits for, the edges of the wait graph of `DeadLockCheck`.
    waits: HashMap<u32, u32>,
    /// The new versions of the transactions in progress whose keys a deferrable unique index must check again.
    rechecks: Vec<Recheck>,
    /// The number of the values that the writes moved to the TOAST tables, whose OIDs the counter did not give yet.
    toast_oids: u32,
}

/// A new version with the key of a live version, or of a version of a transaction in progress, in a deferrable unique index, as `UNIQUE_CHECK_PARTIAL` finds it. The key is checked again after the statement or at the commit, as the event of `unique_key_recheck` does it.
#[derive(Clone, Debug)]
pub(crate) struct Recheck {
    xid: u32,
    /// The command that made the version.
    command: u32,
    database: u32,
    /// The OID of the table.
    pub(crate) table: u32,
    /// The index whose key the version has.
    pub(crate) unique: Unique,
    /// The ID of the version.
    pub(crate) tid: u64,
}

/// The rechecks of the new versions `partial` of the command of `view` in the table `relation`, by the place of the index in `unique` and the ID of the version.
fn rechecks<'a>(
    view: &'a View,
    relation: u32,
    unique: &'a [Unique],
    partial: Vec<(usize, u64)>,
) -> impl Iterator<Item = Recheck> + 'a {
    partial.into_iter().filter_map(move |(index, tid)| {
        Some(Recheck {
            xid: view.xid,
            command: view.command,
            database: view.database,
            table: relation,
            unique: unique.get(index)?.clone(),
            tid,
        })
    })
}

/// An edge of a cycle of waits: the process that waits, the transaction that it waits for, and the process of that transaction.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Wait {
    pub(crate) pid: i32,
    pub(crate) xid: u32,
    pub(crate) holder: i32,
}

/// The versions of the rows of a table, in the order of their IDs.
#[derive(Debug, Default)]
struct Table {
    versions: Vec<Version>,
    /// The number of versions that the table got, which gives each version its place in the order of the inserts.
    made: u64,
    /// The map of each unique key that a write checked. A map has the IDs of all the versions, live or not, with a key that has no null.
    keys: Vec<KeyMap>,
    pages: Pages,
    /// The transaction and the command of the statement that fills the table with a `BulkInsertState`, as `CREATE TABLE AS` does.
    bulk: Option<(u32, u32)>,
    /// The pages of the heap before each `TRUNCATE` of the transaction in progress, with the transaction and the command, which a rollback brings back, as `RelationSetNewRelfilenumber` gives the table a new file.
    truncated: Vec<(u32, u32, Pages)>,
    /// The rows of `pg_statistic` of the table, by `staattnum` and `stainherit`, which `ANALYZE` writes outside of the transactions.
    statistics: BTreeMap<(i16, bool), Statistic>,
    /// The size of each `btree` index of the table by the OID of the index, from the versions that the table had.
    btree_sizes: BTreeMap<u32, btree::Cached>,
    /// The storage of each column with a type of fixed length, which the new versions take in their pages.
    fixed: Vec<Fixed>,
    /// `attstorage` of each column, with the method that compresses its values.
    storage: Vec<Storage>,
}

/// The versions of a table by the hash of their key, for one unique key, which takes the place of the btree of a unique index. The map can keep the ID of a version that went, which a check skips.
#[derive(Debug)]
struct KeyMap {
    columns: Vec<usize>,
    types: Vec<u32>,
    nulls_not_distinct: bool,
    /// The expressions and the predicate of the index. `columns` are then the places in the values of the expressions.
    derived: Option<Box<Derived>>,
    hashes: HashMap<u64, Vec<u64>>,
    /// The key of each version in an index with an expression or a predicate, by the ID of the version. The expressions ran before the heap was locked.
    derived_keys: HashMap<u64, Box<[Value]>>,
    /// The map of an index with an expression did not get the key of a new version, because the write did not have the index. A later write with the index makes the map again.
    stale: bool,
}

impl KeyMap {
    fn of(key: &Unique) -> Self {
        KeyMap {
            columns: key.columns.clone(),
            types: key.types.clone(),
            nulls_not_distinct: key.nulls_not_distinct,
            derived: key.derived.clone(),
            hashes: HashMap::new(),
            derived_keys: HashMap::new(),
            stale: false,
        }
    }

    fn is(&self, key: &Unique) -> bool {
        !self.stale
            && self.columns == key.columns
            && self.types == key.types
            && self.nulls_not_distinct == key.nulls_not_distinct
            && self.derived == key.derived
    }

    /// The values that the key columns are places in for the version `tid` with the values `values`: the row itself, or the values of the expressions of the index. `None` is a row that is not in the index.
    fn key<'v>(&'v self, tid: u64, values: &'v [Value]) -> Option<&'v [Value]> {
        match &self.derived {
            None => Some(values),
            Some(_) => self.derived_keys.get(&tid).map(|key| &**key),
        }
    }

    /// Adds the version `tid` with the values `values` to the map. For an index with an expression or a predicate, `derived` is the key of the version, or `None` when the index does not have the version.
    fn add(&mut self, tid: u64, values: &[Value], derived: DerivedKey) {
        let hash = if self.derived.is_none() {
            self.hash(values)
        } else if let Some(key) = derived {
            let hash = self.hash(&key);
            self.derived_keys.insert(tid, key);
            hash
        } else {
            self.derived_keys.remove(&tid);
            None
        };
        if let Some(hash) = hash {
            self.hashes.entry(hash).or_default().push(tid);
        }
    }

    /// The map of an index with an expression stops, as it misses the key of a new version.
    fn make_stale(&mut self) {
        self.stale = true;
        self.hashes = HashMap::new();
        self.derived_keys = HashMap::new();
    }

    /// The hash of the key of a row, or `None` when the key has a null and two nulls are not equal, so the key cannot conflict. Two keys that are equal have the same hash.
    fn hash(&self, values: &[Value]) -> Option<u64> {
        let mut hasher = DefaultHasher::new();
        for (&column, &ty) in self.columns.iter().zip(&self.types) {
            match values.get(column)? {
                Value::Null if self.nulls_not_distinct => 0_u8.hash(&mut hasher),
                Value::Null => return None,
                value => hash_value(value, ty, &mut hasher),
            }
        }
        Some(hasher.finish())
    }

    /// True when two rows have the same key, with the equality of the btree operator class of each column.
    fn same(&self, a: &[Value], b: &[Value]) -> Result<bool> {
        for (&column, &ty) in self.columns.iter().zip(&self.types) {
            let equal = match (&a[column], &b[column]) {
                (Value::Null, Value::Null) => self.nulls_not_distinct,
                (Value::Null, _) | (_, Value::Null) => false,
                (x, y) => rupg_func::equal(ty, x, ty, y)?,
            };
            if !equal {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

/// Adds a value that is not null to a hash. The types whose equality is not the equality of their form in memory all go to the same hash.
pub(crate) fn hash_value(value: &Value, ty: u32, hasher: &mut DefaultHasher) {
    match value {
        Value::Bool(v) => v.hash(hasher),
        Value::Int2(_) | Value::Int4(_) | Value::Int8(_) => value.as_i64().hash(hasher),
        Value::Float4(_) | Value::Float8(_) => {
            let x = match value {
                Value::Float4(x) => f64::from(*x),
                Value::Float8(x) => *x,
                _ => 0.0,
            };
            // `float8_cmp_internal`: -0 is equal to 0, and NaN is equal to NaN.
            let x = if x.is_nan() {
                f64::NAN
            } else if x == 0.0 {
                0.0
            } else {
                x
            };
            x.to_bits().hash(hasher);
        }
        Value::Oid(v) => v.hash(hasher),
        Value::Char(v) => v.hash(hasher),
        Value::Text(v) if rupg_func::base_type(ty) == oid::BPCHAR => {
            v.trim_end_matches(' ').hash(hasher);
        }
        Value::Text(v) => v.hash(hasher),
        Value::Bytea(v) => v.hash(hasher),
        Value::Uuid(v) => v.hash(hasher),
        Value::Date(v) => v.hash(hasher),
        Value::Time(v) | Value::Timestamp(v) | Value::TimestampTz(v) => v.hash(hasher),
        _ => {}
    }
}

/// What a dirty snapshot sees of a version with the same key, as `_bt_check_unique` reads it with `SnapshotDirty`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Seen {
    /// The version went, or the transaction deleted it.
    Dead,
    /// The version is live: the key is a duplicate.
    Live,
    /// A transaction that is in progress inserted or deleted the version.
    Wait(u32),
}

/// `HeapTupleSatisfiesDirty` for the transaction `xid`. An aborted transaction left no versions, so each `xmin` committed or is in progress.
fn dirty(version: &Version, xid: u32, running: &BTreeSet<u32>) -> Seen {
    if version.xmin != xid && running.contains(&version.xmin) {
        return Seen::Wait(version.xmin);
    }
    match version.xmax {
        0 => Seen::Live,
        xmax if xmax == xid => Seen::Dead,
        xmax if running.contains(&xmax) => Seen::Wait(xmax),
        _ => Seen::Dead,
    }
}

impl Table {
    /// Adds a version with the ID `kept` that an earlier try of the write had, or with a new ID, at its place in the order of the IDs. The version goes in the map of each unique key. `fresh` has the keys of the version in the indexes of `unique` with an expression. The map of an index with an expression that is not in `unique` becomes stale.
    fn add(
        &mut self,
        kept: Option<u64>,
        (xmin, cmin): (u32, u32),
        values: Vec<Value>,
        (unique, fresh): (&[Unique], &[DerivedKey]),
        old: Option<u64>,
        moved: &mut u32,
    ) -> u64 {
        let prior = old.and_then(|old| self.find(old)).map(|at| &*self.versions[at].values);
        let toasted = toast(&values, &self.fixed, &self.storage, prior);
        // A write that waited makes its versions again in the place that the first try took. The first try took the OIDs of the TOAST values.
        if kept.is_none() {
            *moved += toasted.moved;
        }
        let len = max_align(toasted.len);
        let tid = kept.unwrap_or_else(|| {
            let bulk = self.bulk.filter(|&made| made == (xmin, cmin));
            let (page, slot) = self.pages.place(len, old.map(page_of), bulk);
            pages::tid(self.pages.generation, page, slot)
        });
        self.made += 1;
        for map in &mut self.keys {
            if map.stale {
                continue;
            }
            if map.derived.is_none() {
                map.add(tid, &values, None);
            } else if let Some(place) = unique.iter().position(|key| map.is(key)) {
                map.add(tid, &values, fresh.get(place).cloned().flatten());
            } else {
                map.make_stale();
            }
        }
        let values = values.into_boxed_slice();
        let len = u16::try_from(len).unwrap_or(u16::MAX);
        let made = self.made;
        let version =
            Version { tid, made, xmin, cmin, xmax: 0, cmax: 0, aborted: 0, next: 0, len, values };
        if self.versions.last().is_none_or(|last| last.tid < tid) {
            self.versions.push(version);
        } else {
            let at = self.versions.partition_point(|other| other.tid < tid);
            self.versions.insert(at, version);
        }
        tid
    }

    /// The place of the version `tid`.
    fn find(&self, tid: u64) -> Option<usize> {
        self.versions.binary_search_by_key(&tid, |version| version.tid).ok()
    }

    /// The place in `keys` of the map of each unique key. The first write that checks a key with no expression makes its map from all the versions. The result is `None` when an index with an expression has no map, which [`Heap::lock_keys`] makes while the heap is not locked.
    fn key_maps(&mut self, unique: &[Unique]) -> Option<Vec<usize>> {
        self.keys.retain(|map| !map.stale);
        let mut places = Vec::with_capacity(unique.len());
        for key in unique {
            let place = match self.keys.iter().position(|map| map.is(key)) {
                Some(place) => place,
                None if key.derived.is_some() => return None,
                None => {
                    let mut map = KeyMap::of(key);
                    for version in &self.versions {
                        map.add(version.tid, &version.values, None);
                    }
                    self.keys.push(map);
                    self.keys.len() - 1
                }
            };
            places.push(place);
        }
        Some(places)
    }

    /// `_bt_check_unique` of the new version `tid` for each unique key, in the order of the indexes. The result is the first conflict: a live version with the same key, or a transaction in progress that the write must wait for. `used` has the IDs of the new versions of the write up to `tid`. A conflict in a deferrable index is no error: the place of the index and `tid` go to `partial`, for the check after the statement.
    fn check_keys(
        &self,
        (places, unique): (&[usize], &[Unique]),
        used: &[u64],
        (xid, running): (u32, &BTreeSet<u32>),
        partial: &mut Vec<(usize, u64)>,
    ) -> Result<Option<Changed>> {
        let Some(&tid) = used.last() else {
            return Ok(None);
        };
        let Some(at) = self.find(tid) else {
            return Ok(None);
        };
        let row = &self.versions[at].values;
        for (index, &place) in places.iter().enumerate() {
            let map = &self.keys[place];
            let Some(values) = map.key(tid, row) else {
                continue;
            };
            let Some(hash) = map.hash(values) else {
                continue;
            };
            for &other in map.hashes.get(&hash).into_iter().flatten() {
                if other == tid {
                    continue;
                }
                let Some(found) = self.find(other) else {
                    continue;
                };
                let version = &self.versions[found];
                let seen = dirty(version, xid, running);
                if seen == Seen::Dead {
                    continue;
                }
                match map.key(other, &version.values) {
                    Some(other) if map.same(values, other)? => {}
                    _ => continue,
                }
                if unique.get(index).is_some_and(|key| key.deferrable.is_some()) {
                    partial.push((index, tid));
                    break;
                }
                return Ok(Some(match seen {
                    Seen::Wait(holder) => Changed::WaitKey { holder, index, used: used.to_vec() },
                    _ => Changed::Duplicate {
                        index,
                        key: map.columns.iter().map(|&column| values[column].clone()).collect(),
                    },
                }));
            }
        }
        Ok(None)
    }

    /// A transaction other than `xid` that is in progress and inserted or deleted a version of the table.
    fn holder(&self, xid: u32, running: &BTreeSet<u32>) -> Option<u32> {
        let other = |x: u32| x != 0 && x != xid && running.contains(&x);
        self.versions.iter().find_map(|v| [v.xmin, v.xmax].into_iter().find(|&x| other(x)))
    }

    /// The first version that is not dead with the key of a version before it in the map `map` of the unique index at the place `index`, as [`Heap::check_build`] gives it.
    fn first_duplicate(
        &self,
        index: usize,
        map: &KeyMap,
        (xid, running): (u32, &BTreeSet<u32>),
    ) -> Result<Option<Changed>> {
        for version in &self.versions {
            let seen = dirty(version, xid, running);
            if seen == Seen::Dead {
                continue;
            }
            let Some(values) = map.key(version.tid, &version.values) else {
                continue;
            };
            let Some(hash) = map.hash(values) else {
                continue;
            };
            for &other in map.hashes.get(&hash).into_iter().flatten() {
                if other >= version.tid {
                    continue;
                }
                let Some(found) = self.find(other) else {
                    continue;
                };
                let before = &self.versions[found];
                let seen_before = dirty(before, xid, running);
                if seen_before == Seen::Dead {
                    continue;
                }
                match map.key(other, &before.values) {
                    Some(other) if map.same(values, other)? => {}
                    _ => continue,
                }
                return Ok(Some(match (seen_before, seen) {
                    (Seen::Wait(holder), _) | (_, Seen::Wait(holder)) => {
                        Changed::WaitKey { holder, index, used: vec![version.tid] }
                    }
                    _ => Changed::Duplicate {
                        index,
                        key: map.columns.iter().map(|&column| values[column].clone()).collect(),
                    },
                }));
            }
        }
        Ok(None)
    }

    /// Takes back the new versions `used` and the deletes of the versions `deleted`, after a write that must wait or that failed. The IDs stay used: after a wait the write makes its versions again with the same IDs, as the versions of PostgreSQL stay in place while it waits, and after a failure the versions of PostgreSQL stay in the heap until the end of the transaction.
    fn undo(&mut self, used: &[u64], deleted: &[u64]) {
        let mut used = used.to_vec();
        used.sort_unstable();
        let used = used.as_slice();
        if let Some(&first) = used.first() {
            let at = self.versions.partition_point(|version| version.tid < first);
            let tail = self.versions.split_off(at);
            self.versions.extend(tail.into_iter().filter(|v| used.binary_search(&v.tid).is_err()));
            self.forget_keys(used);
        }
        for &tid in deleted {
            if let Some(at) = self.find(tid) {
                let version = &mut self.versions[at];
                version.xmax = 0;
                version.cmax = 0;
                version.next = 0;
            }
        }
    }

    /// Removes the versions that `gone` picks.
    fn remove(&mut self, gone: impl Fn(&Version) -> bool) {
        if self.keys.iter().any(|map| !map.derived_keys.is_empty()) {
            let tids: Vec<u64> = self.versions.iter().filter(|v| gone(v)).map(|v| v.tid).collect();
            self.forget_keys(&tids);
        }
        self.versions.retain(|version| !gone(version));
    }

    /// Removes the versions `gone`, in the order of their IDs, that `VACUUM` found dead, also from the maps of the unique keys, as the IDs of their slots can come back.
    fn purge(&mut self, gone: &[u64]) {
        if gone.is_empty() {
            return;
        }
        self.versions.retain(|version| gone.binary_search(&version.tid).is_err());
        self.forget_keys(gone);
        for map in &mut self.keys {
            map.hashes.retain(|_, tids| {
                tids.retain(|tid| gone.binary_search(tid).is_err());
                !tids.is_empty()
            });
        }
    }

    /// Removes the keys of the versions `tids` that went from the maps of the indexes with an expression.
    fn forget_keys(&mut self, tids: &[u64]) {
        for map in self.keys.iter_mut().filter(|map| !map.derived_keys.is_empty()) {
            for tid in tids {
                map.derived_keys.remove(tid);
            }
        }
    }
}

/// One version of a row, `HeapTupleHeaderData`.
#[derive(Debug)]
struct Version {
    /// The ID of the version in its table, its item pointer with the generation of the heap of [`pages::tid`]. It does not change while the version lives.
    tid: u64,
    /// The place of the version in the order of the inserts into the table.
    made: u64,
    /// The transaction that inserted the version.
    xmin: u32,
    /// The command of `xmin` that inserted the version.
    cmin: u32,
    /// The transaction that deleted the version, or 0.
    xmax: u32,
    /// The command of `xmax` that deleted the version.
    cmax: u32,
    /// The transaction that deleted the version and then aborted, or 0. The column `xmax` of the version shows it while no other transaction deletes the version, as `t_xmax` keeps the ID of an aborted transaction.
    aborted: u32,
    /// `t_ctid`: the ID of the version that an `UPDATE` made from this version, or 0.
    next: u64,
    /// `MAXALIGN(t_len)`: the space of the version in its page.
    len: u16,
    values: Box<[Value]>,
}

/// `SnapshotData` of an MVCC snapshot: the transactions whose changes a statement does not see.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Snapshot {
    /// All the transactions from this ID are not visible.
    xmax: u32,
    /// The transactions before `xmax` that were in progress.
    running: BTreeSet<u32>,
    /// `latestCompletedXid + 1`, the `xmax` of the snapshot of `GetSnapshotData`. No transaction from this ID on ended, so the visibility is the same with this ID as with `xmax`.
    completed: u32,
}

impl Snapshot {
    /// The `xmin` of `GetSnapshotData`: the first transaction in progress before `latestCompletedXid + 1`, also when it is the transaction of the session, or `latestCompletedXid + 1` when there is none.
    pub(crate) fn xmin(&self) -> u32 {
        self.running.range(..self.completed).next().copied().unwrap_or(self.completed)
    }

    /// The snapshot as a `pg_snapshot`, as `GetSnapshotData` makes it for the session whose transaction is `own`: `latestCompletedXid + 1` as `xmax`, the transactions in progress before it without `own`, and [`Snapshot::xmin`] as `xmin`.
    pub(crate) fn exported(&self, own: u32) -> rupg_types::Snapshot {
        let xip: Vec<u64> = self
            .running
            .range(..self.completed)
            .filter(|&&xid| xid != own)
            .map(|&xid| u64::from(xid))
            .collect();
        rupg_types::Snapshot { xmin: u64::from(self.xmin()), xmax: u64::from(self.completed), xip }
    }
}

/// What a statement sees of the rows: its snapshot, and the transaction and the command that run it.
#[derive(Clone, Debug, Default)]
pub(crate) struct View {
    pub(crate) database: u32,
    pub(crate) snapshot: Snapshot,
    /// The transaction ID of the transaction, or 0 when the transaction did not write.
    pub(crate) xid: u32,
    /// `currentCommandId`.
    pub(crate) command: u32,
    /// True in `REPEATABLE READ` and `SERIALIZABLE`, where a row that another transaction changed after the snapshot is an error.
    pub(crate) serializable: bool,
}

/// A change of a row by `UPDATE` or `DELETE`: the ID of the version, and the new values, or `None` to delete it.
pub(crate) type Change = (u64, Option<Vec<Value>>);

/// The result of [`Heap::insert`] and [`Heap::change`].
#[derive(Debug, PartialEq)]
pub(crate) enum Changed {
    /// The number of rows that changed.
    Done(u64),
    /// The transaction `holder` is in progress and changed the version `tid`, so the statement must wait for the end of `holder` and try again.
    Wait { holder: u32, tid: u64 },
    /// In `READ COMMITTED`, a transaction that committed after the snapshot changed a row. The statement must run again with a new snapshot, which sees the last version of the row, as `EvalPlanQual` checks it.
    Again,
    /// The transaction `holder` is in progress and inserted or deleted a version with the same key in the unique index at the place `index` of the list of the write. The write must wait for the end of `holder` and try again with the IDs `used` of its new versions, of which the last is the version that waits.
    WaitKey { holder: u32, index: usize, used: Vec<u64> },
    /// A live version has the key of a new version in the unique index at the place `index`. `key` is the value of each key column.
    Duplicate { index: usize, key: Vec<Value> },
    /// `TM_SelfModified` with a `cmax` that is not the command of the view: a later command of the transaction, such as a statement of a trigger, changed the row. `update` is true when the change of the row is an update.
    SelfModified { update: bool },
}

/// The result of [`Heap::speculative`] for one row of `INSERT ... ON CONFLICT`.
#[derive(Debug, PartialEq)]
pub(crate) enum Speculative {
    /// The row went in, as the version with this ID.
    Inserted(u64),
    /// A live version has the key of the row in an arbiter index: the ID of the version and its values. `own` is true when the transaction of the view made the version, `command` is true when the command of the view made it, and `visible` is true when the snapshot of the view sees it.
    Conflict { tid: u64, values: Vec<Value>, own: bool, command: bool, visible: bool },
    /// The transaction `holder` is in progress and inserted or deleted the version `tid` with the key of the row in an arbiter index. The statement must wait for its end and check the row again.
    Wait { holder: u32, tid: u64 },
    /// A conflict in a unique index that is not an arbiter, as [`Heap::insert`] gives it.
    Key(Changed),
}

/// A version that an exclusion constraint checks, from [`Heap::exclusion_rows`].
#[derive(Clone, Debug)]
pub(crate) struct Candidate {
    pub(crate) tid: u64,
    /// The place of the version in the order of the inserts.
    pub(crate) made: u64,
    /// True when the commands that the check is for made the version.
    pub(crate) new: bool,
    /// The transaction in progress that inserted or deleted the version.
    pub(crate) holder: Option<u32>,
    /// True when the version is live for a dirty snapshot.
    pub(crate) live: bool,
    /// The place in the order of the inserts of the version that an `UPDATE` of the commands made from this version, or 0. The version was live until then.
    pub(crate) replaced: u64,
    pub(crate) values: Vec<Value>,
}

/// The text of the item pointer of a version, `(page,item)`, as `tidout` gives it.
pub(crate) fn tid_text(tid: u64) -> String {
    format!("({},{})", page_of(tid), slot_of(tid))
}

impl Heap {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Locks the heap when the table `table` has the map of each unique key of `unique`, and gives the place of each map. The map of an index with an expression or a predicate gets the keys of the versions while the heap is not locked, as a function in an expression can read and change the heap. The versions that other writes add meanwhile get their keys in the next round.
    ///
    /// # Errors
    ///
    /// The errors of the expressions of an index.
    fn lock_keys(
        &self,
        table: (u32, u32),
        unique: &[Unique],
        derive: Derive<'_>,
    ) -> Result<(MutexGuard<'_, State>, Vec<usize>)> {
        // The maps that this call makes, by the place of the key in `unique`, with the address of the values of each version whose key the map has, as an abort and a new try of a write can give a new version the ID of an old one.
        let mut made: Vec<(usize, KeyMap, HashMap<u64, usize>)> = Vec::new();
        loop {
            let mut state = self.state();
            let rows = state.tables.entry(table).or_default();
            if let Some(places) = rows.key_maps(unique) {
                return Ok((state, places));
            }
            let mut todo = Vec::new();
            for (place, key) in unique.iter().enumerate() {
                if key.derived.is_none() || rows.keys.iter().any(|map| map.is(key)) {
                    continue;
                }
                let at = match made.iter().position(|(other, ..)| *other == place) {
                    Some(at) => at,
                    None => {
                        made.push((place, KeyMap::of(key), HashMap::new()));
                        made.len() - 1
                    }
                };
                let seen = &made[at].2;
                let versions: Vec<_> = rows
                    .versions
                    .iter()
                    .filter(|v| seen.get(&v.tid) != Some(&(v.values.as_ptr() as usize)))
                    .map(|v| (v.tid, v.values.as_ptr() as usize, v.values.clone(), v.xmax != 0))
                    .collect();
                if versions.is_empty() {
                    let (_, map, _) = made.swap_remove(at);
                    rows.keys.push(map);
                } else {
                    todo.push((place, versions));
                }
            }
            if todo.is_empty() {
                continue;
            }
            drop(state);
            for (place, versions) in todo {
                let Some((_, map, seen)) = made.iter_mut().find(|(other, ..)| *other == place)
                else {
                    continue;
                };
                let Some(derived) = map.derived.clone() else { continue };
                for (tid, address, values, deleted) in versions {
                    // A version that a rewrite of the table deleted has the values of the old types of the columns, which the expressions of the new index can not take. The version is not in the index.
                    let key = match derive(&derived, &values) {
                        Err(_) if deleted => None,
                        key => key?.map(Vec::into_boxed_slice),
                    };
                    map.add(tid, &values, key);
                    seen.insert(tid, address);
                }
            }
        }
    }

    /// `GetNewTransactionId`: a transaction ID for a transaction of the session `pid` that writes. The transaction is in progress until [`Heap::commit`] or [`Heap::abort`].
    pub(crate) fn begin(&self, pid: i32) -> u32 {
        let mut state = self.state();
        let xid = state.next_xid.max(INITDB_NEXT_XID);
        state.next_xid = xid + 1;
        state.running.insert(xid);
        state.owners.insert(xid, pid);
        xid
    }

    /// `ReadNextFullTransactionId`: the transaction ID that the next transaction that writes gets.
    pub(crate) fn next_xid(&self) -> u32 {
        self.state().next_xid.max(INITDB_NEXT_XID)
    }

    /// Sets the `xmin` of the session `pid` to the `xmin` of `snapshot` when the session holds no snapshot, as `GetSnapshotData` sets `MyProc->xmin`.
    pub(crate) fn hold(&self, pid: i32, snapshot: &Snapshot) {
        self.state().xmins.entry(pid).or_insert_with(|| snapshot.xmin());
    }

    /// The session `pid` holds no snapshot, as `SnapshotResetXmin` clears `MyProc->xmin`.
    pub(crate) fn release(&self, pid: i32) {
        self.state().xmins.remove(&pid);
    }

    /// `ProcNumberGetTransactionIds`: the transaction ID of the session `pid` and its `xmin`, with 0 for none.
    pub(crate) fn transaction_ids(&self, pid: i32) -> (u32, u32) {
        let state = self.state();
        let xid = state.owners.iter().find(|&(_, &owner)| owner == pid).map_or(0, |(&xid, _)| xid);
        (xid, state.xmins.get(&pid).copied().unwrap_or_default())
    }

    /// The state of the transaction `xid` in `pg_xact` and `ProcArray`, for a transaction before [`Heap::next_xid`].
    pub(crate) fn xact_state(&self, xid: u32) -> XactState {
        let state = self.state();
        if state.running.contains(&xid) {
            XactState::InProgress
        } else if state.aborted.contains(&xid) {
            XactState::Aborted
        } else {
            XactState::Committed
        }
    }

    /// `GetSnapshotData`: the transactions whose changes are not visible now.
    pub(crate) fn snapshot(&self) -> Snapshot {
        let state = self.state();
        Snapshot {
            xmax: state.next_xid.max(INITDB_NEXT_XID),
            running: state.running.clone(),
            completed: (state.latest_completed + 1).max(INITDB_NEXT_XID),
        }
    }

    /// The rows of a table that the view sees, `HeapTupleSatisfiesMVCC`. Each row ends with the ID of its version.
    pub(crate) fn rows(&self, relation: u32, view: &View) -> Vec<Vec<Value>> {
        self.visible_rows(relation, view, false)
    }

    /// The rows of a table that the view sees, as [`Heap::rows`] gives them, with the values of the system columns `xmin` and `xmax` of each version before the ID of the version.
    pub(crate) fn rows_with_xids(&self, relation: u32, view: &View) -> Vec<Vec<Value>> {
        self.visible_rows(relation, view, true)
    }

    fn visible_rows(&self, relation: u32, view: &View, xids: bool) -> Vec<Vec<Value>> {
        let state = self.state();
        let Some(table) = state.tables.get(&(view.database, relation)) else {
            return Vec::new();
        };
        table
            .versions
            .iter()
            .filter(|version| visible(version, view))
            .map(|version| {
                let mut row = Vec::with_capacity(version.values.len() + 3);
                row.extend_from_slice(&version.values);
                if xids {
                    let xmax = if version.xmax == 0 { version.aborted } else { version.xmax };
                    row.push(Value::Oid(version.xmin));
                    row.push(Value::Oid(xmax));
                }
                row.push(Value::Int8(version.tid.cast_signed()));
                row
            })
            .collect()
    }

    /// The values of the versions `tids` of a table, also of a version that a transaction deleted, in the order of the list. A version that is not in the table gives no row.
    pub(crate) fn versions(&self, database: u32, relation: u32, tids: &[u64]) -> Vec<Vec<Value>> {
        let state = self.state();
        let Some(table) = state.tables.get(&(database, relation)) else {
            return Vec::new();
        };
        tids.iter()
            .filter_map(|&tid| table.find(tid).map(|at| table.versions[at].values.to_vec()))
            .collect()
    }

    /// True when a command of the transaction of `view` after the command of `view` changed the version `tid` of a table: `TM_SelfModified` with a `cmax` that is not the command of `view`.
    pub(crate) fn modified_later(&self, view: &View, relation: u32, tid: u64) -> bool {
        let state = self.state();
        let Some(table) = state.tables.get(&(view.database, relation)) else {
            return false;
        };
        table.find(tid).is_some_and(|at| {
            let version = &table.versions[at];
            view.xid != 0 && version.xmax == view.xid && version.cmax != view.command
        })
    }

    /// `heap_get_latest_tid`: the last version of the chain of updates from the version `tid` of a table, as the transaction of `view` sees the updates of its own and of the transactions that committed.
    pub(crate) fn latest_tid(&self, view: &View, relation: u32, mut tid: u64) -> u64 {
        let state = self.state();
        let Some(table) = state.tables.get(&(view.database, relation)) else {
            return tid;
        };
        while let Some(version) = table.find(tid).map(|at| &table.versions[at]) {
            let done = version.xmax == view.xid || !state.running.contains(&version.xmax);
            if version.next == 0 || version.xmax == 0 || !done {
                break;
            }
            tid = version.next;
        }
        tid
    }

    /// For each version of a table that the command `command` of the transaction `xid` added, in the order of the inserts, true if the transaction did not delete or update it after, as `SnapshotSelf` sees it.
    pub(crate) fn live(&self, database: u32, relation: u32, xid: u32, command: u32) -> Vec<bool> {
        let state = self.state();
        let Some(table) = state.tables.get(&(database, relation)) else {
            return Vec::new();
        };
        let mut made: Vec<(u64, bool)> = table
            .versions
            .iter()
            .filter(|version| version.xmin == xid && version.cmin == command)
            .map(|version| (version.made, version.xmax != xid))
            .collect();
        made.sort_unstable_by_key(|&(made, _)| made);
        made.into_iter().map(|(_, live)| live).collect()
    }

    /// Adds the rows that the command of the view inserted, and checks each new version for each unique key. When a key conflicts, no row is added, and the result tells the statement to wait or gives the duplicate key. `kept` has the IDs of the new versions of an earlier try that waited.
    ///
    /// # Errors
    ///
    /// The errors of the equality of a key.
    pub(crate) fn insert(
        &self,
        relation: u32,
        view: &View,
        rows: Vec<Vec<Value>>,
        unique: (&[Unique], Derive<'_>),
        kept: &[u64],
    ) -> Result<Changed> {
        self.insert_ids(relation, view, rows, unique, kept, &mut Vec::new())
    }

    /// [`Heap::insert`] that gives in `ids` the ID of the new version of each row, for `ctid` of `RETURNING`.
    ///
    /// # Errors
    ///
    /// The errors of the equality of a key.
    pub(crate) fn insert_ids(
        &self,
        relation: u32,
        view: &View,
        rows: Vec<Vec<Value>>,
        (unique, derive): (&[Unique], Derive<'_>),
        kept: &[u64],
        ids: &mut Vec<u64>,
    ) -> Result<Changed> {
        ids.clear();
        let count = rows.len() as u64;
        let made = (view.xid, view.command);
        if unique.is_empty() {
            let mut state = self.state();
            let State { tables, toast_oids, .. } = &mut *state;
            let table = tables.entry((view.database, relation)).or_default();
            for values in rows {
                ids.push(table.add(None, made, values, (unique, &[]), None, toast_oids));
            }
            return Ok(Changed::Done(count));
        }
        let mut keys = Vec::with_capacity(rows.len());
        for values in &rows {
            keys.push(fresh(unique, values, derive)?);
        }
        let (mut state, places) = self.lock_keys((view.database, relation), unique, derive)?;
        let State { tables, running, rechecks: pending, toast_oids, .. } = &mut *state;
        let table = tables.entry((view.database, relation)).or_default();
        let mut kept = kept.iter().copied();
        let mut used = Vec::with_capacity(rows.len());
        let mut partial = Vec::new();
        for (values, keys) in rows.into_iter().zip(&keys) {
            used.push(table.add(kept.next(), made, values, (unique, keys), None, toast_oids));
            let checked = (view.xid, &*running);
            match table.check_keys((&places, unique), &used, checked, &mut partial) {
                Ok(None) => {}
                result => {
                    table.undo(&used, &[]);
                    return result.map(|conflict| conflict.unwrap_or(Changed::Done(0)));
                }
            }
        }
        pending.extend(rechecks(view, relation, unique, partial));
        *ids = used;
        Ok(Changed::Done(count))
    }

    /// For each version of `tids` that is in the table, in the order of [`Heap::versions`], true when the transaction `xid` inserted it, as `slot_is_current_xact_tuple` tells.
    pub(crate) fn made_by(
        &self,
        database: u32,
        relation: u32,
        tids: &[u64],
        xid: u32,
    ) -> Vec<bool> {
        let state = self.state();
        let Some(table) = state.tables.get(&(database, relation)) else {
            return Vec::new();
        };
        tids.iter()
            .filter_map(|&tid| table.find(tid).map(|at| table.versions[at].xmin == xid))
            .collect()
    }

    /// The ID of the version after each version of `tids`, which an `UPDATE` made, or 0 for a version with no next version.
    pub(crate) fn next_versions(&self, database: u32, relation: u32, tids: &[u64]) -> Vec<u64> {
        let state = self.state();
        let Some(table) = state.tables.get(&(database, relation)) else {
            return vec![0; tids.len()];
        };
        tids.iter().map(|&tid| table.find(tid).map_or(0, |at| table.versions[at].next)).collect()
    }

    /// `ExecCheckIndexConstraints` and the speculative insertion of `ExecInsert` for one row of `INSERT ... ON CONFLICT`. The row goes in when no version that a dirty snapshot sees has its key in an arbiter index, the indexes at the places `arbiters` of `unique`. The check and the insertion hold the lock of the heap, so no other write can come between them. `kept` is the ID of the version of an earlier try that waited.
    ///
    /// # Errors
    ///
    /// The errors of the equality of a key.
    pub(crate) fn speculative(
        &self,
        relation: u32,
        view: &View,
        values: Vec<Value>,
        (unique, derive): (&[Unique], Derive<'_>),
        arbiters: &[usize],
        kept: Option<u64>,
    ) -> Result<Speculative> {
        let keys = fresh(unique, &values, derive)?;
        let (mut state, places) = self.lock_keys((view.database, relation), unique, derive)?;
        let State { tables, running, rechecks: pending, toast_oids, .. } = &mut *state;
        let table = tables.entry((view.database, relation)).or_default();
        for &arbiter in arbiters {
            let Some(&place) = places.get(arbiter) else { continue };
            let map = &table.keys[place];
            let key = match &map.derived {
                None => Some(values.as_slice()),
                Some(_) => keys.get(arbiter).and_then(|key| key.as_deref()),
            };
            let Some(places) = key else {
                continue;
            };
            let Some(hash) = map.hash(places) else {
                continue;
            };
            for &other in map.hashes.get(&hash).into_iter().flatten() {
                let Some(found) = table.find(other) else {
                    continue;
                };
                let version = &table.versions[found];
                let seen = dirty(version, view.xid, running);
                if seen == Seen::Dead {
                    continue;
                }
                match map.key(other, &version.values) {
                    Some(found) if map.same(places, found)? => {}
                    _ => continue,
                }
                if let Seen::Wait(holder) = seen {
                    return Ok(Speculative::Wait { holder, tid: other });
                }
                let own = version.xmin == view.xid;
                return Ok(Speculative::Conflict {
                    tid: other,
                    values: version.values.to_vec(),
                    own,
                    command: own && version.cmin == view.command,
                    visible: visible(version, view),
                });
            }
        }
        let used =
            [table.add(kept, (view.xid, view.command), values, (unique, &keys), None, toast_oids)];
        let mut partial = Vec::new();
        let checked = (view.xid, &*running);
        match table.check_keys((&places, unique), &used, checked, &mut partial) {
            Ok(None) => {
                pending.extend(rechecks(view, relation, unique, partial));
                Ok(Speculative::Inserted(used[0]))
            }
            result => {
                table.undo(&used, &[]);
                Ok(Speculative::Key(result?.unwrap_or(Changed::Done(0))))
            }
        }
    }

    /// `heap_update` and `heap_delete` for the rows that the command of the view changes. A row that the command changed before stays as it is, as for a row that a join gives more than once. When another transaction changed a row after the snapshot, nothing changes, and the result tells the statement to wait or to run again. When the change is done, `applied` has the index in `changes` of each row that changed, for `RETURNING`.
    ///
    /// # Errors
    ///
    /// `40001` in `REPEATABLE READ` and `SERIALIZABLE` for a row that a transaction that committed after the snapshot changed.
    pub(crate) fn change(
        &self,
        relation: u32,
        view: &View,
        changes: Vec<Change>,
        (unique, derive): (&[Unique], Derive<'_>),
        kept: &[u64],
        applied: &mut Vec<usize>,
    ) -> Result<Changed> {
        applied.clear();
        let mut keys = Vec::with_capacity(changes.len());
        for (_, values) in &changes {
            keys.push(match values {
                Some(values) => fresh(unique, values, derive)?,
                None => Vec::new(),
            });
        }
        let (mut state, places) = self.lock_keys((view.database, relation), unique, derive)?;
        let State { tables, running, rechecks: pending, toast_oids, .. } = &mut *state;
        let Some(table) = tables.get_mut(&(view.database, relation)) else {
            return Ok(Changed::Done(0));
        };
        let mut found: Vec<(u64, Option<Vec<Value>>, Fresh)> = Vec::with_capacity(changes.len());
        let mut indexes = Vec::with_capacity(changes.len());
        for (index, ((tid, values), keys)) in changes.into_iter().zip(keys).enumerate() {
            let Ok(at) = table.versions.binary_search_by_key(&tid, |version| version.tid) else {
                continue;
            };
            let version = &table.versions[at];
            match version.xmax {
                0 => {}
                // The command changed the row before, as for a row that a join gives more than once.
                xmax if xmax == view.xid && version.cmax == view.command => continue,
                xmax if xmax == view.xid => {
                    return Ok(Changed::SelfModified { update: values.is_some() });
                }
                xmax if running.contains(&xmax) => {
                    return Ok(Changed::Wait { holder: xmax, tid });
                }
                _ if view.serializable => {
                    let what = if version.next == 0 { "delete" } else { "update" };
                    return Err(Error::new(
                        SqlState::T_R_SERIALIZATION_FAILURE,
                        format!("could not serialize access due to concurrent {what}"),
                    ));
                }
                _ => return Ok(Changed::Again),
            }
            if found.iter().any(|(old, ..)| *old == tid) {
                continue;
            }
            found.push((tid, values, keys));
            indexes.push(index);
        }
        let count = found.len() as u64;
        let mut kept = kept.iter().copied();
        let mut used = Vec::new();
        let mut deleted = Vec::with_capacity(found.len());
        let mut partial = Vec::new();
        for (old, values, keys) in found {
            let made = (view.xid, view.command);
            table.pages.touch(page_of(old));
            let next = values.map_or(0, |v| {
                table.add(kept.next(), made, v, (unique, &keys), Some(old), toast_oids)
            });
            if let Some(at) = table.find(old) {
                let version = &mut table.versions[at];
                version.xmax = view.xid;
                version.cmax = view.command;
                version.next = next;
            }
            deleted.push(old);
            if next == 0 || places.is_empty() {
                continue;
            }
            used.push(next);
            let checked = (view.xid, &*running);
            match table.check_keys((&places, unique), &used, checked, &mut partial) {
                Ok(None) => {}
                result => {
                    table.undo(&used, &deleted);
                    return result.map(|conflict| conflict.unwrap_or(Changed::Done(0)));
                }
            }
        }
        pending.extend(rechecks(view, relation, unique, partial));
        *applied = indexes;
        Ok(Changed::Done(count))
    }

    /// The rows that `TRUNCATE` removes. Each version that no transaction deleted gets the transaction of the view as `xmax`, so that a rollback brings the rows back, as the old file of `RelationSetNewRelfilenumber` stays until the commit. `TRUNCATE` takes `AccessExclusiveLock`, which waits for each transaction that changed the table. So when another transaction in progress inserted or deleted a version, nothing changes, and the result is that transaction, for the statement to wait for.
    pub(crate) fn truncate(&self, relation: u32, view: &View) -> Option<u32> {
        let mut state = self.state();
        let State { tables, running, truncating, .. } = &mut *state;
        let table = tables.get_mut(&(view.database, relation))?;
        if let Some(holder) = table.holder(view.xid, running) {
            return Some(holder);
        }
        for version in table.versions.iter_mut().filter(|version| version.xmax == 0) {
            version.xmax = view.xid;
            version.cmax = view.command;
        }
        let new = table.pages.renewed();
        let old = std::mem::replace(&mut table.pages, new);
        if table.truncated.is_empty() {
            truncating.entry(view.xid).or_default().push((view.database, relation));
        }
        table.truncated.push((view.xid, view.command, old));
        None
    }

    /// Marks the statement of `view` as the bulk insert of `CREATE TABLE AS` into the table `relation`, which extends the heap by more pages at a time, as a `BulkInsertState` does.
    pub(crate) fn start_bulk(&self, relation: u32, view: &View) {
        let mut state = self.state();
        let table = state.tables.entry((view.database, relation)).or_default();
        table.bulk = Some((view.xid, view.command));
    }

    /// `RelationGetNumberOfBlocks` of the heap of a table and of its free space map and visibility map, and `relpages`, `reltuples`, `relallvisible` and `relallfrozen` of the table.
    pub(crate) fn size(&self, database: u32, relation: u32) -> HeapSize {
        let state = self.state();
        let pages = state.tables.get(&(database, relation)).map(|table| &table.pages);
        let (fsm_blocks, vm_blocks) = pages.map_or((0, 0), Pages::fork_blocks);
        HeapSize {
            blocks: pages.map_or(0, Pages::blocks),
            relpages: pages.map_or(0, |p| p.relpages),
            reltuples: pages.map_or(-1.0, |p| p.reltuples),
            relallvisible: pages.map_or(0, |p| p.relallvisible),
            relallfrozen: pages.map_or(0, |p| p.relallfrozen),
            fsm_blocks,
            vm_blocks,
        }
    }

    /// `RelationGetNumberOfBlocks` and `_bt_getrootheight` of the `btree` index `index` of the shape `shape` on a table, from the versions of the table that a transaction that did not abort inserted, as `CREATE INDEX` would build the index now. A table with more versions than [`btree::EXACT`] gives the size of a sample, which serves until the versions change by a 32nd. The values of the sample are read with the heap locked and sorted with the heap not locked.
    pub(crate) fn btree_size(
        &self,
        database: u32,
        relation: u32,
        index: u32,
        shape: &Shape,
    ) -> BtreeSize {
        let (entries, total, made, count) = {
            let state = self.state();
            let Some(table) = state.tables.get(&(database, relation)) else {
                return BtreeSize::EMPTY;
            };
            let count = table.versions.len();
            if let Some(cached) = table.btree_sizes.get(&index)
                && cached.shape == *shape
                && (cached.made == table.made && cached.count == count
                    || count > btree::EXACT && cached.count.abs_diff(count) < cached.count / 32)
            {
                return cached.size;
            }
            let step = count.div_ceil(btree::EXACT).max(1);
            let kept = table.versions.iter().filter(|v| !state.aborted.contains(&v.xmin));
            let total = if step == 1 { 0 } else { kept.clone().count() };
            let entries: Vec<Box<[Value]>> = kept
                .step_by(step)
                .map(|v| {
                    shape
                        .columns
                        .iter()
                        .map(|&c| v.values.get(c).cloned().unwrap_or(Value::Null))
                        .collect()
                })
                .collect();
            let total = total.max(entries.len());
            (entries, total, table.made, count)
        };
        let size = btree::size(entries, total, shape);
        let mut state = self.state();
        if let Some(table) = state.tables.get_mut(&(database, relation)) {
            let cached = btree::Cached { made, count, shape: shape.clone(), size };
            table.btree_sizes.insert(index, cached);
        }
        size
    }

    /// The size of a `btree` index whose build put `entries` in it, the keys of an index with expressions or a predicate. The columns of `shape` are places in the entries.
    pub(crate) fn btree_size_of(entries: Vec<Box<[Value]>>, shape: &Shape) -> BtreeSize {
        let total = entries.len();
        btree::size(entries, total, shape)
    }

    /// The number of live versions of a table for `ANALYZE` and `VACUUM`, as `heapam_scan_analyze_next_tuple` counts them for the transaction `xid`: the versions that a transaction that committed or `xid` inserted, and that no transaction deleted, or that a transaction in progress other than `xid` deletes.
    pub(crate) fn live_count(&self, database: u32, relation: u32, xid: u32) -> u64 {
        let state = self.state();
        let Some(table) = state.tables.get(&(database, relation)) else { return 0 };
        let done = |x: u32| x == xid || !state.running.contains(&x);
        let live = |v: &&Version| {
            done(v.xmin) && (v.xmax == 0 || (v.xmax != xid && state.running.contains(&v.xmax)))
        };
        table.versions.iter().filter(live).count() as u64
    }

    /// `acquire_sample_rows` of `ANALYZE` for the transaction `xid`: the values of the live versions of a table, as [`Heap::live_count`] counts them, in the order of their places in the heap, and the number of the live versions. A table with more than `target` live versions gives `target` of them at equal distances, where PostgreSQL takes a random sample.
    pub(crate) fn sample(
        &self,
        database: u32,
        relation: u32,
        xid: u32,
        target: usize,
    ) -> (Vec<Vec<Value>>, f64) {
        let state = self.state();
        let Some(table) = state.tables.get(&(database, relation)) else {
            return (Vec::new(), 0.0);
        };
        let done = |x: u32| x == xid || !state.running.contains(&x);
        let live: Vec<&Version> = table
            .versions
            .iter()
            .filter(|v| {
                done(v.xmin) && (v.xmax == 0 || (v.xmax != xid && state.running.contains(&v.xmax)))
            })
            .collect();
        let total = live.len();
        let rows = if total <= target {
            live.iter().map(|v| v.values.to_vec()).collect()
        } else {
            (0..target).map(|i| live[i * total / target].values.to_vec()).collect()
        };
        (rows, total as f64)
    }

    /// `get_actual_variable_endpoint`: the least and the greatest value that is not null of the column at `column` of a table, in the order `order`, of the versions that `SnapshotNonVacuumable` sees. A version is dead when a transaction that ended deleted it before the horizon of the running transactions and of the snapshots, as `HeapTupleSatisfiesVacuum` finds it.
    pub(crate) fn variable_range(
        &self,
        database: u32,
        relation: u32,
        column: usize,
        order: impl Fn(&Value, &Value) -> Option<Ordering>,
    ) -> Option<(Value, Value)> {
        let state = self.state();
        let table = state.tables.get(&(database, relation))?;
        let horizon = state
            .running
            .first()
            .copied()
            .into_iter()
            .chain(state.xmins.values().copied())
            .fold((state.latest_completed + 1).max(INITDB_NEXT_XID), u32::min);
        let dead =
            |v: &Version| v.xmax != 0 && !state.running.contains(&v.xmax) && v.xmax < horizon;
        let mut range: Option<(&Value, &Value)> = None;
        for version in table.versions.iter().filter(|v| !dead(v)) {
            let Some(value) = version.values.get(column).filter(|v| !v.is_null()) else {
                continue;
            };
            range = Some(match range {
                None => (value, value),
                Some((min, max)) => (
                    if order(value, min)?.is_lt() { value } else { min },
                    if order(value, max)?.is_gt() { value } else { max },
                ),
            });
        }
        range.map(|(min, max)| (min.clone(), max.clone()))
    }

    /// `update_attstats`: replaces the rows of `pg_statistic` of a table for the columns of `rows`, outside of the transactions.
    pub(crate) fn set_statistics(&self, database: u32, relation: u32, rows: Vec<Statistic>) {
        let mut state = self.state();
        let table = state.tables.entry((database, relation)).or_default();
        for row in rows {
            table.statistics.insert((row.attnum, row.inherit), row);
        }
    }

    /// The rows of `pg_statistic` of a table, in the order of `staattnum` and `stainherit`.
    pub(crate) fn statistics(&self, database: u32, relation: u32) -> Vec<Statistic> {
        let state = self.state();
        state
            .tables
            .get(&(database, relation))
            .map(|table| table.statistics.values().cloned().collect())
            .unwrap_or_default()
    }

    /// Sets `attlen` and `attalign` of the columns of the table `relation` with a type of fixed length, which the new versions take in their pages, as `heap_compute_data_size` stores them.
    pub(crate) fn set_layout(
        &self,
        database: u32,
        relation: u32,
        (fixed, storage): (Vec<Fixed>, Vec<Storage>),
    ) {
        let mut state = self.state();
        let table = state.tables.entry((database, relation)).or_default();
        if table.fixed != fixed {
            table.fixed = fixed;
        }
        if table.storage != storage {
            table.storage = storage;
        }
    }

    /// The storage of the columns of the table `relation` that [`Heap::set_layout`] set, for the widths of the values of `ANALYZE`.
    pub(crate) fn layout(&self, database: u32, relation: u32) -> (Vec<Fixed>, Vec<Storage>) {
        let state = self.state();
        state
            .tables
            .get(&(database, relation))
            .map(|table| (table.fixed.clone(), table.storage.clone()))
            .unwrap_or_default()
    }

    /// Takes the number of the values that the writes moved to the TOAST tables since the last call. Each one used an OID of the counter of the cluster, as `toast_save_datum` gives it.
    pub(crate) fn take_toast_oids(&self) -> u32 {
        std::mem::take(&mut self.state().toast_oids)
    }

    /// `vac_update_relstats`: sets `relpages` and `reltuples` of a table, outside of the transactions.
    pub(crate) fn set_stats(&self, database: u32, relation: u32, relpages: i32, reltuples: f32) {
        let mut state = self.state();
        state.tables.entry((database, relation)).or_default().pages.set_stats(relpages, reltuples);
    }

    /// `heap_vacuum_rel` of the table `relation` for the session `pid`: the versions that a transaction deleted before each snapshot and transaction in progress went, as `HeapTupleSatisfiesVacuum` finds them `HEAPTUPLE_DEAD`, give their space and their line pointers back. The pages that the visibility map marks for `skip` keep their versions. Each page that `VACUUM` scans goes to the free space map, with its bits in the visibility map. `indexed` is true when the table has an index, `freeze` freezes the versions that each snapshot sees, and `truncate` lets the heap lose its empty pages at the end. The result is `reltuples`.
    pub(crate) fn vacuum(
        &self,
        (database, relation, pid): (u32, u32, i32),
        skip: Skip,
        (indexed, freeze, truncate): (bool, bool, bool),
    ) -> f32 {
        let mut state = self.state();
        let State { tables, running, xmins, latest_completed, .. } = &mut *state;
        // `GetOldestNonRemovableTransactionId`, without the snapshot of `VACUUM`, as `PROC_IN_VACUUM` leaves it out.
        let horizon = running
            .first()
            .copied()
            .into_iter()
            .chain(xmins.iter().filter(|&(&other, _)| other != pid).map(|(_, &xmin)| xmin))
            .fold((*latest_completed + 1).max(INITDB_NEXT_XID), u32::min);
        let Some(table) = tables.get_mut(&(database, relation)) else { return 0.0 };
        let generation = table.pages.generation;
        let scanned = table.pages.scanned(skip);
        let in_scan = |tid: u64| {
            generation_of(tid) == generation
                && scanned.get(page_of(tid) as usize).copied().unwrap_or(false)
        };
        let dead = |v: &Version| v.xmax != 0 && !running.contains(&v.xmax) && v.xmax < horizon;
        let gone: Vec<u64> =
            table.versions.iter().filter(|v| in_scan(v.tid) && dead(v)).map(|v| v.tid).collect();
        table.purge(&gone);
        let mut pruned: Vec<Pruned> = scanned
            .iter()
            .map(|_| Pruned { kept: Vec::new(), visible: true, frozen: true })
            .collect();
        let (mut live, mut nonempty) = (0.0, 0);
        for version in &table.versions {
            if generation_of(version.tid) != generation {
                continue;
            }
            let at = page_of(version.tid);
            nonempty = nonempty.max(at + 1);
            if !in_scan(version.tid) {
                continue;
            }
            let page = &mut pruned[at as usize];
            page.kept.push((slot_of(version.tid), version.len));
            let committed = !running.contains(&version.xmin);
            let visible = committed && version.xmin < horizon && version.xmax == 0;
            page.visible &= visible;
            page.frozen &= freeze && visible;
            if committed && (version.xmax == 0 || running.contains(&version.xmax)) {
                live += 1.0;
            }
        }
        for (at, page) in pruned.iter().enumerate().filter(|&(at, _)| scanned[at]) {
            table.pages.prune(at as u32, page, indexed);
        }
        let total = table.pages.vacuumed(&scanned, nonempty, truncate);
        let count = u32::try_from(scanned.iter().filter(|&&s| s).count()).unwrap_or(u32::MAX);
        table.pages.vacuum_stats(total, count, live)
    }

    /// Records that `xid` waits for `holder`, as `LockAcquire` adds an edge to the wait graph.
    pub(crate) fn wait(&self, xid: u32, holder: u32) {
        self.state().waits.insert(xid, holder);
    }

    /// The end of the wait of `xid`.
    pub(crate) fn waited(&self, xid: u32) {
        self.state().waits.remove(&xid);
    }

    /// The process IDs of the sessions whose transactions the transactions of the session `pid` wait for.
    pub(crate) fn blockers(&self, pid: i32) -> Vec<i32> {
        let state = self.state();
        let mut pids = Vec::new();
        for (xid, holder) in &state.waits {
            if state.owners.get(xid) != Some(&pid) {
                continue;
            }
            if let Some(&holder) = state.owners.get(holder)
                && holder != pid
                && !pids.contains(&holder)
            {
                pids.push(holder);
            }
        }
        pids
    }

    /// `DeadLockCheck`: the cycle of waits from `xid` back to `xid`, or `None` when the waits of `xid` make no cycle.
    pub(crate) fn deadlock(&self, xid: u32) -> Option<Vec<Wait>> {
        let state = self.state();
        let pid = |xid: u32| state.owners.get(&xid).copied().unwrap_or_default();
        let mut cycle = Vec::new();
        let mut next = xid;
        while let Some(&holder) = state.waits.get(&next) {
            cycle.push(Wait { pid: pid(next), xid: holder, holder: pid(holder) });
            if holder == xid {
                return Some(cycle);
            }
            if cycle.len() > state.waits.len() {
                return None;
            }
            next = holder;
        }
        None
    }

    /// True while a transaction is in progress.
    pub(crate) fn running(&self, xid: u32) -> bool {
        self.state().running.contains(&xid)
    }

    /// True if a table has a version of a row, visible or not.
    pub(crate) fn has_rows(&self, database: u32, relation: u32) -> bool {
        let state = self.state();
        state.tables.get(&(database, relation)).is_some_and(|table| !table.versions.is_empty())
    }

    /// `RecordTransactionCommit`: the changes of the transaction become visible to the new snapshots.
    pub(crate) fn commit(&self, xid: u32) {
        let mut state = self.state();
        let State { tables, truncating, .. } = &mut *state;
        for table in truncating.remove(&xid).into_iter().flatten() {
            let Some(table) = tables.get_mut(&table) else { continue };
            table.truncated.clear();
            let generation = table.pages.generation;
            table.remove(|version| generation_of(version.tid) != generation);
        }
        state.running.remove(&xid);
        state.latest_completed = state.latest_completed.max(xid);
        state.owners.remove(&xid);
        state.waits.remove(&xid);
        state.rechecks.retain(|recheck| recheck.xid != xid);
    }

    /// `RecordTransactionAbort`: the rows that the transaction inserted go, and the rows that it deleted stay.
    pub(crate) fn abort(&self, xid: u32) {
        let mut state = self.state();
        for table in state.tables.values_mut() {
            if table.truncated.first().is_some_and(|(x, ..)| *x == xid) {
                table.pages = table.truncated.swap_remove(0).2;
                table.truncated.clear();
            }
            table.remove(|version| version.xmin == xid);
            for version in table.versions.iter_mut().filter(|version| version.xmax == xid) {
                version.aborted = xid;
                version.xmax = 0;
                version.cmax = 0;
                version.next = 0;
            }
        }
        state.running.remove(&xid);
        state.latest_completed = state.latest_completed.max(xid);
        state.aborted.insert(xid);
        state.truncating.remove(&xid);
        state.owners.remove(&xid);
        state.waits.remove(&xid);
        state.rechecks.retain(|recheck| recheck.xid != xid);
    }

    /// `AbortSubTransaction` for the rows: the versions that the commands of `xid` from `command` on inserted go, and the versions that they deleted stay. A transaction has no transaction IDs for its subtransactions, so the command of each change tells the changes of the subtransaction from the changes before it.
    pub(crate) fn rollback_to(&self, xid: u32, command: u32) {
        let mut state = self.state();
        state.rechecks.retain(|recheck| recheck.xid != xid || recheck.command < command);
        for table in state.tables.values_mut() {
            if let Some(at) = table.truncated.iter().position(|&(x, c, _)| x == xid && c >= command)
            {
                table.pages = table.truncated.swap_remove(at).2;
                table.truncated.truncate(at);
            }
            table.remove(|version| version.xmin == xid && version.cmin >= command);
            let deleted = |version: &&mut Version| version.xmax == xid && version.cmax >= command;
            for version in table.versions.iter_mut().filter(deleted) {
                version.aborted = xid;
                version.xmax = 0;
                version.cmax = 0;
                version.next = 0;
            }
        }
    }

    /// The rechecks of the transaction `xid` from the command `command` on, in the order of the writes, which leave the heap.
    pub(crate) fn take_rechecks(&self, xid: u32, command: u32) -> Vec<Recheck> {
        let mut state = self.state();
        let (taken, kept) = std::mem::take(&mut state.rechecks)
            .into_iter()
            .partition(|recheck| recheck.xid == xid && recheck.command >= command);
        state.rechecks = kept;
        taken
    }

    /// `unique_key_recheck` for the transaction `xid`: the key of the version of `recheck` in its index, checked again with `UNIQUE_CHECK_EXISTING`. A version that went needs no check. The result is the first conflict, as [`Heap::insert`] gives it: a live version with the same key, or a transaction in progress to wait for.
    ///
    /// # Errors
    ///
    /// The errors of the equality of a key.
    pub(crate) fn recheck(
        &self,
        xid: u32,
        recheck: &Recheck,
        derive: Derive<'_>,
    ) -> Result<Option<Changed>> {
        let unique = [Unique { deferrable: None, ..recheck.unique.clone() }];
        let (mut state, places) =
            self.lock_keys((recheck.database, recheck.table), &unique, derive)?;
        let State { tables, running, .. } = &mut *state;
        let Some(table) = tables.get_mut(&(recheck.database, recheck.table)) else {
            return Ok(None);
        };
        let live = table.find(recheck.tid).map(|at| dirty(&table.versions[at], xid, running));
        if live.is_none_or(|seen| seen == Seen::Dead) {
            return Ok(None);
        }
        let checked = (xid, &*running);
        table.check_keys((&places, &unique), &[recheck.tid], checked, &mut Vec::new())
    }

    /// A transaction other than the one of `view` that is in progress and wrote a version of the table `relation`, which a check of the rows in place waits for, as the lock of the table waits for it in PostgreSQL.
    pub(crate) fn holder(&self, relation: u32, view: &View) -> Option<u32> {
        let state = self.state();
        state.tables.get(&(view.database, relation))?.holder(view.xid, &state.running)
    }

    /// `_bt_load` with `UNIQUE_CHECK_YES` of a new unique index on the versions that the table `relation` has, as `CREATE UNIQUE INDEX` and `ALTER TABLE ... ADD UNIQUE` check them. The versions stay in place. The result is the first live version, in the order of the IDs, with the key of a live version before it, or a transaction in progress to wait for.
    ///
    /// # Errors
    ///
    /// The errors of the expressions of the index and of the equality of a key.
    pub(crate) fn check_build(
        &self,
        relation: u32,
        view: &View,
        unique: &[Unique],
        derive: Derive<'_>,
    ) -> Result<Option<Changed>> {
        let table = (view.database, relation);
        let (state, places) = self.lock_keys(table, unique, derive)?;
        let Some(rows) = state.tables.get(&table) else {
            return Ok(None);
        };
        let checked = (view.xid, &state.running);
        for (index, &place) in places.iter().enumerate() {
            if let Some(changed) = rows.first_duplicate(index, &rows.keys[place], checked)? {
                return Ok(Some(changed));
            }
        }
        Ok(None)
    }

    /// The versions of the table `relation` that an exclusion constraint checks for the transaction `xid`, as a scan of the index with `SnapshotDirty` finds them, in the order of the IDs. A version that went before the command `command` is not in the list. The result is empty when the commands of `xid` from `command` on made no version of the table. With no command, the list has the live versions, as the build of a new index sees them.
    pub(crate) fn exclusion_rows(
        &self,
        database: u32,
        relation: u32,
        xid: u32,
        command: Option<u32>,
    ) -> Vec<Candidate> {
        let state = self.state();
        let Some(table) = state.tables.get(&(database, relation)) else {
            return Vec::new();
        };
        let new = |v: &Version| command.is_some_and(|command| v.xmin == xid && v.cmin >= command);
        if command.is_some() && !table.versions.iter().any(new) {
            return Vec::new();
        }
        let command = command.unwrap_or(u32::MAX);
        let mut list = Vec::new();
        for version in &table.versions {
            let seen = dirty(version, xid, &state.running);
            let replaced = if version.xmax == xid && version.cmax >= command && version.next != 0 {
                table.find(version.next).map_or(0, |at| table.versions[at].made)
            } else {
                0
            };
            let fresh = new(version);
            if seen == Seen::Dead && replaced == 0 && !fresh {
                continue;
            }
            list.push(Candidate {
                tid: version.tid,
                made: version.made,
                new: fresh,
                holder: match seen {
                    Seen::Wait(holder) => Some(holder),
                    _ => None,
                },
                live: seen == Seen::Live,
                replaced,
                values: version.values.to_vec(),
            });
        }
        list
    }

    /// Removes the rows of the tables of a database that `keep` does not keep, after a commit drops the tables.
    pub(crate) fn drop_tables(&self, database: u32, keep: impl Fn(u32) -> bool) {
        self.state().tables.retain(|&(db, relation), _| db != database || keep(relation));
    }
}

/// `XidInMVCCSnapshot` with the reverse sense: true if the snapshot sees the changes of another transaction.
fn committed_before(xid: u32, snapshot: &Snapshot) -> bool {
    xid < snapshot.xmax && !snapshot.running.contains(&xid)
}

/// `HeapTupleSatisfiesMVCC`: true if the view sees the version.
fn visible(version: &Version, view: &View) -> bool {
    let inserted = if view.xid != 0 && version.xmin == view.xid {
        version.cmin < view.command
    } else {
        committed_before(version.xmin, &view.snapshot)
    };
    if !inserted {
        return false;
    }
    match version.xmax {
        0 => true,
        xmax if view.xid != 0 && xmax == view.xid => version.cmax >= view.command,
        xmax => !committed_before(xmax, &view.snapshot),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The key of a row in an index with an expression, which the tests do not have.
    fn none(_: &Derived, _: &[Value]) -> Result<Option<Vec<Value>>> {
        Ok(None)
    }

    fn view(snapshot: Snapshot, xid: u32, command: u32) -> View {
        View { database: 1, snapshot, xid, command, serializable: false }
    }

    /// The ID of the version of the first row that the view sees.
    fn first_tid(heap: &Heap, view: &View) -> u64 {
        let rows = heap.rows(7, view);
        let Some(Value::Int8(tid)) = rows[0].last() else { panic!("a row with no ID") };
        tid.cast_unsigned()
    }

    #[test]
    fn a_snapshot_sees_only_the_committed_rows() {
        let heap = Heap::default();
        let xid = heap.begin(1);
        heap.insert(
            7,
            &view(heap.snapshot(), xid, 0),
            vec![vec![Value::Int4(1)]],
            (&[], &none),
            &[],
        )
        .unwrap();
        assert_eq!(heap.rows(7, &view(heap.snapshot(), 0, 0)).len(), 0);
        assert_eq!(heap.rows(7, &view(heap.snapshot(), xid, 0)).len(), 0);
        assert_eq!(heap.rows(7, &view(heap.snapshot(), xid, 1)).len(), 1);
        let before = heap.snapshot();
        heap.commit(xid);
        assert_eq!(heap.rows(7, &view(before, 0, 0)).len(), 0);
        assert_eq!(heap.rows(7, &view(heap.snapshot(), 0, 0)).len(), 1);
    }

    #[test]
    fn an_abort_removes_the_rows() {
        let heap = Heap::default();
        let xid = heap.begin(1);
        heap.insert(
            7,
            &view(heap.snapshot(), xid, 0),
            vec![vec![Value::Int4(1)]],
            (&[], &none),
            &[],
        )
        .unwrap();
        heap.abort(xid);
        assert!(!heap.has_rows(1, 7));
        assert_eq!(heap.rows(7, &view(heap.snapshot(), 0, 0)).len(), 0);
        let xid = heap.begin(1);
        heap.insert(
            7,
            &view(heap.snapshot(), xid, 0),
            vec![vec![Value::Int4(1)]],
            (&[], &none),
            &[],
        )
        .unwrap();
        assert_eq!(first_tid(&heap, &view(heap.snapshot(), xid, 1)), 2);
    }

    #[test]
    fn a_truncate_waits_and_a_rollback_brings_the_rows_back() {
        let heap = Heap::default();
        let first = heap.begin(1);
        heap.insert(
            7,
            &view(heap.snapshot(), first, 0),
            vec![vec![Value::Int4(1)]],
            (&[], &none),
            &[],
        )
        .unwrap();
        let a = heap.begin(2);
        assert_eq!(heap.truncate(7, &view(heap.snapshot(), a, 0)), Some(first));
        heap.commit(first);
        assert_eq!(heap.truncate(7, &view(heap.snapshot(), a, 0)), None);
        assert_eq!(heap.rows(7, &view(heap.snapshot(), a, 1)).len(), 0);
        assert_eq!(heap.rows(7, &view(heap.snapshot(), 0, 0)).len(), 1);
        heap.abort(a);
        assert_eq!(heap.rows(7, &view(heap.snapshot(), 0, 0)).len(), 1);
    }

    #[test]
    fn a_rollback_to_a_savepoint_undoes_the_later_commands() {
        let heap = Heap::default();
        let a = heap.begin(1);
        let row = |n| vec![vec![Value::Int4(n)]];
        heap.insert(7, &view(heap.snapshot(), a, 0), row(1), (&[], &none), &[]).unwrap();
        heap.insert(7, &view(heap.snapshot(), a, 1), row(2), (&[], &none), &[]).unwrap();
        assert_eq!(heap.truncate(7, &view(heap.snapshot(), a, 2)), None);
        assert_eq!(heap.rows(7, &view(heap.snapshot(), a, 3)).len(), 0);
        heap.rollback_to(a, 1);
        let rows = heap.rows(7, &view(heap.snapshot(), a, 3));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0], Value::Int4(1));
    }

    #[test]
    fn an_update_waits_for_the_other_transaction() {
        let heap = Heap::default();
        let first = heap.begin(1);
        heap.insert(
            7,
            &view(heap.snapshot(), first, 0),
            vec![vec![Value::Int4(1)]],
            (&[], &none),
            &[],
        )
        .unwrap();
        heap.commit(first);
        let a = heap.begin(1);
        let b = heap.begin(2);
        let tid = first_tid(&heap, &view(heap.snapshot(), a, 0));
        let change = || vec![(tid, Some(vec![Value::Int4(2)]))];
        let done = heap
            .change(7, &view(heap.snapshot(), a, 0), change(), (&[], &none), &[], &mut Vec::new())
            .unwrap();
        assert_eq!(done, Changed::Done(1));
        let wait = Changed::Wait { holder: a, tid };
        assert_eq!(
            heap.change(
                7,
                &view(heap.snapshot(), b, 0),
                change(),
                (&[], &none),
                &[],
                &mut Vec::new()
            )
            .unwrap(),
            wait
        );
        heap.wait(b, a);
        assert_eq!(heap.deadlock(b), None);
        assert_eq!(heap.blockers(2), [1]);
        assert_eq!(heap.blockers(1), [] as [i32; 0]);
        heap.wait(a, b);
        let cycle = heap.deadlock(b).unwrap();
        assert_eq!(cycle, [Wait { pid: 2, xid: a, holder: 1 }, Wait { pid: 1, xid: b, holder: 2 }]);
        heap.waited(a);
        heap.abort(a);
        let done = heap
            .change(7, &view(heap.snapshot(), b, 0), change(), (&[], &none), &[], &mut Vec::new())
            .unwrap();
        assert_eq!(done, Changed::Done(1));
        heap.commit(b);
        let rows = heap.rows(7, &view(heap.snapshot(), 0, 0));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][0], Value::Int4(2));
    }

    #[test]
    fn a_unique_key_waits_for_the_other_transaction() {
        let heap = Heap::default();
        let key = Unique {
            oid: 0,
            name: "t_pkey".to_string(),
            columns: vec![0],
            types: vec![oid::INT4],
            shown: "a".to_string(),
            nulls_not_distinct: false,
            derived: None,
            deferrable: None,
        };
        let unique = std::slice::from_ref(&key);
        let row = |a: i32| vec![Value::Int4(a)];
        let first = heap.begin(1);
        let rows = vec![row(1), Vec::from([Value::Null]), Vec::from([Value::Null])];
        let done =
            heap.insert(7, &view(heap.snapshot(), first, 0), rows, (unique, &none), &[]).unwrap();
        assert_eq!(done, Changed::Done(3));
        let again =
            heap.insert(7, &view(heap.snapshot(), first, 1), vec![row(1)], (unique, &none), &[]);
        assert_eq!(again.unwrap(), Changed::Duplicate { index: 0, key: row(1) });
        heap.commit(first);
        let a = heap.begin(1);
        let b = heap.begin(2);
        heap.insert(7, &view(heap.snapshot(), a, 0), vec![row(2)], (unique, &none), &[]).unwrap();
        let wait = heap.insert(
            7,
            &view(heap.snapshot(), b, 0),
            vec![row(3), row(2)],
            (unique, &none),
            &[],
        );
        assert_eq!(wait.unwrap(), Changed::WaitKey { holder: a, index: 0, used: vec![6, 7] });
        heap.abort(a);
        let done = heap.insert(
            7,
            &view(heap.snapshot(), b, 0),
            vec![row(3), row(2)],
            (unique, &none),
            &[6, 7],
        );
        assert_eq!(done.unwrap(), Changed::Done(2));
        heap.commit(b);
        // The keys of the rows that the transaction deleted are free for the transaction.
        let c = heap.begin(1);
        let view_c = view(heap.snapshot(), c, 0);
        let rows = heap.rows(7, &view_c);
        let tids: Vec<u64> = rows
            .iter()
            .filter(|r| matches!(r[0], Value::Int4(2 | 3)))
            .map(|r| match r[1] {
                Value::Int8(tid) => tid.cast_unsigned(),
                _ => 0,
            })
            .collect();
        let changes = tids.iter().map(|&tid| (tid, None)).collect();
        assert_eq!(
            heap.change(7, &view_c, changes, (unique, &none), &[], &mut Vec::new()).unwrap(),
            Changed::Done(2)
        );
        let view_c = view(heap.snapshot(), c, 1);
        assert_eq!(
            heap.insert(7, &view_c, vec![row(2), row(3)], (unique, &none), &[]).unwrap(),
            Changed::Done(2)
        );
    }

    #[test]
    fn a_committed_change_runs_the_statement_again() {
        let heap = Heap::default();
        let first = heap.begin(1);
        heap.insert(
            7,
            &view(heap.snapshot(), first, 0),
            vec![vec![Value::Int4(1)]],
            (&[], &none),
            &[],
        )
        .unwrap();
        heap.commit(first);
        let old = heap.snapshot();
        let tid = first_tid(&heap, &view(old.clone(), 0, 0));
        let a = heap.begin(1);
        heap.change(
            7,
            &view(heap.snapshot(), a, 0),
            vec![(tid, None)],
            (&[], &none),
            &[],
            &mut Vec::new(),
        )
        .unwrap();
        heap.commit(a);
        let b = heap.begin(2);
        let change = vec![(tid, Some(vec![Value::Int4(3)]))];
        assert_eq!(
            heap.change(
                7,
                &view(old.clone(), b, 0),
                change.clone(),
                (&[], &none),
                &[],
                &mut Vec::new()
            )
            .unwrap(),
            Changed::Again
        );
        let serializable = View { serializable: true, ..view(old, b, 0) };
        let error =
            heap.change(7, &serializable, change, (&[], &none), &[], &mut Vec::new()).unwrap_err();
        assert_eq!(error.state(), SqlState::T_R_SERIALIZATION_FAILURE);
        assert_eq!(error.message(), "could not serialize access due to concurrent delete");
        assert_eq!(tid_text(1), "(0,1)");
        assert_eq!(tid_text(1 << 16 | 1), "(1,1)");
    }
}
