//! `DefineView`: `CREATE VIEW` and `CREATE OR REPLACE VIEW`.
//!
//! The catalog keeps the text of the statement and the schemas of `search_path`, and the analyzer reads the query again each time a query uses the view. The rule of the view depends on the relations and the columns that the query reads, as `recordDependencyOnExpr` records them.

use rupg_catalog::{
    Catalog, Column, MAX_COLUMNS, NewRelation, ObjRef, PG_CLASS, PG_CONSTRAINT, PG_PROC, PG_TYPE,
    RelKind,
};
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin::{self, Named};
use rupg_sql::nodes::{DefElem, Node, ViewCheckOption, ViewStmt};
use rupg_types::{Value, oid};

use super::reloptions;
use super::{Definer, Found, Message, check_attribute_type, not_yet, relname};
use crate::collate::target_collation;
use crate::expr::{Expr, ExprKind};
use crate::select::Query;
use crate::typename::{names, type_name_text};
use crate::types;

impl Definer<'_, '_> {
    /// `DefineView` and `DefineVirtualRelation`. `text` is the text of the statement, which the catalog keeps.
    pub(super) fn create_view(&mut self, stmt: &ViewStmt, text: &str) -> Result<()> {
        let mut rv = stmt.view.as_deref().cloned().unwrap_or_default();
        let Some(Node::SelectStmt(select)) = stmt.query.as_ref() else {
            return Err(Error::internal("the query of a view is not a SELECT"));
        };
        if select.intoClause.is_some() {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "views must not contain SELECT INTO",
            ));
        }
        let query = self.an.select(select)?;
        if query.ctes.iter().any(|c| c.write.is_some()) {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "views must not contain data-modifying statements in WITH",
            ));
        }
        // `WITH CHECK OPTION` is the option `check_option` of the view.
        let mut options = stmt.options.clone();
        let check = match stmt.withCheckOption {
            ViewCheckOption::LOCAL_CHECK_OPTION => Some("local"),
            ViewCheckOption::CASCADED_CHECK_OPTION => Some("cascaded"),
            _ => None,
        };
        if let Some(check) = check {
            let option = DefElem {
                defname: Some("check_option".into()),
                arg: Some(Node::String(check.into())),
                location: -1,
                ..DefElem::default()
            };
            options.push(Some(option.into()));
        }
        if options.iter().flatten().any(|o| def_name(o) == Some("check_option")) {
            self.an.auto_updatable(&query, true).map_err(check_option_error)?;
        }
        let targets: Vec<&Expr> =
            query.targets.iter().filter(|t| !t.junk).map(|t| &t.expr).collect();
        let aliases = names(&stmt.aliases);
        if aliases.len() > targets.len() {
            return Err(Error::new(
                SqlState::SYNTAX_ERROR,
                "CREATE VIEW specifies more column names than columns",
            ));
        }
        if rv.relpersistence == b'u' {
            return Err(Error::new(
                SqlState::SYNTAX_ERROR,
                "views cannot be unlogged because they do not have storage",
            ));
        }
        let names = query.targets.iter().filter(|t| !t.junk).map(|t| t.name.as_str());
        let mut columns = Vec::with_capacity(targets.len());
        for (i, (expr, name)) in targets.iter().zip(names).enumerate() {
            let name = aliases.get(i).copied().unwrap_or(name);
            let collation = target_collation(expr, &query, Some(&*self.catalog));
            if types::collatable(expr.ty) && collation == 0 {
                return Err(Error::new(
                    SqlState::INDETERMINATE_COLLATION,
                    format!(
                        "could not determine which collation to use for view column \"{name}\""
                    ),
                )
                .with_hint("Use the COLLATE clause to set the collation explicitly."));
            }
            columns.push(Column::new(name, expr.ty, expr.typmod, collation));
        }
        let mut refs = Vec::new();
        query_references(self.catalog, &query, &[], &mut refs);
        // `isQueryUsingTempRelation`: a view of a temporary relation is a temporary view.
        let temp = refs.iter().copied().find(|r| {
            r.class == PG_CLASS
                && self
                    .catalog
                    .relation(r.oid)
                    .is_some_and(|rel| self.catalog.is_temp_schema(rel.namespace))
        });
        if rv.relpersistence == b'p'
            && let Some(temp) = temp
        {
            rv.relpersistence = b't';
            let temp = ObjRef { sub: 0, ..temp };
            let detail = format!("It depends on temporary {}.", self.describe(temp));
            self.flush();
            self.messages.push(Message::Notice(
                Error::new(
                    SqlState::SUCCESSFUL_COMPLETION,
                    format!("view \"{}\" will be a temporary view", relname(&rv)),
                )
                .with_detail(detail),
            ));
        }
        // DefineVirtualRelation looks up the schema with no parse state, so its errors have no position.
        let namespace = self.creation_namespace(&rv).map_err(|e| e.with_position(None))?;
        let name = relname(&rv).to_string();
        let path = self.an.path.clone();
        if stmt.replace
            && let Some(found) = self.relation_in(namespace, &name)
        {
            let Found::User(view) = found else {
                if let Found::Builtin(row) = found
                    && row.kind == b'v'
                {
                    return Err(not_yet("CREATE OR REPLACE VIEW of a system view", None));
                }
                return Err(not_a_view(&name));
            };
            let old = match self.catalog.relation(view) {
                Some(rel) if rel.kind == RelKind::View => rel.columns.clone(),
                _ => return Err(not_a_view(&name)),
            };
            check_view_columns(&columns, &old)?;
            let options = view_options(&options)?;
            self.catalog.replace_view(view, columns, text.to_string(), path, &refs)?;
            return self.catalog.set_options(view, options);
        }
        let options = view_options(&options)?;
        if columns.len() > MAX_COLUMNS {
            return Err(Error::new(
                SqlState::TOO_MANY_COLUMNS,
                format!("tables can have at most {MAX_COLUMNS} columns"),
            ));
        }
        for (i, column) in columns.iter().enumerate() {
            if columns[..i].iter().any(|c| c.name == column.name) {
                return Err(Error::new(
                    SqlState::DUPLICATE_COLUMN,
                    format!("column \"{}\" specified more than once", column.name),
                ));
            }
        }
        for column in &columns {
            check_attribute_type(&column.name, column.ty, self.an.env.allow_system_table_mods())?;
        }
        self.check_relation_name(namespace, &name)?;
        let new = NewRelation { namespace, name, owner: self.user, columns };
        let view = self.catalog.create_view(new, text.to_string(), path, &refs)?;
        self.apply_default_acl(ObjRef::new(PG_CLASS, view))?;
        self.catalog.set_options(view, options)
    }
}

