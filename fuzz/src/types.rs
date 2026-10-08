//! The M2 targets `type_input` and `type_recv` of spec/21 section 21.7, and the lines that `examples/type_replay.rs` compares with the oracle.
//!
//! A target gives one value to the input or the receive function of a type. A panic and an internal error (`XX000`) are failures. When the function accepts the value, the output, input, send and receive functions must agree on it: the text of the value must read back to the same text, and the binary form must read back to the same text. A difference from PostgreSQL is not a failure of the target, because the oracle is a separate process. The replay tool runs the corpus of a target on the oracle and shows each case where rupg gives a different answer.

use std::rc::Rc;

use arbitrary::{Arbitrary, Unstructured};
use rupg_common::{Error, Result, SqlState};
use rupg_func::Session;
use rupg_types::typmod::{INTERVAL_FULL_RANGE, char_typmod, interval_typmod, numeric_typmod};
use rupg_types::{
    ByteaOutput, DateFormat, DateOrder, DateStyle, FixedZone, IntervalStyle, TimeZone,
    USECS_PER_DAY, oid,
};

/// A type of the targets: the name for the oracle, the OID and the typmod.
pub struct Type {
    pub name: &'static str,
    pub oid: u32,
    pub typmod: i32,
}

const fn ty(name: &'static str, oid: u32, typmod: i32) -> Type {
    Type { name, oid, typmod }
}

/// The types that the engine reads as text and as binary.
pub const TYPES: &[Type] = &[
    ty("bool", oid::BOOL, -1),
    ty("bytea", oid::BYTEA, -1),
    ty("\"char\"", oid::CHAR, -1),
    ty("name", oid::NAME, -1),
    ty("int2", oid::INT2, -1),
    ty("int4", oid::INT4, -1),
    ty("int8", oid::INT8, -1),
    ty("oid", oid::OID, -1),
    ty("text", oid::TEXT, -1),
    ty("varchar", oid::VARCHAR, -1),
    ty("varchar(5)", oid::VARCHAR, char_typmod(5)),
    ty("bpchar", oid::BPCHAR, -1),
    ty("character(5)", oid::BPCHAR, char_typmod(5)),
    ty("json", oid::JSON, -1),
    ty("jsonb", oid::JSONB, -1),
    ty("float4", oid::FLOAT4, -1),
    ty("float8", oid::FLOAT8, -1),
    ty("numeric", oid::NUMERIC, -1),
    ty("numeric(10,4)", oid::NUMERIC, numeric_typmod(10, 4)),
    ty("numeric(4,-3)", oid::NUMERIC, numeric_typmod(4, -3)),
    ty("uuid", oid::UUID, -1),
    ty("int2vector", oid::INT2VECTOR, -1),
    ty("oidvector", oid::OIDVECTOR, -1),
    ty("date", oid::DATE, -1),
    ty("time", oid::TIME, -1),
    ty("time(2)", oid::TIME, 2),
    ty("timetz", oid::TIMETZ, -1),
    ty("timestamp", oid::TIMESTAMP, -1),
    ty("timestamp(0)", oid::TIMESTAMP, 0),
    ty("timestamptz", oid::TIMESTAMPTZ, -1),
    ty("timestamptz(3)", oid::TIMESTAMPTZ, 3),
    ty("interval", oid::INTERVAL, -1),
    ty("interval(2)", oid::INTERVAL, interval_typmod(2, INTERVAL_FULL_RANGE)),
    ty("bool[]", oid::BOOL_ARRAY, -1),
    ty("bytea[]", oid::BYTEA_ARRAY, -1),
    ty("\"char\"[]", oid::CHAR_ARRAY, -1),
    ty("name[]", oid::NAME_ARRAY, -1),
    ty("int2[]", oid::INT2_ARRAY, -1),
    ty("int4[]", oid::INT4_ARRAY, -1),
    ty("int8[]", oid::INT8_ARRAY, -1),
    ty("oid[]", oid::OID_ARRAY, -1),
    ty("text[]", oid::TEXT_ARRAY, -1),
    ty("varchar(5)[]", oid::VARCHAR_ARRAY, char_typmod(5)),
    ty("float4[]", oid::FLOAT4_ARRAY, -1),
    ty("float8[]", oid::FLOAT8_ARRAY, -1),
    ty("numeric(5,2)[]", oid::NUMERIC_ARRAY, numeric_typmod(5, 2)),
    ty("uuid[]", oid::UUID_ARRAY, -1),
    ty("jsonb[]", oid::JSONB_ARRAY, -1),
    ty("date[]", oid::DATE_ARRAY, -1),
    ty("timestamptz[]", oid::TIMESTAMPTZ_ARRAY, -1),
    ty("interval[]", oid::INTERVAL_ARRAY, -1),
    ty("int2vector[]", oid::INT2VECTOR_ARRAY, -1),
];

