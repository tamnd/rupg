//! The triggers of the foreign keys after a write, as `ri_triggers.c` makes them: the checks of `RI_FKey_check_ins` and `RI_FKey_check_upd`, the checks of `NO ACTION` and `RESTRICT`, and the actions of `CASCADE`, `SET NULL` and `SET DEFAULT`.
//!
//! PostgreSQL runs a query for each row that the statement wrote. rupg reads the other table one time for each foreign key and keeps its keys in a map by their hash. The checks come after the last row of the statement and see all the rows of the statement. In `READ COMMITTED` they also see the rows that other transactions committed before the check. The check of a new key locks the referenced row with `FOR KEY SHARE`, so it waits for a transaction in progress that deletes the row or changes its key, and then reads the keys again.
//!
//! An action runs its statement on the referencing table for each old key, as `ri_PerformCheck` runs it with SPI. SPI does not fire the triggers at the end of that statement, so its rows go to the end of the queue of the statement of the client, and the triggers fire in the order of `afterTriggerInvokeEvents`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{DefaultHasher, Hasher};
use std::sync::Arc;

use rupg_analyze::{Action, Foreign, Params, Write, WriteKind};
use rupg_catalog::RelKind;
use rupg_common::{Error, SqlState};
use rupg_exec::Plan;
use rupg_sql::nodes::Node;
use rupg_types::{
    DATE_INFINITY, DATE_NEGATIVE_INFINITY, Numeric, TIMESTAMP_INFINITY,
    TIMESTAMP_NEGATIVE_INFINITY, USECS_PER_DAY, Value, oid,
};

use super::deferred::Modes;
use super::{Applied, Connection};
use crate::heap::{KEY_SHARE, RowLocked, hash_value, tid_text};
use crate::query;

/// The rows of a write of a table with foreign keys, for the triggers of the foreign keys, `AfterTriggerEventList`.
#[derive(Clone, Debug)]
pub(super) struct Batch {
    /// The OID of the table.
    table: u32,
    /// The name of the table, for the errors.
    name: String,
    /// The foreign keys that the write checks, in the order of the names of their triggers.
    pub(super) foreign: Vec<Foreign>,
    /// The command of the write.
    command: u32,
    /// The new rows of an `INSERT` or an `UPDATE`, in the order of the write.
    new: Vec<Vec<Value>>,
    /// The old rows of an `UPDATE` or a `DELETE`, in the order of the write.
    old: Vec<Vec<Value>>,
    /// For each old row of an `UPDATE`, true when the transaction of the write inserted it, as `slot_is_current_xact_tuple` tells. Empty when the write does not know.
    own: Vec<bool>,
    /// True for the rows of the validation of a foreign key, which `RI_Initial_Check` reads as the current user when it can.
    validation: bool,
}

impl Batch {
    /// The rows of the write `info` of the command `command`, with the foreign keys `foreign` of the event of the rows.
    pub(super) fn new(
        info: &Write,
        foreign: &[Foreign],
        command: u32,
        new: Vec<Vec<Value>>,
        old: Vec<Vec<Value>>,
    ) -> Batch {
        Batch {
            table: info.table,
            name: info.name.clone(),
            foreign: foreign.to_vec(),
            command,
            new,
            old,
            own: Vec::new(),
            validation: false,
        }
    }

    /// The OID of the table of the write.
    pub(super) fn table(&self) -> u32 {
        self.table
    }

    /// The batch of the validation of a foreign key.
    pub(super) fn validating(self) -> Batch {
        Batch { validation: true, ..self }
    }

    /// The batch with `own` for its old rows.
    pub(super) fn with_own(self, own: Vec<bool>) -> Batch {
        Batch { own, ..self }
    }

    /// The batch of the triggers that fire now and the batch of the triggers that `modes` defers to the commit, as `afterTriggerCheckState` decides for each trigger.
    fn split(self, modes: &Modes) -> (Option<Batch>, Option<Batch>) {
        if !self.foreign.iter().any(|foreign| modes.deferred(foreign)) {
            return (Some(self), None);
        }
        let (later, now): (Vec<Foreign>, Vec<Foreign>) =
            self.foreign.iter().cloned().partition(|foreign| modes.deferred(foreign));
        if now.is_empty() {
            return (None, Some(self));
        }
        let deferred = Batch { foreign: later, ..self.clone() };
        (Some(Batch { foreign: now, ..self }), Some(deferred))
    }
}

/// What the triggers of one foreign key of a batch keep between the rows: the keys of the other table, the keys of the table of the batch, and the plan of the statement of the action.
#[derive(Default)]
struct State {
    other: Option<Keys>,
    own: Option<Keys>,
    /// The keys of the rows of the referencing table that the latest snapshot sees and the snapshot of the transaction does not, for [`Connection::crosscheck`].
    newer: Option<Keys>,
    plan: Option<Plan>,
    /// The plans of [`Foreign::casts`], one for each key column with a cast.
    casts: Vec<Option<Plan>>,
    /// True after the check of the privileges of the query on the other table.
    permitted: bool,
    /// True after the check of the privileges of the query of `ri_Check_Pk_Match` on the table of the batch.
    own_permitted: bool,
}

