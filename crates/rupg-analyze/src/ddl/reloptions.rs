//! The options of relations and of columns, as `transformRelOptions` and `parseRelOptions` of `reloptions.c` merge and check them. The options are kept as `name=value`, as in `reloptions` and `attoptions`.

use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::Node;

use super::view::{def_string, parse_bool};

/// The kind of the value of an option, with its bounds.
#[derive(Clone, Copy)]
pub(super) enum Kind {
    /// `RELOPT_TYPE_BOOL`.
    Bool,
    /// `RELOPT_TYPE_INT` with the smallest and the largest value.
    Int(i32, i32),
    /// `RELOPT_TYPE_REAL` with the smallest and the largest value.
    Real(f64, f64),
    /// `RELOPT_TYPE_ENUM` with the words that it accepts and the detail of its error.
    Enum(&'static [&'static str], &'static str),
}

/// An option that a kind of relation or a column accepts, as `relopt_gen` gives it. `toast` is true when a TOAST table also accepts it, as `RELOPT_KIND_TOAST` gives it.
pub(super) struct Opt {
    pub(super) name: &'static str,
    pub(super) kind: Kind,
    pub(super) toast: bool,
}

/// `attribute_reloptions`: the options of a column.
pub(super) const ATTRIBUTE: &[Opt] = &[
    Opt { name: "n_distinct", kind: Kind::Real(-1.0, f64::MAX), toast: false },
    Opt { name: "n_distinct_inherited", kind: Kind::Real(-1.0, f64::MAX), toast: false },
];

/// The words of `vacuum_index_cleanup`, as `StdRdOptIndexCleanupValues` gives them.
const INDEX_CLEANUP: Kind = Kind::Enum(
    &["auto", "on", "off", "true", "false", "yes", "no", "1", "0"],
    "Valid values are \"on\", \"off\", and \"auto\".",
);

/// `default_reloptions`: the options of a table and, where `toast` is true, of its TOAST table.
pub(super) const HEAP: &[Opt] = &[
    Opt { name: "fillfactor", kind: Kind::Int(10, 100), toast: false },
    Opt { name: "autovacuum_enabled", kind: Kind::Bool, toast: true },
    Opt { name: "autovacuum_vacuum_threshold", kind: Kind::Int(0, i32::MAX), toast: true },
    Opt { name: "autovacuum_vacuum_max_threshold", kind: Kind::Int(-1, i32::MAX), toast: true },
    Opt { name: "autovacuum_vacuum_insert_threshold", kind: Kind::Int(-1, i32::MAX), toast: true },
    Opt { name: "autovacuum_analyze_threshold", kind: Kind::Int(0, i32::MAX), toast: false },
    Opt { name: "autovacuum_vacuum_cost_limit", kind: Kind::Int(1, 10000), toast: true },
    Opt { name: "autovacuum_freeze_min_age", kind: Kind::Int(0, 1_000_000_000), toast: true },
    Opt { name: "autovacuum_freeze_max_age", kind: Kind::Int(100_000, 2_000_000_000), toast: true },
    Opt { name: "autovacuum_freeze_table_age", kind: Kind::Int(0, 2_000_000_000), toast: true },
    Opt {
        name: "autovacuum_multixact_freeze_min_age",
        kind: Kind::Int(0, 1_000_000_000),
        toast: true,
    },
    Opt {
        name: "autovacuum_multixact_freeze_max_age",
        kind: Kind::Int(10000, 2_000_000_000),
        toast: true,
    },
    Opt {
        name: "autovacuum_multixact_freeze_table_age",
        kind: Kind::Int(0, 2_000_000_000),
        toast: true,
    },
    Opt { name: "log_autovacuum_min_duration", kind: Kind::Int(-1, i32::MAX), toast: true },
    Opt { name: "log_autoanalyze_min_duration", kind: Kind::Int(-1, i32::MAX), toast: false },
    Opt { name: "toast_tuple_target", kind: Kind::Int(128, 8160), toast: false },
    Opt { name: "autovacuum_vacuum_cost_delay", kind: Kind::Real(0.0, 100.0), toast: true },
    Opt { name: "autovacuum_vacuum_scale_factor", kind: Kind::Real(0.0, 100.0), toast: true },
    Opt {
        name: "autovacuum_vacuum_insert_scale_factor",
        kind: Kind::Real(0.0, 100.0),
        toast: true,
    },
    Opt { name: "autovacuum_analyze_scale_factor", kind: Kind::Real(0.0, 100.0), toast: false },
    Opt { name: "user_catalog_table", kind: Kind::Bool, toast: false },
    Opt { name: "parallel_workers", kind: Kind::Int(0, 1024), toast: false },
    Opt { name: "vacuum_index_cleanup", kind: INDEX_CLEANUP, toast: true },
    Opt { name: "vacuum_truncate", kind: Kind::Bool, toast: true },
    Opt { name: "vacuum_max_eager_freeze_failure_rate", kind: Kind::Real(0.0, 1.0), toast: true },
];

