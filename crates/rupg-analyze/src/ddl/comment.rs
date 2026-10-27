//! `CommentObject`: `COMMENT ON`, as `comment.c` runs it, with the lookup of `get_object_address` and the checks of `check_object_ownership`. The objects are the schemas, the tables, the views, the indexes, the sequences, the columns, the constraints of the tables, the triggers, the types, the functions, the operators and the objects of text search of the catalog. A comment on a built-in object or on an object of another kind gives `0A000`.

use rupg_catalog::{
    FIRST_NORMAL_OID, ObjRef, PG_CLASS, PG_CONSTRAINT, PG_NAMESPACE, PG_PROC, PG_TYPE,
};
use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{CommentStmt, Node, ObjectType, RangeVar};

use super::drop_function::routine_word;
use super::index::system_column;
use super::{Definer, Found, not_yet, relkind_detail, relname};
use crate::typename::{names, type_name_text};

/// The error for a built-in object, which rupg cannot change.
fn builtin_object() -> Error {
    not_yet("COMMENT on a built-in object", None)
}

/// The names of a node that is a list of names.
pub(super) fn name_list(object: Option<&Node>) -> Vec<&str> {
    match object {
        Some(Node::List(list)) => names(list),
        _ => Vec::new(),
    }
}

/// `makeRangeVarFromNameList`.
pub(super) fn range_var(names: &[&str]) -> Result<RangeVar> {
    let text = |s: &str| Some(s.into());
    let (catalog, schema, name) = match names {
        [name] => (None, None, text(name)),
        [schema, name] => (None, text(schema), text(name)),
        [catalog, schema, name] => (text(catalog), text(schema), text(name)),
        _ => {
            return Err(Error::new(
                SqlState::SYNTAX_ERROR,
                format!("improper relation name (too many dotted names): {}", names.join(".")),
            ));
        }
    };
    Ok(RangeVar { catalogname: catalog, schemaname: schema, relname: name, ..RangeVar::default() })
}

/// The word of `aclcheck_error` with `ACLCHECK_NOT_OWNER` for a relation.
fn owner_word(objtype: ObjectType) -> &'static str {
    match objtype {
        ObjectType::OBJECT_INDEX => "index",
        ObjectType::OBJECT_SEQUENCE => "sequence",
        ObjectType::OBJECT_TABLE => "table",
        ObjectType::OBJECT_VIEW => "view",
        ObjectType::OBJECT_MATVIEW => "materialized view",
        _ => "relation",
    }
}

/// The error of `aclcheck_error` with `ACLCHECK_NOT_OWNER`.
pub(super) fn not_owner(word: &str, name: &str) -> Error {
    Error::new(SqlState::INSUFFICIENT_PRIVILEGE, format!("must be owner of {word} {name}"))
}

