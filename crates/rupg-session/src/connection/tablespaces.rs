//! `CREATE TABLESPACE` and `DROP TABLESPACE`: the checks of `CreateTableSpace` and `DropTableSpace` in the order of PostgreSQL, on the rows of `pg_tablespace` that the store keeps. The engine keeps no files for a tablespace, so it makes only the in-place tablespaces of `allow_in_place_tablespaces`, and a table cannot be in a tablespace yet. Thus a tablespace is always empty, and `DROP TABLESPACE` has no files to check.

use std::path::Path;

use rupg_catalog::Tablespace;
use rupg_common::{Error, SqlState};
use rupg_sql::Severity;
use rupg_sql::nodes::{CreateTableSpaceStmt, DropTableSpaceStmt, RoleSpecType};
use rupg_wire::CommandTag;

use super::{Connection, Done};
use crate::utility::Notice;

/// The OIDs of `pg_default` and `pg_global`, which `initdb` pins.
const DEFAULT_TABLESPACE: u32 = 1663;
const GLOBAL_TABLESPACE: u32 = 1664;
/// The OID of the bootstrap superuser, which owns the tablespaces of `initdb`.
const BOOTSTRAP_SUPERUSER: u32 = 10;

/// `canonicalize_path`, for the parts that a location of a tablespace needs: the trailing slashes go away, and a run of slashes becomes one.
fn canonical(location: &str) -> String {
    let mut out = String::with_capacity(location.len());
    for c in location.chars() {
        if !(c == '/' && out.ends_with('/')) {
            out.push(c);
        }
    }
    while out.len() > 1 && out.ends_with('/') {
        out.pop();
    }
    out
}

impl Connection {
    /// `CreateTableSpace`.
    pub(super) fn create_tablespace(&mut self, stmt: &CreateTableSpaceStmt) -> Result<Done, Error> {
        self.prevent_block("CREATE TABLESPACE")?;
        let name = stmt.tablespacename.as_deref().unwrap_or_default();
        if !self.superuser() {
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("permission denied to create tablespace \"{name}\""),
            )
            .with_hint("Must be superuser to create a tablespace."));
        }
        let owner = match stmt.owner.as_deref() {
            Some(spec) => match spec.roletype {
                RoleSpecType::ROLESPEC_CSTRING => {
                    self.role_named(spec.rolename.as_deref().unwrap_or_default())?
                }
                RoleSpecType::ROLESPEC_PUBLIC => self.role_named("public")?,
                _ => self.current_role(),
            },
            None => self.current_role(),
        };
        let location = canonical(stmt.location.as_deref().unwrap_or_default());
        if location.contains('\'') {
            return Err(Error::new(
                SqlState::INVALID_NAME,
                "tablespace location cannot contain single quotes",
            ));
        }
        if name.contains(['\n', '\r']) {
            return Err(Error::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!(
                    "tablespace name \"{name}\" contains a newline or carriage return character"
                ),
            ));
        }
        let setting = |name: &str| self.settings().get(name).as_deref() == Some("on");
        let in_place = setting("allow_in_place_tablespaces") && location.is_empty();
        if !in_place && !location.starts_with('/') {
            return Err(Error::new(
                SqlState::INVALID_OBJECT_DEFINITION,
                "tablespace location must be an absolute path",
            ));
        }
        if !setting("allow_system_table_mods") && name.starts_with("pg_") {
            return Err(Error::new(
                SqlState::RESERVED_NAME,
                format!("unacceptable tablespace name \"{name}\""),
            )
            .with_detail("The prefix \"pg_\" is reserved for system tablespaces."));
        }
        if self.tablespace_oid(name).is_some() {
            return Err(Error::new(
                SqlState::DUPLICATE_OBJECT,
                format!("tablespace \"{name}\" already exists"),
            ));
        }
        let options = rupg_analyze::tablespace_options(&stmt.options)?;
        if !in_place {
            // `create_tablespace_directories`: the directory must exist. The engine keeps no files in it yet.
            if let Some(io) = &self.io
                && !io.exists(Path::new(&location))?
            {
                return Err(Error::new(
                    SqlState::UNDEFINED_FILE,
                    format!("directory \"{location}\" does not exist"),
                ));
            }
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "CREATE TABLESPACE with a location is not supported yet",
            ));
        }
        self.store.create_tablespace(Tablespace {
            oid: 0,
            name: name.to_owned(),
            owner,
            acl: None,
            options,
        })?;
        Ok(Done::Tag(CommandTag::CreateTablespace))
    }

    /// `DropTableSpace`.
    pub(super) fn drop_tablespace(
        &mut self,
        stmt: &DropTableSpaceStmt,
        notices: &mut Vec<Notice>,
    ) -> Result<Done, Error> {
        self.prevent_block("DROP TABLESPACE")?;
        let name = stmt.tablespacename.as_deref().unwrap_or_default();
        let Some((oid, owner)) = self.tablespace_oid(name) else {
            if !stmt.missing_ok {
                return Err(Error::new(
                    SqlState::UNDEFINED_OBJECT,
                    format!("tablespace \"{name}\" does not exist"),
                ));
            }
            notices.push(Notice {
                severity: Severity::Notice,
                error: Error::new(
                    SqlState::SUCCESSFUL_COMPLETION,
                    format!("tablespace \"{name}\" does not exist, skipping"),
                ),
            });
            return Ok(Done::Tag(CommandTag::DropTablespace));
        };
        if !self.superuser() && owner != self.current_role() {
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("must be owner of tablespace {name}"),
            ));
        }
        if oid == DEFAULT_TABLESPACE || oid == GLOBAL_TABLESPACE {
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("permission denied for tablespace {name}"),
            ));
        }
        self.store.drop_tablespace(oid);
        Ok(Done::Tag(CommandTag::DropTablespace))
    }

    /// `get_tablespace_oid`: the OID and the owner of the tablespace of the name.
    fn tablespace_oid(&self, name: &str) -> Option<(u32, u32)> {
        match name {
            "pg_default" => Some((DEFAULT_TABLESPACE, BOOTSTRAP_SUPERUSER)),
            "pg_global" => Some((GLOBAL_TABLESPACE, BOOTSTRAP_SUPERUSER)),
            _ => self.store.tablespaces().iter().find(|t| t.name == name).map(|t| (t.oid, t.owner)),
        }
    }
}