/// `view_reloptions`: the options of a view.
pub(super) const VIEW: &[Opt] = &[
    Opt { name: "security_barrier", kind: Kind::Bool, toast: false },
    Opt { name: "security_invoker", kind: Kind::Bool, toast: false },
    Opt {
        name: "check_option",
        kind: Kind::Enum(&["local", "cascaded"], "Valid values are \"local\" and \"cascaded\"."),
        toast: false,
    },
];

/// `btoptions`: the options of a btree index.
pub(super) const BTREE: &[Opt] = &[
    Opt { name: "fillfactor", kind: Kind::Int(10, 100), toast: false },
    Opt { name: "deduplicate_items", kind: Kind::Bool, toast: false },
];

/// `hashoptions`: the options of a hash index.
pub(super) const HASH: &[Opt] =
    &[Opt { name: "fillfactor", kind: Kind::Int(10, 100), toast: false }];

/// `gistoptions`: the options of a gist index.
pub(super) const GIST: &[Opt] = &[
    Opt { name: "fillfactor", kind: Kind::Int(10, 100), toast: false },
    Opt {
        name: "buffering",
        kind: Kind::Enum(&["on", "off", "auto"], "Valid values are \"on\", \"off\", and \"auto\"."),
        toast: false,
    },
];

/// `ginoptions`: the options of a gin index. `MAX_KILOBYTES` is `INT_MAX / 1024`.
pub(super) const GIN: &[Opt] = &[
    Opt { name: "fastupdate", kind: Kind::Bool, toast: false },
    Opt { name: "gin_pending_list_limit", kind: Kind::Int(64, i32::MAX / 1024), toast: false },
];

/// `spgoptions`: the options of an spgist index.
pub(super) const SPGIST: &[Opt] =
    &[Opt { name: "fillfactor", kind: Kind::Int(10, 100), toast: false }];

/// `brinoptions`: the options of a brin index. `BRIN_MAX_PAGES_PER_RANGE` is 131072.
pub(super) const BRIN: &[Opt] = &[
    Opt { name: "pages_per_range", kind: Kind::Int(1, 131_072), toast: false },
    Opt { name: "autosummarize", kind: Kind::Bool, toast: false },
];

/// `spc_relopt_tab`: the options of a tablespace. `MAX_IO_CONCURRENCY` is 1000.
const TABLESPACE: &[Opt] = &[
    Opt { name: "random_page_cost", kind: Kind::Real(0.0, f64::MAX), toast: false },
    Opt { name: "seq_page_cost", kind: Kind::Real(0.0, f64::MAX), toast: false },
    Opt { name: "effective_io_concurrency", kind: Kind::Int(0, 1000), toast: false },
    Opt { name: "maintenance_io_concurrency", kind: Kind::Int(0, 1000), toast: false },
];

