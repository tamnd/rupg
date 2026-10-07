//! The OID alias types, such as `regclass` and `regtype`, and the name lists that their input reads.
//!
//! A port of the parts of `src/backend/utils/adt/regproc.c` that do not read the catalog, and of `SplitIdentifierString` in `varlena.c`. A value of each type is an OID. The input takes a number or a name, and the output gives the name of the object or the number when no object has the OID. The catalog finds the names, so here [`reg_in`] gives the number or the name to look up, and [`reg_out_oid`] writes the text when the catalog has no name. The binary form of each type is the binary form of `oid`.
//!
//! Lifted from `crates/rudb-pgtypes/src/reg.rs` of tamnd/rudb at f5f7065a (spec/04 section 4.9).

use std::borrow::Cow;

use rupg_common::SqlState;

use crate::error::TypeError;
use crate::generated::oids;
use crate::number::{is_space, oid_in};
use crate::scalar::name_in;
use crate::types::Oid;

/// One of the OID alias types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RegKind {
    /// `regproc`, a function by name.
    Proc,
    /// `regprocedure`, a function with its argument types.
    Procedure,
    /// `regoper`, an operator by name.
    Oper,
    /// `regoperator`, an operator with its argument types.
    Operator,
    /// `regclass`, a relation.
    Class,
    /// `regtype`, a type.
    Type,
    /// `regrole`, a role.
    Role,
    /// `regnamespace`, a schema.
    Namespace,
    /// `regcollation`, a collation.
    Collation,
    /// `regconfig`, a text search configuration.
    Config,
    /// `regdictionary`, a text search dictionary.
    Dictionary,
    /// `regdatabase`, a database.
    Database,
}

impl RegKind {
    /// All the kinds, in the order of their OIDs in `pg_type.dat`.
    pub const ALL: [RegKind; 12] = [
        RegKind::Proc,
        RegKind::Procedure,
        RegKind::Oper,
        RegKind::Operator,
        RegKind::Class,
        RegKind::Type,
        RegKind::Config,
        RegKind::Dictionary,
        RegKind::Namespace,
        RegKind::Role,
        RegKind::Collation,
        RegKind::Database,
    ];

    /// The kind of a type OID, or `None` when the type is not an OID alias type.
    pub fn from_oid(oid: Oid) -> Option<RegKind> {
        RegKind::ALL.into_iter().find(|kind| kind.oid() == oid)
    }

    /// The OID of the type.
    pub fn oid(self) -> Oid {
        match self {
            RegKind::Proc => oids::REGPROC,
            RegKind::Procedure => oids::REGPROCEDURE,
            RegKind::Oper => oids::REGOPER,
            RegKind::Operator => oids::REGOPERATOR,
            RegKind::Class => oids::REGCLASS,
            RegKind::Type => oids::REGTYPE,
            RegKind::Role => oids::REGROLE,
            RegKind::Namespace => oids::REGNAMESPACE,
            RegKind::Collation => oids::REGCOLLATION,
            RegKind::Config => oids::REGCONFIG,
            RegKind::Dictionary => oids::REGDICTIONARY,
            RegKind::Database => oids::REGDATABASE,
        }
    }

    /// The name of the type, such as `regclass`.
    pub fn type_name(self) -> &'static str {
        match self {
            RegKind::Proc => "regproc",
            RegKind::Procedure => "regprocedure",
            RegKind::Oper => "regoper",
            RegKind::Operator => "regoperator",
            RegKind::Class => "regclass",
            RegKind::Type => "regtype",
            RegKind::Role => "regrole",
            RegKind::Namespace => "regnamespace",
            RegKind::Collation => "regcollation",
            RegKind::Config => "regconfig",
            RegKind::Dictionary => "regdictionary",
            RegKind::Database => "regdatabase",
        }
    }

    /// The text of OID 0. It is `-`, but the operator types use `0`, because `-` is the name of an operator.
    pub fn zero_text(self) -> &'static str {
        match self {
            RegKind::Oper | RegKind::Operator => "0",
            _ => "-",
        }
    }
}

/// What the text input of an OID alias type found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegInput<'a> {
    /// The input is a number, or `-` for OID 0. The value needs no lookup.
    Oid(u32),
    /// The input is a name. The catalog finds the OID, with the errors of the kind.
    Name(&'a str),
}

/// The first step of the text input of an OID alias type, `parseDashOrOid` and `parseNumericOid`.
///
/// A string of only ASCII digits is an OID, read as `oid` reads it. So `010` is octal and is 8, and `09` is an error of the type `oid`. `-` is OID 0, but not for `regoper` and `regoperator`. Any other string is a name, which the catalog reads with [`qualified_name_list`] or the type parser.
pub fn reg_in(kind: RegKind, s: &str) -> Result<RegInput<'_>, TypeError> {
    if s == "-" && kind.zero_text() == "-" {
        return Ok(RegInput::Oid(0));
    }
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
        return oid_in(s).map(RegInput::Oid);
    }
    Ok(RegInput::Name(s))
}

/// The text output of an OID alias type when the catalog has no object with the OID: the text of OID 0 for the kind, or the number.
pub fn reg_out_oid(kind: RegKind, oid: u32, out: &mut Vec<u8>) {
    if oid == 0 {
        out.extend_from_slice(kind.zero_text().as_bytes());
    } else {
        crate::number::oid_out(oid, out);
    }
}

