//! `DROP ACCESS METHOD`, `DROP CAST`, `DROP COLLATION`, `DROP CONVERSION`, `DROP EXTENSION`, `DROP LANGUAGE`, `DROP FOREIGN DATA WRAPPER`, `DROP SERVER`, `DROP EVENT TRIGGER` and `DROP PUBLICATION`: the lookups of `get_object_address` and the notices of `does_not_exist_skipping`. rupg cannot make these objects, so a name finds a built-in object or nothing. A pinned object gives the error of `findDependentObjects`, and another built-in object gives `0A000`.

use rupg_catalog::{
    FIRST_NORMAL_OID, ObjRef, PG_CAST, PG_COLLATION, PG_EXTENSION, PG_LANGUAGE, is_pinned,
};
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin::{self, Named, Owned};
use rupg_sql::nodes::{DropBehavior, DropStmt, Node, ObjectType, TypeName};

use super::comment::not_owner;
use super::drop::{builtin_object, quote_identifier};
use super::{Definer, not_yet};
use crate::typename::{names, type_name_text};

/// `AccessMethodRelationId`.
const PG_AM: u32 = 2601;
/// `ConversionRelationId`.
const PG_CONVERSION: u32 = 2607;
/// `PG_UTF8`: the encoding of the databases of rupg.
const UTF8: i32 = 6;

/// An object that a name of `DROP` finds: its class, its OID and the words of `getObjectDescription`.
struct Removable {
    object: ObjRef,
    description: String,
}

impl Definer<'_, '_> {
    /// The objects of `RemoveObjects` for the kinds of this module. A missing object gives the error of `get_object_address`, or with `IF EXISTS` the notice of `does_not_exist_skipping`.
    pub(super) fn drop_named(&mut self, stmt: &DropStmt) -> Result<()> {
        let mut objects = Vec::new();
        for item in stmt.objects.iter().flatten() {
            let found = match stmt.removeType {
                ObjectType::OBJECT_CAST => self.find_cast(item, stmt.missing_ok)?,
                ObjectType::OBJECT_COLLATION | ObjectType::OBJECT_CONVERSION => {
                    self.find_in_schema(stmt.removeType, item, stmt.missing_ok)?
                }
                kind => self.find_by_name(kind, item, stmt.missing_ok)?,
            };
            let Some(found) = found else { continue };
            if found.object.oid >= FIRST_NORMAL_OID && found.object.class == PG_CAST {
                objects.push(found.object);
                continue;
            }
            if found.object.oid >= FIRST_NORMAL_OID {
                return Err(not_yet("DROP of this kind of object", None));
            }
            if !is_pinned(found.object) {
                return Err(builtin_object());
            }
            return Err(Error::new(
                SqlState::DEPENDENT_OBJECTS_STILL_EXIST,
                format!(
                    "cannot drop {} because it is required by the database system",
                    found.description
                ),
            ));
        }
        self.perform(&objects, stmt.behavior == DropBehavior::DROP_CASCADE)
    }

    /// The notice of `does_not_exist_skipping`.
    fn skip_missing(&mut self, message: &str) -> Option<Removable> {
        self.notice(SqlState::SUCCESSFUL_COMPLETION, format!("{message}, skipping"));
        None
    }

    /// `get_object_address_unqualified` for an object whose name has no schema, with the check of `check_object_ownership`. No role but a superuser owns a built-in object.
    fn find_by_name(
        &mut self,
        kind: ObjectType,
        item: &Node,
        missing_ok: bool,
    ) -> Result<Option<Removable>> {
        let name = match item {
            Node::String(name) => &**name,
            _ => return Err(Error::internal("the name of an object")),
        };
        let (word, found) = match kind {
            ObjectType::OBJECT_ACCESS_METHOD => {
                ("access method", builtin::access_method_oid(name).map(|oid| (PG_AM, oid)))
            }
            ObjectType::OBJECT_EXTENSION => (
                "extension",
                self.catalog.extensions().find(|e| e.name == name).map(|e| (PG_EXTENSION, e.oid)),
            ),
            ObjectType::OBJECT_LANGUAGE => {
                let user = self.catalog.languages().find(|l| l.name == name).map(|l| l.oid);
                let found = user.or_else(|| {
                    builtin::owned(Owned::Language).iter().find(|l| l.name == name).map(|l| l.oid)
                });
                ("language", found.map(|oid| (PG_LANGUAGE, oid)))
            }
            ObjectType::OBJECT_FDW => ("foreign-data wrapper", None),
            ObjectType::OBJECT_FOREIGN_SERVER => ("server", None),
            ObjectType::OBJECT_EVENT_TRIGGER => ("event trigger", None),
            ObjectType::OBJECT_PUBLICATION => ("publication", None),
            _ => return Err(not_yet("DROP of this kind of object", None)),
        };
        let Some((class, oid)) = found else {
            let message = format!("{word} \"{name}\" does not exist");
            if missing_ok {
                return Ok(self.skip_missing(&message));
            }
            return Err(Error::new(SqlState::UNDEFINED_OBJECT, message));
        };
        if !self.is_superuser() {
            if kind == ObjectType::OBJECT_ACCESS_METHOD {
                return Err(Error::new(SqlState::INSUFFICIENT_PRIVILEGE, "must be superuser"));
            }
            return Err(not_owner(word, name));
        }
        Ok(Some(Removable {
            object: ObjRef::new(class, oid),
            description: format!("{word} {name}"),
        }))
    }

