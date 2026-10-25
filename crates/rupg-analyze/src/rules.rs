//! `QueryRewrite`, `RewriteQuery` and `fireRules` of `rewriteHandler.c`: the statements that the rules `ON INSERT`, `ON UPDATE` and `ON DELETE` of a table give for a write of the table.
//!
//! PostgreSQL rewrites the query trees. rupg rewrites the raw statements: an action of a rule gets the statement of the user, with no `RETURNING`, as the subquery `*rule*` first in its `FROM`, and the analysis reads that subquery as the rows that the statement writes. The names `old` and `new` of the action read the old row and the new row of each of these rows, as `rewriteRuleAction` puts the range table of the statement in the action and the values of the new row in place of `NEW`. The condition of the rule goes to the `WHERE` of the action, and the statement of the user gets the inverse of the condition of each rule `DO INSTEAD` with a condition, as `CopyAndAddInvertedQual` adds it.

use rupg_catalog::Rule;
use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{
    A_Star, Alias, BoolExpr, BoolExprType, CmdType, ColumnRef, Node, OnConflictAction,
    RangeSubselect, RangeVar, ResTarget, ReturningClause, RuleStmt, SelectStmt, SetOperation,
    SubLink, SubLinkType, WithClause,
};
use rupg_types::oid;

use crate::agg::Kind;
use crate::expr::{BoolOp, BoolTest, Expr, ExprKind, Var};
use crate::from::Column;
use crate::insert::Generated;
use crate::walk::{self, Walker};
use crate::{Analyzer, Env, Params, Query};

/// The name of the subquery of an action that gives the rows of the statement of the user.
const RULE: &str = "*rule*";

/// A statement of the rewrite of a write by the rules of its table, in the order that `RewriteQuery` gives.
#[derive(Clone, Debug)]
pub struct Product {
    /// The statement: the statement of the user, or an action of a rule with the statement of the user in its `FROM`.
    pub stmt: Node,
    /// `canSetTag`: true for the statement whose tag and number of rows are the result of the command.
    pub sets_tag: bool,
    /// True for an action of a rule, whose errors have no place in the statement of the user.
    pub action: bool,
    /// The conditions of the rules `DO INSTEAD` with `WHERE`. The statement writes only the rows for which no condition is true.
    inverted: Vec<Node>,
    /// The OID of the target table when the rewrite fired its rules for this statement.
    fired: Option<u32>,
    source: Source,
    /// The list of `RETURNING` of the statement of the user, its name of the target relation and the OID of the relation, for the action whose list of `RETURNING` gives the rows.
    returning: Option<Returning>,
}

impl Product {
    /// True for the action of a rule whose list of `RETURNING` gives the rows of the statement of the user.
    pub fn gives_rows(&self) -> bool {
        self.returning.is_some()
    }
}

/// The list of `RETURNING` of the statement of the user, the name of its target relation and the OID of the relation.
pub(crate) type Returning = (String, ReturningClause, u32);

/// `querySource`: where a statement of the rewrite comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    /// `QSRC_ORIGINAL`: the statement of the user.
    Original,
    /// `QSRC_INSTEAD_RULE`: an action of a rule `DO INSTEAD` with no condition.
    Instead,
    /// `QSRC_QUAL_INSTEAD_RULE`: an action of a rule `DO INSTEAD` with a condition.
    QualInstead,
    /// `QSRC_NON_INSTEAD_RULE`: an action of a rule `DO ALSO`.
    Also,
}

/// A statement of the rewrite before the choice of the statement that sets the tag.
struct Item {
    stmt: Node,
    source: Source,
    inverted: Vec<Node>,
    fired: Option<u32>,
    returning: Option<Returning>,
}

