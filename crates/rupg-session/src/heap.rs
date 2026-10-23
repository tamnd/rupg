//! The rows of the tables in memory, with the transaction IDs of `heapam_visibility.c`. Each version of a row has the transaction and the command that made it, and the transaction and the command that deleted it. A snapshot tells which versions a statement sees.

use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Mutex, MutexGuard, PoisonError};

use rupg_analyze::{Derived, Unique};
use rupg_common::{Error, Result, SqlState};
use rupg_types::{Value, oid};

/// The key of a row in a unique index with an expression or a predicate, or `None` for a row that is not in the index, as [`rupg_exec::index_key`] gives it.
pub(crate) type Derive<'a> = &'a dyn Fn(&Derived, &[Value]) -> Result<Option<Vec<Value>>>;

/// `FirstNormalTransactionId`: the first transaction ID that a transaction gets.
const FIRST_NORMAL_XID: u32 = 3;

/// `MaxHeapTuplesPerPage`: the most versions that a page of 8 kB has. The ID of a version gives its page and its place in the page as if each page had this number of versions.
const TUPLES_PER_PAGE: u64 = 291;

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
    /// The process ID of the session of each transaction that is in progress.
    owners: HashMap<u32, i32>,
    /// The versions of the rows of each table, by the OID of the database and the OID of the table.
    tables: HashMap<(u32, u32), Table>,
    /// The transaction that each waiting transaction waits for, the edges of the wait graph of `DeadLockCheck`.
    waits: HashMap<u32, u32>,
    /// The new versions of the transactions in progress whose keys a deferrable unique index must check again.
    rechecks: Vec<Recheck>,
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
    /// The ID of the last version that the table got. An ID is not used again, also after an abort.
    last_tid: u64,
    /// The map of each unique key that a write checked. A map has the IDs of all the versions, live or not, with a key that has no null.
    keys: Vec<KeyMap>,
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
}

impl KeyMap {
    fn of(key: &Unique) -> Self {
        KeyMap {
            columns: key.columns.clone(),
            types: key.types.clone(),
            nulls_not_distinct: key.nulls_not_distinct,
            derived: key.derived.clone(),
            hashes: HashMap::new(),
        }
    }

    fn is(&self, key: &Unique) -> bool {
        self.columns == key.columns
            && self.types == key.types
            && self.nulls_not_distinct == key.nulls_not_distinct
            && self.derived == key.derived
    }

    /// The values that the key columns are places in: the row itself, or the values of the expressions of the index. `None` is a row that is not in the index.
    fn key<'v>(&self, values: &'v [Value], derive: Derive<'_>) -> Result<Option<Cow<'v, [Value]>>> {
        match &self.derived {
            None => Ok(Some(Cow::Borrowed(values))),
            Some(derived) => Ok(derive(derived, values)?.map(Cow::Owned)),
        }
    }

