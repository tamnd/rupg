//! The value model, `Datum`, `Vector` of 1024 values, `TypeId`, the text and binary forms of the 197 types, numeric, date and time, JSON, arrays, ranges.
//!
//! See `spec/07-sql-types-and-catalog.md` and `spec/22-crate-layout.md` section 22.4.
//!
//! A client reads the type of a column from the OID and the typmod in `RowDescription`, and it parses each value with the rules of that type. So each input function here takes what the function of PostgreSQL takes and refuses what it refuses, with the same SQLSTATE and the same message, and each output function writes the same bytes. The server at the pin is the reference.
//!
//! # What is here
//!
//! [`oid`], one constant for each type in `pg_type.dat` at the pin and for its array type, and [`TypeInfo`], the row of `pg_type` that the protocol needs. [`PgType`] is an OID with a typmod, and [`typmod`] encodes and decodes the typmod of `varchar`, `numeric` and `interval`. The row types of the shared catalogs, such as `pg_database`, come from the catalog headers and not from `pg_type.dat`, so they are not here.
//!
//! [`Value`], one value of any type in the engine. The expression that gives a value has its type, so the value does not carry it.
//!
//! The text forms of `bool`, `"char"`, `name`, `int2`, `int4`, `int8`, `oid`, `float4`, `float8`, `bytea` and `uuid`. An input function takes the string and gives the value or a [`TypeError`]. An output function appends to a buffer, so the row encoder can write a whole row into one buffer with no allocation per value. [`float8_out`] and [`float4_out`] take `extra_float_digits`: above zero they give the shortest text that reads back to the same value, and at zero or below they give the `%g` text of old servers.
//!
//! `text`, `varchar(n)`, `character(n)` and `json`. The text form of `text` is the string. [`varchar_in`] and [`bpchar_in`] take the typmod, refuse a value that is too long, and remove the spaces after the length. [`bpchar_in`] also pads a short value with spaces. [`varchar_coerce`] and [`bpchar_coerce`] are the length casts, which cut with no error when the cast is explicit. [`json_in`] checks the syntax with the error details of PostgreSQL and gives the string. [`Recv::text`] is the encoding check of the receive function of each string type.
//!
//! [`Numeric`], a `numeric` value in the layout of PostgreSQL, with [`numeric_in`], [`numeric_out`], [`numeric_recv`] and [`numeric_send`]. The input and the receive function take the typmod and round to it as PostgreSQL does. [`decimal_out`] and [`decimal_send`] write a `numeric(p, s)` value with p up to 38 that is kept as a scaled `i128`, with no allocation. [`numeric_layout`] packs a value into bytes that sort as the numbers do.
//!
//! `date`, `time`, `timetz`, `timestamp`, `timestamptz` and `interval` in the layout of PostgreSQL: days or microseconds since 2000-01-01, with the infinities at the ends of the integer range. The output functions take the `DateStyle` as a [`DateFormat`] and the `IntervalStyle` as an [`IntervalStyle`], and [`timestamptz_out`] takes a [`TimeZone`] that gives the offset and the abbreviation at an instant. The receive functions check the range and round to the typmod. [`date2j`] and [`j2date`] convert between a calendar date and a Julian day.
//!
//! The text input of the same types, [`date_in`], [`time_in`], [`timetz_in`], [`timestamp_in`], [`timestamptz_in`] and [`interval_in`], is a port of the decoder in `datetime.c`. It reads the session state from a [`DateTimeInput`]: the field order, the time zone, the zone names, the time zone abbreviations and the time of the transaction start. [`ZoneAbbrevs`] reads a file of `src/timezone/tznames`, and [`ZoneAbbrevs::postgres_default`] is the `Default` set from `vendor/postgres-19`.
//!
//! [`Array`], an array of any element type with up to [`MAXDIM`] dimensions and any lower bounds. [`array_in`], [`array_out`], [`array_recv`] and [`array_send`] take the function of the element type as a closure and give the errors of PostgreSQL, also the error of the element. The binary input refuses an element type that is not the expected type, and [`format_type`] gives the names in that error. `int2vector` and `oidvector` are arrays with the lower bound 0 and a text form with spaces, in [`int2vector_in`], [`oidvector_in`] and the functions beside them.
//!
//! [`Inet`], a value of `inet` or `cidr`, with [`inet_in`], [`inet_out`], [`inet_recv`] and [`inet_send`], which take a flag for `cidr`. [`network_cmp`] is the order of the B-tree operator class.
//!
//! [`RegKind`], the OID alias types such as `regclass` and `regtype`. [`reg_in`] reads a number or `-` as PostgreSQL does and gives any other string as a name, and [`reg_out_oid`] writes OID 0 and an OID with no object. The catalog finds the names: it splits a name such as `schema.table` with [`qualified_name_list`] and writes the names of the objects.
//!
//! [`Recv`], the binary input of a `Bind` parameter, with the errors of PostgreSQL when the value is too short or too long. The binary output of the other types is the value in big-endian bytes.
//!
//! [`unicode`], the four Unicode normalization forms, and [`tz`], the time zones of the IANA data.
//!
//! # Known differences
//!
//! The input functions take a string in the server encoding, which is UTF-8. The caller converts from the client encoding first.
//!
//! [`tz::Zone`] implements [`TimeZone`] with the rules of the IANA data, and [`tz::TzDatabase`] implements [`ZoneLookup`]. [`FixedZone`] and [`NoZones`] are for the tests and for a caller without the data.
//!
//! [`pg_ndistinct_in`] and [`pg_dependencies_in`] read the JSON text of the types of the extended statistics with the details of the errors of PostgreSQL, and give the bytes of the value. [`pg_ndistinct_out`] and [`pg_dependencies_out`] write the text from the bytes.
//!
//! PostgreSQL parses `json` by recursion and stops a deep value with `stack depth limit exceeded` when it reaches `max_stack_depth`. [`json_in`] uses no recursion and takes a value at any depth.
//!
//! The text and binary forms are lifted from `crates/rudb-pgtypes` of tamnd/rudb at f5f7065a (spec/04 section 4.9). The `DataRow` encoder and the `Bind` decoder of that crate need the executor, so they come with rupg-server.

