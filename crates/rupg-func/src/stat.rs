//! The functions of the cumulative statistics for one object, from `pgstatfuncs.c`: the counters of a relation or an index, of a function and of a database, and the `xact` forms that read the counts of the transaction.
//!
//! rupg keeps the sequential scans of a relation, their count `numscans` and the time of the last one `lastscan`, and the counters of the rows that the transactions wrote: `tuples_inserted`, `tuples_updated`, `tuples_deleted`, `live_tuples`, `dead_tuples`, `mod_since_analyze` and `ins_since_vacuum`. The other functions give the value that PostgreSQL gives for an object that has no entry in the statistics: 0 for a counter and for a total time of a relation or a database, and null for a time stamp and for each value of a function. The views `pg_stat_*` and `pg_statio_*` then have a row for each object, with the columns and the types of PostgreSQL (spec/07 section 7.14).

use rupg_common::Result;
use rupg_types::Value;

use crate::{Call, Kernel, bad_value};

/// The counters of a relation that `PG_STAT_GET_RELENTRY_INT64` and `PG_STAT_GET_XACT_RELENTRY_INT64` read.
const RELATION_COUNTERS: [&str; 18] = [
    "analyze_count",
    "autoanalyze_count",
    "autovacuum_count",
    "blocks_fetched",
    "blocks_hit",
    "dead_tuples",
    "ins_since_vacuum",
    "live_tuples",
    "mod_since_analyze",
    "numscans",
    "tuples_deleted",
    "tuples_fetched",
    "tuples_hot_updated",
    "tuples_newpage_updated",
    "tuples_inserted",
    "tuples_returned",
    "tuples_updated",
    "vacuum_count",
];

/// The counters of a database that `PG_STAT_GET_DBENTRY_INT64` reads, after `pg_stat_get_db_`.
const DATABASE_COUNTERS: [&str; 25] = [
    "blocks_fetched",
    "blocks_hit",
    "conflict_bufferpin",
    "conflict_lock",
    "conflict_snapshot",
    "conflict_startup_deadlock",
    "conflict_tablespace",
    "conflict_logicalslot",
    "conflict_all",
    "deadlocks",
    "sessions",
    "sessions_abandoned",
    "sessions_fatal",
    "sessions_killed",
    "parallel_workers_to_launch",
    "parallel_workers_launched",
    "temp_bytes",
    "temp_files",
    "tuples_deleted",
    "tuples_fetched",
    "tuples_inserted",
    "tuples_returned",
    "tuples_updated",
    "xact_commit",
    "xact_rollback",
];

/// The times of a database in milliseconds that `PG_STAT_GET_DBENTRY_FLOAT8_MS` reads, after `pg_stat_get_db_`.
const DATABASE_TIMES: [&str; 5] =
    ["active_time", "blk_read_time", "blk_write_time", "idle_in_transaction_time", "session_time"];

/// The total times of a relation that `PG_STAT_GET_RELENTRY_FLOAT8` reads.
const RELATION_TIMES: [&str; 4] =
    ["total_vacuum_time", "total_autovacuum_time", "total_analyze_time", "total_autoanalyze_time"];

/// The time stamps of a relation that `PG_STAT_GET_RELENTRY_TIMESTAMPTZ` reads.
const RELATION_STAMPS: [&str; 6] = [
    "last_analyze_time",
    "last_autoanalyze_time",
    "last_autovacuum_time",
    "last_vacuum_time",
    "lastscan",
    "stat_reset_time",
];