/// The state of the analysis of the rules of a write.
#[derive(Debug, Default)]
pub(crate) struct State {
    /// True while the analyzer reads the actions of `CREATE RULE`, whose target tables can have rules.
    pub(crate) defining: bool,
    /// The OID of the target table of the statement whose rules the rewrite fired.
    pub(crate) fired: Option<u32>,
    /// True when the next write gives the rows of the subquery `*rule*`: the new row of each row, then the old row, with no write.
    pub(crate) rows: bool,
    /// The columns of the target table of the last write that gave its rows.
    pub(crate) columns: Vec<Column>,
    /// The conditions that the next write inverts, which read `old` and `new`.
    pub(crate) inverted: Vec<Node>,
    /// The defaults of the target columns of an action `INSERT` whose `VALUES` the rewrite made the subquery `*VALUES*`, for `DEFAULT` in its rows, as `rewriteValuesRTE` gives them.
    pub(crate) defaults: Option<Vec<Expr>>,
    /// [`State::defaults`] while the analyzer reads the subquery `*VALUES*`, for its `VALUES` list.
    pub(crate) values: Option<Vec<Expr>>,
    /// The list of `RETURNING` of the statement of the user while the analyzer reads the action of a rule that gives its rows.
    pub(crate) returning: Option<Returning>,
}

/// The alias of the subquery that the rewrite makes of the `VALUES` of an action `INSERT`.
pub(crate) const VALUES_NAME: &str = "*VALUES*";

/// True when the `SELECT` of an `INSERT` reads the subquery `*VALUES*` that the rewrite made, and the `VALUES` of the subquery has `DEFAULT`.
pub(crate) fn has_values_default(select: &SelectStmt) -> bool {
    select.fromClause.iter().flatten().any(|item| match item {
        Node::RangeSubselect(r) if r.lateral => {
            let named = r.alias.as_ref().and_then(|a| a.aliasname.as_deref()) == Some(VALUES_NAME);
            named && matches!(&r.subquery, Some(Node::SelectStmt(s)) if has_default(s))
        }
        _ => false,
    })
}

/// True when a row of the `VALUES` of the `SELECT` has `DEFAULT`.
fn has_default(select: &SelectStmt) -> bool {
    select.valuesLists.iter().flatten().any(|row| match row {
        Node::List(items) => items.iter().flatten().any(|n| matches!(n, Node::SetToDefault(_))),
        _ => false,
    })
}

/// The kind of a write and its target table.
fn target_of(stmt: &Node) -> Option<(CmdType, &RangeVar)> {
    match stmt {
        Node::InsertStmt(s) => Some((CmdType::CMD_INSERT, s.relation.as_deref()?)),
        Node::UpdateStmt(s) => Some((CmdType::CMD_UPDATE, s.relation.as_deref()?)),
        Node::DeleteStmt(s) => Some((CmdType::CMD_DELETE, s.relation.as_deref()?)),
        _ => None,
    }
}

/// True for a write, which the subquery `*rule*` has.
pub(crate) fn is_write(node: &Node) -> bool {
    matches!(node, Node::InsertStmt(_) | Node::UpdateStmt(_) | Node::DeleteStmt(_))
}

/// The `ev_type` code of an event.
fn event_code(event: CmdType) -> u8 {
    match event {
        CmdType::CMD_UPDATE => b'2',
        CmdType::CMD_INSERT => b'3',
        _ => b'4',
    }
}

/// The word of an event in the errors.
fn event_word(event: CmdType) -> &'static str {
    match event {
        CmdType::CMD_UPDATE => "UPDATE",
        CmdType::CMD_INSERT => "INSERT",
        _ => "DELETE",
    }
}

/// True when the rule fires in the mode of `session_replication_role`, as `matchLocks` skips the rules that are disabled or that fire only in the other mode.
pub(crate) fn fires(rule: &Rule, env: &dyn Env) -> bool {
    rupg_catalog::fires(rule.enabled, env.replica())
}

/// An error for a rule that rupg does not take yet.
fn not_yet(what: &str) -> Error {
    Error::new(SqlState::FEATURE_NOT_SUPPORTED, format!("{what} is not supported yet"))
}

/// The walk that finds a name `old` or `new` of a rule.
struct UsesRow(bool);

impl Walker for UsesRow {
    fn node(&mut self, node: &Node) -> bool {
        if let Node::ColumnRef(c) = node
            && c.fields.len() > 1
            && let Some(Some(Node::String(first))) = c.fields.first()
            && matches!(&**first, "old" | "new")
        {
            self.0 = true;
            return true;
        }
        walk::walk(self, node)
    }

