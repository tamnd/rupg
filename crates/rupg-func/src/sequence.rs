//! The sequence functions of `sequence.c`: `nextval`, `currval`, `lastval` and `setval`, with the cache of each session, which is `SeqTable` of PostgreSQL.
//!
//! The row of the file of a sequence is not in a transaction. The session changes it with [`Session::change_sequence`], so a value that a transaction fetched stays used after a rollback, as in PostgreSQL. rupg keeps no WAL and makes no checkpoints, so `log_cnt` follows the rules of `nextval_internal` for a server without a checkpoint.

use std::collections::HashMap;

use rupg_catalog::{RelKind, SequenceData, SequenceInfo};
use rupg_common::{Error, Result, SqlState};
use rupg_types::{Record, Value, oid};

use crate::{Call, Kernel, Session, bad_value, not_yet, reg};

/// `SEQ_LOG_VALS`: the number of values that `nextval` fetches ahead each time it writes a WAL record.
const SEQ_LOG_VALS: i64 = 32;

/// The cache of the sequences of a session, `seqhashtab` and `last_used_seq` of `sequence.c`.
#[derive(Debug, Default)]
pub struct Sequences {
    table: HashMap<u32, Elm>,
    /// The OID of the sequence of the last call of `nextval`.
    last_used: Option<u32>,
}

/// An entry of the cache, `SeqTableData`.
#[derive(Clone, Copy, Debug, Default)]
struct Elm {
    /// The file of the sequence when the session last read it. A new file drops the values of the cache.
    file: u32,
    /// True when `last` is the value of `currval`.
    last_valid: bool,
    /// The last value that `nextval` gave.
    last: i64,
    /// The last value that the session fetched into its cache.
    cached: i64,
    /// The increment of the sequence when the session fetched the values.
    increment: i64,
}

impl Sequences {
    /// `ResetSequenceCaches`: `DISCARD SEQUENCES` and `DISCARD ALL` drop the cache and the state of `currval` and `lastval`.
    pub fn discard(&mut self) {
        *self = Sequences::default();
    }

    /// The part of `init_sequence` that finds the entry of the cache. When the file of the sequence changed, the values of the cache are dropped, but the state of `currval` stays.
    fn init(&mut self, relid: u32, file: u32) -> &mut Elm {
        let elm = self.table.entry(relid).or_default();
        if elm.file != file {
            elm.file = file;
            elm.cached = elm.last;
        }
        elm
    }
}

/// The OID argument at `index`.
fn oid_arg(args: &[Value], index: usize) -> Result<u32> {
    args.get(index).and_then(Value::as_oid).ok_or_else(bad_value)
}

/// The `bigint` argument at `index`.
fn int8_arg(args: &[Value], index: usize) -> Result<i64> {
    args.get(index).and_then(Value::as_i64).ok_or_else(bad_value)
}

/// The cache of the session, or `0A000` for a session with no server.
fn local(session: &dyn Session) -> Result<&std::cell::RefCell<Sequences>> {
    session.local_sequences().ok_or_else(|| not_yet("a sequence in a session with no server"))
}

/// `sequence_open`: the name and the `pg_sequence` row of a sequence. A relation of another kind is `42809`.
fn open(relid: u32, session: &dyn Session) -> Result<(&str, &SequenceInfo)> {
    let Some((name, _)) = reg::class_name(relid, session) else {
        return Err(Error::internal(format!("could not open relation with OID {relid}")));
    };
    let info = session
        .catalog()
        .and_then(|c| c.relation(relid))
        .filter(|r| r.kind == RelKind::Sequence)
        .and_then(|r| r.sequence.as_ref());
    match info {
        Some(info) => Ok((name, info)),
        None => {
            Err(Error::new(SqlState::WRONG_OBJECT_TYPE, format!("\"{name}\" is not a sequence")))
        }
    }
}

/// The row of the file of a sequence: the row that the sessions changed, or the row of the file when it was made.
pub(crate) fn row(info: &SequenceInfo, session: &dyn Session) -> SequenceData {
    session.sequence_row(info.file).unwrap_or(info.data)
}

/// The check of `pg_class_aclcheck` for the privileges of `mask` on a sequence.
fn check(relid: u32, name: &str, mask: u32, session: &dyn Session) -> Result<()> {
    if crate::acl::sequence_allowed(relid, mask, session)? {
        return Ok(());
    }
    Err(Error::new(
        SqlState::INSUFFICIENT_PRIVILEGE,
        format!("permission denied for sequence {name}"),
    ))
}

