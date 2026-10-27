//! `CREATE FOREIGN DATA WRAPPER`, `ALTER FOREIGN DATA WRAPPER`, `CREATE SERVER`, `ALTER SERVER` and `CREATE`, `ALTER` and `DROP USER MAPPING`, as `CreateForeignDataWrapper`, `AlterForeignDataWrapper`, `CreateForeignServer`, `AlterForeignServer`, `CreateUserMapping`, `AlterUserMapping` and `RemoveUserMapping` of `foreigncmds.c` run them, with `RENAME TO` and `OWNER TO` as `AlterObjectRename_internal`, `AlterForeignDataWrapperOwner_internal` and `AlterForeignServerOwner_internal` run them.

use rupg_catalog::{
    ForeignDataWrapper, ForeignServer, ObjRef, PG_FOREIGN_DATA_WRAPPER, PG_FOREIGN_SERVER,
    PG_USER_MAPPING, UserMapping,
};
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin;
use rupg_sql::nodes::{
    AlterFdwStmt, AlterForeignServerStmt, AlterOwnerStmt, AlterUserMappingStmt, CreateFdwStmt,
    CreateForeignServerStmt, CreateUserMappingStmt, DefElem, DefElemAction, DropUserMappingStmt,
    List, Node, RenameStmt, RoleSpec, RoleSpecType,
};
use rupg_types::oid;

use super::Definer;
use super::comment::not_owner;
use super::sequence::conflicting;
use super::tsearch::def_string;
use crate::from::levenshtein;
use crate::typename::names;

/// `libpq_conninfo_options` of `foreign.c`: the options that `postgresql_fdw_validator` takes, with the catalog that each one is for.
const CONNINFO_OPTIONS: [(&str, u32); 15] = [
    ("authtype", PG_FOREIGN_SERVER),
    ("service", PG_FOREIGN_SERVER),
    ("user", PG_USER_MAPPING),
    ("password", PG_USER_MAPPING),
    ("connect_timeout", PG_FOREIGN_SERVER),
    ("dbname", PG_FOREIGN_SERVER),
    ("host", PG_FOREIGN_SERVER),
    ("hostaddr", PG_FOREIGN_SERVER),
    ("port", PG_FOREIGN_SERVER),
    ("tty", PG_FOREIGN_SERVER),
    ("options", PG_FOREIGN_SERVER),
    ("requiressl", PG_FOREIGN_SERVER),
    ("sslmode", PG_FOREIGN_SERVER),
    ("gsslib", PG_FOREIGN_SERVER),
    ("gssdelegation", PG_FOREIGN_SERVER),
];

/// The name of a `DefElem`.
fn def_name(def: &DefElem) -> &str {
    def.defname.as_deref().unwrap_or_default()
}

/// The `DefElem` items of a list.
fn defs(list: &List) -> impl Iterator<Item = &DefElem> {
    list.iter().flatten().filter_map(|node| match node {
        Node::DefElem(def) => Some(&**def),
        _ => None,
    })
}

/// The error of a missing foreign-data wrapper.
fn no_fdw(name: &str) -> Error {
    Error::new(
        SqlState::UNDEFINED_OBJECT,
        format!("foreign-data wrapper \"{name}\" does not exist"),
    )
}

/// The error of a missing server.
fn no_server(name: &str) -> Error {
    Error::new(SqlState::UNDEFINED_OBJECT, format!("server \"{name}\" does not exist"))
}

/// `MappingUserName`: the name of the role of a user mapping, or `public` for 0.
fn mapping_user(role: u32) -> String {
    if role == 0 {
        return "public".to_string();
    }
    rupg_pgcatalog::roles::current().name(role).unwrap_or_default().to_string()
}

/// `untransformRelOptions`: the name of an option of the form `name=value`.
fn option_name(option: &str) -> &str {
    option.split_once('=').map_or(option, |(name, _)| name)
}