/// The keys with no null of the rows of a table for one foreign key, as values of the types of the referenced key, by the hash of the columns that the equality compares.
struct Keys {
    keys: Vec<Vec<Value>>,
    /// The table or the leaf partition of the row of each key and the ID of its version, or 0 for a row with no ID.
    places: Vec<(u32, u64)>,
    /// The keys whose versions have the lock of `FOR KEY SHARE` of the check, which the statement takes one time.
    locked: HashSet<usize>,
    hashes: HashMap<u64, Vec<usize>>,
    /// The number of the columns of the equality: all the columns, or the columns before the period of a temporal foreign key.
    equal: usize,
}

impl Keys {
    /// The keys of the columns `columns` of the rows, with the values of the types `from` as values of the types `to`. `period` is true for a temporal foreign key.
    fn of(
        rows: Vec<(u32, Vec<Value>)>,
        columns: &[usize],
        from: &[u32],
        to: &[u32],
        period: bool,
    ) -> Keys {
        let equal = columns.len() - usize::from(period);
        let mut keys = Keys {
            keys: Vec::with_capacity(rows.len()),
            places: Vec::with_capacity(rows.len()),
            locked: HashSet::new(),
            hashes: HashMap::new(),
            equal,
        };
        for (relation, row) in rows {
            let tid = match row.last() {
                Some(&Value::Int8(tid)) => tid.cast_unsigned(),
                _ => 0,
            };
            let key: Option<Vec<Value>> = columns
                .iter()
                .zip(from.iter().zip(to))
                .map(|(&column, (&from, &to))| {
                    row.get(column).filter(|value| !value.is_null()).map(|v| key_value(v, from, to))
                })
                .collect();
            let Some(key) = key else { continue };
            keys.hashes.entry(hash_key(&key[..equal], to)).or_default().push(keys.keys.len());
            keys.keys.push(key);
            keys.places.push((relation, tid));
        }
        keys
    }