/// `PreventCommandIfReadOnly`: a read-only transaction cannot change a sequence or a large object.
pub(crate) fn check_writable(command: &str, session: &dyn Session) -> Result<()> {
    if session.setting("transaction_read_only").as_deref() == Some("on") {
        return Err(Error::new(
            SqlState::READ_ONLY_SQL_TRANSACTION,
            format!("cannot execute {command} in a read-only transaction"),
        ));
    }
    Ok(())
}

/// True for a temporary sequence, `rd_islocaltemp`: a read-only transaction can change it.
fn local_temp(relid: u32, session: &dyn Session) -> bool {
    session.catalog().is_some_and(|catalog| {
        catalog.relation(relid).is_some_and(|rel| catalog.is_temp_schema(rel.namespace))
    })
}

/// `nextval_oid`: `nextval_internal` with the check of the privileges. The analyzer gives a second argument `true` for the default of an identity column, which is `NextValueExpr`: that call does not check the privileges.
fn nextval(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let relid = oid_arg(args, 0)?;
    let session = call.session;
    let (name, info) = open(relid, session)?;
    let mut sequences = local(session)?.borrow_mut();
    let elm = *sequences.init(relid, info.file);
    if !matches!(args.get(1), Some(Value::Bool(true))) {
        check(relid, name, crate::acl::USAGE_UPDATE, session)?;
    }
    if !local_temp(relid, session) {
        check_writable("nextval()", session)?;
    }
    if elm.last != elm.cached {
        let elm = sequences.init(relid, info.file);
        elm.last += elm.increment;
        let last = elm.last;
        sequences.last_used = Some(relid);
        return Ok(Value::Int8(last));
    }
    let (incby, maxv, minv, cache, cycle) =
        (info.increment, info.max, info.min, info.cache, info.cycle);
    let mut fetched = (0, 0);
    session.change_sequence(info.file, info.data, &mut |seq| {
        let (mut last, mut next, mut result) = (seq.last_value, seq.last_value, seq.last_value);
        let mut fetch = cache;
        let mut log = seq.log_cnt;
        let mut rescnt = 0;
        if !seq.is_called {
            rescnt += 1;
            fetch -= 1;
        }
        if log < fetch || !seq.is_called {
            fetch += SEQ_LOG_VALS;
            log = fetch;
        }
        while fetch > 0 {
            if incby > 0 {
                if (maxv >= 0 && next > maxv - incby) || (maxv < 0 && next + incby > maxv) {
                    if rescnt > 0 {
                        break;
                    }
                    if !cycle {
                        return Err(Error::new(
                            SqlState::SEQUENCE_GENERATOR_LIMIT_EXCEEDED,
                            format!(
                                "nextval: reached maximum value of sequence \"{name}\" ({maxv})"
                            ),
                        ));
                    }
                    next = minv;
                } else {
                    next += incby;
                }
            } else if (minv < 0 && next < minv - incby) || (minv >= 0 && next + incby < minv) {
                if rescnt > 0 {
                    break;
                }
                if !cycle {
                    return Err(Error::new(
                        SqlState::SEQUENCE_GENERATOR_LIMIT_EXCEEDED,
                        format!("nextval: reached minimum value of sequence \"{name}\" ({minv})"),
                    ));
                }
                next = maxv;
            } else {
                next += incby;
            }
            fetch -= 1;
            if rescnt < cache {
                log -= 1;
                rescnt += 1;
                last = next;
                if rescnt == 1 {
                    result = next;
                }
            }
        }
        log -= fetch;
        *seq = SequenceData { last_value: last, log_cnt: log, is_called: true };
        fetched = (result, last);
        Ok(())
    })?;
    let elm = sequences.init(relid, info.file);
    elm.increment = incby;
    elm.last = fetched.0;
    elm.cached = fetched.1;
    elm.last_valid = true;
    sequences.last_used = Some(relid);
    Ok(Value::Int8(fetched.0))
}

/// `currval_oid`: the last value of `nextval` for the sequence in this session.
fn currval(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let relid = oid_arg(args, 0)?;
    let session = call.session;
    let (name, info) = open(relid, session)?;
    let mut sequences = local(session)?.borrow_mut();
    let elm = *sequences.init(relid, info.file);
    check(relid, name, crate::acl::SELECT_USAGE, session)?;
    if !elm.last_valid {
        return Err(Error::new(
            SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
            format!("currval of sequence \"{name}\" is not yet defined in this session"),
        ));
    }
    Ok(Value::Int8(elm.last))
}

/// `lastval`: the last value of `nextval` for any sequence in this session. A sequence that was dropped after the call has no last value.
fn lastval(call: &Call<'_>, _: &[Value]) -> Result<Value> {
    let session = call.session;
    let sequences = local(session)?.borrow();
    let undefined = || {
        Error::new(
            SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
            "lastval is not yet defined in this session",
        )
    };
    let relid = sequences.last_used.ok_or_else(undefined)?;
    let Some((name, _)) = reg::class_name(relid, session) else {
        return Err(undefined());
    };
    check(relid, name, crate::acl::SELECT_USAGE, session)?;
    Ok(Value::Int8(sequences.table.get(&relid).map_or(0, |elm| elm.last)))
}

