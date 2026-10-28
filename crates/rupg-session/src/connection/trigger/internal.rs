//! The trigger functions in C of PostgreSQL that a trigger can call: `tsvector_update_trigger_byid` and `tsvector_update_trigger_bycolumn` of `tsvector_op.c`, `suppress_redundant_updates_trigger` of `trigfuncs.c`, and `trigger_return_old` of `regress.c`, the library of the regression tests.

use rupg_catalog::{
    Catalog, Column, TRIGGER_BEFORE, TRIGGER_INSERT, TRIGGER_ROW, TRIGGER_UPDATE, Trigger,
};
use rupg_common::{Error, SqlState};
use rupg_pgcatalog::builtin;
use rupg_types::{Value, oid};

use super::super::Connection;

/// The OID of `internal` in `pg_language`.
const INTERNAL_LANGUAGE: u32 = 12;
/// The OID of `c` in `pg_language`.
const C_LANGUAGE: u32 = 13;

/// A trigger function in C that runs here.
#[derive(Clone, Copy)]
pub(super) enum Internal {
    /// `tsvector_update_trigger_byid`.
    TsvectorById,
    /// `tsvector_update_trigger_bycolumn`.
    TsvectorByColumn,
    /// `suppress_redundant_updates_trigger`.
    SuppressRedundant,
    /// `trigger_return_old` of `regress.c`.
    ReturnOld,
}

/// The trigger function in C that the function `function` calls, by its `prosrc`: a built-in function, a function in `internal` that a user makes with the `prosrc` of a built-in function, or a function in C that links to `trigger_return_old`.
pub(super) fn internal(catalog: &Catalog, function: u32) -> Option<Internal> {
    let (lang, src) = match catalog.function(function) {
        Some(f) => (f.lang, f.src.as_str()),
        None => builtin::proc_by_oid(function).map(|p| (p.lang, p.src))?,
    };
    match (lang, src) {
        (INTERNAL_LANGUAGE, "tsvector_update_trigger_byid") => Some(Internal::TsvectorById),
        (INTERNAL_LANGUAGE, "tsvector_update_trigger_bycolumn") => Some(Internal::TsvectorByColumn),
        (INTERNAL_LANGUAGE, "suppress_redundant_updates_trigger") => {
            Some(Internal::SuppressRedundant)
        }
        (C_LANGUAGE, "trigger_return_old") => Some(Internal::ReturnOld),
        _ => None,
    }
}

/// `trigger_return_old`: the function gives `tg_trigtuple`, which is the new row of an `INSERT` and the old row of an `UPDATE` or a `DELETE`. A trigger for each statement has no row.
pub(super) fn trigger_return_old(
    event: i16,
    (old, new): (Option<Vec<Value>>, Option<Vec<Value>>),
) -> Option<Vec<Value>> {
    if event == TRIGGER_INSERT { new } else { old }
}

/// The names of the system columns, which `SPI_fnumber` finds with a negative number.
const SYSTEM_COLUMNS: [&str; 6] = ["ctid", "xmin", "cmin", "xmax", "cmax", "tableoid"];

/// The result of `SPI_fnumber` for `name`.
enum Found<'a> {
    /// A column that is not dropped, with its place in the row.
    Column(usize, &'a Column),
    /// A system column, whose type is not a type that the functions take.
    System,
    /// `SPI_ERROR_NOATTRIBUTE`.
    Missing,
}

/// `SPI_fnumber`: the column of `columns` with the name `name`.
fn fnumber<'a>(columns: &'a [Column], name: &str) -> Found<'a> {
    if let Some((i, c)) = columns.iter().enumerate().find(|(_, c)| !c.dropped && c.name == name) {
        Found::Column(i, c)
    } else if SYSTEM_COLUMNS.contains(&name) {
        Found::System
    } else {
        Found::Missing
    }
}

/// An `elog(ERROR)` of `tsvector_update_trigger`.
fn tsvector_internal(message: &str) -> Error {
    Error::internal(format!("tsvector_update_trigger: {message}"))
}

/// An error of `suppress_redundant_updates_trigger` for a call that is not the right kind of trigger.
fn suppress_violation(message: &str) -> Error {
    Error::new(
        SqlState::E_R_I_E_TRIGGER_PROTOCOL_VIOLATED,
        format!("suppress_redundant_updates_trigger: {message}"),
    )
}

/// The `42804` error of a column with the wrong type.
fn mismatch(message: String) -> Error {
    Error::new(SqlState::DATATYPE_MISMATCH, message)
}