impl Definer<'_, '_> {
    /// `CommentObject`. A null or empty comment drops the comment of the object, as `CreateComments` does.
    pub(super) fn comment(&mut self, stmt: &CommentStmt) -> Result<()> {
        let object = stmt.object.as_ref();
        let target = match stmt.objtype {
            ObjectType::OBJECT_SCHEMA => self.comment_schema(object)?,
            ObjectType::OBJECT_TABLE
            | ObjectType::OBJECT_VIEW
            | ObjectType::OBJECT_MATVIEW
            | ObjectType::OBJECT_INDEX
            | ObjectType::OBJECT_SEQUENCE => self.comment_relation(stmt.objtype, object)?,
            ObjectType::OBJECT_COLUMN => self.comment_column(object)?,
            ObjectType::OBJECT_TABCONSTRAINT => self.comment_constraint(object)?,
            ObjectType::OBJECT_TRIGGER => self.comment_trigger(object)?,
            ObjectType::OBJECT_RULE => self.comment_rule(object)?,
            ObjectType::OBJECT_POLICY => self.comment_policy(object)?,
            ObjectType::OBJECT_TYPE | ObjectType::OBJECT_DOMAIN => {
                self.comment_type(stmt.objtype, object)?
            }
            ObjectType::OBJECT_TSTEMPLATE
            | ObjectType::OBJECT_TSPARSER
            | ObjectType::OBJECT_TSDICTIONARY
            | ObjectType::OBJECT_TSCONFIGURATION => self.comment_ts(stmt.objtype, object)?,
            ObjectType::OBJECT_OPERATOR => self.comment_operator(object)?,
            ObjectType::OBJECT_STATISTIC_EXT => self.comment_stats(object)?,
            ObjectType::OBJECT_CONVERSION => self.comment_conversion(object)?,
            ObjectType::OBJECT_FUNCTION
            | ObjectType::OBJECT_PROCEDURE
            | ObjectType::OBJECT_ROUTINE
            | ObjectType::OBJECT_AGGREGATE => self.comment_function(stmt.objtype, object)?,
            _ => return Err(not_yet("COMMENT on this kind of object", None)),
        };
        let text = stmt.comment.as_deref().filter(|c| !c.is_empty()).map(str::to_string);
        self.catalog.set_description(target, text);
        Ok(())
    }

    /// The OID of a relation of the catalog after the check of its owner.
    fn owned_relation(&self, found: Found, word: &str, name: &str) -> Result<u32> {
        let Found::User(oid) = found else { return Err(builtin_object()) };
        let owner = self.catalog.relation(oid).map(|r| r.owner);
        if !self.is_superuser() && owner != Some(self.user) {
            return Err(not_owner(word, name));
        }
        Ok(oid)
    }

    /// A function, by `LookupFuncWithArgs`.
    fn comment_function(&mut self, objtype: ObjectType, object: Option<&Node>) -> Result<ObjRef> {
        let Some(Node::ObjectWithArgs(item)) = object else {
            return Err(Error::internal("a function name with arguments"));
        };
        let found = self
            .find_function(objtype, item, false)?
            .ok_or_else(|| Error::internal("a function that the lookup did not find"))?;
        if !self.is_superuser() && found.owner != self.user {
            return Err(not_owner(routine_word(objtype), &names(&item.objname).join(".")));
        }
        if found.oid < FIRST_NORMAL_OID {
            return Err(builtin_object());
        }
        Ok(ObjRef::new(PG_PROC, found.oid))
    }

    /// A schema, by `get_namespace_oid`.
    fn comment_schema(&self, object: Option<&Node>) -> Result<ObjRef> {
        let name = match object {
            Some(Node::String(name)) => name.as_ref(),
            _ => "",
        };
        if let Some(schema) = self.catalog.schema_by_name(name) {
            if !self.is_superuser() && schema.owner != self.user {
                return Err(not_owner("schema", name));
            }
            return Ok(ObjRef::new(PG_NAMESPACE, schema.oid));
        }
        if self.builtin_schema(name).is_some() {
            return Err(builtin_object());
        }
        Err(Error::new(SqlState::UNDEFINED_SCHEMA, format!("schema \"{name}\" does not exist")))
    }