/// The counters of a relation in the statistics of the server, the part of `PgStat_StatTabEntry` that rupg keeps.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RelationCounts {
    /// `numscans`: the count of the sequential scans of the relation.
    pub numscans: i64,
    /// `lastscan`: the end of the last transaction that scanned the relation, in microseconds since 2000-01-01 UTC, or `None` before the first scan.
    pub lastscan: Option<i64>,
    /// `tuples_inserted`, `tuples_updated` and `tuples_deleted`: the rows that the transactions wrote, also the transactions that rolled back.
    pub written: Writes,
    /// `live_tuples`: the rows that the committed transactions inserted and did not delete.
    pub live_tuples: i64,
    /// `dead_tuples`: the old versions of the rows that the committed updates and deletes left, and the rows that the rolled back transactions inserted or updated.
    pub dead_tuples: i64,
    /// `mod_since_analyze`: the rows that the committed transactions wrote.
    pub mod_since_analyze: i64,
    /// `ins_since_vacuum`: the rows that the transactions inserted.
    pub ins_since_vacuum: i64,
}

/// The rows that a transaction inserted, updated and deleted in one relation, as `pgstat_count_heap_insert`, `pgstat_count_heap_update` and `pgstat_count_heap_delete` count them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Writes {
    /// The rows that the transaction inserted.
    pub inserted: i64,
    /// The rows that the transaction updated.
    pub updated: i64,
    /// The rows that the transaction deleted.
    pub deleted: i64,
}

impl std::ops::Add for Writes {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        Self {
            inserted: self.inserted + other.inserted,
            updated: self.updated + other.updated,
            deleted: self.deleted + other.deleted,
        }
    }
}

impl std::ops::Sub for Writes {
    type Output = Self;

    fn sub(self, other: Self) -> Self {
        Self {
            inserted: self.inserted - other.inserted,
            updated: self.updated - other.updated,
            deleted: self.deleted - other.deleted,
        }
    }
}

/// The OID of the object. A strict function does not get null, and PostgreSQL does not check that the object exists.
fn object(args: &[Value]) -> Result<u32> {
    args.first().and_then(Value::as_oid).ok_or_else(bad_value)
}

/// A counter of an object with no entry.
fn counter(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    object(args)?;
    Ok(Value::Int8(0))
}

/// `pg_stat_get_numscans`: the sequential scans of a relation that the server reported.
fn numscans(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let counts = call.session.relation_counts(object(args)?);
    Ok(Value::Int8(counts.map_or(0, |c| c.numscans)))
}

/// `pg_stat_get_lastscan`: the time of the last sequential scan of a relation, or null.
fn lastscan(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let counts = call.session.relation_counts(object(args)?);
    Ok(counts.and_then(|c| c.lastscan).map_or(Value::Null, Value::TimestampTz))
}

/// `pg_stat_get_xact_numscans`: the sequential scans of a relation that the transaction counted.
fn xact_numscans(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Int8(call.session.pending_scans(object(args)?)))
}

/// A counter of a relation that the server reported, or 0 for a relation with no entry.
fn relation_counter(
    call: &Call<'_>,
    args: &[Value],
    read: fn(&RelationCounts) -> i64,
) -> Result<Value> {
    let counts = call.session.relation_counts(object(args)?);
    Ok(Value::Int8(counts.as_ref().map_or(0, read)))
}

/// A count of the rows that the transaction wrote in a relation.
fn xact_counter(call: &Call<'_>, args: &[Value], read: fn(&Writes) -> i64) -> Result<Value> {
    Ok(Value::Int8(read(&call.session.pending_writes(object(args)?))))
}

/// `pg_stat_get_tuples_inserted`.
fn tuples_inserted(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    relation_counter(call, args, |c| c.written.inserted)
}

/// `pg_stat_get_tuples_updated`.
fn tuples_updated(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    relation_counter(call, args, |c| c.written.updated)
}

/// `pg_stat_get_tuples_deleted`.
fn tuples_deleted(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    relation_counter(call, args, |c| c.written.deleted)
}

/// `pg_stat_get_live_tuples`.
fn live_tuples(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    relation_counter(call, args, |c| c.live_tuples)
}

/// `pg_stat_get_dead_tuples`.
fn dead_tuples(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    relation_counter(call, args, |c| c.dead_tuples)
}

/// `pg_stat_get_mod_since_analyze`.
fn mod_since_analyze(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    relation_counter(call, args, |c| c.mod_since_analyze)
}