impl Connection {
    /// `tsvector_update_trigger`: the trigger must be `BEFORE INSERT OR UPDATE ... FOR EACH ROW`, and its arguments are the `tsvector` column, the text search configuration and the text columns. `tsvector_update_trigger_column` takes a column of type `regconfig` in the place of the name of the configuration. The lexemes of the text columns of the new row go into the `tsvector` column, for an `INSERT` and for an `UPDATE` that sets one of the text columns. A null text adds no lexemes.
    pub(super) fn tsvector_update_trigger(
        &self,
        (trigger, by_column): (&Trigger, bool),
        event: i16,
        (_, new): (Option<Vec<Value>>, Option<Vec<Value>>),
        updated: &[i16],
    ) -> Result<Option<Vec<Value>>, Error> {
        if trigger.kind & TRIGGER_ROW == 0 {
            return Err(tsvector_internal("must be fired for row"));
        }
        if trigger.kind & TRIGGER_BEFORE == 0 {
            return Err(tsvector_internal("must be fired BEFORE event"));
        }
        let (mut row, mut needed) = match (event, new) {
            (TRIGGER_INSERT, Some(new)) => (new, true),
            (TRIGGER_UPDATE, Some(new)) => (new, false),
            _ => return Err(tsvector_internal("must be fired for INSERT or UPDATE")),
        };
        let args = &trigger.args;
        if args.len() < 3 {
            return Err(tsvector_internal(
                "arguments must be tsvector_field, ts_config, text_field1, ...)",
            ));
        }
        let catalog = self.catalog();
        let Some(rel) = catalog.relation(trigger.relation) else {
            return Err(Error::internal(format!(
                "could not open relation with OID {}",
                trigger.relation
            )));
        };
        let columns = &rel.columns;
        let target = match fnumber(columns, &args[0]) {
            Found::Column(i, c) if rupg_analyze::is_binary_coercible(c.ty, oid::TSVECTOR) => i,
            Found::Missing => {
                return Err(Error::new(
                    SqlState::UNDEFINED_COLUMN,
                    format!("tsvector column \"{}\" does not exist", args[0]),
                ));
            }
            _ => return Err(mismatch(format!("column \"{}\" is not of tsvector type", args[0]))),
        };
        let reader = self.reader();
        let config = if by_column {
            let at = match fnumber(columns, &args[1]) {
                Found::Column(i, c) if rupg_analyze::is_binary_coercible(c.ty, oid::REGCONFIG) => i,
                Found::Missing => {
                    return Err(Error::new(
                        SqlState::UNDEFINED_COLUMN,
                        format!("configuration column \"{}\" does not exist", args[1]),
                    ));
                }
                _ => {
                    return Err(mismatch(format!(
                        "column \"{}\" is not of regconfig type",
                        args[1]
                    )));
                }
            };
            match row.get(at).and_then(Value::as_oid) {
                Some(config) => config,
                None => {
                    return Err(Error::new(
                        SqlState::NULL_VALUE_NOT_ALLOWED,
                        format!("configuration column \"{}\" must not be null", args[1]),
                    ));
                }
            }
        } else {
            rupg_func::ts_config_qualified(&reader, &args[1])?
        };
        let mut texts = Vec::new();
        for name in &args[2..] {
            let at = match fnumber(columns, name) {
                Found::Column(i, c) if rupg_analyze::is_binary_coercible(c.ty, oid::TEXT) => i,
                Found::Missing => {
                    return Err(Error::new(
                        SqlState::UNDEFINED_COLUMN,
                        format!("column \"{name}\" does not exist"),
                    ));
                }
                _ => return Err(mismatch(format!("column \"{name}\" is not of a character type"))),
            };
            if i16::try_from(at + 1).is_ok_and(|attnum| updated.contains(&attnum)) {
                needed = true;
            }
            if let Some(text) = row.get(at).and_then(Value::as_str) {
                texts.push(text);
            }
        }
        if !needed {
            return Ok(Some(row));
        }
        let value = rupg_func::tsvector_of_texts(&reader, config, &texts)?;
        if let Some(slot) = row.get_mut(target) {
            *slot = value;
        }
        Ok(Some(row))
    }

    /// `suppress_redundant_updates_trigger`: the trigger must be `BEFORE UPDATE ... FOR EACH ROW`, and it skips the update of a row when the new row is the same as the old row. PostgreSQL compares the bytes of the two tuples. The stored generated columns do not count here, as the new row gets them after the triggers.
    pub(super) fn suppress_redundant_updates_trigger(
        &self,
        trigger: &Trigger,
        event: i16,
        (old, new): (Option<Vec<Value>>, Option<Vec<Value>>),
    ) -> Result<Option<Vec<Value>>, Error> {
        if event != TRIGGER_UPDATE {
            return Err(suppress_violation("must be called on update"));
        }
        if trigger.kind & TRIGGER_BEFORE == 0 {
            return Err(suppress_violation("must be called before update"));
        }
        if trigger.kind & TRIGGER_ROW == 0 {
            return Err(suppress_violation("must be called for each row"));
        }
        let (Some(old), Some(new)) = (old, new) else {
            return Err(Error::internal("an update with no old or no new row"));
        };
        let catalog = self.catalog();
        let columns = catalog.relation(trigger.relation).map_or(&[][..], |rel| &rel.columns[..]);
        let generated = |i: usize| columns.get(i).is_some_and(|c| c.generated != 0);
        let null = Value::Null;
        let same = (0..new.len().max(old.len())).all(|i| {
            generated(i) || same_bytes(old.get(i).unwrap_or(&null), new.get(i).unwrap_or(&null))
        });
        Ok(if same { None } else { Some(new) })
    }
}

/// True when two values have the same bytes in a tuple, so a float `NaN` is the same as itself.
fn same_bytes(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Float8(a), Value::Float8(b)) => a.to_bits() == b.to_bits(),
        (Value::Float4(a), Value::Float4(b)) => a.to_bits() == b.to_bits(),
        _ => a == b,
    }
}
