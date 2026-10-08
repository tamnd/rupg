//! The type of a `TypeName`, as `typenameTypeIdAndMod` of `parse_type.c` finds it: the OID from the name and the array bounds, and the typmod from the type modifiers and the `typmodin` function of the type.

use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin::{self, TypeRow};
use rupg_sql::nodes::{Node, TypeName};
use rupg_types::typmod::{
    INTERVAL_FULL_PRECISION, INTERVAL_FULL_RANGE, MAX_TIME_PRECISION, VARHDRSZ, interval_typmod,
    numeric_typmod,
};

use crate::coerce::AtOpt;
use crate::{Analyzer, Env, Params};

/// `MaxAttrSize`, the largest length of `varchar(n)` and `character(n)`.
const MAX_ATTR_SIZE: i32 = 10 * 1024 * 1024;
/// `NUMERIC_MAX_PRECISION`.
const NUMERIC_MAX_PRECISION: i32 = 1000;
/// `NUMERIC_MIN_SCALE` and `NUMERIC_MAX_SCALE`.
const NUMERIC_SCALE: (i32, i32) = (-1000, 1000);

/// The bits of the fields of an interval range, as `INTERVAL_MASK` gives them.
const fn mask(bits: &[u8]) -> i32 {
    let mut mask = 0;
    let mut i = 0;
    while i < bits.len() {
        mask |= 1 << bits[i];
        i += 1;
    }
    mask
}

/// The range masks that `intervaltypmodin` takes.
const INTERVAL_RANGES: [i32; 14] = [
    mask(&[2]),
    mask(&[1]),
    mask(&[3]),
    mask(&[10]),
    mask(&[11]),
    mask(&[12]),
    mask(&[2, 1]),
    mask(&[3, 10]),
    mask(&[3, 10, 11]),
    mask(&[3, 10, 11, 12]),
    mask(&[10, 11]),
    mask(&[10, 11, 12]),
    mask(&[11, 12]),
    INTERVAL_FULL_RANGE,
];

/// `TypeNameToString`: the name as the query wrote it, for an error message.
pub(crate) fn type_name_text(name: &TypeName) -> String {
    let mut text = names(&name.names).join(".");
    if name.pct_type {
        text.push_str("%TYPE");
    }
    if !name.arrayBounds.is_empty() {
        text.push_str("[]");
    }
    text
}

/// The strings of a list of `String` nodes.
pub(crate) fn names(list: &[Option<Node>]) -> Vec<&str> {
    list.iter()
        .filter_map(|n| match n {
            Some(Node::String(s)) => Some(&**s),
            _ => None,
        })
        .collect()
}

/// The location of a node in the raw tree as a byte offset, or `None` for -1.
pub(crate) fn place(location: i32) -> Option<usize> {
    usize::try_from(location).ok()
}

/// The bytes that `scanner_isspace` takes as white space.
fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

/// `parseTypeString`: the OID and the typmod of the type that a string names, as the input of `regtype` and `to_regtypemod` read it.
///
/// # Errors
///
/// The outer error is an error for any caller: a syntax error, `SETOF`, a name with too many dots and a bad type modifier. The inner error is a type or a schema that does not exist, which `to_regtype` gives as a null. No error has a place, except a syntax error, whose place is in `text`.
pub fn parse_type(text: &str, env: &dyn Env) -> Result<Result<(u32, i32)>> {
    let invalid = || Error::new(SqlState::SYNTAX_ERROR, format!("invalid type name \"{text}\""));
    if text.bytes().all(is_space) {
        return Err(invalid());
    }
    let name = rupg_sql::parse_type_name(text).map_err(|e| {
        // `scanner_errposition` counts characters, and the session reads the place as a byte offset in the query text.
        let at = e.location.map(|at| text[..at.min(text.len())].chars().count());
        Error::from(e).with_position(at).with_context(format!("invalid type name \"{text}\""))
    })?;
    if name.setof {
        return Err(invalid());
    }
    let mut analyzer = Analyzer::new(env, &Params::default());
    match analyzer.type_name(&name) {
        Ok(found) => Ok(Ok(found)),
        Err(e) if e.state() == SqlState::UNDEFINED_SCHEMA => Ok(Err(e.with_position(None))),
        Err(e) if e.state() == SqlState::UNDEFINED_OBJECT => Ok(Err(Error::new(
            SqlState::UNDEFINED_OBJECT,
            format!("type \"{}\" does not exist", type_name_text(&name)),
        ))),
        Err(e) => Err(e.with_position(None)),
    }
}

/// The facts of a type that a type name reads.
struct Found {
    oid: u32,
    /// The OID of the array type, or 0.
    array: u32,
    /// The OID of the `typmodin` function, or 0.
    modin: u32,
}

