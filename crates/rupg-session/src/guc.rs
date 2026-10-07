//! The configuration parameters of PostgreSQL and the rules that read and print their values.
//!
//! The table is generated from the vendored `guc_parameters.dat` and `guc_tables.c` into [`generated::guc`](crate::generated::guc), with the boot values of the Linux build of the oracle (spec/21 section 21.3.3). This module finds a parameter by name, reads a value in the text that `SET` takes, and prints a value in the text that `SHOW` gives. The rules and the error texts are the ones of `parse_bool`, `parse_int`, `parse_real` and `_ShowOption` in `guc.c`, so a value that PostgreSQL takes is taken here and a value that it refuses is refused with the same text. What a parameter does is not here: the server keeps the values of a session and decides which of them change its behavior.
//!
//! Lifted from `crates/rudb-common/src/guc.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

use std::cmp::Ordering;

mod check;
mod settings;

pub use check::{Zone, encoding, split_identifiers, zone};
pub use settings::{Action, Arg, Characteristics, Origin, Settings, Source, flatten};

use crate::generated::guc::PARAMETERS;
use rupg_common::Error;
use rupg_common::SqlState;

/// The flags of a parameter, with the values of `guc.h`.
pub mod flag {
    /// The value can be a list.
    pub const LIST_INPUT: u32 = 0x00_0001;
    /// The items of a list are in double quotes.
    pub const LIST_QUOTE: u32 = 0x00_0002;
    /// `SHOW ALL` and `pg_settings` do not list the parameter.
    pub const NO_SHOW_ALL: u32 = 0x00_0004;
    /// `RESET` and `SET ... FROM CURRENT` are not allowed.
    pub const NO_RESET: u32 = 0x00_0008;
    /// `RESET ALL` does not change the parameter.
    pub const NO_RESET_ALL: u32 = 0x00_0010;
    /// `EXPLAIN (SETTINGS)` shows the parameter.
    pub const EXPLAIN: u32 = 0x00_0020;
    /// The server sends a change of the value to the client in `ParameterStatus`.
    pub const REPORT: u32 = 0x00_0040;
    /// `postgresql.conf.sample` does not have the parameter.
    pub const NOT_IN_SAMPLE: u32 = 0x00_0080;
    /// The configuration file cannot set the parameter.
    pub const DISALLOW_IN_FILE: u32 = 0x00_0100;
    /// A placeholder for a custom parameter.
    pub const CUSTOM_PLACEHOLDER: u32 = 0x00_0200;
    /// Only a superuser can read the value.
    pub const SUPERUSER_ONLY: u32 = 0x00_0400;
    /// The value is a name, cut to 63 bytes.
    pub const IS_NAME: u32 = 0x00_0800;
    /// A security-restricted operation cannot set the parameter.
    pub const NOT_WHILE_SEC_REST: u32 = 0x00_1000;
    /// `ALTER SYSTEM` cannot set the parameter.
    pub const DISALLOW_IN_AUTO_FILE: u32 = 0x00_2000;
    /// The value is computed late in the start of the server.
    pub const RUNTIME_COMPUTED: u32 = 0x00_4000;
    /// A parallel worker can set the parameter.
    pub const ALLOW_IN_PARALLEL: u32 = 0x00_8000;
    /// The value is in kilobytes.
    pub const UNIT_KB: u32 = 0x0100_0000;
    /// The value is in blocks of 8 kB.
    pub const UNIT_BLOCKS: u32 = 0x0200_0000;
    /// The value is in WAL blocks of 8 kB.
    pub const UNIT_XBLOCKS: u32 = 0x0300_0000;
    /// The value is in megabytes.
    pub const UNIT_MB: u32 = 0x0400_0000;
    /// The value is in bytes.
    pub const UNIT_BYTE: u32 = 0x0500_0000;
    /// The mask of the memory units.
    pub const UNIT_MEMORY: u32 = 0x0F00_0000;
    /// The value is in milliseconds.
    pub const UNIT_MS: u32 = 0x1000_0000;
    /// The value is in seconds.
    pub const UNIT_S: u32 = 0x2000_0000;
    /// The value is in minutes.
    pub const UNIT_MIN: u32 = 0x3000_0000;
    /// The mask of the time units.
    pub const UNIT_TIME: u32 = 0x7000_0000;
    /// The mask of all units.
    pub const UNIT: u32 = UNIT_MEMORY | UNIT_TIME;
}

