//! `FOR UPDATE`, `FOR NO KEY UPDATE`, `FOR SHARE` and `FOR KEY SHARE`, as `transformLockingClause` and `CheckSelectLocking` of `analyze.c` and `markQueryForLocking` of `rewriteHandler.c` run them.
//!
//! rupg takes no lock on a row. No statement of rupg changes the rows of a table, so a lock on a row has no effect on another session. The analyzer gives the errors of PostgreSQL and keeps the row marks, which the planner and the executor check.

use rupg_common::{Error, Result, SqlState};
use rupg_sql::nodes::{LockClauseStrength, LockWaitPolicy, LockingClause, Node};

use crate::coerce::AtOpt;
use crate::from::{FromItem, Join, Relation};
use crate::setop::strength_name;
use crate::typename::place;
use crate::{JoinKind, Query};

/// A relation that `FOR UPDATE` or `FOR SHARE` locks, as a `RowMarkClause`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RowMark {
    /// The index of the relation in [`Query::relations`].
    pub relation: usize,
    pub strength: LockClauseStrength,
    /// `waitPolicy`: `NOWAIT`, `SKIP LOCKED` or a wait for the lock.
    pub wait: LockWaitPolicy,
    /// `pushedDown`: the mark comes from a lock on a query outside, not from a clause of this query.
    pub pushed: bool,
}

/// The lock of a locking clause on one relation: the strength, the wait policy and whether the lock comes from a query outside.
#[derive(Clone, Copy)]
struct Lock {
    strength: LockClauseStrength,
    wait: LockWaitPolicy,
    pushed: bool,
}

/// An item of the range table of a query in the order of PostgreSQL: a relation of [`Query::relations`], or a join with the name that it has as a relation, which comes after its two sides.
enum Entry {
    Relation(usize),
    Join(Option<String>),
}

/// The items of the range table of the query.
fn entries(query: &Query) -> Vec<Entry> {
    fn walk(item: &FromItem, out: &mut Vec<Entry>) {
        match item {
            FromItem::Relation(index) => out.push(Entry::Relation(*index)),
            FromItem::Join(join) => {
                walk(&join.left, out);
                walk(&join.right, out);
                out.push(Entry::Join(join_name(join)));
            }
        }
    }
    let mut out = Vec::new();
    for item in &query.from {
        walk(item, &mut out);
    }
    out
}

/// The name of a join as a relation: its alias, or the alias of `USING (...) AS name`. A join with neither has no name.
fn join_name(join: &Join) -> Option<String> {
    join.alias.clone().or_else(|| join.using_alias.clone())
}

/// The relations of the range table of the query, in its order.
fn relation_entries(query: &Query) -> Vec<usize> {
    entries(query)
        .into_iter()
        .filter_map(|entry| match entry {
            Entry::Relation(index) => Some(index),
            Entry::Join(_) => None,
        })
        .collect()
}

/// A table or a view: an `RTE_RELATION` before the rewriter reads the view.
fn is_relation(relation: &Relation) -> bool {
    relation.oid != 0 && relation.function.is_none() && relation.cte.is_none()
}

/// A subquery in `FROM`, which is not a view.
fn is_subquery(relation: &Relation) -> bool {
    relation.oid == 0 && relation.subquery.is_some() && relation.cte.is_none()
}

/// `CheckSelectLocking`: the parts of a query that cannot go with a lock on its rows.
pub(crate) fn check_select_locking(query: &Query, strength: LockClauseStrength) -> Result<()> {
    let what = if query.set_op.is_some() {
        "UNION/INTERSECT/EXCEPT"
    } else if !query.distinct.is_empty() {
        "DISTINCT clause"
    } else if !query.group.is_empty()
        || (query.grouped && !has_aggs(query) && query.having.is_none())
    {
        "GROUP BY clause"
    } else if query.having.is_some() {
        "HAVING clause"
    } else if has_aggs(query) {
        "aggregate functions"
    } else if query.targets.iter().any(|t| t.expr.first_window().is_some()) {
        "window functions"
    } else if query.has_target_srfs() {
        "set-returning functions in the target list"
    } else {
        return Ok(());
    };
    Err(Error::new(
        SqlState::FEATURE_NOT_SUPPORTED,
        format!("{} is not allowed with {what}", strength_name(strength)),
    ))
}

