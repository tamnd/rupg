//! The set-returning functions of the views `pg_timezone_names` and `pg_timezone_abbrevs`, from `src/backend/utils/adt/datetime.c`. Each one reads the zones at the start of the transaction.

use rupg_common::{Error, Result, SqlState};
use rupg_types::{
    Abbrev, Interval, TimeZone, UNIX_TO_POSTGRES_USECS, USECS_PER_SEC, Value, ZoneAbbrevs,
    local_offset, tz,
};

use crate::Call;

/// `timestamptz_to_time_t`: seconds since 1970-01-01 UTC.
fn unix_seconds(timestamp: i64) -> i64 {
    timestamp / USECS_PER_SEC - UNIX_TO_POSTGRES_USECS / USECS_PER_SEC
}

/// The row of an abbreviation: the name, the offset east of UTC as an interval, and the daylight saving flag.
fn row(abbrev: &str, offset: i32, dst: bool) -> Vec<Value> {
    let offset = Interval { time: i64::from(offset) * USECS_PER_SEC, day: 0, month: 0 };
    vec![Value::text(abbrev), Value::Interval(offset), Value::Bool(dst)]
}

/// `pg_timezone_names`: each zone of the data with its abbreviation, its offset and its daylight saving flag now. A zone whose abbreviation has more than 31 bytes does not give a row.
pub(crate) fn names(call: &Call<'_>, _: &[Value]) -> Result<Vec<Vec<Value>>> {
    let now = unix_seconds(call.session.transaction_start());
    let mut rows = Vec::new();
    for zone in tz::zones() {
        let (offset, dst, abbrev) = zone.local(now);
        if abbrev.len() > 31 {
            continue;
        }
        let mut values = row(abbrev, offset, dst);
        values.insert(0, Value::text(zone.name()));
        rows.push(values);
    }
    Ok(rows)
}

/// `pg_timezone_abbrevs_zone`: each abbreviation of the zone of the session that has only the letters `A` to `Z`, with its meaning now.
pub(crate) fn abbrevs_zone(call: &Call<'_>, _: &[Value]) -> Result<Vec<Vec<Value>>> {
    let zone = call.session.zone()?;
    let now = unix_seconds(call.session.transaction_start());
    let mut rows = Vec::new();
    for abbrev in zone.abbrevs() {
        if !abbrev.bytes().all(|b| b.is_ascii_uppercase()) {
            continue;
        }
        if let Some((offset, dst)) = zone.abbrev_at(abbrev, now) {
            rows.push(row(abbrev, offset, dst));
        }
    }
    Ok(rows)
}

/// `pg_timezone_abbrevs_abbrevs`: each abbreviation of `timezone_abbreviations` in upper case, with its meaning now. An abbreviation for a zone has the meaning that `DetermineTimeZoneAbbrevOffsetTS` gives.
pub(crate) fn abbrevs_abbrevs(call: &Call<'_>, _: &[Value]) -> Result<Vec<Vec<Value>>> {
    let now = unix_seconds(call.session.transaction_start());
    let mut rows = Vec::new();
    for (abbrev, meaning) in ZoneAbbrevs::postgres_default().iter() {
        let upper = abbrev.to_ascii_uppercase();
        let (offset, dst) = match meaning {
            Abbrev::Fixed { offset, dst } => (*offset, *dst),
            Abbrev::Zone(name) => {
                let Some(zone) = tz::load(name) else {
                    return Err(Error::new(
                        SqlState::CONFIG_FILE_ERROR,
                        format!("time zone \"{name}\" not recognized"),
                    )
                    .with_detail(format!(
                        "This time zone name appears in the configuration file for time zone abbreviation \"{abbrev}\"."
                    )));
                };
                zone.abbrev_at(&upper, now).unwrap_or_else(|| {
                    let (offset, _, _) = zone.local(now);
                    let (west, instant) = local_offset(now + i64::from(offset), zone.as_ref());
                    (-west, zone.local(instant).1)
                })
            }
        };
        rows.push(row(&upper, offset, dst));
    }
    Ok(rows)
}
