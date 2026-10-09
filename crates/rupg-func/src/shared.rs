//! The functions of the statistics of the whole server, from `pgstatfuncs.c`: the background writer, the checkpointer, WAL, I/O, the archiver, the SLRU caches, the prefetch of recovery and the locks, which the views `pg_stat_bgwriter`, `pg_stat_checkpointer`, `pg_stat_wal`, `pg_stat_io`, `pg_stat_archiver`, `pg_stat_slru`, `pg_stat_recovery_prefetch` and `pg_stat_lock` show.
//!
//! rupg does not keep these counters yet, so each counter is 0, each time is 0 and each name of a WAL file and its time is null, as on a server that has not done the work. The rows and the null cells are those of PostgreSQL. `stats_reset` is the time that the store of the server started its statistics, which is the start of its first session.

use rupg_common::Result;
use rupg_types::{Numeric, Value};

use crate::{Call, Kernel};

/// The SLRU caches of `pgstat_get_slru_name`, in their order.
const SLRUS: [&str; 8] = [
    "commit_timestamp",
    "multixact_member",
    "multixact_offset",
    "notify",
    "serializable",
    "subtransaction",
    "transaction",
    "other",
];

/// `LockTagTypeNames`, in the order of `LockTagType`.
const LOCK_TYPES: [&str; 12] = [
    "relation",
    "extend",
    "frozenid",
    "page",
    "tuple",
    "transactionid",
    "virtualxid",
    "spectoken",
    "object",
    "userlock",
    "advisory",
    "applytransaction",
];

/// The counters of the background writer and of the checkpointer, after `pg_stat_get_`.
const COUNTERS: [&str; 11] = [
    "bgwriter_buf_written_clean",
    "bgwriter_maxwritten_clean",
    "buf_alloc",
    "checkpointer_num_timed",
    "checkpointer_num_requested",
    "checkpointer_num_performed",
    "checkpointer_restartpoints_timed",
    "checkpointer_restartpoints_requested",
    "checkpointer_restartpoints_performed",
    "checkpointer_buffers_written",
    "checkpointer_slru_written",
];

/// The 20 columns of `pg_stat_get_io`.
const IO_COLUMNS: usize = 20;

/// The 10 columns of `pg_stat_get_recovery_prefetch`, after `stats_reset`: six counts of the type `bigint` and three of the type `integer`.
const PREFETCH_COUNTS: usize = 6;
const PREFETCH_DEPTHS: usize = 3;

/// The types of processes in `BackendType` that count their I/O, in its order: `pgstat_tracks_io_bktype`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Process {
    Client,
    AutovacuumLauncher,
    AutovacuumWorker,
    BackgroundWorker,
    WalSender,
    SlotsyncWorker,
    Standalone,
    BackgroundWriter,
    Checkpointer,
    IoWorker,
    Startup,
    WalReceiver,
    WalSummarizer,
    WalWriter,
}

/// Each type of process with the name of `GetBackendTypeDesc`.
const PROCESSES: [(Process, &str); 14] = [
    (Process::Client, "client backend"),
    (Process::AutovacuumLauncher, "autovacuum launcher"),
    (Process::AutovacuumWorker, "autovacuum worker"),
    (Process::BackgroundWorker, "background worker"),
    (Process::WalSender, "walsender"),
    (Process::SlotsyncWorker, "slotsync worker"),
    (Process::Standalone, "standalone backend"),
    (Process::BackgroundWriter, "background writer"),
    (Process::Checkpointer, "checkpointer"),
    (Process::IoWorker, "io worker"),
    (Process::Startup, "startup"),
    (Process::WalReceiver, "walreceiver"),
    (Process::WalSummarizer, "walsummarizer"),
    (Process::WalWriter, "walwriter"),
];

/// `IOObject`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Object {
    Relation,
    TempRelation,
    Wal,
}

const OBJECTS: [(Object, &str); 3] =
    [(Object::Relation, "relation"), (Object::TempRelation, "temp relation"), (Object::Wal, "wal")];

/// `IOContext`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Context {
    BulkRead,
    BulkWrite,
    Init,
    Normal,
    Vacuum,
}

const CONTEXTS: [(Context, &str); 5] = [
    (Context::BulkRead, "bulkread"),
    (Context::BulkWrite, "bulkwrite"),
    (Context::Init, "init"),
    (Context::Normal, "normal"),
    (Context::Vacuum, "vacuum"),
];

/// `IOOp`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Evict,
    Fsync,
    Hit,
    Reuse,
    Writeback,
    Extend,
    Read,
    Write,
}

/// Each operation with the columns of its count, its time and its bytes in `pg_stat_get_io`: `pgstat_get_io_op_index`, `pgstat_get_io_time_index` and `pgstat_get_io_byte_index`.
const OPS: [(Op, usize, Option<usize>, Option<usize>); 8] = [
    (Op::Evict, 15, None, None),
    (Op::Fsync, 17, Some(18), None),
    (Op::Hit, 14, None, None),
    (Op::Reuse, 16, None, None),
    (Op::Writeback, 9, Some(10), None),
    (Op::Extend, 11, Some(13), Some(12)),
    (Op::Read, 3, Some(5), Some(4)),
    (Op::Write, 6, Some(8), Some(7)),
];