    fn select(&mut self, stmt: &SelectStmt) -> bool {
        walk::select(self, stmt)
    }
}

/// True when the node names `old` or `new`.
fn uses_row(node: &Node) -> bool {
    let mut w = UsesRow(false);
    w.node(node);
    w.0
}

/// The walk that finds a column name with no name of a relation.
struct Bare(bool);

impl Walker for Bare {
    fn node(&mut self, node: &Node) -> bool {
        if let Node::ColumnRef(c) = node
            && let [Some(Node::String(_))] = c.fields.as_slice()
        {
            self.0 = true;
            return true;
        }
        walk::walk(self, node)
    }

    fn select(&mut self, stmt: &SelectStmt) -> bool {
        walk::select(self, stmt)
    }
}

/// The condition of a rule as the action reads it. `transformRuleStmt` reads the condition of a rule `ON INSERT` with the columns of `new` visible, and of a rule `ON DELETE` with the columns of `old` visible, so a column name with no name of a relation is a column of the row. In the action these names would read the tables of the action, so a condition with such a name becomes `(SELECT condition FROM (SELECT new.*) AS new)`, whose `FROM` gives the columns of the row first.
fn row_qual(qual: &Node, event: CmdType) -> Node {
    let name = match event {
        CmdType::CMD_INSERT => "new",
        CmdType::CMD_DELETE => "old",
        _ => return qual.clone(),
    };
    let mut w = Bare(false);
    w.node(qual);
    if !w.0 {
        return qual.clone();
    }
    let target = |val| {
        Some(Node::ResTarget(Box::new(ResTarget {
            name: None,
            indirection: Vec::new(),
            val: Some(val),
            location: -1,
        })))
    };
    let star = ColumnRef {
        fields: vec![Some(Node::String(name.into())), Some(Node::A_Star(Box::new(A_Star {})))],
        location: -1,
    };
    let row = SelectStmt {
        targetList: vec![target(Node::ColumnRef(Box::new(star)))],
        ..SelectStmt::default()
    };
    let select = SelectStmt {
        targetList: vec![target(qual.clone())],
        fromClause: vec![Some(Node::RangeSubselect(Box::new(RangeSubselect {
            lateral: false,
            subquery: Some(Node::SelectStmt(Box::new(row))),
            alias: Some(Box::new(Alias { aliasname: Some(name.into()), colnames: Vec::new() })),
        })))],
        ..SelectStmt::default()
    };
    Node::SubLink(Box::new(SubLink {
        subLinkType: SubLinkType::EXPR_SUBLINK,
        subselect: Some(Node::SelectStmt(Box::new(select))),
        location: -1,
        ..SubLink::default()
    }))
}

/// True when a query of the `WITH` clause changes data.
fn writes_in_with(with: Option<&WithClause>) -> bool {
    with.is_some_and(|w| {
        w.ctes.iter().flatten().any(|cte| match cte {
            Node::CommonTableExpr(c) => {
                c.ctequery.as_ref().is_some_and(|q| is_write(q) || matches!(q, Node::MergeStmt(_)))
            }
            _ => false,
        })
    })
}

/// The statement with no `RETURNING`, and its `WITH` clause, and true when it has `RETURNING`, `ON CONFLICT` or a condition of `WHERE`, and its parts of `FROM` or `USING`. `clause` is the list of `RETURNING` and the name of the target relation.
struct Parts {
    base: Node,
    returning: bool,
    clause: Option<(String, ReturningClause)>,
    on_conflict: bool,
    filter: bool,
    from: bool,
    with: Option<WithClause>,
}