/// The values of `DateStyle`. A style and an order that PostgreSQL does not read back, such as `German, MDY`, are not in the list.
const DATE_STYLES: &[(&str, DateStyle, DateOrder)] = &[
    ("ISO, MDY", DateStyle::Iso, DateOrder::Mdy),
    ("ISO, DMY", DateStyle::Iso, DateOrder::Dmy),
    ("ISO, YMD", DateStyle::Iso, DateOrder::Ymd),
    ("SQL, MDY", DateStyle::Sql, DateOrder::Mdy),
    ("SQL, DMY", DateStyle::Sql, DateOrder::Dmy),
    ("Postgres, MDY", DateStyle::Postgres, DateOrder::Mdy),
    ("Postgres, DMY", DateStyle::Postgres, DateOrder::Dmy),
    ("German, DMY", DateStyle::German, DateOrder::Dmy),
];

const INTERVAL_STYLES: &[(&str, IntervalStyle)] = &[
    ("postgres", IntervalStyle::Postgres),
    ("postgres_verbose", IntervalStyle::PostgresVerbose),
    ("sql_standard", IntervalStyle::SqlStandard),
    ("iso_8601", IntervalStyle::Iso8601),
];

const FLOAT_DIGITS: &[i32] = &[1, 0, 2, 3, -4, -15];

/// The settings that the input and the output functions read. Each field picks a value from a list.
#[derive(Arbitrary, Debug, Clone, Copy, Default)]
pub struct Settings {
    date: u8,
    interval: u8,
    float_digits: u8,
    escape: bool,
    no_array_nulls: bool,
}

impl Settings {
    fn date(self) -> (&'static str, DateStyle, DateOrder) {
        DATE_STYLES[usize::from(self.date) % DATE_STYLES.len()]
    }

    fn interval(self) -> (&'static str, IntervalStyle) {
        INTERVAL_STYLES[usize::from(self.interval) % INTERVAL_STYLES.len()]
    }

    fn float_digits(self) -> i32 {
        FLOAT_DIGITS[usize::from(self.float_digits) % FLOAT_DIGITS.len()]
    }

    /// The `SET` commands that give the oracle these settings.
    pub fn sql(self) -> String {
        format!(
            "set datestyle = '{}'; set intervalstyle = '{}'; set extra_float_digits = {}; set bytea_output = '{}'; set array_nulls = {};",
            self.date().0,
            self.interval().0,
            self.float_digits(),
            if self.escape { "escape" } else { "hex" },
            if self.no_array_nulls { "off" } else { "on" },
        )
    }

    fn session(self) -> FuzzSession {
        FuzzSession { settings: self, zone: Rc::new(FixedZone::utc()) }
    }
}

/// A session in UTC at 2000-01-11 00:00 with the settings.
struct FuzzSession {
    settings: Settings,
    zone: Rc<FixedZone>,
}

impl Session for FuzzSession {
    fn date_format(&self) -> DateFormat {
        let (_, style, order) = self.settings.date();
        DateFormat { style, order }
    }
    fn interval_style(&self) -> IntervalStyle {
        self.settings.interval().1
    }
    fn zone(&self) -> Result<Rc<dyn TimeZone>> {
        Ok(Rc::clone(&self.zone) as Rc<dyn TimeZone>)
    }
    fn extra_float_digits(&self) -> i32 {
        self.settings.float_digits()
    }
    fn bytea_output(&self) -> ByteaOutput {
        if self.settings.escape { ByteaOutput::Escape } else { ByteaOutput::Hex }
    }
    fn array_nulls(&self) -> bool {
        !self.settings.no_array_nulls
    }
    fn transaction_start(&self) -> i64 {
        10 * USECS_PER_DAY
    }
    fn statement_start(&self) -> i64 {
        10 * USECS_PER_DAY
    }
    fn clock(&self) -> i64 {
        10 * USECS_PER_DAY
    }
    fn user(&self) -> &str {
        "postgres"
    }
    fn session_user(&self) -> &str {
        "postgres"
    }
    fn database(&self) -> &str {
        "postgres"
    }
    fn schemas(&self) -> Vec<String> {
        Vec::new()
    }
    fn backend_pid(&self) -> i32 {
        1
    }
    fn version(&self) -> String {
        String::new()
    }
    fn setting(&self, _: &str) -> Option<String> {
        None
    }
    fn set_setting(&self, _: &str, _: Option<&str>, _: bool) -> Result<String> {
        Err(Error::internal("the fuzz session has no settings to change"))
    }
}