/// `transformRelOptions` and `tablespace_reloptions` for `CREATE TABLESPACE`: the options of the statement, each as `name=value`, in the order of the statement. An option cannot have a namespace.
///
/// # Errors
///
/// `22023` for an option that a tablespace does not have, an option that the statement gives twice, or a value that is not valid.
pub fn tablespace_options(defs: &[Option<Node>]) -> Result<Vec<String>> {
    let options = transform(&[], defs, None, &[], false)?;
    validate(&options, TABLESPACE, false)?;
    Ok(options)
}

/// The error of a value that is not valid, `ERRCODE_INVALID_PARAMETER_VALUE`.
fn invalid(message: String) -> Error {
    Error::new(SqlState::INVALID_PARAMETER_VALUE, message)
}

/// `transformRelOptions`: the old options, with the options of the statement replaced, added or removed for `RESET`. An option of the statement with a namespace must have a namespace of `valid`. Only the options with the namespace `namespace` go into the result, or the options with no namespace when it is `None`.
pub(super) fn transform(
    old: &[String],
    defs: &[Option<Node>],
    namespace: Option<&str>,
    valid: &[&str],
    reset: bool,
) -> Result<Vec<String>> {
    transform_with(old, defs, namespace, valid, reset, false)
}

/// `transformRelOptions` with `acceptOidsOff`, for `CREATE TABLE`: `oids` with a false value is dropped, and `oids` with a true value gives an error.
pub(super) fn transform_create(
    defs: &[Option<Node>],
    namespace: Option<&str>,
    valid: &[&str],
) -> Result<Vec<String>> {
    transform_with(&[], defs, namespace, valid, false, true)
}

/// The work of [`transform`] and [`transform_create`].
fn transform_with(
    old: &[String],
    defs: &[Option<Node>],
    namespace: Option<&str>,
    valid: &[&str],
    reset: bool,
    accept_oids_off: bool,
) -> Result<Vec<String>> {
    let defs: Vec<_> = defs
        .iter()
        .flatten()
        .filter_map(|n| match n {
            Node::DefElem(def) => Some(def),
            _ => None,
        })
        .collect();
    let mut out: Vec<String> = old
        .iter()
        .filter(|text| {
            let name = text.split_once('=').map_or(text.as_str(), |(name, _)| name);
            !defs.iter().any(|def| {
                def.defnamespace.as_deref() == namespace && def.defname.as_deref() == Some(name)
            })
        })
        .cloned()
        .collect();
    for def in defs {
        if reset {
            if def.arg.is_some() {
                return Err(Error::new(
                    SqlState::SYNTAX_ERROR,
                    "RESET must not include values for parameters",
                ));
            }
            continue;
        }
        if let Some(space) = def.defnamespace.as_deref()
            && !valid.contains(&space)
        {
            return Err(invalid(format!("unrecognized parameter namespace \"{space}\"")));
        }
        if def.defnamespace.as_deref() != namespace {
            continue;
        }
        let name = def.defname.as_deref().unwrap_or_default();
        if name.contains('=') {
            return Err(invalid(format!("invalid option name \"{name}\": must not contain \"=\"")));
        }
        if accept_oids_off && def.defnamespace.is_none() && name == "oids" {
            if def_boolean(name, def.arg.as_ref())? {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    "tables declared WITH OIDS are not supported",
                ));
            }
            continue;
        }
        out.push(format!("{name}={}", def_string(def.arg.as_ref())));
    }
    Ok(out)
}

/// `defGetBoolean`: no value is true, and the words `true`, `false`, `on` and `off` and the numbers 0 and 1 are the others.
pub(super) fn def_boolean(name: &str, arg: Option<&Node>) -> Result<bool> {
    let value = match arg {
        None => return Ok(true),
        Some(Node::Integer(0)) => Some(false),
        Some(Node::Integer(1)) => Some(true),
        Some(Node::Integer(_)) => None,
        Some(other) => match def_string(Some(other)).to_ascii_lowercase().as_str() {
            "true" | "on" => Some(true),
            "false" | "off" => Some(false),
            _ => None,
        },
    };
    value.ok_or_else(|| {
        Error::new(SqlState::SYNTAX_ERROR, format!("{name} requires a Boolean value"))
    })
}