/// `hasAggs`: the query has an aggregate call of its own level.
fn has_aggs(query: &Query) -> bool {
    query.targets.iter().any(|t| t.expr.first_agg().is_some())
        || query.having.as_ref().is_some_and(|h| h.first_agg().is_some())
}

/// `applyLockingClause`: the row mark of a relation. A relation with two marks keeps the strongest strength and the strongest wait policy, and the mark is pushed down only when both locks are.
fn apply(query: &mut Query, relation: usize, lock: Lock) {
    match query.row_marks.iter_mut().find(|mark| mark.relation == relation) {
        Some(mark) => {
            if lock.strength.0 > mark.strength.0 {
                mark.strength = lock.strength;
            }
            if lock.wait.0 > mark.wait.0 {
                mark.wait = lock.wait;
            }
            mark.pushed &= lock.pushed;
        }
        None => query.row_marks.push(RowMark {
            relation,
            strength: lock.strength,
            wait: lock.wait,
            pushed: lock.pushed,
        }),
    }
}

/// The row mark of a table or a view. The rewriter puts the lock of a view on the relations of its query, as `markQueryForLocking` does, and the planner then checks the query of the view.
fn apply_relation(query: &mut Query, relation: usize, lock: Lock) -> Result<()> {
    apply(query, relation, lock);
    if let Some(view) = query.relations[relation].subquery.as_deref_mut() {
        mark_for_locking(view, Lock { pushed: true, ..lock })?;
    }
    Ok(())
}

/// `markQueryForLocking` on the query of a view, then the check of `preprocess_rowmarks` when the query has a row mark.
fn mark_for_locking(query: &mut Query, lock: Lock) -> Result<()> {
    for index in relation_entries(query) {
        let relation = &query.relations[index];
        if is_relation(relation) {
            apply_relation(query, index, lock)?;
        } else if is_subquery(relation) {
            apply(query, index, lock);
            if let Some(sub) = query.relations[index].subquery.as_deref_mut() {
                mark_for_locking(sub, lock)?;
            }
        }
    }
    if let Some(first) = query.row_marks.first() {
        check_select_locking(query, first.strength)?;
    }
    Ok(())
}

/// `transformLockingClause`: the row marks of a locking clause of a query, on the relations that the clause names or on all the relations of `FROM`.
pub(crate) fn transform_locking_clause(query: &mut Query, clause: &LockingClause) -> Result<()> {
    transform_locking(query, clause, false)
}

/// `transformLockingClause` with `pushedDown`, which is true for the lock of a subquery that a lock outside it gives.
fn transform_locking(query: &mut Query, clause: &LockingClause, pushed: bool) -> Result<()> {
    let strength = clause.strength;
    let lock = Lock { strength, wait: clause.waitPolicy, pushed };
    check_select_locking(query, strength)?;
    if clause.lockedRels.is_empty() {
        for index in relation_entries(query) {
            lock_relation(query, index, lock)?;
        }
        return Ok(());
    }
    let order = entries(query);
    for node in clause.lockedRels.iter().flatten() {
        let Node::RangeVar(rv) = node else { continue };
        let at = place(rv.location);
        if rv.catalogname.is_some() || rv.schemaname.is_some() {
            return Err(Error::new(
                SqlState::SYNTAX_ERROR,
                format!("{} must specify unqualified relation names", strength_name(strength)),
            )
            .at_opt(at));
        }
        let name = rv.relname.as_deref().unwrap_or_default();
        let found = order.iter().find_map(|entry| match entry {
            Entry::Join(alias) => {
                (alias.as_deref() == Some(name)).then_some(Found::Error("a join"))
            }
            Entry::Relation(index) => {
                let relation = &query.relations[*index];
                let hidden = relation.alias.is_none()
                    && (is_subquery(relation) || relation.values.is_some());
                if hidden || relation.name != name {
                    return None;
                }
                Some(if relation.cte.is_some() {
                    Found::Error("a WITH query")
                } else if relation.json_table().is_some() {
                    Found::Error("a table function")
                } else if relation.function.is_some() {
                    Found::Error("a function")
                } else if relation.values.is_some() {
                    Found::Error("VALUES")
                } else {
                    Found::Relation(*index)
                })
            }
        });
        match found {
            Some(Found::Relation(index)) => lock_relation(query, index, lock)?,
            Some(Found::Error(what)) => {
                return Err(Error::new(
                    SqlState::FEATURE_NOT_SUPPORTED,
                    format!("{} cannot be applied to {what}", strength_name(strength)),
                )
                .at_opt(at));
            }
            None => {
                return Err(Error::new(
                    SqlState::UNDEFINED_TABLE,
                    format!(
                        "relation \"{name}\" in {} clause not found in FROM clause",
                        strength_name(strength)
                    ),
                )
                .at_opt(at));
            }
        }
    }
    Ok(())
}

