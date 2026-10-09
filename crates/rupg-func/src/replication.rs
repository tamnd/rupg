//! The functions of the views of the work that rupg does not do yet: the progress of `VACUUM`, `ANALYZE`, `REPACK`, `CREATE INDEX`, `BASE_BACKUP` and `COPY`, the WAL senders and the WAL receiver, the logical replication workers and their statistics, the replication slots and the prepared transactions.
//!
//! rupg has no WAL, no replication, no two-phase commit and no progress reports, so these functions give what PostgreSQL gives on a server with none of that work: no rows, or one row of nulls for `pg_stat_get_wal_receiver`, and zero counts for the statistics of a subscription or of a replication slot.

use rupg_common::{Error, Result, SqlState};
use rupg_types::{Value, name_in};

use crate::{Call, Kernel, bad_value};

/// The OIDs of the index access methods `btree` and `gin`, which have names for the phases of an index build.
const BTREE: u32 = 403;
const GIN: u32 = 2742;

/// The 11 counts of `pg_stat_get_subscription_stats`, from `apply_error_count` to `confl_multiple_unique_conflicts`.
const SUBSCRIPTION_COUNTS: usize = 11;

/// The commands of `pg_stat_get_progress_info`.
const COMMANDS: [&str; 6] = ["VACUUM", "ANALYZE", "REPACK", "CREATE INDEX", "BASEBACKUP", "COPY"];

/// The 10 counts of `pg_stat_get_replication_slot`, from `spill_txns` to `slotsync_skip_count`.
const SLOT_COUNTS: usize = 10;

/// The 15 columns of `pg_stat_get_wal_receiver`.
const WAL_RECEIVER_COLUMNS: usize = 15;

/// `pg_stat_get_progress_info`: a row for each session that runs the command. The name of the command must be known, in any case.
pub(crate) fn progress_info(_: &Call<'_>, args: &[Value]) -> Result<Vec<Vec<Value>>> {
    let Some(Value::Text(name)) = args.first() else { return Err(bad_value()) };
    if !COMMANDS.iter().any(|command| command.eq_ignore_ascii_case(name)) {
        return Err(Error::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("invalid command name: \"{name}\""),
        ));
    }
    Ok(Vec::new())
}

/// A set function with no rows: `pg_stat_get_wal_senders`, `pg_stat_get_subscription`, `pg_get_replication_slots` and `pg_prepared_xact`.
pub(crate) fn none(_: &Call<'_>, _: &[Value]) -> Result<Vec<Vec<Value>>> {
    Ok(Vec::new())
}

/// `pg_stat_get_wal_receiver`: a null record, which is one row of nulls in `FROM`, as no WAL receiver runs.
pub(crate) fn wal_receiver(_: &Call<'_>, _: &[Value]) -> Result<Vec<Vec<Value>>> {
    Ok(vec![vec![Value::Null; WAL_RECEIVER_COLUMNS]])
}

/// `pg_stat_get_subscription_stats`: the statistics of a subscription, which are zero counts and a null `stats_reset` for a subscription with no statistics.
pub(crate) fn subscription_stats(_: &Call<'_>, args: &[Value]) -> Result<Vec<Vec<Value>>> {
    let Some(Value::Oid(subscription)) = args.first() else { return Err(bad_value()) };
    let mut row = vec![Value::Oid(*subscription)];
    row.extend(std::iter::repeat_n(Value::Int8(0), SUBSCRIPTION_COUNTS));
    row.push(Value::Null);
    Ok(vec![row])
}

/// `pg_stat_get_replication_slot`: the statistics of a replication slot, which are zero counts and null times for a slot with no statistics. The name is cut to 63 bytes, as `namestrcpy` does.
pub(crate) fn replication_slot(_: &Call<'_>, args: &[Value]) -> Result<Vec<Vec<Value>>> {
    let Some(Value::Text(name)) = args.first() else { return Err(bad_value()) };
    let mut row = vec![Value::text(name_in(name))];
    row.extend(std::iter::repeat_n(Value::Int8(0), SLOT_COUNTS));
    row.extend([Value::Null, Value::Null]);
    Ok(vec![row])
}

/// `pg_indexam_progress_phasename`: the name of a phase of an index build, from `ambuildphasename` of the access method. PostgreSQL reads the `bigint` argument as `int4`, so only its low 32 bits count.
#[allow(clippy::cast_possible_truncation)]
fn phase_name(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let (Some(Value::Oid(method)), Some(Value::Int8(phase))) = (args.first(), args.get(1)) else {
        return Err(bad_value());
    };
    let phase = *phase as i32;
    let name = match (*method, phase) {
        (BTREE | GIN, 1) => "initializing",
        (BTREE | GIN, 2) => "scanning table",
        (BTREE, 3) => "sorting live tuples",
        (BTREE, 4) => "sorting dead tuples",
        (BTREE, 5) => "loading tuples in tree",
        (GIN, 3) => "sorting tuples (workers)",
        (GIN, 4) => "merging tuples (workers)",
        (GIN, 5) => "sorting tuples",
        (GIN, 6) => "merging tuples",
        _ => return Ok(Value::Null),
    };
    Ok(Value::text(name))
}

pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    Some(match src {
        "pg_indexam_progress_phasename" => phase_name,
        _ => return None,
    })
}