/// `postgresql_fdw_validator`: each option must be an option of `libpq` for the catalog. An unknown option gives the error with the hint of the closest option, as `updateClosestMatch` finds it.
fn postgresql_fdw_validator(options: &[String], catalog: u32) -> Result<()> {
    for option in options {
        let name = option_name(option);
        if CONNINFO_OPTIONS.iter().any(|&(o, c)| c == catalog && o == name) {
            continue;
        }
        let mut valid = false;
        let mut best: Option<(usize, &str)> = None;
        for &(candidate, _) in CONNINFO_OPTIONS.iter().filter(|&&(_, c)| c == catalog) {
            valid = true;
            if name.is_empty() {
                continue;
            }
            let distance = levenshtein(name, candidate);
            if distance <= 4
                && distance <= name.len() / 2
                && best.is_none_or(|(min, _)| distance < min)
            {
                best = Some((distance, candidate));
            }
        }
        let error = Error::new(SqlState::SYNTAX_ERROR, format!("invalid option \"{name}\""));
        return Err(match (valid, best) {
            (false, _) => error.with_hint("There are no valid options in this context."),
            (true, Some((_, closest))) => {
                error.with_hint(format!("Perhaps you meant the option \"{closest}\"."))
            }
            (true, None) => error,
        });
    }
    Ok(())
}

/// The name of a statement that names a foreign-data wrapper or a server.
fn foreign_name(object: Option<&Node>) -> &str {
    match object {
        Some(Node::String(name)) => name,
        _ => "",
    }
}

/// The functions of `parse_func_options`: `None` for a function that the statement does not give, and an OID or 0 for one that it gives.
#[derive(Default)]
struct FuncOptions {
    handler: Option<u32>,
    validator: Option<u32>,
    connection: Option<u32>,
}

impl Definer<'_, '_> {
    /// `parse_func_options` with `lookup_fdw_handler_func`, `lookup_fdw_validator_func` and `lookup_fdw_connection_func`.
    fn func_options(&self, list: &List) -> Result<FuncOptions> {
        let mut found = FuncOptions::default();
        for def in defs(list) {
            let func = match &def.arg {
                Some(Node::List(list)) => names(list),
                _ => Vec::new(),
            };
            let shown = func.join(".");
            let (slot, args, rettype): (_, &[u32], _) = match def_name(def) {
                "handler" => (&mut found.handler, &[], Some((oid::FDW_HANDLER, "fdw_handler"))),
                "validator" => (&mut found.validator, &[oid::TEXT_ARRAY, oid::OID], None),
                "connection" => (
                    &mut found.connection,
                    &[oid::OID, oid::OID, oid::INTERNAL],
                    Some((oid::TEXT, "text")),
                ),
                name => {
                    return Err(Error::internal(format!("option \"{name}\" not recognized")));
                }
            };
            if slot.is_some() {
                return Err(conflicting(def));
            }
            if func.is_empty() {
                *slot = Some(0);
                continue;
            }
            let (proc, returns) = self.func_oid(&func, args)?;
            if let Some((wanted, word)) = rettype
                && returns != wanted
            {
                return Err(Error::new(
                    SqlState::WRONG_OBJECT_TYPE,
                    format!("function {shown} must return type {word}"),
                ));
            }
            *slot = Some(proc);
        }
        Ok(found)
    }

