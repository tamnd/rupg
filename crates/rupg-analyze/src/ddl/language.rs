//! `CREATE LANGUAGE` with a handler, as `CreateProceduralLanguage` of `proclang.c` runs it, and `ALTER LANGUAGE` with `RENAME TO` and `OWNER TO`, as `AlterObjectRename_internal` and `AlterObjectOwner_internal` of `alter.c` run them. `CREATE LANGUAGE` with no handler is `CREATE EXTENSION` in the grammar.

use rupg_catalog::{Language, ObjRef, PG_LANGUAGE};
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin::{self, Owned};
use rupg_sql::nodes::{AlterOwnerStmt, CreateExtensionStmt, CreatePLangStmt, Node, RenameStmt};
use rupg_types::oid;

use super::comment::not_owner;
use super::{Definer, not_yet};
use crate::typename::names;

/// The error of a name that a language has.
fn taken(name: &str) -> Error {
    Error::new(SqlState::DUPLICATE_OBJECT, format!("language \"{name}\" already exists"))
}

/// The name of a language in the object of a statement.
fn object_name(object: Option<&Node>) -> &str {
    match object {
        Some(Node::String(name)) => name,
        _ => "",
    }
}

impl Definer<'_, '_> {
    /// `CreateProceduralLanguage`: only a superuser can make a language. The handler has no arguments and gives `language_handler`, the inline handler takes `internal` and the validator takes `oid`. With `OR REPLACE`, a language that exists gets the new handlers and keeps its OID, its owner and its ACL.
    pub(super) fn create_language(&mut self, stmt: &CreatePLangStmt) -> Result<()> {
        let name = stmt.plname.as_deref().unwrap_or_default();
        if !self.is_superuser() {
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                "must be superuser to create custom procedural language",
            ));
        }
        let handler_name = names(&stmt.plhandler);
        let (handler, rettype) = self.func_oid(&handler_name, &[])?;
        if rettype != oid::LANGUAGE_HANDLER {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("function {} must return type language_handler", handler_name.join(".")),
            ));
        }
        let inline = match names(&stmt.plinline) {
            list if list.is_empty() => 0,
            list => self.func_oid(&list, &[oid::INTERNAL])?.0,
        };
        let validator = match names(&stmt.plvalidator) {
            list if list.is_empty() => 0,
            list => self.func_oid(&list, &[oid::OID])?.0,
        };
        let new = Language {
            oid: 0,
            name: name.to_string(),
            owner: self.user,
            trusted: stmt.pltrusted,
            handler,
            inline,
            validator,
        };
        let existing = self.catalog.languages().find(|l| l.name == name).map(|l| l.oid);
        if let Some(oid) = existing {
            if !stmt.replace {
                return Err(taken(name));
            }
            return self.catalog.replace_language(oid, new);
        }
        if builtin::owned(Owned::Language).iter().any(|l| l.name == name) {
            if !stmt.replace {
                return Err(taken(name));
            }
            return Err(not_yet("CREATE OR REPLACE LANGUAGE for a built-in language", None));
        }
        self.catalog.create_language(new)?;
        Ok(())
    }

    /// The language of a name for `ALTER LANGUAGE`. A built-in language cannot change yet.
    fn language_row(&self, name: &str) -> Result<Language> {
        if let Some(found) = self.catalog.languages().find(|l| l.name == name) {
            return Ok(found.clone());
        }
        if builtin::owned(Owned::Language).iter().any(|l| l.name == name) {
            return Err(not_yet("ALTER LANGUAGE for a built-in language", None));
        }
        Err(Error::new(SqlState::UNDEFINED_OBJECT, format!("language \"{name}\" does not exist")))
    }

    /// `AlterObjectRename_internal` for a language: a user who is not a superuser must own the language, and the new name must be free.
    pub(super) fn rename_language(&mut self, stmt: &RenameStmt) -> Result<()> {
        let name = object_name(stmt.object.as_ref());
        let newname = stmt.newname.as_deref().unwrap_or_default();
        let found = self.language_row(name)?;
        if !self.is_superuser() && !self.owns(found.owner) {
            return Err(not_owner("language", name));
        }
        let builtin = builtin::owned(Owned::Language).iter().any(|l| l.name == newname);
        if builtin || self.catalog.languages().any(|l| l.name == newname) {
            return Err(taken(newname));
        }
        self.catalog.alter_language(found.oid, |l| l.name = newname.to_string());
        Ok(())
    }

    /// `AlterObjectOwner_internal` for a language: a user who is not a superuser must own the language and be able to become the new owner. The ACL gets `aclnewowner`.
    pub(super) fn alter_language_owner(&mut self, stmt: &AlterOwnerStmt) -> Result<()> {
        let role = stmt.newowner.as_deref().cloned().unwrap_or_default();
        let new = self.role_oid(&role)?;
        let name = object_name(stmt.object.as_ref());
        let found = self.language_row(name)?;
        if found.owner == new {
            return Ok(());
        }
        if !self.is_superuser() {
            if !self.owns(found.owner) {
                return Err(not_owner("language", name));
            }
            self.check_can_set_role(new)?;
        }
        let roles = rupg_pgcatalog::roles::current();
        let (old_name, new_name) = (
            roles.name(found.owner).unwrap_or_default().to_string(),
            roles.name(new).unwrap_or_default().to_string(),
        );
        self.catalog.set_object_owner(
            ObjRef::new(PG_LANGUAGE, found.oid),
            new,
            &old_name,
            &new_name,
        )
    }

    /// The object of `COMMENT ON LANGUAGE`, after the check of `pg_language_ownercheck`. The bootstrap superuser owns a built-in language.
    pub(super) fn comment_language(&self, object: Option<&Node>) -> Result<ObjRef> {
        let name = object_name(object);
        if let Some(found) = self.catalog.languages().find(|l| l.name == name) {
            if !self.is_superuser() && !self.owns(found.owner) {
                return Err(not_owner("language", name));
            }
            return Ok(ObjRef::new(PG_LANGUAGE, found.oid));
        }
        let Some(found) = builtin::owned(Owned::Language).iter().find(|l| l.name == name) else {
            return Err(Error::new(
                SqlState::UNDEFINED_OBJECT,
                format!("language \"{name}\" does not exist"),
            ));
        };
        if !self.is_superuser() {
            return Err(not_owner("language", name));
        }
        Ok(ObjRef::new(PG_LANGUAGE, found.oid))
    }

    /// `getObjectDescription` for a language.
    pub(super) fn describe_language(&self, oid: u32) -> String {
        let user = self.catalog.languages().find(|l| l.oid == oid).map(|l| l.name.as_str());
        let name = user.or_else(|| {
            builtin::owned(Owned::Language).iter().find(|l| l.oid == oid).map(|l| l.name)
        });
        match name {
            Some(name) => format!("language {name}"),
            None => format!("language {oid}"),
        }
    }

    /// `CreateExtension` for an extension that exists, which is also `CREATE LANGUAGE` with no handler: an error, or a notice with `IF NOT EXISTS`. The session does not load the script of an extension.
    pub(super) fn create_extension(&mut self, stmt: &CreateExtensionStmt) -> Result<()> {
        let name = stmt.extname.as_deref().unwrap_or_default();
        if !self.catalog.extensions().any(|e| e.name == name) {
            return Err(not_yet("CREATE EXTENSION", None));
        }
        let message = format!("extension \"{name}\" already exists");
        if stmt.if_not_exists {
            self.notice(SqlState::DUPLICATE_OBJECT, format!("{message}, skipping"));
            return Ok(());
        }
        Err(Error::new(SqlState::DUPLICATE_OBJECT, message))
    }
}
