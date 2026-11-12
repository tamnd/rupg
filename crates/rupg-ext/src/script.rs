//! The text of a script as `execute_extension_script` of `extension.c` runs it: the lines of `\echo` are empty, and the macros `@extowner@`, `@extschema@`, `@extschema:name@` and `MODULE_PATHNAME` are replaced.

use rupg_common::{Error, Result, SqlState};
use rupg_func::quote_identifier;

use crate::Control;

/// The characters that no quoting of a name can make safe in every kind of string of a script.
const QUOTING_RELEVANT: &str = "\"$'\\";

/// What the macros of a script stand for.
#[derive(Clone, Copy, Debug)]
pub struct Macros<'a> {
    /// The name of the user that runs `CREATE EXTENSION` or `ALTER EXTENSION`, for `@extowner@`.
    pub owner: &'a str,
    /// The name of the schema of the extension, for `@extschema@`.
    pub schema: &'a str,
    /// The name and the schema of each required extension, for `@extschema:name@`.
    pub required: &'a [(&'a str, &'a str)],
}

/// The text of the script `text` of the extension `control`, ready to run.
///
/// # Errors
///
/// `22P02` when a name that replaces a macro has a character of [`QUOTING_RELEVANT`].
pub fn prepare(text: &str, control: &Control, macros: &Macros<'_>) -> Result<String> {
    let invalid = |what: String| {
        Error::new(
            SqlState::INVALID_TEXT_REPRESENTATION,
            format!("invalid character in {what}: must not contain any of \"{QUOTING_RELEVANT}\""),
        )
    };
    let unsafe_name = |name: &str| name.contains(|c| QUOTING_RELEVANT.contains(c));
    let mut sql = strip_echo(text);
    if text.contains("@extowner@") {
        sql = sql.replace("@extowner@", &quote_identifier(macros.owner));
        if unsafe_name(macros.owner) {
            return Err(invalid("extension owner".into()));
        }
    }
    if !control.relocatable && sql.contains("@extschema@") {
        sql = sql.replace("@extschema@", &quote_identifier(macros.schema));
        if unsafe_name(macros.schema) {
            return Err(invalid(format!("extension \"{}\" schema", control.name)));
        }
    }
    for (name, schema) in macros.required {
        let token = format!("@extschema:{name}@");
        if sql.contains(&token) {
            sql = sql.replace(&token, &quote_identifier(schema));
            if unsafe_name(schema) {
                return Err(invalid(format!("extension \"{name}\" schema")));
            }
        }
    }
    if let Some(path) = &control.module_pathname {
        sql = sql.replace("MODULE_PATHNAME", path);
    }
    Ok(sql)
}

/// `regexp_replace(sql, '^\\echo.*$', '', 'ng')`: each line that begins with `\echo` is empty, with its end of line kept.
fn strip_echo(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        if line.starts_with("\\echo") {
            if line.ends_with('\n') {
                out.push('\n');
            }
        } else {
            out.push_str(line);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macros() {
        let mut control = Control::new("e");
        control.module_pathname = Some("$libdir/e".into());
        let text = "\\echo Use \"CREATE EXTENSION e\" to load this file. \\quit\nCREATE FUNCTION @extschema@.f() RETURNS int AS 'MODULE_PATHNAME' LANGUAGE C;\nGRANT ALL ON @extschema:r@.t TO @extowner@;\n";
        let macros = Macros { owner: "Bob", schema: "my schema", required: &[("r", "public")] };
        let sql = prepare(text, &control, &macros).unwrap();
        assert_eq!(
            sql,
            "\nCREATE FUNCTION \"my schema\".f() RETURNS int AS '$libdir/e' LANGUAGE C;\nGRANT ALL ON public.t TO \"Bob\";\n"
        );
        control.relocatable = true;
        let sql = prepare("SELECT '@extschema@'", &control, &macros).unwrap();
        assert_eq!(sql, "SELECT '@extschema@'");
    }

    #[test]
    fn unsafe_names() {
        let control = Control::new("e");
        let macros = Macros { owner: "o", schema: "a'b", required: &[] };
        assert!(prepare("SELECT 1", &control, &macros).is_ok());
        let err = prepare("SELECT '@extschema@'", &control, &macros).unwrap_err();
        assert_eq!(
            err.message(),
            "invalid character in extension \"e\" schema: must not contain any of \"\"$'\\\""
        );
    }
}