/// `parseRelOptions` with `validate`: each option is one of `known`, once, with a value of its kind in its bounds. For a TOAST table, `toast` is true and only the options that a TOAST table accepts are known.
pub(super) fn validate(options: &[String], known: &[Opt], toast: bool) -> Result<()> {
    let mut seen: Vec<&str> = Vec::new();
    for text in options {
        let (name, value) = text.split_once('=').unwrap_or((text, ""));
        let Some(opt) = known.iter().find(|o| o.name == name && (o.toast || !toast)) else {
            return Err(invalid(format!("unrecognized parameter \"{name}\"")));
        };
        if seen.contains(&name) {
            return Err(invalid(format!("parameter \"{name}\" specified more than once")));
        }
        seen.push(name);
        let out_of_bounds = |detail: String| {
            invalid(format!("value {value} out of bounds for option \"{name}\""))
                .with_detail(detail)
        };
        match opt.kind {
            Kind::Bool => {
                if parse_bool(value).is_none() {
                    return Err(invalid(format!(
                        "invalid value for boolean option \"{name}\": {value}"
                    )));
                }
            }
            Kind::Int(min, max) => {
                let Some(parsed) = parse_int(value) else {
                    return Err(invalid(format!(
                        "invalid value for integer option \"{name}\": {value}"
                    )));
                };
                if parsed < min || parsed > max {
                    return Err(out_of_bounds(format!(
                        "Valid values are between \"{min}\" and \"{max}\"."
                    )));
                }
            }
            Kind::Real(min, max) => {
                let Some(parsed) = parse_real(value) else {
                    return Err(invalid(format!(
                        "invalid value for floating point option \"{name}\": {value}"
                    )));
                };
                if parsed < min || parsed > max {
                    return Err(out_of_bounds(format!(
                        "Valid values are between \"{min:.6}\" and \"{max:.6}\"."
                    )));
                }
            }
            Kind::Enum(words, detail) => {
                if !words.iter().any(|w| w.eq_ignore_ascii_case(value)) {
                    return Err(invalid(format!(
                        "invalid value for enum option \"{name}\": {value}"
                    ))
                    .with_detail(detail));
                }
            }
        }
    }
    Ok(())
}

/// The value of the integer option `name` in `options`, as the reader of `StdRdOptions` gets it, or `None` when the options do not set it.
pub(super) fn int_option(options: &[String], name: &str) -> Option<i32> {
    options.iter().find_map(|text| {
        let (found, value) = text.split_once('=')?;
        if found == name { parse_int(value) } else { None }
    })
}

/// The white space of `isspace` in the C locale.
fn is_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c')
}

/// `strtol` with base 0: space, a sign, and a hexadecimal number after `0x`, an octal number after `0`, or a decimal number. The result is the value and the rest of the text, or `None` when there is no number or the value does not fit in a `long`.
fn strtol(text: &str) -> Option<(i64, &str)> {
    let s = text.trim_start_matches(is_space);
    let (negative, s) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let hex = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X"));
    let (radix, digits) = match hex {
        Some(rest) if rest.starts_with(|c: char| c.is_ascii_hexdigit()) => (16, rest),
        _ if s.starts_with('0') => (8, s),
        _ => (10, s),
    };
    let end = digits.find(|c: char| !c.is_digit(radix)).unwrap_or(digits.len());
    if end == 0 {
        return None;
    }
    let magnitude = i128::from_str_radix(&digits[..end], radix).ok()?;
    let value = i64::try_from(if negative { -magnitude } else { magnitude }).ok()?;
    Some((value, &digits[end..]))
}

