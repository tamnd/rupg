//! The advisory lock functions of `lockfuncs.c`. The session keeps the locks, so each function is a call of [`crate::Session::advisory_lock`] or [`crate::Session::advisory_unlock`].

use rupg_common::{Error, Result, SqlState};
use rupg_types::Value;

use crate::{AdvisoryKey, AdvisoryLock, Call, Kernel, bad_value};

/// The key of the arguments: `SET_LOCKTAG_INT64` for one `bigint`, and `SET_LOCKTAG_INT32` for two `integer` values.
#[allow(clippy::cast_possible_truncation)]
fn key(args: &[Value]) -> Result<AdvisoryKey> {
    match args {
        [Value::Int8(key)] => {
            Ok(AdvisoryKey { high: (*key >> 32) as u32, low: *key as u32, kind: 1 })
        }
        [Value::Int4(key1), Value::Int4(key2)] => {
            Ok(AdvisoryKey { high: key1.cast_unsigned(), low: key2.cast_unsigned(), kind: 2 })
        }
        _ => Err(bad_value()),
    }
}

/// `LockAcquire` with a wait: the functions that give `void`.
fn lock(call: &Call<'_>, args: &[Value], share: bool, session: bool) -> Result<Value> {
    call.session.advisory_lock(AdvisoryLock { key: key(args)?, share, session }, true)?;
    Ok(Value::Text(String::new()))
}

/// `LockAcquire` with no wait: the `pg_try_` functions, which give false when the lock is not free.
fn try_lock(call: &Call<'_>, args: &[Value], share: bool, session: bool) -> Result<Value> {
    let lock = AdvisoryLock { key: key(args)?, share, session };
    Ok(Value::Bool(call.session.advisory_lock(lock, false)?))
}

/// `LockRelease` of a lock of the session level. A lock that the session does not hold gives false and a warning.
fn unlock(call: &Call<'_>, args: &[Value], share: bool) -> Result<Value> {
    let lock = AdvisoryLock { key: key(args)?, share, session: true };
    let released = call.session.advisory_unlock(lock)?;
    if !released {
        let mode = if share { "ShareLock" } else { "ExclusiveLock" };
        call.session
            .warn(Error::new(SqlState::WARNING, format!("you don't own a lock of type {mode}")));
    }
    Ok(Value::Bool(released))
}

/// `pg_advisory_unlock_all`: `LockReleaseSession`, which releases the locks of the session level.
fn unlock_all(call: &Call<'_>, _: &[Value]) -> Result<Value> {
    call.session.advisory_unlock_all()?;
    Ok(Value::Text(String::new()))
}

pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    let kernel: Kernel = match src {
        "pg_advisory_lock_int8" | "pg_advisory_lock_int4" => |c, a| lock(c, a, false, true),
        "pg_advisory_xact_lock_int8" | "pg_advisory_xact_lock_int4" => {
            |c, a| lock(c, a, false, false)
        }
        "pg_advisory_lock_shared_int8" | "pg_advisory_lock_shared_int4" => {
            |c, a| lock(c, a, true, true)
        }
        "pg_advisory_xact_lock_shared_int8" | "pg_advisory_xact_lock_shared_int4" => {
            |c, a| lock(c, a, true, false)
        }
        "pg_try_advisory_lock_int8" | "pg_try_advisory_lock_int4" => {
            |c, a| try_lock(c, a, false, true)
        }
        "pg_try_advisory_xact_lock_int8" | "pg_try_advisory_xact_lock_int4" => {
            |c, a| try_lock(c, a, false, false)
        }
        "pg_try_advisory_lock_shared_int8" | "pg_try_advisory_lock_shared_int4" => {
            |c, a| try_lock(c, a, true, true)
        }
        "pg_try_advisory_xact_lock_shared_int8" | "pg_try_advisory_xact_lock_shared_int4" => {
            |c, a| try_lock(c, a, true, false)
        }
        "pg_advisory_unlock_int8" | "pg_advisory_unlock_int4" => |c, a| unlock(c, a, false),
        "pg_advisory_unlock_shared_int8" | "pg_advisory_unlock_shared_int4" => {
            |c, a| unlock(c, a, true)
        }
        "pg_advisory_unlock_all" => unlock_all,
        _ => return None,
    };
    Some(kernel)
}
