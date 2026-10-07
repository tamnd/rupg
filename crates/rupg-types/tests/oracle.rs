//! Checks the text forms against `oracle.tsv`, which `oracle.sql` records from the server at the pin. Each line is the type, a setting, the input, and the output or the error. The setting is `extra_float_digits` for the floats, `DateStyle` for the date and time types with the `TimeZone` after a `|` for `timestamptz`, and `IntervalStyle` for `interval`. A type that starts with `send` has the hex of the binary output. For the output of the date and time types the input is the hex of the binary form. A type that starts with `in` is the text input of a date or time type with a typmod, and the output is the hex of the binary form. Its setting is the `DateStyle` and the `TimeZone` after a `|`, or the `IntervalStyle`. A string type or `json` after `in` has no setting, and `inhex` is the same with the hex of the input, for an input with a control character. A type that starts with `coerce` is the length cast of `varchar` or `bpchar` with the typmod and `true` for an explicit cast.
//!
//! A type that starts with `array` is the text input of an array, with the element type and the typmod, and the setting is `array_nulls`. A type that starts with `vector` is `int2vector` or `oidvector`. The output of both is the hex of the binary form, a space and the text form. `arrayhex` and `vectorhex` have the hex of the input and the hex of the output. A `format` line has an OID and the name that `format_type` gives it.
//!
//! A type that starts with `reg` is the text input and output of an OID alias type, and the output is `ERROR NAME` when the input is a name for the catalog to find. A `names` line has a name list as `regclass` reads it and the parts with a dot between them. The output starts with `rel` for one or two parts, `db` for three parts and `many` for more, as the errors of `regclass` for a name that is not found give them.
//!
//! Lifted from `crates/rudb-pgtypes/tests/oracle.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

use rupg_types::*;

fn text(f: impl FnOnce(&mut Vec<u8>)) -> String {
    let mut out = Vec::new();
    f(&mut out);
    String::from_utf8(out).unwrap()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap()).collect()
}

fn date_format(setting: &str) -> DateFormat {
    let (style, order) = setting.split_once(", ").unwrap();
    let style = match style {
        "ISO" => DateStyle::Iso,
        "SQL" => DateStyle::Sql,
        "Postgres" => DateStyle::Postgres,
        "German" => DateStyle::German,
        _ => panic!("unknown DateStyle {setting:?}"),
    };
    let order = match order {
        "MDY" => DateOrder::Mdy,
        "DMY" => DateOrder::Dmy,
        "YMD" => DateOrder::Ymd,
        _ => panic!("unknown DateStyle {setting:?}"),
    };
    DateFormat { style, order }
}

/// The fixed zones of the fixture. A POSIX zone such as `+05:30` is west of UTC and has no abbreviation.
fn zone(name: &str) -> FixedZone {
    match name {
        "UTC" => FixedZone::utc(),
        "<+05:30>-05:30" => FixedZone { offset: 19800, abbrev: "+05:30".to_string() },
        "+05:30" => FixedZone { offset: -19800, abbrev: String::new() },
        _ => panic!("unknown TimeZone {name:?}"),
    }
}

fn interval_style(setting: &str) -> IntervalStyle {
    match setting {
        "postgres" => IntervalStyle::Postgres,
        "postgres_verbose" => IntervalStyle::PostgresVerbose,
        "sql_standard" => IntervalStyle::SqlStandard,
        "iso_8601" => IntervalStyle::Iso8601,
        _ => panic!("unknown IntervalStyle {setting:?}"),
    }
}

/// The text output of a date or time type from its binary form. The value must also go back to the same bytes.
fn datetime(type_name: &str, setting: &str, input: &str) -> Result<String, TypeError> {
    let bytes = unhex(input);
    let recv = &mut Recv::new(&bytes);
    let mut out = Vec::new();
    let mut back = Vec::new();
    match type_name {
        "date" => {
            let v = date_recv(recv)?;
            date_out(v, date_format(setting), &mut out);
            back.extend_from_slice(&v.to_be_bytes());
        }
        "time" => {
            let v = time_recv(recv, -1)?;
            time_out(v, &mut out);
            back.extend_from_slice(&v.to_be_bytes());
        }
        "timetz" => {
            let (time, zone) = timetz_recv(recv, -1)?;
            timetz_out(time, zone, &mut out);
            back.extend_from_slice(&time.to_be_bytes());
            back.extend_from_slice(&zone.to_be_bytes());
        }
        "timestamp" => {
            let v = timestamp_recv(recv, -1)?;
            timestamp_out(v, date_format(setting), &mut out)?;
            back.extend_from_slice(&v.to_be_bytes());
        }
        "timestamptz" => {
            let (style, name) = setting.split_once('|').unwrap();
            let v = timestamp_recv(recv, -1)?;
            timestamptz_out(v, date_format(style), &zone(name), &mut out)?;
            back.extend_from_slice(&v.to_be_bytes());
        }
        "interval" => {
            let v = interval_recv(recv, -1)?;
            interval_out(&v, interval_style(setting), &mut out);
            interval_send(&v, &mut back);
        }
        _ => unreachable!(),
    }
    assert_eq!(back, bytes, "{type_name} {input} does not go back to the same bytes");
    Ok(String::from_utf8(out).unwrap())
}

