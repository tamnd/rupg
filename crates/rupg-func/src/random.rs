//! The random functions of `pseudorandomfuncs.c` and the UUID functions of `uuid.c`: `setseed`, `random` of each type, `random_normal`, `gen_random_uuid`, `uuidv7`, `uuid_extract_version` and `uuid_extract_timestamp`, with the casts between `uuid` and `bytea`. The generator is that of `pg_prng.c`, so the numbers after `setseed` are the numbers of PostgreSQL. A generator with no seed takes its seed from [`Session::strong_random`], as `pg_prng_strong_seed` does.

use std::cell::{Cell, RefCell};

use rupg_common::{Error, Result, SqlState};
use rupg_types::prng::Prng;
use rupg_types::{
    DATE_INFINITY, DATE_NEGATIVE_INFINITY, Interval, POSTGRES_EPOCH_JDATE, TIMESTAMP_INFINITY,
    TIMESTAMP_NEGATIVE_INFINITY, UNIX_TO_POSTGRES_USECS, Value,
};

use crate::calendar::format_g;
use crate::datetime::timestamp_plus;
use crate::{Call, Kernel, Session, bad_value};

const NS_PER_MS: i64 = 1_000_000;
const NS_PER_US: i64 = 1000;
const US_PER_MS: i64 = 1000;
/// The microseconds from 1970-01-01 to 2000-01-01, `PG_UNIX_EPOCH_OFFSET_US`.
const UNIX_OFFSET_US: i64 = -UNIX_TO_POSTGRES_USECS;
/// `SUBMS_MINIMAL_STEP_NS`: the smallest step of the clock of `uuidv7`, with the 12 bits of the fraction of a millisecond that Linux gives.
const SUBMS_MINIMAL_STEP_NS: i64 = NS_PER_MS / (1 << 12) + 1;
/// `GREGORIAN_EPOCH_JDATE`: the Julian day of 1582-10-15, the epoch of UUID version 1.
const GREGORIAN_EPOCH_JDATE: i64 = 2_299_161;

/// The random state of a session: the generator of `random`, and the last time of `uuidv7` in nanoseconds since 1970-01-01.
#[derive(Debug, Default)]
pub struct Randomness {
    prng: Option<Prng>,
    /// `pg_global_prng_state`, the generator of `array_shuffle` and `array_sample`, which `setseed` does not change.
    global: Option<Prng>,
    previous_ns: i64,
}

thread_local! {
    /// The state of a session with no server.
    static LOCAL: RefCell<Randomness> = RefCell::new(Randomness::default());
    /// The generator of the random bytes of a session with no server.
    static BYTES: Cell<Option<Prng>> = const { Cell::new(None) };
}

/// Random bytes from a generator with a seed from the clock, as `initialize_prng` seeds the generator when the system gives no random bytes. A session with no server uses these bytes.
#[allow(clippy::cast_sign_loss)]
pub(crate) fn clock_random(clock: i64, buf: &mut [u8]) {
    BYTES.with(|cell| {
        let mut prng = cell.get().unwrap_or_else(|| Prng::seed(clock as u64));
        for chunk in buf.chunks_mut(8) {
            chunk.copy_from_slice(&prng.next_u64().to_ne_bytes()[..chunk.len()]);
        }
        cell.set(Some(prng));
    });
}

fn with_state<T>(session: &dyn Session, f: impl FnOnce(&mut Randomness) -> T) -> T {
    match session.randomness() {
        Some(cell) => f(&mut cell.borrow_mut()),
        None => LOCAL.with(|cell| f(&mut cell.borrow_mut())),
    }
}

/// `pg_prng_strong_seed`.
fn strong_seed(session: &dyn Session) -> Prng {
    let mut bytes = [0; 16];
    session.strong_random(&mut bytes);
    Prng::from_bytes(bytes)
}

/// `initialize_prng` and then `f` with the generator.
fn with_prng<T>(session: &dyn Session, f: impl FnOnce(&mut Prng) -> T) -> T {
    with_state(session, |state| f(state.prng.get_or_insert_with(|| strong_seed(session))))
}

/// `pg_prng_uint64_range` with `pg_global_prng_state`.
pub(crate) fn global_range(session: &dyn Session, rmin: u64, rmax: u64) -> u64 {
    with_state(session, |state| {
        state.global.get_or_insert_with(|| strong_seed(session)).uint64_range(rmin, rmax)
    })
}

fn parameter_error(message: &str) -> Error {
    Error::new(SqlState::INVALID_PARAMETER_VALUE, message)
}

/// `CHECK_RANGE_BOUNDS`.
fn check_bounds<T: PartialOrd>(rmin: T, rmax: T) -> Result<()> {
    if rmin > rmax {
        return Err(parameter_error("lower bound must be less than or equal to upper bound"));
    }
    Ok(())
}

fn finite_bounds(finite: bool) -> Result<()> {
    if finite { Ok(()) } else { Err(parameter_error("lower and upper bounds must be finite")) }
}