    /// `transformGenericOptions`: the options after the changes of the statement, as `name=value`, and then the call of the validator for the catalog.
    fn generic_options(
        &self,
        catalog: u32,
        old: &[String],
        list: &List,
        validator: u32,
    ) -> Result<Vec<String>> {
        let mut result: Vec<(String, String)> = old
            .iter()
            .map(|o| match o.split_once('=') {
                Some((name, value)) => (name.to_string(), value.to_string()),
                None => (o.clone(), String::new()),
            })
            .collect();
        for def in defs(list) {
            let name = def_name(def);
            let at = result.iter().position(|(n, _)| n == name);
            match (def.defaction, at) {
                (DefElemAction::DEFELEM_DROP | DefElemAction::DEFELEM_SET, None) => {
                    return Err(Error::new(
                        SqlState::UNDEFINED_OBJECT,
                        format!("option \"{name}\" not found"),
                    ));
                }
                (DefElemAction::DEFELEM_DROP, Some(at)) => {
                    result.remove(at);
                }
                (DefElemAction::DEFELEM_SET, Some(at)) => {
                    result[at] = (name.to_string(), def_string(def)?);
                }
                (_, Some(_)) => {
                    return Err(Error::new(
                        SqlState::DUPLICATE_OBJECT,
                        format!("option \"{name}\" provided more than once"),
                    ));
                }
                (_, None) => result.push((name.to_string(), def_string(def)?)),
            }
        }
        let mut options = Vec::with_capacity(result.len());
        for (name, value) in result {
            if name.contains('=') {
                return Err(Error::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    format!("invalid option name \"{name}\": must not contain \"=\""),
                ));
            }
            options.push(format!("{name}={value}"));
        }
        self.validate_options(validator, &options, catalog)?;
        Ok(options)
    }

    /// The call of the validator of a foreign-data wrapper. rupg runs `postgresql_fdw_validator` and does not call another validator.
    fn validate_options(&self, validator: u32, options: &[String], catalog: u32) -> Result<()> {
        if builtin::proc_by_oid(validator).is_some_and(|p| p.name == "postgresql_fdw_validator") {
            return postgresql_fdw_validator(options, catalog);
        }
        Ok(())
    }

    /// `CreateForeignDataWrapper`.
    pub(super) fn create_fdw(&mut self, stmt: &CreateFdwStmt) -> Result<()> {
        let name = stmt.fdwname.as_deref().unwrap_or_default();
        if !self.is_superuser() {
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("permission denied to create foreign-data wrapper \"{name}\""),
            )
            .with_hint("Must be superuser to create a foreign-data wrapper."));
        }
        if self.catalog.fdw_by_name(name).is_some() {
            return Err(Error::new(
                SqlState::DUPLICATE_OBJECT,
                format!("foreign-data wrapper \"{name}\" already exists"),
            ));
        }
        let funcs = self.func_options(&stmt.func_options)?;
        let validator = funcs.validator.unwrap_or_default();
        let options =
            self.generic_options(PG_FOREIGN_DATA_WRAPPER, &[], &stmt.options, validator)?;
        self.catalog.create_fdw(ForeignDataWrapper {
            oid: 0,
            name: name.to_string(),
            owner: self.user,
            handler: funcs.handler.unwrap_or_default(),
            validator,
            connection: funcs.connection.unwrap_or_default(),
            options,
        });
        Ok(())
    }

    /// `AlterForeignDataWrapper`, with the warnings of a new handler, a new validator and a new connection function.
    pub(super) fn alter_fdw(&mut self, stmt: &AlterFdwStmt) -> Result<()> {
        let name = stmt.fdwname.as_deref().unwrap_or_default();
        if !self.is_superuser() {
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("permission denied to alter foreign-data wrapper \"{name}\""),
            )
            .with_hint("Must be superuser to alter a foreign-data wrapper."));
        }
        let fdw = self.catalog.fdw_by_name(name).cloned().ok_or_else(|| no_fdw(name))?;
        let funcs = self.func_options(&stmt.func_options)?;
        if funcs.handler.is_some() {
            self.warning(
                SqlState::WARNING,
                "changing the foreign-data wrapper handler can change behavior of existing foreign tables"
                    .to_string(),
            );
        }
        if funcs.validator.is_some_and(|v| v != 0) {
            self.warning(
                SqlState::WARNING,
                "changing the foreign-data wrapper validator can cause the options for dependent objects to become invalid"
                    .to_string(),
            );
        }
        if let Some(connection) = funcs.connection
            && fdw.connection != 0
        {
            let message = if connection != 0 {
                "changing the foreign-data wrapper connection function can cause the options for dependent objects to become invalid"
            } else {
                "removing the foreign-data wrapper connection function will cause dependent subscriptions to fail"
            };
            self.warning(SqlState::WARNING, message.to_string());
        }
        let validator = funcs.validator.unwrap_or(fdw.validator);
        let options = if !stmt.options.is_empty() {
            Some(self.generic_options(
                PG_FOREIGN_DATA_WRAPPER,
                &fdw.options,
                &stmt.options,
                validator,
            )?)
        } else {
            None
        };
        self.catalog.alter_fdw(fdw.oid, |f| {
            f.handler = funcs.handler.unwrap_or(f.handler);
            f.validator = validator;
            f.connection = funcs.connection.unwrap_or(f.connection);
            if let Some(options) = options {
                f.options = options;
            }
        });
        Ok(())
    }

    /// `object_aclcheck` of `USAGE` on a foreign-data wrapper for the role, with the error of `aclcheck_error`.
    fn check_fdw_usage(&self, fdw: &ForeignDataWrapper, role: u32) -> Result<()> {
        if self.foreign_usage_allowed(true, fdw.oid, role) {
            return Ok(());
        }
        Err(Error::new(
            SqlState::INSUFFICIENT_PRIVILEGE,
            format!("permission denied for foreign-data wrapper {}", fdw.name),
        ))
    }

    /// `CreateForeignServer`. With `IF NOT EXISTS`, a server that exists gives a notice.
    pub(super) fn create_server(&mut self, stmt: &CreateForeignServerStmt) -> Result<()> {
        let name = stmt.servername.as_deref().unwrap_or_default();
        if self.catalog.server_by_name(name).is_some() {
            let message = format!("server \"{name}\" already exists");
            if stmt.if_not_exists {
                self.notice(SqlState::DUPLICATE_OBJECT, format!("{message}, skipping"));
                return Ok(());
            }
            return Err(Error::new(SqlState::DUPLICATE_OBJECT, message));
        }
        let fdw_name = stmt.fdwname.as_deref().unwrap_or_default();
        let fdw = self.catalog.fdw_by_name(fdw_name).cloned().ok_or_else(|| no_fdw(fdw_name))?;
        self.check_fdw_usage(&fdw, self.user)?;
        let options = self.generic_options(PG_FOREIGN_SERVER, &[], &stmt.options, fdw.validator)?;
        self.catalog.create_server(ForeignServer {
            oid: 0,
            name: name.to_string(),
            owner: self.user,
            fdw: fdw.oid,
            kind: stmt.servertype.as_deref().map(str::to_string),
            version: stmt.version.as_deref().map(str::to_string),
            options,
        });
        Ok(())
    }

    /// `AlterForeignServer`: the owner of the server can change its version and its options.
    pub(super) fn alter_server(&mut self, stmt: &AlterForeignServerStmt) -> Result<()> {
        let name = stmt.servername.as_deref().unwrap_or_default();
        let server = self.catalog.server_by_name(name).cloned().ok_or_else(|| no_server(name))?;
        if !self.is_superuser() && !self.owns(server.owner) {
            return Err(not_owner("foreign server", name));
        }
        let options = if !stmt.options.is_empty() {
            let validator = self.catalog.fdw(server.fdw).map_or(0, |f| f.validator);
            Some(self.generic_options(
                PG_FOREIGN_SERVER,
                &server.options,
                &stmt.options,
                validator,
            )?)
        } else {
            None
        };
        let version = stmt.version.as_deref().map(str::to_string);
        self.catalog.alter_server(server.oid, |s| {
            if stmt.has_version {
                s.version = version;
            }
            if let Some(options) = options {
                s.options = options;
            }
        });
        Ok(())
    }

    /// The foreign-data wrapper or the server of a name, which must exist, as an object with its owner and the words of its kind.
    fn foreign_object(&self, fdw: bool, name: &str) -> Result<(ObjRef, u32)> {
        if fdw {
            let found = self.catalog.fdw_by_name(name).ok_or_else(|| no_fdw(name))?;
            Ok((ObjRef::new(PG_FOREIGN_DATA_WRAPPER, found.oid), found.owner))
        } else {
            let found = self.catalog.server_by_name(name).ok_or_else(|| no_server(name))?;
            Ok((ObjRef::new(PG_FOREIGN_SERVER, found.oid), found.owner))
        }
    }

    /// The object of `COMMENT ON FOREIGN DATA WRAPPER` and `COMMENT ON SERVER`, after the check of its owner.
    pub(super) fn comment_foreign(&self, fdw: bool, object: Option<&Node>) -> Result<ObjRef> {
        let name = foreign_name(object);
        let (found, owner) = self.foreign_object(fdw, name)?;
        if !self.is_superuser() && !self.owns(owner) {
            let word = if fdw { "foreign-data wrapper" } else { "foreign server" };
            return Err(not_owner(word, name));
        }
        Ok(found)
    }

    /// `AlterObjectRename_internal` for a foreign-data wrapper or a server.
    pub(super) fn rename_foreign(&mut self, fdw: bool, stmt: &RenameStmt) -> Result<()> {
        let name = foreign_name(stmt.object.as_ref());
        let newname = stmt.newname.as_deref().unwrap_or_default();
        let (found, owner) = self.foreign_object(fdw, name)?;
        if !self.is_superuser() && !self.owns(owner) {
            let word = if fdw { "foreign-data wrapper" } else { "foreign server" };
            return Err(not_owner(word, name));
        }
        let taken = if fdw {
            self.catalog.fdw_by_name(newname).is_some()
        } else {
            self.catalog.server_by_name(newname).is_some()
        };
        if taken {
            let word = if fdw { "foreign-data wrapper" } else { "server" };
            return Err(Error::new(
                SqlState::DUPLICATE_OBJECT,
                format!("{word} \"{newname}\" already exists"),
            ));
        }
        let newname = newname.to_string();
        if fdw {
            self.catalog.alter_fdw(found.oid, |f| f.name = newname);
        } else {
            self.catalog.alter_server(found.oid, |s| s.name = newname);
        }
        Ok(())
    }

    /// `AlterForeignDataWrapperOwner_internal` and `AlterForeignServerOwner_internal`. Only a superuser can change the owner of a foreign-data wrapper, and the new owner must be a superuser. The owner of a server can give it to a role that it can become and that has `USAGE` on the wrapper.
    pub(super) fn alter_foreign_owner(&mut self, fdw: bool, stmt: &AlterOwnerStmt) -> Result<()> {
        let role = stmt.newowner.as_deref().cloned().unwrap_or_default();
        let name = foreign_name(stmt.object.as_ref());
        let (found, owner) = self.foreign_object(fdw, name)?;
        let new = self.role_oid(&role)?;
        let roles = rupg_pgcatalog::roles::current();
        if fdw {
            let denied = |hint: &str| {
                Error::new(
                    SqlState::INSUFFICIENT_PRIVILEGE,
                    format!("permission denied to change owner of foreign-data wrapper \"{name}\""),
                )
                .with_hint(hint.to_string())
            };
            if !self.is_superuser() {
                return Err(denied("Must be superuser to change owner of a foreign-data wrapper."));
            }
            if !roles.superuser(new) {
                return Err(denied("The owner of a foreign-data wrapper must be a superuser."));
            }
        }
        if owner == new {
            return Ok(());
        }
        if !fdw && !self.is_superuser() {
            if !self.owns(owner) {
                return Err(not_owner("foreign server", name));
            }
            self.check_can_set_role(new)?;
            let server = self.catalog.server(found.oid).map_or(0, |s| s.fdw);
            if let Some(wrapper) = self.catalog.fdw(server).cloned() {
                self.check_fdw_usage(&wrapper, new)?;
            }
        }
        let (old, new_name) = (
            roles.name(owner).unwrap_or_default().to_string(),
            roles.name(new).unwrap_or_default().to_string(),
        );
        self.catalog.set_object_owner(found, new, &old, &new_name)
    }

    /// `getObjectDescription` for a foreign-data wrapper or a server.
    pub(super) fn describe_foreign(&self, object: ObjRef) -> String {
        if object.class == PG_USER_MAPPING {
            let Some(mapping) = self.catalog.user_mapping(object.oid) else {
                return format!("user mapping {}", object.oid);
            };
            let server = self.catalog.server(mapping.server).map_or("", |s| s.name.as_str());
            return format!("user mapping for {} on server {server}", mapping_user(mapping.user));
        }
        if object.class == PG_FOREIGN_DATA_WRAPPER {
            return match self.catalog.fdw(object.oid) {
                Some(fdw) => format!("foreign-data wrapper {}", fdw.name),
                None => format!("foreign-data wrapper {}", object.oid),
            };
        }
        match self.catalog.server(object.oid) {
            Some(server) => format!("server {}", server.name),
            None => format!("server {}", object.oid),
        }
    }

    /// The role of a user mapping: 0 for `PUBLIC`, as `CreateUserMapping` takes it.
    fn mapping_role(&self, user: Option<&RoleSpec>) -> Result<u32> {
        let spec = user.cloned().unwrap_or_default();
        if spec.roletype == RoleSpecType::ROLESPEC_PUBLIC {
            return Ok(0);
        }
        self.role_oid(&spec)
    }

    /// `user_mapping_ddl_aclcheck`: the owner of the server may change any of its mappings, and a role with `USAGE` on the server may change its own mapping.
    fn user_mapping_check(&self, user: u32, server: &ForeignServer) -> Result<()> {
        if self.is_superuser() || self.owns(server.owner) {
            return Ok(());
        }
        if user != self.user {
            return Err(not_owner("foreign server", &server.name));
        }
        if self.foreign_usage_allowed(false, server.oid, self.user) {
            return Ok(());
        }
        Err(Error::new(
            SqlState::INSUFFICIENT_PRIVILEGE,
            format!("permission denied for foreign server {}", server.name),
        ))
    }

    /// `CreateUserMapping`. With `IF NOT EXISTS`, a mapping that exists gives a notice.
    pub(super) fn create_user_mapping(&mut self, stmt: &CreateUserMappingStmt) -> Result<()> {
        let user = self.mapping_role(stmt.user.as_deref())?;
        let name = stmt.servername.as_deref().unwrap_or_default();
        let server = self.catalog.server_by_name(name).cloned().ok_or_else(|| no_server(name))?;
        self.user_mapping_check(user, &server)?;
        if self.catalog.user_mapping_for(user, server.oid).is_some() {
            let message = format!(
                "user mapping for \"{}\" already exists for server \"{name}\"",
                mapping_user(user)
            );
            if stmt.if_not_exists {
                self.notice(SqlState::DUPLICATE_OBJECT, format!("{message}, skipping"));
                return Ok(());
            }
            return Err(Error::new(SqlState::DUPLICATE_OBJECT, message));
        }
        let validator = self.catalog.fdw(server.fdw).map_or(0, |f| f.validator);
        let options = self.generic_options(PG_USER_MAPPING, &[], &stmt.options, validator)?;
        self.catalog.create_user_mapping(UserMapping { oid: 0, user, server: server.oid, options });
        Ok(())
    }

    /// `AlterUserMapping`: the new options of a mapping.
    pub(super) fn alter_user_mapping(&mut self, stmt: &AlterUserMappingStmt) -> Result<()> {
        let user = self.mapping_role(stmt.user.as_deref())?;
        let name = stmt.servername.as_deref().unwrap_or_default();
        let server = self.catalog.server_by_name(name).cloned().ok_or_else(|| no_server(name))?;
        let Some(mapping) = self.catalog.user_mapping_for(user, server.oid).cloned() else {
            return Err(Error::new(
                SqlState::UNDEFINED_OBJECT,
                format!(
                    "user mapping for \"{}\" does not exist for server \"{name}\"",
                    mapping_user(user)
                ),
            ));
        };
        self.user_mapping_check(user, &server)?;
        if !stmt.options.is_empty() {
            let validator = self.catalog.fdw(server.fdw).map_or(0, |f| f.validator);
            let options =
                self.generic_options(PG_USER_MAPPING, &mapping.options, &stmt.options, validator)?;
            self.catalog.set_user_mapping_options(mapping.oid, options);
        }
        Ok(())
    }

    /// `RemoveUserMapping`. With `IF EXISTS`, a role, a server or a mapping that does not exist gives a notice.
    pub(super) fn drop_user_mapping(&mut self, stmt: &DropUserMappingStmt) -> Result<()> {
        let spec = stmt.user.as_deref().cloned().unwrap_or_default();
        let user = if spec.roletype == RoleSpecType::ROLESPEC_PUBLIC {
            0
        } else {
            match self.role_oid(&spec) {
                Ok(user) => user,
                Err(_) if stmt.missing_ok => {
                    let role = spec.rolename.as_deref().unwrap_or_default();
                    self.notice(
                        SqlState::SUCCESSFUL_COMPLETION,
                        format!("role \"{role}\" does not exist, skipping"),
                    );
                    return Ok(());
                }
                Err(error) => return Err(error),
            }
        };
        let name = stmt.servername.as_deref().unwrap_or_default();
        let Some(server) = self.catalog.server_by_name(name).cloned() else {
            if !stmt.missing_ok {
                return Err(no_server(name));
            }
            self.notice(
                SqlState::SUCCESSFUL_COMPLETION,
                format!("server \"{name}\" does not exist, skipping"),
            );
            return Ok(());
        };
        let Some(mapping) = self.catalog.user_mapping_for(user, server.oid).cloned() else {
            let message = format!(
                "user mapping for \"{}\" does not exist for server \"{name}\"",
                mapping_user(user)
            );
            if !stmt.missing_ok {
                return Err(Error::new(SqlState::UNDEFINED_OBJECT, message));
            }
            self.notice(SqlState::SUCCESSFUL_COMPLETION, format!("{message}, skipping"));
            return Ok(());
        };
        self.user_mapping_check(user, &server)?;
        self.perform(&[ObjRef::new(PG_USER_MAPPING, mapping.oid)], true)
    }
}