/// The input of `type_input`: a type, the settings and the text.
#[derive(Arbitrary, Debug)]
pub struct InputCase {
    ty: u8,
    pub settings: Settings,
    text: String,
}

impl InputCase {
    pub fn ty(&self) -> &'static Type {
        &TYPES[usize::from(self.ty) % TYPES.len()]
    }

    /// The text as the input function gets it. A C string ends at the first zero byte, and a SQL literal cannot hold one.
    pub fn text(&self) -> &str {
        self.text.split('\0').next().unwrap_or_default()
    }
}

/// The input of `type_recv`: a type and the bytes of one value.
#[derive(Arbitrary, Debug)]
pub struct RecvCase {
    ty: u8,
    pub data: Vec<u8>,
}

impl RecvCase {
    pub fn ty(&self) -> &'static Type {
        &TYPES[usize::from(self.ty) % TYPES.len()]
    }
}

/// Reads a case from the bytes of a fuzz input, as the `fuzz_target!` macro of libfuzzer-sys does.
pub fn decode<'a, T: Arbitrary<'a>>(data: &'a [u8]) -> Option<T> {
    if data.len() < T::size_hint(0).0 {
        return None;
    }
    T::arbitrary_take_rest(Unstructured::new(data)).ok()
}

fn output(ty: &Type, value: &rupg_types::Value, session: &dyn Session) -> Result<String> {
    let mut out = Vec::new();
    rupg_func::output(ty.oid, value, session, &mut out)?;
    Ok(String::from_utf8(out).expect("an output function gave bytes that are not UTF-8"))
}

fn send(ty: &Type, value: &rupg_types::Value) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    rupg_func::send(ty.oid, value, &mut out)?;
    Ok(out)
}

/// True for a type whose output can lose digits. With `extra_float_digits` below 1 the text of a float does not always read back: the largest `float8` with one digit is `2e+308`, which is out of range.
fn inexact_float(ty: &Type, settings: Settings) -> bool {
    settings.float_digits() < 1
        && [oid::FLOAT4, oid::FLOAT8, oid::FLOAT4_ARRAY, oid::FLOAT8_ARRAY].contains(&ty.oid)
}

/// The errors that PostgreSQL gives with `elog`, so with `XX000`, for a bad input. Any other internal error is a bug of rupg.
const ELOG: &[&str] = &["unsupported jsonb version number ", "unrecognized interval typmod: "];

fn not_internal(error: &Error, what: &str) {
    let elog = ELOG.iter().any(|m| error.message().starts_with(m));
    assert!(
        error.state() != SqlState::INTERNAL_ERROR || elog,
        "{what} gave an internal error: {error:?}"
    );
}

/// The `type_input` target.
pub fn type_input(case: &InputCase) {
    let ty = case.ty();
    let session = case.settings.session();
    let value = match rupg_func::input(ty.oid, case.text(), ty.typmod, &session) {
        Ok(value) => value,
        Err(error) => return not_internal(&error, "the input"),
    };
    let text = output(ty, &value, &session).expect("the output of an accepted value");
    if !inexact_float(ty, case.settings) {
        let again = rupg_func::input(ty.oid, &text, ty.typmod, &session)
            .unwrap_or_else(|e| panic!("the input of the output {text:?} failed: {e:?}"));
        assert_eq!(output(ty, &again, &session).expect("the output"), text, "the text round trip");
    }
    let bytes = send(ty, &value).expect("the send of an accepted value");
    match rupg_func::receive(ty.oid, &bytes, ty.typmod, 1) {
        Ok(back) => {
            let back = output(ty, &back, &session).expect("the output");
            assert_eq!(back, text, "the binary round trip");
        }
        Err(e) if empty_vector(ty, &text) => not_internal(&e, "the receive"),
        Err(e) => panic!("the receive of the send of {text:?} failed: {e:?}"),
    }
}

/// True for an `int2vector` or an `oidvector` with no element, or an array that holds one. PostgreSQL sends it as an array of one dimension of length 0, and its receive function refuses that: a `COPY` in the binary format of `''::oidvector` to a file and back gives `invalid oidvector data` on the oracle.
fn empty_vector(ty: &Type, text: &str) -> bool {
    match ty.oid {
        oid::INT2VECTOR | oid::OIDVECTOR => text.is_empty(),
        oid::INT2VECTOR_ARRAY => text.contains("\"\""),
        _ => false,
    }
}