fn parts(stmt: &Node) -> Parts {
    let mut base = stmt.clone();
    let clause = match stmt {
        Node::InsertStmt(s) => s.returningClause.as_deref().zip(s.relation.as_deref()),
        Node::UpdateStmt(s) => s.returningClause.as_deref().zip(s.relation.as_deref()),
        Node::DeleteStmt(s) => s.returningClause.as_deref().zip(s.relation.as_deref()),
        _ => None,
    };
    let clause = clause.map(|(clause, rv)| {
        let alias = rv.alias.as_ref().and_then(|a| a.aliasname.clone());
        let name = alias.or_else(|| rv.relname.clone()).unwrap_or_default();
        (name.to_string(), clause.clone())
    });
    let (returning, on_conflict, filter, from, with) = match &mut base {
        Node::InsertStmt(s) => (
            s.returningClause.take().is_some(),
            s.onConflictClause.is_some(),
            false,
            false,
            s.withClause.as_deref().cloned(),
        ),
        Node::UpdateStmt(s) => (
            s.returningClause.take().is_some(),
            false,
            s.whereClause.is_some(),
            !s.fromClause.is_empty(),
            s.withClause.as_deref().cloned(),
        ),
        Node::DeleteStmt(s) => (
            s.returningClause.take().is_some(),
            false,
            s.whereClause.is_some(),
            !s.usingClause.is_empty(),
            s.withClause.as_deref().cloned(),
        ),
        _ => (false, false, false, false, None),
    };
    Parts { base, returning, clause, on_conflict, filter, from, with }
}

/// The `CREATE RULE` statement of the text that the catalog keeps for a rule.
fn rule_stmt(rule: &Rule) -> Result<RuleStmt> {
    let (list, _) = rupg_sql::parse(&rule.text).map_err(Error::from)?;
    list.iter()
        .flatten()
        .find_map(|node| match node {
            Node::RawStmt(raw) => match raw.stmt.as_ref() {
                Some(Node::RuleStmt(stmt)) => Some((**stmt).clone()),
                _ => None,
            },
            _ => None,
        })
        .ok_or_else(|| Error::internal("the text of a rule is not a rule"))
}

/// `make_and_qual`: the condition `node` and the condition `more`.
fn and(node: Option<Node>, more: Option<&Node>) -> Option<Node> {
    match (node, more) {
        (Some(node), Some(more)) => Some(Node::BoolExpr(Box::new(BoolExpr {
            boolop: BoolExprType::AND_EXPR,
            args: vec![Some(node), Some(more.clone())],
            location: -1,
        }))),
        (node, more) => node.or_else(|| more.cloned()),
    }
}

/// True when the write has a list of `RETURNING`.
fn has_returning(stmt: &Node) -> bool {
    match stmt {
        Node::InsertStmt(s) => s.returningClause.is_some(),
        Node::UpdateStmt(s) => s.returningClause.is_some(),
        Node::DeleteStmt(s) => s.returningClause.is_some(),
        _ => false,
    }
}

