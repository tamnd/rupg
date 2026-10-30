//! `CREATE TYPE ... AS RANGE`, as `DefineRange` runs it.

use rupg_catalog::{NewRange, ObjRef, PG_TYPE, RangeInfo};
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin;
use rupg_sql::nodes::{CreateRangeStmt, DefElem, List, Node};
use rupg_types::oid;

use super::Definer;
use super::sequence::conflicting;
use super::tsearch::qualified_name;
use crate::typcache::{BTREE, default_opclass, is_binary_coercible};
use crate::typename::names;
use crate::types;

/// The longest name in bytes, `NAMEDATALEN - 1`.
const MAX_NAME: usize = 63;

/// The options of `DefineRange` by name.
#[derive(Default)]
struct Options<'n> {
    subtype: Option<&'n DefElem>,
    opclass: Option<&'n DefElem>,
    collation: Option<&'n DefElem>,
    canonical: Option<&'n DefElem>,
    subtype_diff: Option<&'n DefElem>,
    multirange: Option<&'n DefElem>,
}

/// The names of the option `collation` as a list of `String` nodes.
fn collation_names(def: &DefElem) -> List {
    match &def.arg {
        Some(Node::TypeName(name)) => name.names.clone(),
        Some(Node::List(list)) => list.clone(),
        Some(node) => vec![Some(node.clone())],
        None => Vec::new(),
    }
}

/// `makeMultirangeTypeName`: the name of the range type with `range` changed to `multirange`, or else with `_multirange` after it, clipped to the length of a name.
fn multirange_name(range: &str) -> String {
    let mut name = match range.find("range") {
        Some(at) => format!("{}multi{}", &range[..at], &range[at..]),
        None => format!("{}_multirange", clip(range, MAX_NAME + 1 - 12)),
    };
    name.truncate(clip(&name, MAX_NAME).len());
    name
}