/// Who can change a parameter, and when.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Context {
    /// Nobody. The value is a fact about the build or the server.
    Internal,
    /// Only at server start.
    Postmaster,
    /// The configuration file, at start and at a reload.
    Sighup,
    /// A superuser, at connection start.
    SuperuserBackend,
    /// Any user, at connection start.
    Backend,
    /// A superuser or a role with the `SET` privilege, at any time.
    Superuser,
    /// Any user at any time.
    User,
}

impl Context {
    /// The name in the `context` column of `pg_settings`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Internal => "internal",
            Self::Postmaster => "postmaster",
            Self::Sighup => "sighup",
            Self::SuperuserBackend => "superuser-backend",
            Self::Backend => "backend",
            Self::Superuser => "superuser",
            Self::User => "user",
        }
    }
}

/// One value of an enum parameter. The names that mean the same value share `value`.
#[derive(Debug)]
pub struct EnumOption {
    /// The name, which `SET` takes without regard to case.
    pub name: &'static str,
    /// The value. `SHOW` gives the name of the first option with it.
    pub value: u8,
    /// Whether the hint and `enumvals` leave the name out.
    pub hidden: bool,
}

/// The type of a parameter, with its boot value and its range.
#[derive(Clone, Copy, Debug)]
pub enum Kind {
    /// `bool`.
    Bool {
        /// The boot value.
        boot: bool,
    },
    /// `integer`, in the base unit of the parameter.
    Int {
        /// The boot value.
        boot: i32,
        /// The least value.
        min: i32,
        /// The greatest value.
        max: i32,
    },
    /// `real`, in the base unit of the parameter.
    Real {
        /// The boot value.
        boot: f64,
        /// The least value.
        min: f64,
        /// The greatest value.
        max: f64,
    },
    /// `string`.
    String {
        /// The boot value, or `None` for a parameter that the server sets at start.
        boot: Option<&'static str>,
    },
    /// `enum`.
    Enum {
        /// The boot value.
        boot: u8,
        /// The options, in the order of the PostgreSQL array.
        options: &'static [EnumOption],
    },
}

/// One configuration parameter of PostgreSQL.
#[derive(Debug)]
pub struct Parameter {
    /// The canonical name, for example `DateStyle`.
    pub name: &'static str,
    /// Who can change it, and when.
    pub context: Context,
    /// The text of its group, the `category` column of `pg_settings`.
    pub category: &'static str,
    /// The short description.
    pub short_desc: &'static str,
    /// The long description, if it has one.
    pub extra_desc: Option<&'static str>,
    /// The [`flag`] bits.
    pub flags: u32,
    /// The type, the boot value and the range.
    pub kind: Kind,
}

/// A value of a parameter, in the base unit of the parameter.
#[derive(Clone, Debug, PartialEq)]
pub enum Setting {
    /// The value of a `bool` parameter.
    Bool(bool),
    /// The value of an `integer` parameter.
    Int(i32),
    /// The value of a `real` parameter.
    Real(f64),
    /// The value of a `string` parameter. A `None` boot value is the empty string here.
    String(String),
    /// The value of an `enum` parameter.
    Enum(u8),
}

/// The names that PostgreSQL still takes for a renamed parameter, from `map_old_guc_names` in `guc.c`.
const OLD_NAMES: [(&str, &str); 3] = [
    ("sort_mem", "work_mem"),
    ("vacuum_mem", "maintenance_work_mem"),
    ("ssl_ecdh_curve", "ssl_groups"),
];

/// The block size of the oracle build, for the units `8kB`.
const BLCKSZ: f64 = 8192.0;

