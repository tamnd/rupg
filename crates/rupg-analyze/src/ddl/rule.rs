//! `DefineRule`: `CREATE RULE` and `DROP RULE`.
//!
//! The analyzer reads the condition and the actions of a rule as `transformRuleStmt` does, with the relation of the rule as `old` and as `new`, and runs the checks of `DefineQueryRewrite`. The catalog keeps the text of the statement and the schemas of `search_path`, and [`rule_tree`] reads the rule again for `pg_get_ruledef`. The executor does not apply the rules yet, because only `SELECT` runs.

use rupg_catalog::{NewRule, ObjRef, PG_REWRITE};
use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{CmdType, DropBehavior, DropStmt, Node, RuleStmt};

use super::comment::{name_list, not_owner, range_var};
use super::drop::owner_word;
use super::view::{expr_references, query_references};
use super::{Definer, Found, not_yet, relkind_detail, relname};
use crate::agg::Kind;
use crate::expr::{Expr, ExprKind};
use crate::select::Query;
use crate::{Analyzer, Env, Params, types};

/// The name of the `ON SELECT` rule of a view.
const RETURN: &str = "_RETURN";

/// The query trees of a rule, as `transformRuleStmt` makes them. Each query has the relation of the rule as its first two relations, `old` and `new`.
#[derive(Clone, Debug, Default)]
pub struct RuleTree {
    /// The range table of the condition: only `old` and `new`. `DO INSTEAD NOTHING` has this query as its action.
    pub base: Query,
    /// The condition of `WHERE`, or `None`.
    pub qual: Option<Expr>,
    /// The actions in order. `NOTHING` has no actions.
    pub actions: Vec<Query>,
}

/// The `ev_type` code of an event.
fn event_code(event: CmdType) -> u8 {
    match event {
        CmdType::CMD_SELECT => b'1',
        CmdType::CMD_UPDATE => b'2',
        CmdType::CMD_INSERT => b'3',
        _ => b'4',
    }
}

/// `rangeTableEntry_used` for the relation at the index of the query `depth` levels up: true when a column of the relation is in the query, in its subqueries or in its `WITH` queries.
fn query_uses(query: &Query, relation: usize, depth: usize) -> bool {
    query.all_exprs(depth).into_iter().any(|(e, d)| expr_uses(e, relation, d))
        || query.ctes.iter().any(|c| query_uses(&c.query, relation, depth + 1))
}

/// `rangeTableEntry_used` for an expression.
fn expr_uses(expr: &Expr, relation: usize, depth: usize) -> bool {
    expr.find(depth, &mut |e, d| match e.kind {
        ExprKind::Var(var) if var.relation == relation && var.levels_up == d => Some(()),
        _ => None,
    })
    .is_some()
}

/// An error of `DefineQueryRewrite` with `ERRCODE_INVALID_OBJECT_DEFINITION`.
fn invalid(message: impl Into<String>) -> Error {
    Error::new(SqlState::INVALID_OBJECT_DEFINITION, message)
}

impl Analyzer<'_> {
    /// `transformRuleStmt` for the relation `oid` with the name `name`.
    pub(crate) fn rule_tree(&mut self, stmt: &RuleStmt, oid: u32, name: &str) -> Result<RuleTree> {
        let visible = match stmt.event {
            CmdType::CMD_UPDATE => [true, true],
            CmdType::CMD_INSERT => [false, true],
            _ => [true, false],
        };
        self.rule_scope(oid, name, visible, true)?;
        let qual = match &stmt.whereClause {
            Some(node) => {
                let expr = self.with_kind(Kind::Where, |a| a.transform(Some(node)))?;
                Some(self.coerce_to_boolean(expr, "WHERE")?)
            }
            None => None,
        };
        let base =
            Query { relations: std::mem::take(&mut self.scope.relations), ..Query::default() };
        let mut actions = Vec::with_capacity(stmt.actions.len());
        for action in stmt.actions.iter().flatten() {
            let select = match action {
                Node::SelectStmt(select) => select,
                Node::NotifyStmt(_) if qual.is_some() => {
                    return Err(invalid(
                        "rules with WHERE conditions can only have SELECT, INSERT, UPDATE, or DELETE actions",
                    ));
                }
                Node::NotifyStmt(_) => return Err(not_yet("NOTIFY in a rule", None)),
                _ => return Err(not_yet("INSERT, UPDATE or DELETE in a rule", None)),
            };
            self.rule_scope(oid, name, [true, true], false)?;
            let query = self.select(select)?;
            if query.set_op.is_some() && qual.is_some() {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    "conditional UNION/INTERSECT/EXCEPT statements are not implemented",
                ));
            }
            let in_qual = |relation| qual.as_ref().is_some_and(|q| expr_uses(q, relation, 0));
            let has_old = query_uses(&query, 0, 0) || in_qual(0);
            let has_new = query_uses(&query, 1, 0) || in_qual(1);
            match stmt.event {
                CmdType::CMD_SELECT if has_old => {
                    return Err(invalid("ON SELECT rule cannot use OLD"));
                }
                CmdType::CMD_SELECT if has_new => {
                    return Err(invalid("ON SELECT rule cannot use NEW"));
                }
                CmdType::CMD_INSERT if has_old => {
                    return Err(invalid("ON INSERT rule cannot use OLD"));
                }
                CmdType::CMD_DELETE if has_new => {
                    return Err(invalid("ON DELETE rule cannot use NEW"));
                }
                _ => {}
            }
            for (relation, word) in [(0, "OLD"), (1, "NEW")] {
                if query.ctes.iter().any(|c| query_uses(&c.query, relation, 1)) {
                    return Err(Error::new(
                        SqlState::FEATURE_NOT_SUPPORTED,
                        format!("cannot refer to {word} within WITH query"),
                    ));
                }
            }
            actions.push(query);
        }
        Ok(RuleTree { base, qual, actions })
    }
}

