//! `RTEPermissionInfo`: the privileges that a statement needs on the relations that it reads and writes, which `ExecCheckPermissions` checks before the statement runs.

use std::collections::BTreeSet;

use rupg_catalog::RelKind;

use crate::Analyzer;
use crate::expr::{Expr, ExprKind};
use crate::insert::Given;
use crate::select::Query;

/// `ACL_INSERT`.
pub(crate) const ACL_INSERT: u32 = 1 << 0;
/// `ACL_SELECT`.
pub(crate) const ACL_SELECT: u32 = 1 << 1;
/// `ACL_UPDATE`, which is also `ACL_SELECT_FOR_UPDATE`.
pub(crate) const ACL_UPDATE: u32 = 1 << 2;
/// `ACL_DELETE`.
pub(crate) const ACL_DELETE: u32 = 1 << 3;

/// `RTEPermissionInfo`: the privileges that the statement needs on a relation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PermInfo {
    /// The OID of the relation.
    pub relid: u32,
    /// `checkAsUser`: the role whose privileges count, or 0 for the current user. A relation of the query of a view uses the owner of the view, unless the view has `security_invoker`.
    pub check_as: u32,
    /// True for a relation of the query of a view with `security_invoker`, whose 0 in `check_as` stays the current user in a view outside it, as `RelationBuildRuleLock` gives `InvalidOid` to `setRuleCheckAsUser` for the rules of the view.
    pub invoker: bool,
    /// `requiredPerms`: the bits of `ACL_SELECT`, `ACL_INSERT`, `ACL_UPDATE` and `ACL_DELETE` that the statement needs.
    pub required: u32,
    /// `selectedCols`: the numbers of the columns that the statement reads, with 0 for the whole row.
    pub selected: BTreeSet<i16>,
    /// `insertedCols`: the numbers of the columns that an `INSERT` gives values.
    pub inserted: BTreeSet<i16>,
    /// `updatedCols`: the numbers of the columns that an `UPDATE` sets.
    pub updated: BTreeSet<i16>,
    /// The level of the subquery `*rule*` of an action of a rule that names the relation, which is 0 for the action itself. `rewriteRuleAction` puts the privileges of the statement that the rule rewrites before the privileges of the action, so the checks start at the deepest level.
    pub rule_level: usize,
}

/// Calls `f` for the query and for each query in it: the subqueries in `FROM`, the queries of `WITH` and the subqueries of its expressions.
fn each_query(query: &mut Query, f: &mut impl FnMut(&mut Query)) {
    fn expr(e: &mut Expr, f: &mut impl FnMut(&mut Query)) {
        for child in e.children_mut() {
            expr(child, f);
        }
        if let ExprKind::SubLink(link) = &mut e.kind {
            each_query(&mut link.query, f);
        }
    }
    f(query);
    for relation in &mut query.relations {
        if let Some(sub) = relation.subquery.as_deref_mut() {
            each_query(sub, f);
        }
    }
    for cte in &mut query.ctes {
        each_query(&mut cte.query, f);
    }
    for e in query.exprs_mut() {
        expr(e, f);
    }
}

/// Adds `offset` to the index of the privileges of each relation of the query and of the queries in it, after the privileges of the query of a view or of a policy go after the privileges of the statement.
pub(crate) fn shift(query: &mut Query, offset: usize) {
    each_query(query, &mut |q| {
        for perm in q.relations.iter_mut().filter_map(|r| r.perm.as_mut()) {
            *perm += offset;
        }
    });
}

/// [`shift`] for the subqueries of an expression.
pub(crate) fn shift_expr(expr: &mut Expr, offset: usize) {
    for child in expr.children_mut() {
        shift_expr(child, offset);
    }
    if let ExprKind::SubLink(link) = &mut expr.kind {
        shift(&mut link.query, offset);
    }
}

/// Forgets the privileges of the relations of the query and of the queries in it, for the query of a view that the statement does not read.
pub(crate) fn forget(query: &mut Query) {
    each_query(query, &mut |q| {
        for relation in &mut q.relations {
            relation.perm = None;
        }
    });
}

/// True when the boolean option `name` of `reloptions` is on, as `RelationHasSecurityInvoker` and `RelationIsSecurityView` read `security_invoker` and `security_barrier`.
pub fn option_on(options: &[String], name: &str) -> bool {
    options.iter().any(|option| {
        option.split_once('=').is_some_and(|(option, value)| {
            option == name
                && matches!(
                    value.to_ascii_lowercase().as_str(),
                    "true" | "on" | "yes" | "1" | "t" | "y"
                )
        })
    })
}

/// True when a subquery of `FROM` for the relation `rel` is a security barrier: a view with `security_barrier`, or a table, which the plan reads through a subquery only when its row-level security policies apply.
pub fn security_barrier(rel: &rupg_catalog::Relation) -> bool {
    matches!(rel.kind, RelKind::Table | RelKind::Partitioned)
        || option_on(&rel.options, "security_barrier")
}