/// The text input of a date or time type, as the hex of the binary form.
fn datetime_in(
    type_name: &str,
    typmod: i32,
    setting: &str,
    input: &str,
) -> Result<String, TypeError> {
    let mut out = Vec::new();
    if type_name == "interval" {
        let v = interval_in(input, typmod, interval_style(setting))?;
        interval_send(&v, &mut out);
        return Ok(hex(&out));
    }
    let (style, name) = setting.split_once('|').unwrap();
    let zone = zone(name);
    let cx = DateTimeInput {
        order: date_format(style).order,
        zone: &zone,
        zones: &NoZones,
        abbrevs: ZoneAbbrevs::postgres_default(),
        now: 0,
    };
    match type_name {
        "date" => out.extend_from_slice(&date_in(input, &cx)?.to_be_bytes()),
        "time" => out.extend_from_slice(&time_in(input, typmod, &cx)?.to_be_bytes()),
        "timetz" => {
            let (time, zone) = timetz_in(input, typmod, &cx)?;
            out.extend_from_slice(&time.to_be_bytes());
            out.extend_from_slice(&zone.to_be_bytes());
        }
        "timestamp" => out.extend_from_slice(&timestamp_in(input, typmod, &cx)?.to_be_bytes()),
        "timestamptz" => out.extend_from_slice(&timestamptz_in(input, typmod, &cx)?.to_be_bytes()),
        _ => panic!("the fixture has a type that the test does not know: {type_name}"),
    }
    Ok(hex(&out))
}

/// The text input of a string type or `json`, as the hex of the binary form. The binary form of each is the bytes of the string, so the receive function is the same after the encoding check.
fn string_in(type_name: &str, typmod: i32, input: &str) -> Option<Result<String, TypeError>> {
    let result = match type_name {
        "text" => Ok(input.to_string()),
        "varchar" => varchar_in(input, typmod).map(str::to_string),
        "bpchar" => bpchar_in(input, typmod).map(|v| v.into_owned()),
        "json" => json_in(input).map(str::to_string),
        _ => return None,
    };
    Some(result.map(|v| {
        assert_eq!(Recv::new(v.as_bytes()).text(), Ok(&v[..]));
        hex(v.as_bytes())
    }))
}

/// One element of an array: its text output and its binary output.
type Item = (Vec<u8>, Vec<u8>);

/// The input of an array element of a type in the fixture, with the typmod of the array.
fn item(type_name: &str, typmod: i32, input: &str) -> Result<Item, TypeError> {
    let (mut text, mut bytes) = (Vec::new(), Vec::new());
    match type_name {
        "int2" => {
            let v = int2_in(input)?;
            int_out(v.into(), &mut text);
            bytes.extend_from_slice(&v.to_be_bytes());
        }
        "int4" => {
            let v = int4_in(input)?;
            int_out(v.into(), &mut text);
            bytes.extend_from_slice(&v.to_be_bytes());
        }
        "int8" => {
            let v = int8_in(input)?;
            int_out(v, &mut text);
            bytes.extend_from_slice(&v.to_be_bytes());
        }
        "oid" => {
            let v = oid_in(input)?;
            oid_out(v, &mut text);
            bytes.extend_from_slice(&v.to_be_bytes());
        }
        "float8" => {
            let v = float8_in(input)?;
            float8_out(v, 1, &mut text);
            bytes.extend_from_slice(&v.to_be_bytes());
        }
        "bool" => {
            let v = bool_in(input)?;
            bool_out(v, &mut text);
            bytes.push(u8::from(v));
        }
        "numeric" => {
            let v = numeric_in(input, typmod)?;
            numeric_out(&v, &mut text);
            numeric_send(&v, &mut bytes);
        }
        "char" => {
            let v = char_in(input);
            char_out(v, &mut text);
            bytes.push(v);
        }
        "bytea" => {
            let v = bytea_in(input)?;
            bytea_out(&v, ByteaOutput::Hex, &mut text);
            bytes = v;
        }
        "uuid" => {
            let v = uuid_in(input)?;
            uuid_out(&v, &mut text);
            bytes.extend_from_slice(&v);
        }
        "name" => {
            text.extend_from_slice(name_in(input).as_bytes());
            bytes.clone_from(&text);
        }
        _ => match string_in(type_name, typmod, input) {
            Some(result) => {
                text = result.map(|v| unhex(&v))?;
                bytes.clone_from(&text);
            }
            None => {
                panic!("the fixture has an array type that the test does not know: {type_name}")
            }
        },
    }
    Ok((text, bytes))
}

