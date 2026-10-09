//! The functions of the cumulative statistics for one object, from `pgstatfuncs.c`: the counters of a relation or an index, of a function and of a database, and the `xact` forms that read the counts of the transaction.
//!
//! rupg does not keep the counters yet. So each function gives the value that PostgreSQL gives for an object that has no entry in the statistics: 0 for a counter and for a total time of a relation or a database, and null for a time stamp and for each value of a function. The views `pg_stat_*` and `pg_statio_*` then have a row for each object, with the columns and the types of PostgreSQL (spec/07 section 7.14).

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

/// The OID of the object. A strict function does not get null, and PostgreSQL does not check that the object exists.
fn object(args: &[Value]) -> Result<u32> {
    args.first().and_then(Value::as_oid).ok_or_else(bad_value)
}

/// A counter of an object with no entry.
fn counter(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    object(args)?;
    Ok(Value::Int8(0))
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

/// The kernel of a function of this module by its `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
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
            _ if RELATION_COUNTERS.contains(&name) => counter,
            _ => return None,
        });
    }
    Some(match name {
        "function_calls" | "function_total_time" | "function_self_time" => none,
        "function_stat_reset_time" => none,
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
