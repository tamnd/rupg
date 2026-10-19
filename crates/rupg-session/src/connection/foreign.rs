//! The checks of the foreign keys after a write, as the triggers `RI_FKey_check_ins`, `RI_FKey_check_upd`, `RI_FKey_noaction_del`, `RI_FKey_noaction_upd`, `RI_FKey_restrict_del` and `RI_FKey_restrict_upd` of `ri_triggers.c` make them.
//!
//! PostgreSQL runs a query for each row that the statement wrote. rupg reads the other table one time for each foreign key and keeps its keys in a map by their hash. The checks come after the last row of the statement and see all the rows of the statement. In `READ COMMITTED` they also see the rows that other transactions committed before the check. The row locks of `FOR KEY SHARE` are not made yet, so a check does not wait for a write of another transaction that is in progress.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hasher};

use rupg_analyze::{Foreign, Write};
use rupg_common::{Error, SqlState};
use rupg_types::{Numeric, Value, oid};

use super::Connection;
use crate::heap::{View, hash_value};

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
    /// The checks of the foreign keys of a write, after the last row of the statement. `new` has the new rows of an `INSERT` or an `UPDATE`, and `old` has the old rows of an `UPDATE` or a `DELETE`, in the order of the write. The checks go row by row, and for each row in the order of the list of the write, as `afterTriggerInvokeEvents` fires the triggers.
    ///
    /// # Errors
    ///
    /// `23503` for a key that the referenced table does not have, for a key of `MATCH FULL` with nulls and values, and for an old key that a row of the referencing table still has.
    pub(super) fn check_foreign(
        &self,
        info: &Write,
        view: &View,
        new: &[Vec<Value>],
        old: &[Vec<Value>],
    ) -> Result<(), Error> {
        let heap = self.store.heap();
        // The check sees the rows of the statement, as the command counter goes up before the triggers fire.
        let check = View { xid: view.xid, command: view.command + 1, ..self.view() };
        let mut others: Vec<Option<Keys>> = info.foreign.iter().map(|_| None).collect();
        let mut owns: Vec<Option<Keys>> = info.foreign.iter().map(|_| None).collect();
        for at in 0..new.len().max(old.len()) {
            for (index, foreign) in info.foreign.iter().enumerate() {
                let rows = if foreign.referencing { new } else { old };
                let Some(row) = rows.get(at) else { continue };
                let key: Vec<&Value> = foreign
                    .columns
                    .iter()
                    .map(|&column| row.get(column).unwrap_or(&Value::Null))
                    .collect();
                if foreign.referencing {
                    // `RI_FKey_fk_upd_check_required` and `ri_NullCheck`.
                    let nulls = key.iter().filter(|value| value.is_null()).count();
                    if nulls == key.len() {
                        continue;
                    }
                    if nulls > 0 {
                        if foreign.full {
                            return Err(violation(info, foreign).with_detail(
                                "MATCH FULL does not allow mixing of null and nonnull key values.",
                            ));
                        }
                        continue;
                    }
                    if let Some(before) = old.get(at) {
                        let mut unchanged = true;
                        for ((&column, value), &ty) in
                            foreign.columns.iter().zip(&key).zip(&foreign.referencing_types)
                        {
                            let was = before.get(column).unwrap_or(&Value::Null);
                            if was.is_null() || !rupg_func::equal(ty, was, ty, value)? {
                                unchanged = false;
                                break;
                            }
                        }
                        if unchanged {
                            continue;
                        }
                    }
                    let probe: Vec<Value> = key
                        .iter()
                        .zip(foreign.referencing_types.iter().zip(&foreign.types))
                        .map(|(value, (&from, &to))| key_value(value, from, to))
                        .collect();
                    let keys = others[index].get_or_insert_with(|| {
                        let rows = heap.rows(foreign.other, &check);
                        Keys::of(rows, &foreign.other_columns, &foreign.types, &foreign.types)
                    });
                    if !keys.has(&probe, &foreign.types)? {
                        let shown = self.key_text(foreign, &key, &foreign.referencing_types)?;
                        return Err(violation(info, foreign).with_detail(format!(
                            "Key {shown} is not present in table \"{}\".",
                            foreign.other_name
                        )));
                    }
                } else {
                    // `RI_FKey_pk_upd_check_required`: an old key with a null has no reference, and an update that keeps the key needs no check.
                    if key.iter().any(|value| value.is_null()) {
                        continue;
                    }
                    if let Some(after) = new.get(at)
                        && foreign
                            .columns
                            .iter()
                            .zip(&key)
                            .all(|(&column, &value)| after.get(column) == Some(value))
                    {
                        continue;
                    }
                    let probe: Vec<Value> = key.iter().map(|&value| value.clone()).collect();
                    // `ri_Check_Pk_Match`: for `NO ACTION`, another row with the old key keeps the references.
                    if !foreign.restrict {
                        let keys = owns[index].get_or_insert_with(|| {
                            let rows = heap.rows(info.table, &check);
                            Keys::of(rows, &foreign.columns, &foreign.types, &foreign.types)
                        });
                        if keys.has(&probe, &foreign.types)? {
                            continue;
                        }
                    }
                    let keys = others[index].get_or_insert_with(|| {
                        let rows = heap.rows(foreign.other, &check);
                        Keys::of(
                            rows,
                            &foreign.other_columns,
                            &foreign.referencing_types,
                            &foreign.types,
                        )
                    });
                    if keys.has(&probe, &foreign.types)? {
                        let shown = self.key_text(foreign, &key, &foreign.types)?;
                        let still = if foreign.restrict { "" } else { "still " };
                        return Err(violation(info, foreign).with_detail(format!(
                            "Key {shown} is {still}referenced from table \"{}\".",
                            foreign.other_name
                        )));
                    }
                }
            }
        }
        Ok(())
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

/// The error of `ri_ReportViolation` with no detail, or of `ri_restrict` for `RESTRICT`.
fn violation(info: &Write, foreign: &Foreign) -> Error {
    let message = if foreign.referencing {
        format!(
            "insert or update on table \"{}\" violates foreign key constraint \"{}\"",
            info.name, foreign.name
        )
    } else if foreign.restrict {
        let message = format!(
            "update or delete on table \"{}\" violates RESTRICT setting of foreign key constraint \"{}\" on table \"{}\"",
            info.name, foreign.name, foreign.other_name
        );
        return Error::new(SqlState::RESTRICT_VIOLATION, message);
    } else {
        format!(
            "update or delete on table \"{}\" violates foreign key constraint \"{}\" on table \"{}\"",
            info.name, foreign.name, foreign.other_name
        )
    };
    Error::new(SqlState::FOREIGN_KEY_VIOLATION, message)
}