    /// The hash of the key of a row, as [`KeyMap::hash`] gives it, or `None` when the row is not in the index.
    fn row_hash(&self, values: &[Value], derive: Derive<'_>) -> Result<Option<u64>> {
        Ok(self.key(values, derive)?.and_then(|places| self.hash(&places)))
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
    /// Adds a version with the ID `kept` that an earlier try of the write had, or with a new ID, at its place in the order of the IDs. The version goes in the map of each unique key.
    fn add(
        &mut self,
        kept: Option<u64>,
        (xmin, cmin): (u32, u32),
        values: Vec<Value>,
        derive: Derive<'_>,
    ) -> Result<u64> {
        let mut hashes = Vec::with_capacity(self.keys.len());
        for map in &self.keys {
            hashes.push(map.row_hash(&values, derive)?);
        }
        let tid = kept.unwrap_or_else(|| {
            self.last_tid += 1;
            self.last_tid
        });
        for (map, hash) in self.keys.iter_mut().zip(hashes) {
            if let Some(hash) = hash {
                map.hashes.entry(hash).or_default().push(tid);
            }
        }
        let values = values.into_boxed_slice();
        let version = Version { tid, xmin, cmin, xmax: 0, cmax: 0, next: 0, values };
        if self.versions.last().is_none_or(|last| last.tid < tid) {
            self.versions.push(version);
        } else {
            let at = self.versions.partition_point(|other| other.tid < tid);
            self.versions.insert(at, version);
        }
        Ok(tid)
    }

    /// The place of the version `tid`.
    fn find(&self, tid: u64) -> Option<usize> {
        self.versions.binary_search_by_key(&tid, |version| version.tid).ok()
    }

    /// The place in `keys` of the map of each unique key. The first write that checks a key makes its map from all the versions.
    fn key_maps(&mut self, unique: &[Unique], derive: Derive<'_>) -> Result<Vec<usize>> {
        let mut places = Vec::with_capacity(unique.len());
        for key in unique {
            let place = match self.keys.iter().position(|map| map.is(key)) {
                Some(place) => place,
                None => {
                    let mut map = KeyMap::of(key);
                    for version in &self.versions {
                        if let Some(hash) = map.row_hash(&version.values, derive)? {
                            map.hashes.entry(hash).or_default().push(version.tid);
                        }
                    }
                    self.keys.push(map);
                    self.keys.len() - 1
                }
            };
            places.push(place);
        }
        Ok(places)
    }

    /// `_bt_check_unique` of the new version `tid` for each unique key, in the order of the indexes. The result is the first conflict: a live version with the same key, or a transaction in progress that the write must wait for. `used` has the IDs of the new versions of the write up to `tid`. A conflict in a deferrable index is no error: the place of the index and `tid` go to `partial`, for the check after the statement.
    fn check_keys(
        &self,
        (places, unique): (&[usize], &[Unique]),
        used: &[u64],
        (xid, running): (u32, &BTreeSet<u32>),
        derive: Derive<'_>,
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
            let Some(values) = map.key(row, derive)? else {
                continue;
            };
            let Some(hash) = map.hash(&values) else {
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
                match map.key(&version.values, derive)? {
                    Some(other) if map.same(&values, &other)? => {}
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

    /// Takes back the new versions `used` and the deletes of the versions `deleted`, after a write that must wait or that failed. The IDs stay used: after a wait the write makes its versions again with the same IDs, as the versions of PostgreSQL stay in place while it waits, and after a failure the versions of PostgreSQL stay in the heap until the end of the transaction.
    fn undo(&mut self, used: &[u64], deleted: &[u64]) {
        if let Some(&first) = used.first() {
            let at = self.versions.partition_point(|version| version.tid < first);
            let tail = self.versions.split_off(at);
            self.versions.extend(tail.into_iter().filter(|v| used.binary_search(&v.tid).is_err()));
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
}

/// One version of a row, `HeapTupleHeaderData`.
#[derive(Debug)]
struct Version {
    /// The ID of the version in its table, from 1, which takes the place of the item pointer of the version. It does not change while the version lives.
    tid: u64,
    /// The transaction that inserted the version.
    xmin: u32,
    /// The command of `xmin` that inserted the version.
    cmin: u32,
    /// The transaction that deleted the version, or 0.
    xmax: u32,
    /// The command of `xmax` that deleted the version.
    cmax: u32,
    /// `t_ctid`: the ID of the version that an `UPDATE` made from this version, or 0.
    next: u64,
    values: Box<[Value]>,
}

/// `SnapshotData` of an MVCC snapshot: the transactions whose changes a statement does not see.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Snapshot {
    /// All the transactions from this ID are not visible.
    xmax: u32,
    /// The transactions before `xmax` that were in progress.
    running: BTreeSet<u32>,
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
}

/// The result of [`Heap::speculative`] for one row of `INSERT ... ON CONFLICT`.
#[derive(Debug, PartialEq)]
pub(crate) enum Speculative {
    /// The row went in.
    Inserted,
    /// A live version has the key of the row in an arbiter index: the ID of the version and its values. `own` is true when the transaction of the view made the version, `command` is true when the command of the view made it, and `visible` is true when the snapshot of the view sees it.
    Conflict { tid: u64, values: Vec<Value>, own: bool, command: bool, visible: bool },
    /// The transaction `holder` is in progress and inserted or deleted the version `tid` with the key of the row in an arbiter index. The statement must wait for its end and check the row again.
    Wait { holder: u32, tid: u64 },
    /// A conflict in a unique index that is not an arbiter, as [`Heap::insert`] gives it.
    Key(Changed),
}

/// The text of the item pointer of a version, `(page,item)`, as `tidout` gives it.
pub(crate) fn tid_text(tid: u64) -> String {
    let at = tid.saturating_sub(1);
    format!("({},{})", at / TUPLES_PER_PAGE, at % TUPLES_PER_PAGE + 1)
}

impl Heap {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `GetNewTransactionId`: a transaction ID for a transaction of the session `pid` that writes. The transaction is in progress until [`Heap::commit`] or [`Heap::abort`].
    pub(crate) fn begin(&self, pid: i32) -> u32 {
        let mut state = self.state();
        let xid = state.next_xid.max(FIRST_NORMAL_XID);
        state.next_xid = xid + 1;
        state.running.insert(xid);
        state.owners.insert(xid, pid);
        xid
    }

    /// `GetSnapshotData`: the transactions whose changes are not visible now.
    pub(crate) fn snapshot(&self) -> Snapshot {
        let state = self.state();
        Snapshot { xmax: state.next_xid.max(FIRST_NORMAL_XID), running: state.running.clone() }
    }

    /// The rows of a table that the view sees, `HeapTupleSatisfiesMVCC`. Each row ends with the ID of its version.
    pub(crate) fn rows(&self, relation: u32, view: &View) -> Vec<Vec<Value>> {
        let state = self.state();
        let Some(table) = state.tables.get(&(view.database, relation)) else {
            return Vec::new();
        };
        table
            .versions
            .iter()
            .filter(|version| visible(version, view))
            .map(|version| {
                let mut row = Vec::with_capacity(version.values.len() + 1);
                row.extend_from_slice(&version.values);
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

    /// For each version of a table that the command `command` of the transaction `xid` added, in the order of their IDs, true if the transaction did not delete or update it after, as `SnapshotSelf` sees it.
    pub(crate) fn live(&self, database: u32, relation: u32, xid: u32, command: u32) -> Vec<bool> {
        let state = self.state();
        let Some(table) = state.tables.get(&(database, relation)) else {
            return Vec::new();
        };
        table
            .versions
            .iter()
            .filter(|version| version.xmin == xid && version.cmin == command)
            .map(|version| version.xmax != xid)
            .collect()
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
        (unique, derive): (&[Unique], Derive<'_>),
        kept: &[u64],
    ) -> Result<Changed> {
        let mut state = self.state();
        let State { tables, running, rechecks: pending, .. } = &mut *state;
        let table = tables.entry((view.database, relation)).or_default();
        let count = rows.len() as u64;
        let made = (view.xid, view.command);
        if unique.is_empty() {
            for values in rows {
                table.add(None, made, values, derive)?;
            }
            return Ok(Changed::Done(count));
        }
        let places = table.key_maps(unique, derive)?;
        let mut kept = kept.iter().copied();
        let mut used = Vec::with_capacity(rows.len());
        let mut partial = Vec::new();
        for values in rows {
            match table.add(kept.next(), made, values, derive) {
                Ok(tid) => used.push(tid),
                Err(error) => {
                    table.undo(&used, &[]);
                    return Err(error);
                }
            }
            let checked = (view.xid, &*running);
            match table.check_keys((&places, unique), &used, checked, derive, &mut partial) {
                Ok(None) => {}
                result => {
                    table.undo(&used, &[]);
                    return result.map(|conflict| conflict.unwrap_or(Changed::Done(0)));
                }
            }
        }
        pending.extend(rechecks(view, relation, unique, partial));
        Ok(Changed::Done(count))
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
        let mut state = self.state();
        let State { tables, running, rechecks: pending, .. } = &mut *state;
        let table = tables.entry((view.database, relation)).or_default();
        let places = table.key_maps(unique, derive)?;
        for &arbiter in arbiters {
            let Some(&place) = places.get(arbiter) else { continue };
            let map = &table.keys[place];
            let Some(places) = map.key(&values, derive)? else {
                continue;
            };
            let Some(hash) = map.hash(&places) else {
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
                match map.key(&version.values, derive)? {
                    Some(found) if map.same(&places, &found)? => {}
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
        let used = [table.add(kept, (view.xid, view.command), values, derive)?];
        let mut partial = Vec::new();
        let checked = (view.xid, &*running);
        match table.check_keys((&places, unique), &used, checked, derive, &mut partial) {
            Ok(None) => {
                pending.extend(rechecks(view, relation, unique, partial));
                Ok(Speculative::Inserted)
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
        let mut state = self.state();
        let State { tables, running, rechecks: pending, .. } = &mut *state;
        let Some(table) = tables.get_mut(&(view.database, relation)) else {
            return Ok(Changed::Done(0));
        };
        let mut found = Vec::with_capacity(changes.len());
        let mut indexes = Vec::with_capacity(changes.len());
        for (index, (tid, values)) in changes.into_iter().enumerate() {
            let Ok(at) = table.versions.binary_search_by_key(&tid, |version| version.tid) else {
                continue;
            };
            let version = &table.versions[at];
            match version.xmax {
                0 => {}
                xmax if xmax == view.xid => continue,
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
            if found.iter().any(|&(old, _)| old == tid) {
                continue;
            }
            found.push((tid, values));
            indexes.push(index);
        }
        let count = found.len() as u64;
        let places = table.key_maps(unique, derive)?;
        let mut kept = kept.iter().copied();
        let mut used = Vec::new();
        let mut deleted = Vec::with_capacity(found.len());
        let mut partial = Vec::new();
        for (old, values) in found {
            let next =
                match values.map(|v| table.add(kept.next(), (view.xid, view.command), v, derive)) {
                    Some(Ok(tid)) => tid,
                    Some(Err(error)) => {
                        table.undo(&used, &deleted);
                        return Err(error);
                    }
                    None => 0,
                };
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
            match table.check_keys((&places, unique), &used, checked, derive, &mut partial) {
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
        let State { tables, running, .. } = &mut *state;
        let table = tables.get_mut(&(view.database, relation))?;
        let other = |xid: u32| xid != 0 && xid != view.xid && running.contains(&xid);
        for version in &table.versions {
            if let Some(holder) = [version.xmin, version.xmax].into_iter().find(|&x| other(x)) {
                return Some(holder);
            }
        }
        for version in table.versions.iter_mut().filter(|version| version.xmax == 0) {
            version.xmax = view.xid;
            version.cmax = view.command;
        }
        None
    }

    /// Records that `xid` waits for `holder`, as `LockAcquire` adds an edge to the wait graph.
    pub(crate) fn wait(&self, xid: u32, holder: u32) {
        self.state().waits.insert(xid, holder);
    }

    /// The end of the wait of `xid`.
    pub(crate) fn waited(&self, xid: u32) {
        self.state().waits.remove(&xid);
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
        state.running.remove(&xid);
        state.owners.remove(&xid);
        state.waits.remove(&xid);
        state.rechecks.retain(|recheck| recheck.xid != xid);
    }

    /// `RecordTransactionAbort`: the rows that the transaction inserted go, and the rows that it deleted stay.
    pub(crate) fn abort(&self, xid: u32) {
        let mut state = self.state();
        for table in state.tables.values_mut() {
            table.versions.retain(|version| version.xmin != xid);
            for version in table.versions.iter_mut().filter(|version| version.xmax == xid) {
                version.xmax = 0;
                version.cmax = 0;
                version.next = 0;
            }
        }
        state.running.remove(&xid);
        state.owners.remove(&xid);
        state.waits.remove(&xid);
        state.rechecks.retain(|recheck| recheck.xid != xid);
    }

    /// `AbortSubTransaction` for the rows: the versions that the commands of `xid` from `command` on inserted go, and the versions that they deleted stay. A transaction has no transaction IDs for its subtransactions, so the command of each change tells the changes of the subtransaction from the changes before it.
    pub(crate) fn rollback_to(&self, xid: u32, command: u32) {
        let mut state = self.state();
        state.rechecks.retain(|recheck| recheck.xid != xid || recheck.command < command);
        for table in state.tables.values_mut() {
            table.versions.retain(|version| version.xmin != xid || version.cmin < command);
            let deleted = |version: &&mut Version| version.xmax == xid && version.cmax >= command;
            for version in table.versions.iter_mut().filter(deleted) {
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
        let mut state = self.state();
        let State { tables, running, .. } = &mut *state;
        let Some(table) = tables.get_mut(&(recheck.database, recheck.table)) else {
            return Ok(None);
        };
        let unique = [Unique { deferrable: None, ..recheck.unique.clone() }];
        let places = table.key_maps(&unique, derive)?;
        let live = table.find(recheck.tid).map(|at| dirty(&table.versions[at], xid, running));
        if live.is_none_or(|seen| seen == Seen::Dead) {
            return Ok(None);
        }
        let checked = (xid, &*running);
        table.check_keys((&places, &unique), &[recheck.tid], checked, derive, &mut Vec::new())
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
        assert_eq!(tid_text(292), "(1,1)");
    }
}
