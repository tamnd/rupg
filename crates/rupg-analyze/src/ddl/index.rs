//! `CREATE INDEX`, as `transformIndexStmt`, `DefineIndex` and `ComputeIndexAttrs` run it.

use rupg_catalog::size::{empty_index_pages, new_table_tuples, typavgwidth};
use rupg_catalog::{ConKind, NewIndex, NewIndexColumn, RelKind};
use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{IndexElem, IndexStmt, Node, SortByDir, SortByNulls};
use rupg_types::oid;

use super::reloptions;
use super::{Definer, Found, is_mutable, not_yet, references, relkind_detail};
use crate::agg::Kind;
use crate::coerce::AtOpt;
use crate::colname::figure_index_colname;
use crate::expr::{Expr, ExprKind};
use crate::typcache::{default_opclass, is_binary_coercible};
use crate::typename::{names, place};
use crate::types;

/// The OID of the access method `btree`.
pub(super) const BTREE: u32 = 403;
/// The OID of the access method `hash`.
pub(super) const HASH: u32 = 405;
/// The most columns of an index, `INDEX_MAX_KEYS`.
const INDEX_MAX_KEYS: usize = 32;
/// The OID of the collation `default`.
const DEFAULT_COLLATION: u32 = 100;

/// The names of operator classes that PostgreSQL ignores, from before version 7.
const LEGACY_OPCLASSES: [&str; 6] =
    ["network_ops", "timespan_ops", "datetime_ops", "lztext_ops", "timestamp_ops", "bigbox_ops"];

