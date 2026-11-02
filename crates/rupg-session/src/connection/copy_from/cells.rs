//! The bulk load of `COPY FROM` (spec/10 section 10.8): the fields of the rows become typed cells on the threads of the server, and the cells go straight into the encoded columns of the table with no `Value` for each field. Only the input functions that need no session run on the threads, and a row with a field that the fast input does not read, or that the input function does not accept, sends the whole batch to the path of `NextCopyFrom`, which gives the errors of PostgreSQL.

use rupg_column::segment::Kind;
use rupg_platform::{Job, Tasks};
use rupg_types::oid;

use super::Row;
use crate::heap::{Cells, Values};

/// The rows of a batch that each thread reads, at the least.
const JOB_ROWS: usize = 1024;

/// The days from 1 January 1970 to 1 January 2000, `POSTGRES_EPOCH_JDATE - UNIX_EPOCH_JDATE`.
const EPOCH_DAYS: i64 = 10_957;

/// The microseconds of a day.
const DAY_MICROS: i64 = 86_400_000_000;

/// The input function of a column of a bulk load, as `NextCopyFrom` calls it for the type of the column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Parse {
    Int2,
    Int4,
    Int8,
    Date,
    Timestamp,
    Text,
    Varchar(i32),
    Bpchar(i32),
}

impl Parse {
    /// The input of a column with the type `ty` and the type modifier `typmod`, or `None` for a type that the bulk load does not read.
    pub(super) fn of(ty: u32, typmod: i32) -> Option<Parse> {
        Some(match ty {
            oid::INT2 => Parse::Int2,
            oid::INT4 => Parse::Int4,
            oid::INT8 => Parse::Int8,
            oid::DATE => Parse::Date,
            oid::TIMESTAMP => Parse::Timestamp,
            oid::TEXT => Parse::Text,
            oid::VARCHAR => Parse::Varchar(typmod),
            oid::BPCHAR => Parse::Bpchar(typmod),
            _ => return None,
        })
    }

    /// The kind of the column, as the frozen columns of the heap keep its values.
    fn kind(self) -> Kind {
        match self {
            Parse::Int2 => Kind::Int2,
            Parse::Int4 => Kind::Int4,
            Parse::Int8 => Kind::Int8,
            Parse::Date => Kind::Date,
            Parse::Timestamp => Kind::Timestamp,
            Parse::Text | Parse::Varchar(_) | Parse::Bpchar(_) => Kind::Text,
        }
    }

    /// Puts the value of the field `field` on `values`. False when the field needs the input function of the session: a form that the fast input does not read, or a value that the input function does not accept.
    fn put(self, field: &[u8], values: &mut Values) -> bool {
        let Ok(text) = std::str::from_utf8(field) else { return false };
        match (self, values) {
            (Parse::Int2, Values::Ints(values)) => {
                rupg_types::int2_in(text).map(|v| values.push(i64::from(v))).is_ok()
            }
            (Parse::Int4, Values::Ints(values)) => {
                rupg_types::int4_in(text).map(|v| values.push(i64::from(v))).is_ok()
            }
            (Parse::Int8, Values::Ints(values)) => {
                rupg_types::int8_in(text).map(|v| values.push(v)).is_ok()
            }
            (Parse::Date, Values::Ints(values)) => {
                iso_date(field).map(|days| values.push(days)).is_some()
            }
            (Parse::Timestamp, Values::Ints(values)) => {
                iso_timestamp(field).map(|micros| values.push(micros)).is_some()
            }
            (Parse::Text, Values::Bytes(bytes, ends)) => {
                bytes.extend_from_slice(field);
                ends.push(bytes.len());
                true
            }
            (Parse::Varchar(typmod), Values::Bytes(bytes, ends)) => {
                rupg_types::varchar_in(text, typmod)
                    .map(|text| {
                        bytes.extend_from_slice(text.as_bytes());
                        ends.push(bytes.len());
                    })
                    .is_ok()
            }
            (Parse::Bpchar(typmod), Values::Bytes(bytes, ends)) => {
                rupg_types::bpchar_in(text, typmod)
                    .map(|text| {
                        bytes.extend_from_slice(text.as_bytes());
                        ends.push(bytes.len());
                    })
                    .is_ok()
            }
            _ => false,
        }
    }
}

/// The digits of `bytes` as a number, or `None` when a byte is not a digit.
fn digits(bytes: &[u8]) -> Option<i64> {
    bytes.iter().try_fold(0, |n, &b| b.is_ascii_digit().then(|| n * 10 + i64::from(b - b'0')))
}