impl Found {
    fn builtin(row: &TypeRow) -> Found {
        Found { oid: row.oid, array: row.array, modin: row.modin }
    }

    /// A row type or an array type of the catalog, which takes no type modifier.
    fn user(ty: &rupg_catalog::Type) -> Found {
        Found { oid: ty.oid, array: ty.array, modin: 0 }
    }
}

impl Analyzer<'_> {
    /// `typenameTypeIdAndMod`: the OID and the typmod of a type name.
    pub(crate) fn type_name(&mut self, name: &TypeName) -> Result<(u32, i32)> {
        let at = place(name.location);
        if name.pct_type {
            return Err(Error::new(SqlState::FEATURE_NOT_SUPPORTED, "%TYPE is not supported yet")
                .at_opt(at));
        }
        let mut row = if name.typeOid != 0 {
            self.type_by_oid(name.typeOid)
        } else {
            self.lookup_type(&names(&name.names), at)?
        };
        if !name.arrayBounds.is_empty() {
            // `get_array_type`: an array type has no array type, so the name finds no type.
            row = row.and_then(|element| self.type_by_oid(element.array));
        }
        let Some(row) = row else {
            return Err(Error::new(
                SqlState::UNDEFINED_OBJECT,
                format!("type \"{}\" does not exist", type_name_text(name)),
            )
            .at_opt(at));
        };
        let first = self.notices.len();
        let typmod = self.type_modifier(name, &row).map_err(|e| e.at_opt(at))?;
        for notice in &mut self.notices[first..] {
            *notice = notice.clone().at_opt(at);
        }
        Ok((row.oid, typmod))
    }

    /// The type with this OID, a built-in type or a type of the catalog.
    fn type_by_oid(&self, oid: u32) -> Option<Found> {
        if oid == 0 {
            return None;
        }
        if let Some(row) = builtin::type_by_oid(oid) {
            return Some(Found::builtin(row));
        }
        self.env.catalog()?.type_by_oid(oid).map(Found::user)
    }

    /// The type with this name in the schema.
    fn type_in(&self, namespace: u32, name: &str) -> Option<Found> {
        if let Some(row) = builtin::type_by_name(namespace, name) {
            return Some(Found::builtin(row));
        }
        self.env.catalog()?.type_by_name(namespace, name).map(Found::user)
    }

    /// `LookupTypeName`: the type with this name, in the schema that the name gives or in the search path.
    fn lookup_type(&self, names: &[&str], at: Option<usize>) -> Result<Option<Found>> {
        match names {
            [name] => Ok(self.path.iter().find_map(|&ns| self.type_in(ns, name))),
            [schema, name] => {
                let ns = self.schema(schema).ok_or_else(|| {
                    Error::new(
                        SqlState::UNDEFINED_SCHEMA,
                        format!("schema \"{schema}\" does not exist"),
                    )
                    .at_opt(at)
                })?;
                Ok(self.type_in(ns, name))
            }
            [catalog, schema, name] => {
                if *catalog != self.env.database() {
                    return Err(Error::new(
                        SqlState::FEATURE_NOT_SUPPORTED,
                        format!(
                            "cross-database references are not implemented: {}",
                            names.join(".")
                        ),
                    )
                    .at_opt(at));
                }
                self.lookup_type(&[schema, name], at)
            }
            _ => Err(Error::new(
                SqlState::SYNTAX_ERROR,
                format!("improper qualified name (too many dotted names): {}", names.join(".")),
            )
            .at_opt(at)),
        }
    }

    /// `typenameTypeMod`: the typmod from the type modifiers of the name, with the `typmodin` function of the type.
    fn type_modifier(&mut self, name: &TypeName, row: &Found) -> Result<i32> {
        if name.typemod != -1 {
            return Ok(name.typemod);
        }
        if name.typmods.is_empty() {
            return Ok(-1);
        }
        let modin = builtin::proc_by_oid(row.modin).map(|p| p.src);
        let Some(modin) = modin else {
            return Err(Error::new(
                SqlState::SYNTAX_ERROR,
                format!("type modifier is not allowed for type \"{}\"", type_name_text(name)),
            ));
        };
        let mut texts = Vec::new();
        for modifier in &name.typmods {
            let text = match modifier {
                Some(Node::A_Const(c)) => match &c.val {
                    Some(Node::Integer(v)) => Some(v.to_string()),
                    Some(Node::Float(v) | Node::String(v)) => Some(v.to_string()),
                    _ => None,
                },
                Some(Node::ColumnRef(c)) => match c.fields.as_slice() {
                    [Some(Node::String(s))] => Some(s.to_string()),
                    _ => None,
                },
                _ => None,
            };
            let Some(text) = text else {
                return Err(Error::new(
                    SqlState::SYNTAX_ERROR,
                    "type modifiers must be simple constants or identifiers",
                ));
            };
            texts.push(text);
        }
        let mut values = Vec::with_capacity(texts.len());
        for text in &texts {
            // `ArrayGetIntegerTypmods` reads each modifier with `pg_strtoint32`.
            let value = rupg_types::int4_in(text).map_err(Error::from)?;
            values.push(value);
        }
        self.typmod_in(modin, &values)
    }

    /// The `typmodin` functions of the built-in types.
    fn typmod_in(&mut self, modin: &str, values: &[i32]) -> Result<i32> {
        let invalid = |what: &str| {
            Error::new(SqlState::INVALID_PARAMETER_VALUE, format!("invalid {what}type modifier"))
        };
        match modin {
            "varchartypmodin" | "bpchartypmodin" => {
                let name = if modin == "varchartypmodin" { "varchar" } else { "char" };
                let &[n] = values else { return Err(invalid("")) };
                if n < 1 {
                    return Err(Error::new(
                        SqlState::INVALID_PARAMETER_VALUE,
                        format!("length for type {name} must be at least 1"),
                    ));
                }
                if n > MAX_ATTR_SIZE {
                    return Err(Error::new(
                        SqlState::INVALID_PARAMETER_VALUE,
                        format!("length for type {name} cannot exceed {MAX_ATTR_SIZE}"),
                    ));
                }
                Ok(VARHDRSZ + n)
            }
            "bittypmodin" | "varbittypmodin" => {
                let name = if modin == "bittypmodin" { "bit" } else { "varbit" };
                let &[n] = values else { return Err(invalid("")) };
                if n < 1 {
                    return Err(Error::new(
                        SqlState::INVALID_PARAMETER_VALUE,
                        format!("length for type {name} must be at least 1"),
                    ));
                }
                if n > MAX_ATTR_SIZE * 8 {
                    return Err(Error::new(
                        SqlState::INVALID_PARAMETER_VALUE,
                        format!("length for type {name} cannot exceed {}", MAX_ATTR_SIZE * 8),
                    ));
                }
                Ok(n)
            }
            "numerictypmodin" => {
                let (precision, scale) = match *values {
                    [p] => (p, 0),
                    [p, s] => (p, s),
                    _ => return Err(invalid("NUMERIC ")),
                };
                if !(1..=NUMERIC_MAX_PRECISION).contains(&precision) {
                    return Err(Error::new(
                        SqlState::INVALID_PARAMETER_VALUE,
                        format!(
                            "NUMERIC precision {precision} must be between 1 and {NUMERIC_MAX_PRECISION}"
                        ),
                    ));
                }
                if !(NUMERIC_SCALE.0..=NUMERIC_SCALE.1).contains(&scale) {
                    return Err(Error::new(
                        SqlState::INVALID_PARAMETER_VALUE,
                        format!(
                            "NUMERIC scale {scale} must be between {} and {}",
                            NUMERIC_SCALE.0, NUMERIC_SCALE.1
                        ),
                    ));
                }
                Ok(numeric_typmod(precision, scale))
            }
            "timetypmodin" | "timetztypmodin" | "timestamptypmodin" | "timestamptztypmodin" => {
                let &[n] = values else { return Err(invalid("")) };
                let what = if modin.starts_with("timestamp") { "TIMESTAMP" } else { "TIME" };
                let tz = if modin.contains("tz") { " WITH TIME ZONE" } else { "" };
                self.precision(n, &format!("{what}({n}){tz}"))
            }
            "intervaltypmodin" => {
                if let Some(range) = values.first()
                    && !INTERVAL_RANGES.contains(range)
                {
                    return Err(invalid("INTERVAL "));
                }
                match *values {
                    [range] if range == INTERVAL_FULL_RANGE => Ok(-1),
                    [range] => Ok(interval_typmod(INTERVAL_FULL_PRECISION, range)),
                    [range, precision] => Ok(interval_typmod(
                        self.precision(precision, &format!("INTERVAL({precision})"))?,
                        range,
                    )),
                    _ => Err(invalid("INTERVAL ")),
                }
            }
            _ => Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!("the type modifier function {modin} is not supported yet"),
            )),
        }
    }

    /// The check of a fractional second precision: an error below zero, and a warning and the largest precision above it.
    pub(crate) fn precision(&mut self, n: i32, what: &str) -> Result<i32> {
        if n < 0 {
            return Err(Error::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!("{what} precision must not be negative"),
            ));
        }
        if n > MAX_TIME_PRECISION {
            self.notices.push(Error::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!("{what} precision reduced to maximum allowed, {MAX_TIME_PRECISION}"),
            ));
            return Ok(MAX_TIME_PRECISION);
        }
        Ok(n)
    }
}