/// The name of a `DefElem`.
fn def_name(node: &Node) -> Option<&str> {
    match node {
        Node::DefElem(def) => def.defname.as_deref(),
        _ => None,
    }
}

/// `defGetString`: the value of an option as text. An option without a value is `true`.
pub(super) fn def_string(arg: Option<&Node>) -> String {
    match arg {
        None => "true".to_string(),
        Some(Node::Integer(n)) => n.to_string(),
        Some(Node::Float(s) | Node::String(s) | Node::BitString(s)) => s.to_string(),
        Some(Node::Boolean(b)) => b.to_string(),
        Some(Node::TypeName(name)) => type_name_text(name),
        Some(_) => String::new(),
    }
}

/// `parse_bool`: as `boolin`, but without spaces around the word.
pub(super) fn parse_bool(text: &str) -> Option<bool> {
    if text.trim() != text {
        return None;
    }
    rupg_types::bool_in(text).ok()
}

/// `transformRelOptions` and `view_reloptions`: the options of `WITH` as `name=value`, in the order of the statement. An option in the namespace `toast` does not apply to a view and is dropped.
fn view_options(options: &[Option<Node>]) -> Result<Vec<String>> {
    let options = reloptions::transform_create(options, None, &["toast"])?;
    reloptions::validate(&options, reloptions::VIEW, false)?;
    Ok(options)
}

/// The error of `DefineView` and `ATExecSetRelOptions` for the option `check_option` of a view that is not automatically updatable, with the reason as the hint.
pub(super) fn check_option_error(reason: &str) -> Error {
    Error::new(
        SqlState::FEATURE_NOT_SUPPORTED,
        "WITH CHECK OPTION is supported only on automatically updatable views",
    )
    .with_hint(reason)
}

/// The error of `CREATE OR REPLACE VIEW` for a relation that is not a view.
fn not_a_view(name: &str) -> Error {
    Error::new(SqlState::WRONG_OBJECT_TYPE, format!("\"{name}\" is not a view"))
}

/// `checkViewColumns`: the old columns of the view come first in the new columns, with the same names, types and collations.
fn check_view_columns(new: &[Column], old: &[Column]) -> Result<()> {
    if new.len() < old.len() {
        return Err(Error::new(
            SqlState::INVALID_TABLE_DEFINITION,
            "cannot drop columns from view",
        ));
    }
    for (new, old) in new.iter().zip(old) {
        if new.name != old.name {
            return Err(Error::new(
                SqlState::INVALID_TABLE_DEFINITION,
                format!("cannot change name of view column \"{}\" to \"{}\"", old.name, new.name),
            )
            .with_hint(
                "Use ALTER VIEW ... RENAME COLUMN ... to change name of view column instead.",
            ));
        }
        if new.ty != old.ty || new.typmod != old.typmod {
            return Err(Error::new(
                SqlState::INVALID_TABLE_DEFINITION,
                format!(
                    "cannot change data type of view column \"{}\" from {} to {}",
                    old.name,
                    types::with_typmod(old.ty, old.typmod),
                    types::with_typmod(new.ty, new.typmod)
                ),
            ));
        }
        if new.collation != old.collation {
            let collation =
                |oid: u32| builtin::named_by_oid(Named::Collation, oid).map_or("???", |n| n.name);
            return Err(Error::new(
                SqlState::INVALID_TABLE_DEFINITION,
                format!(
                    "cannot change collation of view column \"{}\" from \"{}\" to \"{}\"",
                    old.name,
                    collation(old.collation),
                    collation(new.collation)
                ),
            ));
        }
    }
    Ok(())
}