/// `SetSequence`: the next call of `nextval` gives the value after `next`, or `next` itself when `iscalled` is false.
fn set_sequence(call: &Call<'_>, relid: u32, next: i64, iscalled: bool) -> Result<Value> {
    let session = call.session;
    let (name, info) = open(relid, session)?;
    let mut sequences = local(session)?.borrow_mut();
    sequences.init(relid, info.file);
    check(relid, name, crate::acl::UPDATE, session)?;
    if !local_temp(relid, session) {
        check_writable("setval()", session)?;
    }
    if next < info.min || next > info.max {
        return Err(Error::new(
            SqlState::NUMERIC_VALUE_OUT_OF_RANGE,
            format!(
                "setval: value {next} is out of bounds for sequence \"{name}\" ({}..{})",
                info.min, info.max
            ),
        ));
    }
    let elm = sequences.init(relid, info.file);
    if iscalled {
        elm.last = next;
        elm.last_valid = true;
    }
    elm.cached = elm.last;
    session.change_sequence(info.file, info.data, &mut |seq| {
        *seq = SequenceData { last_value: next, log_cnt: 0, is_called: iscalled };
        Ok(())
    })?;
    Ok(Value::Int8(next))
}

/// `setval_oid`: `setval(regclass, bigint)`.
fn setval(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    set_sequence(call, oid_arg(args, 0)?, int8_arg(args, 1)?, true)
}

/// `setval3_oid`: `setval(regclass, bigint, boolean)`, which `pg_dump` calls.
fn setval3(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let iscalled = match args.get(2) {
        Some(Value::Bool(b)) => *b,
        _ => return Err(bad_value()),
    };
    set_sequence(call, oid_arg(args, 0)?, int8_arg(args, 1)?, iscalled)
}

/// `pg_sequence_last_value(regclass)`: the last value of the file of a sequence when `is_called` is true, else null. The result is null also for a sequence on which the current user has no `SELECT` and no `USAGE`, for the view `pg_sequences`. The errors are those of `init_sequence`.
pub(crate) fn last_value(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let relid = oid_arg(args, 0)?;
    let session = call.session;
    let (_, info) = open(relid, session)?;
    if let Some(sequences) = session.local_sequences() {
        sequences.borrow_mut().init(relid, info.file);
    }
    if !crate::acl::sequence_allowed(relid, crate::acl::SELECT_USAGE, session)? {
        return Ok(Value::Null);
    }
    let seq = row(info, session);
    Ok(if seq.is_called { Value::Int8(seq.last_value) } else { Value::Null })
}

/// `pg_sequence_parameters(oid)`: the row of `pg_sequence` of a sequence, for the views of `information_schema`. The current user needs `SELECT`, `UPDATE` or `USAGE` on the sequence.
fn parameters(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let relid = oid_arg(args, 0)?;
    let session = call.session;
    let mask = crate::acl::SELECT_USAGE | crate::acl::UPDATE;
    if !crate::acl::sequence_allowed(relid, mask, session)? {
        let name = reg::class_name(relid, session).map_or("", |(name, _)| name);
        return Err(Error::new(
            SqlState::INSUFFICIENT_PRIVILEGE,
            format!("permission denied for sequence {name}"),
        ));
    }
    let info = session
        .catalog()
        .and_then(|c| c.relation(relid))
        .filter(|r| r.kind == RelKind::Sequence)
        .and_then(|r| r.sequence.as_ref());
    let Some(info) = info else {
        return Err(Error::internal(format!("cache lookup failed for sequence {relid}")));
    };
    let values = vec![
        Value::Int8(info.start),
        Value::Int8(info.min),
        Value::Int8(info.max),
        Value::Int8(info.increment),
        Value::Bool(info.cycle),
        Value::Int8(info.cache),
        Value::Oid(info.ty),
    ];
    let types = vec![oid::INT8, oid::INT8, oid::INT8, oid::INT8, oid::BOOL, oid::INT8, oid::OID];
    Ok(Value::Record(Box::new(Record::new(types, values))))
}

/// The kernels of the sequence functions by `prosrc`.
pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    let kernel: Kernel = match src {
        "nextval_oid" => nextval,
        "currval_oid" => currval,
        "lastval" => lastval,
        "setval_oid" => setval,
        "setval3_oid" => setval3,
        "pg_sequence_last_value" => last_value,
        "pg_sequence_parameters" => parameters,
        _ => return None,
    };
    Some(kernel)
}