/// The parameter with this name, without regard to case, or `None` if PostgreSQL has none. An old name of a renamed parameter finds the new one.
#[must_use]
pub fn find(name: &str) -> Option<&'static Parameter> {
    let search = |name: &str| {
        PARAMETERS.binary_search_by(|p| compare(p.name, name)).ok().map(|i| &PARAMETERS[i])
    };
    search(name).or_else(|| {
        OLD_NAMES
            .iter()
            .find(|(old, _)| old.eq_ignore_ascii_case(name))
            .and_then(|(_, new)| search(new))
    })
}

/// Every parameter, in the order of the names without regard to case.
#[must_use]
pub fn all() -> &'static [Parameter] {
    &PARAMETERS
}

fn compare(a: &str, b: &str) -> Ordering {
    a.bytes().map(|c| c.to_ascii_lowercase()).cmp(b.bytes().map(|c| c.to_ascii_lowercase()))
}

/// Whether `name` is a valid name for a custom parameter: two or more identifiers with dots between them, as `valid_custom_variable_name` in `guc.c` checks.
#[must_use]
pub fn valid_custom_name(name: &str) -> bool {
    let mut dots = 0;
    for part in name.split('.') {
        let mut bytes = part.bytes();
        match bytes.next() {
            Some(c) if c.is_ascii_alphabetic() || c == b'_' || c >= 0x80 => {}
            _ => return false,
        }
        if !bytes.all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80) {
            return false;
        }
        dots += 1;
    }
    dots > 1
}

/// Reads a Boolean with the rules of `parse_bool` in PostgreSQL: `true`, `false`, `yes`, `no`, `on`, `off`, `1` and `0`, without regard to case, and a prefix of a word that is not ambiguous.
#[must_use]
pub fn parse_bool(text: &str) -> Option<bool> {
    let lower = text.to_ascii_lowercase();
    let prefix = |word: &str, least: usize| lower.len() >= least && word.starts_with(&lower);
    match lower.as_bytes().first()? {
        b't' if prefix("true", 1) => Some(true),
        b'f' if prefix("false", 1) => Some(false),
        b'y' if prefix("yes", 1) => Some(true),
        b'n' if prefix("no", 1) => Some(false),
        b'o' if prefix("on", 2) => Some(true),
        b'o' if prefix("off", 2) => Some(false),
        b'1' if lower.len() == 1 => Some(true),
        b'0' if lower.len() == 1 => Some(false),
        _ => None,
    }
}

impl Parameter {
    /// The boot value.
    #[must_use]
    pub fn boot(&self) -> Setting {
        match self.kind {
            Kind::Bool { boot } => Setting::Bool(boot),
            Kind::Int { boot, .. } => Setting::Int(boot),
            Kind::Real { boot, .. } => Setting::Real(boot),
            Kind::String { boot } => Setting::String(boot.unwrap_or("").to_owned()),
            Kind::Enum { boot, .. } => Setting::Enum(boot),
        }
    }

    /// Whether the parameter has this flag.
    #[must_use]
    pub const fn has(&self, flag: u32) -> bool {
        self.flags & flag != 0
    }

