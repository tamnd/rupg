//! `ALTER FUNCTION` and `ALTER ROUTINE` for a function of the catalog: the options of `AlterFunction`, and `RENAME TO`, `SET SCHEMA` and `OWNER TO` as `AlterObjectRename_internal`, `AlterObjectNamespace_internal` and `AlterObjectOwner_internal` run them for a function. `LookupFuncWithArgs` finds the function. A built-in function gives `0A000`, because rupg cannot change it.

use rupg_catalog::{FIRST_NORMAL_OID, Function, ObjRef, PG_PROC};
use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{
    AlterFunctionStmt, AlterObjectSchemaStmt, AlterOwnerStmt, DefElem, Node, ObjectType,
    ObjectWithArgs,
};

use super::comment::not_owner;
use super::drop_function::Candidate;
use super::{Definer, not_yet};
use crate::typename::{names, place};

/// True for the kinds of objects of `ALTER FUNCTION` and `ALTER ROUTINE`, which [`super::define`] runs with `RENAME TO`, `SET SCHEMA` and `OWNER TO`.
pub(super) fn is_function_kind(kind: ObjectType) -> bool {
    matches!(
        kind,
        ObjectType::OBJECT_FUNCTION
            | ObjectType::OBJECT_PROCEDURE
            | ObjectType::OBJECT_ROUTINE
            | ObjectType::OBJECT_AGGREGATE
    )
}

/// The word of `aclcheck_error` with `ACLCHECK_NOT_OWNER` for a function.
fn owner_word(kind: ObjectType) -> &'static str {
    super::drop_function::routine_word(kind)
}

/// The function name with arguments of a node of `RenameStmt`, `AlterObjectSchemaStmt` or `AlterOwnerStmt`.
fn function_name(object: Option<&Node>) -> Result<&ObjectWithArgs> {
    match object {
        Some(Node::ObjectWithArgs(item)) => Ok(item),
        _ => Err(Error::internal("a function name with arguments")),
    }
}

/// `defGetNumeric` for `COST` and `ROWS`. rupg keeps the values as whole numbers, so a value with a fraction gives `0A000`.
fn whole_number(def: &DefElem, word: &str) -> Result<u32> {
    let value = match &def.arg {
        Some(Node::Integer(value)) => f64::from(*value),
        Some(Node::Float(text)) => text.parse().unwrap_or(f64::NAN),
        _ => {
            return Err(Error::new(
                SqlState::SYNTAX_ERROR,
                format!("{word} requires a numeric value"),
            ));
        }
    };
    if value.is_nan() || value <= 0.0 {
        return Err(Error::new(
            SqlState::INVALID_PARAMETER_VALUE,
            format!("{word} must be positive"),
        ));
    }
    if value.fract() != 0.0 || value > f64::from(u32::MAX) {
        return Err(not_yet(&format!("a {word} that is not a whole number"), None));
    }
    // The checks above keep the value in the range of `u32` with no fraction.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Ok(value as u32)
}

impl Definer<'_, '_> {
    /// The function of `LookupFuncWithArgs` that a statement changes.
    fn lookup_function(&mut self, kind: ObjectType, item: &ObjectWithArgs) -> Result<Candidate> {
        self.find_function(kind, item, false)?
            .ok_or_else(|| Error::internal("a function that the lookup did not find"))
    }

    /// The check of `object_ownercheck` on the function.
    fn check_function_owner(
        &self,
        kind: ObjectType,
        item: &ObjectWithArgs,
        found: &Candidate,
    ) -> Result<()> {
        if !self.is_superuser() && found.owner != self.user {
            return Err(not_owner(owner_word(kind), &names(&item.objname).join(".")));
        }
        Ok(())
    }

    /// The function that a statement changes, after the check of its owner.
    fn altered_function(&mut self, kind: ObjectType, item: &ObjectWithArgs) -> Result<Candidate> {
        let found = self.lookup_function(kind, item)?;
        self.check_function_owner(kind, item, &found)?;
        Ok(found)
    }

    /// The error of a change of a built-in function.
    fn check_user_function(found: &Candidate) -> Result<()> {
        if found.oid < FIRST_NORMAL_OID {
            return Err(not_yet("ALTER FUNCTION of a built-in function", None));
        }
        Ok(())
    }

