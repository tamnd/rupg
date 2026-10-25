//! The triggers of the foreign keys after a write, as `ri_triggers.c` makes them: the checks of `RI_FKey_check_ins` and `RI_FKey_check_upd`, the checks of `NO ACTION` and `RESTRICT`, and the actions of `CASCADE`, `SET NULL` and `SET DEFAULT`.
//!
//! PostgreSQL runs a query for each row that the statement wrote. rupg reads the other table one time for each foreign key and keeps its keys in a map by their hash. The checks come after the last row of the statement and see all the rows of the statement. In `READ COMMITTED` they also see the rows that other transactions committed before the check. The row locks of `FOR KEY SHARE` are not made yet, so a check does not wait for a write of another transaction that is in progress.
//!
//! An action runs its statement on the referencing table for each old key, as `ri_PerformCheck` runs it with SPI. SPI does not fire the triggers at the end of that statement, so its rows go to the end of the queue of the statement of the client, and the triggers fire in the order of `afterTriggerInvokeEvents`.

use std::collections::{HashMap, VecDeque};
use std::hash::{DefaultHasher, Hasher};

use rupg_analyze::{Action, Foreign, Params, Write, WriteKind};
use rupg_catalog::RelKind;
use rupg_common::{Error, SqlState};
use rupg_exec::Plan;
use rupg_sql::nodes::Node;
use rupg_types::{Numeric, Value, oid};

use super::deferred::Modes;
use super::{Applied, Connection};
use crate::heap::hash_value;
use crate::query;

/// The rows of a write of a table with foreign keys, for the triggers of the foreign keys, `AfterTriggerEventList`.
#[derive(Clone, Debug)]
pub(super) struct Batch {
    /// The OID of the table.
    table: u32,
    /// The name of the table, for the errors.
    name: String,
    /// The foreign keys that the write checks, in the order of the names of their triggers.
    foreign: Vec<Foreign>,
    /// The command of the write.
    command: u32,
    /// The new rows of an `INSERT` or an `UPDATE`, in the order of the write.
    new: Vec<Vec<Value>>,
    /// The old rows of an `UPDATE` or a `DELETE`, in the order of the write.
    old: Vec<Vec<Value>>,
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
        }
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
    plan: Option<Plan>,
}

/// The keys with no null of the rows of a table for one foreign key, as values of the types of the referenced key, by their hash.
struct Keys {
    keys: Vec<Vec<Value>>,
    hashes: HashMap<u64, Vec<usize>>,
}

impl Keys {
    /// The keys of the columns `columns` of the rows, with the values of the types `from` as values of the types `to`.
    fn of(rows: Vec<Vec<Value>>, columns: &[usize], from: &[u32], to: &[u32]) -> Keys {
        let mut keys = Keys { keys: Vec::with_capacity(rows.len()), hashes: HashMap::new() };
        for row in rows {
            let key: Option<Vec<Value>> = columns
                .iter()
                .zip(from.iter().zip(to))
                .map(|(&column, (&from, &to))| {
                    row.get(column).filter(|value| !value.is_null()).map(|v| key_value(v, from, to))
                })
                .collect();
            let Some(key) = key else { continue };
            keys.hashes.entry(hash_key(&key, to)).or_default().push(keys.keys.len());
            keys.keys.push(key);
        }
        keys
    }

