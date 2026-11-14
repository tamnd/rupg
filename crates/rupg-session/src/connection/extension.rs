//! `CREATE EXTENSION` and `ALTER EXTENSION ... UPDATE` of `extension.c` for the extensions that rupg builds in: the control files and the scripts come from [`rupg_contrib`], and the functions in C of a script bind to the kernels of the library, as spec 16.7 tells.

use rupg_catalog::{Catalog, Extension, ObjRef, PG_EXTENSION};
use rupg_common::{Error, SqlState};
use rupg_ext::{Builtin, Control, Macros};
use rupg_func::quote_identifier;
use rupg_pgcatalog::builtin::{self, Named};
use rupg_sql::Severity;
use rupg_sql::nodes::{AlterExtensionStmt, CreateExtensionStmt, CreateSchemaStmt, DefElem, Node};
use rupg_types::{Array, Value};
use rupg_wire::CommandTag;

use super::copy::def_string;
use super::roles::{has_privs_of_role, role_name};
use super::{BOOTSTRAP_SUPERUSER, Connection, Done, statements};
use crate::query;
use crate::utility::Notice;

/// The hooks that bind the functions in C of the scripts to the kernels of the built-in libraries: [`rupg_func::set_libraries`] for the calls, and [`rupg_analyze::set_libraries`] for the checks of `CREATE FUNCTION ... LANGUAGE C`.
pub(super) fn install_hooks() {
    rupg_func::set_libraries(rupg_contrib::library_symbol);
    rupg_analyze::set_libraries(|bin, symbol| {
        rupg_contrib::library_symbol(bin, symbol).map(|kernel| kernel.is_some())
    });
    rupg_func::set_available_extensions(available_extensions);
}

/// The values of `plpgsql.control`, the extension that the catalog of initdb has.
const PLPGSQL: (&str, &str, &str) = ("plpgsql", "1.0", "PL/pgSQL procedural language");

/// The name of the schema `oid`, a schema of initdb or of the catalog.
fn name_of_schema(catalog: &Catalog, oid: u32) -> Option<String> {
    if let Some(schema) = catalog.schema(oid) {
        return Some(schema.name.clone());
    }
    builtin::named_by_oid(Named::Namespace, oid)
        .filter(|_| !catalog.schema_dropped(oid))
        .map(|row| row.name.to_string())
}

/// A text, or null.
fn text_or_null(text: Option<&str>) -> Value {
    text.map_or(Value::Null, Value::text)
}

/// `convert_requires_to_datum`: the required extensions as a `name[]`, or null for none.
fn requires_value(requires: &[String]) -> Value {
    if requires.is_empty() {
        return Value::Null;
    }
    Value::Array(Box::new(Array::one(requires.iter().map(|r| Some(Value::text(r))).collect())))
}

/// The rows of `pg_available_extensions`, or of `pg_available_extension_versions` with `versions`: `plpgsql` and the extensions of [`rupg_contrib`], in the order of their names. An extension with a control file that does not parse has no row.
fn available_extensions(versions: bool) -> Vec<Vec<Value>> {
    let location = Value::text(rupg_ext::SYSTEM_LOCATION);
    let (name, version, comment) = PLPGSQL;
    let mut rows: Vec<Vec<Value>> = vec![if versions {
        vec![
            Value::text(name),
            Value::text(version),
            Value::Bool(true),
            Value::Bool(true),
            Value::Bool(false),
            Value::text("pg_catalog"),
            Value::Null,
            location.clone(),
            Value::text(comment),
        ]
    } else {
        vec![Value::text(name), Value::text(version), location.clone(), Value::text(comment)]
    }];
    let mut builtins: Vec<&Builtin> = rupg_contrib::builtins().iter().collect();
    builtins.sort_by_key(|b| b.name);
    for builtin in builtins {
        let Ok(pcontrol) = builtin.control() else { continue };
        if !versions {
            rows.push(vec![
                Value::text(builtin.name),
                text_or_null(pcontrol.default_version.as_deref()),
                location.clone(),
                text_or_null(pcontrol.comment.as_deref()),
            ]);
            continue;
        }
        // `get_available_versions_for_extension`.
        let graph = builtin.versions();
        for start in graph.installable() {
            let Ok(control) = builtin.secondary(&pcontrol, start) else { continue };
            let mut row = vec![
                Value::text(builtin.name),
                Value::text(start),
                Value::Bool(control.superuser),
                Value::Bool(control.trusted),
                Value::Bool(control.relocatable),
                text_or_null(control.schema.as_deref()),
                requires_value(&control.requires),
                location.clone(),
                text_or_null(control.comment.as_deref()),
            ];
            rows.push(row.clone());
            for indirect in graph.indirect() {
                if graph.install_path(indirect).is_none_or(|(from, _)| from != start) {
                    continue;
                }
                let Ok(control) = builtin.secondary(&pcontrol, indirect) else { continue };
                row[1] = Value::text(indirect);
                row[2] = Value::Bool(control.superuser);
                row[3] = Value::Bool(control.trusted);
                row[4] = Value::Bool(control.relocatable);
                row[6] = requires_value(&control.requires);
                rows.push(row.clone());
            }
        }
    }
    rows
}

