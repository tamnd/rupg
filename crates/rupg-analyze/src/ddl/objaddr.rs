//! `get_object_address` with `missing_ok` false, as `pg_get_object_address` calls it: the catalog, the OID and the number of the column of an object that a name gives. The lookups are those of the statements that name objects, without the checks of the owner. The caller gives the objects that the session knows: the databases, the tablespaces, the roles, the schemas and the other objects that have one name.

use rupg_catalog::{
    Catalog, ObjRef, PG_AMOP, PG_AMPROC, PG_ATTRDEF, PG_CAST, PG_CLASS, PG_COLLATION,
    PG_CONSTRAINT, PG_CONVERSION, PG_DEFAULT_ACL, PG_OPERATOR, PG_POLICY, PG_PROC, PG_REWRITE,
    PG_STATISTIC_EXT, PG_TRIGGER, PG_TYPE,
};
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin::{self, Owned};
use rupg_sql::nodes::{Node, ObjectType, TypeName};

use super::comment::range_var;
use super::index::system_column;
use super::opclass::index_method;
use super::{Definer, Found, SchemaEnv};
use crate::typename::{names, type_name_text};
use crate::{Analyzer, Env, Params};

/// `get_object_address` with `missing_ok` false for the role `user`: the address of the object of the kind `objtype` that `object` names, in the form that `pg_get_object_address` gives it.
///
/// # Errors
///
/// An object that does not exist, a relation of the wrong kind, or a kind of object that the caller looks up.
pub fn object_address(
    objtype: ObjectType,
    object: &Node,
    env: &dyn Env,
    catalog: &Catalog,
    user: u32,
) -> Result<ObjRef> {
    let mut copy = catalog.clone();
    let schemas = SchemaEnv::new(env, &copy);
    let mut definer = Definer {
        an: Analyzer::new(&schemas, &Params::default()),
        catalog: &mut copy,
        user,
        messages: Vec::new(),
        scans: Vec::new(),
        repacked: Vec::new(),
        truncated: Vec::new(),
        transforms: Vec::new(),
        added: Vec::new(),
        views: Vec::new(),
        checks: Vec::new(),
        detached: Vec::new(),
        grantors: Vec::new(),
        databases: Vec::new(),
        schema: None,
        top_level: None,
        concurrent: None,
    };
    definer.object_address(objtype, object)
}

/// The strings of a list node.
fn strings(object: &Node) -> Vec<&str> {
    match object {
        Node::List(list) => names(list),
        _ => Vec::new(),
    }
}

/// The item `at` of a list node.
fn item(object: &Node, at: usize) -> Option<&Node> {
    match object {
        Node::List(list) => list.get(at).and_then(Option::as_ref),
        _ => None,
    }
}

/// The type name of a node.
fn type_name(node: Option<&Node>) -> Result<&TypeName> {
    match node {
        Some(Node::TypeName(name)) => Ok(name),
        _ => Err(Error::internal("a type name")),
    }
}

/// The string of a node.
fn string(node: Option<&Node>) -> Result<&str> {
    match node {
        Some(Node::String(s)) => Ok(s),
        _ => Err(Error::internal("a name")),
    }
}