    /// The `vartype` column of `pg_settings`.
    #[must_use]
    pub const fn vartype(&self) -> &'static str {
        match self.kind {
            Kind::Bool { .. } => "bool",
            Kind::Int { .. } => "integer",
            Kind::Real { .. } => "real",
            Kind::String { .. } => "string",
            Kind::Enum { .. } => "enum",
        }
    }

    /// The base unit, the `unit` column of `pg_settings`, or `None` for a parameter without one.
    #[must_use]
    pub const fn unit(&self) -> Option<&'static str> {
        match self.flags & flag::UNIT {
            flag::UNIT_BYTE => Some("B"),
            flag::UNIT_KB => Some("kB"),
            flag::UNIT_MB => Some("MB"),
            flag::UNIT_BLOCKS | flag::UNIT_XBLOCKS => Some("8kB"),
            flag::UNIT_MS => Some("ms"),
            flag::UNIT_S => Some("s"),
            flag::UNIT_MIN => Some("min"),
            _ => None,
        }
    }

    /// Reads a value in the text that `SET` takes.
    ///
    /// # Errors
    ///
    /// `22023` with the text and the hint of PostgreSQL if the text is not a value of the parameter or is out of its range.
    pub fn parse(&self, text: &str) -> Result<Setting, Error> {
        let invalid = |hint: Option<&str>| {
            let error = Error::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!("invalid value for parameter \"{}\": \"{text}\"", self.name),
            );
            match hint {
                Some(hint) => error.with_hint(hint),
                None => error,
            }
        };
        match self.kind {
            Kind::Bool { .. } => parse_bool(text).map(Setting::Bool).ok_or_else(|| {
                Error::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    format!("parameter \"{}\" requires a Boolean value", self.name),
                )
            }),
            Kind::Int { min, max, .. } => {
                let value = self.parse_int(text).map_err(invalid)?;
                if value < min || value > max {
                    return Err(self.out_of_range(&value, &min, &max));
                }
                Ok(Setting::Int(value))
            }
            Kind::Real { min, max, .. } => {
                let value = self.parse_real(text).map_err(invalid)?;
                if value < min || value > max {
                    return Err(self.out_of_range(&g(value), &g(min), &g(max)));
                }
                Ok(Setting::Real(value))
            }
            Kind::String { .. } => {
                let mut value = text.to_owned();
                if self.has(flag::IS_NAME) && value.len() > 63 {
                    let mut end = 63;
                    while !value.is_char_boundary(end) {
                        end -= 1;
                    }
                    value.truncate(end);
                }
                Ok(Setting::String(value))
            }
            Kind::Enum { options, .. } => options
                .iter()
                .find(|option| option.name.eq_ignore_ascii_case(text))
                .map(|option| Setting::Enum(option.value))
                .ok_or_else(|| {
                    let names: Vec<&str> =
                        options.iter().filter(|o| !o.hidden).map(|o| o.name).collect();
                    invalid(Some(&format!("Available values: {}.", names.join(", "))))
                }),
        }
    }

    fn out_of_range(
        &self,
        value: &dyn std::fmt::Display,
        min: &dyn std::fmt::Display,
        max: &dyn std::fmt::Display,
    ) -> Error {
        let unit = self.unit().map(|unit| format!(" {unit}")).unwrap_or_default();
        Error::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!(
                "{value}{unit} is outside the valid range for parameter \"{}\" ({min}{unit} .. {max}{unit})",
                self.name
            ),
        )
    }

    /// `parse_int` of `guc.c`: a decimal, octal or hexadecimal integer or a decimal fraction, then an optional unit. The error is the hint, if there is one.
    fn parse_int(&self, text: &str) -> Result<i32, Option<&'static str>> {
        let (mut value, rest) = match strtol(text) {
            Some((value, rest)) if !rest.starts_with(['.', 'e', 'E']) => (value, rest),
            _ => strtod(text).ok_or(None)?,
        };
        if value.is_nan() {
            return Err(None);
        }
        value = self.apply_unit(value, rest)?;
        let value = value.round_ties_even();
        if value > f64::from(i32::MAX) || value < f64::from(i32::MIN) {
            return Err(Some("Value exceeds integer range."));
        }
        #[allow(clippy::cast_possible_truncation)]
        Ok(value as i32)
    }

    /// `parse_real` of `guc.c`: a decimal number, then an optional unit.
    fn parse_real(&self, text: &str) -> Result<f64, Option<&'static str>> {
        let (value, rest) = strtod(text).ok_or(None)?;
        if value.is_nan() {
            return Err(None);
        }
        self.apply_unit(value, rest)
    }

    /// The value in the base unit of the parameter, from a value in the unit that `rest` names.
    fn apply_unit(&self, value: f64, rest: &str) -> Result<f64, Option<&'static str>> {
        let rest = rest.trim_start_matches(is_space);
        if rest.is_empty() {
            return Ok(value);
        }
        let base = self.flags & flag::UNIT;
        if base == 0 {
            return Err(None);
        }
        let hint = if base & flag::UNIT_MEMORY != 0 {
            "Valid units for this parameter are \"B\", \"kB\", \"MB\", \"GB\", and \"TB\"."
        } else {
            "Valid units for this parameter are \"us\", \"ms\", \"s\", \"min\", \"h\", and \"d\"."
        };
        let end = rest.find(is_space).unwrap_or(rest.len());
        let (unit, after) = rest.split_at(end);
        if unit.len() > 3 || !after.trim_start_matches(is_space).is_empty() {
            return Err(Some(hint));
        }
        let table = units(base);
        let Some(i) = table.iter().position(|(name, _)| *name == unit) else {
            return Err(Some(hint));
        };
        let mut value = value * table[i].1;
        // A fraction such as `30.1GB` is rounded to the next smaller unit, if there is one.
        if let Some((_, smaller)) = table.get(i + 1) {
            value = (value / smaller).round_ties_even() * smaller;
        }
        Ok(value)
    }

    /// The text that `SHOW` and `current_setting` give for a value of the parameter.
    #[must_use]
    pub fn show(&self, value: &Setting) -> String {
        match (value, self.kind) {
            (Setting::Bool(value), _) => (if *value { "on" } else { "off" }).to_owned(),
            (Setting::Int(value), _) if self.mode() => format!("{value:04o}"),
            (Setting::Int(value), _) => {
                let base = self.flags & flag::UNIT;
                if *value <= 0 || base == 0 {
                    return value.to_string();
                }
                let value = i64::from(*value);
                for (unit, multiplier) in units(base) {
                    #[allow(clippy::cast_possible_truncation)]
                    if *multiplier <= 1.0 || value % (*multiplier as i64) == 0 {
                        #[allow(clippy::cast_precision_loss)]
                        let shown = (value as f64 / multiplier).round_ties_even();
                        return format!("{shown}{unit}");
                    }
                }
                value.to_string()
            }
            (Setting::Real(value), _) => {
                let base = self.flags & flag::UNIT;
                if *value <= 0.0 || base == 0 {
                    return g(*value);
                }
                let mut shown = (*value, "");
                for (unit, multiplier) in units(base) {
                    let converted = value / multiplier;
                    shown = (converted, unit);
                    if converted > 0.0
                        && (converted.round_ties_even() / converted - 1.0).abs() <= 1e-8
                    {
                        break;
                    }
                }
                format!("{}{}", g(shown.0), shown.1)
            }
            (Setting::String(value), _) => value.clone(),
            (Setting::Enum(value), Kind::Enum { options, .. }) => options
                .iter()
                .find(|option| option.value == *value)
                .map_or_else(String::new, |option| option.name.to_owned()),
            (Setting::Enum(value), _) => value.to_string(),
        }
    }

    /// The text of the `setting` column of `pg_settings`: the value in the base unit, without the unit.
    #[must_use]
    pub fn setting(&self, value: &Setting) -> String {
        match value {
            Setting::Int(value) => value.to_string(),
            Setting::Real(value) => g(*value),
            value => self.show(value),
        }
    }

    /// Whether `SHOW` prints the value as a file mode in octal, as the show hooks of these three parameters do.
    fn mode(&self) -> bool {
        matches!(self.name, "data_directory_mode" | "log_file_mode" | "unix_socket_permissions")
    }
}

