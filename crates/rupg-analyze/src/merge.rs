//! `transformMergeStmt` of `parse_merge.c`, with `transformMergeSupportFunc` of `parse_expr.c` and the join of `transform_MERGE_to_join` of `prepjointree.c`.
//!
//! The query of a `MERGE` joins the source and the target table, which is the relation 0. Each row of the query gives the columns of the target table, the OID of the leaf partition of the row when the target table is a partitioned table, the ID of the version of the row of the target table, a marker that is null when the row has no source row, and then the carried values: the columns of the source that the `WHEN` clauses and `RETURNING` read. The ID of the version is null when the row has no target row.
//!
//! Each `WHEN` clause has a small query that reads two relations: the row of the target table with the ID of its version, and the carried values. It gives the new row of an `INSERT` or an `UPDATE`, then the conditions of the check constraints, or one `true` value for a `DELETE` and `DO NOTHING`. It gives no row when its condition is not true.

use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{
    CmdType, List, MergeMatchKind, MergeStmt, MergeSupportFunc, MergeWhenClause, Node, RangeVar,
};
use rupg_types::{Value, oid};

use crate::Analyzer;
use crate::agg::Kind;
use crate::coerce::AtOpt;
use crate::expr::{Expr, ExprKind, Var};
use crate::from::{Column, FromItem, Join, JoinKind, Relation};
use crate::insert::{Foreign, Write, WriteKind, targets, with_checks};
use crate::modify::{carried_targets, old_row, set_columns, table_oid_target, tid_target};
use crate::perm::{ACL_DELETE, ACL_INSERT, ACL_SELECT, ACL_UPDATE};
use crate::returning::rewrite;
use crate::select::{Query, Target};
use crate::typename::place;

/// `MergeMatchKind`: the rows that a `WHEN` clause takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatchKind {
    /// `WHEN MATCHED`: a row of the target table with a source row.
    Matched,
    /// `WHEN NOT MATCHED BY SOURCE`: a row of the target table with no source row.
    NotMatchedBySource,
    /// `WHEN NOT MATCHED [BY TARGET]`: a source row with no row of the target table.
    NotMatchedByTarget,
}

/// The action of a `WHEN` clause.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeCommand {
    Insert,
    Update,
    Delete,
    /// `DO NOTHING`.
    Nothing,
}

/// `MergeAction`: a `WHEN` clause of a `MERGE`.
#[derive(Clone, Debug, PartialEq)]
pub struct MergeClause {
    pub kind: MatchKind,
    pub command: MergeCommand,
    /// The query of the clause, which reads the row of the target table with the ID of its version and the carried values. It gives the new row of an `INSERT` or an `UPDATE` with the conditions of the check constraints after the columns, or one `true` value for a `DELETE` and `DO NOTHING`. It gives no row when the condition of the clause is not true.
    pub query: Box<Query>,
}

/// The clauses of a `MERGE` and the foreign keys that its actions check.
#[derive(Clone, Debug, PartialEq)]
pub struct Merge {
    /// The `WHEN` clauses in the order of the statement.
    pub clauses: Vec<MergeClause>,
    /// The number of the carried values at the end of each row of the query of the statement.
    pub carried: usize,
    /// The foreign keys that an inserted row checks.
    pub insert_foreign: Vec<Foreign>,
    /// The foreign keys that an updated row checks.
    pub update_foreign: Vec<Foreign>,
    /// The foreign keys that a deleted row checks.
    pub delete_foreign: Vec<Foreign>,
}

/// A `WHEN` clause after the analysis of its parts, before the analyzer knows all the carried values.
struct Parts {
    kind: MatchKind,
    command: MergeCommand,
    condition: Option<Expr>,
    row: Vec<Expr>,
}

/// The action of a `WHEN` clause.
fn command(kind: CmdType) -> MergeCommand {
    match kind {
        CmdType::CMD_INSERT => MergeCommand::Insert,
        CmdType::CMD_UPDATE => MergeCommand::Update,
        CmdType::CMD_DELETE => MergeCommand::Delete,
        _ => MergeCommand::Nothing,
    }
}