/// `pg_stat_get_ins_since_vacuum`.
fn ins_since_vacuum(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    relation_counter(call, args, |c| c.ins_since_vacuum)
}

/// `pg_stat_get_xact_tuples_inserted`.
fn xact_inserted(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    xact_counter(call, args, |w| w.inserted)
}

/// `pg_stat_get_xact_tuples_updated`.
fn xact_updated(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    xact_counter(call, args, |w| w.updated)
}

/// `pg_stat_get_xact_tuples_deleted`.
fn xact_deleted(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    xact_counter(call, args, |w| w.deleted)
}

/// A total time of an object with no entry.
fn total_time(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    object(args)?;
    Ok(Value::Float8(0.0))
}

/// A time stamp of an object with no entry, or a value of a function with no entry.
fn none(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    object(args)?;
    Ok(Value::Null)
}

/// `pg_stat_get_db_checksum_failures`: null when the data has no checksums, as `DataChecksumsEnabled` tells, else the count of a database with no entry.
fn checksum_failures(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    object(args)?;
    Ok(match call.session.setting("data_checksums").as_deref() {
        Some("on") => Value::Int8(0),
        _ => Value::Null,
    })
}

/// `pg_stat_force_next_flush`: the next end of a transaction reports the counts at once. rupg reports them at the end of each transaction, so the call has no effect. The result is `void`, whose value the engine keeps as an empty text.
fn force_next_flush(_: &Call<'_>, _: &[Value]) -> Result<Value> {
    Ok(Value::text(""))
}

/// The kernel of a function of this module by its `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    if src == "pg_stat_force_next_flush" {
        return Some(force_next_flush);
    }
    let name = src.strip_prefix("pg_stat_get_")?;
    if let Some(name) = name.strip_prefix("db_") {
        return Some(match name {
            "checksum_failures" => checksum_failures,
            "checksum_last_failure" | "stat_reset_time" => none,
            _ if DATABASE_COUNTERS.contains(&name) => counter,
            _ if DATABASE_TIMES.contains(&name) => total_time,
            _ => return None,
        });
    }
    if let Some(name) = name.strip_prefix("xact_") {
        return Some(match name {
            "function_calls" | "function_total_time" | "function_self_time" => none,
            "numscans" => xact_numscans,
            "tuples_inserted" => xact_inserted,
            "tuples_updated" => xact_updated,
            "tuples_deleted" => xact_deleted,
            _ if RELATION_COUNTERS.contains(&name) => counter,
            _ => return None,
        });
    }
    Some(match name {
        "function_calls" | "function_total_time" | "function_self_time" => none,
        "function_stat_reset_time" => none,
        "numscans" => numscans,
        "lastscan" => lastscan,
        "tuples_inserted" => tuples_inserted,
        "tuples_updated" => tuples_updated,
        "tuples_deleted" => tuples_deleted,
        "live_tuples" => live_tuples,
        "dead_tuples" => dead_tuples,
        "mod_since_analyze" => mod_since_analyze,
        "ins_since_vacuum" => ins_since_vacuum,
        _ if RELATION_COUNTERS.contains(&name) => counter,
        _ if RELATION_TIMES.contains(&name) => total_time,
        _ if RELATION_STAMPS.contains(&name) => none,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The module has the 78 functions of one object of `pg_proc`, each with one argument of the type `oid` and a result of the type `int8`, `float8` or `timestamptz`.
    #[test]
    fn covers_the_functions_of_one_object() {
        let mut found = 0;
        for proc in rupg_pgcatalog::builtin::procs() {
            if !proc.src.starts_with("pg_stat_get_") || by_src(proc.src).is_none() {
                continue;
            }
            found += 1;
            assert_eq!(proc.argtypes, [26], "{}", proc.src);
            assert!(matches!(proc.rettype, 20 | 701 | 1184), "{}", proc.src);
        }
        assert_eq!(found, 78);
    }
}
