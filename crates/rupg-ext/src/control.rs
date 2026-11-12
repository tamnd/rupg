//! The control file of an extension: `parse_extension_control_file` of `extension.c`, over the text of the file.

use rupg_common::{Error, Result, SqlState};

/// The facts of a control file, `ExtensionControlFile` of `extension.c`. A secondary control file, `name--version.control`, changes some of them for one version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Control {
    /// The name of the extension, the name of the file without `.control`.
    pub name: String,
    /// `directory`: the directory of the scripts, or `None` for the directory of the control file.
    pub directory: Option<String>,
    /// `default_version`: the version that `CREATE EXTENSION` installs without `VERSION`.
    pub default_version: Option<String>,
    /// `module_pathname`: the text that replaces `MODULE_PATHNAME` in a script.
    pub module_pathname: Option<String>,
    /// `comment`: the comment of the extension, which `CREATE EXTENSION` sets.
    pub comment: Option<String>,
    /// `schema`: the only schema that the extension can be in, or `None`.
    pub schema: Option<String>,
    /// `relocatable`: true when `ALTER EXTENSION SET SCHEMA` can move the objects of the extension.
    pub relocatable: bool,
    /// `superuser`: true when only a superuser can install or update the extension, unless it is trusted.
    pub superuser: bool,
    /// `trusted`: true when a user with `CREATE` on the database can install the extension, which then runs its script as the bootstrap superuser.
    pub trusted: bool,
    /// `encoding`: the encoding of the scripts, or `None` for the encoding of the database.
    pub encoding: Option<String>,
    /// `requires`: the extensions that must be installed first.
    pub requires: Vec<String>,
    /// `no_relocate`: the required extensions whose schema `@extschema:name@` names, so they cannot move.
    pub no_relocate: Vec<String>,
}

impl Control {
    /// `new_ExtensionControlFile`: the facts of a control file that sets nothing.
    pub fn new(name: &str) -> Control {
        Control {
            name: name.to_string(),
            directory: None,
            default_version: None,
            module_pathname: None,
            comment: None,
            schema: None,
            relocatable: false,
            superuser: true,
            trusted: false,
            encoding: None,
            requires: Vec::new(),
            no_relocate: Vec::new(),
        }
    }

    /// `read_extension_control_file`: the facts of the primary control file of `name`, with the text `text` and the path `file`.
    ///
    /// # Errors
    ///
    /// The errors of [`Control::apply`].
    pub fn parse(name: &str, text: &str, file: &str) -> Result<Control> {
        let mut control = Control::new(name);
        control.apply(text, file, None)?;
        Ok(control)
    }

    /// `read_extension_aux_control_file`: these facts, changed by the secondary control file of `version`, with the text `text` and the path `file`.
    ///
    /// # Errors
    ///
    /// The errors of [`Control::apply`].
    pub fn with_secondary(&self, version: &str, text: &str, file: &str) -> Result<Control> {
        let mut control = self.clone();
        control.apply(text, file, Some(version))?;
        Ok(control)
    }

    /// `parse_extension_control_file`: sets the facts that the text of a control file sets. `version` is the version of a secondary control file, or `None` for the primary one.
    ///
    /// # Errors
    ///
    /// `42601` for a parameter that rupg does not know, for `directory` or `default_version` in a secondary file, for a line that is not `name = value`, and for `schema` with `relocatable`. `22023` for a value of the wrong form.
    pub fn apply(&mut self, text: &str, file: &str, version: Option<&str>) -> Result<()> {
        for (name, value) in parse_config(text, file)? {
            let primary_only = |name: &str| {
                if version.is_some() {
                    return Err(Error::new(
                        SqlState::SYNTAX_ERROR,
                        format!(
                            "parameter \"{name}\" cannot be set in a secondary extension control file"
                        ),
                    ));
                }
                Ok(())
            };
            let boolean = |name: &str, value: &str| {
                parse_bool(value).ok_or_else(|| {
                    Error::new(
                        SqlState::INVALID_PARAMETER_VALUE,
                        format!("parameter \"{name}\" requires a Boolean value"),
                    )
                })
            };
            let names = |name: &str, value: &str| {
                split_identifiers(value).ok_or_else(|| {
                    Error::new(
                        SqlState::INVALID_PARAMETER_VALUE,
                        format!("parameter \"{name}\" must be a list of extension names"),
                    )
                })
            };
            match name.as_str() {
                "directory" => {
                    primary_only(&name)?;
                    self.directory = Some(value);
                }
                "default_version" => {
                    primary_only(&name)?;
                    self.default_version = Some(value);
                }
                "module_pathname" => self.module_pathname = Some(value),
                "comment" => self.comment = Some(value),
                "schema" => self.schema = Some(value),
                "relocatable" => self.relocatable = boolean(&name, &value)?,
                "superuser" => self.superuser = boolean(&name, &value)?,
                "trusted" => self.trusted = boolean(&name, &value)?,
                "encoding" => {
                    if !valid_server_encoding(&value) {
                        return Err(Error::new(
                            SqlState::UNDEFINED_OBJECT,
                            format!("\"{value}\" is not a valid encoding name"),
                        ));
                    }
                    self.encoding = Some(value);
                }
                "requires" => self.requires = names(&name, &value)?,
                "no_relocate" => self.no_relocate = names(&name, &value)?,
                _ => {
                    return Err(Error::new(
                        SqlState::SYNTAX_ERROR,
                        format!("unrecognized parameter \"{name}\" in file \"{file}\""),
                    ));
                }
            }
        }
        if self.relocatable && self.schema.is_some() {
            return Err(Error::new(
                SqlState::SYNTAX_ERROR,
                "parameter \"schema\" cannot be specified when \"relocatable\" is true",
            ));
        }
        Ok(())
    }
}

