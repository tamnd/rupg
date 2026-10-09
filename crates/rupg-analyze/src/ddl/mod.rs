//! The statements that define objects: `CREATE TABLE`, `CREATE INDEX`, `CREATE SCHEMA`, `CREATE VIEW`, `CREATE DOMAIN` and `CREATE FUNCTION`. They run as `transformCreateStmt`, `DefineRelation`, `DefineIndex`, `CreateSchemaCommand`, `DefineView`, `DefineDomain` and `CreateFunction` run them, and they change a [`Catalog`]. Only the setup of a new cluster runs `CREATE DOMAIN` and `CREATE FUNCTION`, for the domains and the functions of `information_schema`, so [`is_definition`] does not list them.

mod domain;
mod fkey;
mod function;
mod index;
mod table;
#[cfg(test)]
mod tests;
mod view;

pub use function::{Body, function_body};
pub(crate) use table::check_attribute_type;

use rupg_catalog::{Catalog, ObjRef, PG_CLASS, PG_TYPE, RelKind};
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin::{self, ClassRow, Named, Owned};
use rupg_sql::nodes::{CreateSchemaStmt, Node, RangeVar, RoleSpecType};
use rupg_types::{Value, oid};

use crate::coerce::AtOpt;
use crate::expr::{Expr, ExprKind};
use crate::typename::place;
use crate::{Analyzer, Env, PG_CATALOG_NAMESPACE, Params};

/// The OID of the schema `pg_toast`, where a user cannot create a relation.
const PG_TOAST_NAMESPACE: u32 = 99;

/// A message of a statement that defines an object. The client gets the messages before the result.
#[derive(Clone, Debug, PartialEq)]
pub enum Message {
    /// A `WARNING`, such as a precision that is too large.
    Warning(Error),
    /// A `NOTICE`, such as an object that `IF NOT EXISTS` skips.
    Notice(Error),
}

/// The result of [`define`].
#[derive(Debug)]
pub struct Defined {
    /// The messages in order. The client gets them also when the statement fails.
    pub messages: Vec<Message>,
    /// The result of the statement. After an error the caller discards the changes of the catalog, but the OIDs that the statement used stay used, as in PostgreSQL.
    pub result: Result<()>,
}

/// True for a statement that [`define`] runs.
pub fn is_definition(stmt: &Node) -> bool {
    matches!(
        stmt,
        Node::CreateStmt(_) | Node::IndexStmt(_) | Node::CreateSchemaStmt(_) | Node::ViewStmt(_)
    )
}

/// Runs a statement that defines an object, for the role `user`, and adds the object to the catalog. The statement is the `stmt` of a `RawStmt`, and `text` is its text, which the catalog keeps for a view and for a function.
pub fn define(stmt: &Node, text: &str, env: &dyn Env, catalog: &mut Catalog, user: u32) -> Defined {
    let schemas = SchemaEnv::new(env, catalog);
    let mut definer = Definer {
        an: Analyzer::new(&schemas, &Params::default()),
        catalog,
        user,
        messages: Vec::new(),
    };
    let result = match stmt {
        Node::CreateStmt(create) => definer.create_table(create),
        Node::IndexStmt(index) => definer.create_index(index),
        Node::CreateSchemaStmt(schema) => definer.create_schema(schema),
        Node::ViewStmt(view) => definer.create_view(view, text),
        Node::CreateDomainStmt(domain) => definer.create_domain(domain).map(|_| ()),
        Node::CreateFunctionStmt(function) => definer.create_function(function, text).map(|_| ()),
        _ => {
            Err(Error::new(SqlState::FEATURE_NOT_SUPPORTED, "this statement is not supported yet"))
        }
    };
    definer.flush();
    Defined { messages: definer.messages, result }
}

/// The session with the schemas of the catalog, which the analyzer finds by name.
struct SchemaEnv<'e> {
    env: &'e dyn Env,
    schemas: Vec<(String, u32)>,
}

impl<'e> SchemaEnv<'e> {
    fn new(env: &'e dyn Env, catalog: &Catalog) -> SchemaEnv<'e> {
        let schemas = catalog.schemas().map(|s| (s.name.clone(), s.oid)).collect();
        SchemaEnv { env, schemas }
    }
}

impl Env for SchemaEnv<'_> {
    fn search_path(&self) -> Vec<u32> {
        self.env.search_path()
    }

