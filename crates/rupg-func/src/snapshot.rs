//! The functions of `pg_snapshot` and `txid_snapshot` from `xid8funcs.c`: `xmin`, `xmax`, the transactions in progress and the visibility of a transaction, and the functions of the ID, the snapshot and the state of a transaction. Also `age` and `mxid_age` of `xid` from `xid.c`. The functions of `txid_snapshot` have the same `prosrc` as the functions of `pg_snapshot`, and they give `bigint` where these give `xid8`. Both hold a `u64` in the bits of [`Value::Int8`].

use rupg_common::{Error, Result, SqlState};
use rupg_types::{Snapshot, Value};

use crate::{Call, Kernel, bad_value, not_yet};

/// `FirstNormalTransactionId`: the IDs before it are the IDs of no transaction, of the bootstrap and of the frozen rows.
const FIRST_NORMAL_XID: u32 = 3;

/// The state of a transaction for `pg_xact_status`, as `TransactionIdIsInProgress` and `TransactionIdDidCommit` give it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum XactState {
    /// The transaction did not end.
    InProgress,
    /// The transaction committed.
    Committed,
    /// The transaction aborted, or its session ended before it committed.
    Aborted,
}

/// The kernel of a function of `pg_snapshot` by its `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    Some(match src {
        "pg_snapshot_xmin" => snapshot_xmin,
        "pg_snapshot_xmax" => snapshot_xmax,
        "pg_visible_in_snapshot" => visible_in_snapshot,
        "pg_current_xact_id" => current_xact_id,
        "pg_current_xact_id_if_assigned" => current_xact_id_if_assigned,
        "pg_current_snapshot" => current_snapshot,
        "pg_xact_status" => xact_status,
        "xid_age" => xid_age,
        "mxid_age" => mxid_age,
        _ => return None,
    })
}

/// `ReadNextMultiXactId`: rupg has no multixacts, so the next is the first.
const FIRST_MULTI_XACT_ID: u32 = 1;

/// `xid_age`: the number of transactions from `xid` to [`crate::Session::stable_transaction_id`]. An ID that is not normal is the oldest.
fn xid_age(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Some(Value::Oid(xid)) = args.first() else { return Err(bad_value()) };
    if *xid < FIRST_NORMAL_XID {
        return Ok(Value::Int4(i32::MAX));
    }
    Ok(Value::Int4(call.session.stable_transaction_id().wrapping_sub(*xid).cast_signed()))
}

/// `mxid_age`: the number of multixacts from `xid` to the next multixact. The ID 0 is the oldest.
fn mxid_age(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Some(Value::Oid(xid)) = args.first() else { return Err(bad_value()) };
    if *xid == 0 {
        return Ok(Value::Int4(i32::MAX));
    }
    Ok(Value::Int4(FIRST_MULTI_XACT_ID.wrapping_sub(*xid).cast_signed()))
}

/// The snapshot of an argument.
fn snapshot(args: &[Value], n: usize) -> Result<Snapshot> {
    match args.get(n) {
        Some(Value::Bytea(v)) => Snapshot::from_bytes(v).ok_or_else(bad_value),
        _ => Err(bad_value()),
    }
}

fn snapshot_xmin(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Int8(snapshot(args, 0)?.xmin.cast_signed()))
}

fn snapshot_xmax(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Int8(snapshot(args, 0)?.xmax.cast_signed()))
}

/// `pg_visible_in_snapshot`: true when the transaction ended before the snapshot.
fn visible_in_snapshot(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Some(Value::Int8(xid)) = args.first() else { return Err(bad_value()) };
    Ok(Value::Bool(snapshot(args, 1)?.is_visible(xid.cast_unsigned())))
}

/// `pg_snapshot_xip`: a row for each transaction in progress, in order.
pub(crate) fn snapshot_xip(_: &Call<'_>, args: &[Value]) -> Result<Vec<Vec<Value>>> {
    let snapshot = snapshot(args, 0)?;
    Ok(snapshot.xip.iter().map(|xid| vec![Value::Int8(xid.cast_signed())]).collect())
}

/// `pg_current_xact_id`: the ID of the transaction. A transaction that has no ID gets one, as `GetTopFullTransactionId` assigns it.
fn current_xact_id(call: &Call<'_>, _: &[Value]) -> Result<Value> {
    let Some(xid) = call.session.transaction_id(true) else {
        return Err(not_yet("a transaction ID in a session with no server"));
    };
    Ok(Value::Int8(xid.cast_signed()))
}

/// `pg_current_xact_id_if_assigned`: the ID of the transaction, or NULL when it has none.
fn current_xact_id_if_assigned(call: &Call<'_>, _: &[Value]) -> Result<Value> {
    Ok(call.session.transaction_id(false).map_or(Value::Null, |xid| Value::Int8(xid.cast_signed())))
}

/// `pg_current_snapshot`: the snapshot of the statement.
fn current_snapshot(call: &Call<'_>, _: &[Value]) -> Result<Value> {
    let Some(snapshot) = call.session.current_snapshot() else {
        return Err(Error::internal("no active snapshot set"));
    };
    Ok(Value::Bytea(snapshot.to_bytes()))
}

/// `pg_xact_status` with the checks of `TransactionIdInRecentPast`: NULL for the ID 0 and for a transaction that the session does not know, `committed` for the IDs of the bootstrap and of the frozen rows, and an error for an ID in the future.
fn xact_status(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let Some(Value::Int8(full)) = args.first() else { return Err(bad_value()) };
    let full = full.cast_unsigned();
    let xid = u32::try_from(full & u64::from(u32::MAX)).unwrap_or_default();
    if xid == 0 {
        return Ok(Value::Null);
    }
    if xid < FIRST_NORMAL_XID {
        return Ok(Value::Text("committed".into()));
    }
    if full >= call.session.next_transaction_id() {
        return Err(Error::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("transaction ID {full} is in the future"),
        ));
    }
    Ok(match call.session.transaction_state(xid) {
        None => Value::Null,
        Some(XactState::InProgress) => Value::Text("in progress".into()),
        Some(XactState::Committed) => Value::Text("committed".into()),
        Some(XactState::Aborted) => Value::Text("aborted".into()),
    })
}