    /// True if a row has the key, with the equality of the types `types`.
    fn has(&self, key: &[Value], types: &[u32]) -> Result<bool, Error> {
        for &at in self.hashes.get(&hash_key(key, types)).into_iter().flatten() {
            if same(&self.keys[at], key, types)? {
                return Ok(true);
            }
        }
        Ok(false)
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
        let mut queue = VecDeque::from(batches);
        while let Some(batch) = queue.pop_front() {
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
                    self.check_key(batch, foreign, &key, batch.old.get(at), state)?;
                    continue;
                }
                // `RI_FKey_pk_upd_check_required`: an old key with a null has no reference, and an update that keeps the key needs no trigger.
                if key.iter().any(|value| value.is_null()) {
                    continue;
                }
                let after = batch.new.get(at);
                if let Some(after) = after
                    && foreign
                        .columns
                        .iter()
                        .zip(&key)
                        .all(|(&column, &value)| after.get(column) == Some(value))
                {
                    continue;
                }
                if matches!(foreign.action, Action::NoAction | Action::Restrict) {
                    self.check_references(batch, foreign, &key, state)?;
                    continue;
                }
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

    /// `RI_FKey_check`: the referenced table must have the new key of a row of the referencing table. `before` is the old row of an `UPDATE`.
    fn check_key(
        &self,
        batch: &Batch,
        foreign: &Foreign,
        key: &[&Value],
        before: Option<&Vec<Value>>,
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
        if let Some(before) = before {
            let mut unchanged = true;
            for ((&column, value), &ty) in
                foreign.columns.iter().zip(key).zip(&foreign.referencing_types)
            {
                let was = before.get(column).unwrap_or(&Value::Null);
                if was.is_null() || !rupg_func::equal(ty, was, ty, value)? {
                    unchanged = false;
                    break;
                }
            }
            if unchanged {
                return Ok(());
            }
        }
        let probe: Vec<Value> = key
            .iter()
            .zip(foreign.referencing_types.iter().zip(&foreign.types))
            .map(|(value, (&from, &to))| key_value(value, from, to))
            .collect();
        let keys = state.other.get_or_insert_with(|| {
            // The check sees the rows of the statement, as the command counter goes up before the triggers fire.
            let rows = self.table_rows(foreign.other);
            Keys::of(rows, &foreign.other_columns, &foreign.types, &foreign.types)
        });
        if !keys.has(&probe, &foreign.types)? {
            let shown = self.key_text(foreign, key, &foreign.referencing_types)?;
            return Err(violation(batch, foreign, &self.reader()).with_detail(format!(
                "Key {shown} is not present in table \"{}\".",
                foreign.other_name
            )));
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
        // `ri_Check_Pk_Match`: for `NO ACTION`, another row with the old key keeps the references.
        if !restrict {
            let keys = state.own.get_or_insert_with(|| {
                let rows = self.table_rows(batch.table);
                Keys::of(rows, &foreign.columns, &foreign.types, &foreign.types)
            });
            if keys.has(&probe, &foreign.types)? {
                return Ok(());
            }
        }
        let keys = state.other.get_or_insert_with(|| {
            let rows = self.table_rows(foreign.other);
            Keys::of(rows, &foreign.other_columns, &foreign.referencing_types, &foreign.types)
        });
        if keys.has(&probe, &foreign.types)? {
            let shown = self.key_text(foreign, key, &foreign.types)?;
            let still = if restrict { "" } else { "still " };
            return Err(violation(batch, foreign, &self.reader()).with_detail(format!(
                "Key {shown} is {still}referenced from table \"{}\".",
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
            let Some(name) = self.catalog().relation(partition).map(|r| r.name.clone()) else {
                continue;
            };
            let foreign = rupg_analyze::foreign_keys(&reader, partition, WriteKind::Delete)?;
            for foreign in foreign.iter().filter(|f| !f.referencing && f.cloned) {
                let others = Keys::of(
                    self.table_rows(foreign.other),
                    &foreign.other_columns,
                    &foreign.referencing_types,
                    &foreign.types,
                );
                for row in self.table_rows(partition) {
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
                    let shown = self.key_text(foreign, &key, &foreign.types)?;
                    let error = Error::new(
                        SqlState::FOREIGN_KEY_VIOLATION,
                        format!(
                            "removing partition \"{name}\" violates foreign key constraint \"{}\"",
                            foreign.name
                        ),
                    )
                    .with_detail(format!(
                        "Key {shown} is still referenced from table \"{}\".",
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

    /// The rows of the table `table` that a check of a foreign key reads. A partitioned table has the rows of its leaf partitions, with the columns of the table matched by name, as the scan of the query of `ri_PerformCheck` gives them.
    fn table_rows(&self, table: u32) -> Vec<Vec<Value>> {
        let heap = self.store.heap();
        let view = self.view();
        let catalog = self.catalog();
        let Some(rel) = catalog.relation(table).filter(|r| r.kind == RelKind::Partitioned) else {
            return heap.rows(table, &view);
        };
        let mut rows = Vec::new();
        let mut pending: Vec<u32> = catalog.partitions(table).iter().map(|p| p.oid).collect();
        while let Some(oid) = pending.pop() {
            let Some(part) = catalog.relation(oid) else { continue };
            if part.kind == RelKind::Partitioned {
                pending.extend(catalog.partitions(oid).iter().map(|p| p.oid));
                continue;
            }
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
            rows.extend(heap.rows(oid, &view).into_iter().map(|row| {
                places
                    .iter()
                    .map(|at| at.and_then(|at| row.get(at).cloned()).unwrap_or(Value::Null))
                    .collect::<Vec<Value>>()
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

    /// The key of the detail of `ri_ReportViolation`: `(a, b)=(1, x)`, with the names of the columns as they are and the values of their output functions.
    fn key_text(&self, foreign: &Foreign, key: &[&Value], types: &[u32]) -> Result<String, Error> {
        let reader = self.reader();
        let mut values = String::new();
        for (value, &ty) in key.iter().zip(types) {
            if !values.is_empty() {
                values.push_str(", ");
            }
            if value.is_null() {
                values.push_str("null");
            } else {
                let mut out = Vec::new();
                rupg_func::output(ty, value, &reader, &mut out)?;
                values.push_str(&String::from_utf8_lossy(&out));
            }
        }
        Ok(format!("({})=({values})", foreign.names.join(", ")))
    }
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