/// `rewriteRuleAction`: the action with the statement of the user as `*rule*` first in its `FROM`, and the condition of the rule in its `WHERE`. The action keeps its list of `RETURNING` only when the statement of the user has one. An action that reads neither `old` nor `new`, with no condition that reads them, runs once for the statement, as the target table then leaves the join tree of the action.
fn rule_action(parts: &Parts, action: &Node, qual: Option<&Node>, event: CmdType) -> Result<Node> {
    let qual = qual.map(|q| row_qual(q, event));
    let qual = qual.as_ref();
    let needs_rows = uses_row(action)
        || qual.is_some_and(uses_row)
        || event == CmdType::CMD_INSERT
        || parts.filter;
    if !needs_rows && parts.from {
        return Err(not_yet(
            "a rule action that does not read OLD or NEW for a statement with FROM or USING",
        ));
    }
    let rows = needs_rows.then(|| {
        Node::RangeSubselect(Box::new(RangeSubselect {
            lateral: false,
            subquery: Some(parts.base.clone()),
            alias: Some(Box::new(Alias { aliasname: Some(RULE.into()), colnames: Vec::new() })),
        }))
    });
    let mut action = action.clone();
    match &mut action {
        Node::InsertStmt(s) => {
            // `rewriteRuleAction`: the statement must have `RETURNING` for an action with `DO SELECT`.
            let select = s
                .onConflictClause
                .as_deref()
                .is_some_and(|c| c.action == OnConflictAction::ONCONFLICT_SELECT);
            if select && !parts.returning {
                return Err(Error::new(
                    SqlState::SYNTAX_ERROR,
                    "ON CONFLICT DO SELECT requires a RETURNING clause",
                )
                .with_detail(
                    "A rule action is INSERT ... ON CONFLICT DO SELECT, which requires a RETURNING clause.",
                ));
            }
            if !parts.returning {
                s.returningClause = None;
            }
            if rows.is_none() && qual.is_none() {
                return Ok(action);
            }
            // DEFAULT VALUES becomes a SELECT with no columns, so that every
            // column takes its default for each row that the qual keeps.
            let select = s.selectStmt.get_or_insert_with(|| Node::SelectStmt(Box::default()));
            let Node::SelectStmt(select) = select else {
                return Err(not_yet("a rule action INSERT that is not a SELECT"));
            };
            if select.op != SetOperation::SETOP_NONE {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    "conditional UNION/INTERSECT/EXCEPT statements are not implemented",
                ));
            }
            if !select.valuesLists.is_empty() {
                values_to_select(select, true)?;
            }
            if let Some(rows) = rows {
                lateral_items(&mut select.fromClause);
                select.fromClause.insert(0, Some(rows));
            }
            select.whereClause = and(select.whereClause.take(), qual);
        }
        Node::UpdateStmt(s) => {
            if !parts.returning {
                s.returningClause = None;
            }
            if let Some(rows) = rows {
                lateral_items(&mut s.fromClause);
                s.fromClause.insert(0, Some(rows));
            }
            s.whereClause = and(s.whereClause.take(), qual);
        }
        Node::DeleteStmt(s) => {
            if !parts.returning {
                s.returningClause = None;
            }
            if let Some(rows) = rows {
                lateral_items(&mut s.usingClause);
                s.usingClause.insert(0, Some(rows));
            }
            s.whereClause = and(s.whereClause.take(), qual);
        }
        // A utility action runs once, with no regard to the condition or the rows of the statement.
        Node::NotifyStmt(_) => {}
        Node::SelectStmt(s) => {
            if rows.is_some() && s.op != SetOperation::SETOP_NONE {
                return Err(not_yet("a rule action UNION that reads OLD or NEW"));
            }
            if rows.is_some() && !s.valuesLists.is_empty() {
                // The columns of `VALUES` keep their names `column1` and on.
                values_to_select(s, false)?;
            }
            if let Some(rows) = rows {
                lateral_items(&mut s.fromClause);
                s.fromClause.insert(0, Some(rows));
            }
            s.whereClause = and(s.whereClause.take(), qual);
        }
        _ => return Err(Error::internal("a rule action that is not a statement")),
    }
    Ok(action)
}

/// The items of `FROM` of an action with `*rule*` first, with each subquery and each function `LATERAL`. `rewriteRuleAction` puts the values of `new` in place of its names, so a subquery of the action can read `new` and `old`. Here they read `*rule*`, which only a `LATERAL` item can read. `CREATE RULE` read the action without `LATERAL`, so this changes no other name.
fn lateral_items(list: &mut [Option<Node>]) {
    for item in list.iter_mut().flatten() {
        lateral_item(item);
    }
}

fn lateral_item(item: &mut Node) {
    match item {
        Node::RangeSubselect(r) => r.lateral = true,
        Node::RangeFunction(r) => r.lateral = true,
        Node::JoinExpr(j) => {
            for side in [&mut j.larg, &mut j.rarg].into_iter().flatten() {
                lateral_item(side);
            }
        }
        _ => {}
    }
}