    /// `get_collation_oid` and `get_conversion_oid`: the collation of the encoding of the database or of any encoding, or the conversion, in the schema of the name or in the first schema of the path that has one.
    fn find_in_schema(
        &mut self,
        kind: ObjectType,
        item: &Node,
        missing_ok: bool,
    ) -> Result<Option<Removable>> {
        let Node::List(list) = item else { return Err(Error::internal("a qualified name")) };
        let parts = names(list);
        let shown = parts.join(".");
        let (schema, name) = match self.an.split_name(&parts, None) {
            Err(error) if missing_ok && error.state() == SqlState::UNDEFINED_SCHEMA => {
                return Ok(self.skip_missing(error.message()));
            }
            split => split?,
        };
        let path = schema.map_or_else(|| self.an.path.clone(), |schema| vec![schema]);
        let collation = kind == ObjectType::OBJECT_COLLATION;
        let found = path.iter().find_map(|&namespace| {
            if collation {
                let user = self.catalog.collation_by_name(namespace, name).map(|c| c.oid);
                user.or_else(|| {
                    let rows = builtin::named(Named::Collation).iter();
                    let mut rows = rows.filter(|r| r.namespace == namespace && r.name == name);
                    rows.find(|r| r.encoding == UTF8 || r.encoding == -1).map(|r| r.oid)
                })
            } else {
                builtin::named(Named::Conversion)
                    .iter()
                    .find(|r| r.namespace == namespace && r.name == name)
                    .map(|r| r.oid)
            }
        });
        let word = if collation { "collation" } else { "conversion" };
        let Some(oid) = found else {
            if missing_ok {
                return Ok(self.skip_missing(&format!("{word} \"{shown}\" does not exist")));
            }
            let message = if collation {
                format!("collation \"{shown}\" for encoding \"UTF8\" does not exist")
            } else {
                format!("conversion \"{shown}\" does not exist")
            };
            return Err(Error::new(SqlState::UNDEFINED_OBJECT, message));
        };
        if !self.is_superuser() {
            return Err(not_owner(word, &shown));
        }
        let class = if collation { PG_COLLATION } else { PG_CONVERSION };
        let description = format!("{word} {}", quote_identifier(name));
        Ok(Some(Removable { object: ObjRef::new(class, oid), description }))
    }

    /// `get_object_address` for `OBJECT_CAST`: the cast from the first type to the second type, with the check of `check_object_ownership`, which needs the owner of one of the types.
    fn find_cast(&mut self, item: &Node, missing_ok: bool) -> Result<Option<Removable>> {
        let Node::List(list) = item else { return Err(Error::internal("the types of a cast")) };
        let types: Vec<&TypeName> = list
            .iter()
            .flatten()
            .filter_map(|n| match n {
                Node::TypeName(t) => Some(&**t),
                _ => None,
            })
            .collect();
        let [source, target] = types.as_slice() else {
            return Err(Error::internal("the two types of a cast"));
        };
        let mut oids = [0; 2];
        for (slot, ty) in oids.iter_mut().zip([source, target]) {
            let found = match self.an.lookup_type_name(ty) {
                Err(error) if missing_ok && error.state() == SqlState::UNDEFINED_SCHEMA => {
                    return Ok(self.skip_missing(error.message()));
                }
                found => found?,
            };
            let Some(oid) = found else {
                let message = format!("type \"{}\" does not exist", type_name_text(ty));
                if missing_ok {
                    return Ok(self.skip_missing(&message));
                }
                return Err(Error::new(SqlState::UNDEFINED_OBJECT, message));
            };
            *slot = oid;
        }
        let [from, to] = oids;
        let user = self.catalog.casts().find(|c| c.source == from && c.target == to);
        let found = user.map(|c| c.oid).or_else(|| builtin::cast(from, to).map(|c| c.oid));
        let Some(oid) = found else {
            if missing_ok {
                return Ok(self.skip_missing(&format!(
                    "cast from type {} to type {} does not exist",
                    type_name_text(source),
                    type_name_text(target)
                )));
            }
            return Err(Error::new(
                SqlState::UNDEFINED_OBJECT,
                format!(
                    "cast from type {} to type {} does not exist",
                    self.type_text(from),
                    self.type_text(to)
                ),
            ));
        };
        let (from, to) = (self.type_text(from), self.type_text(to));
        if !self.is_superuser() {
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("must be owner of type {from} or type {to}"),
            ));
        }
        Ok(Some(Removable {
            object: ObjRef::new(PG_CAST, oid),
            description: format!("cast from {from} to {to}"),
        }))
    }
}