    /// True if a row has the key, with the equality of the types `types`. For a temporal foreign key the other columns are equal and the periods overlap, as the operator `&&` of `conpfeqop` compares them.
    fn has(&self, key: &[Value], types: &[u32]) -> Result<bool, Error> {
        for other in self.matching(key, types)? {
            if self.equal == key.len()
                || rupg_func::period_overlaps(
                    types[self.equal],
                    &other[self.equal],
                    &key[self.equal],
                )?
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The periods of the keys of a temporal foreign key with the other columns equal to those of `key` and a period that overlaps the period of `key`.
    fn periods(&self, key: &[Value], types: &[u32]) -> Result<Vec<&Value>, Error> {
        let found = self.overlapping(key, types)?;
        Ok(found.into_iter().map(|at| &self.keys[at][self.equal]).collect())
    }

    /// The places of the keys of [`Keys::periods`].
    fn overlapping(&self, key: &[Value], types: &[u32]) -> Result<Vec<usize>, Error> {
        let (equal, ty, period) = (self.equal, types[self.equal], &key[self.equal]);
        let mut found = Vec::new();
        for &at in self.hashes.get(&hash_key(&key[..equal], types)).into_iter().flatten() {
            let other = &self.keys[at];
            if same(&other[..equal], &key[..equal], types)?
                && rupg_func::period_overlaps(ty, &other[equal], period)?
            {
                found.push(at);
            }
        }
        Ok(found)
    }

    /// The place of the first key equal to `key`, with no period, or `None`.
    fn find(&self, key: &[Value], types: &[u32]) -> Result<Option<usize>, Error> {
        for &at in self.hashes.get(&hash_key(&key[..self.equal], types)).into_iter().flatten() {
            if same(&self.keys[at], key, types)? {
                return Ok(Some(at));
            }
        }
        Ok(None)
    }

    /// The keys with the columns of the equality equal to those of `key`.
    fn matching(&self, key: &[Value], types: &[u32]) -> Result<Vec<&[Value]>, Error> {
        let equal = self.equal;
        let mut found = Vec::new();
        for &at in self.hashes.get(&hash_key(&key[..equal], types)).into_iter().flatten() {
            if same(&self.keys[at][..equal], &key[..equal], types)? {
                found.push(self.keys[at].as_slice());
            }
        }
        Ok(found)
    }
}

/// True if the two keys with no null are equal with the equality of the types `types`.
fn same(a: &[Value], b: &[Value], types: &[u32]) -> Result<bool, Error> {
    for ((x, y), &ty) in a.iter().zip(b).zip(types) {
        if !rupg_func::equal(ty, x, ty, y)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The hash of a key with no null.
fn hash_key(key: &[Value], types: &[u32]) -> u64 {
    let mut hasher = DefaultHasher::new();
    for (value, &ty) in key.iter().zip(types) {
        hash_value(value, ty, &mut hasher);
    }
    hasher.finish()
}

/// `datum_image_eq`: the two values have the same bytes. `ri_KeysEqual` compares the keys of the referenced table in this way, because an update to an equal key with other bytes, as `-0` to `0`, must go to the referencing rows.
fn same_image(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Float8(x), Value::Float8(y)) => x.to_bits() == y.to_bits(),
        (Value::Float4(x), Value::Float4(y)) => x.to_bits() == y.to_bits(),
        _ => a == b,
    }
}

/// A value of the type `from` as a value of the type `to` of the referenced column. When the two types have no equality operator, `ATAddForeignKeyConstraint` casts the referencing value to the type of the referenced column, and the check compares the values after the cast.
fn key_value(value: &Value, from: u32, to: u32) -> Value {
    let to = rupg_func::base_type(to);
    match value {
        Value::Int2(_) | Value::Int4(_) | Value::Int8(_) => match (value.as_i64(), to) {
            (Some(v), oid::NUMERIC) => Value::Numeric(Numeric::from_integer(i128::from(v))),
            (Some(v), oid::FLOAT4 | oid::FLOAT8) => Value::Float8(v as f64),
            _ => value.clone(),
        },
        Value::Numeric(v) if matches!(to, oid::FLOAT4 | oid::FLOAT8) => Value::Float8(v.to_f64()),
        // `date_eq_timestamp` and `timestamp_eq_date`: a date compares as its midnight. A timestamp that is not a midnight equals no date and stays a timestamp, which no date key equals.
        &Value::Date(date) if to == oid::TIMESTAMP => Value::Timestamp(match date {
            DATE_NEGATIVE_INFINITY => TIMESTAMP_NEGATIVE_INFINITY,
            DATE_INFINITY => TIMESTAMP_INFINITY,
            date => i64::from(date) * USECS_PER_DAY,
        }),
        &Value::Timestamp(time) if to == oid::DATE => match time {
            TIMESTAMP_NEGATIVE_INFINITY => Value::Date(DATE_NEGATIVE_INFINITY),
            TIMESTAMP_INFINITY => Value::Date(DATE_INFINITY),
            time => match i32::try_from(time / USECS_PER_DAY) {
                Ok(date) if time % USECS_PER_DAY == 0 => Value::Date(date),
                _ => value.clone(),
            },
        },
        // `rtrim1`: the cast of `bpchar` to `text` removes the spaces at the end.
        Value::Text(v) if rupg_func::base_type(from) == oid::BPCHAR && to != oid::BPCHAR => {
            Value::Text(v.trim_end_matches(' ').into())
        }
        _ => value.clone(),
    }
}

impl Connection {
    /// Fires the triggers of the foreign keys of the batches of a write and of the writes of their actions, batch after batch in the order of the queue. The rows of a trigger that is deferred go to the queue of the transaction.
    ///
    /// # Errors
    ///
    /// `23503` for a key that the referenced table does not have, for a key of `MATCH FULL` with nulls and values, and for an old key that a row of the referencing table still has. `23001` for an old key of `RESTRICT` that a row of the referencing table has. The errors of the statement of an action, with the statement in their context.
    pub(super) fn fire(&self, batches: Vec<Batch>) -> Result<(), Error> {
        let tables: Vec<u32> = batches.iter().map(|b| b.table).collect();
        self.fire_early_events(&tables)?;
        self.fire_forced(batches, 0)
    }

    /// [`Connection::fire`] where the first `forced` batches fire even when their triggers are deferred, as `AfterTriggerFireDeferred` fires the queue of the transaction.
    pub(super) fn fire_forced(&self, batches: Vec<Batch>, mut forced: usize) -> Result<(), Error> {
        let mut queue = VecDeque::from(batches);
        while let Some(batch) = queue.pop_front() {
            if forced > 0 {
                forced -= 1;
                self.fire_batch(&batch, &mut queue)?;
                continue;
            }
            let (now, later) = batch.split(&self.modes.borrow());
            if let Some(later) = later {
                self.deferred.borrow_mut().push(later);
            }
            if let Some(now) = now {
                self.fire_batch(&now, &mut queue)?;
            }
        }
        Ok(())
    }

    /// The triggers of the rows of one batch. They go row by row, and for each row in the order of the list of the write, as `afterTriggerInvokeEvents` fires them.
    fn fire_batch(&self, batch: &Batch, queue: &mut VecDeque<Batch>) -> Result<(), Error> {
        let mut states: Vec<State> = batch.foreign.iter().map(|_| State::default()).collect();
        // The new rows that are still live, after an action changed rows.
        let mut live: Option<Vec<bool>> = None;
        for at in 0..batch.new.len().max(batch.old.len()) {
            for (index, foreign) in batch.foreign.iter().enumerate() {
                let rows = if foreign.referencing { &batch.new } else { &batch.old };
                let Some(row) = rows.get(at) else { continue };
                let key: Vec<&Value> = foreign
                    .columns
                    .iter()
                    .map(|&column| row.get(column).unwrap_or(&Value::Null))
                    .collect();
                let state = &mut states[index];
                if foreign.referencing {
                    // `RI_FKey_check`: a new row that a later write deleted or updated needs no check.
                    if self.command.get() > batch.command + 1 {
                        let live = live.get_or_insert_with(|| {
                            let view = self.view();
                            self.store.heap().live(
                                view.database,
                                batch.table,
                                view.xid,
                                batch.command,
                            )
                        });
                        if live.len() == batch.new.len() && !live[at] {
                            continue;
                        }
                    }
                    let before =
                        batch.old.get(at).map(|row| (row, batch.own.get(at) == Some(&true)));
                    self.check_key(batch, foreign, &key, before, state)?;
                    continue;
                }
                // `RI_FKey_pk_upd_check_required`: an old key with a null has no reference, and an update that keeps the key needs no trigger.
                if key.iter().any(|value| value.is_null()) {
                    continue;
                }
                let after = batch.new.get(at);
                if let Some(after) = after
                    && foreign.columns.iter().zip(&key).all(|(&column, &value)| {
                        after.get(column).is_some_and(|v| same_image(v, value))
                    })
                {
                    continue;
                }
                if matches!(foreign.action, Action::NoAction | Action::Restrict) {
                    self.check_references(batch, foreign, &key, state)?;
                    continue;
                }
                self.crosscheck(foreign, &key, state)?;
                let mut params = Vec::with_capacity(key.len() * 2);
                if foreign.action == Action::Cascade
                    && let Some(after) = after
                {
                    params.extend(
                        foreign
                            .columns
                            .iter()
                            .map(|&column| after.get(column).cloned().unwrap_or(Value::Null)),
                    );
                }
                params.extend(key.iter().map(|&value| value.clone()));
                if self.act(foreign, &mut state.plan, &params, queue)? {
                    for state in &mut states {
                        state.other = None;
                        state.own = None;
                    }
                    live = None;
                }
                // `ri_set` for `SET DEFAULT`: a row that got the default can have the old key again, so the check of `NO ACTION` runs after the action.
                if foreign.action == Action::SetDefault {
                    self.check_references(batch, foreign, &key, &mut states[index])?;
                }
            }
        }
        Ok(())
    }

    /// `RI_FKey_check`: the referenced table must have the new key of a row of the referencing table. `before` is the old row of an `UPDATE`, with true when the transaction inserted that row.
    fn check_key(
        &self,
        batch: &Batch,
        foreign: &Foreign,
        key: &[&Value],
        before: Option<(&Vec<Value>, bool)>,
        state: &mut State,
    ) -> Result<(), Error> {
        // `RI_FKey_fk_upd_check_required` and `ri_NullCheck`.
        let nulls = key.iter().filter(|value| value.is_null()).count();
        if nulls == key.len() {
            return Ok(());
        }
        if nulls > 0 {
            if foreign.full {
                return Err(violation(batch, foreign, &self.reader()).with_detail(
                    "MATCH FULL does not allow mixing of null and nonnull key values.",
                ));
            }
            return Ok(());
        }
        // An old row that the transaction inserted needs the check, as the update makes the check of the insert see no row.
        if let Some((before, false)) = before {
            let mut unchanged = true;
            let last = key.len() - 1;
            for (i, ((&column, value), &ty)) in
                foreign.columns.iter().zip(key).zip(&foreign.referencing_types).enumerate()
            {
                let was = before.get(column).unwrap_or(&Value::Null);
                // `ri_KeysEqual`: a period that stays the same or gets smaller needs no check.
                let same = if foreign.period && i == last {
                    !was.is_null() && rupg_func::period_covered(ty, value, None, &[was])?
                } else {
                    !was.is_null() && rupg_func::equal(ty, was, ty, value)?
                };
                if !same {
                    unchanged = false;
                    break;
                }
            }
            if unchanged {
                return Ok(());
            }
        }
        if !state.permitted {
            let reader = self.reader();
            let pk = (foreign.other, attnums(&foreign.other_columns));
            let fk = (batch.table, attnums(&foreign.columns));
            let owner = self.owner_of(pk.0);
            if !batch.validation
                || !rupg_func::ri_initial_permitted((pk.0, &pk.1), (fk.0, &fk.1), &reader)?
            {
                rupg_func::ri_check_permissions(pk.0, &pk.1, owner, &reader)?;
                rupg_func::check_execute_as(&foreign.functions, owner, &reader).map_err(
                    |error| {
                        if foreign.check.is_empty() {
                            error
                        } else {
                            query::inner_error(error, &foreign.check)
                        }
                    },
                )?;
            } else {
                rupg_func::check_execute(&foreign.functions, &reader)?;
            }
            state.permitted = true;
        }
        let mut probe = Vec::with_capacity(key.len());
        for (at, (value, (&from, &to))) in
            key.iter().zip(foreign.referencing_types.iter().zip(&foreign.types)).enumerate()
        {
            probe.push(match foreign.casts.get(at).filter(|cast| !cast.is_empty()) {
                Some(cast) => self.cast_key(cast, from, value, at, state)?,
                None => key_value(value, from, to),
            });
        }
        let present = loop {
            let keys = state.other.get_or_insert_with(|| {
                // The check sees the rows of the statement, as the command counter goes up before the triggers fire. For a partitioned referenced table, the latest snapshot that finds the partition reads the rows too, so the check sees a row that committed after the snapshot of the transaction, as PostgreSQL does.
                let partitioned = self.is_partitioned_table(foreign.other);
                let rows = self.table_rows(foreign.other, partitioned, &foreign.other_columns);
                Keys::of(
                    rows,
                    &foreign.other_columns,
                    &foreign.types,
                    &foreign.types,
                    foreign.period,
                )
            });
            // For a temporal foreign key, `HAVING $n <@ range_agg(x1.r)`: the periods of the keys that overlap the period of the new key cover it.
            if foreign.period {
                let last = probe.len() - 1;
                let found = keys.overlapping(&probe, &foreign.types)?;
                // The subquery locks each row that it finds with `FOR KEY SHARE`, before the check of the cover.
                let mut changed = false;
                for &at in found.iter().filter(|_| !batch.validation) {
                    if keys.locked.contains(&at) {
                        continue;
                    }
                    if !self.lock_referenced(keys.places[at])? {
                        changed = true;
                        break;
                    }
                    keys.locked.insert(at);
                }
                if changed {
                    state.other = None;
                    continue;
                }
                let periods: Vec<&Value> =
                    found.iter().map(|&at| &keys.keys[at][keys.equal]).collect();
                break rupg_func::period_covered(
                    foreign.types[last],
                    &probe[last],
                    None,
                    &periods,
                )?;
            }
            let Some(at) = keys.find(&probe, &foreign.types)? else { break false };
            if batch.validation || keys.locked.contains(&at) {
                break true;
            }
            if self.lock_referenced(keys.places[at])? {
                keys.locked.insert(at);
                break true;
            }
            // A transaction that committed after the snapshot changed the row: the check reads the keys again with a new snapshot, and the latest version decides, as `TUPLE_LOCK_FLAG_FIND_LAST_VERSION` gives it.
            state.other = None;
        };
        if !present {
            let shown = self.key_text(batch, foreign, key, &foreign.referencing_types)?;
            return Err(violation(batch, foreign, &self.reader()).with_detail(format!(
                "Key{shown} is not present in table \"{}\".",
                foreign.other_name
            )));
        }
        Ok(())
    }

    /// The lock of `FOR KEY SHARE OF x` of the query of `RI_FKey_check` on the version `tid` of the referenced row in the table or leaf partition `relation`, which waits for a transaction in progress that deletes the row or changes its key. False when a transaction that committed after the snapshot changed the row. A version with no ID, as of a foreign table, takes no lock.
    ///
    /// # Errors
    ///
    /// `40001` in `REPEATABLE READ` and `SERIALIZABLE` for a row that a transaction that committed after the snapshot changed, and the errors of [`query::Reader::wait_xact`].
    fn lock_referenced(&self, (relation, tid): (u32, u64)) -> Result<bool, Error> {
        let table = self.catalog().relation(relation).map(|r| (r.kind, r.name.clone()));
        let Some((RelKind::Table, name)) = table else { return Ok(true) };
        let view = self.view();
        if tid == 0 || view.xid == 0 {
            return Ok(true);
        }
        let heap = self.store.heap();
        loop {
            match heap.lock_row((relation, tid), &view, (view.xid, KEY_SHARE))? {
                RowLocked::Done | RowLocked::Gone => return Ok(true),
                RowLocked::Again => return Ok(false),
                RowLocked::Wait(holder) => {
                    let context =
                        format!("while locking tuple {} in relation \"{name}\"", tid_text(tid));
                    let row = (view.database, relation, tid);
                    self.reader().wait_xact_while(
                        view.xid,
                        holder,
                        &context,
                        Some((row, KEY_SHARE)),
                    )?;
                }
            }
        }
    }

    /// The crosscheck snapshot of `ri_PerformCheck` for an action in `REPEATABLE READ` and `SERIALIZABLE`: the statement of the action finds its rows with the latest snapshot, and a row with the old key that the snapshot of the transaction does not see, as of a transaction that committed after it, makes `heap_update` and `heap_delete` fail, in place of a row that the action leaves with a key that is gone.
    ///
    /// # Errors
    ///
    /// `40001` for a row of the referencing table with the old key that committed after the snapshot of the transaction.
    fn crosscheck(
        &self,
        foreign: &Foreign,
        key: &[&Value],
        state: &mut State,
    ) -> Result<(), Error> {
        if !self.view().serializable {
            return Ok(());
        }
        let newer = state.newer.get_or_insert_with(|| {
            let seen: HashSet<(u32, u64)> = self
                .table_rows(foreign.other, false, &[])
                .iter()
                .filter_map(|(relation, row)| match row.last() {
                    Some(&Value::Int8(tid)) => Some((*relation, tid.cast_unsigned())),
                    _ => None,
                })
                .collect();
            let rows: Vec<(u32, Vec<Value>)> = self
                .table_rows(foreign.other, true, &foreign.other_columns)
                .into_iter()
                .filter(|(relation, row)| match row.last() {
                    Some(&Value::Int8(tid)) => !seen.contains(&(*relation, tid.cast_unsigned())),
                    _ => false,
                })
                .collect();
            Keys::of(
                rows,
                &foreign.other_columns,
                &foreign.referencing_types,
                &foreign.types,
                false,
            )
        });
        let probe: Vec<Value> = key.iter().map(|&value| value.clone()).collect();
        if newer.has(&probe, &foreign.types)? {
            return Err(Error::new(
                SqlState::T_R_SERIALIZATION_FAILURE,
                "could not serialize access due to concurrent update",
            ));
        }
        Ok(())
    }

    /// `ri_restrict`: no row of the referencing table can have an old key of the referenced table.
    fn check_references(
        &self,
        batch: &Batch,
        foreign: &Foreign,
        key: &[&Value],
        state: &mut State,
    ) -> Result<(), Error> {
        let restrict = foreign.action == Action::Restrict;
        let probe: Vec<Value> = key.iter().map(|&value| value.clone()).collect();
        if foreign.period {
            return self.check_period_references(batch, foreign, key, &probe, state);
        }
        // `ri_Check_Pk_Match`: for `NO ACTION`, another row with the old key keeps the references. Its query reads the referenced table as the owner of the table.
        if !restrict {
            if !state.own_permitted {
                let own = attnums(&foreign.columns);
                let owner = self.owner_of(batch.table);
                rupg_func::ri_check_permissions(batch.table, &own, owner, &self.reader())?;
                state.own_permitted = true;
            }
            let keys = state.own.get_or_insert_with(|| {
                let rows = self.table_rows(batch.table, false, &foreign.columns);
                Keys::of(rows, &foreign.columns, &foreign.types, &foreign.types, false)
            });
            if keys.has(&probe, &foreign.types)? {
                return Ok(());
            }
        }
        // The query of `ri_restrict` reads the referencing table as the owner of the table.
        if !state.permitted {
            let other = attnums(&foreign.other_columns);
            let owner = self.owner_of(foreign.other);
            rupg_func::ri_check_permissions(foreign.other, &other, owner, &self.reader()).map_err(
                |error| match foreign.restrict.as_str() {
                    "" => error,
                    text => query::inner_error(error, text),
                },
            )?;
            state.permitted = true;
        }
        // `ri_restrict` passes `detectNewRows`, so it sees a row of the referencing table that committed after the snapshot of the transaction.
        let keys = state.other.get_or_insert_with(|| {
            let rows = self.table_rows(foreign.other, true, &foreign.other_columns);
            Keys::of(
                rows,
                &foreign.other_columns,
                &foreign.referencing_types,
                &foreign.types,
                false,
            )
        });
        if keys.has(&probe, &foreign.types)? {
            let shown = self.key_text(batch, foreign, key, &foreign.types)?;
            let still = if restrict { "" } else { "still " };
            return Err(violation(batch, foreign, &self.reader()).with_detail(format!(
                "Key{shown} is {still}referenced from table \"{}\".",
                foreign.other_name
            )));
        }
        Ok(())
    }

    /// `ri_restrict` for `NO ACTION` of a temporal foreign key: a row of the referencing table with the other columns of the old key and a period that overlaps the old period is an error, unless the periods of the rows of the referenced table with the other columns of the old key, which overlap the old period, cover the part of its period in the old period.
    fn check_period_references(
        &self,
        batch: &Batch,
        foreign: &Foreign,
        key: &[&Value],
        probe: &[Value],
        state: &mut State,
    ) -> Result<(), Error> {
        let last = probe.len() - 1;
        let ty = foreign.types[last];
        let own = state.own.get_or_insert_with(|| {
            let rows = self.table_rows(batch.table, false, &foreign.columns);
            Keys::of(rows, &foreign.columns, &foreign.types, &foreign.types, true)
        });
        let replacements: Vec<Value> =
            own.periods(probe, &foreign.types)?.into_iter().cloned().collect();
        let replacements: Vec<&Value> = replacements.iter().collect();
        let others = state.other.get_or_insert_with(|| {
            let rows = self.table_rows(foreign.other, true, &foreign.other_columns);
            Keys::of(rows, &foreign.other_columns, &foreign.referencing_types, &foreign.types, true)
        });
        for period in others.periods(probe, &foreign.types)? {
            if rupg_func::period_covered(ty, period, Some(&probe[last]), &replacements)? {
                continue;
            }
            let shown = self.key_text(batch, foreign, key, &foreign.types)?;
            return Err(violation(batch, foreign, &self.reader()).with_detail(format!(
                "Key{shown} is still referenced from table \"{}\".",
                foreign.other_name
            )));
        }
        Ok(())
    }

    /// `RI_PartitionRemove_Check` for the partitions of `DETACH PARTITION`: no row of a referencing table can have a key of a partition, for each foreign key that refers to the partition through its parent. The foreign keys come from the catalog before the statement.
    ///
    /// # Errors
    ///
    /// `23503` for a key of the partition that a row of a referencing table has.
    pub(super) fn check_detached(&self, partitions: &[u32]) -> Result<(), Error> {
        let reader = self.reader();
        for &partition in partitions {
            let Some((name, kind)) =
                self.catalog().relation(partition).map(|r| (r.name.clone(), r.kind))
            else {
                continue;
            };
            // A partitioned table fires no triggers for a `DELETE`, so its keys come from the triggers of an `UPDATE`, which are for the same constraints.
            let write =
                if kind == RelKind::Partitioned { WriteKind::Update } else { WriteKind::Delete };
            let foreign = rupg_analyze::foreign_keys(&reader, partition, write)?;
            for foreign in foreign.iter().filter(|f| !f.referencing && f.cloned) {
                let others = Keys::of(
                    self.table_rows(foreign.other, false, &foreign.other_columns),
                    &foreign.other_columns,
                    &foreign.referencing_types,
                    &foreign.types,
                    foreign.period,
                );
                for (_, row) in self.table_rows(partition, false, &foreign.columns) {
                    let key: Vec<&Value> = foreign
                        .columns
                        .iter()
                        .map(|&column| row.get(column).unwrap_or(&Value::Null))
                        .collect();
                    if key.iter().any(|value| value.is_null()) {
                        continue;
                    }
                    let probe: Vec<Value> = key.iter().map(|&value| value.clone()).collect();
                    if !others.has(&probe, &foreign.types)? {
                        continue;
                    }
                    let shown = key_values(&key, &foreign.types, &reader)?;
                    let error = Error::new(
                        SqlState::FOREIGN_KEY_VIOLATION,
                        format!(
                            "removing partition \"{name}\" violates foreign key constraint \"{}\"",
                            foreign.name
                        ),
                    )
                    .with_detail(format!(
                        "Key ({})=({shown}) is still referenced from table \"{}\".",
                        foreign.names.join(", "),
                        foreign.other_name
                    ));
                    return Err(rupg_func::with_table_constraint(
                        error,
                        foreign.other,
                        &foreign.name,
                        &reader,
                    ));
                }
            }
        }
        Ok(())
    }

    /// True for a partitioned table.
    fn is_partitioned_table(&self, table: u32) -> bool {
        self.catalog().relation(table).is_some_and(|r| r.kind == RelKind::Partitioned)
    }

    /// The rows of the table `table` that a check of a foreign key reads, with the latest snapshot when `latest` is true, each with the table or leaf partition that has it and ending with the ID of its version. A partitioned table has the rows of its leaf partitions, with the columns of the table matched by name, as the scan of the query of `ri_PerformCheck` gives them. The rows of a table that is not partitioned have the values of only the columns `columns`, at their places, so that a check reads only the key columns.
    fn table_rows(&self, table: u32, latest: bool, columns: &[usize]) -> Vec<(u32, Vec<Value>)> {
        let heap = self.store.heap();
        let mut view = self.view();
        // `ri_PerformCheck` with `detectNewRows` in `REPEATABLE READ` and `SERIALIZABLE`: the query reads the rows with a new snapshot, as `GetLatestSnapshot` gives it. The session holds the older `xmin` of the snapshot of the transaction, which keeps the versions of the new snapshot.
        if latest {
            view.snapshot = heap.held_snapshot_in(&self.xmin_slot());
        }
        let catalog = self.catalog();
        // `ri_PerformCheck` with `detectNewRows` for a partitioned table: the partition directory of the query leaves out each partition with a pending detach that committed, as the latest snapshot sees it.
        let catalog = match catalog.omit_detached(|_| true) {
            Some(omitted) => Arc::new(omitted),
            None => catalog,
        };
        let Some(rel) = catalog.relation(table).filter(|r| r.kind == RelKind::Partitioned) else {
            let mut read = columns.to_vec();
            read.sort_unstable();
            read.dedup();
            return heap
                .sparse_rows(table, &view, &read)
                .into_iter()
                .map(|row| (table, row))
                .collect();
        };
        let mut rows = Vec::new();
        // The leaves in the order of a scan, or in the order of their OIDs when a compare of the bounds fails.
        let reader = self.reader_without_view(Arc::clone(&catalog));
        let leaves = rupg_exec::partition_leaves(table, &reader).unwrap_or_else(|_| {
            let mut leaves = Vec::new();
            let mut pending: Vec<u32> = catalog.partitions(table).iter().map(|p| p.oid).collect();
            while let Some(oid) = pending.pop() {
                if catalog.relation(oid).is_some_and(|r| r.kind == RelKind::Partitioned) {
                    pending.extend(catalog.partitions(oid).iter().map(|p| p.oid));
                } else {
                    leaves.push(oid);
                }
            }
            leaves.sort_unstable();
            leaves
        });
        for oid in leaves {
            let Some(part) = catalog.relation(oid) else { continue };
            let places: Vec<Option<usize>> = rel
                .columns
                .iter()
                .map(|c| {
                    if c.dropped {
                        return None;
                    }
                    part.columns.iter().position(|p| !p.dropped && p.name == c.name)
                })
                .collect();
            // The row ends with the ID of the version of the leaf, as a row of a table does, for the lock of the check.
            rows.extend(heap.rows(oid, &view).into_iter().map(|row| {
                let mut values: Vec<Value> = places
                    .iter()
                    .map(|at| at.and_then(|at| row.get(at).cloned()).unwrap_or(Value::Null))
                    .collect();
                values.extend(row.last().cloned());
                (oid, values)
            }));
        }
        rows
    }

    /// The action of a foreign key for an old key: its statement on the referencing table with the parameters `params`. The rows of the statement go to the end of the queue. True if the statement changed rows.
    fn act(
        &self,
        foreign: &Foreign,
        plan: &mut Option<Plan>,
        params: &[Value],
        queue: &mut VecDeque<Batch>,
    ) -> Result<bool, Error> {
        let applied = self
            .run_action(foreign, plan, params)
            .map_err(|error| query::inner_error(error, &foreign.query))?;
        queue.extend(applied.batches);
        Ok(applied.count > 0)
    }

    /// `ri_PlanCheck` and `ri_PerformCheck`: plans the statement of an action one time for each batch, and runs it.
    fn run_action(
        &self,
        foreign: &Foreign,
        plan: &mut Option<Plan>,
        params: &[Value],
    ) -> Result<Applied, Error> {
        let reader = self.reader();
        if plan.is_none() {
            let (list, _) = rupg_sql::parse(&foreign.query)?;
            let node = list.iter().flatten().find_map(|node| match node {
                Node::RawStmt(raw) => raw.stmt.as_ref(),
                _ => None,
            });
            let Some(node) = node else {
                return Err(Error::internal("an action of a foreign key with no statement"));
            };
            let types = foreign.types.iter().cycle().take(params.len()).copied().collect();
            let planned = query::plan_internal(node, &reader, &Params { types, variable: false })?;
            // The action runs as the owner of the table, as the queries of `ri_triggers.c` do.
            *plan = Some(planned.fold(None, &reader)?);
        }
        let Some(plan) = plan.as_ref() else {
            return Err(Error::internal("an action of a foreign key with no plan"));
        };
        let rows = plan.run(params, &reader)?;
        let view = reader.view().cloned().unwrap_or_default();
        drop(reader);
        self.apply_write(plan, params, &view, rows)
    }

    /// `ri_add_cast_to`: the value `value` of the type `from` after the cast of the query `cast`, which calls a function of the user. The plan is made one time for each batch.
    fn cast_key(
        &self,
        cast: &str,
        from: u32,
        value: &Value,
        at: usize,
        state: &mut State,
    ) -> Result<Value, Error> {
        if state.casts.len() <= at {
            state.casts.resize_with(at + 1, || None);
        }
        let reader = self.reader();
        if state.casts[at].is_none() {
            let (list, _) = rupg_sql::parse(cast)?;
            let node = list.iter().flatten().find_map(|node| match node {
                Node::RawStmt(raw) => raw.stmt.as_ref(),
                _ => None,
            });
            let Some(node) = node else {
                return Err(Error::internal("a cast of a foreign key with no statement"));
            };
            let params = Params { types: vec![from], variable: false };
            state.casts[at] =
                Some(query::plan_internal(node, &reader, &params)?.fold(None, &reader)?);
        }
        let Some(plan) = state.casts[at].as_ref() else {
            return Err(Error::internal("a cast of a foreign key with no plan"));
        };
        let rows = plan.run(std::slice::from_ref(value), &reader)?;
        Ok(rows.into_iter().next().and_then(|row| row.into_iter().next()).unwrap_or(Value::Null))
    }

    /// The owner of the table `table`, as whom `ri_PerformCheck` runs the query on the table.
    fn owner_of(&self, table: u32) -> u32 {
        self.catalog().relation(table).map_or(0, |rel| rel.owner)
    }

    /// `ri_ReportViolation`: the key of the row of `batch` with its columns, as ` (a, b)=(1, 2)`, or nothing when the current user may not see it, as the policies of the table or the privileges of the user hide it.
    fn key_text(
        &self,
        batch: &Batch,
        foreign: &Foreign,
        key: &[&Value],
        types: &[u32],
    ) -> Result<String, Error> {
        let reader = self.reader();
        let attrs: Vec<i16> =
            foreign.columns.iter().map(|&c| i16::try_from(c + 1).unwrap_or(0)).collect();
        if !rupg_func::key_shown(batch.table, &attrs, &reader)? {
            return Ok(String::new());
        }
        let values = key_values(key, types, &reader)?;
        Ok(format!(" ({})=({values})", foreign.names.join(", ")))
    }
}

/// The numbers of the columns at the places `columns`.
fn attnums(columns: &[usize]) -> Vec<i16> {
    columns.iter().map(|&c| i16::try_from(c + 1).unwrap_or(0)).collect()
}

/// The values of a key as the detail of an error shows them, with `null` for a null value.
fn key_values(
    key: &[&Value],
    types: &[u32],
    session: &dyn rupg_func::Session,
) -> Result<String, Error> {
    let mut values = String::new();
    for (value, &ty) in key.iter().zip(types) {
        if !values.is_empty() {
            values.push_str(", ");
        }
        if value.is_null() {
            values.push_str("null");
        } else {
            let mut out = Vec::new();
            rupg_func::output(ty, value, session, &mut out)?;
            values.push_str(&String::from_utf8_lossy(&out));
        }
    }
    Ok(values)
}

/// The error of `ri_ReportViolation` with no detail, or of `ri_restrict` for `RESTRICT`. The object of the error is the constraint on the referencing table.
fn violation(batch: &Batch, foreign: &Foreign, session: &dyn rupg_func::Session) -> Error {
    let referencing = if foreign.referencing { batch.table } else { foreign.other };
    let (state, message) = if foreign.referencing {
        (
            SqlState::FOREIGN_KEY_VIOLATION,
            format!(
                "insert or update on table \"{}\" violates foreign key constraint \"{}\"",
                batch.name, foreign.name
            ),
        )
    } else if foreign.action == Action::Restrict {
        (
            SqlState::RESTRICT_VIOLATION,
            format!(
                "update or delete on table \"{}\" violates RESTRICT setting of foreign key constraint \"{}\" on table \"{}\"",
                batch.name, foreign.name, foreign.other_name
            ),
        )
    } else {
        (
            SqlState::FOREIGN_KEY_VIOLATION,
            format!(
                "update or delete on table \"{}\" violates foreign key constraint \"{}\" on table \"{}\"",
                batch.name, foreign.name, foreign.other_name
            ),
        )
    };
    rupg_func::with_table_constraint(
        Error::new(state, message),
        referencing,
        &foreign.name,
        session,
    )
}