/// `pgstat_tracks_io_object`: true when the process can do I/O on the object in the context, so that the view has a row for them.
fn tracks_object(process: Process, object: Object, context: Context) -> bool {
    if object == Object::Wal && !matches!(context, Context::Normal | Context::Init) {
        return false;
    }
    if object == Object::TempRelation && context != Context::Normal {
        return false;
    }
    let no_temp_relation = matches!(
        process,
        Process::AutovacuumLauncher
            | Process::BackgroundWriter
            | Process::Checkpointer
            | Process::AutovacuumWorker
            | Process::Standalone
            | Process::Startup
            | Process::WalSummarizer
            | Process::WalWriter
            | Process::WalReceiver
    );
    if no_temp_relation && object == Object::TempRelation {
        return false;
    }
    if matches!(process, Process::WalSummarizer | Process::WalReceiver | Process::WalWriter)
        && object != Object::Wal
    {
        return false;
    }
    let strategy = matches!(context, Context::BulkRead | Context::BulkWrite | Context::Vacuum);
    if matches!(process, Process::Checkpointer | Process::BackgroundWriter) && strategy {
        return false;
    }
    if process == Process::AutovacuumLauncher && context == Context::Vacuum {
        return false;
    }
    !(matches!(process, Process::AutovacuumWorker | Process::AutovacuumLauncher)
        && context == Context::BulkWrite)
}

/// `pgstat_tracks_io_op`: true when the process can do the operation on the object in the context, so that the cell of the view is not null.
fn tracks_op(process: Process, object: Object, context: Context, op: Op) -> bool {
    if !tracks_object(process, object, context) {
        return false;
    }
    if process == Process::BackgroundWriter && matches!(op, Op::Read | Op::Evict | Op::Hit) {
        return false;
    }
    if process == Process::Checkpointer
        && ((object != Object::Wal && op == Op::Read) || matches!(op, Op::Evict | Op::Hit))
    {
        return false;
    }
    if matches!(process, Process::BackgroundWriter | Process::Checkpointer) && op == Op::Extend {
        return false;
    }
    if object == Object::Wal
        && op == Op::Read
        && matches!(
            process,
            Process::WalReceiver
                | Process::BackgroundWriter
                | Process::AutovacuumLauncher
                | Process::AutovacuumWorker
                | Process::WalWriter
        )
    {
        return false;
    }
    if object == Object::TempRelation && matches!(op, Op::Fsync | Op::Writeback) {
        return false;
    }
    if context == Context::BulkRead && op == Op::Extend {
        return false;
    }
    let strategy = matches!(context, Context::BulkRead | Context::BulkWrite | Context::Vacuum);
    if !strategy && op == Op::Reuse {
        return false;
    }
    if object == Object::Wal && context == Context::Init && !matches!(op, Op::Write | Op::Fsync) {
        return false;
    }
    if object == Object::Wal
        && context == Context::Normal
        && !matches!(op, Op::Write | Op::Read | Op::Fsync)
    {
        return false;
    }
    !(strategy && op == Op::Fsync)
}

/// The time of the last reset of the statistics, or null when it is 0, as most of the functions give it.
fn reset_or_null(call: &Call<'_>) -> Value {
    match call.session.stats_reset() {
        0 => Value::Null,
        at => Value::TimestampTz(at),
    }
}

/// A count of 0 bytes, of the type `numeric`.
fn no_bytes() -> Value {
    Value::Numeric(Numeric::from_integer(0))
}

/// A counter of the background writer or of the checkpointer.
fn counter(_: &Call<'_>, _: &[Value]) -> Result<Value> {
    Ok(Value::Int8(0))
}

/// `pg_stat_get_checkpointer_write_time` and `pg_stat_get_checkpointer_sync_time`, in milliseconds.
fn time(_: &Call<'_>, _: &[Value]) -> Result<Value> {
    Ok(Value::Float8(0.0))
}

/// `pg_stat_get_bgwriter_stat_reset_time` and `pg_stat_get_checkpointer_stat_reset_time`, which give the time even when it is 0.
fn reset_time(call: &Call<'_>, _: &[Value]) -> Result<Value> {
    Ok(Value::TimestampTz(call.session.stats_reset()))
}

/// `pg_stat_get_wal`: the counts of the WAL records, the full page images, the bytes and the waits for a full WAL buffer.
pub(crate) fn wal(call: &Call<'_>, _: &[Value]) -> Result<Vec<Vec<Value>>> {
    Ok(vec![vec![
        Value::Int8(0),
        Value::Int8(0),
        no_bytes(),
        no_bytes(),
        Value::Int8(0),
        reset_or_null(call),
    ]])
}