/// The relation that a name of a locking clause finds: a table, a view or a subquery, or the error of another kind of relation.
enum Found {
    Relation(usize),
    Error(&'static str),
}

/// The lock of a relation of `FROM`: the row mark of a table or a view, or the row mark of a subquery and the lock of all the relations of the subquery. The other relations have no lock.
fn lock_relation(query: &mut Query, index: usize, lock: Lock) -> Result<()> {
    let relation = &query.relations[index];
    if is_relation(relation) {
        apply_relation(query, index, lock)
    } else if is_subquery(relation) {
        apply(query, index, lock);
        let all = LockingClause {
            lockedRels: Vec::new(),
            strength: lock.strength,
            waitPolicy: lock.wait,
        };
        match query.relations[index].subquery.as_deref_mut() {
            Some(sub) => transform_locking(sub, &all, true),
            None => Ok(()),
        }
    } else {
        Ok(())
    }
}

/// The check of `make_outerjoininfo`: a relation with a row mark cannot be on the nullable side of an outer join, at each level of the query.
///
/// # Errors
///
/// `0A000` for a row mark on the nullable side of an outer join.
pub fn check_outer_joins(query: &Query) -> Result<()> {
    fn relations(item: &FromItem, out: &mut Vec<usize>) {
        match item {
            FromItem::Relation(index) => out.push(*index),
            FromItem::Join(join) => {
                relations(&join.left, out);
                relations(&join.right, out);
            }
        }
    }
    fn check(query: &Query, item: &FromItem) -> Result<()> {
        let FromItem::Join(join) = item else { return Ok(()) };
        check(query, &join.left)?;
        check(query, &join.right)?;
        let (mut left, mut right) = (Vec::new(), Vec::new());
        relations(&join.left, &mut left);
        relations(&join.right, &mut right);
        let nullable: Vec<usize> = match join.kind {
            JoinKind::Left => right,
            JoinKind::Right => left,
            JoinKind::Full => left.into_iter().chain(right).collect(),
            _ => Vec::new(),
        };
        match query.row_marks.iter().find(|mark| nullable.contains(&mark.relation)) {
            Some(mark) => Err(Error::new(
                SqlState::FEATURE_NOT_SUPPORTED,
                format!(
                    "{} cannot be applied to the nullable side of an outer join",
                    strength_name(mark.strength)
                ),
            )),
            None => Ok(()),
        }
    }
    for item in &query.from {
        check(query, item)?;
    }
    for relation in &query.relations {
        if let Some(sub) = &relation.subquery {
            check_outer_joins(sub)?;
        }
    }
    Ok(())
}

/// The strength of the first row mark on a table in the query, in its subqueries in `FROM` and in its views, as the first `PlanRowMark` of the plan.
pub fn first_row_mark(query: &Query) -> Option<LockClauseStrength> {
    let own = query.row_marks.iter().find(|mark| query.relations[mark.relation].subquery.is_none());
    match own {
        Some(mark) => Some(mark.strength),
        None => {
            query.relations.iter().filter_map(|r| r.subquery.as_deref()).find_map(first_row_mark)
        }
    }
}

/// The tables with a row mark in the query, in its subqueries in `FROM` and in its views.
pub fn locked_tables(query: &Query, out: &mut Vec<u32>) {
    for mark in &query.row_marks {
        let relation = &query.relations[mark.relation];
        if relation.oid != 0 && relation.subquery.is_none() {
            out.push(relation.oid);
        }
    }
    for sub in query.relations.iter().filter_map(|r| r.subquery.as_deref()) {
        locked_tables(sub, out);
    }
}