#![forbid(unsafe_code)]

mod array;
mod binary;
mod datetime;
mod datum;
mod error;
mod float;
mod generated;
mod json;
mod jsonb;
mod network;
mod number;
mod numeric;
pub mod numeric_layout;
mod reg;
mod scalar;
mod stats;
mod string;
mod typeid;
mod types;
pub mod typmod;
pub mod tz;
pub mod unicode;
mod value;

pub use array::{
    Array, ArrayDim, MAX_ARRAY_SIZE, MAXDIM, array_in, array_out, array_recv, array_send,
    int2vector_in, int2vector_out, int2vector_recv, int2vector_send, oidvector_in, oidvector_out,
    oidvector_recv, oidvector_send,
};
pub use binary::{Recv, name_recv};
pub use datetime::{
    Abbrev, AbbrevMeaning, DATE_INFINITY, DATE_NEGATIVE_INFINITY, DateFormat, DateOrder, DateStyle,
    DateTimeInput, FixedZone, Interval, IntervalStyle, NoZones, POSTGRES_EPOCH_JDATE,
    TIMESTAMP_INFINITY, TIMESTAMP_NEGATIVE_INFINITY, TimeZone, UNIX_EPOCH_JDATE,
    UNIX_TO_POSTGRES_DAYS, UNIX_TO_POSTGRES_USECS, USECS_PER_DAY, USECS_PER_SEC, ZoneAbbrevs,
    ZoneLookup, adjust_interval, adjust_time, adjust_timestamp, date_from_unix, date_in, date_out,
    date_recv, date2j, interval_in, interval_out, interval_recv, interval_send, j2date,
    local_offset, time_in, time_out, time_recv, timestamp_from_unix, timestamp_in, timestamp_out,
    timestamp_recv, timestamptz_in, timestamptz_out, timetz_in, timetz_out, timetz_recv,
};
pub use datum::Datum;
pub use error::TypeError;
pub use float::{float4_in, float4_out, float8_in, float8_out};
pub use generated::oids as oid;
pub use json::json_in;
pub use jsonb::{jsonb_in, jsonb_recv, jsonb_send};
pub use network::{
    Inet, NetFamily, bitncmp, bitncommon, cidr_abbrev, inet_in, inet_out, inet_recv, inet_send,
    net_ntop, network_cmp,
};
pub use number::{int_out, int2_in, int4_in, int8_in, oid_in, oid_out, u64_out, uint32_in};
pub use numeric::{
    Numeric, NumericSign, decimal_out, decimal_send, numeric_in, numeric_out, numeric_recv,
    numeric_send,
};
pub use reg::{
    RegInput, RegKind, qualified_name_list, reg_in, reg_out_oid, split_identifier_string,
};
pub use scalar::{
    ByteaOutput, NAME_MAX_BYTES, bool_in, bool_out, bytea_in, bytea_out, char_in, char_out,
    name_in, uuid_in, uuid_out,
};
pub use stats::{pg_dependencies_in, pg_dependencies_out, pg_ndistinct_in, pg_ndistinct_out};
pub use string::{bpchar_coerce, bpchar_in, varchar_coerce, varchar_in};
pub use typeid::TypeId;
pub use types::{Oid, PgType, TypeInfo, format_type};
pub use value::{Record, Value};