/// `strtod` for decimal numbers: space, a sign, and digits with a fraction and an exponent, or `inf`, `infinity` or `nan`. The result is the value and the rest of the text, or `None` when there is no number or the value is out of the range of a double.
fn strtod(text: &str) -> Option<(f64, &str)> {
    let s = text.trim_start_matches(is_space);
    let body = s.strip_prefix(['-', '+']).unwrap_or(s);
    let sign = s.len() - body.len();
    for word in ["infinity", "inf", "nan"] {
        if body.get(..word.len()).is_some_and(|w| w.eq_ignore_ascii_case(word)) {
            let end = sign + word.len();
            return Some((s[..end].parse().ok()?, &s[end..]));
        }
    }
    let digits = |from: usize| {
        body[from..].find(|c: char| !c.is_ascii_digit()).map_or(body.len(), |n| from + n)
    };
    let mut end = digits(0);
    let whole = end;
    if body[end..].starts_with('.') {
        end = digits(end + 1);
    }
    if whole == 0 && end <= 1 {
        return None;
    }
    let mantissa = &body[..end];
    if body[end..].starts_with(['e', 'E']) {
        let after_sign = end + 1 + usize::from(body[end + 1..].starts_with(['-', '+']));
        let exponent_end = digits(after_sign);
        if exponent_end > after_sign {
            end = exponent_end;
        }
    }
    let value: f64 = s[..sign + end].parse().ok()?;
    // `strtod` sets `ERANGE` for a value that is too large or too small for a double.
    let underflow = (value == 0.0 && mantissa.bytes().any(|b| matches!(b, b'1'..=b'9')))
        || (value != 0.0 && value.abs() < f64::MIN_POSITIVE);
    if value.is_infinite() || underflow {
        return None;
    }
    Some((value, &s[sign + end..]))
}

/// `parse_int` with no unit: `strtol`, or `strtod` and `rint` for a number with a fraction or an exponent or out of the range of a `long`, then space. The value must fit in an `int`.
fn parse_int(text: &str) -> Option<i32> {
    let (value, rest) = match strtol(text) {
        Some((value, rest)) if !rest.starts_with(['.', 'e', 'E']) =>
        {
            #[allow(clippy::cast_precision_loss)]
            (value as f64, rest)
        }
        _ => strtod(text)?,
    };
    if value.is_nan() || !rest.trim_start_matches(is_space).is_empty() {
        return None;
    }
    let value = value.round_ties_even();
    if value > f64::from(i32::MAX) || value < f64::from(i32::MIN) {
        return None;
    }
    #[allow(clippy::cast_possible_truncation)]
    Some(value as i32)
}

/// `parse_real` with no unit: `strtod`, then space. `NaN` is not valid.
fn parse_real(text: &str) -> Option<f64> {
    let (value, rest) = strtod(text)?;
    (!value.is_nan() && rest.trim_start_matches(is_space).is_empty()).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integers() {
        assert_eq!(parse_int(" 70 "), Some(70));
        assert_eq!(parse_int("0x10"), Some(16));
        assert_eq!(parse_int("-0x10"), Some(-16));
        assert_eq!(parse_int("010"), Some(8));
        assert_eq!(parse_int("7e1"), Some(70));
        assert_eq!(parse_int("10.6e1"), Some(106));
        assert_eq!(parse_int("70.5"), Some(70));
        assert_eq!(parse_int("1.5"), Some(2));
        assert_eq!(parse_int(".5"), Some(0));
        assert_eq!(parse_int("2147483648"), None);
        assert_eq!(parse_int("1e10"), None);
        assert_eq!(parse_int("70abc"), None);
        assert_eq!(parse_int("08"), None);
        assert_eq!(parse_int(""), None);
        assert_eq!(parse_int("true"), None);
        assert_eq!(parse_int("inf"), None);
    }

    #[test]
    fn reals() {
        assert_eq!(parse_real(" 0.5 "), Some(0.5));
        assert_eq!(parse_real("1e2"), Some(100.0));
        assert_eq!(parse_real("-inf"), Some(f64::NEG_INFINITY));
        assert_eq!(parse_real("1e400"), None);
        assert_eq!(parse_real("1e-400"), None);
        assert_eq!(parse_real("nan"), None);
        assert_eq!(parse_real("1e"), None);
        assert_eq!(parse_real("."), None);
    }
}
