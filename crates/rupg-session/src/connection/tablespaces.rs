//! `CREATE TABLESPACE`, `DROP TABLESPACE`, `ALTER TABLESPACE ... SET`, `ALTER TABLESPACE ... RENAME TO` and `ALTER TABLESPACE ... OWNER TO`: the checks of `CreateTableSpace`, `DropTableSpace`, `AlterTableSpaceOptions`, `RenameTableSpace` and `AlterObjectOwner_internal` in the order of PostgreSQL, on the rows of `pg_tablespace` that the store keeps. The statements of `ALTER TABLESPACE` keep the changed rows in the roles of the transaction, and the commit moves them to the store. The engine keeps no files for a tablespace, so it makes only the in-place tablespaces of `allow_in_place_tablespaces`, and a table cannot be in a tablespace yet. Thus a tablespace is always empty, and `DROP TABLESPACE` has no files to check.

use std::path::Path;
use std::sync::Arc;

use rupg_catalog::{ObjRef, PG_CLASS, RelKind, Tablespace};
use rupg_common::{Error, SqlState};
use rupg_pgcatalog::roles::Roles;
use rupg_sql::Severity;
use rupg_sql::nodes::{
    AlterOwnerStmt, AlterTableSpaceOptionsStmt, CreateTableSpaceStmt, DropTableSpaceStmt, Node,
    RenameStmt, RoleSpecType,
};
use rupg_wire::CommandTag;