/// The objects that a query in the body of a function refers to, as for a view, each one once.
pub(super) fn function_references(catalog: &Catalog, query: &Query) -> Vec<ObjRef> {
    let mut found = Vec::new();
    query_references(catalog, query, &[], &mut found);
    let mut refs = Vec::new();
    for object in found {
        if !refs.contains(&object) {
            refs.push(object);
        }
    }
    refs
}

/// `find_expr_references_walker` for the query of a view: each relation of the query, each column that the query reads, each field of a composite type that the query selects, each function that the query calls, and the relation or the type of each `regclass` or `regtype` constant, also in the subqueries and in the queries of `WITH`. A relation that reads a query of `WITH` has a copy of the query, which does not count, so a query of `WITH` that no part of the query reads also counts, as in PostgreSQL. `outer` holds the queries outside `query`, from the nearest. A view in the query is one relation, and the query of the view does not count.
pub(super) fn query_references(
    catalog: &Catalog,
    query: &Query,
    outer: &[&Query],
    refs: &mut Vec<ObjRef>,
) {
    let mut levels = vec![query];
    levels.extend_from_slice(outer);
    for relation in &query.relations {
        if relation.oid != 0 {
            refs.push(ObjRef::new(PG_CLASS, relation.oid));
        } else if let Some(sub) = &relation.subquery
            && relation.cte.is_none()
        {
            query_references(catalog, sub, &levels, refs);
        }
    }
    for expr in query.exprs() {
        expr_references(catalog, expr, &levels, refs);
    }
    for cte in &query.ctes {
        query_references(catalog, &cte.query, &levels, refs);
    }
    refs.extend(query.constraint_deps.iter().map(|&c| ObjRef::new(PG_CONSTRAINT, c)));
}

/// `process_function_rte_ref`: the column of the relation of the composite type that a call of a function in `FROM` gives for the column `attnum` of the relation of the call. The row type has no field for a dropped column, so the column is the live column of the same place.
fn function_column_reference(
    catalog: &Catalog,
    function: &crate::FromFunction,
    attnum: i16,
) -> Option<ObjRef> {
    let mut place = usize::try_from(attnum).ok()?.checked_sub(1)?;
    for (call, count) in &function.calls {
        if place >= *count {
            place -= count;
            continue;
        }
        let relid = match types::row(call.ty) {
            Some(row) if row.kind == b'c' => row.relid,
            Some(_) => return None,
            None => catalog.relations().find(|r| r.row_type == call.ty)?.oid,
        };
        let rel = catalog.relation(relid)?;
        let (i, _) = rel.columns.iter().enumerate().filter(|(_, c)| !c.dropped).nth(place)?;
        return Some(ObjRef::column(relid, i16::try_from(i + 1).ok()?));
    }
    None
}

/// The references of an expression of the first query of `levels`. A column of the whole row has no reference.
pub(super) fn expr_references(
    catalog: &Catalog,
    expr: &Expr,
    levels: &[&Query],
    refs: &mut Vec<ObjRef>,
) {
    match (&expr.kind, expr.ty) {
        (ExprKind::Var(var), _) if var.attnum != 0 => {
            let relation = levels.get(var.levels_up).and_then(|q| q.relations.get(var.relation));
            if let Some(relation) = relation
                && relation.oid != 0
            {
                refs.push(ObjRef::column(relation.oid, var.attnum));
            } else if let Some(function) = relation.and_then(|r| r.function.as_ref()) {
                refs.extend(function_column_reference(catalog, function, var.attnum));
            }
        }
        (ExprKind::Const(Value::Oid(found)), oid::REGCLASS) => {
            refs.push(ObjRef::new(PG_CLASS, *found));
        }
        (ExprKind::Const(Value::Oid(found)), oid::REGTYPE) => {
            refs.push(ObjRef::new(PG_TYPE, *found));
        }
        (ExprKind::Func(f), _) => refs.push(ObjRef::new(PG_PROC, f.oid)),
        (ExprKind::Agg(a), _) => refs.push(ObjRef::new(PG_PROC, a.oid)),
        (ExprKind::Window(w), _) => refs.push(ObjRef::new(PG_PROC, w.oid)),
        (ExprKind::SubLink(sub), _) => query_references(catalog, &sub.query, levels, refs),
        _ => refs.extend(super::field_reference(catalog, expr)),
    }
    refs.extend(super::type_reference(expr));
    for child in expr.children() {
        expr_references(catalog, child, levels, refs);
    }
}
