//! The rows of the views `pg_hba_file_rules` and `pg_ident_file_mappings`, as `hbafuncs.c` of PostgreSQL makes them, and of the view `pg_file_settings`.
//!
//! The server owns the authentication files and the configuration files, so it gives the lines of the files through [`AuthFiles`], and this module puts them in the columns of the views. PostgreSQL reads the files again at each call of the functions, so a view shows the files as they are now, and not the rules that the server loaded. The module also gives the server the regular expressions of the files.

use std::fmt;

use crate::guc::Settings;
use rupg_common::Result;
pub use rupg_func::{AuthMatch, auth_compile, auth_search};
use rupg_types::{Array, Value};

/// The parts of a line of the host rules that the view `pg_hba_file_rules` shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HbaFields {
    /// The connection type, for example `host`.
    pub kind: String,
    pub databases: Vec<String>,
    pub users: Vec<String>,
    /// The address, a host name or a keyword, or `None` for a `local` line.
    pub address: Option<String>,
    /// The mask of a numeric address.
    pub netmask: Option<String>,
    pub method: String,
    /// The options, for example `map=name`, in the order of `get_hba_options`.
    pub options: Vec<String>,
}

/// One line of the host rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HbaRule {
    /// The number of the rule among the lines with no error, or `None` for a line with an error.
    pub rule_number: Option<i32>,
    pub file_name: String,
    pub line_number: i32,
    /// The parts of the line, or `None` for a line that did not parse.
    pub fields: Option<HbaFields>,
    pub error: Option<String>,
}

/// The parts of a line of the user maps that the view `pg_ident_file_mappings` shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentFields {
    pub map_name: String,
    pub sys_name: String,
    pub pg_username: String,
}

/// One line of the user maps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentMapping {
    /// The number of the map among the lines with no error, or `None` for a line with an error.
    pub map_number: Option<i32>,
    pub file_name: String,
    pub line_number: i32,
    /// The parts of the line, or `None` for a line that did not parse.
    pub fields: Option<IdentFields>,
    pub error: Option<String>,
}

/// One line of the configuration files, or one error in them, as the view `pg_file_settings` shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSetting {
    /// The file, or `None` for an error that has no file.
    pub file: Option<String>,
    pub line: i32,
    /// The name of the parameter, or `None` for an error.
    pub name: Option<String>,
    /// The value, or `None` for an error.
    pub setting: Option<String>,
    pub applied: bool,
    pub error: Option<String>,
}

/// The authentication files and the configuration files of a server.
pub trait AuthFiles: Send + Sync + fmt::Debug {
    /// The lines of the host rules, read again from their source.
    ///
    /// # Errors
    ///
    /// The error of a file of the rules that does not open.
    fn hba_rules(&self) -> Result<Vec<HbaRule>>;

    /// The lines of the user maps, read again from their source.
    ///
    /// # Errors
    ///
    /// The error of a file of the maps that does not open.
    fn ident_mappings(&self) -> Result<Vec<IdentMapping>>;

    /// The lines of the configuration files, read again and checked with the settings of the session, as `ProcessConfigFileInternal` gives them when it changes nothing. A server with no configuration file has no lines.
    fn file_settings(&self, settings: &Settings) -> Vec<FileSetting> {
        let _ = settings;
        Vec::new()
    }
}

/// A `text[]` value.
fn text_array(items: Vec<String>) -> Value {
    Value::Array(Box::new(Array::one(items.into_iter().map(|s| Some(Value::text(s))).collect())))
}

fn int4(value: Option<i32>) -> Value {
    value.map_or(Value::Null, Value::Int4)
}

fn text(value: Option<String>) -> Value {
    value.map_or(Value::Null, Value::text)
}

/// `fill_hba_line`: the 11 columns of `pg_hba_file_rules`. A line that did not parse has nulls from the type to the options.
pub(crate) fn hba_values(rule: HbaRule) -> Vec<Value> {
    let mut row =
        vec![int4(rule.rule_number), Value::text(rule.file_name), Value::Int4(rule.line_number)];
    match rule.fields {
        Some(fields) => row.extend([
            Value::text(fields.kind),
            text_array(fields.databases),
            text_array(fields.users),
            text(fields.address),
            text(fields.netmask),
            Value::text(fields.method),
            if fields.options.is_empty() { Value::Null } else { text_array(fields.options) },
        ]),
        None => row.extend([const { Value::Null }; 7]),
    }
    row.push(text(rule.error));
    row
}

/// `show_all_file_settings`: the 7 columns of `pg_file_settings` for the line number `seqno`, from 1.
pub(crate) fn file_values(seqno: usize, setting: FileSetting) -> Vec<Value> {
    let line = if setting.file.is_some() { Value::Int4(setting.line) } else { Value::Null };
    vec![
        text(setting.file),
        line,
        Value::Int4(i32::try_from(seqno).unwrap_or(i32::MAX)),
        text(setting.name),
        text(setting.setting),
        Value::Bool(setting.applied),
        text(setting.error),
    ]
}

/// `fill_ident_line`: the 7 columns of `pg_ident_file_mappings`. A line that did not parse has nulls for the map and the two users.
pub(crate) fn ident_values(mapping: IdentMapping) -> Vec<Value> {
    let mut row = vec![
        int4(mapping.map_number),
        Value::text(mapping.file_name),
        Value::Int4(mapping.line_number),
    ];
    match mapping.fields {
        Some(fields) => row.extend([
            Value::text(fields.map_name),
            Value::text(fields.sys_name),
            Value::text(fields.pg_username),
        ]),
        None => row.extend([const { Value::Null }; 3]),
    }
    row.push(text(mapping.error));
    row
}