/// `pg_valid_server_encoding` for the names of the encodings that a script can have. rupg keeps every database in UTF8, and a script in `SQL_ASCII` or `UTF8` needs no conversion.
fn valid_server_encoding(name: &str) -> bool {
    let name: String =
        name.chars().filter(char::is_ascii_alphanumeric).map(|c| c.to_ascii_lowercase()).collect();
    matches!(name.as_str(), "utf8" | "unicode" | "sqlascii")
}

/// `parse_bool`: the Boolean values of a setting.
fn parse_bool(value: &str) -> Option<bool> {
    let value = value.trim().to_ascii_lowercase();
    if value.is_empty() {
        return None;
    }
    let prefix = |word: &str| word.starts_with(value.as_str());
    match value.as_str() {
        "on" => Some(true),
        "of" | "off" => Some(false),
        "o" => None,
        "1" => Some(true),
        "0" => Some(false),
        _ if prefix("true") || prefix("yes") => Some(true),
        _ if prefix("false") || prefix("no") => Some(false),
        _ => None,
    }
}

/// `SplitIdentifierString` with a comma: the names of a list, with a name in double quotes as it is and another name in lower case. `None` for a list of the wrong form.
pub fn split_identifiers(text: &str) -> Option<Vec<String>> {
    let mut names = Vec::new();
    let mut rest = text.trim_start();
    if rest.is_empty() {
        return Some(names);
    }
    loop {
        let name;
        if let Some(quoted) = rest.strip_prefix('"') {
            let mut out = String::new();
            let mut chars = quoted.char_indices();
            let end = loop {
                match chars.next() {
                    None => return None,
                    Some((i, '"')) => {
                        if quoted[i + 1..].starts_with('"') {
                            out.push('"');
                            chars.next();
                        } else {
                            break i + 1;
                        }
                    }
                    Some((_, c)) => out.push(c),
                }
            };
            name = out;
            rest = &quoted[end..];
        } else {
            let end = rest.find(|c: char| c == ',' || c.is_whitespace()).unwrap_or(rest.len());
            if end == 0 {
                return None;
            }
            name = rest[..end].to_lowercase();
            rest = &rest[end..];
        }
        if name.is_empty() {
            return None;
        }
        names.push(name);
        rest = rest.trim_start();
        match rest.strip_prefix(',') {
            Some(after) => rest = after.trim_start(),
            None if rest.is_empty() => return Some(names),
            None => return None,
        }
    }
}