/// The `VALUES` of an action as a `SELECT` that can have `FROM` and `WHERE`: the rows are a `LATERAL` subquery. With `inline`, one row with no `DEFAULT` gives the list of `SELECT`, which is good for an `INSERT`, as its columns have no names.
fn values_to_select(select: &mut SelectStmt, inline: bool) -> Result<()> {
    if inline
        && let [Some(Node::List(items))] = select.valuesLists.as_slice()
        && !has_default(select)
    {
        select.targetList = items
            .iter()
            .map(|item| {
                Some(Node::ResTarget(Box::new(ResTarget {
                    name: None,
                    indirection: Vec::new(),
                    val: item.clone(),
                    location: -1,
                })))
            })
            .collect();
        select.valuesLists.clear();
        return Ok(());
    }
    let values = SelectStmt {
        valuesLists: std::mem::take(&mut select.valuesLists),
        ..SelectStmt::default()
    };
    let name = VALUES_NAME;
    select.fromClause.push(Some(Node::RangeSubselect(Box::new(RangeSubselect {
        lateral: true,
        subquery: Some(Node::SelectStmt(Box::new(values))),
        alias: Some(Box::new(Alias { aliasname: Some(name.into()), colnames: Vec::new() })),
    }))));
    let star = ColumnRef {
        fields: vec![Some(Node::String(name.into())), Some(Node::A_Star(Box::new(A_Star {})))],
        location: -1,
    };
    select.targetList = vec![Some(Node::ResTarget(Box::new(ResTarget {
        name: None,
        indirection: Vec::new(),
        val: Some(Node::ColumnRef(Box::new(star))),
        location: -1,
    })))];
    Ok(())
}

/// `QueryRewrite` for a write: the statements of the rewrite by the rules of the target table, in order, or `None` when no rule fires.
///
/// # Errors
///
/// The errors of `RewriteQuery`: a rule that fires itself again, `RETURNING` with a rule `DO INSTEAD`, and `ON CONFLICT` for a table with rules. `0A000` for an action that rupg does not take yet.
pub fn query_rewrite(stmt: &Node, env: &dyn Env) -> Result<Option<Vec<Product>>> {
    let Some(catalog) = env.catalog() else { return Ok(None) };
    if !catalog.rules().any(|r| r.event != b'1') || target_of(stmt).is_none() {
        return Ok(None);
    }
    let mut events = Vec::new();
    let Some(items) = rewrite_query(stmt, Source::Original, env, &mut events)? else {
        return Ok(None);
    };
    // The statement of the user sets the tag, or else the last action of a rule `DO INSTEAD` of the same kind of write.
    let kind = target_of(stmt).map(|(kind, _)| kind);
    let tag = items.iter().position(|i| i.source == Source::Original).or_else(|| {
        items.iter().rposition(|i| {
            matches!(i.source, Source::Instead | Source::QualInstead)
                && target_of(&i.stmt).map(|(k, _)| k) == kind
        })
    });
    let products = items
        .into_iter()
        .enumerate()
        .map(|(n, item)| Product {
            stmt: item.stmt,
            sets_tag: Some(n) == tag,
            action: item.source != Source::Original,
            inverted: item.inverted,
            fired: item.fired,
            source: item.source,
            returning: item.returning,
        })
        .collect();
    Ok(Some(products))
}

/// The checks of `BeginCopyTo` on the rewrite of the query of `COPY` by the rules of its table: the error for a rewrite that is not one statement.
pub fn copy_rules_error(products: &[Product]) -> Option<Error> {
    let text = match products {
        [] => "DO INSTEAD NOTHING rules are not supported for COPY",
        [_] => return None,
        _ => products
            .iter()
            .find_map(|p| match p.source {
                Source::QualInstead => {
                    Some("conditional DO INSTEAD rules are not supported for COPY")
                }
                Source::Also => Some("DO ALSO rules are not supported for COPY"),
                _ => None,
            })
            .unwrap_or("multi-statement DO INSTEAD rules are not supported for COPY"),
    };
    Some(Error::new(SqlState::FEATURE_NOT_SUPPORTED, text))
}