/// A name in a list of identifiers. An unquoted name is in lowercase. A quoted name keeps its case and has one quote for each pair of quotes. Both are cut to 63 bytes.
fn identifier(token: &str, quoted: bool) -> Cow<'_, str> {
    let name: Cow<'_, str> = if quoted {
        if token.contains("\"\"") {
            Cow::Owned(token.replace("\"\"", "\""))
        } else {
            Cow::Borrowed(token)
        }
    } else if token.bytes().any(|b| b.is_ascii_uppercase()) {
        // Only ASCII letters change, as in `downcase_identifier` for a multibyte encoding.
        Cow::Owned(token.to_ascii_lowercase())
    } else {
        Cow::Borrowed(token)
    };
    match name {
        Cow::Borrowed(s) => Cow::Borrowed(name_in(s)),
        Cow::Owned(mut s) => {
            s.truncate(name_in(&s).len());
            Cow::Owned(s)
        }
    }
}

/// `SplitIdentifierString`: the identifiers in `s` with `separator` between them, or `None` when the syntax is not correct.
///
/// White space can be before and after each identifier. An identifier in double quotes can have any character, and two quotes in it are one quote. An identifier without quotes ends at the separator or at white space, and it cannot be empty. A string of only white space is an empty list. The search path setting and the name input of the OID alias types use this syntax.
pub fn split_identifier_string(s: &str, separator: u8) -> Option<Vec<Cow<'_, str>>> {
    let b = s.as_bytes();
    let skip_space = |mut at: usize| {
        while at < b.len() && is_space(b[at]) {
            at += 1;
        }
        at
    };
    let mut names = Vec::new();
    let mut at = skip_space(0);
    if at == b.len() {
        return Some(names);
    }
    loop {
        if b[at] == b'"' {
            // The end is the first quote that is not one of a pair.
            let start = at + 1;
            let mut end = start;
            loop {
                end += b[end..].iter().position(|&c| c == b'"')?;
                if b.get(end + 1) != Some(&b'"') {
                    break;
                }
                end += 2;
            }
            names.push(identifier(&s[start..end], true));
            at = end + 1;
        } else {
            let start = at;
            while at < b.len() && b[at] != separator && !is_space(b[at]) {
                at += 1;
            }
            if at == start {
                return None;
            }
            names.push(identifier(&s[start..at], false));
        }
        at = skip_space(at);
        if at == b.len() {
            return Some(names);
        }
        if b[at] != separator {
            return None;
        }
        at = skip_space(at + 1);
        if at == b.len() {
            // A separator at the end needs one more name, and an empty name is an error.
            return None;
        }
    }
}

/// `stringToQualifiedNameList`: the parts of a name such as `schema.table`, with `42602` and `invalid name syntax` when the syntax is not correct or there is no name.
pub fn qualified_name_list(s: &str) -> Result<Vec<Cow<'_, str>>, TypeError> {
    match split_identifier_string(s, b'.') {
        Some(names) if !names.is_empty() => Ok(names),
        _ => Err(TypeError::new(SqlState::INVALID_NAME, "invalid name syntax".to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_input_takes_a_number_or_a_dash() {
        assert_eq!(reg_in(RegKind::Class, "-"), Ok(RegInput::Oid(0)));
        assert_eq!(reg_in(RegKind::Oper, "-"), Ok(RegInput::Name("-")));
        assert_eq!(reg_in(RegKind::Type, "010"), Ok(RegInput::Oid(8)));
        assert_eq!(reg_in(RegKind::Type, "4294967295"), Ok(RegInput::Oid(u32::MAX)));
        assert_eq!(reg_in(RegKind::Type, " 1"), Ok(RegInput::Name(" 1")));
        assert_eq!(reg_in(RegKind::Type, ""), Ok(RegInput::Name("")));
        let error = reg_in(RegKind::Class, "09").unwrap_err();
        assert_eq!(error.message, "invalid input syntax for type oid: \"09\"");
        let error = reg_in(RegKind::Class, "4294967296").unwrap_err();
        assert_eq!(error.message, "value \"4294967296\" is out of range for type oid");
    }

    #[test]
    fn the_output_of_zero_depends_on_the_kind() {
        let text = |kind, oid| {
            let mut out = Vec::new();
            reg_out_oid(kind, oid, &mut out);
            String::from_utf8(out).unwrap()
        };
        assert_eq!(text(RegKind::Class, 0), "-");
        assert_eq!(text(RegKind::Operator, 0), "0");
        assert_eq!(text(RegKind::Type, 70000), "70000");
        for kind in RegKind::ALL {
            assert_eq!(RegKind::from_oid(kind.oid()), Some(kind));
        }
        assert_eq!(RegKind::from_oid(oids::OID), None);
    }

    #[test]
    fn name_lists_split_at_the_separator() {
        let list = |s| qualified_name_list(s).map(|names| names.join("|"));
        assert_eq!(list("Ab . \"C\"\"d\""), Ok("ab|C\"d".to_owned()));
        assert_eq!(list("\"\""), Ok(String::new()));
        assert_eq!(list("a\"b.c"), Ok("a\"b|c".to_owned()));
        assert_eq!(list("ÉA"), Ok("Éa".to_owned()));
        for bad in ["", "  ", "a.", ".a", "a..b", "a b", "\"a", "\"a\"b", "a.\"b\"\""] {
            assert_eq!(list(bad).unwrap_err().message, "invalid name syntax", "{bad:?}");
        }
        let long = format!("{}é", "a".repeat(62));
        assert_eq!(list(&long), Ok("a".repeat(62)));
        assert_eq!(split_identifier_string(" a , B ", b',').unwrap(), ["a", "b"]);
    }
}
