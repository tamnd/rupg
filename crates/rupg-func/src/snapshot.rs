//! The functions of `pg_snapshot` and `txid_snapshot` from `xid8funcs.c`: `xmin`, `xmax`, the transactions in progress and the visibility of a transaction. The functions of `txid_snapshot` have the same `prosrc` as the functions of `pg_snapshot`, and they give `bigint` where these give `xid8`. Both hold a `u64` in the bits of [`Value::Int8`].

use rupg_common::Result;
use rupg_types::{Snapshot, Value};

use crate::{Call, Kernel, bad_value};

/// The kernel of a function of `pg_snapshot` by its `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    Some(match src {
        "pg_snapshot_xmin" => snapshot_xmin,
        "pg_snapshot_xmax" => snapshot_xmax,
        "pg_visible_in_snapshot" => visible_in_snapshot,
        _ => return None,
    })
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
