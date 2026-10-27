//! `CREATE CONVERSION`, as `CreateConversionCommand` and `ConversionCreate` run it, and `ALTER CONVERSION` with `RENAME TO`, `OWNER TO` and `SET SCHEMA`, as `AlterObjectRename_internal`, `AlterObjectOwner_internal` and `AlterObjectNamespace_internal` run them for a conversion.

use rupg_catalog::{Conversion, ObjRef, PG_CONVERSION};
use rupg_common::encoding::{char_to_encoding, encoding_to_char};
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin::{self, Named};
use rupg_sql::nodes::{
    AlterObjectSchemaStmt, AlterOwnerStmt, CreateConversionStmt, Node, RenameStmt,
};
use rupg_types::oid;

use super::comment::{name_list, not_owner};
use super::drop::quote_identifier;
use super::{Definer, PG_TOAST_NAMESPACE, not_yet};
use crate::typename::names;

/// `PG_SQL_ASCII`: the number of the encoding `SQL_ASCII`.
const SQL_ASCII: i32 = 0;

/// The error of `report_namespace_conflict` for a conversion.
fn conflict(name: &str, schema: &str) -> Error {
    Error::new(
        SqlState::DUPLICATE_OBJECT,
        format!("conversion \"{name}\" already exists in schema \"{schema}\""),
    )
}

fn invalid(message: String) -> Error {
    Error::new(SqlState::INVALID_OBJECT_DEFINITION, message)
}

impl Definer<'_, '_> {
    /// `CreateConversionCommand`: the checks of the schema, of the two encodings and of the function, then the conversion of `ConversionCreate`. rupg does not call the function with an empty string.
    pub(super) fn create_conversion(&mut self, stmt: &CreateConversionStmt) -> Result<()> {
        let list = names(&stmt.conversion_name);
        let (namespace, name) = self.ts_creation(&list)?;
        self.check_create_in(namespace, None)?;
        let from_name = stmt.for_encoding_name.as_deref().unwrap_or_default();
        let to_name = stmt.to_encoding_name.as_deref().unwrap_or_default();
        let from = char_to_encoding(from_name);
        if from < 0 {
            return Err(Error::new(
                SqlState::UNDEFINED_OBJECT,
                format!("source encoding \"{from_name}\" does not exist"),
            ));
        }
        let to = char_to_encoding(to_name);
        if to < 0 {
            return Err(Error::new(
                SqlState::UNDEFINED_OBJECT,
                format!("destination encoding \"{to_name}\" does not exist"),
            ));
        }
        if from == SQL_ASCII || to == SQL_ASCII {
            return Err(invalid(
                "encoding conversion to or from \"SQL_ASCII\" is not supported".to_string(),
            ));
        }
        let func_name = names(&stmt.func_name);
        let args = [oid::INT4, oid::INT4, oid::CSTRING, oid::INTERNAL, oid::INT4, oid::BOOL];
        let (proc, rettype) = self.func_oid(&func_name, &args)?;
        let shown = func_name.join(".");
        if rettype != oid::INT4 {
            return Err(invalid(format!(
                "encoding conversion function {shown} must return type integer"
            )));
        }
        self.check_function_execute(proc, &shown)?;
        if self.conversion_in(namespace, name).is_some() {
            return Err(Error::new(
                SqlState::DUPLICATE_OBJECT,
                format!("conversion \"{name}\" already exists"),
            ));
        }
        let default = self
            .catalog
            .conversions()
            .any(|c| c.namespace == namespace && c.from == from && c.to == to && c.default);
        if stmt.def && default {
            return Err(Error::new(
                SqlState::DUPLICATE_OBJECT,
                format!(
                    "default conversion for {} to {} already exists",
                    encoding_to_char(from.into()),
                    encoding_to_char(to.into())
                ),
            ));
        }
        let new = Conversion {
            oid: 0,
            name: name.to_string(),
            namespace,
            owner: self.user,
            from,
            to,
            proc,
            default: stmt.def,
        };
        self.catalog.create_conversion(new);
        Ok(())
    }

    /// The OID of the conversion with the name in the schema, of the catalog of the session or built in.
    pub(super) fn conversion_in(&self, namespace: u32, name: &str) -> Option<u32> {
        let user = self.catalog.conversion_by_name(namespace, name).map(|c| c.oid);
        user.or_else(|| {
            builtin::named(Named::Conversion)
                .iter()
                .find(|r| r.namespace == namespace && r.name == name)
                .map(|r| r.oid)
        })
    }