/// `pg_stat_get_io`: a row for each type of process, object and context that `pgstat_tracks_io_object` allows, with a null cell for each operation that `pgstat_tracks_io_op` does not allow.
pub(crate) fn io(call: &Call<'_>, _: &[Value]) -> Result<Vec<Vec<Value>>> {
    let reset = reset_or_null(call);
    let mut rows = Vec::new();
    for (process, process_name) in PROCESSES {
        for (object, object_name) in OBJECTS {
            for (context, context_name) in CONTEXTS {
                if !tracks_object(process, object, context) {
                    continue;
                }
                let mut row = vec![Value::Null; IO_COLUMNS];
                row[0] = Value::text(process_name);
                row[1] = Value::text(object_name);
                row[2] = Value::text(context_name);
                row[IO_COLUMNS - 1] = reset.clone();
                for (op, count, time, bytes) in OPS {
                    if !tracks_op(process, object, context, op) {
                        continue;
                    }
                    row[count] = Value::Int8(0);
                    if let Some(time) = time {
                        row[time] = Value::Float8(0.0);
                    }
                    if let Some(bytes) = bytes {
                        row[bytes] = no_bytes();
                    }
                }
                rows.push(row);
            }
        }
    }
    Ok(rows)
}

/// `pg_stat_get_archiver`: no archived or failed WAL files.
pub(crate) fn archiver(call: &Call<'_>, _: &[Value]) -> Result<Vec<Vec<Value>>> {
    Ok(vec![vec![
        Value::Int8(0),
        Value::Null,
        Value::Null,
        Value::Int8(0),
        Value::Null,
        Value::Null,
        reset_or_null(call),
    ]])
}

/// `pg_stat_get_slru`: a row for each SLRU cache.
pub(crate) fn slru(call: &Call<'_>, _: &[Value]) -> Result<Vec<Vec<Value>>> {
    let reset = Value::TimestampTz(call.session.stats_reset());
    Ok(SLRUS
        .iter()
        .map(|name| {
            let mut row = vec![Value::text(*name)];
            row.extend(std::iter::repeat_n(Value::Int8(0), 7));
            row.push(reset.clone());
            row
        })
        .collect())
}

/// `pg_stat_get_recovery_prefetch`: the counts of the prefetch of recovery, which are 0 on a server that is not in recovery.
pub(crate) fn recovery_prefetch(call: &Call<'_>, _: &[Value]) -> Result<Vec<Vec<Value>>> {
    let mut row = vec![reset_or_null(call)];
    row.extend(std::iter::repeat_n(Value::Int8(0), PREFETCH_COUNTS));
    row.extend(std::iter::repeat_n(Value::Int4(0), PREFETCH_DEPTHS));
    Ok(vec![row])
}

/// `pg_stat_get_lock`: a row for each type of lock tag.
pub(crate) fn lock(call: &Call<'_>, _: &[Value]) -> Result<Vec<Vec<Value>>> {
    let reset = Value::TimestampTz(call.session.stats_reset());
    Ok(LOCK_TYPES
        .iter()
        .map(|name| {
            vec![
                Value::text(*name),
                Value::Int8(0),
                Value::Float8(0.0),
                Value::Int8(0),
                reset.clone(),
            ]
        })
        .collect())
}

/// The kernel of a function of this module by its `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    let name = src.strip_prefix("pg_stat_get_")?;
    Some(match name {
        "checkpointer_write_time" | "checkpointer_sync_time" => time,
        "bgwriter_stat_reset_time" | "checkpointer_stat_reset_time" => reset_time,
        _ if COUNTERS.contains(&name) => counter,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The module has the 15 functions of the background writer and of the checkpointer in `pg_proc`, each with no arguments.
    #[test]
    fn covers_the_functions_of_the_writers() {
        let mut found = 0;
        for proc in rupg_pgcatalog::builtin::procs() {
            if by_src(proc.src).is_none() {
                continue;
            }
            found += 1;
            assert!(proc.argtypes.is_empty(), "{}", proc.src);
            assert!(matches!(proc.rettype, 20 | 701 | 1184), "{}", proc.src);
        }
        assert_eq!(found, 15);
    }

    /// `pg_stat_get_io` has 79 rows, as in PostgreSQL 19, and a client backend has a row for each context of a relation.
    #[test]
    fn the_rows_of_io() {
        let rows: Vec<_> = PROCESSES
            .iter()
            .flat_map(|&(process, _)| {
                OBJECTS.iter().flat_map(move |&(object, _)| {
                    CONTEXTS
                        .iter()
                        .filter(move |&&(context, _)| tracks_object(process, object, context))
                })
            })
            .collect();
        assert_eq!(rows.len(), 79);
        assert!(CONTEXTS.iter().all(|&(context, _)| tracks_object(
            Process::Client,
            Object::Relation,
            context
        )));
    }
}
