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
    /// `requiredPerms`: the bits of `ACL_SELECT`, `ACL_INSERT`, `ACL_UPDATE` and `ACL_DELETE` that the statement needs.
    pub required: u32,
    /// `selectedCols`: the numbers of the columns that the statement reads, with 0 for the whole row.
    pub selected: BTreeSet<i16>,
    /// `insertedCols`: the numbers of the columns that an `INSERT` gives values.
    pub inserted: BTreeSet<i16>,
    /// `updatedCols`: the numbers of the columns that an `UPDATE` sets.
    pub updated: BTreeSet<i16>,
}

/// Adds `offset` to the index of the privileges of each relation of the query and of its subqueries in `FROM`, after the privileges of the query of a view go after the privileges of the statement.
fn shift(query: &mut Query, offset: usize) {
    for relation in &mut query.relations {
        if let Some(perm) = &mut relation.perm {
            *perm += offset;
        }
        if let Some(sub) = relation.subquery.as_deref_mut() {
            shift(sub, offset);
        }
    }
}

/// Forgets the privileges of the relations of the query and of its subqueries in `FROM`, for the query of a view that the statement does not read.
pub(crate) fn forget(query: &mut Query) {
    for relation in &mut query.relations {
        relation.perm = None;
        if let Some(sub) = relation.subquery.as_deref_mut() {
            forget(sub);
        }
    }
}

/// `RelationHasSecurityInvoker`: true when the option `security_invoker` of the view is on.
fn security_invoker(options: &[String]) -> bool {
    options.iter().any(|option| {
        option.split_once('=').is_some_and(|(name, value)| {
            name == "security_invoker"
                && matches!(
                    value.to_ascii_lowercase().as_str(),
                    "true" | "on" | "yes" | "1" | "t" | "y"
                )
        })
    })
}

impl Analyzer<'_> {
    /// `addRTEPermissionInfo`: a new entry of the privileges of the statement for the relation, which needs the bits `required`. It gives the index of the entry.
    pub(crate) fn add_perm(&mut self, relid: u32, required: u32) -> usize {
        self.perms.push(PermInfo { relid, required, ..PermInfo::default() });
        self.perms.len() - 1
    }

    /// The part of `ApplyRetrieveRule` for the privileges: the privileges of the query of the view `view` go after the privileges of the statement, and they are the privileges of the owner of the view, unless the view has `security_invoker`.
    pub(crate) fn add_view_perms(&mut self, view: u32, query: &mut Query, perms: Vec<PermInfo>) {
        let rel = self.env.catalog().and_then(|c| c.relation(view));
        let owner = match rel {
            Some(rel) if rel.kind == RelKind::View && security_invoker(&rel.options) => 0,
            Some(rel) => rel.owner,
            None => 0,
        };
        shift(query, self.perms.len());
        self.funcs.append(&mut query.funcs);
        for mut perm in perms {
            if perm.check_as == 0 {
                perm.check_as = owner;
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