    /// A table, a view, a materialized view, an index or a sequence, by `get_relation_by_qualified_name`.
    fn comment_relation(&self, objtype: ObjectType, object: Option<&Node>) -> Result<ObjRef> {
        let rv = range_var(&name_list(object))?;
        let found = self.lookup_relation(&rv)?;
        let kind = self.kind_of(found);
        let (fits, what) = match objtype {
            ObjectType::OBJECT_INDEX => (matches!(kind, b'i' | b'I'), "an index"),
            ObjectType::OBJECT_SEQUENCE => (kind == b'S', "a sequence"),
            ObjectType::OBJECT_TABLE => (matches!(kind, b'r' | b'p'), "a table"),
            ObjectType::OBJECT_MATVIEW => (kind == b'm', "a materialized view"),
            _ => (kind == b'v', "a view"),
        };
        let name = relname(&rv);
        if !fits {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("\"{name}\" is not {what}"),
            ));
        }
        let oid = self.owned_relation(found, owner_word(objtype), name)?;
        Ok(ObjRef::new(PG_CLASS, oid))
    }

    /// A column, by `get_object_address_attribute`. A column of an index or a sequence has no comment, because its name can change between versions of PostgreSQL.
    fn comment_column(&self, object: Option<&Node>) -> Result<ObjRef> {
        let names = name_list(object);
        let Some((column, relation)) = names.split_last().filter(|(_, r)| !r.is_empty()) else {
            return Err(Error::new(SqlState::SYNTAX_ERROR, "column name must be qualified"));
        };
        let rv = range_var(relation)?;
        let found = self.lookup_relation(&rv)?;
        let Found::User(oid) = found else { return Err(builtin_object()) };
        if system_column(column).is_some() {
            return Err(not_yet("COMMENT on a system column", None));
        }
        let number = self.catalog.relation(oid).and_then(|r| r.column_number(column));
        let Some(number) = number else {
            return Err(Error::new(
                SqlState::UNDEFINED_COLUMN,
                format!(
                    "column \"{column}\" of relation \"{}\" does not exist",
                    relation.join(".")
                ),
            ));
        };
        let name = relname(&rv);
        self.owned_relation(found, "relation", name)?;
        let kind = self.kind_of(found);
        if !matches!(kind, b'r' | b'v' | b'm' | b'c' | b'f' | b'p') {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("cannot set comment on relation \"{name}\""),
            )
            .with_detail(relkind_detail(kind)));
        }
        Ok(ObjRef::column(oid, number))
    }

    /// A constraint of a table, by `get_object_address_relobject`. The table is open as `table_openrv` opens it, which refuses an index.
    fn comment_constraint(&self, object: Option<&Node>) -> Result<ObjRef> {
        let names = name_list(object);
        let Some((constraint, relation)) = names.split_last().filter(|(_, r)| !r.is_empty()) else {
            return Err(Error::new(
                SqlState::SYNTAX_ERROR,
                "must specify relation and object name",
            ));
        };
        let rv = range_var(relation)?;
        let found = self.lookup_relation(&rv)?;
        let name = relname(&rv);
        if matches!(self.kind_of(found), b'i' | b'I') {
            return Err(Error::new(SqlState::WRONG_OBJECT_TYPE, format!("\"{name}\" is an index")));
        }
        let Found::User(oid) = found else { return Err(builtin_object()) };
        let found = self.catalog.constraints_of(oid).find(|c| c.name == *constraint);
        let Some(found) = found.map(|c| c.oid) else {
            return Err(Error::new(
                SqlState::UNDEFINED_OBJECT,
                format!("constraint \"{constraint}\" for table \"{name}\" does not exist"),
            ));
        };
        self.owned_relation(Found::User(oid), "relation", name)?;
        Ok(ObjRef::new(PG_CONSTRAINT, found))
    }

    /// A type or a domain, by `get_object_address_type`.
    fn comment_type(&mut self, objtype: ObjectType, object: Option<&Node>) -> Result<ObjRef> {
        let Some(Node::TypeName(name)) = object else {
            return Err(not_yet("COMMENT on this kind of object", None));
        };
        let (oid, _) = self.an.type_name(name).map_err(|error| error.with_position(None))?;
        let ty = self.catalog.type_by_oid(oid);
        if objtype == ObjectType::OBJECT_DOMAIN && ty.is_none_or(|t| t.domain.is_none()) {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("\"{}\" is not a domain", type_name_text(name)),
            ));
        }
        let Some(ty) = ty else { return Err(builtin_object()) };
        if !self.is_superuser() && ty.owner != self.user {
            return Err(not_owner("type", &type_name_text(name)));
        }
        Ok(ObjRef::new(PG_TYPE, oid))
    }
}