use super::roles::{has_privs_of_role, role_name};
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
        let options = rupg_analyze::tablespace_options(&[], &stmt.options, false)?;
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
        // A partitioned relation has no files, so `pg_shdepend` keeps its tablespace, and `checkSharedDependencies` reports it.
        let current = self.catalog();
        // The objects of the current database come first, then a count for each other database.
        let (mut lines, mut remote) = (Vec::new(), Vec::new());
        for (row, catalog) in self.store.catalogs() {
            let catalog = if row.oid == self.database_oid { &current } else { &catalog };
            let mut dependents: Vec<u32> = catalog
                .relations()
                .filter(|r| {
                    r.tablespace == oid
                        && matches!(r.kind, RelKind::Partitioned | RelKind::PartitionedIndex)
                })
                .map(|r| r.oid)
                .collect();
            if row.oid != self.database_oid {
                match dependents.len() {
                    0 => {}
                    1 => remote.push(format!("1 object in database {}", row.name)),
                    n => remote.push(format!("{n} objects in database {}", row.name)),
                }
                continue;
            }
            dependents.sort_unstable();
            let reader = self.reader();
            for relation in dependents {
                let object = ObjRef::new(PG_CLASS, relation);
                let text = rupg_func::describe(object, &reader)?.unwrap_or_default();
                lines.push(format!("tablespace for {text}"));
            }
        }
        lines.extend(remote);
        if !lines.is_empty() {
            return Err(Error::new(
                SqlState::DEPENDENT_OBJECTS_STILL_EXIST,
                format!(
                    "tablespace \"{name}\" cannot be dropped because some objects depend on it"
                ),
            )
            .with_detail(lines.join("\n")));
        }
        // `destroy_tablespace_directories` finds the files of a relation or of a database in the tablespace.
        let used = self.store.catalogs().iter().any(|(row, catalog)| {
            row.tablespace == oid
                || if row.oid == self.database_oid {
                    current.uses_tablespace(oid)
                } else {
                    catalog.uses_tablespace(oid)
                }
        });
        if used {
            return Err(Error::new(
                SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                format!("tablespace \"{name}\" is not empty"),
            ));
        }
        self.store.drop_tablespace(oid);
        Ok(Done::Tag(CommandTag::DropTablespace))
    }

    /// The rows of `pg_tablespace` of the tablespaces that `CREATE TABLESPACE` made, with the changes of the transaction.
    pub(super) fn tablespace_rows(&self) -> Arc<[Tablespace]> {
        self.store.tablespaces_with(&self.roles().tablespaces)
    }

    /// The row of `pg_tablespace` of the tablespace of the name, with the changes of the transaction, or the error `42704`. The row of `pg_default` or `pg_global` is static.
    fn tablespace_row(&self, name: &str) -> Result<Tablespace, Error> {
        let builtin = |oid: u32| Tablespace {
            oid,
            name: name.to_owned(),
            owner: BOOTSTRAP_SUPERUSER,
            acl: None,
            options: Vec::new(),
        };
        match name {
            "pg_default" => Ok(builtin(DEFAULT_TABLESPACE)),
            "pg_global" => Ok(builtin(GLOBAL_TABLESPACE)),
            _ => self.tablespace_rows().iter().find(|t| t.name == name).cloned().ok_or_else(|| {
                Error::new(
                    SqlState::UNDEFINED_OBJECT,
                    format!("tablespace \"{name}\" does not exist"),
                )
            }),
        }
    }

    /// Puts a changed row of `pg_tablespace` in the changes of the transaction. The commit of the transaction moves the row to the store, and a rollback discards it. The engine cannot change the static rows of `pg_default` and `pg_global` yet.
    fn change_tablespace(&self, row: &Tablespace, statement: &str) -> Result<(), Error> {
        if matches!(row.oid, DEFAULT_TABLESPACE | GLOBAL_TABLESPACE) {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!("{statement} of a tablespace of initdb is not supported yet"),
            ));
        }
        let mut held = self.lease.borrow_mut();
        let Some(lease) = held.as_mut() else {
            return Err(Error::internal("the lock of the catalog is gone"));
        };
        let mut work = Roles::clone(&lease.roles);
        work.tablespaces.retain(|c| c.oid != row.oid);
        work.tablespaces.push(rupg_analyze::tablespace_change(row));
        lease.roles = Arc::new(work);
        Ok(())
    }

    /// `object_ownercheck` of a tablespace.
    fn owns_tablespace(&self, row: &Tablespace, me: u32) -> bool {
        let roles = self.roles();
        roles.superuser(me) || has_privs_of_role(&roles, me, row.owner)
    }

    /// `AlterTableSpaceOptions`: the check of the owner, then the new options of `transformRelOptions` and their check by `tablespace_reloptions`.
    pub(super) fn alter_tablespace_options(
        &self,
        stmt: &AlterTableSpaceOptionsStmt,
        me: u32,
    ) -> Result<Done, Error> {
        let name = stmt.tablespacename.as_deref().unwrap_or_default();
        let mut row = self.tablespace_row(name)?;
        if !self.owns_tablespace(&row, me) {
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("must be owner of tablespace {name}"),
            ));
        }
        row.options = rupg_analyze::tablespace_options(&row.options, &stmt.options, stmt.isReset)?;
        self.change_tablespace(&row, "ALTER TABLESPACE")?;
        Ok(Done::Tag(CommandTag::AlterTablespace))
    }

    /// `RenameTableSpace`: the checks of the owner and of the new name, then the new `spcname`.
    pub(super) fn rename_tablespace(&self, stmt: &RenameStmt, me: u32) -> Result<Done, Error> {
        let old = stmt.subname.as_deref().unwrap_or_default();
        let new = stmt.newname.as_deref().unwrap_or_default();
        let mut row = self.tablespace_row(old)?;
        if !self.owns_tablespace(&row, me) {
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("permission denied for tablespace {old}"),
            ));
        }
        if self.settings().get("allow_system_table_mods").as_deref() != Some("on")
            && new.starts_with("pg_")
        {
            return Err(Error::new(
                SqlState::RESERVED_NAME,
                format!("unacceptable tablespace name \"{new}\""),
            )
            .with_detail("The prefix \"pg_\" is reserved for system tablespaces."));
        }
        if new.contains(['\n', '\r']) {
            return Err(Error::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!(
                    "tablespace name \"{new}\" contains a newline or carriage return character"
                ),
            ));
        }
        if self.tablespace_oid(new).is_some() {
            return Err(Error::new(
                SqlState::DUPLICATE_OBJECT,
                format!("tablespace \"{new}\" already exists"),
            ));
        }
        new.clone_into(&mut row.name);
        self.change_tablespace(&row, "ALTER TABLESPACE RENAME")?;
        Ok(Done::Tag(CommandTag::AlterTablespace))
    }

    /// `ExecAlterOwnerStmt` and `AlterObjectOwner_internal` of a tablespace: the checks of the owner and of `SET ROLE`, then `spcacl` with the new owner.
    pub(super) fn alter_tablespace_owner(
        &self,
        stmt: &AlterOwnerStmt,
        me: u32,
    ) -> Result<Done, Error> {
        let roles = self.roles();
        let new = match &stmt.newowner {
            Some(spec) => self.spec_oid(&roles, spec)?,
            None => return Err(Error::internal("ALTER TABLESPACE OWNER with no owner")),
        };
        let name = match &stmt.object {
            Some(Node::String(name)) => &**name,
            _ => return Err(Error::internal("ALTER TABLESPACE OWNER with no name")),
        };
        let mut row = self.tablespace_row(name)?;
        if row.owner == new {
            return Ok(Done::Tag(CommandTag::AlterTablespace));
        }
        if !roles.superuser(me) {
            if !has_privs_of_role(&roles, me, row.owner) {
                return Err(Error::new(
                    SqlState::INSUFFICIENT_PRIVILEGE,
                    format!("must be owner of tablespace {name}"),
                ));
            }
            if !roles.can_set_role(me, new) {
                return Err(Error::new(
                    SqlState::INSUFFICIENT_PRIVILEGE,
                    format!("must be able to SET ROLE \"{}\"", role_name(&roles, new)),
                ));
            }
        }
        row.acl = row.acl.as_deref().map(|acl| {
            rupg_catalog::acl_new_owner(acl, &role_name(&roles, row.owner), &role_name(&roles, new))
        });
        row.owner = new;
        self.change_tablespace(&row, "ALTER TABLESPACE OWNER")?;
        Ok(Done::Tag(CommandTag::AlterTablespace))
    }

    /// `get_tablespace_oid`: the OID and the owner of the tablespace of the name.
    pub(super) fn tablespace_oid(&self, name: &str) -> Option<(u32, u32)> {
        match name {
            "pg_default" => Some((DEFAULT_TABLESPACE, BOOTSTRAP_SUPERUSER)),
            "pg_global" => Some((GLOBAL_TABLESPACE, BOOTSTRAP_SUPERUSER)),
            _ => self.tablespace_rows().iter().find(|t| t.name == name).map(|t| (t.oid, t.owner)),
        }
    }
}