/// `RewriteQuery` for a write: `None` when no rule of the target table fires for it.
fn rewrite_query(
    stmt: &Node,
    source: Source,
    env: &dyn Env,
    events: &mut Vec<(u32, CmdType)>,
) -> Result<Option<Vec<Item>>> {
    let Some(catalog) = env.catalog() else { return Ok(None) };
    let Some((event, rv)) = target_of(stmt) else { return Ok(None) };
    let Some(oid) = Analyzer::new(env, &Params::default()).lookup_relation(rv)? else {
        return Ok(None);
    };
    let name = catalog.relation(oid).map_or("", |r| r.name.as_str());
    let code = event_code(event);
    let mut rules: Vec<&Rule> =
        catalog.rules().filter(|r| r.relation == oid && r.event == code && fires(r, env)).collect();
    rules.sort_by(|a, b| a.name.cmp(&b.name));
    let parts = parts(stmt);
    let has_update = catalog.rules().any(|r| r.relation == oid && r.event == b'2');
    if parts.on_conflict && (!rules.is_empty() || has_update) {
        return Err(Error::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            "INSERT with ON CONFLICT clause cannot be used with table that has INSERT or UPDATE rules",
        ));
    }
    if rules.is_empty() {
        return Ok(None);
    }
    if writes_in_with(parts.with.as_ref()) {
        return Err(not_yet("a statement in WITH that changes data for a table with rules"));
    }
    let mut instead = false;
    let mut inverted: Option<Vec<Node>> = None;
    let mut products = Vec::new();
    for rule in rules {
        let rule_stmt = rule_stmt(rule)?;
        let qual = rule_stmt.whereClause.as_ref();
        let action_source = match (rule.instead, qual) {
            (true, Some(_)) => Source::QualInstead,
            (true, None) => {
                instead = true;
                Source::Instead
            }
            (false, _) => Source::Also,
        };
        // `CopyAndAddInvertedQual`: the statement of the user writes only the rows for which the condition of no rule `DO INSTEAD` is true.
        if let (Source::QualInstead, Some(qual)) = (action_source, qual)
            && !instead
        {
            inverted.get_or_insert_default().push(qual.clone());
        }
        for action in rule_stmt.actions.iter().flatten() {
            products.push((rule_action(&parts, action, qual, event)?, action_source));
        }
    }
    let mut result = Vec::new();
    // `RewriteQuery`: only one action can give the rows of `RETURNING`.
    let mut returning = false;
    if !products.is_empty() {
        if events.contains(&(oid, event)) {
            return Err(Error::new(
                SqlState::INVALID_OBJECT_DEFINITION,
                format!("infinite recursion detected in rules for relation \"{name}\""),
            ));
        }
        events.push((oid, event));
        for (product, source) in products {
            let gives_rows = has_returning(&product);
            if gives_rows && returning {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    "cannot have RETURNING lists in multiple rules",
                ));
            }
            returning |= gives_rows;
            match rewrite_query(&product, source, env, events)? {
                Some(_) if gives_rows => {
                    return Err(not_yet("RETURNING in a rule action that rules rewrite"));
                }
                Some(items) => result.extend(items),
                None => {
                    let returning = gives_rows
                        .then(|| parts.clause.clone().map(|(name, clause)| (name, clause, oid)))
                        .flatten();
                    let fired = None;
                    result.push(Item {
                        stmt: product,
                        source,
                        inverted: Vec::new(),
                        fired,
                        returning,
                    })
                }
            }
        }
        events.pop();
    }
    if (instead || inverted.is_some()) && parts.returning && !returning {
        let word = event_word(event);
        return Err(Error::new(
            SqlState::FEATURE_NOT_SUPPORTED,
            format!("cannot perform {word} RETURNING on relation \"{name}\""),
        )
        .with_hint(format!(
            "You need an unconditional ON {word} DO INSTEAD rule with a RETURNING clause."
        )));
    }
    // The statement of the user comes first for an `INSERT`, and last for an `UPDATE` or a `DELETE`.
    if !instead {
        let item = Item {
            stmt: stmt.clone(),
            source,
            inverted: inverted.unwrap_or_default(),
            fired: Some(oid),
            returning: None,
        };
        if event == CmdType::CMD_INSERT {
            result.insert(0, item);
        } else {
            result.push(item);
        }
    }
    Ok(Some(result))
}

/// [`crate::analyze`] for a statement of [`query_rewrite`].
///
/// # Errors
///
/// The errors of the analysis. An error of an action of a rule has no place.
pub fn analyze_product(product: &Product, env: &dyn Env, params: &Params) -> Result<Query> {
    let mut analyzer = Analyzer::new(env, params);
    analyzer.rule.fired = product.fired;
    analyzer.rule.inverted.clone_from(&product.inverted);
    analyzer.rule.returning.clone_from(&product.returning);
    let query = crate::analyze_in(analyzer, &product.stmt, None);
    if product.action { query.map_err(|error| error.with_position(None)) } else { query }
}