/// The query trees of the rule of the catalog with the OID, from the text of the rule and the `search_path` that the rule keeps, or `None` when the catalog has no rule with the OID. `pg_get_ruledef` shows these queries.
pub fn rule_tree(env: &dyn Env, oid: u32) -> Result<Option<RuleTree>> {
    let Some(catalog) = env.catalog() else { return Ok(None) };
    let Some(rule) = catalog.rule(oid) else { return Ok(None) };
    let name = catalog.relation(rule.relation).map_or("", |r| r.name.as_str());
    let (list, _) = rupg_sql::parse(&rule.text).map_err(Error::from)?;
    let stmt = list.iter().flatten().find_map(|node| match node {
        Node::RawStmt(raw) => match raw.stmt.as_ref() {
            Some(Node::RuleStmt(stmt)) => Some(stmt),
            _ => None,
        },
        _ => None,
    });
    let Some(stmt) = stmt else { return Err(Error::internal("the text of a rule is not a rule")) };
    let mut an = Analyzer::new(env, &Params::default());
    an.path.clone_from(&rule.path);
    an.rule_tree(stmt, rule.relation, name).map(Some).map_err(|error| error.with_position(None))
}

impl Definer<'_, '_> {
    /// `DefineRule`: `transformRuleStmt`, then the checks of `DefineQueryRewrite`, then `InsertRule`. `text` is the text of the statement, which the catalog keeps.
    pub(super) fn create_rule(&mut self, stmt: &RuleStmt, text: &str) -> Result<()> {
        let rv = stmt.relation.as_deref().cloned().unwrap_or_default();
        let found = self.lookup_relation(&rv)?;
        let name = relname(&rv).to_string();
        let code = self.kind_of(found);
        let oid = match found {
            Found::User(oid) => oid,
            Found::Builtin(row) => row.oid,
        };
        if code == b'm' {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "rules on materialized views are not supported",
            ));
        }
        let tree = self.an.rule_tree(stmt, oid, &name)?;
        if !matches!(code, b'r' | b'm' | b'v' | b'p') {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("relation \"{name}\" cannot have rules"),
            )
            .with_detail(relkind_detail(code)));
        }
        if self.is_system(found) && !self.an.env.allow_system_table_mods() {
            return Err(Error::new(
                SqlState::INSUFFICIENT_PRIVILEGE,
                format!("permission denied: \"{name}\" is a system catalog"),
            ));
        }
        let owner = match found {
            Found::User(oid) => self.catalog.relation(oid).map_or(0, |r| r.owner),
            Found::Builtin(row) => row.owner,
        };
        if !self.is_superuser() && owner != self.user {
            return Err(not_owner(owner_word(code), &name));
        }
        let rulename = stmt.rulename.as_deref().unwrap_or_default();
        if stmt.event == CmdType::CMD_SELECT {
            self.check_select_rule(stmt, &tree, code, oid, &name)?;
        } else if rulename == RETURN {
            return Err(invalid(format!(
                "non-view rule for \"{name}\" must not be named \"{RETURN}\""
            )));
        }
        let Found::User(oid) = found else {
            return Err(not_yet("a rule on a system catalog", None));
        };
        let mut actions = Vec::new();
        if tree.actions.is_empty() {
            query_references(self.catalog, &tree.base, &[], &mut actions);
        }
        for action in &tree.actions {
            query_references(self.catalog, action, &[], &mut actions);
        }
        let mut qual = Vec::new();
        if let Some(expr) = &tree.qual {
            let context = tree.actions.first().unwrap_or(&tree.base);
            expr_references(self.catalog, expr, &[context], &mut qual);
        }
        let new = NewRule {
            name: rulename.to_string(),
            relation: oid,
            event: event_code(stmt.event),
            instead: stmt.instead,
            qualified: tree.qual.is_some(),
            text: text.to_string(),
            path: self.an.path.clone(),
        };
        self.catalog.create_rule(new, stmt.replace, &actions, &qual)?;
        Ok(())
    }

    /// The checks of `DefineQueryRewrite` for an `ON SELECT` rule. Only a view can have one, and it has its `_RETURN` rule, so a new rule is an error. rupg does not replace the `_RETURN` rule yet.
    fn check_select_rule(
        &self,
        stmt: &RuleStmt,
        tree: &RuleTree,
        code: u8,
        oid: u32,
        name: &str,
    ) -> Result<()> {
        if !matches!(code, b'v' | b'm') {
            return Err(Error::new(
                SqlState::WRONG_OBJECT_TYPE,
                format!("relation \"{name}\" cannot have ON SELECT rules"),
            )
            .with_detail(relkind_detail(code)));
        }
        if tree.actions.len() > 1 {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "multiple actions for rules on SELECT are not implemented",
            ));
        }
        let Some(query) = tree.actions.first().filter(|_| stmt.instead) else {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "rules on SELECT must have action INSTEAD SELECT",
            ));
        };
        if tree.qual.is_some() {
            return Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                "event qualifications are not implemented for rules on SELECT",
            ));
        }
        let columns = self.catalog.relation(oid).map(|r| r.columns.clone()).unwrap_or_default();
        let mut count = 0;
        for target in query.targets.iter().filter(|t| !t.junk) {
            count += 1;
            let Some(column) = columns.get(count - 1) else {
                return Err(invalid("SELECT rule's target list has too many entries"));
            };
            let (expr, attname) = (&target.expr, &column.name);
            if target.name != *attname {
                return Err(invalid(format!(
                    "SELECT rule's target entry {count} has different column name from column \"{attname}\""
                ))
                .with_detail(format!("SELECT target entry is named \"{}\".", target.name)));
            }
            if expr.ty != column.ty {
                return Err(invalid(format!(
                    "SELECT rule's target entry {count} has different type from column \"{attname}\""
                ))
                .with_detail(format!(
                    "SELECT target entry has type {}, but column has type {}.",
                    types::name(expr.ty),
                    types::name(column.ty)
                )));
            }
            if expr.typmod != column.typmod && expr.typmod != -1 && column.typmod != -1 {
                return Err(invalid(format!(
                    "SELECT rule's target entry {count} has different size from column \"{attname}\""
                ))
                .with_detail(format!(
                    "SELECT target entry has type {}, but column has type {}.",
                    types::with_typmod(expr.ty, expr.typmod),
                    types::with_typmod(column.ty, column.typmod)
                )));
            }
        }
        if count != columns.len() {
            return Err(invalid("SELECT rule's target list has too few entries"));
        }
        if !stmt.replace {
            return Err(Error::new(
                SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE,
                format!("\"{name}\" is already a view"),
            ));
        }
        if stmt.rulename.as_deref() != Some(RETURN) {
            return Err(invalid(format!("view rule for \"{name}\" must be named \"{RETURN}\"")));
        }
        Err(not_yet("CREATE OR REPLACE RULE of the _RETURN rule of a view", None))
    }

    /// `RemoveObjects` for `DROP RULE`: the rule of the relation with the name, with the check of `check_object_ownership`.
    pub(super) fn drop_rule(&mut self, stmt: &DropStmt) -> Result<()> {
        let mut objects = Vec::new();
        for item in stmt.objects.iter().flatten() {
            let names = name_list(Some(item));
            let Some((&rulename, parent)) = names.split_last() else { continue };
            let rv = range_var(parent)?;
            let found = match self.lookup_relation(&rv) {
                Ok(found) => found,
                Err(error)
                    if stmt.missing_ok
                        && matches!(
                            error.state(),
                            SqlState::UNDEFINED_TABLE | SqlState::UNDEFINED_SCHEMA
                        ) =>
                {
                    let message = error.message().to_string();
                    self.notice(SqlState::SUCCESSFUL_COMPLETION, format!("{message}, skipping"));
                    continue;
                }
                Err(error) => return Err(error),
            };
            let Found::User(relation) = found else {
                return Err(not_yet("DROP RULE of a system catalog", None));
            };
            let rel = self.catalog.relation(relation);
            let relation_name = rel.map_or(String::new(), |r| r.name.clone());
            let owner = rel.map_or(0, |r| r.owner);
            let rule = match rel.and_then(|r| r.view.as_ref()) {
                Some(view) if rulename == RETURN => Some(view.rule),
                _ => self.catalog.rule_by_name(relation, rulename).map(|r| r.oid),
            };
            let Some(rule) = rule else {
                if stmt.missing_ok {
                    self.notice(
                        SqlState::SUCCESSFUL_COMPLETION,
                        format!(
                            "rule \"{rulename}\" for relation \"{}\" does not exist, skipping",
                            parent.join(".")
                        ),
                    );
                    continue;
                }
                return Err(Error::new(
                    SqlState::UNDEFINED_OBJECT,
                    format!("rule \"{rulename}\" for relation \"{relation_name}\" does not exist"),
                ));
            };
            if !self.is_superuser() && owner != self.user {
                return Err(not_owner("relation", &relation_name));
            }
            objects.push(ObjRef::new(PG_REWRITE, rule));
        }
        self.perform(&objects, stmt.behavior == DropBehavior::DROP_CASCADE)
    }
}