/// Whether `isspace` of C is true for the character.
fn is_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c')
}

/// The units of a base unit, from the largest to the smallest, with the number of base units in each, from the tables in `guc.c`.
fn units(base: u32) -> &'static [(&'static str, f64)] {
    const KIB: f64 = 1024.0;
    match base {
        flag::UNIT_BYTE => &[
            ("TB", KIB * KIB * KIB * KIB),
            ("GB", KIB * KIB * KIB),
            ("MB", KIB * KIB),
            ("kB", KIB),
            ("B", 1.0),
        ],
        flag::UNIT_KB => &[
            ("TB", KIB * KIB * KIB),
            ("GB", KIB * KIB),
            ("MB", KIB),
            ("kB", 1.0),
            ("B", 1.0 / KIB),
        ],
        flag::UNIT_MB => &[
            ("TB", KIB * KIB),
            ("GB", KIB),
            ("MB", 1.0),
            ("kB", 1.0 / KIB),
            ("B", 1.0 / (KIB * KIB)),
        ],
        flag::UNIT_BLOCKS | flag::UNIT_XBLOCKS => &[
            ("TB", (KIB * KIB * KIB) / (BLCKSZ / KIB)),
            ("GB", (KIB * KIB) / (BLCKSZ / KIB)),
            ("MB", KIB / (BLCKSZ / KIB)),
            ("kB", 1.0 / (BLCKSZ / KIB)),
            ("B", 1.0 / BLCKSZ),
        ],
        flag::UNIT_MS => &[
            ("d", 1000.0 * 60.0 * 60.0 * 24.0),
            ("h", 1000.0 * 60.0 * 60.0),
            ("min", 1000.0 * 60.0),
            ("s", 1000.0),
            ("ms", 1.0),
            ("us", 1.0 / 1000.0),
        ],
        flag::UNIT_S => &[
            ("d", 60.0 * 60.0 * 24.0),
            ("h", 60.0 * 60.0),
            ("min", 60.0),
            ("s", 1.0),
            ("ms", 1.0 / 1000.0),
            ("us", 1.0 / (1000.0 * 1000.0)),
        ],
        flag::UNIT_MIN => &[
            ("d", 60.0 * 24.0),
            ("h", 60.0),
            ("min", 1.0),
            ("s", 1.0 / 60.0),
            ("ms", 1.0 / (1000.0 * 60.0)),
            ("us", 1.0 / (1000.0 * 1000.0 * 60.0)),
        ],
        _ => &[],
    }
}