    /// `AlterFunction`: the options of `ALTER FUNCTION`, as `compute_common_attribute` reads them. A second option of a kind gives `42601` at the place of the option.
    pub(super) fn alter_function(&mut self, stmt: &AlterFunctionStmt) -> Result<()> {
        let item = stmt.func.as_deref().ok_or_else(|| Error::internal("a function name"))?;
        let found = self.altered_function(stmt.objtype, item)?;
        if found.kind == b'a' {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("\"{}\" is an aggregate function", names(&item.objname).join(".")),
            ));
        }
        let mut seen: Vec<&str> = Vec::new();
        let mut defs: Vec<&DefElem> = Vec::new();
        for def in stmt.actions.iter().flatten() {
            let Node::DefElem(def) = def else { continue };
            let name = def.defname.as_deref().unwrap_or_default();
            if name != "set" {
                if seen.contains(&name) {
                    return Err(Error::new(
                        SqlState::SYNTAX_ERROR,
                        "conflicting or redundant options",
                    )
                    .with_position(place(def.location)));
                }
                seen.push(name);
            }
            defs.push(def);
        }
        Self::check_user_function(&found)?;
        let function = self.catalog.function(found.oid).ok_or_else(|| {
            Error::internal(format!("cache lookup failed for function {}", found.oid))
        })?;
        let mut new = function.clone();
        for def in defs {
            let name = def.defname.as_deref().unwrap_or_default();
            let text = match &def.arg {
                Some(Node::String(text)) => Some(&**text),
                _ => None,
            };
            match (name, text) {
                ("strict", _) => new.strict = matches!(def.arg, Some(Node::Boolean(true))),
                ("leakproof", _) => {
                    new.leakproof = matches!(def.arg, Some(Node::Boolean(true)));
                    if new.leakproof && !self.is_superuser() {
                        return Err(super::function::only_superuser_leakproof());
                    }
                }
                ("volatility", Some("immutable")) => new.volatile = b'i',
                ("volatility", Some("stable")) => new.volatile = b's',
                ("volatility", Some("volatile")) => new.volatile = b'v',
                ("parallel", Some("safe")) => new.parallel = b's',
                ("parallel", Some("restricted")) => new.parallel = b'r',
                ("parallel", Some("unsafe")) => new.parallel = b'u',
                ("parallel", _) => {
                    return Err(Error::new(
                        SqlState::INVALID_PARAMETER_VALUE,
                        "parameter \"parallel\" must be SAFE, RESTRICTED, or UNSAFE",
                    ));
                }
                ("cost", _) => new.cost = whole_number(def, "COST")?,
                ("rows", _) => {
                    new.rows = whole_number(def, "ROWS")?;
                    if !new.retset {
                        return Err(Error::new(
                            SqlState::INVALID_PARAMETER_VALUE,
                            "ROWS is not applicable when function does not return a set",
                        ));
                    }
                }
                _ => {
                    return Err(not_yet(
                        &format!("the option {name} of ALTER FUNCTION"),
                        place(def.location),
                    ));
                }
            }
        }
        self.catalog.alter_function(found.oid, |f| *f = new)
    }

    /// `AlterObjectRename_internal` for a function: the checks of the owner and of `CREATE` on the schema, the check of `IsThereFunctionInNamespace`, then the new name. The message of the owner check names a function for all the kinds, as `get_object_type` gives `OBJECT_FUNCTION` for `pg_proc`. The views that call the function get a new text.
    pub(super) fn rename_function(
        &mut self,
        kind: ObjectType,
        object: Option<&Node>,
        newname: &str,
    ) -> Result<()> {
        let item = function_name(object)?;
        let found = self.lookup_function(kind, item)?;
        self.check_function_owner(ObjectType::OBJECT_FUNCTION, item, &found)?;
        let function = self.user_function(&found)?;
        if !self.is_superuser() {
            self.check_create_in(function.namespace, None)?;
        }
        self.check_function_in(function.namespace, newname, &function.argtypes)?;
        self.check_not_called(found.oid, "RENAME")?;
        self.catalog.alter_function(found.oid, |f| f.name = newname.to_string())?;
        self.views_using(|object| object.class == PG_PROC && object.oid == found.oid)
    }

    /// `AlterObjectNamespace_internal` for a function: the checks of the new schema and of `IsThereFunctionInNamespace`, then the move. A function that is in the schema already needs no change.
    pub(super) fn alter_function_schema(&mut self, stmt: &AlterObjectSchemaStmt) -> Result<()> {
        let item = function_name(stmt.object.as_ref())?;
        let found = self.lookup_function(stmt.objectType, item)?;
        let schema = stmt.newschema.as_deref().unwrap_or_default();
        let namespace = self.an.schema(schema).ok_or_else(|| {
            Error::new(SqlState::UNDEFINED_SCHEMA, format!("schema \"{schema}\" does not exist"))
        })?;
        self.check_create_in(namespace, None)?;
        let function = self.user_function(&found)?;
        if function.namespace == namespace {
            return Ok(());
        }
        self.check_function_owner(ObjectType::OBJECT_FUNCTION, item, &found)?;
        self.check_function_in(namespace, &function.name, &function.argtypes)?;
        self.check_not_called(found.oid, "SET SCHEMA")?;
        self.catalog.alter_function(found.oid, |f| f.namespace = namespace)?;
        self.views_using(|object| object.class == PG_PROC && object.oid == found.oid)
    }

    /// `AlterObjectOwner_internal` for a function. A function that has the owner already needs no change.
    pub(super) fn alter_function_owner(&mut self, stmt: &AlterOwnerStmt) -> Result<()> {
        let role = stmt.newowner.as_deref().cloned().unwrap_or_default();
        let new = self.role_oid(&role)?;
        let item = function_name(stmt.object.as_ref())?;
        let found = self.lookup_function(stmt.objectType, item)?;
        if found.owner == new {
            return Ok(());
        }
        if !self.is_superuser() {
            self.check_function_owner(ObjectType::OBJECT_FUNCTION, item, &found)?;
            self.check_can_set_role(new)?;
            let namespace = self.catalog.function(found.oid).map_or(0, |f| f.namespace);
            if !self.may_create_in(namespace, new) {
                return Err(Error::new(
                    SqlState::INSUFFICIENT_PRIVILEGE,
                    format!("permission denied for schema {}", self.schema_name(namespace)),
                ));
            }
        }
        Self::check_user_function(&found)?;
        self.catalog.alter_function(found.oid, |f| f.owner = new)
    }

    /// The function of the catalog for a candidate, or `0A000` for a built-in function.
    fn user_function(&self, found: &Candidate) -> Result<Function> {
        Self::check_user_function(found)?;
        self.catalog.function(found.oid).cloned().ok_or_else(|| {
            Error::internal(format!("cache lookup failed for function {}", found.oid))
        })
    }

    /// `IsThereFunctionInNamespace`: `42723` when the schema has a function with the name and the types of the input arguments.
    fn check_function_in(&self, namespace: u32, name: &str, argtypes: &[u32]) -> Result<()> {
        if self.functions_in(namespace, name).iter().any(|c| c.argtypes == argtypes) {
            let args: Vec<String> = argtypes.iter().map(|&ty| self.type_text(ty)).collect();
            return Err(Error::new(
                SqlState::DUPLICATE_FUNCTION,
                format!(
                    "function {name}({}) already exists in schema \"{}\"",
                    args.join(", "),
                    self.schema_name(namespace)
                ),
            ));
        }
        Ok(())
    }

    /// The error for a function that the body of another function calls. The catalog keeps the text of a body, so a new name of the function would break the other function. An aggregate keeps the OIDs of its support functions, so it does not count.
    fn check_not_called(&self, oid: u32, what: &str) -> Result<()> {
        let me = ObjRef::new(PG_PROC, oid);
        if self.catalog.depends().iter().any(|d| {
            d.referenced == me
                && d.object.class == PG_PROC
                && self.catalog.function(d.object.oid).is_none_or(|f| f.kind != b'a')
        }) {
            return Err(not_yet(
                &format!("ALTER FUNCTION ... {what} of a function that another function calls"),
                None,
            ));
        }
        Ok(())
    }
}