    /// `get_conversion_oid`: the conversion of a name with or without its schema. A missing conversion gives `None` with `missing_ok`, and an error without it.
    pub(super) fn conversion_lookup(&self, list: &[&str], missing_ok: bool) -> Result<Option<u32>> {
        let (schema, name) = match self.an.split_name(list, None) {
            Ok(split) => split,
            Err(error) if missing_ok && error.state() == SqlState::UNDEFINED_SCHEMA => {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let path = schema.map_or_else(|| self.an.path.clone(), |schema| vec![schema]);
        let found = path.iter().find_map(|&namespace| self.conversion_in(namespace, name));
        if found.is_none() && !missing_ok {
            return Err(Error::new(
                SqlState::UNDEFINED_OBJECT,
                format!("conversion \"{}\" does not exist", list.join(".")),
            ));
        }
        Ok(found)
    }

    /// The row of the conversion that a name gives, which must exist. Only a superuser passes the check of the owner of a built-in conversion, and rupg cannot change one.
    fn conversion_row(&self, object: Option<&Node>) -> Result<Conversion> {
        let list = name_list(object);
        let oid = self.conversion_lookup(&list, false)?.unwrap_or_default();
        match self.catalog.conversion(oid) {
            Some(conversion) => Ok(conversion.clone()),
            None if self.is_superuser() => Err(not_yet("ALTER of a built-in conversion", None)),
            None => Err(not_owner("conversion", list.last().copied().unwrap_or_default())),
        }
    }

    /// The object of `COMMENT ON CONVERSION`, after the check of its owner.
    pub(super) fn comment_conversion(&self, object: Option<&Node>) -> Result<ObjRef> {
        let list = name_list(object);
        let oid = self.conversion_lookup(&list, false)?.unwrap_or_default();
        let owner = self.catalog.conversion(oid).map(|c| c.owner);
        if !self.is_superuser() && !owner.is_some_and(|o| self.owns(o)) {
            return Err(not_owner("conversion", &list.join(".")));
        }
        Ok(ObjRef::new(PG_CONVERSION, oid))
    }

    /// `getObjectDescription` for a conversion: `conversion` and its name, after its schema when `ConversionIsVisible` is false.
    pub(super) fn describe_conversion(&self, oid: u32) -> String {
        let Some(conversion) = self.catalog.conversion(oid) else {
            return format!("conversion {oid}");
        };
        let name = quote_identifier(&conversion.name);
        let first = self.an.path.iter().copied().find(|&ns| {
            ns == conversion.namespace || self.conversion_in(ns, &conversion.name).is_some()
        });
        if first == Some(conversion.namespace) {
            return format!("conversion {name}");
        }
        format!("conversion {}.{name}", quote_identifier(&self.schema_name(conversion.namespace)))
    }

    /// `AlterObjectRename_internal` for `ALTER CONVERSION ... RENAME TO`. A user who is not a superuser must own the conversion and be able to create in its schema.
    pub(super) fn rename_conversion(&mut self, stmt: &RenameStmt) -> Result<()> {
        let newname = stmt.newname.as_deref().unwrap_or_default();
        let conversion = self.conversion_row(stmt.object.as_ref())?;
        if !self.is_superuser() {
            if !self.owns(conversion.owner) {
                return Err(not_owner("conversion", &conversion.name));
            }
            self.check_create_in(conversion.namespace, None)?;
        }
        if self.conversion_in(conversion.namespace, newname).is_some() {
            return Err(conflict(newname, &self.schema_name(conversion.namespace)));
        }
        self.catalog.alter_conversion(conversion.oid, |c| c.name = newname.to_string());
        Ok(())
    }

    /// `AlterObjectOwner_internal` for `ALTER CONVERSION ... OWNER TO`. A user who is not a superuser must own the conversion and be able to become the new owner, and the new owner must be able to create in the schema of the conversion.
    pub(super) fn alter_conversion_owner(&mut self, stmt: &AlterOwnerStmt) -> Result<()> {
        let role = stmt.newowner.as_deref().cloned().unwrap_or_default();
        let new = self.role_oid(&role)?;
        let conversion = self.conversion_row(stmt.object.as_ref())?;
        if conversion.owner == new {
            return Ok(());
        }
        if !self.is_superuser() {
            if !self.owns(conversion.owner) {
                return Err(not_owner("conversion", &conversion.name));
            }
            self.check_can_set_role(new)?;
            if !self.may_create_in(conversion.namespace, new) {
                return Err(Error::new(
                    SqlState::INSUFFICIENT_PRIVILEGE,
                    format!(
                        "permission denied for schema {}",
                        self.schema_name(conversion.namespace)
                    ),
                ));
            }
        }
        self.catalog.alter_conversion(conversion.oid, |c| c.owner = new);
        Ok(())
    }

    /// `ExecAlterObjectSchemaStmt` and `AlterObjectNamespace_internal` for `ALTER CONVERSION ... SET SCHEMA`: `LookupCreationNamespace` and `CheckSetNamespace` for the new schema, the checks of the owner and of the name, and then the move. A conversion that is in the schema already needs no change.
    pub(super) fn alter_conversion_schema(&mut self, stmt: &AlterObjectSchemaStmt) -> Result<()> {
        let conversion = self.conversion_row(stmt.object.as_ref())?;
        let schema = stmt.newschema.as_deref().unwrap_or_default();
        let namespace = self.an.schema(schema).ok_or_else(|| {
            Error::new(SqlState::UNDEFINED_SCHEMA, format!("schema \"{schema}\" does not exist"))
        })?;
        self.check_create_in(namespace, None)?;
        if conversion.namespace == namespace {
            return Ok(());
        }
        let temp = |ns: u32| self.catalog.is_temp_schema(ns);
        if temp(namespace) || temp(conversion.namespace) {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "cannot move objects into or out of temporary schemas",
            ));
        }
        if namespace == PG_TOAST_NAMESPACE || conversion.namespace == PG_TOAST_NAMESPACE {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "cannot move objects into or out of TOAST schema",
            ));
        }
        if !self.is_superuser() && !self.owns(conversion.owner) {
            return Err(not_owner("conversion", &conversion.name));
        }
        if self.conversion_in(namespace, &conversion.name).is_some() {
            return Err(conflict(&conversion.name, &self.schema_name(namespace)));
        }
        self.catalog.set_conversion_namespace(conversion.oid, namespace);
        Ok(())
    }
}