/// `strtol` with base 0: space, a sign, then `0x` and hexadecimal digits, `0` and octal digits, or decimal digits. The value and the rest of the text, or `None` if there is no digit or the value does not fit in 64 bits.
#[allow(clippy::cast_precision_loss)]
fn strtol(text: &str) -> Option<(f64, &str)> {
    let s = text.trim_start_matches(is_space);
    let (negative, s) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let (radix, digits) = if (s.starts_with("0x") || s.starts_with("0X"))
        && s[2..].starts_with(|c: char| c.is_ascii_hexdigit())
    {
        (16, &s[2..])
    } else if s.starts_with('0') {
        (8, s)
    } else {
        (10, s)
    };
    let end = digits.find(|c: char| !c.is_digit(radix)).unwrap_or(digits.len());
    if end == 0 {
        return None;
    }
    let value = i64::from_str_radix(&digits[..end], radix).ok()?;
    Some((if negative { -(value as f64) } else { value as f64 }, &digits[end..]))
}

/// `strtod`: space, a sign, then a decimal number with an optional fraction and exponent, or `inf`, `infinity` or `nan`. The value and the rest of the text, or `None` if there is no number or the value is too large for a double.
fn strtod(text: &str) -> Option<(f64, &str)> {
    let s = text.trim_start_matches(is_space);
    let bytes = s.as_bytes();
    let mut i = usize::from(matches!(bytes.first(), Some(b'-' | b'+')));
    let lower = s[i..].to_ascii_lowercase();
    for word in ["infinity", "inf", "nan"] {
        if lower.starts_with(word) {
            let value = if word == "nan" { f64::NAN } else { f64::INFINITY };
            let value = if bytes[0] == b'-' { -value } else { value };
            return Some((value, &s[i + word.len()..]));
        }
    }
    let digits = |i: &mut usize| {
        let start = *i;
        while bytes.get(*i).is_some_and(u8::is_ascii_digit) {
            *i += 1;
        }
        *i - start
    };
    let mut count = digits(&mut i);
    if bytes.get(i) == Some(&b'.') {
        i += 1;
        count += digits(&mut i);
    }
    if count == 0 {
        return None;
    }
    if matches!(bytes.get(i), Some(b'e' | b'E')) {
        let mut j = i + 1;
        if matches!(bytes.get(j), Some(b'-' | b'+')) {
            j += 1;
        }
        if digits(&mut j) > 0 {
            i = j;
        }
    }
    let value: f64 = s[..i].parse().ok()?;
    if value.is_infinite() {
        return None;
    }
    Some((value, &s[i..]))
}

