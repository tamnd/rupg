//! `CREATE INDEX`, as `transformIndexStmt`, `DefineIndex` and `ComputeIndexAttrs` run it.

use rupg_catalog::{ConKind, NewIndex, NewIndexColumn};
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin;
use rupg_sql::nodes::{IndexElem, IndexStmt, Node, SortByDir, SortByNulls};
use rupg_types::oid;

use super::{Definer, Found, is_mutable, is_system, not_yet, references, relkind_detail};
use crate::agg::Kind;
use crate::coerce::AtOpt;
use crate::colname::figure_index_colname;
use crate::expr::{Expr, ExprKind};
use crate::typcache::{default_opclass, is_binary_coercible};
use crate::typename::{names, place};
use crate::types;

/// The OID of the access method `btree`.
const BTREE: u32 = 403;
/// The OID of the access method `hash`.
const HASH: u32 = 405;
/// The most columns of an index, `INDEX_MAX_KEYS`.
const INDEX_MAX_KEYS: usize = 32;
/// The OID of the collation `default`.
const DEFAULT_COLLATION: u32 = 100;

/// The names of operator classes that PostgreSQL ignores, from before version 7.
const LEGACY_OPCLASSES: [&str; 6] =
    ["network_ops", "timespan_ops", "datetime_ops", "lztext_ops", "timestamp_ops", "bigbox_ops"];

/// The type of a system column, by name.
fn system_column(name: &str) -> Option<(i16, u32)> {
    match name {
        "ctid" => Some((-1, oid::TID)),
        "xmin" => Some((-2, oid::XID)),
        "cmin" => Some((-3, oid::CID)),
        "xmax" => Some((-4, oid::XID)),
        "cmax" => Some((-5, oid::CID)),
        "tableoid" => Some((-6, oid::OID)),
        _ => None,
    }
}

/// An element of the index after `transformIndexStmt`.
struct Element {
    elem: IndexElem,
    expr: Option<Expr>,
}