impl Definer<'_, '_> {
    /// `get_object_address`.
    fn object_address(&mut self, objtype: ObjectType, object: &Node) -> Result<ObjRef> {
        match objtype {
            ObjectType::OBJECT_TABLE
            | ObjectType::OBJECT_SEQUENCE
            | ObjectType::OBJECT_VIEW
            | ObjectType::OBJECT_MATVIEW
            | ObjectType::OBJECT_INDEX
            | ObjectType::OBJECT_FOREIGN_TABLE => self.relation_address(objtype, object),
            ObjectType::OBJECT_COLUMN => self.attribute_address(object),
            ObjectType::OBJECT_DEFAULT => self.attrdef_address(object),
            ObjectType::OBJECT_RULE
            | ObjectType::OBJECT_TRIGGER
            | ObjectType::OBJECT_TABCONSTRAINT
            | ObjectType::OBJECT_POLICY => self.relobject_address(objtype, object),
            ObjectType::OBJECT_DOMCONSTRAINT => {
                let domain = self.type_address(true, type_name(item(object, 0))?)?;
                let name = string(item(object, 1))?;
                let found = self.catalog.constraints_of_domain(domain).find(|c| c.name == name);
                let Some(found) = found else {
                    return Err(Error::new(
                        SqlState::UNDEFINED_OBJECT,
                        format!(
                            "constraint \"{name}\" for domain {} does not exist",
                            self.type_text(domain)
                        ),
                    ));
                };
                Ok(ObjRef::new(PG_CONSTRAINT, found.oid))
            }
            ObjectType::OBJECT_TYPE | ObjectType::OBJECT_DOMAIN => {
                let oid = self
                    .type_address(objtype == ObjectType::OBJECT_DOMAIN, type_name(Some(object))?)?;
                Ok(ObjRef::new(PG_TYPE, oid))
            }
            ObjectType::OBJECT_AGGREGATE
            | ObjectType::OBJECT_FUNCTION
            | ObjectType::OBJECT_PROCEDURE
            | ObjectType::OBJECT_ROUTINE => {
                let Node::ObjectWithArgs(owa) = object else {
                    return Err(Error::internal("a function name with arguments"));
                };
                let found = self
                    .find_function(objtype, owa, false)?
                    .ok_or_else(|| Error::internal("a function that the lookup did not find"))?;
                Ok(ObjRef::new(PG_PROC, found.oid))
            }
            ObjectType::OBJECT_OPERATOR => {
                let Node::ObjectWithArgs(owa) = object else {
                    return Err(Error::internal("an operator name with arguments"));
                };
                let found = self
                    .lookup_oper_with_args(owa, false)?
                    .ok_or_else(|| Error::internal("an operator that the lookup did not find"))?;
                Ok(ObjRef::new(PG_OPERATOR, found.oid))
            }
            ObjectType::OBJECT_COLLATION => {
                Ok(ObjRef::new(PG_COLLATION, self.collation_oid(&strings(object))?))
            }
            ObjectType::OBJECT_CONVERSION => {
                let oid = self.conversion_lookup(&strings(object), false)?.unwrap_or_default();
                Ok(ObjRef::new(PG_CONVERSION, oid))
            }
            ObjectType::OBJECT_OPCLASS | ObjectType::OBJECT_OPFAMILY => {
                self.opcf_address(objtype == ObjectType::OBJECT_OPFAMILY, &strings(object))
            }
            ObjectType::OBJECT_AMOP | ObjectType::OBJECT_AMPROC => {
                self.opf_member_address(objtype == ObjectType::OBJECT_AMPROC, object)
            }
            ObjectType::OBJECT_CAST => {
                let source = self.type_oid(type_name(item(object, 0))?)?;
                let target = self.type_oid(type_name(item(object, 1))?)?;
                let user = self.catalog.casts().find(|c| c.source == source && c.target == target);
                let found =
                    user.map(|c| c.oid).or_else(|| builtin::cast(source, target).map(|c| c.oid));
                let Some(oid) = found else {
                    return Err(Error::new(
                        SqlState::UNDEFINED_OBJECT,
                        format!(
                            "cast from type {} to type {} does not exist",
                            self.type_text(source),
                            self.type_text(target)
                        ),
                    ));
                };
                Ok(ObjRef::new(PG_CAST, oid))
            }
            ObjectType::OBJECT_TRANSFORM => {
                let ty = self.type_oid(type_name(item(object, 0))?)?;
                let language = string(item(object, 1))?;
                if self.language_oid(language).is_none() {
                    return Err(Error::new(
                        SqlState::UNDEFINED_OBJECT,
                        format!("language \"{language}\" does not exist"),
                    ));
                }
                Err(Error::new(
                    SqlState::UNDEFINED_OBJECT,
                    format!(
                        "transform for type {} language \"{language}\" does not exist",
                        self.type_text(ty)
                    ),
                ))
            }
            ObjectType::OBJECT_TSPARSER
            | ObjectType::OBJECT_TSDICTIONARY
            | ObjectType::OBJECT_TSTEMPLATE
            | ObjectType::OBJECT_TSCONFIGURATION => self.ts_address(objtype, &strings(object)),
            ObjectType::OBJECT_STATISTIC_EXT => {
                Ok(ObjRef::new(PG_STATISTIC_EXT, self.stats_oid(&strings(object))?))
            }
            ObjectType::OBJECT_USER_MAPPING => {
                let user = string(item(object, 0))?;
                let server = string(item(object, 1))?;
                let roles = rupg_pgcatalog::roles::current();
                let missing = || {
                    Error::new(
                        SqlState::UNDEFINED_OBJECT,
                        format!(
                            "user mapping for user \"{user}\" on server \"{server}\" does not exist"
                        ),
                    )
                };
                let role =
                    if user == "public" { 0 } else { roles.oid(user).ok_or_else(missing)? };
                let found = self.catalog.server_by_name(server).ok_or_else(|| {
                    Error::new(
                        SqlState::UNDEFINED_OBJECT,
                        format!("server \"{server}\" does not exist"),
                    )
                })?;
                let mapping = self.catalog.user_mapping_for(role, found.oid).ok_or_else(missing)?;
                Ok(ObjRef::new(rupg_catalog::PG_USER_MAPPING, mapping.oid))
            }
            ObjectType::OBJECT_PUBLICATION_NAMESPACE => {
                let schema = string(item(object, 0))?;
                if self.namespace_oid(schema).is_none() {
                    return Err(Error::new(
                        SqlState::UNDEFINED_SCHEMA,
                        format!("schema \"{schema}\" does not exist"),
                    ));
                }
                let name = string(item(object, 1))?;
                let publication =
                    self.catalog.publication_by_name(name).ok_or_else(|| no_publication(name))?;
                // `get_object_address_publication_schema`.
                let schema_oid = self.namespace_oid(schema).unwrap_or_default();
                let row = self
                    .catalog
                    .publication_namespaces()
                    .find(|n| n.publication == publication.oid && n.schema == schema_oid)
                    .ok_or_else(|| {
                        Error::new(
                            SqlState::UNDEFINED_OBJECT,
                            format!(
                                "publication schema \"{schema}\" in publication \"{name}\" does not exist"
                            ),
                        )
                    })?;
                Ok(ObjRef::new(rupg_catalog::PG_PUBLICATION_NAMESPACE, row.oid))
            }
            ObjectType::OBJECT_PUBLICATION_REL | ObjectType::OBJECT_PUBLICATION_EXCLUDED_REL => {
                let Some(relation) = item(object, 0) else {
                    return Err(Error::internal("the name of a relation"));
                };
                let found = self.lookup_relation(&range_var(&strings(relation))?)?;
                let oid = found_oid(found);
                let name = string(item(object, 1))?;
                let publication =
                    self.catalog.publication_by_name(name).ok_or_else(|| no_publication(name))?;
                // `get_object_address_publication_rel`: the pair of the relation and the publication is a published relation or an excluded relation, and the kind must be the kind that the call asks for.
                let rel_name = self.catalog.relation(oid).map_or("", |r| r.name.as_str());
                let excluded = objtype == ObjectType::OBJECT_PUBLICATION_EXCLUDED_REL;
                let Some(row) = self.catalog.publication_rel_of(publication.oid, oid) else {
                    let what = if excluded {
                        "publication excluded relation"
                    } else {
                        "publication relation"
                    };
                    return Err(Error::new(
                        SqlState::UNDEFINED_OBJECT,
                        format!("{what} \"{rel_name}\" in publication \"{name}\" does not exist"),
                    ));
                };
                if excluded && !row.except {
                    return Err(Error::new(
                        SqlState::WRONG_OBJECT_TYPE,
                        format!(
                            "\"{rel_name}\" is not an excluded relation of publication \"{name}\""
                        ),
                    ));
                }
                if !excluded && row.except {
                    return Err(Error::new(
                        SqlState::WRONG_OBJECT_TYPE,
                        format!(
                            "\"{rel_name}\" is not a published relation of publication \"{name}\""
                        ),
                    ));
                }
                Ok(ObjRef::new(rupg_catalog::PG_PUBLICATION_REL, row.oid))
            }
            ObjectType::OBJECT_DEFACL => self.defacl_address(object),
            _ => Err(Error::internal(format!("unrecognized object type: {}", objtype.0))),
        }
    }