/// A double as `%g` of C prints it: six significant digits, the exponent form for an exponent below -4 or above 5, and no zeros at the end of the fraction.
#[must_use]
pub fn g(value: f64) -> String {
    if value.is_nan() {
        return "nan".to_owned();
    }
    if value.is_infinite() {
        return (if value < 0.0 { "-inf" } else { "inf" }).to_owned();
    }
    if value == 0.0 {
        return (if value.is_sign_negative() { "-0" } else { "0" }).to_owned();
    }
    let scientific = format!("{value:.5e}");
    let (mantissa, exponent) = scientific.split_once('e').unwrap_or((&scientific, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let trim = |text: &str| {
        if text.contains('.') {
            text.trim_end_matches('0').trim_end_matches('.').to_owned()
        } else {
            text.to_owned()
        }
    };
    if (-4..6).contains(&exponent) {
        let decimals = usize::try_from(5 - exponent).unwrap_or(0);
        trim(&format!("{value:.decimals$}"))
    } else {
        let sign = if exponent < 0 { '-' } else { '+' };
        format!("{}e{sign}{:02}", trim(mantissa), exponent.unsigned_abs())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn show(name: &str, text: &str) -> String {
        let parameter = find(name).unwrap();
        parameter.show(&parameter.parse(text).unwrap())
    }

    fn error(name: &str, text: &str) -> (String, Option<String>) {
        let error = find(name).unwrap().parse(text).unwrap_err();
        assert_eq!(error.state(), SqlState::INVALID_PARAMETER_VALUE);
        (error.message().to_owned(), error.hint().map(str::to_owned))
    }

    #[test]
    fn the_table_has_every_parameter_of_the_release_build() {
        assert_eq!(all().len(), 425);
        assert!(all().windows(2).all(|w| compare(w[0].name, w[1].name) == Ordering::Less));
        assert_eq!(find("datestyle").unwrap().name, "DateStyle");
        assert_eq!(find("TIMEZONE").unwrap().name, "TimeZone");
        assert_eq!(find("sort_mem").unwrap().name, "work_mem");
        assert!(find("threads").is_none());
        let reported: Vec<_> =
            all().iter().filter(|p| p.has(flag::REPORT)).map(|p| p.name).collect();
        assert_eq!(reported.len(), 15);
        let version = find("server_version_num").unwrap();
        assert_eq!(version.show(&version.boot()), "190000");
        assert_eq!(version.context, Context::Internal);
    }

    #[test]
    fn the_boot_values_show_as_in_postgres() {
        let boot = |name: &str| {
            let parameter = find(name).unwrap();
            parameter.show(&parameter.boot())
        };
        assert_eq!(boot("shared_buffers"), "128MB");
        assert_eq!(boot("work_mem"), "4MB");
        assert_eq!(boot("statement_timeout"), "0");
        assert_eq!(boot("checkpoint_timeout"), "5min");
        assert_eq!(boot("wal_level"), "replica");
        assert_eq!(boot("IntervalStyle"), "postgres");
        assert_eq!(boot("unix_socket_permissions"), "0777");
        assert_eq!(boot("search_path"), "\"$user\", public");
        assert_eq!(boot("cpu_operator_cost"), "0.0025");
        assert_eq!(boot("seq_page_cost"), "1");
        assert_eq!(boot("autovacuum_vacuum_cost_delay"), "2ms");
        assert_eq!(boot("ssl_max_protocol_version"), "");
        assert_eq!(boot("standard_conforming_strings"), "on");
    }

    #[test]
    fn values_take_units_and_show_the_largest_exact_unit() {
        assert_eq!(show("statement_timeout", "60000"), "1min");
        assert_eq!(show("statement_timeout", "1.5s"), "1500ms");
        assert_eq!(show("statement_timeout", " 2 h "), "2h");
        assert_eq!(show("statement_timeout", "0x10"), "16ms");
        assert_eq!(show("work_mem", "1025kB"), "1025kB");
        assert_eq!(show("work_mem", "1GB"), "1GB");
        assert_eq!(show("work_mem", "0.5MB"), "512kB");
        assert_eq!(show("shared_buffers", "1MB"), "1MB");
        assert_eq!(show("vacuum_cost_delay", "500us"), "500us");
        assert_eq!(show("vacuum_cost_delay", "0.25"), "250us");
        assert_eq!(show("work_mem", "1e3"), "1000kB");
        assert_eq!(show("statement_timeout", "0.4"), "0");
        assert_eq!(show("extra_float_digits", "3"), "3");
        assert_eq!(show("jit", "OFF"), "off");
        assert_eq!(show("jit", "y"), "on");
        assert_eq!(show("bytea_output", "ESCAPE"), "escape");
        assert_eq!(show("constraint_exclusion", "yes"), "on");
        assert_eq!(show("application_name", "psql"), "psql");
    }

    #[test]
    fn a_bad_value_fails_with_the_text_of_postgres() {
        assert_eq!(error("jit", "maybe").0, "parameter \"jit\" requires a Boolean value");
        assert_eq!(error("jit", "o").0, "parameter \"jit\" requires a Boolean value");
        assert_eq!(
            error("extra_float_digits", "4"),
            (
                "4 is outside the valid range for parameter \"extra_float_digits\" (-15 .. 3)"
                    .to_owned(),
                None
            )
        );
        assert_eq!(
            error("statement_timeout", "-1").0,
            "-1 ms is outside the valid range for parameter \"statement_timeout\" (0 ms .. 2147483647 ms)"
        );
        assert_eq!(
            error("work_mem", "1 parsec"),
            (
                "invalid value for parameter \"work_mem\": \"1 parsec\"".to_owned(),
                Some(
                    "Valid units for this parameter are \"B\", \"kB\", \"MB\", \"GB\", and \"TB\"."
                        .to_owned()
                )
            )
        );
        assert_eq!(error("extra_float_digits", "1x").1, None);
        assert_eq!(
            error("work_mem", "010").0,
            "8 kB is outside the valid range for parameter \"work_mem\" (64 kB .. 2147483647 kB)"
        );
        assert_eq!(error("work_mem", "1.5").0.split(' ').next(), Some("2"));
        assert_eq!(
            error("extra_float_digits", "99999999999").1.as_deref(),
            Some("Value exceeds integer range.")
        );
        assert_eq!(
            error("bytea_output", "base64").1.as_deref(),
            Some("Available values: escape, hex.")
        );
        assert_eq!(
            error("seq_page_cost", "-1").0,
            "-1 is outside the valid range for parameter \"seq_page_cost\" (0 .. 1.79769e+308)"
        );
    }

    #[test]
    fn custom_names_need_two_identifiers() {
        assert!(valid_custom_name("myapp.user_id"));
        assert!(valid_custom_name("a.b.c$1"));
        assert!(!valid_custom_name("myapp"));
        assert!(!valid_custom_name("myapp."));
        assert!(!valid_custom_name(".x"));
        assert!(!valid_custom_name("a.1b"));
    }

    #[test]
    fn g_prints_as_c_does() {
        assert_eq!(g(0.1), "0.1");
        assert_eq!(g(1.0), "1");
        assert_eq!(g(1000.0), "1000");
        assert_eq!(g(1e10), "1e+10");
        assert_eq!(g(0.000_01), "1e-05");
        assert_eq!(g(123_456.0), "123456");
        assert_eq!(g(1_234_567.0), "1.23457e+06");
        assert_eq!(g(f64::MAX), "1.79769e+308");
        assert_eq!(g(-2.5), "-2.5");
    }
}