/// `errorConflictingDefElem`.
fn conflicting(def: &DefElem) -> Error {
    let error = Error::new(SqlState::SYNTAX_ERROR, "conflicting or redundant options");
    match usize::try_from(def.location) {
        Ok(at) => error.with_position(Some(at)),
        Err(_) => error,
    }
}

/// The options of `CREATE EXTENSION` and `ALTER EXTENSION ... UPDATE`, each one at most once.
#[derive(Default)]
struct Options {
    schema: Option<String>,
    version: Option<String>,
    cascade: bool,
}

/// The extension that the built-in libraries have, or `0A000` "is not available".
fn builtin(name: &str) -> Result<&'static Builtin, Error> {
    rupg_contrib::find(name).ok_or_else(|| rupg_ext::not_available(name))
}

/// The version to install: the version of the statement, else the default version of the control file.
fn version_to_install(given: Option<String>, control: &Control) -> Result<String, Error> {
    let version = given.or_else(|| control.default_version.clone()).ok_or_else(|| {
        Error::new(SqlState::INVALID_PARAMETER_VALUE, "version to install must be specified")
    })?;
    rupg_ext::check_version_name(&version)?;
    Ok(version)
}

impl Connection {
    /// `CreateExtension`.
    ///
    /// # Errors
    ///
    /// The errors of `CreateExtension` and of the statements of the scripts.
    pub(super) fn create_extension(
        &self,
        stmt: &CreateExtensionStmt,
        notices: &mut Vec<Notice>,
    ) -> Result<Done, Error> {
        let name = stmt.extname.as_deref().unwrap_or_default();
        rupg_ext::check_extension_name(name)?;
        let done = Done::Tag(CommandTag::CreateExtension);
        if self.catalog().extensions().any(|e| e.name == name) {
            let message = format!("extension \"{name}\" already exists");
            if stmt.if_not_exists {
                notices.push(Notice {
                    severity: Severity::Notice,
                    error: Error::new(SqlState::DUPLICATE_OBJECT, format!("{message}, skipping")),
                });
                return Ok(done);
            }
            return Err(Error::new(SqlState::DUPLICATE_OBJECT, message));
        }
        if self.creating_extension.get() {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "nested CREATE EXTENSION is not supported",
            ));
        }
        let mut options = Options::default();
        let (mut schema_seen, mut version_seen, mut cascade_seen) = (false, false, false);
        for def in stmt.options.iter().flatten() {
            let Node::DefElem(def) = def else { continue };
            match def.defname.as_deref().unwrap_or_default() {
                "schema" => {
                    if schema_seen {
                        return Err(conflicting(def));
                    }
                    schema_seen = true;
                    options.schema = Some(def_string(def)?);
                }
                "new_version" => {
                    if version_seen {
                        return Err(conflicting(def));
                    }
                    version_seen = true;
                    options.version = Some(def_string(def)?);
                }
                "cascade" => {
                    if cascade_seen {
                        return Err(conflicting(def));
                    }
                    cascade_seen = true;
                    options.cascade = super::copy::def_boolean(def)?;
                }
                other => {
                    return Err(Error::internal(format!("unrecognized option: {other}")));
                }
            }
        }
        self.create_extension_internal(
            name,
            options.schema.as_deref(),
            options.version,
            options.cascade,
            &mut Vec::new(),
            true,
            notices,
        )?;
        Ok(done)
    }

    /// `CreateExtensionInternal`: the new extension `name`, and the extensions that it requires with `cascade`. `parents` are the extensions that `CASCADE` installs above this one, for the check of a cycle. It gives the OID of the extension.
    #[allow(clippy::too_many_arguments)]
    fn create_extension_internal(
        &self,
        name: &str,
        schema_name: Option<&str>,
        version: Option<String>,
        cascade: bool,
        parents: &mut Vec<String>,
        is_create: bool,
        notices: &mut Vec<Notice>,
    ) -> Result<u32, Error> {
        let builtin = builtin(name)?;
        let pcontrol = builtin.control()?;
        let version = version_to_install(version, &pcontrol)?;
        let versions = builtin.versions();
        let (start, updates) = versions.install_path(&version).ok_or_else(|| {
            Error::new(
                SqlState::INVALID_PARAMETER_VALUE,
                format!(
                    "extension \"{name}\" has no installation script nor update path for version \"{version}\""
                ),
            )
        })?;
        let control = builtin.secondary(&pcontrol, &start)?;
        // The schema of the objects.
        let catalog = self.catalog();
        let schema = match (schema_name, &control.schema) {
            (Some(given), Some(fixed)) if given != fixed && !cascade => {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    format!(
                        "extension \"{}\" must be installed in schema \"{fixed}\"",
                        control.name
                    ),
                ));
            }
            (_, Some(fixed)) => match rupg_analyze::Env::namespace(&self.reader(), fixed) {
                Some(oid) => oid,
                None => {
                    drop(catalog);
                    self.create_extension_schema(fixed, notices)?
                }
            },
            (Some(given), None) => {
                rupg_analyze::Env::namespace(&self.reader(), given).ok_or_else(|| {
                    Error::new(
                        SqlState::UNDEFINED_SCHEMA,
                        format!("schema \"{given}\" does not exist"),
                    )
                })?
            }
            (None, None) => {
                drop(catalog);
                let path = rupg_analyze::Env::search_path(&self.reader());
                *path.first().ok_or_else(|| {
                    Error::new(
                        SqlState::UNDEFINED_SCHEMA,
                        "no schema has been selected to create in",
                    )
                })?
            }
        };
        // The extensions that it requires.
        let mut required = Vec::new();
        for req in &control.requires {
            let oid = self.required_extension(
                req,
                name,
                schema_name,
                cascade,
                parents,
                is_create,
                notices,
            )?;
            required.push(oid);
        }
        let comment = control.comment.clone();
        let relocatable = control.relocatable;
        let owner = self.current_role();
        let mut oid = 0;
        self.change_catalog(&mut |c: &mut Catalog| {
            oid = c.create_extension(Extension {
                oid: 0,
                name: name.to_string(),
                owner,
                namespace: schema,
                relocatable,
                version: start.clone(),
            })?;
            c.set_required_extensions(oid, &required);
            if comment.is_some() {
                c.set_description(ObjRef::new(PG_EXTENSION, oid), comment.clone());
            }
            Ok(())
        })?;
        self.execute_extension_script(
            builtin, oid, &control, None, &start, &required, schema, notices,
        )?;
        self.apply_extension_updates(
            builtin,
            oid,
            &pcontrol,
            &start,
            &updates,
            schema_name,
            parents,
            cascade,
            is_create,
            notices,
        )?;
        Ok(oid)
    }

    /// The schema `name` of the control file of an extension, which `CreateExtensionInternal` makes when it does not exist, as `CreateSchemaCommand` makes it.
    fn create_extension_schema(&self, name: &str, notices: &mut Vec<Notice>) -> Result<u32, Error> {
        let stmt = Node::CreateSchemaStmt(Box::new(CreateSchemaStmt {
            schemaname: Some(name.into()),
            ..CreateSchemaStmt::default()
        }));
        self.define_with(
            &stmt,
            "(generated CREATE SCHEMA command)",
            notices,
            self.current_role(),
            true,
        )?;
        rupg_analyze::Env::namespace(&self.reader(), name)
            .ok_or_else(|| Error::internal(format!("cache lookup failed for schema \"{name}\"")))
    }

    /// `get_required_extension`: the OID of the extension `req` that the extension `name` requires, which `CASCADE` installs when it does not exist.
    #[allow(clippy::too_many_arguments)]
    fn required_extension(
        &self,
        req: &str,
        name: &str,
        schema_name: Option<&str>,
        cascade: bool,
        parents: &mut Vec<String>,
        is_create: bool,
        notices: &mut Vec<Notice>,
    ) -> Result<u32, Error> {
        if let Some(found) = self.catalog().extensions().find(|e| e.name == req) {
            return Ok(found.oid);
        }
        if !cascade {
            let error = Error::new(
                SqlState::UNDEFINED_OBJECT,
                format!("required extension \"{req}\" is not installed"),
            );
            return Err(if is_create {
                error.with_hint(
                    "Use CREATE EXTENSION ... CASCADE to install required extensions too.",
                )
            } else {
                error
            });
        }
        if parents.iter().any(|p| p == req) {
            return Err(Error::new(
                SqlState::INVALID_RECURSION,
                format!("cyclic dependency detected between extensions \"{req}\" and \"{name}\""),
            ));
        }
        notices.push(Notice {
            severity: Severity::Notice,
            error: Error::new(
                SqlState::SUCCESSFUL_COMPLETION,
                format!("installing required extension \"{req}\""),
            ),
        });
        parents.push(name.to_string());
        let result = self.create_extension_internal(
            req,
            schema_name,
            None,
            cascade,
            parents,
            is_create,
            notices,
        );
        parents.pop();
        result
    }

    /// `ApplyExtensionUpdates`: the update scripts of `updates` from the version `from`, each with the secondary control file of its version.
    #[allow(clippy::too_many_arguments)]
    fn apply_extension_updates(
        &self,
        builtin: &'static Builtin,
        extension: u32,
        pcontrol: &Control,
        from: &str,
        updates: &[String],
        orig_schema: Option<&str>,
        parents: &mut Vec<String>,
        cascade: bool,
        is_create: bool,
        notices: &mut Vec<Notice>,
    ) -> Result<(), Error> {
        let mut old = from.to_string();
        for version in updates {
            let control = builtin.secondary(pcontrol, version)?;
            let relocatable = control.relocatable;
            let catalog = self.catalog();
            let Some(row) = catalog.extension(extension) else {
                return Err(Error::internal(format!(
                    "could not find tuple for extension {extension}"
                )));
            };
            let (name, schema) = (row.name.clone(), row.namespace);
            drop(catalog);
            let mut required = Vec::new();
            for req in &control.requires {
                required.push(self.required_extension(
                    req,
                    &name,
                    orig_schema,
                    cascade,
                    parents,
                    is_create,
                    notices,
                )?);
            }
            let new = version.clone();
            self.change_catalog(&mut |c: &mut Catalog| {
                c.alter_extension(extension, |e| {
                    e.relocatable = relocatable;
                    e.version.clone_from(&new);
                });
                c.set_required_extensions(extension, &required);
                Ok(())
            })?;
            self.execute_extension_script(
                builtin,
                extension,
                &control,
                Some(&old),
                version,
                &required,
                schema,
                notices,
            )?;
            old.clone_from(version);
        }
        Ok(())
    }

    /// `execute_extension_script`: the script of `builtin` that installs `version`, or that updates `from` to `version`, with `schema` first in the search path. Each object that a statement of the script makes becomes a member of `extension`, as `recordDependencyOnCurrentExtension` records it.
    #[allow(clippy::too_many_arguments)]
    fn execute_extension_script(
        &self,
        builtin: &'static Builtin,
        extension: u32,
        control: &Control,
        from: Option<&str>,
        version: &str,
        required: &[u32],
        schema: u32,
        notices: &mut Vec<Notice>,
    ) -> Result<(), Error> {
        let me = self.current_role();
        let roles = self.roles();
        let mut definer = None;
        if control.superuser && !roles.superuser(me) {
            if control.trusted && self.database_create_allowed(me) {
                definer = Some((BOOTSTRAP_SUPERUSER, role_name(&roles, BOOTSTRAP_SUPERUSER)));
            } else {
                let what = if from.is_some() { "update" } else { "create" };
                let hint = if control.trusted {
                    format!(
                        "Must have CREATE privilege on current database to {what} this extension."
                    )
                } else {
                    format!("Must be superuser to {what} this extension.")
                };
                return Err(Error::new(
                    SqlState::INSUFFICIENT_PRIVILEGE,
                    format!("permission denied to {what} extension \"{}\"", control.name),
                )
                .with_hint(hint));
            }
        }
        let owner = role_name(&roles, me);
        drop(roles);
        let Some((_, text)) = builtin.script(from, version) else {
            let file = match from {
                Some(from) => format!("{}--{from}--{version}.sql", control.name),
                None => format!("{}--{version}.sql", control.name),
            };
            return Err(Error::new(
                SqlState::UNDEFINED_FILE,
                format!(
                    "could not open file \"{}\" for reading: No such file or directory",
                    Builtin::path(&file)
                ),
            ));
        };
        let catalog = self.catalog();
        let schema_name = name_of_schema(&catalog, schema)
            .ok_or_else(|| Error::internal(format!("cache lookup failed for schema {schema}")))?;
        let mut required_names = Vec::new();
        for &oid in required {
            let Some(row) = catalog.extension(oid) else { continue };
            let req_schema = name_of_schema(&catalog, row.namespace).unwrap_or_default();
            required_names.push((row.name.clone(), req_schema));
        }
        drop(catalog);
        let pairs: Vec<(&str, &str)> =
            required_names.iter().map(|(n, s)| (n.as_str(), s.as_str())).collect();
        let sql = rupg_ext::prepare(
            text,
            control,
            &Macros { owner: &owner, schema: &schema_name, required: &pairs },
        )?;
        // The search path: the schema of the extension, the schemas of the required extensions but `pg_catalog`, and `pg_temp` last.
        let mut path = vec![quote_identifier(&schema_name)];
        for (_, req_schema) in &required_names {
            if req_schema != "pg_catalog" && !path.contains(&quote_identifier(req_schema)) {
                path.push(quote_identifier(req_schema));
            }
        }
        path.push("pg_temp".into());
        let mut config = vec!["check_function_bodies=off".to_string()];
        let level = self.settings.borrow().get_of(crate::guc::known!("client_min_messages"));
        if matches!(
            level.as_deref(),
            Some(
                "debug5"
                    | "debug4"
                    | "debug3"
                    | "debug2"
                    | "debug1"
                    | "debug"
                    | "log"
                    | "notice"
                    | "info"
            )
        ) {
            config.push("client_min_messages=warning".into());
        }
        config.push(format!("search_path={}", path.join(", ")));
        let pushed = self.settings.borrow_mut().push_function(&config, definer);
        let was = self.creating_extension.replace(true);
        let result = pushed.and_then(|()| self.run_extension_sql(&sql, extension, notices));
        self.creating_extension.set(was);
        self.settings.borrow_mut().pop_function();
        result
    }

    /// `execute_sql_string`: each statement of the script `sql`, with the objects that it makes as members of `extension`. The notices go away, as `client_min_messages` is `warning` during the script.
    fn run_extension_sql(
        &self,
        sql: &str,
        extension: u32,
        notices: &mut Vec<Notice>,
    ) -> Result<(), Error> {
        let (list, _) = rupg_sql::parse(sql).map_err(Error::from)?;
        for (node, text) in statements(&list, sql) {
            let since = self.catalog().next_oid();
            let mut found = Vec::new();
            let result = if query::is_query(node) {
                let (mut late, mut described) = (Vec::new(), None);
                let result = self.select(node, Some(text), &mut found, &mut late, &mut described);
                found.extend(late);
                result.map(|_| ())
            } else if let Node::DoStmt(stmt) = node {
                let mut warnings = Vec::new();
                let result = self.run_do(stmt, text, &mut warnings, false);
                found.extend(warnings.into_iter().map(Notice::of_function));
                result
            } else {
                let mut messages = Vec::new();
                let result = self.call_utility(node, text, &mut messages);
                found.extend(messages.into_iter().map(Notice::of_function));
                result.map(|_| ())
            };
            notices.extend(found.into_iter().filter(|n| n.severity != Severity::Notice));
            result?;
            self.change_catalog(&mut |c: &mut Catalog| {
                c.add_new_members(extension, since);
                Ok(())
            })?;
        }
        Ok(())
    }

    /// `object_aclcheck(DatabaseRelationId, MyDatabaseId, role, ACL_CREATE)`.
    fn database_create_allowed(&self, role: u32) -> bool {
        let roles = self.roles();
        if roles.superuser(role) {
            return true;
        }
        let Some(row) = self.database_row(&self.database) else { return false };
        if has_privs_of_role(&roles, role, row.owner) {
            return true;
        }
        row.acl.as_deref().is_some_and(|acl| {
            rupg_analyze::acl_allows_create(acl, |grantee| has_privs_of_role(&roles, role, grantee))
        })
    }

    /// `ExecAlterExtensionStmt`: `ALTER EXTENSION ... UPDATE`.
    ///
    /// # Errors
    ///
    /// The errors of `ExecAlterExtensionStmt` and of the update scripts.
    pub(super) fn alter_extension(
        &self,
        stmt: &AlterExtensionStmt,
        notices: &mut Vec<Notice>,
    ) -> Result<Done, Error> {
        let done = Done::Tag(CommandTag::AlterExtension);
        let name = stmt.extname.as_deref().unwrap_or_default();
        if self.creating_extension.get() {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "nested ALTER EXTENSION is not supported",
            ));
        }
        let catalog = self.catalog();
        let Some(row) = catalog.extensions().find(|e| e.name == name) else {
            return Err(Error::new(
                SqlState::UNDEFINED_OBJECT,
                format!("extension \"{name}\" does not exist"),
            ));
        };
        let (oid, old, owner) = (row.oid, row.version.clone(), row.owner);
        drop(catalog);
        let mut given = None;
        for def in stmt.options.iter().flatten() {
            let Node::DefElem(def) = def else { continue };
            match def.defname.as_deref().unwrap_or_default() {
                "new_version" => {
                    if given.is_some() {
                        return Err(conflicting(def));
                    }
                    given = Some(def_string(def)?);
                }
                other => {
                    return Err(Error::internal(format!("unrecognized option: {other}")));
                }
            }
        }
        let me = self.current_role();
        if !has_privs_of_role(&self.roles(), me, owner) {
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("must be owner of extension {name}"),
            ));
        }
        let builtin = builtin(name)?;
        let pcontrol = builtin.control()?;
        let version = version_to_install(given, &pcontrol)?;
        if version == old {
            notices.push(Notice {
                severity: Severity::Notice,
                error: Error::new(
                    SqlState::SUCCESSFUL_COMPLETION,
                    format!("version \"{version}\" of extension \"{name}\" is already installed"),
                ),
            });
            return Ok(done);
        }
        let updates = builtin.versions().identify_update_path(name, &old, &version)?;
        self.apply_extension_updates(
            builtin,
            oid,
            &pcontrol,
            &old,
            &updates,
            None,
            &mut Vec::new(),
            false,
            false,
            notices,
        )?;
        Ok(done)
    }
}