/// The longest start of `text` that has at most `max` bytes and ends at the end of a character.
fn clip(text: &str, max: usize) -> &str {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

impl Definer<'_, '_> {
    /// `DefineRange`: the range type, its multirange type, their array types, the constructors and the cast from the range type to the multirange type.
    pub(super) fn create_range(&mut self, stmt: &CreateRangeStmt) -> Result<()> {
        let parts = names(&stmt.typeName);
        let (namespace, name) = self.ts_creation(&parts)?;
        self.check_create_in(namespace, None)?;
        self.check_type_name(namespace, name)?;
        let mut options = Options::default();
        for def in stmt.params.iter().flatten() {
            let Node::DefElem(def) = def else { continue };
            let defname = def.defname.as_deref().unwrap_or_default();
            let slot = match defname {
                "subtype" => &mut options.subtype,
                "subtype_opclass" => &mut options.opclass,
                "collation" => &mut options.collation,
                "canonical" => &mut options.canonical,
                "subtype_diff" => &mut options.subtype_diff,
                "multirange_type_name" => &mut options.multirange,
                _ => {
                    return Err(Error::new(
                        SqlState::SYNTAX_ERROR,
                        format!("type attribute \"{defname}\" not recognized"),
                    ));
                }
            };
            if slot.is_some() {
                return Err(conflicting(def));
            }
            *slot = Some(def);
            if defname == "subtype" {
                self.subtype(def)?;
            }
        }
        let subtype = match options.subtype {
            Some(def) => self.subtype(def)?,
            None => {
                return Err(Error::new(
                    SqlState::SYNTAX_ERROR,
                    "type attribute \"subtype\" is required",
                ));
            }
        };
        let row = types::row(subtype);
        if row.is_some_and(|t| t.kind == b'p') {
            return Err(Error::new(
                SqlState::DATATYPE_MISMATCH,
                format!("range subtype cannot be {}", types::name(subtype)),
            ));
        }
        self.check_type_usage(subtype)?;
        let subopc = self.range_opclass(options.opclass, subtype)?;
        let collation = match options.collation {
            Some(def) if types::collatable(subtype) => {
                self.an.collation_oid(&collation_names(def), None)?
            }
            None if types::collatable(subtype) => row.map_or(0, |t| t.collation),
            None => 0,
            Some(_) => {
                return Err(Error::new(
                    SqlState::WRONG_OBJECT_TYPE,
                    "range collation specified but subtype does not support collation",
                ));
            }
        };
        if options.canonical.is_some() {
            return Err(Error::new(
                SqlState::INVALID_OBJECT_DEFINITION,
                "cannot specify a canonical function without a pre-created shell type",
            )
            .with_hint("Create the type as a shell type, then create its canonicalization function, then do a full CREATE TYPE."));
        }
        let subdiff = match options.subtype_diff {
            Some(def) => self.subtype_diff(def, subtype)?,
            None => 0,
        };
        let align = if row.is_some_and(|t| t.align == b'd') { b'd' } else { b'i' };
        let (multirange_namespace, multirange_name) = match options.multirange {
            Some(def) => {
                let parts = qualified_name(def)?;
                // `LookupCreationNamespace` checks `CREATE` on the schema, and not `USAGE`.
                let (multirange_namespace, multirange) = match parts[..] {
                    [schema, multirange] => (self.lookup_creation_namespace(schema)?, multirange),
                    _ => {
                        let (schema, multirange) = self.an.split_name(&parts, None)?;
                        let namespace = match schema {
                            Some(namespace) => namespace,
                            None => self.first_schema()?,
                        };
                        self.check_create_in(namespace, None)?;
                        (namespace, multirange)
                    }
                };
                self.check_type_name(multirange_namespace, multirange)?;
                (multirange_namespace, multirange.to_string())
            }
            None => {
                let multirange = multirange_name(name);
                self.check_type_name(namespace, &multirange).map_err(|e| {
                    e.with_detail(format!(
                        "Failed while creating a multirange type for type \"{name}\"."
                    ))
                    .with_hint("You can manually specify a multirange type name using the \"multirange_type_name\" attribute.")
                })?;
                (namespace, multirange)
            }
        };
        let info = RangeInfo {
            subtype,
            collation,
            subopc,
            canonical: 0,
            subdiff,
            multirange: 0,
            align,
            constructors: [0; 5],
        };
        let new = NewRange {
            namespace,
            name: name.to_string(),
            owner: self.user,
            multirange_namespace,
            multirange_name,
            info,
        };
        let ty = self.catalog.create_range(new)?;
        self.collect(None, ObjRef::new(PG_TYPE, ty));
        self.apply_default_acl(ObjRef::new(PG_TYPE, ty))
    }

    /// The first schema of `search_path`, where a new object goes when its name has no schema.
    fn first_schema(&self) -> Result<u32> {
        self.an.env.search_path().first().copied().ok_or_else(|| {
            Error::new(SqlState::UNDEFINED_SCHEMA, "no schema has been selected to create in")
        })
    }

    /// `typenameTypeId` of the option `subtype`, with no place in the errors, as `DefineRange` gives no parse state.
    fn subtype(&mut self, def: &DefElem) -> Result<u32> {
        let mut name = match &def.arg {
            Some(Node::TypeName(name)) => (**name).clone(),
            _ => rupg_sql::nodes::TypeName { names: collation_names(def), ..Default::default() },
        };
        name.location = -1;
        Ok(self.an.type_name(&name)?.0)
    }

    /// `findRangeSubOpclass`: the `btree` operator class with the name, which must accept the subtype, or else the default `btree` operator class of the subtype.
    fn range_opclass(&self, def: Option<&DefElem>, subtype: u32) -> Result<u32> {
        let Some(def) = def else {
            return default_opclass(subtype, BTREE).map(|c| c.oid).ok_or_else(|| {
                Error::new(
                    SqlState::UNDEFINED_OBJECT,
                    format!(
                        "data type {} has no default operator class for access method \"btree\"",
                        types::name(subtype)
                    ),
                )
                .with_hint("You must specify an operator class for the range type or define a default operator class for the subtype.")
            });
        };
        let parts = qualified_name(def)?;
        let (schema, name) = self.an.split_name(&parts, None)?;
        let in_catalog = schema.is_none_or(|schema| schema == crate::PG_CATALOG_NAMESPACE);
        let class =
            builtin::opclasses().iter().find(|c| in_catalog && c.method == BTREE && c.name == name);
        let class = class.ok_or_else(|| {
            Error::new(
                SqlState::UNDEFINED_OBJECT,
                format!(
                    "operator class \"{}\" does not exist for access method \"btree\"",
                    parts.join(".")
                ),
            )
        })?;
        if !is_binary_coercible(subtype, class.intype) {
            return Err(Error::new(
                SqlState::DATATYPE_MISMATCH,
                format!(
                    "operator class \"{}\" does not accept data type {}",
                    parts.join("."),
                    types::name(subtype)
                ),
            ));
        }
        Ok(class.oid)
    }

    /// `findRangeSubtypeDiffFunction`: the function with the name and two arguments of the subtype, which must give `double precision` and be immutable.
    fn subtype_diff(&self, def: &DefElem, subtype: u32) -> Result<u32> {
        let parts = qualified_name(def)?;
        let (schema, name) = self.an.split_name(&parts, None)?;
        let path = schema.map_or_else(|| self.an.path.clone(), |schema| vec![schema]);
        let args = [subtype, subtype];
        let found = path.iter().find_map(|&namespace| {
            let user = self
                .catalog
                .functions()
                .find(|f| f.namespace == namespace && f.name == name && f.argtypes == args)
                .map(|f| (f.oid, f.rettype, f.volatile));
            user.or_else(|| {
                builtin::procs_named(name)
                    .find(|p| p.namespace == namespace && p.argtypes == args.as_slice())
                    .map(|p| (p.oid, p.rettype, p.volatile))
            })
        });
        let arg = types::name(subtype);
        let signature = format!("{}({arg}, {arg})", parts.join("."));
        let Some((function, rettype, volatile)) = found else {
            return Err(Error::new(
                SqlState::UNDEFINED_FUNCTION,
                format!("function {signature} does not exist"),
            ));
        };
        if rettype != oid::FLOAT8 {
            return Err(Error::new(
                SqlState::INVALID_OBJECT_DEFINITION,
                format!(
                    "range subtype diff function {signature} must return type double precision"
                ),
            ));
        }
        if volatile != b'i' {
            return Err(Error::new(
                SqlState::INVALID_OBJECT_DEFINITION,
                format!("range subtype diff function {signature} must be immutable"),
            ));
        }
        Ok(function)
    }
}

#[cfg(test)]
mod tests {
    use super::multirange_name;

    #[test]
    fn names() {
        assert_eq!(multirange_name("floatrange"), "floatmultirange");
        assert_eq!(multirange_name("int4r"), "int4r_multirange");
        assert_eq!(multirange_name(&"a".repeat(63)).len(), 63);
        assert_eq!(multirange_name(&"a".repeat(63)), format!("{}_multirange", "a".repeat(52)));
    }
}