/// The type of a system column, by name.
pub(super) fn system_column(name: &str) -> Option<(i16, u32)> {
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
    /// `transformIndexStmt` and `DefineIndex`. `alter` is true for the index of a constraint of `ALTER TABLE`, as `is_alter_table` is.
    pub(super) fn create_index(&mut self, stmt: &IndexStmt, alter: bool) -> Result<()> {
        let rv = stmt.relation.as_deref().cloned().unwrap_or_default();
        let found = self.lookup_relation(&rv)?;
        let kind = self.kind_of(found);
        let owner = match found {
            Found::User(oid) => self.catalog.relation(oid).map_or(0, |r| r.owner),
            Found::Builtin(row) => row.owner,
        };
        let name = super::relname(&rv).to_string();
        if !self.is_superuser() && !self.owns(owner) {
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
        if self.is_system(found) {
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
                rel.columns.iter().map(crate::from::scope_column).collect();
            let relname = rel.name.clone();
            let row_type = rel.row_type;
            self.an.add_table(table, &relname, &columns, row_type);
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
        if !matches!(kind, b'r' | b'p' | b'm') {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("cannot create index on relation \"{name}\""),
            )
            .with_detail(relkind_detail(kind)));
        }
        if kind == b'p' && stmt.concurrent {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!("cannot create index on partitioned table \"{name}\" concurrently"),
            ));
        }
        let Some(rel) = self.catalog.relation(table) else {
            return Err(not_yet("an index on a built-in relation", None));
        };
        let (namespace, rel_name) = (rel.namespace, rel.name.clone());
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
        let options = self.create_index_options(method, &stmt.options)?;
        if stmt.tableSpace.is_some() {
            return Err(not_yet("TABLESPACE", None));
        }
        if predicate.as_ref().is_some_and(|p| is_mutable(self.catalog, p)) {
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
        if stmt.primary {
            self.check_primary_key(table, &rel_name, &columns[..key_count], alter)?;
        }
        if kind == b'p' && stmt.unique {
            self.check_partition_key(table, &columns[..key_count], stmt.primary)?;
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
        let mut refs =
            references(self.catalog, table, &exprs.iter().chain(&predicate).collect::<Vec<_>>())?;
        if exprs.is_empty() && predicate.is_none() {
            refs.clear();
        }
        // `DefineIndex` checks `USAGE` with the search path that `RestrictSearchPath` sets, so the error names the type with its schema.
        let restricted = [crate::PG_CATALOG_NAMESPACE]
            .into_iter()
            .chain((self.an.temp != 0).then_some(self.an.temp))
            .collect();
        let path = std::mem::replace(&mut self.an.temp_path, restricted);
        let usage = self.check_usage_on_types(&refs);
        self.an.temp_path = path;
        usage?;
        let pages = self.index_pages(table, method);
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
            pages,
        };
        let (index, constraint) = self.catalog.create_index(new)?;
        // `DefineIndex` builds an index of `CONCURRENTLY` on a temporary table as any other index.
        if stmt.concurrent && !self.catalog.is_temp_schema(namespace) {
            self.concurrent = Some(index);
        }
        if !options.is_empty() {
            self.catalog.set_options(index, options)?;
        }
        if kind == b'p' {
            // With `ONLY`, the index is not valid until each partition has an index for it.
            if !rv.inh {
                if !self.catalog.partitions(table).is_empty() {
                    self.catalog.set_index_valid(index, false)?;
                }
            } else {
                self.index_partitions(table, index, constraint, alter)?;
            }
        }
        // `index_build` reads the rows of the table with a sequential scan.
        if self
            .catalog
            .relation(table)
            .is_some_and(|r| matches!(r.kind, RelKind::Table | RelKind::MatView))
        {
            self.scans.push(table);
        }
        Ok(())
    }

    /// The pages of the main fork of a new index of the table, from [`empty_index_pages`]. The index of a partitioned table has no storage.
    pub(super) fn index_pages(&self, table: u32, method: u32) -> i32 {
        self.catalog.relation(table).map_or(1, |rel| {
            if rel.kind == RelKind::Partitioned {
                return 0;
            }
            let widths = rel
                .columns
                .iter()
                .filter(|c| !c.dropped)
                .map(|c| typavgwidth(types::row(c.ty).map_or(-1, |t| t.len), c.ty, c.typmod));
            let fillfactor = reloptions::int_option(&rel.options, "fillfactor").unwrap_or(100);
            empty_index_pages(method, new_table_tuples(widths, fillfactor))
        })
    }

    /// `index_check_primary_key`: `ALTER TABLE` cannot add a second primary key, and the columns of a primary key must be not null.
    pub(super) fn check_primary_key(
        &self,
        table: u32,
        name: &str,
        keys: &[NewIndexColumn],
        alter: bool,
    ) -> Result<()> {
        let invalid = |message: String| Error::new(SqlState::INVALID_TABLE_DEFINITION, message);
        if alter && self.catalog.constraints_of(table).any(|c| c.kind == ConKind::Primary) {
            return Err(invalid(format!(
                "multiple primary keys for table \"{name}\" are not allowed"
            )));
        }
        for key in keys {
            if key.key == 0 {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    "primary keys cannot be expressions",
                ));
            }
            let column = self.catalog.relation(table).and_then(|r| r.column(key.key));
            if let Some(column) = column
                && !column.not_null
            {
                return Err(invalid(format!(
                    "primary key column \"{}\" is not marked NOT NULL",
                    column.name
                )));
            }
        }
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
                _ if is_mutable(self.catalog, expr) => {
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
            if let Some(error) = self.opclass_without_options(class) {
                return Err(error);
            }
            return Err(not_yet("operator class options", at));
        }
        Ok(NewIndexColumn { name: column_name, key, ty, typmod, collation, class, option })
    }

    /// `exprCollation` of an index expression. An explicit `COLLATE` at the top gives its collation. Else the collation of the columns in the expression wins over the default collation, two different collations of columns give no collation, and an expression without columns has the default collation.
    pub(super) fn expr_collation(
        &self,
        table: u32,
        raw: Option<&Node>,
        expr: &Expr,
    ) -> Result<u32> {
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
    pub(super) fn resolve_opclass(
        &self,
        opclass: &[Option<Node>],
        ty: u32,
        method: u32,
    ) -> Result<u32> {
        let parts = names(opclass);
        let legacy = parts.len() == 1 && LEGACY_OPCLASSES.contains(&parts[0]);
        if parts.is_empty() || legacy {
            return default_opclass(ty, method)
                .map(|c| c.oid)
                .ok_or_else(|| no_default_opclass(ty, method));
        }
        let shown = parts.join(".");
        let (class, intype) = self.opclass_by_name(method, &parts)?;
        if !is_binary_coercible(ty, intype) {
            return Err(Error::new(
                SqlState::DATATYPE_MISMATCH,
                format!("operator class \"{shown}\" does not accept data type {}", types::name(ty)),
            ));
        }
        Ok(class)
    }
}

/// The error of `ResolveOpClass` for a type with no default operator class for the access method.
pub(super) fn no_default_opclass(ty: u32, method: u32) -> Error {
    let method_name = if method == HASH { "hash" } else { "btree" };
    Error::new(
        SqlState::UNDEFINED_OBJECT,
        format!(
            "data type {} has no default operator class for access method \"{method_name}\"",
            types::name(ty)
        ),
    )
    .with_hint("You must specify an operator class for the index or define a default operator class for the data type.")
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