    /// `get_relation_by_qualified_name`.
    fn relation_address(&self, objtype: ObjectType, object: &Node) -> Result<ObjRef> {
        let rv = range_var(&strings(object))?;
        let found = self.lookup_relation(&rv)?;
        let kind = self.kind_of(found);
        let (fits, what) = match objtype {
            ObjectType::OBJECT_INDEX => (matches!(kind, b'i' | b'I'), "an index"),
            ObjectType::OBJECT_SEQUENCE => (kind == b'S', "a sequence"),
            ObjectType::OBJECT_TABLE => (matches!(kind, b'r' | b'p'), "a table"),
            ObjectType::OBJECT_VIEW => (kind == b'v', "a view"),
            ObjectType::OBJECT_MATVIEW => (kind == b'm', "a materialized view"),
            _ => (kind == b'f', "a foreign table"),
        };
        if !fits {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("\"{}\" is not {what}", self.found_name(found)),
            ));
        }
        Ok(ObjRef::new(PG_CLASS, found_oid(found)))
    }

    /// The name of a relation that a lookup found.
    pub(super) fn found_name(&self, found: Found) -> String {
        match found {
            Found::User(oid) => {
                self.catalog.relation(oid).map_or_else(String::new, |r| r.name.clone())
            }
            Found::Builtin(row) => row.name.to_string(),
        }
    }

    /// The relation and the column of `get_object_address_attribute` and `get_object_address_attrdef`.
    fn column_of<'n>(
        &self,
        object: &'n Node,
    ) -> Result<(Found, Vec<&'n str>, &'n str, Option<i16>)> {
        let names = strings(object);
        let Some((&column, relation)) = names.split_last().filter(|(_, r)| !r.is_empty()) else {
            return Err(Error::new(SqlState::SYNTAX_ERROR, "column name must be qualified"));
        };
        let found = self.lookup_relation(&range_var(relation)?)?;
        let number = match found {
            Found::User(oid) => self.catalog.relation(oid).and_then(|r| r.column_number(column)),
            Found::Builtin(row) => builtin::attributes(row.oid)
                .iter()
                .find(|a| !a.dropped && a.name == column)
                .map(|a| a.num),
        };
        let number = number.or_else(|| system_column(column).map(|(n, _)| n));
        Ok((found, relation.to_vec(), column, number))
    }

    /// `get_object_address_attribute`.
    fn attribute_address(&self, object: &Node) -> Result<ObjRef> {
        let (found, relation, column, number) = self.column_of(object)?;
        let Some(number) = number else {
            return Err(Error::new(
                SqlState::UNDEFINED_COLUMN,
                format!(
                    "column \"{column}\" of relation \"{}\" does not exist",
                    relation.join(".")
                ),
            ));
        };
        Ok(ObjRef::column(found_oid(found), number))
    }

    /// `get_object_address_attrdef`.
    fn attrdef_address(&self, object: &Node) -> Result<ObjRef> {
        let (found, relation, column, number) = self.column_of(object)?;
        let default = match (found, number) {
            (Found::User(oid), Some(number)) => self.catalog.default_of(oid, number).map(|d| d.oid),
            _ => None,
        };
        let Some(default) = default else {
            return Err(Error::new(
                SqlState::UNDEFINED_COLUMN,
                format!(
                    "default value for column \"{column}\" of relation \"{}\" does not exist",
                    relation.join(".")
                ),
            ));
        };
        Ok(ObjRef::new(PG_ATTRDEF, default))
    }

    /// `get_object_address_relobject`: a rule, a trigger, a constraint or a policy of a table, which `table_openrv` opens.
    fn relobject_address(&self, objtype: ObjectType, object: &Node) -> Result<ObjRef> {
        let names = strings(object);
        let Some((&name, relation)) = names.split_last().filter(|(_, r)| !r.is_empty()) else {
            return Err(Error::new(
                SqlState::SYNTAX_ERROR,
                "must specify relation and object name",
            ));
        };
        let found = self.lookup_relation(&range_var(relation)?)?;
        let table = self.found_name(found);
        match self.kind_of(found) {
            b'i' | b'I' => {
                return Err(Error::new(
                    SqlState::WRONG_OBJECT_TYPE,
                    format!("\"{table}\" is an index"),
                ));
            }
            b'c' => {
                return Err(Error::new(
                    SqlState::WRONG_OBJECT_TYPE,
                    format!("\"{table}\" is a composite type"),
                ));
            }
            _ => {}
        }
        let oid = match found {
            Found::User(oid) => Some(oid),
            Found::Builtin(_) => None,
        };
        let rel = oid.and_then(|oid| self.catalog.relation(oid));
        let (class, found, word) = match objtype {
            ObjectType::OBJECT_RULE => {
                let rule = match rel.and_then(|r| r.view.as_ref()) {
                    Some(view) if name == "_RETURN" => Some(view.rule),
                    _ => oid.and_then(|oid| self.catalog.rule_by_name(oid, name)).map(|r| r.oid),
                };
                (PG_REWRITE, rule, "rule")
            }
            ObjectType::OBJECT_TRIGGER => (
                PG_TRIGGER,
                oid.and_then(|oid| self.catalog.trigger_by_name(oid, name)).map(|t| t.oid),
                "trigger",
            ),
            ObjectType::OBJECT_TABCONSTRAINT => (
                PG_CONSTRAINT,
                oid.and_then(|oid| self.catalog.constraints_of(oid).find(|c| c.name == name))
                    .map(|c| c.oid),
                "constraint",
            ),
            _ => (
                PG_POLICY,
                oid.and_then(|oid| self.catalog.policy_by_name(oid, name)).map(|p| p.oid),
                "policy",
            ),
        };
        let Some(found) = found else {
            let message = if class == PG_REWRITE {
                format!("rule \"{name}\" for relation \"{table}\" does not exist")
            } else {
                format!("{word} \"{name}\" for table \"{table}\" does not exist")
            };
            return Err(Error::new(SqlState::UNDEFINED_OBJECT, message));
        };
        Ok(ObjRef::new(class, found))
    }

    /// `get_object_address_type`: the type, which must be a domain when `domain` is true.
    fn type_address(&self, domain: bool, name: &TypeName) -> Result<u32> {
        let oid = self.type_oid(name)?;
        if domain {
            let is_domain = match self.catalog.type_by_oid(oid) {
                Some(ty) => ty.domain.is_some(),
                None => builtin::type_by_oid(oid).is_some_and(|row| row.kind == b'd'),
            };
            if !is_domain {
                return Err(Error::new(
                    SqlState::WRONG_OBJECT_TYPE,
                    format!("\"{}\" is not a domain", type_name_text(name)),
                ));
            }
        }
        Ok(oid)
    }

    /// `LookupTypeNameOid` with `missing_ok` false.
    fn type_oid(&self, name: &TypeName) -> Result<u32> {
        self.an.lookup_type_name(name)?.ok_or_else(|| {
            Error::new(
                SqlState::UNDEFINED_OBJECT,
                format!("type \"{}\" does not exist", type_name_text(name)),
            )
        })
    }

    /// `get_collation_oid` with `missing_ok` false: the collation of the encoding of the database or of any encoding, in the schema of the name or in the first schema of the path that has one.
    fn collation_oid(&self, parts: &[&str]) -> Result<u32> {
        let (schema, name) = self.an.split_name(parts, None)?;
        let path = schema.map_or_else(|| self.an.path.clone(), |schema| vec![schema]);
        let found = path.iter().find_map(|&namespace| self.collation_in(namespace, name));
        found.ok_or_else(|| {
            Error::new(
                SqlState::UNDEFINED_OBJECT,
                format!("collation \"{}\" for encoding \"UTF8\" does not exist", parts.join(".")),
            )
        })
    }

    /// `get_object_address_opcf`: the class or the family of the access method that the first name gives.
    fn opcf_address(&self, family: bool, list: &[&str]) -> Result<ObjRef> {
        let Some((&method, name)) = list.split_first() else {
            return Err(Error::internal("the name of an access method"));
        };
        let method = index_method(method)?;
        let oid = self.opcf_lookup(family, method, name, false)?.unwrap_or_default();
        let class = if family { rupg_catalog::PG_OPFAMILY } else { rupg_catalog::PG_OPCLASS };
        Ok(ObjRef::new(class, oid))
    }

    /// `get_object_address_opf_member`: the operator or the function of the family with the number that the last name gives, for the two types.
    fn opf_member_address(&self, function: bool, object: &Node) -> Result<ObjRef> {
        let list = item(object, 0).map(strings).unwrap_or_default();
        let Some((&number, family)) = list.split_last() else {
            return Err(Error::internal("the name of an operator family"));
        };
        let number = i16::try_from(atoi(number)).unwrap_or_default();
        let family = self.opcf_address(true, family)?.oid;
        let mut written = Vec::new();
        let mut types = [0; 2];
        if let Some(Node::List(args)) = item(object, 1) {
            for (slot, arg) in types.iter_mut().zip(args.iter().flatten()) {
                let name = type_name(Some(arg))?;
                *slot = self.type_address(false, name)?;
                written.push(type_name_text(name));
            }
        }
        let [left, right] = types;
        let found = if function {
            let user = self.catalog.amprocs().find(|a| {
                a.family == family && a.left == left && a.right == right && a.number == number
            });
            user.map(|a| a.oid).or_else(|| {
                builtin::amprocs(family, left)
                    .iter()
                    .find(|a| a.right == right && a.number == number)
                    .map(|a| a.oid)
            })
        } else {
            let user = self.catalog.amops().find(|a| {
                a.family == family && a.left == left && a.right == right && a.strategy == number
            });
            user.map(|a| a.oid).or_else(|| {
                builtin::amops_of_family(family)
                    .find(|a| a.left == left && a.right == right && a.strategy == number)
                    .map(|a| a.oid)
            })
        };
        let Some(oid) = found else {
            let word = if function { "function" } else { "operator" };
            let text = |at: usize| written.get(at).map_or("???", String::as_str).to_string();
            return Err(Error::new(
                SqlState::UNDEFINED_OBJECT,
                format!(
                    "{word} {number} ({}, {}) of {} does not exist",
                    text(0),
                    text(1),
                    self.opfamily_description(family)
                ),
            ));
        };
        Ok(ObjRef::new(if function { PG_AMPROC } else { PG_AMOP }, oid))
    }

    /// `get_object_address_defacl`: the default privileges of the role, in the schema when the list has one, for the kind of objects that the first letter of the first item gives.
    fn defacl_address(&self, object: &Node) -> Result<ObjRef> {
        let list = strings(object);
        let (kind, user, schema) = match list.as_slice() {
            [kind, user] => (*kind, *user, None),
            [kind, user, schema, ..] => (*kind, *user, Some(*schema)),
            _ => return Err(Error::internal("the items of a default ACL")),
        };
        let code = kind.bytes().next().unwrap_or_default();
        let what = match code {
            b'r' => "tables",
            b'S' => "sequences",
            b'f' => "functions",
            b'T' => "types",
            b'n' => "schemas",
            b'L' => "large objects",
            _ => {
                return Err(Error::new(
                    SqlState::INVALID_PARAMETER_VALUE,
                    format!("unrecognized default ACL object type \"{}\"", char::from(code)),
                )
                .with_hint("Valid object types are \"r\", \"S\", \"f\", \"T\", \"n\", \"L\"."));
            }
        };
        let role = rupg_pgcatalog::roles::current().oid(user);
        let namespace = match schema {
            Some(schema) => self.namespace_oid(schema),
            None => Some(0),
        };
        let found = role.zip(namespace).and_then(|(role, namespace)| {
            self.catalog
                .default_acls()
                .find(|d| d.role == role && d.namespace == namespace && d.objtype == code)
                .map(|d| d.oid)
        });
        found.map(|oid| ObjRef::new(PG_DEFAULT_ACL, oid)).ok_or_else(|| {
            let message = match schema {
                Some(schema) => format!(
                    "default ACL for user \"{user}\" in schema \"{schema}\" on {what} does not exist"
                ),
                None => format!("default ACL for user \"{user}\" on {what} does not exist"),
            };
            Error::new(SqlState::UNDEFINED_OBJECT, message)
        })
    }

    /// `get_namespace_oid` with `missing_ok`.
    pub(super) fn namespace_oid(&self, name: &str) -> Option<u32> {
        self.catalog
            .schema_by_name(name)
            .map(|s| s.oid)
            .or_else(|| self.builtin_schema(name).map(|n| n.oid))
    }

    /// `get_language_oid` with `missing_ok`.
    fn language_oid(&self, name: &str) -> Option<u32> {
        let user = self.catalog.languages().find(|l| l.name == name).map(|l| l.oid);
        user.or_else(|| {
            builtin::owned(Owned::Language).iter().find(|l| l.name == name).map(|l| l.oid)
        })
    }
}

/// The OID of a relation that a lookup found.
pub(super) fn found_oid(found: Found) -> u32 {
    match found {
        Found::User(oid) => oid,
        Found::Builtin(row) => row.oid,
    }
}

/// The error of `GetPublicationByName` for a publication that does not exist. rupg has no publications.
fn no_publication(name: &str) -> Error {
    Error::new(SqlState::UNDEFINED_OBJECT, format!("publication \"{name}\" does not exist"))
}

/// `atoi`: the number at the start of the text after white space, or 0.
fn atoi(text: &str) -> i64 {
    let text = text.trim_start();
    let (sign, digits) = match text.as_bytes().first() {
        Some(b'-') => (-1, &text[1..]),
        Some(b'+') => (1, &text[1..]),
        _ => (1, text),
    };
    let end = digits.bytes().position(|b| !b.is_ascii_digit()).unwrap_or(digits.len());
    sign * digits[..end].parse::<i64>().unwrap_or_default()
}