/// The expression with the value of each column of `row` in place of the column of the relation 0, as `ReplaceVarsFromTargetList` puts the new row in a generation expression.
pub(crate) fn replace_row(expr: &mut Expr, row: &[Expr]) {
    expr.each_mut(0, &mut |e, depth| {
        let ExprKind::Var(var) = e.kind else { return false };
        if var.levels_up != depth || var.relation != 0 {
            return false;
        }
        let index = usize::try_from(var.attnum).ok().and_then(|n| n.checked_sub(1));
        let Some(value) = index.and_then(|i| row.get(i)) else { return false };
        let mut value = value.clone();
        value.raise(depth);
        *e = value;
        true
    });
}

/// The names and the values of the columns of a row of the target table, with no dropped column.
pub(crate) fn named_row(columns: &[Column], row: Vec<Expr>) -> Vec<(String, Expr)> {
    columns
        .iter()
        .zip(row)
        .filter(|(c, _)| !c.is_dropped())
        .map(|(c, e)| (c.name.clone(), e))
        .collect()
}

/// The columns of the relation 0 as the values of a row.
pub(crate) fn relation_row(columns: &[Column], relation: usize, offset: usize) -> Vec<Expr> {
    columns
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let attnum = i16::try_from(offset + i + 1).unwrap_or(i16::MAX);
            let mut expr = Expr::new(ExprKind::Var(Var { relation, attnum, levels_up: 0 }), c.ty);
            expr.typmod = c.typmod;
            expr
        })
        .collect()
}

/// The new row with the value of each generated column of `generated`, from the values of the other columns. The rules see the value of a virtual generated column too, as `get_generated_columns` gives it.
pub(crate) fn with_generated(mut row: Vec<Expr>, generated: &[Generated]) -> Vec<Expr> {
    let values = row.clone();
    for (index, expr, _) in generated {
        let mut expr = expr.clone();
        replace_row(&mut expr, &values);
        if let Some(slot) = row.get_mut(*index) {
            *slot = expr;
        }
    }
    row
}

/// `AddQual`: the condition and the conditions of `more`.
pub(crate) fn and_filter(filter: Option<Expr>, more: Vec<Expr>) -> Option<Expr> {
    let mut parts: Vec<Expr> = filter.into_iter().chain(more).collect();
    match parts.len() {
        0 => None,
        1 => parts.pop(),
        _ => Some(Expr::new(ExprKind::Bool(BoolOp::And, parts), oid::BOOL)),
    }
}

impl Analyzer<'_> {
    /// `CopyAndAddInvertedQual`: the conditions of the rules `DO INSTEAD` that a write inverts, each as `IS NOT TRUE`, with `old` as the old row and `new` as the new row of the write.
    pub(crate) fn inverted_quals(
        &mut self,
        quals: &[Node],
        old: Option<Vec<(String, Expr)>>,
        new: Option<Vec<(String, Expr)>>,
    ) -> Result<Vec<Expr>> {
        let outer = std::mem::take(&mut self.scope);
        let result = self.inverted_in_scope(quals, old, new);
        self.scope = outer;
        result
    }

    fn inverted_in_scope(
        &mut self,
        quals: &[Node],
        old: Option<Vec<(String, Expr)>>,
        new: Option<Vec<(String, Expr)>>,
    ) -> Result<Vec<Expr>> {
        // `transformRuleStmt` reads the condition with the columns of `old` and `new` visible, so a column with no name of a relation is a column of the row of the event.
        for (name, row) in [("old", old), ("new", new)] {
            if let Some(row) = row {
                let mut item = self.rule_item(name, row);
                item.show_columns();
                self.push_item(item);
            }
        }
        let mut out = Vec::with_capacity(quals.len());
        for qual in quals {
            let expr = self.with_kind(Kind::Where, |a| a.transform(Some(qual)))?;
            let expr = self.coerce_to_boolean(expr, "WHERE")?;
            out.push(Expr::new(
                ExprKind::BooleanTest(Box::new(expr), BoolTest::IsNotTrue),
                oid::BOOL,
            ));
        }
        Ok(out)
    }
}
