//! `DefineView`: `CREATE VIEW` and `CREATE OR REPLACE VIEW`.
//!
//! The catalog keeps the text of the statement and the schemas of `search_path`, and the analyzer reads the query again each time a query uses the view. The rule of the view depends on the relations and the columns that the query reads, as `recordDependencyOnExpr` records them.

use rupg_catalog::{Column, MAX_COLUMNS, NewRelation, ObjRef, PG_CLASS, PG_PROC, PG_TYPE};
use rupg_common::{Error, Result, SqlState};
use rupg_pgcatalog::builtin::{self, Named};
use rupg_sql::nodes::{Node, ViewCheckOption, ViewStmt};
use rupg_types::{Value, oid};

use super::{Definer, Found, check_attribute_type, not_yet, relname};
use crate::collate::target_collation;
use crate::expr::{Expr, ExprKind};
use crate::select::Query;
use crate::typename::{names, place, type_name_text};
use crate::types;

/// The 4 bytes of the length word of a `varlena` value, which a typmod of the character types and `numeric` includes.
const VARHDRSZ: i32 = 4;

impl Definer<'_, '_> {
    /// `DefineView` and `DefineVirtualRelation`. `text` is the text of the statement, which the catalog keeps.
    pub(super) fn create_view(&mut self, stmt: &ViewStmt, text: &str) -> Result<()> {
        let rv = stmt.view.as_deref().cloned().unwrap_or_default();
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
        if stmt.withCheckOption != ViewCheckOption::NO_CHECK_OPTION {
            return Err(not_yet("WITH CHECK OPTION", None));
        }
        if stmt.options.iter().flatten().any(|o| def_name(o) == Some("check_option")) {
            return Err(not_yet("WITH CHECK OPTION", None));
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
        if rv.relpersistence == b't' {
            return Err(not_yet("a temporary view", place(rv.location)));
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
        // DefineVirtualRelation looks up the schema with no parse state, so its errors have no position.
        let namespace = self.creation_namespace(&rv).map_err(|e| e.with_position(None))?;
        let name = relname(&rv).to_string();
        let mut refs = Vec::new();
        query_references(&query, &[], &mut refs);
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
                Some(rel) if rel.view.is_some() => rel.columns.clone(),
                _ => return Err(not_a_view(&name)),
            };
            check_view_columns(&columns, &old)?;
            let options = view_options(&stmt.options)?;
            self.catalog.replace_view(view, columns, text.to_string(), path, &refs)?;
            return self.catalog.set_options(view, options);
        }
        let options = view_options(&stmt.options)?;
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
fn def_string(arg: Option<&Node>) -> String {
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
fn parse_bool(text: &str) -> Option<bool> {
    if text.trim() != text {
        return None;
    }
    rupg_types::bool_in(text).ok()
}

/// `transformRelOptions` and `view_reloptions`: the options of `WITH` as `name=value`, in the order of the statement. An option with a namespace, such as `toast.name`, does not apply to a view and is dropped.
fn view_options(options: &[Option<Node>]) -> Result<Vec<String>> {
    let invalid = |message: String| Error::new(SqlState::INVALID_PARAMETER_VALUE, message);
    let mut out = Vec::new();
    let mut seen: Vec<&str> = Vec::new();
    for option in options.iter().flatten() {
        let Node::DefElem(def) = option else { continue };
        if def.defnamespace.is_some() {
            continue;
        }
        let name = def.defname.as_deref().unwrap_or_default();
        let value = def_string(def.arg.as_ref());
        out.push((name, value));
    }
    for (name, value) in &out {
        if !matches!(*name, "security_barrier" | "security_invoker") {
            return Err(invalid(format!("unrecognized parameter \"{name}\"")));
        }
        if seen.contains(name) {
            return Err(invalid(format!("parameter \"{name}\" specified more than once")));
        }
        seen.push(name);
        if parse_bool(value).is_none() {
            return Err(invalid(format!("invalid value for boolean option \"{name}\": {value}")));
        }
    }
    Ok(out.into_iter().map(|(name, value)| format!("{name}={value}")).collect())
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
                    type_with_typmod(old.ty, old.typmod),
                    type_with_typmod(new.ty, new.typmod)
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

/// `format_type_with_typemod` for the types whose typmod the analyzer can show: the character types and `numeric`. Another type shows its name without the typmod.
fn type_with_typmod(ty: u32, typmod: i32) -> String {
    let name = types::name(ty);
    if typmod < VARHDRSZ {
        return name;
    }
    match ty {
        oid::VARCHAR | oid::BPCHAR => format!("{name}({})", typmod - VARHDRSZ),
        oid::NUMERIC => {
            let bits = typmod - VARHDRSZ;
            let scale = ((bits & 0x7ff) ^ 1024) - 1024;
            format!("numeric({},{scale})", (bits >> 16) & 0xffff)
        }
        _ => name,
    }
}

/// The objects that a query in the body of a function refers to, as for a view, each one once.
pub(super) fn function_references(query: &Query) -> Vec<ObjRef> {
    let mut found = Vec::new();
    query_references(query, &[], &mut found);
    let mut refs = Vec::new();
    for object in found {
        if !refs.contains(&object) {
            refs.push(object);
        }
    }
    refs
}

/// `find_expr_references_walker` for the query of a view: each relation of the query, each column that the query reads, each function that the query calls, and the relation or the type of each `regclass` or `regtype` constant, also in the subqueries. `outer` holds the queries outside `query`, from the nearest. A view in the query is one relation, and the query of the view does not count.
fn query_references(query: &Query, outer: &[&Query], refs: &mut Vec<ObjRef>) {
    let mut levels = vec![query];
    levels.extend_from_slice(outer);
    for relation in &query.relations {
        if relation.oid != 0 {
            refs.push(ObjRef::new(PG_CLASS, relation.oid));
        } else if let Some(sub) = &relation.subquery {
            query_references(sub, &levels, refs);
        }
    }
    for expr in query.exprs() {
        expr_references(expr, &levels, refs);
    }
}

/// The references of an expression of the first query of `levels`. A column of the whole row has no reference.
fn expr_references(expr: &Expr, levels: &[&Query], refs: &mut Vec<ObjRef>) {
    match (&expr.kind, expr.ty) {
        (ExprKind::Var(var), _) if var.attnum != 0 => {
            let relation = levels.get(var.levels_up).and_then(|q| q.relations.get(var.relation));
            if let Some(relation) = relation
                && relation.oid != 0
            {
                refs.push(ObjRef::column(relation.oid, var.attnum));
            }
        }
        (ExprKind::Const(Value::Oid(found)), oid::REGCLASS) => {
            refs.push(ObjRef::new(PG_CLASS, *found));
        }
        (ExprKind::Const(Value::Oid(found)), oid::REGTYPE) => {
            refs.push(ObjRef::new(PG_TYPE, *found));
        }
        (ExprKind::Func(f), _) => refs.push(ObjRef::new(PG_PROC, f.oid)),
        (ExprKind::SubLink(sub), _) => query_references(&sub.query, levels, refs),
        _ => {}
    }
    refs.extend(super::type_reference(expr));
    for child in expr.children() {
        expr_references(child, levels, refs);
    }
}
