//! The options of relations and of columns, as `transformRelOptions` and `parseRelOptions` of `reloptions.c` merge and check them. The options are kept as `name=value`, as in `reloptions` and `attoptions`.

use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::Node;

use super::view::def_string;

/// The kind of the value of an option, with its bounds.
#[derive(Clone, Copy)]
pub(super) enum Kind {
    /// `RELOPT_TYPE_REAL` with the smallest and the largest value.
    Real(f64, f64),
}

/// An option that a kind of relation or a column accepts, as `relopt_gen` gives it.
pub(super) struct Opt {
    pub(super) name: &'static str,
    pub(super) kind: Kind,
}

/// `attribute_reloptions`: the options of a column.
pub(super) const ATTRIBUTE: &[Opt] = &[
    Opt { name: "n_distinct", kind: Kind::Real(-1.0, f64::MAX) },
    Opt { name: "n_distinct_inherited", kind: Kind::Real(-1.0, f64::MAX) },
];

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
        out.push(format!("{name}={}", def_string(def.arg.as_ref())));
    }
    Ok(out)
}

/// `parseRelOptions` with `validate`: each option is one of `known`, once, with a value of its kind in its bounds.
pub(super) fn validate(options: &[String], known: &[Opt]) -> Result<()> {
    let mut seen: Vec<&str> = Vec::new();
    for text in options {
        let (name, value) = text.split_once('=').unwrap_or((text, ""));
        let Some(opt) = known.iter().find(|o| o.name == name) else {
            return Err(invalid(format!("unrecognized parameter \"{name}\"")));
        };
        if seen.contains(&name) {
            return Err(invalid(format!("parameter \"{name}\" specified more than once")));
        }
        seen.push(name);
        match opt.kind {
            Kind::Real(min, max) => {
                let Some(parsed) = parse_real(value) else {
                    return Err(invalid(format!(
                        "invalid value for floating point option \"{name}\": {value}"
                    )));
                };
                if parsed < min || parsed > max {
                    return Err(invalid(format!(
                        "value {value} out of bounds for option \"{name}\""
                    ))
                    .with_detail(format!(
                        "Valid values are between \"{min:.6}\" and \"{max:.6}\"."
                    )));
                }
            }
        }
    }
    Ok(())
}

/// The white space of `isspace` in the C locale.
fn is_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c')
}

/// `parse_real` with no unit: space, a decimal number or `inf`, and space. `NaN` and a value out of the range of a double are not valid.
fn parse_real(text: &str) -> Option<f64> {
    let s = text.trim_matches(is_space);
    let unsigned = s.strip_prefix(['-', '+']).unwrap_or(s);
    if unsigned.eq_ignore_ascii_case("inf") || unsigned.eq_ignore_ascii_case("infinity") {
        return Some(if s.starts_with('-') { f64::NEG_INFINITY } else { f64::INFINITY });
    }
    let (mantissa, exponent) = match unsigned.find(['e', 'E']) {
        Some(at) => (&unsigned[..at], Some(&unsigned[at + 1..])),
        None => (unsigned, None),
    };
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits = |part: &str| part.bytes().all(|b| b.is_ascii_digit());
    if whole.len() + fraction.len() == 0 || !digits(whole) || !digits(fraction) {
        return None;
    }
    if let Some(exponent) = exponent {
        let exponent = exponent.strip_prefix(['-', '+']).unwrap_or(exponent);
        if exponent.is_empty() || !digits(exponent) {
            return None;
        }
    }
    let value: f64 = s.parse().ok()?;
    // `strtod` sets `ERANGE` for a value that is too large or too small for a double.
    let underflow = value == 0.0 && mantissa.bytes().any(|b| matches!(b, b'1'..=b'9'));
    (value.is_finite() && !underflow).then_some(value)
}