impl Definer<'_, '_> {
    /// `transformIndexStmt` and `DefineIndex`.
    pub(super) fn create_index(&mut self, stmt: &IndexStmt) -> Result<()> {
        let rv = stmt.relation.as_deref().cloned().unwrap_or_default();
        let found = self.lookup_relation(&rv)?;
        let kind = self.kind_of(found);
        let owner = match found {
            Found::User(oid) => self.catalog.relation(oid).map_or(0, |r| r.owner),
            Found::Builtin(row) => row.owner,
        };
        let name = super::relname(&rv).to_string();
        if !self.is_superuser() && owner != self.user {
            let what = match kind {
                b'i' => "index",
                b'S' => "sequence",
                b'v' => "view",
                b'm' => "materialized view",
                _ => "table",
            };
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("must be owner of {what} {name}"),
            ));
        }
        if is_system(found) {
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("permission denied: \"{name}\" is a system catalog"),
            ));
        }
        let elements: Vec<&IndexElem> = elems(&stmt.indexParams);
        let including: Vec<&IndexElem> = elems(&stmt.indexIncludingParams);
        let has_exprs = elements.iter().chain(&including).any(|e| e.expr.is_some());
        let table = match found {
            Found::User(oid) => oid,
            Found::Builtin(_) if has_exprs || stmt.whereClause.is_some() => {
                return Err(not_yet("an index on a built-in relation", None));
            }
            Found::Builtin(_) => 0,
        };
        self.an.scope = crate::from::Scope::default();
        if let Some(rel) = self.catalog.relation(table) {
            let columns: Vec<(String, u32, i32)> =
                rel.columns.iter().map(|c| (c.name.clone(), c.ty, c.typmod)).collect();
            let relname = rel.name.clone();
            self.an.add_table(table, &relname, &columns);
        }
        let predicate = match &stmt.whereClause {
            Some(node) => {
                let expr =
                    self.an.with_kind(Kind::IndexPredicate, |an| an.transform(Some(node)))?;
                Some(self.an.coerce_to_boolean(expr, "WHERE")?)
            }
            None => None,
        };
        let mut all = Vec::new();
        for elem in elements.iter().chain(&including) {
            let mut elem = (*elem).clone();
            let expr = match &elem.expr {
                Some(node) => {
                    if elem.indexcolname.is_none() {
                        elem.indexcolname = figure_index_colname(Some(node)).map(Into::into);
                    }
                    let expr =
                        self.an.with_kind(Kind::IndexExpression, |an| an.transform(Some(node)))?;
                    Some(expr)
                }
                None => None,
            };
            all.push(Element { elem, expr });
        }
        let key_count = elements.len();
        if key_count == 0 {
            return Err(Error::new(
                SqlState::INVALID_OBJECT_DEFINITION,
                "must specify at least one column",
            ));
        }
        if all.len() > INDEX_MAX_KEYS {
            return Err(Error::new(
                SqlState::TOO_MANY_COLUMNS,
                format!("cannot use more than {INDEX_MAX_KEYS} columns in an index"),
            ));
        }
        if kind == b'i' || kind == b'I' {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("cannot open relation \"{name}\""),
            )
            .with_detail(relkind_detail(kind)));
        }
        if kind != b'r' {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("cannot create index on relation \"{name}\""),
            )
            .with_detail(relkind_detail(kind)));
        }
        let Some(rel) = self.catalog.relation(table) else {
            return Err(not_yet("an index on a built-in relation", None));
        };
        let namespace = rel.namespace;
        self.check_create_in(namespace, None)?;
        let method_name = stmt.accessMethod.as_deref().unwrap_or("btree");
        let method = match method_name {
            "btree" => BTREE,
            "hash" => HASH,
            "gist" | "gin" | "spgist" | "brin" => {
                return Err(not_yet(&format!("the access method {method_name}"), None));
            }
            "rtree" => {
                self.notice(
                    SqlState::SUCCESSFUL_COMPLETION,
                    "substituting access method \"gist\" for obsolete method \"rtree\"".into(),
                );
                return Err(not_yet("the access method gist", None));
            }
            "heap" => {
                return Err(Error::internal(
                    "index access method handler function 3 did not return an IndexAmRoutine struct",
                ));
            }
            _ => {
                return Err(Error::new(
                    SqlState::UNDEFINED_OBJECT,
                    format!("access method \"{method_name}\" does not exist"),
                ));
            }
        };
        if method == HASH {
            let unsupported = if stmt.unique {
                Some("unique indexes")
            } else if !including.is_empty() {
                Some("included columns")
            } else if key_count > 1 {
                Some("multicolumn indexes")
            } else {
                None
            };
            if let Some(what) = unsupported {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    format!("access method \"hash\" does not support {what}"),
                ));
            }
        }
        if !stmt.options.is_empty() {
            return Err(not_yet("WITH options", None));
        }
        if stmt.tableSpace.is_some() {
            return Err(not_yet("TABLESPACE", None));
        }
        if stmt.concurrent {
            return Err(not_yet("CREATE INDEX CONCURRENTLY", None));
        }
        if predicate.as_ref().is_some_and(is_mutable) {
            return Err(Error::new(
                SqlState::INVALID_OBJECT_DEFINITION,
                "functions in index predicate must be marked IMMUTABLE",
            ));
        }
        let mut columns = Vec::with_capacity(all.len());
        let mut exprs = Vec::new();
        for (i, element) in all.iter().enumerate() {
            let column =
                self.index_column(table, element, i >= key_count, method, stmt.isconstraint)?;
            if column.key == 0 {
                exprs.extend(element.expr.clone());
            }
            columns.push(column);
        }
        let system_var = |e: &Expr| {
            e.find(0, &mut |e, _| match &e.kind {
                ExprKind::Var(var) if var.attnum < 0 => Some(()),
                _ => None,
            })
            .is_some()
        };
        if columns.iter().any(|c| c.key < 0) || exprs.iter().chain(&predicate).any(system_var) {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "index creation on system columns is not supported",
            ));
        }
        if let Some(index_name) = stmt.idxname.as_deref()
            && stmt.if_not_exists
            && self.relation_in(namespace, index_name).is_some()
        {
            self.notice(
                SqlState::DUPLICATE_TABLE,
                format!("relation \"{index_name}\" already exists, skipping"),
            );
            return Ok(());
        }
        let constraint = match (stmt.isconstraint, stmt.primary) {
            (false, _) => None,
            (true, true) => Some(ConKind::Primary),
            (true, false) => Some(ConKind::Unique),
        };
        let mut refs = references(table, &exprs.iter().chain(&predicate).collect::<Vec<_>>());
        if exprs.is_empty() && predicate.is_none() {
            refs.clear();
        }
        let new = NewIndex {
            table,
            name: stmt.idxname.as_deref().map(str::to_string),
            columns,
            key_count,
            method,
            unique: stmt.unique,
            nulls_not_distinct: stmt.nulls_not_distinct,
            constraint,
            deferrable: stmt.deferrable,
            deferred: stmt.initdeferred,
            exprs: if exprs.is_empty() { None } else { Some(crate::node::write_list(&exprs)?) },
            predicate: predicate.as_ref().map(crate::node::write).transpose()?,
            refs,
        };
        self.catalog.create_index(new)?;
        Ok(())
    }

    /// One column of `ComputeIndexAttrs`.
    fn index_column(
        &self,
        table: u32,
        element: &Element,
        included: bool,
        method: u32,
        isconstraint: bool,
    ) -> Result<NewIndexColumn> {
        let elem = &element.elem;
        let at = place(elem.location);
        let rel = self.catalog.relation(table);
        let (mut key, ty, typmod, mut collation) = match (&elem.name, &element.expr) {
            (Some(name), _) => {
                let column = rel.and_then(|r| r.column_number(name).map(|n| (n, r.column(n))));
                match (column, system_column(name)) {
                    (Some((n, Some(c))), _) => (n, c.ty, c.typmod, c.collation),
                    (_, Some((n, ty))) => (n, ty, -1, 0),
                    _ => {
                        let message = if isconstraint {
                            format!("column \"{name}\" named in key does not exist")
                        } else {
                            format!("column \"{name}\" does not exist")
                        };
                        return Err(Error::new(SqlState::UNDEFINED_COLUMN, message).at_opt(at));
                    }
                }
            }
            (None, Some(expr)) => {
                if included {
                    return Err(Error::new(
                        SqlState::FEATURE_NOT_SUPPORTED,
                        "expressions are not supported in included columns",
                    )
                    .at_opt(at));
                }
                let collation = self.expr_collation(table, elem.expr.as_ref(), expr)?;
                (0, expr.ty, expr.typmod, collation)
            }
            (None, None) => {
                return Err(Error::internal("index element has no name and no expression"));
            }
        };
        if let Some(expr) = &element.expr {
            match &expr.kind {
                ExprKind::Var(var) if var.attnum != 0 => key = var.attnum,
                _ if is_mutable(expr) => {
                    return Err(Error::new(
                        SqlState::INVALID_OBJECT_DEFINITION,
                        "functions in index expression must be marked IMMUTABLE",
                    )
                    .at_opt(at));
                }
                _ => {}
            }
        }
        let column_name =
            elem.indexcolname.as_deref().or(elem.name.as_deref()).unwrap_or("expr").to_string();
        if included {
            let unsupported = if !elem.collation.is_empty() {
                Some("a collation")
            } else if !elem.opclass.is_empty() {
                Some("an operator class")
            } else if elem.ordering != SortByDir::SORTBY_DEFAULT {
                Some("ASC/DESC options")
            } else if elem.nulls_ordering != SortByNulls::SORTBY_NULLS_DEFAULT {
                Some("NULLS FIRST/LAST options")
            } else {
                None
            };
            if let Some(what) = unsupported {
                return Err(Error::new(
                    SqlState::INVALID_OBJECT_DEFINITION,
                    format!("including column does not support {what}"),
                )
                .at_opt(at));
            }
            return Ok(NewIndexColumn {
                name: column_name,
                key,
                ty,
                typmod,
                collation: 0,
                class: 0,
                option: 0,
            });
        }
        if !elem.collation.is_empty() {
            collation = self.an.collation_oid(&elem.collation, at)?;
        }
        if types::collatable(ty) {
            if collation == 0 {
                return Err(Error::new(
                    SqlState::INDETERMINATE_COLLATION,
                    "could not determine which collation to use for index expression",
                )
                .with_hint("Use the COLLATE clause to set the collation explicitly."));
            }
        } else if collation != 0 {
            return Err(Error::new(
                SqlState::DATATYPE_MISMATCH,
                format!("collations are not supported by type {}", types::name(ty)),
            )
            .at_opt(at));
        }
        let class = self.resolve_opclass(&elem.opclass, ty, method)?;
        let mut option = 0;
        if method == BTREE {
            if elem.ordering == SortByDir::SORTBY_DESC {
                option |= 1;
            }
            if elem.nulls_ordering == SortByNulls::SORTBY_NULLS_FIRST
                || (elem.nulls_ordering == SortByNulls::SORTBY_NULLS_DEFAULT
                    && elem.ordering == SortByDir::SORTBY_DESC)
            {
                option |= 2;
            }
        } else {
            let unsupported = if elem.ordering != SortByDir::SORTBY_DEFAULT {
                Some("ASC/DESC options")
            } else if elem.nulls_ordering != SortByNulls::SORTBY_NULLS_DEFAULT {
                Some("NULLS FIRST/LAST options")
            } else {
                None
            };
            if let Some(what) = unsupported {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    format!("access method \"hash\" does not support {what}"),
                ));
            }
        }
        if !elem.opclassopts.is_empty() {
            return Err(not_yet("operator class options", at));
        }
        Ok(NewIndexColumn { name: column_name, key, ty, typmod, collation, class, option })
    }

    /// `exprCollation` of an index expression. An explicit `COLLATE` at the top gives its collation. Else the collation of the columns in the expression wins over the default collation, two different collations of columns give no collation, and an expression without columns has the default collation.
    fn expr_collation(&self, table: u32, raw: Option<&Node>, expr: &Expr) -> Result<u32> {
        if !types::collatable(expr.ty) {
            return Ok(0);
        }
        if let Some(Node::CollateClause(clause)) = raw {
            return self.an.collation_oid(&clause.collname, place(clause.location));
        }
        let rel = self.catalog.relation(table);
        let mut found: Option<u32> = None;
        let mut conflict = false;
        expr.find(0, &mut |e, _| {
            if let ExprKind::Var(var) = &e.kind
                && let Some(c) = rel.and_then(|r| r.column(var.attnum))
                && c.collation != 0
            {
                match found {
                    None => found = Some(c.collation),
                    Some(DEFAULT_COLLATION) => found = Some(c.collation),
                    Some(f) if f != c.collation && c.collation != DEFAULT_COLLATION => {
                        conflict = true;
                    }
                    Some(_) => {}
                }
            }
            None::<()>
        });
        if conflict {
            return Ok(0);
        }
        Ok(found.unwrap_or(DEFAULT_COLLATION))
    }

    /// `ResolveOpClass`.
    fn resolve_opclass(&self, opclass: &[Option<Node>], ty: u32, method: u32) -> Result<u32> {
        let method_name = if method == HASH { "hash" } else { "btree" };
        let parts = names(opclass);
        let legacy = parts.len() == 1 && LEGACY_OPCLASSES.contains(&parts[0]);
        if parts.is_empty() || legacy {
            return default_opclass(ty, method).map(|c| c.oid).ok_or_else(|| {
                Error::new(
                    SqlState::UNDEFINED_OBJECT,
                    format!(
                        "data type {} has no default operator class for access method \"{method_name}\"",
                        types::name(ty)
                    ),
                )
                .with_hint("You must specify an operator class for the index or define a default operator class for the data type.")
            });
        }
        let shown = parts.join(".");
        let (schema, name) = match parts.as_slice() {
            [name] => (None, *name),
            [schema, name] => (Some(*schema), *name),
            _ => {
                return Err(Error::new(
                    SqlState::SYNTAX_ERROR,
                    format!("improper qualified name (too many dotted names): {shown}"),
                ));
            }
        };
        let in_catalog = match schema {
            None => true,
            Some(schema) => {
                let ns = self.an.schema(schema).ok_or_else(|| {
                    Error::new(
                        SqlState::UNDEFINED_SCHEMA,
                        format!("schema \"{schema}\" does not exist"),
                    )
                })?;
                ns == crate::PG_CATALOG_NAMESPACE
            }
        };
        let class = builtin::opclasses()
            .iter()
            .find(|c| in_catalog && c.method == method && c.name == name)
            .ok_or_else(|| {
                Error::new(
                    SqlState::UNDEFINED_OBJECT,
                    format!(
                        "operator class \"{shown}\" does not exist for access method \"{method_name}\""
                    ),
                )
            })?;
        if !is_binary_coercible(ty, class.intype) {
            return Err(Error::new(
                SqlState::DATATYPE_MISMATCH,
                format!("operator class \"{shown}\" does not accept data type {}", types::name(ty)),
            ));
        }
        Ok(class.oid)
    }
}

/// The index elements of a list.
fn elems(list: &[Option<Node>]) -> Vec<&IndexElem> {
    list.iter()
        .flatten()
        .filter_map(|n| match n {
            Node::IndexElem(e) => Some(&**e),
            _ => None,
        })
        .collect()
}