/// `setseed`.
fn setseed(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [Value::Float8(seed)] = args else { return Err(bad_value()) };
    let seed = *seed;
    if !(-1.0..=1.0).contains(&seed) {
        return Err(parameter_error(&format!(
            "setseed parameter {} is out of allowed range [-1,1]",
            format_g(seed)
        )));
    }
    with_state(call.session, |state| state.prng = Some(Prng::fseed(seed)));
    Ok(Value::Text(String::new()))
}

/// `drandom`: a value from 0 up to but not including 1.
fn drandom(call: &Call<'_>, _: &[Value]) -> Result<Value> {
    Ok(Value::Float8(with_prng(call.session, Prng::double)))
}

/// `drandom_normal`.
fn drandom_normal(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [Value::Float8(mean), Value::Float8(stddev)] = args else { return Err(bad_value()) };
    let z = with_prng(call.session, Prng::double_normal);
    Ok(Value::Float8(stddev * z + mean))
}

/// `int4random`, `int8random`, `date_random`, `timestamp_random` and `timestamptz_random`.
#[allow(clippy::cast_possible_truncation)]
fn int_random(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let range = |rmin: i64, rmax: i64| -> Result<i64> {
        check_bounds(rmin, rmax)?;
        Ok(with_prng(call.session, |prng| prng.int64_range(rmin, rmax)))
    };
    Ok(match args {
        [Value::Int4(a), Value::Int4(b)] => Value::Int4(range((*a).into(), (*b).into())? as i32),
        [Value::Int8(a), Value::Int8(b)] => Value::Int8(range(*a, *b)?),
        [Value::Date(a), Value::Date(b)] => {
            check_bounds(a, b)?;
            finite_bounds(*a != DATE_NEGATIVE_INFINITY && *b != DATE_INFINITY)?;
            Value::Date(range((*a).into(), (*b).into())? as i32)
        }
        [Value::Timestamp(a), Value::Timestamp(b)]
        | [Value::TimestampTz(a), Value::TimestampTz(b)] => {
            check_bounds(a, b)?;
            finite_bounds(*a != TIMESTAMP_NEGATIVE_INFINITY && *b != TIMESTAMP_INFINITY)?;
            let result = range(*a, *b)?;
            match args[0] {
                Value::Timestamp(_) => Value::Timestamp(result),
                _ => Value::TimestampTz(result),
            }
        }
        _ => return Err(bad_value()),
    })
}

/// `numeric_random`.
fn numeric_random(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [Value::Numeric(rmin), Value::Numeric(rmax)] = args else { return Err(bad_value()) };
    let result = with_prng(call.session, |prng| rupg_types::Numeric::random(prng, rmin, rmax))?;
    Ok(Value::Numeric(result))
}

/// `uuid_set_version`: the version in the top four bits of byte 6, and the variant of RFC 9562 in the top two bits of byte 8.
fn set_version(uuid: &mut [u8; 16], version: u8) {
    uuid[6] = (uuid[6] & 0x0f) | (version << 4);
    uuid[8] = (uuid[8] & 0x3f) | 0x80;
}

/// `gen_random_uuid`: a UUID of version 4.
fn gen_random_uuid(call: &Call<'_>, _: &[Value]) -> Result<Value> {
    let mut uuid = [0; 16];
    call.session.strong_random(&mut uuid);
    set_version(&mut uuid, 4);
    Ok(Value::Uuid(uuid))
}

/// `get_real_time_ns_ascending`: the time in nanoseconds since 1970-01-01, at least `SUBMS_MINIMAL_STEP_NS` after the last time of the session.
fn time_ns_ascending(session: &dyn Session) -> i64 {
    let mut ns = (session.clock() + UNIX_OFFSET_US) * NS_PER_US;
    with_state(session, |state| {
        if state.previous_ns + SUBMS_MINIMAL_STEP_NS >= ns {
            ns = state.previous_ns + SUBMS_MINIMAL_STEP_NS;
        }
        state.previous_ns = ns;
    });
    ns
}

/// `generate_uuidv7`: 48 bits of milliseconds since 1970-01-01, 12 bits of the fraction of the millisecond, and random bits.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn generate_uuidv7(session: &dyn Session, unix_ts_ms: i64, sub_ms: i64) -> Value {
    let mut uuid = [0; 16];
    let ms = (unix_ts_ms as u64).to_be_bytes();
    uuid[..6].copy_from_slice(&ms[2..]);
    let precision = (sub_ms * (1 << 12) / NS_PER_MS) as u32;
    uuid[6] = (precision >> 8) as u8;
    uuid[7] = precision as u8;
    session.strong_random(&mut uuid[8..]);
    set_version(&mut uuid, 7);
    Value::Uuid(uuid)
}

/// `uuidv7`.
fn uuidv7(call: &Call<'_>, _: &[Value]) -> Result<Value> {
    let ns = time_ns_ascending(call.session);
    Ok(generate_uuidv7(call.session, ns / NS_PER_MS, ns % NS_PER_MS))
}