/// The days since 1 January 2000 of a date in the form `YYYY-MM-DD`, which `date_in` reads the same way with each `DateStyle`, or `None` for another form or a day that is not in its month.
fn iso_date(field: &[u8]) -> Option<i64> {
    let [y0, y1, y2, y3, b'-', m0, m1, b'-', d0, d1] = *field else { return None };
    let year = digits(&[y0, y1, y2, y3])?;
    let month = digits(&[m0, m1])?;
    let day = digits(&[d0, d1])?;
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let last = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return None,
    };
    if year == 0 || !(1..=last).contains(&day) {
        return None;
    }
    // `date2j` with the days of the proleptic Gregorian calendar from 1 March of year 0.
    let (y, m) = if month <= 2 { (year - 1, month + 9) } else { (year, month - 3) };
    let era = y.div_euclid(400);
    let of_era = y - era * 400;
    let of_year = (153 * m + 2) / 5 + day - 1;
    let of_cycle = of_era * 365 + of_era / 4 - of_era / 100 + of_year;
    Some(era * 146_097 + of_cycle - 719_468 - EPOCH_DAYS)
}

/// The microseconds since 1 January 2000 of a timestamp in the form `YYYY-MM-DD HH:MM:SS`, or `None` for another form, such as a fraction of a second, which needs the rounding of the type modifier.
fn iso_timestamp(field: &[u8]) -> Option<i64> {
    let (date, [b' ', h0, h1, b':', i0, i1, b':', s0, s1]) = field.split_at_checked(10)? else {
        return None;
    };
    let days = iso_date(date)?;
    let (hour, minute, second) = (digits(&[*h0, *h1])?, digits(&[*i0, *i1])?, digits(&[*s0, *s1])?);
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    Some(days * DAY_MICROS + ((hour * 60 + minute) * 60 + second) * 1_000_000)
}

/// The cells of the fields of `rows`, with `parse` for each column, on the threads of `tasks`. `None` when a row has a null, a field that the fast input does not read, or not one field for each column.
pub(super) fn read(rows: &[Row], parse: &[Parse], tasks: Option<&dyn Tasks>) -> Option<Cells> {
    let jobs = tasks.map_or(1, |tasks| tasks.parallelism().min(rows.len() / JOB_ROWS).max(1));
    let size = rows.len().div_ceil(jobs).max(1);
    let read_part = |rows: &[Row]| -> Option<Vec<Values>> {
        let mut columns: Vec<Values> = parse.iter().map(|parse| Values::of(parse.kind())).collect();
        for row in rows {
            if row.fields.len() != parse.len() || !row.defaults.is_empty() {
                return None;
            }
            for ((field, parse), values) in row.fields.iter().zip(parse).zip(&mut columns) {
                if !parse.put(field?, values) {
                    return None;
                }
            }
        }
        Some(columns)
    };
    let mut parts: Vec<Option<Vec<Values>>> = Vec::new();
    match tasks {
        Some(tasks) if jobs > 1 => {
            parts.resize_with(rows.len().div_ceil(size), || None);
            let read_part = &read_part;
            let work: Vec<Job<'_>> = rows
                .chunks(size)
                .zip(&mut parts)
                .map(|(rows, part)| Box::new(move || *part = read_part(rows)) as Job<'_>)
                .collect();
            tasks.run_all(work);
        }
        _ => parts.push(read_part(rows)),
    }
    let mut parts = parts.into_iter();
    let mut columns = parts.next()??;
    for part in parts {
        for (values, mut more) in columns.iter_mut().zip(part?) {
            values.append(&mut more);
        }
    }
    let columns = parse.iter().map(|parse| parse.kind()).zip(columns).collect();
    Some(Cells { rows: rows.len(), columns })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fast_dates_and_timestamps_are_the_values_of_the_input_functions() {
        assert_eq!(iso_date(b"2000-01-01"), Some(0));
        assert_eq!(iso_date(b"2000-03-01"), Some(60));
        assert_eq!(iso_date(b"1999-12-31"), Some(-1));
        assert_eq!(iso_date(b"2013-07-15"), Some(4944));
        assert_eq!(iso_date(b"1971-01-01"), Some(-10_592));
        assert_eq!(iso_date(b"2024-02-29"), Some(8825));
        // A day that is not in its month, or another form, goes to the input function, which gives its error or reads it.
        assert_eq!(iso_date(b"2023-02-29"), None);
        assert_eq!(iso_date(b"1900-02-29"), None);
        assert_eq!(iso_date(b"2013-13-01"), None);
        assert_eq!(iso_date(b"2013-7-15"), None);
        assert_eq!(iso_date(b"0000-01-01"), None);
        assert_eq!(iso_timestamp(b"2000-01-01 00:00:01"), Some(1_000_000));
        assert_eq!(iso_timestamp(b"2013-07-14 20:38:47"), Some(4943 * DAY_MICROS + 74_327_000_000));
        assert_eq!(iso_timestamp(b"2013-07-14 24:00:00"), None);
        assert_eq!(iso_timestamp(b"2013-07-14 20:38:47.5"), None);
        assert_eq!(iso_timestamp(b"2013-07-14T20:38:47"), None);
    }
}