impl Analyzer<'_> {
    /// `addRTEPermissionInfo`: a new entry of the privileges of the statement for the relation, which needs the bits `required`. It gives the index of the entry. In an action of a rule, the owner of the table of the rule gets the checks, as `setRuleCheckAsUser` sets `checkAsUser`.
    pub(crate) fn add_perm(&mut self, relid: u32, required: u32) -> usize {
        let rule_level = self.rule.level;
        let check_as = self.rule.owners.get(rule_level).copied().unwrap_or(0);
        self.perms.push(PermInfo { relid, required, check_as, rule_level, ..PermInfo::default() });
        self.perms.len() - 1
    }

    /// The part of `ApplyRetrieveRule` for the privileges: the privileges of the query of the view `view` go after the privileges of the statement, and they are the privileges of the owner of the view, unless the view has `security_invoker`.
    pub(crate) fn add_view_perms(&mut self, view: u32, query: &mut Query, perms: Vec<PermInfo>) {
        let rel = self.env.catalog().and_then(|c| c.relation(view));
        let invoker = rel.is_some_and(|rel| {
            rel.kind == RelKind::View && option_on(&rel.options, "security_invoker")
        });
        let owner = match rel {
            Some(rel) if !invoker => rel.owner,
            _ => 0,
        };
        shift(query, self.perms.len());
        self.funcs.append(&mut query.funcs);
        for mut perm in perms {
            if perm.check_as == 0 && !perm.invoker {
                perm.check_as = owner;
                perm.invoker = invoker;
            }
            self.perms.push(perm);
        }
    }

    /// The functions that a stored expression of the table calls, such as a default, a generation expression or a check constraint. The executor checks `EXECUTE` on them as on the calls of the statement.
    pub(crate) fn note_funcs(&mut self, expr: &Expr) {
        expr.find(0, &mut |e, _| {
            if let ExprKind::Func(f) = &e.kind {
                self.funcs.push(f.oid);
            }
            None::<()>
        });
    }

    /// `markVarForSelectPriv`: each column of a relation that the expression reads needs `SELECT`, in the privileges of the relation at the level of the column.
    pub(crate) fn mark_select(&mut self, expr: &Expr) {
        let mut vars = Vec::new();
        expr.find(0, &mut |e, depth| {
            if let ExprKind::Var(var) = &e.kind
                && var.levels_up >= depth
            {
                vars.push((var.levels_up - depth, var.relation, var.attnum));
            }
            None::<()>
        });
        for (level, relation, attnum) in vars {
            if let Some(perm) = self.relation_perm(level, relation) {
                self.perms[perm].required |= ACL_SELECT;
                self.perms[perm].selected.insert(attnum);
            }
        }
    }

    /// The part of `markRTEForSelectPriv` for a relation of the scope `level` with no column, as for `t.*` of a table with no columns.
    pub(crate) fn mark_relation(&mut self, level: usize, relation: usize) {
        if let Some(perm) = self.relation_perm(level, relation) {
            self.perms[perm].required |= ACL_SELECT;
        }
    }

    /// The index of the privileges of a relation of the scope `level`, or `None` for a relation that needs no privileges.
    fn relation_perm(&self, level: usize, relation: usize) -> Option<usize> {
        let (_, scope) = self.levels().nth(level)?;
        scope.relations.get(relation)?.perm
    }

    /// The privileges of the target table of the write, which `target_table` made, or `None`.
    pub(crate) fn target_perm_mut(&mut self) -> Option<&mut PermInfo> {
        self.target_perm.and_then(|perm| self.perms.get_mut(perm))
    }

    /// `insertedCols`: the columns of the target table that an `INSERT` gives values.
    pub(crate) fn mark_inserted(&mut self, given: &[Given]) {
        if let Some(perm) = self.target_perm_mut() {
            perm.inserted
                .extend(given.iter().map(|(i, _, _)| i16::try_from(i + 1).unwrap_or(i16::MAX)));
        }
    }

    /// `updatedCols`: the columns of the target table that `SET` gives, which need `UPDATE`.
    pub(crate) fn mark_updated(&mut self, columns: &[i16]) {
        if let Some(perm) = self.target_perm_mut() {
            perm.required |= ACL_UPDATE;
            perm.updated.extend(columns.iter().copied());
        }
    }

    /// The part of `transformLockingClause` for the privileges: a relation with a lock on its rows needs `UPDATE`, also in a subquery of `FROM` and in the query of a view.
    pub(crate) fn mark_locked(&mut self, query: &Query) {
        for mark in &query.row_marks {
            if let Some(perm) = query.relations.get(mark.relation).and_then(|r| r.perm) {
                self.perms[perm].required |= ACL_UPDATE;
            }
        }
        for relation in &query.relations {
            if let Some(sub) = relation.subquery.as_deref() {
                self.mark_locked(sub);
            }
        }
    }
}