/// The hex of the binary form and the text form of an array. The binary form must read back to the same dimensions and elements.
fn array(
    type_name: &str,
    typmod: i32,
    array_nulls: bool,
    input: &str,
) -> Result<String, TypeError> {
    let info = TypeInfo::by_name(type_name).unwrap();
    let array = array_in(input, info.delim, array_nulls, |s| item(type_name, typmod, s))?;
    let mut bytes = Vec::new();
    array_send(&array, info.oid, &mut bytes, |v, out| out.extend_from_slice(&v.1));
    let mut text = Vec::new();
    array_out(&array, info.delim, &mut text, |v, out| out.extend_from_slice(&v.0));
    let back = array_recv(&mut Recv::new(&bytes), info.oid, |r| Ok(r.rest().to_vec())).unwrap();
    assert_eq!(back.dims, array.dims, "{input:?} does not read back from its binary form");
    let sent = array.values.iter().map(|v| v.as_ref().map(|v| &v.1));
    assert!(back.values.iter().map(Option::as_ref).eq(sent), "{input:?} in binary");
    Ok(format!("{} {}", hex(&bytes), String::from_utf8(text).unwrap()))
}

/// The hex of the binary form and the text form of `int2vector` or `oidvector`.
fn vector(type_name: &str, input: &str) -> Result<String, TypeError> {
    let (mut bytes, mut text) = (Vec::new(), Vec::new());
    match type_name {
        "int2vector" => {
            let v = int2vector_in(input)?;
            int2vector_send(&v, &mut bytes);
            int2vector_out(&v, &mut text);
            if !v.is_empty() {
                assert_eq!(int2vector_recv(&mut Recv::new(&bytes)), Ok(v));
            }
        }
        "oidvector" => {
            let v = oidvector_in(input)?;
            oidvector_send(&v, &mut bytes);
            oidvector_out(&v, &mut text);
            if !v.is_empty() {
                assert_eq!(oidvector_recv(&mut Recv::new(&bytes)), Ok(v));
            }
        }
        _ => panic!("the fixture has a type that the test does not know: {type_name}"),
    }
    Ok(format!("{} {}", hex(&bytes), String::from_utf8(text).unwrap()))
}

/// The typmod of `numeric` or `numeric(p,s)`.
fn numeric_typmod(type_name: &str) -> Option<i32> {
    let args = type_name.strip_prefix("numeric")?;
    if args.is_empty() {
        return Some(-1);
    }
    let (p, s) = args.strip_prefix('(')?.strip_suffix(')')?.split_once(',')?;
    Some(typmod::numeric_typmod(p.parse().ok()?, s.parse().ok()?))
}

/// The text or the binary output of `numeric`. When the value fits the decimal of the engine at its display scale, the fast path must give the same bytes.
fn numeric(typmod: i32, send: bool, input: &str) -> Result<String, TypeError> {
    let v = numeric_in(input, typmod)?;
    let mut out = Vec::new();
    if send {
        numeric_send(&v, &mut out);
        let back = numeric_recv(&mut Recv::new(&out), typmod)?;
        assert_eq!(back, v, "{input:?} does not read back from its binary form");
    } else {
        numeric_out(&v, &mut out);
    }
    if let Some(value) = v.to_decimal(u32::from(v.dscale())).filter(|_| v.dscale() <= 38) {
        let mut fast = Vec::new();
        match send {
            true => decimal_send(value, u32::from(v.dscale()), &mut fast),
            false => decimal_out(value, u32::from(v.dscale()), &mut fast),
        }
        assert_eq!(fast, out, "{input:?} as a decimal");
    }
    Ok(if send { hex(&out) } else { String::from_utf8(out).unwrap() })
}