/// `ParseConfigFp` of `guc-file.l`: the pairs of name and value of a file in the form of `postgresql.conf`. A line is empty, a comment from `#`, or `name [=] value [# comment]`, with the value as a word, a number or a string in single quotes.
fn parse_config(text: &str, file: &str) -> Result<Vec<(String, String)>> {
    let mut pairs = Vec::new();
    for (number, line) in text.lines().enumerate() {
        let error = || {
            Error::new(
                SqlState::SYNTAX_ERROR,
                format!("syntax error in file \"{file}\" line {}, near token", number + 1),
            )
        };
        let line = line.trim_start();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let end = line
            .find(|c: char| {
                !(c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') || !c.is_ascii())
            })
            .unwrap_or(line.len());
        if end == 0 {
            return Err(error());
        }
        let name = line[..end].to_ascii_lowercase();
        let mut rest = line[end..].trim_start();
        if let Some(after) = rest.strip_prefix('=') {
            rest = after.trim_start();
        }
        let value;
        if let Some(quoted) = rest.strip_prefix('\'') {
            let (text, len) = dequote(quoted).ok_or_else(error)?;
            value = text;
            rest = &quoted[len..];
        } else {
            let end = rest.find(|c: char| c.is_whitespace() || c == '#').unwrap_or(rest.len());
            if end == 0 {
                return Err(error());
            }
            value = rest[..end].to_string();
            rest = &rest[end..];
        }
        let rest = rest.trim_start();
        if !(rest.is_empty() || rest.starts_with('#')) {
            return Err(error());
        }
        pairs.push((name, value));
    }
    Ok(pairs)
}

/// `DeescapeQuotedString`: the text of a string in single quotes from after the opening quote, and the length of the string with the closing quote. A doubled quote is a quote, and a backslash escapes as in a string with `E`.
fn dequote(text: &str) -> Option<(String, usize)> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' if bytes.get(i + 1) == Some(&b'\'') => {
                out.push(b'\'');
                i += 2;
            }
            b'\'' => return Some((String::from_utf8(out).ok()?, i + 1)),
            b'\n' => return None,
            b'\\' => {
                let next = *bytes.get(i + 1)?;
                i += 2;
                match next {
                    b'b' => out.push(8),
                    b'f' => out.push(12),
                    b'n' => out.push(b'\n'),
                    b'r' => out.push(b'\r'),
                    b't' => out.push(b'\t'),
                    b'0'..=b'7' => {
                        let mut value = u32::from(next - b'0');
                        for _ in 0..2 {
                            match bytes.get(i) {
                                Some(digit @ b'0'..=b'7') => {
                                    value = value * 8 + u32::from(digit - b'0');
                                    i += 1;
                                }
                                _ => break,
                            }
                        }
                        out.push(u8::try_from(value & 0xff).ok()?);
                    }
                    other => out.push(other),
                }
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary() {
        let text = "# pg_trgm extension\ncomment = 'text similarity measurement and index searching based on trigrams'\ndefault_version = '1.6'\nmodule_pathname = '$libdir/pg_trgm'\nrelocatable = true\ntrusted = true\n";
        let control = Control::parse("pg_trgm", text, "pg_trgm.control").unwrap();
        assert_eq!(control.default_version.as_deref(), Some("1.6"));
        assert_eq!(control.module_pathname.as_deref(), Some("$libdir/pg_trgm"));
        assert!(control.relocatable && control.trusted && control.superuser);
        assert_eq!(
            control.comment.as_deref(),
            Some("text similarity measurement and index searching based on trigrams")
        );
    }

    #[test]
    fn forms() {
        let text = "requires = 'cube, \"Earth\"'\nsuperuser false # a comment\nschema='it''s'\n";
        let control = Control::parse("x", text, "x.control").unwrap();
        assert_eq!(control.requires, ["cube", "Earth"]);
        assert!(!control.superuser);
        assert_eq!(control.schema.as_deref(), Some("it's"));
    }

    #[test]
    fn errors() {
        let err = Control::parse("x", "bogus = 1\n", "x.control").unwrap_err();
        assert_eq!(err.message(), "unrecognized parameter \"bogus\" in file \"x.control\"");
        let err = Control::parse("x", "trusted = maybe\n", "x.control").unwrap_err();
        assert_eq!(err.message(), "parameter \"trusted\" requires a Boolean value");
        let err = Control::parse("x", "relocatable = true\nschema = s\n", "x.control").unwrap_err();
        assert_eq!(err.state(), SqlState::SYNTAX_ERROR);
        let control = Control::new("x");
        let err =
            control.with_secondary("1.0", "default_version = '2'", "x--1.0.control").unwrap_err();
        assert_eq!(
            err.message(),
            "parameter \"default_version\" cannot be set in a secondary extension control file"
        );
    }

    #[test]
    fn identifiers() {
        assert_eq!(split_identifiers(" a , B,\"c d\" ").unwrap(), ["a", "b", "c d"]);
        assert_eq!(split_identifiers("").unwrap(), Vec::<String>::new());
        assert!(split_identifiers("a,,b").is_none());
        assert!(split_identifiers("a b").is_none());
    }
}