/// `uuidv7_interval`: a UUID of version 7 with the time now moved by an interval.
fn uuidv7_interval(call: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [Value::Interval(shift)] = args else { return Err(bad_value()) };
    if *shift == Interval::INFINITY || *shift == Interval::NEGATIVE_INFINITY {
        return Err(Error::new(
            SqlState::DATETIME_VALUE_OUT_OF_RANGE,
            "interval out of range for UUID version 7",
        )
        .with_detail("UUID version 7 does not support infinite intervals."));
    }
    let ns = time_ns_ascending(call.session);
    let zone = call.session.zone()?;
    let ts = timestamp_plus(ns / NS_PER_US - UNIX_OFFSET_US, shift, Some(zone.as_ref()))?;
    let max = ((1i64 << 48) - 1) * US_PER_MS - UNIX_OFFSET_US;
    if ts < -UNIX_OFFSET_US || ts > max {
        return Err(Error::new(
            SqlState::DATETIME_VALUE_OUT_OF_RANGE,
            "timestamp out of range for UUID version 7",
        )
        .with_detail(
            "UUID version 7 supports timestamps from 1970-01-01 to approximately year 10889.",
        ));
    }
    let us = ts + UNIX_OFFSET_US;
    Ok(generate_uuidv7(call.session, us / US_PER_MS, (us % US_PER_MS) * NS_PER_US + ns % NS_PER_US))
}

fn uuid_arg(args: &[Value]) -> Result<&[u8; 16]> {
    match args {
        [Value::Uuid(uuid)] => Ok(uuid),
        _ => Err(bad_value()),
    }
}

/// `uuid_extract_version`: null for a UUID that is not of the variant of RFC 9562.
fn uuid_extract_version(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let uuid = uuid_arg(args)?;
    if uuid[8] & 0xc0 != 0x80 {
        return Ok(Value::Null);
    }
    Ok(Value::Int2(i16::from(uuid[6] >> 4)))
}

/// `uuid_extract_timestamp`: the time of a UUID of version 1 or 7, and null for the other UUIDs.
#[allow(clippy::cast_possible_wrap)]
fn uuid_extract_timestamp(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let uuid = uuid_arg(args)?;
    if uuid[8] & 0xc0 != 0x80 {
        return Ok(Value::Null);
    }
    let byte = |at: usize, shift: u32| u64::from(uuid[at]) << shift;
    Ok(match uuid[6] >> 4 {
        1 => {
            let tms = byte(0, 24)
                + byte(1, 16)
                + byte(2, 8)
                + byte(3, 0)
                + byte(4, 40)
                + byte(5, 32)
                + (u64::from(uuid[6] & 0xf) << 56)
                + byte(7, 48);
            let offset =
                (i64::from(POSTGRES_EPOCH_JDATE) - GREGORIAN_EPOCH_JDATE) * 86_400 * 1_000_000;
            Value::TimestampTz((tms / 10) as i64 - offset)
        }
        7 => {
            let tms =
                byte(5, 0) + byte(4, 8) + byte(3, 16) + byte(2, 24) + byte(1, 32) + byte(0, 40);
            Value::TimestampTz((tms * US_PER_MS as u64) as i64 - UNIX_OFFSET_US)
        }
        _ => Value::Null,
    })
}

/// `bytea_uuid`: the cast of 16 bytes to a UUID.
fn bytea_uuid(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    let [Value::Bytea(bytes)] = args else { return Err(bad_value()) };
    let uuid = <[u8; 16]>::try_from(bytes.as_slice()).map_err(|_| {
        Error::new(SqlState::INVALID_BINARY_REPRESENTATION, "invalid input length for type uuid")
            .with_detail(format!("Expected 16 bytes, got {}.", bytes.len()))
    })?;
    Ok(Value::Uuid(uuid))
}

/// `uuid_bytea`: the 16 bytes of a UUID.
fn uuid_bytea(_: &Call<'_>, args: &[Value]) -> Result<Value> {
    Ok(Value::Bytea(uuid_arg(args)?.to_vec()))
}

pub(crate) fn by_src(src: &str) -> Option<Kernel> {
    Some(match src {
        "setseed" => setseed,
        "drandom" => drandom,
        "drandom_normal" => drandom_normal,
        "int4random" | "int8random" | "date_random" | "timestamp_random" | "timestamptz_random" => {
            int_random
        }
        "numeric_random" => numeric_random,
        "gen_random_uuid" => gen_random_uuid,
        "uuidv7" => uuidv7,
        "uuidv7_interval" => uuidv7_interval,
        "uuid_extract_version" => uuid_extract_version,
        "uuid_extract_timestamp" => uuid_extract_timestamp,
        "bytea_uuid" => bytea_uuid,
        "uuid_bytea" => uuid_bytea,
        _ => return None,
    })
}