fn run(type_name: &str, setting: &str, input: &str) -> String {
    let (send, base) = match type_name.strip_prefix("send ") {
        Some(base) => (true, base),
        None => (false, type_name),
    };
    if let Some(rest) = type_name.strip_prefix("inhex ") {
        let input = String::from_utf8(unhex(input)).unwrap();
        return run(&format!("in {rest}"), setting, &input);
    }
    for kind in ["array", "vector"] {
        if let Some(rest) = type_name.strip_prefix(&format!("{kind}hex ")) {
            let input = String::from_utf8(unhex(input)).unwrap();
            return hex(run(&format!("{kind} {rest}"), setting, &input).as_bytes());
        }
    }
    if let Some(rest) = type_name.strip_prefix("array ") {
        let (base, typmod) = rest.split_once(' ').unwrap();
        return error_text(array(base, typmod.parse().unwrap(), setting == "on", input));
    }
    if let Some(rest) = type_name.strip_prefix("vector ") {
        return error_text(vector(rest, input));
    }
    if let Some(rest) = type_name.strip_prefix("reg ") {
        let kind = RegKind::ALL.into_iter().find(|kind| kind.type_name() == rest).unwrap();
        return error_text(reg_in(kind, input).map(|value| match value {
            RegInput::Oid(oid) => text(|out| reg_out_oid(kind, oid, out)),
            RegInput::Name(_) => "ERROR NAME".to_owned(),
        }));
    }
    if type_name == "names" {
        return error_text(qualified_name_list(input).map(|names| {
            let class = match names.len() {
                1 | 2 => "rel",
                3 => "db",
                _ => "many",
            };
            format!("{class} {}", names.join("."))
        }));
    }
    if type_name == "format" {
        return format_type(input.parse().unwrap()).into_owned();
    }
    if let Some(rest) = type_name.strip_prefix("in ") {
        let (base, typmod) = rest.split_once(' ').unwrap();
        let typmod = typmod.parse().unwrap();
        if let Some(result) = string_in(base, typmod, input) {
            return error_text(result);
        }
        return error_text(datetime_in(base, typmod, setting, input));
    }
    if let Some(rest) = type_name.strip_prefix("coerce ") {
        let [base, typmod, explicit] = rest.split(' ').collect::<Vec<_>>()[..] else {
            panic!("a coerce line needs a type, a typmod and t or f: {type_name}");
        };
        let (typmod, explicit) = (typmod.parse().unwrap(), explicit == "true");
        let result = match base {
            "varchar" => varchar_coerce(input, typmod, explicit).map(|v| hex(v.as_bytes())),
            "bpchar" => bpchar_coerce(input, typmod, explicit).map(|v| hex(v.as_bytes())),
            _ => panic!("the fixture has a type that the test does not know: {type_name}"),
        };
        return error_text(result);
    }
    if let Some(typmod) = numeric_typmod(base) {
        return error_text(numeric(typmod, send, input));
    }
    if ["date", "time", "timetz", "timestamp", "timestamptz", "interval"].contains(&type_name) {
        return error_text(datetime(type_name, setting, input));
    }
    let extra_float_digits = || setting.parse::<i32>().unwrap();
    let result = match type_name {
        "int2" => int2_in(input).map(|v| text(|out| int_out(v.into(), out))),
        "int4" => int4_in(input).map(|v| text(|out| int_out(v.into(), out))),
        "int8" => int8_in(input).map(|v| text(|out| int_out(v, out))),
        "oid" => oid_in(input).map(|v| text(|out| oid_out(v, out))),
        "bool" => bool_in(input).map(|v| text(|out| bool_out(v, out))),
        "\"char\"" => Ok(text(|out| char_out(char_in(input), out))),
        "name" => Ok(name_in(input).to_string()),
        "bytea" => bytea_in(input).map(|v| text(|out| bytea_out(&v, ByteaOutput::Hex, out))),
        "uuid" => uuid_in(input).map(|v| text(|out| uuid_out(&v, out))),
        "float8" => float8_in(input).map(|v| text(|out| float8_out(v, extra_float_digits(), out))),
        "float4" => float4_in(input).map(|v| text(|out| float4_out(v, extra_float_digits(), out))),
        _ => panic!("the fixture has a type that the test does not know: {type_name}"),
    };
    error_text(result)
}

fn error_text(result: Result<String, TypeError>) -> String {
    result.unwrap_or_else(|error| {
        let detail = error.detail.map(|d| format!(" DETAIL {d}")).unwrap_or_default();
        let hint = error.hint.map(|h| format!(" HINT {h}")).unwrap_or_default();
        format!("ERROR {} {}{detail}{hint}", error.sqlstate.as_str(), error.message)
    })
}

#[test]
fn the_text_forms_match_the_server_at_the_pin() {
    let mut failures = Vec::new();
    for line in include_str!("oracle.tsv").lines() {
        let fields: Vec<&str> = line.split('\t').collect();
        let [type_name, setting, input, expected] = fields[..] else {
            panic!("a line of the fixture does not have four fields: {line:?}");
        };
        let got = run(type_name, setting, input);
        if got != expected {
            failures.push(format!("{type_name} {setting} {input:?}: {got:?}, not {expected:?}"));
        }
    }
    assert!(failures.is_empty(), "{} lines differ:\n{}", failures.len(), failures.join("\n"));
}