    fn namespace(&self, name: &str) -> Option<u32> {
        self.schemas
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, oid)| *oid)
            .or_else(|| self.env.namespace(name))
    }

    fn database(&self) -> String {
        self.env.database()
    }

    fn input(&self, ty: u32, text: &str, typmod: i32) -> Result<Value> {
        self.env.input(ty, text, typmod)
    }

    fn catalog(&self) -> Option<&Catalog> {
        self.env.catalog()
    }

    fn allow_system_table_mods(&self) -> bool {
        self.env.allow_system_table_mods()
    }
}

/// A relation that a name gives.
#[derive(Clone, Copy, Debug)]
enum Found {
    /// A relation of the catalog, with its OID.
    User(u32),
    /// A relation of the built-in catalog.
    Builtin(&'static ClassRow),
}

/// The state of one statement that defines an object.
pub(crate) struct Definer<'a, 'c> {
    an: Analyzer<'a>,
    catalog: &'c mut Catalog,
    user: u32,
    messages: Vec<Message>,
}

/// An error for a part of a statement that rupg does not support yet.
fn not_yet(what: &str, at: Option<usize>) -> Error {
    Error::new(SqlState::FEATURE_NOT_SUPPORTED, format!("{what} is not supported yet")).at_opt(at)
}

/// The name of a relation as the query wrote it, with the schema and the database.
fn qualified(rv: &RangeVar) -> String {
    [rv.catalogname.as_deref(), rv.schemaname.as_deref(), rv.relname.as_deref()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(".")
}

/// The name of the relation of a `RangeVar`.
fn relname(rv: &RangeVar) -> &str {
    rv.relname.as_deref().unwrap_or_default()
}

/// `contain_mutable_functions`: true when the expression calls a function that is not immutable.
fn is_mutable(expr: &Expr) -> bool {
    let mutable_proc = |oid: u32| builtin::proc_by_oid(oid).is_none_or(|p| p.volatile != b'i');
    expr.find(0, &mut |e, _| {
        let mutable = match &e.kind {
            ExprKind::Func(f) => mutable_proc(f.oid),
            ExprKind::NullIf { equal, .. } | ExprKind::Distinct { equal, .. } => {
                mutable_proc(*equal)
            }
            ExprKind::ScalarArrayOp { func, .. } => mutable_proc(*func),
            ExprKind::CoerceViaIo(arg, _) => {
                let output = builtin::type_by_oid(arg.ty).map_or(0, |t| t.output);
                let input = builtin::type_by_oid(e.ty).map_or(0, |t| t.input);
                mutable_proc(output) || mutable_proc(input)
            }
            ExprKind::SqlValue(_) => true,
            _ => false,
        };
        mutable.then_some(())
    })
    .is_some()
}

/// The attribute numbers of the columns of the relation that the expressions read, in the order of their first use, without duplicates.
fn column_numbers(exprs: &[&Expr]) -> Vec<i16> {
    let mut numbers = Vec::new();
    for expr in exprs {
        expr.find(0, &mut |e, depth| {
            if let ExprKind::Var(var) = e.kind
                && var.levels_up == depth
                && !numbers.contains(&var.attnum)
            {
                numbers.push(var.attnum);
            }
            None::<()>
        });
    }
    numbers
}

/// `recordDependencyOnSingleRelExpr`: the objects that the expressions refer to. These are the columns of the relation and the relations and types of `regclass` and `regtype` constants. The catalog drops the references to pinned objects.
fn references(relation: u32, exprs: &[&Expr]) -> Vec<ObjRef> {
    let mut refs: Vec<ObjRef> =
        column_numbers(exprs).into_iter().map(|n| ObjRef::column(relation, n)).collect();
    for expr in exprs {
        expr.find(0, &mut |e, _| {
            let found = match (&e.kind, e.ty) {
                (ExprKind::Const(Value::Oid(oid)), oid::REGCLASS) => {
                    Some(ObjRef::new(PG_CLASS, *oid))
                }
                (ExprKind::Const(Value::Oid(oid)), oid::REGTYPE) => {
                    Some(ObjRef::new(PG_TYPE, *oid))
                }
                _ => None,
            };
            for found in found.into_iter().chain(type_reference(e)) {
                if !refs.contains(&found) {
                    refs.push(found);
                }
            }
            None::<()>
        });
    }
    refs
}

/// The type that `find_expr_references_walker` records for a constant or a cast, which is the type of its result. The catalog drops the references to the built-in types, so only a type such as a domain of `initdb` stays.
pub(crate) fn type_reference(expr: &Expr) -> Option<ObjRef> {
    match expr.kind {
        ExprKind::Const(_)
        | ExprKind::Relabel(..)
        | ExprKind::CoerceViaIo(..)
        | ExprKind::ArrayCoerce { .. }
        | ExprKind::CoerceToDomain(..) => Some(ObjRef::new(PG_TYPE, expr.ty)),
        _ => None,
    }
}

impl Definer<'_, '_> {
    /// Moves the warnings of the analyzer to the messages.
    fn flush(&mut self) {
        self.messages.extend(self.an.notices.drain(..).map(Message::Warning));
    }

    /// Adds a `NOTICE` after the warnings that came before it.
    fn notice(&mut self, state: SqlState, message: String) {
        self.flush();
        self.messages.push(Message::Notice(Error::new(state, message)));
    }

    /// True when the role of the statement is a superuser.
    fn is_superuser(&self) -> bool {
        builtin::roles().iter().any(|r| r.oid == self.user && r.superuser)
    }

    /// The owner of a schema, or `None` for a schema that does not exist.
    fn schema_owner(&self, namespace: u32) -> Option<u32> {
        match self.catalog.schema(namespace) {
            Some(schema) => Some(schema.owner),
            None => builtin::owned_by_oid(Owned::Namespace, namespace).map(|n| n.owner),
        }
    }

    /// The name of a schema.
    fn schema_name(&self, namespace: u32) -> String {
        match self.catalog.schema(namespace) {
            Some(schema) => schema.name.clone(),
            None => builtin::named_by_oid(Named::Namespace, namespace)
                .map_or_else(String::new, |n| n.name.to_string()),
        }
    }

    /// `object_aclcheck` of `CREATE` on a schema: a superuser or the owner of the schema can create objects in it.
    fn check_create_in(&self, namespace: u32, at: Option<usize>) -> Result<()> {
        if self.is_superuser() || self.schema_owner(namespace) == Some(self.user) {
            return Ok(());
        }
        Err(Error::new(
            SqlState::INSUFFICIENT_PRIVILEGE,
            format!("permission denied for schema {}", self.schema_name(namespace)),
        )
        .at_opt(at))
    }

    /// `RangeVarGetAndCheckCreationNamespace`: the schema where a new relation goes.
    fn creation_namespace(&self, rv: &RangeVar) -> Result<u32> {
        let at = place(rv.location);
        if let Some(catalog) = rv.catalogname.as_deref()
            && catalog != self.an.env.database()
        {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!("cross-database references are not implemented: \"{}\"", qualified(rv)),
            )
            .at_opt(at));
        }
        if rv.relpersistence == b't' {
            return Err(not_yet("a temporary table", at));
        }
        if rv.relpersistence == b'u' {
            return Err(not_yet("an unlogged table", at));
        }
        let namespace = match rv.schemaname.as_deref() {
            Some(schema) => self.an.schema(schema).ok_or_else(|| {
                Error::new(
                    SqlState::UNDEFINED_SCHEMA,
                    format!("schema \"{schema}\" does not exist"),
                )
                .at_opt(at)
            })?,
            None => self.an.env.search_path().first().copied().ok_or_else(|| {
                Error::new(SqlState::UNDEFINED_SCHEMA, "no schema has been selected to create in")
                    .at_opt(at)
            })?,
        };
        self.check_create_in(namespace, at)?;
        Ok(namespace)
    }