/// The kind of the rows of a `WHEN` clause.
fn match_kind(clause: &MergeWhenClause) -> MatchKind {
    match clause.matchKind {
        MergeMatchKind::MERGE_WHEN_MATCHED => MatchKind::Matched,
        MergeMatchKind::MERGE_WHEN_NOT_MATCHED_BY_SOURCE => MatchKind::NotMatchedBySource,
        _ => MatchKind::NotMatchedByTarget,
    }
}

/// The place of a carried value that reads the column `attnum` of the relation `relation`, which the list gets when it does not have it.
pub(crate) fn carry(carried: &mut Vec<Expr>, relation: usize, attnum: i16, expr: &Expr) -> usize {
    let found = carried.iter().position(
        |c| matches!(c.kind, ExprKind::Var(v) if v.relation == relation && v.attnum == attnum),
    );
    found.unwrap_or_else(|| {
        let mut value = Expr::new(ExprKind::Var(Var { relation, attnum, levels_up: 0 }), expr.ty);
        value.typmod = expr.typmod;
        carried.push(value);
        carried.len() - 1
    })
}

impl Analyzer<'_> {
    /// `transformMergeStmt`: the query tree of a `MERGE`.
    pub(crate) fn merge(&mut self, s: &MergeStmt) -> Result<Query> {
        self.write_with(s.withClause.as_deref())?;
        let mut clauses = Vec::with_capacity(s.mergeWhenClauses.len());
        for node in &s.mergeWhenClauses {
            let Some(Node::MergeWhenClause(clause)) = node else {
                return Err(Error::internal("a WHEN clause that is not MergeWhenClause"));
            };
            clauses.push(&**clause);
        }
        // A clause after a clause of the same kind with no condition cannot take a row.
        let mut unconditional: Vec<MatchKind> = Vec::new();
        for clause in &clauses {
            let kind = match_kind(clause);
            if unconditional.contains(&kind) {
                return Err(Error::new(
                    SqlState::SYNTAX_ERROR,
                    "unreachable WHEN clause specified after unconditional WHEN clause",
                ));
            }
            if clause.condition.is_none() {
                unconditional.push(kind);
            }
        }
        let rv = s.relation.as_deref().ok_or_else(|| Error::internal("a MERGE with no table"))?;
        self.merge_actions = clauses.iter().map(|c| command(c.commandType)).collect();
        let (table, name, columns) = self.target_table(rv, WriteKind::Merge)?;
        // `DO NOTHING` needs `SELECT`, so that the target table needs some privilege.
        let required = clauses.iter().fold(0, |bits, clause| {
            bits | match clause.commandType {
                CmdType::CMD_INSERT => ACL_INSERT,
                CmdType::CMD_UPDATE => ACL_UPDATE,
                CmdType::CMD_DELETE => ACL_DELETE,
                _ => ACL_SELECT,
            }
        });
        if let Some(perm) = self.target_perm_mut() {
            perm.required |= required;
        }
        let source =
            s.sourceRelation.as_ref().ok_or_else(|| Error::internal("a MERGE with no source"))?;
        let source = self.merge_from(rv, source)?;
        let on = self.with_kind(Kind::JoinOn, |a| a.transform(s.joinCondition.as_ref()))?;
        let on = self.coerce_to_boolean(on, "JOIN/ON")?;
        let (returning, mut carried) = match s.returningClause.as_deref() {
            None => (None, Vec::new()),
            Some(clause) => {
                let (query, carried) = self.returning(clause, WriteKind::Merge)?;
                (Some(Box::new(query)), carried)
            }
        };
        let mut parts = Vec::with_capacity(clauses.len());
        let mut updated = Vec::new();
        for clause in &clauses {
            let kind = match_kind(clause);
            let update = matches!(clause.commandType, CmdType::CMD_UPDATE);
            // The items of `SET` of an updatable view name the columns of its base relation.
            let set =
                if update { self.auto_view_set(rv, &clause.targetList, &columns) } else { None };
            let (renamed, error) = match set {
                Some((list, unknown, fixed)) => (Some(list), unknown.or(fixed)),
                None => (None, None),
            };
            let list = renamed.as_ref().unwrap_or(&clause.targetList);
            if update {
                for column in set_columns(list, &columns) {
                    if !updated.contains(&column) {
                        updated.push(column);
                    }
                }
            }
            let full = self.merge_namespace(
                kind != MatchKind::NotMatchedByTarget,
                kind != MatchKind::NotMatchedBySource,
            );
            let part = match error {
                Some(error) => self.set_items(list).and(Err(error)),
                None => self.merge_clause(clause, list, kind, rv, (table, &name, &columns)),
            };
            self.set_namespace(full);
            let mut part = part?;
            // A column of the source becomes a carried value, which is the relation 1 of the query of the clause.
            let mut change = |expr: &mut Expr, depth: usize| {
                let ExprKind::Var(var) = expr.kind else { return };
                if var.levels_up != depth || var.relation == 0 {
                    return;
                }
                let at = carry(&mut carried, var.relation, var.attnum, expr);
                let attnum = i16::try_from(at + 1).unwrap_or(i16::MAX);
                expr.kind = ExprKind::Var(Var { relation: 1, attnum, levels_up: depth });
            };
            for expr in part.row.iter_mut().chain(part.condition.as_mut()) {
                rewrite(expr, 0, &mut change);
            }
            parts.push(part);
        }
        // The leaf partition of each row of a partitioned table checks the row after the routing, and so does the table of each row of a table with children of `INHERITS`.
        let inherit = self.inherits_target(WriteKind::Merge);
        let partitioned = self.is_partitioned(table) || inherit;
        let checks = if partitioned { Vec::new() } else { self.checks(table)? };
        let names = checks.iter().map(|(n, _)| n.clone()).collect();
        let generated = if partitioned { Vec::new() } else { self.generated_columns(table)? };
        let carried_columns: Vec<Column> = carried
            .iter()
            .map(|expr| Column {
                name: String::new(),
                ty: expr.ty,
                typmod: expr.typmod,
                not_null: false,
            })
            .collect();
        let mut target = self
            .scope
            .relations
            .first()
            .cloned()
            .ok_or_else(|| Error::internal("a MERGE with no table"))?;
        // The query of a clause reads the row of the join, and not the query of a view.
        target.subquery = None;
        let mut merge_clauses = Vec::with_capacity(parts.len());
        for part in parts {
            let writes = matches!(part.command, MergeCommand::Insert | MergeCommand::Update);
            let row = if writes {
                targets(&columns, part.row)
            } else {
                let expr = Expr::new(ExprKind::Const(Value::Bool(true)), oid::BOOL);
                vec![Target { name: String::new(), expr, origin: None, junk: false }]
            };
            let mut query = Query {
                relations: vec![
                    target.clone(),
                    Relation::new(0, carried_columns.clone(), None, None, None),
                ],
                from: vec![FromItem::Relation(0), FromItem::Relation(1)],
                targets: row,
                filter: part.condition,
                ..Query::default()
            };
            if writes && (!checks.is_empty() || !generated.is_empty()) {
                query = with_checks(&name, &columns, query, checks.clone(), &generated, &[]);
            }
            merge_clauses.push(MergeClause {
                kind: part.kind,
                command: part.command,
                query: Box::new(query),
            });
        }
        let has = |kind: MatchKind| merge_clauses.iter().any(|c| c.kind == kind);
        let (by_source, by_target) =
            (has(MatchKind::NotMatchedBySource), has(MatchKind::NotMatchedByTarget));
        // With `NOT MATCHED BY SOURCE`, a marker with each source row tells a row of the target table with no source row.
        let (source, marker) = if by_source {
            let index = self.scope.relations.len();
            let column =
                Column { name: "marker".to_string(), ty: oid::BOOL, typmod: -1, not_null: false };
            let value = Expr::new(ExprKind::Const(Value::Bool(true)), oid::BOOL);
            let mut relation = Relation::new(0, vec![column], None, None, Some(vec![vec![value]]));
            relation.name = "*VALUES*".to_string();
            self.scope.relations.push(relation);
            let var = Var { relation: index, attnum: 1, levels_up: 0 };
            (join(JoinKind::Inner, source, FromItem::Relation(index), None), ExprKind::Var(var))
        } else {
            (source, ExprKind::Const(Value::Bool(true)))
        };
        let kind = match (by_source, by_target) {
            (true, true) => JoinKind::Full,
            (true, false) => JoinKind::Right,
            (false, true) => JoinKind::Left,
            (false, false) => JoinKind::Inner,
        };
        // The conditions of an updatable view limit the rows of the target table before the join, as a join with one row does.
        let target_item = match self.auto_view_quals() {
            None => FromItem::Relation(0),
            Some(quals) => {
                let index = self.scope.relations.len();
                let column =
                    Column { name: "one".to_string(), ty: oid::BOOL, typmod: -1, not_null: false };
                let value = Expr::new(ExprKind::Const(Value::Bool(true)), oid::BOOL);
                let mut relation =
                    Relation::new(0, vec![column], None, None, Some(vec![vec![value]]));
                relation.name = "*VALUES*".to_string();
                self.scope.relations.push(relation);
                join(JoinKind::Inner, FromItem::Relation(0), FromItem::Relation(index), Some(quals))
            }
        };
        // `view_has_instead_trigger`: the triggers `INSTEAD OF` of a view do the actions, and the rows of the view have no ID of their versions. A column of a join with one row tells a row of the view from a source row with no match.
        let instead = self.is_view(table);
        let mut tid = tid_target();
        let target_item = if instead {
            let index = self.scope.relations.len();
            let column =
                Column { name: "ctid".to_string(), ty: oid::INT8, typmod: -1, not_null: false };
            let value = Expr::new(ExprKind::Const(Value::Int8(0)), oid::INT8);
            let mut relation = Relation::new(0, vec![column], None, None, Some(vec![vec![value]]));
            relation.name = "*VALUES*".to_string();
            self.scope.relations.push(relation);
            let var = Var { relation: index, attnum: 1, levels_up: 0 };
            tid.expr = Expr::new(ExprKind::Var(var), oid::INT8);
            join(JoinKind::Inner, target_item, FromItem::Relation(index), None)
        } else {
            target_item
        };
        let from = join(kind, source, target_item, Some(on));
        let mut query_targets = old_row(&columns);
        if partitioned {
            query_targets.push(table_oid_target());
        }
        query_targets.push(tid);
        query_targets.push(Target {
            name: String::new(),
            expr: Expr::new(marker, oid::BOOL),
            origin: None,
            junk: false,
        });
        let count = carried.len();
        query_targets.extend(carried_targets(carried));
        let uses = |command: MergeCommand| merge_clauses.iter().any(|c| c.command == command);
        if !updated.is_empty() {
            self.mark_updated(&updated);
        }
        let foreign = |a: &Self, command: MergeCommand, kind: WriteKind| {
            if uses(command) { a.foreign_keys(table, kind, &columns) } else { Ok(Vec::new()) }
        };
        let insert_foreign = foreign(self, MergeCommand::Insert, WriteKind::Insert)?;
        let update_foreign = foreign(self, MergeCommand::Update, WriteKind::Update)?;
        let delete_foreign = foreign(self, MergeCommand::Delete, WriteKind::Delete)?;
        let unique = self.unique_keys(table, &columns)?;
        // The triggers `BEFORE ... FOR EACH ROW` of the inserts and of the updates come before the checks of the constraints.
        let recheck =
            [(MergeCommand::Insert, WriteKind::Insert), (MergeCommand::Update, WriteKind::Update)]
                .into_iter()
                .filter(|&(command, _)| uses(command))
                .find_map(|(_, kind)| {
                    self.recheck(table, kind, &name, &columns, &checks, &generated)
                });
        let mut query = Query {
            relations: std::mem::take(&mut self.scope.relations),
            from: vec![from],
            targets: query_targets,
            write: Some(Write {
                kind: WriteKind::Merge,
                table,
                name,
                alias: None,
                width: columns.len(),
                checks: names,
                unique,
                returning,
                foreign: Vec::new(),
                on_conflict: None,
                merge: Some(Merge {
                    clauses: merge_clauses,
                    carried: count,
                    insert_foreign,
                    update_foreign,
                    delete_foreign,
                }),
                recheck,
                updated: crate::modify::with_extra_updated(updated, &generated),
                instead,
                inherit,
                view_check: None,
                views: 0,
            }),
            ..Query::default()
        };
        self.end_write_with(&mut query, s.withClause.as_deref())?;
        Ok(query)
    }

    /// `transformMergeStmt` for one `WHEN` clause: the condition, and the new row of an `INSERT` or an `UPDATE`.
    fn merge_clause(
        &mut self,
        clause: &MergeWhenClause,
        list: &List,
        kind: MatchKind,
        rv: &RangeVar,
        (table, name, columns): (u32, &str, &[Column]),
    ) -> Result<Parts> {
        let condition = match &clause.condition {
            None => None,
            Some(node) => {
                let expr = self.with_kind(Kind::MergeWhen, |a| a.transform(Some(node)))?;
                Some(self.coerce_to_boolean(expr, "WHEN")?)
            }
        };
        let (command, row) = match clause.commandType {
            CmdType::CMD_INSERT => {
                self.swap_auto_view_defaults();
                let row = self.merge_insert(clause, table, name, columns);
                self.swap_auto_view_defaults();
                (MergeCommand::Insert, row?)
            }
            CmdType::CMD_UPDATE => {
                (MergeCommand::Update, self.update_row(rv, table, name, columns, list)?)
            }
            CmdType::CMD_DELETE => (MergeCommand::Delete, Vec::new()),
            _ => (MergeCommand::Nothing, Vec::new()),
        };
        Ok(Parts { kind, command, condition, row })
    }

    /// The new row of an `INSERT` action of a `MERGE`: the expressions of `VALUES` for the columns that the action gives, and the defaults of the other columns.
    fn merge_insert(
        &mut self,
        clause: &MergeWhenClause,
        table: u32,
        name: &str,
        columns: &[Column],
    ) -> Result<Vec<Expr>> {
        let given = match self.auto_view_targets(&clause.targetList, columns.len())? {
            Some(given) => given,
            None => self.insert_targets(&clause.targetList, name, columns)?,
        };
        let exprs = if clause.values.is_empty() {
            // `INSERT DEFAULT VALUES`.
            Vec::new()
        } else {
            self.mark_inserted(&given);
            let items = self.value_items(&clause.values, Kind::ValuesSingle)?;
            let explicit = !clause.targetList.is_empty();
            self.insert_row(items, table, columns, &given, explicit, clause.r#override)?
        };
        self.full_row(table, columns, &given, exprs)
    }

    /// `transformMergeSupportFunc`: `merge_action()`, which is only in the list of `RETURNING` of a `MERGE` or in a subquery of it. It reads the column of the relation of the action.
    ///
    /// # Errors
    ///
    /// `42601` in another place.
    pub(crate) fn merge_support_func(&mut self, f: &MergeSupportFunc) -> Result<Expr> {
        let count = self.outer_kinds.len();
        for (level, scope) in self.levels() {
            let kind = match level {
                0 => Some(self.kind),
                _ => count.checked_sub(level).and_then(|i| self.outer_kinds.get(i)?.0),
            };
            if kind == Some(Kind::Returning)
                && let Some(relation) = scope.merge_action
            {
                let var = Var { relation, attnum: 1, levels_up: level };
                return Ok(Expr::new(ExprKind::Var(var), oid::TEXT).at(place(f.location)));
            }
        }
        Err(Error::new(
            SqlState::SYNTAX_ERROR,
            "MERGE_ACTION() can only be used in the RETURNING list of a MERGE command",
        )
        .at_opt(place(f.location)))
    }
}

/// A join with no names of its own.
fn join(kind: JoinKind, left: FromItem, right: FromItem, on: Option<Expr>) -> FromItem {
    FromItem::Join(Box::new(Join {
        kind,
        left,
        right,
        on,
        using: Vec::new(),
        plain_using: true,
        alias: None,
        using_alias: None,
        merged: Vec::new(),
        columns: Vec::new(),
        aliases: Vec::new(),
        sources: Vec::new(),
    }))
}