/// The `type_recv` target. The text checks use the default settings.
pub fn type_recv(case: &RecvCase) {
    let ty = case.ty();
    let session = Settings::default().session();
    let value = match rupg_func::receive(ty.oid, &case.data, ty.typmod, 1) {
        Ok(value) => value,
        Err(error) => return not_internal(&error, "the receive"),
    };
    let bytes = send(ty, &value).expect("the send of a received value");
    let back = rupg_func::receive(ty.oid, &bytes, ty.typmod, 1)
        .unwrap_or_else(|e| panic!("the receive of the send failed: {e:?}"));
    assert_eq!(send(ty, &back).expect("the send"), bytes, "the binary round trip");
    let text = output(ty, &value, &session).expect("the output of a received value");
    if smallest_time(ty, &text) {
        return;
    }
    let again = rupg_func::input(ty.oid, &text, ty.typmod, &session)
        .unwrap_or_else(|e| panic!("the input of the output {text:?} failed: {e:?}"));
    assert_eq!(output(ty, &again, &session).expect("the output"), text, "the text round trip");
}

/// True for an `interval` whose time is the smallest `int64`. The receive function accepts it, but the input function refuses its output: `'-2562047788:00:54.775808'::interval` gives 22007 on the oracle, and `'-2562047788:00:54.775807'::interval` is correct.
fn smallest_time(ty: &Type, text: &str) -> bool {
    matches!(ty.oid, oid::INTERVAL | oid::INTERVAL_ARRAY) && text.contains("-2562047788:00:54.775808")
}

/// A line of the replay: the error, or the output text and the hex of the send.
fn line(result: Result<(String, Vec<u8>)>) -> String {
    match result {
        Ok((text, bytes)) => format!("OK {} {}", escape(&text), hex(&bytes)),
        Err(e) => {
            let mut line = format!("ERROR {} {}", e.state().as_str(), e.message());
            if let Some(detail) = e.detail() {
                line.push_str(" DETAIL ");
                line.push_str(detail);
            }
            if let Some(hint) = e.hint() {
                line.push_str(" HINT ");
                line.push_str(hint);
            }
            escape(&line)
        }
    }
}

/// The line of rupg for a case of `type_input`.
pub fn input_line(case: &InputCase) -> String {
    let ty = case.ty();
    let session = case.settings.session();
    line(
        rupg_func::input(ty.oid, case.text(), ty.typmod, &session)
            .and_then(|value| Ok((output(ty, &value, &session)?, send(ty, &value)?))),
    )
}

/// The line of rupg for a case of `type_recv`. `COPY` gives the error of bytes that are left over without the number of a parameter.
pub fn recv_line(case: &RecvCase) -> String {
    let ty = case.ty();
    let session = Settings::default().session();
    let line = line(
        rupg_func::receive(ty.oid, &case.data, ty.typmod, 1)
            .and_then(|value| Ok((output(ty, &value, &session)?, send(ty, &value)?))),
    );
    line.replace("incorrect binary data format in bind parameter 1", "incorrect binary data format")
}

/// A backslash, a tab, a newline and a carriage return as two characters, so that a line of the replay is one line of a TSV file.
pub fn escape(text: &str) -> String {
    text.replace('\\', "\\\\").replace('\t', "\\t").replace('\n', "\\n").replace('\r', "\\r")
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rupg_platform::sim::SimRng;

    #[test]
    fn known_values_round_trip() {
        let settings = Settings::default();
        for (ty, text) in [(5, "12"), (17, "1.5e3"), (31, "1 day 02:03:04"), (38, "{1,NULL,3}")] {
            let case = InputCase { ty, settings, text: text.to_string() };
            type_input(&case);
            assert!(input_line(&case).starts_with("OK "), "{}", input_line(&case));
        }
        let case = RecvCase { ty: 5, data: vec![0, 0, 0, 7] };
        type_recv(&case);
        assert_eq!(recv_line(&case), "OK 7 00000007");
        let case = RecvCase { ty: 5, data: vec![0, 0, 0, 7, 0] };
        assert_eq!(recv_line(&case), "ERROR 22P03 incorrect binary data format");
    }

    #[test]
    fn random_inputs_do_not_panic() {
        let mut rng = SimRng::new(5);
        for _ in 0..2000 {
            let mut data = vec![0; rng.below(40) as usize];
            rng.fill(&mut data);
            if let Some(case) = decode::<InputCase>(&data) {
                type_input(&case);
            }
            if let Some(case) = decode::<RecvCase>(&data) {
                type_recv(&case);
            }
        }
    }
}