    /// A relation with this name in the schema, of the catalog or built in.
    fn relation_in(&self, namespace: u32, name: &str) -> Option<Found> {
        if let Some(rel) = self.catalog.relation_by_name(namespace, name) {
            return Some(Found::User(rel.oid));
        }
        builtin::named(Named::Class)
            .iter()
            .find(|r| r.namespace == namespace && r.name == name)
            .and_then(|r| builtin::class_by_oid(r.oid))
            .map(Found::Builtin)
    }

    /// `RangeVarGetRelid`: the relation that a name gives. The errors have no position, as in PostgreSQL.
    fn lookup_relation(&self, rv: &RangeVar) -> Result<Found> {
        let name = relname(rv);
        if let Some(catalog) = rv.catalogname.as_deref()
            && catalog != self.an.env.database()
        {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!("cross-database references are not implemented: \"{}\"", qualified(rv)),
            ));
        }
        let found = match rv.schemaname.as_deref() {
            Some(schema) => {
                let namespace = self.an.schema(schema).ok_or_else(|| {
                    Error::new(
                        SqlState::UNDEFINED_SCHEMA,
                        format!("schema \"{schema}\" does not exist"),
                    )
                })?;
                self.relation_in(namespace, name)
            }
            None => self.an.path.iter().find_map(|&ns| self.relation_in(ns, name)),
        };
        found.ok_or_else(|| {
            let shown = match rv.schemaname.as_deref() {
                Some(schema) => format!("{schema}.{name}"),
                None => name.to_string(),
            };
            Error::new(SqlState::UNDEFINED_TABLE, format!("relation \"{shown}\" does not exist"))
        })
    }

    /// `IsSystemRelation`: true for a relation of the catalog and for a TOAST table or its index.
    fn is_system(&self, found: Found) -> bool {
        match found {
            Found::Builtin(row) => row.oid < rupg_catalog::FIRST_UNPINNED_OID,
            Found::User(oid) => {
                self.catalog.relation(oid).is_some_and(|r| r.namespace == PG_TOAST_NAMESPACE)
            }
        }
    }

    /// The kind of a relation as a `relkind` code.
    fn kind_of(&self, found: Found) -> u8 {
        match found {
            Found::User(oid) => match self.catalog.relation(oid).map(|r| r.kind) {
                Some(RelKind::Index) => b'i',
                Some(RelKind::Sequence) => b'S',
                Some(RelKind::Toast) => b't',
                Some(RelKind::View) => b'v',
                _ => b'r',
            },
            Found::Builtin(row) => row.kind,
        }
    }

    /// `CreateSchemaCommand`.
    fn create_schema(&mut self, stmt: &CreateSchemaStmt) -> Result<()> {
        let owner = match &stmt.authrole {
            None => self.user,
            Some(role) => match role.roletype {
                RoleSpecType::ROLESPEC_CSTRING => {
                    let name = role.rolename.as_deref().unwrap_or_default();
                    builtin::roles().iter().find(|r| r.name == name).map(|r| r.oid).ok_or_else(
                        || {
                            Error::new(
                                SqlState::UNDEFINED_OBJECT,
                                format!("role \"{name}\" does not exist"),
                            )
                        },
                    )?
                }
                RoleSpecType::ROLESPEC_PUBLIC => {
                    return Err(Error::new(
                        SqlState::UNDEFINED_OBJECT,
                        "role \"public\" does not exist",
                    ));
                }
                _ => self.user,
            },
        };
        let owner_name = builtin::roles().iter().find(|r| r.oid == owner).map(|r| r.name);
        let name = match stmt.schemaname.as_deref() {
            Some(name) => name.to_string(),
            None => owner_name.unwrap_or_default().to_string(),
        };
        if !self.is_superuser() {
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("permission denied for database {}", self.an.env.database()),
            ));
        }
        if name.starts_with("pg_") {
            return Err(Error::new(
                SqlState::RESERVED_NAME,
                format!("unacceptable schema name \"{name}\""),
            )
            .with_detail("The prefix \"pg_\" is reserved for system schemas."));
        }
        let exists = self.catalog.schema_by_name(&name).is_some()
            || builtin::named(Named::Namespace).iter().any(|n| n.name == name);
        if exists && stmt.if_not_exists {
            self.notice(
                SqlState::DUPLICATE_SCHEMA,
                format!("schema \"{name}\" already exists, skipping"),
            );
            return Ok(());
        }
        if exists {
            return Err(Error::new(
                SqlState::DUPLICATE_SCHEMA,
                format!("schema \"{name}\" already exists"),
            ));
        }
        if !stmt.schemaElts.is_empty() {
            return Err(not_yet("CREATE SCHEMA with elements", None));
        }
        self.catalog.create_schema(&name, owner)?;
        Ok(())
    }
}

/// The detail of `errdetail_relkind_not_supported`.
fn relkind_detail(kind: u8) -> &'static str {
    match kind {
        b'i' | b'I' => "This operation is not supported for indexes.",
        b'S' => "This operation is not supported for sequences.",
        b'v' => "This operation is not supported for views.",
        b'm' => "This operation is not supported for materialized views.",
        b'c' => "This operation is not supported for composite types.",
        b'f' => "This operation is not supported for foreign tables.",
        b't' => "This operation is not supported for TOAST tables.",
        _ => "This operation is not supported for tables.",
    }
}

/// The OID of the schema `pg_catalog` or `pg_toast`, where only the bootstrap can create relations.
fn is_system_namespace(namespace: u32) -> bool {
    namespace == PG_CATALOG_NAMESPACE || namespace == PG_TOAST_NAMESPACE
}
